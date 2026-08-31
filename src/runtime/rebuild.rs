//! Replacing a registered company's runtime in place (issue #290).
//!
//! Which brain a company runs is chosen once, in
//! [`RuntimeBuilder::build`](crate::runtime::RuntimeBuilder::build). A company
//! that resolved no inference at boot is on the offline echo brain with an
//! unwired workflow runner, and a credential written afterwards reaches neither.
//! #266 shipped the honest half of that (`restartRequired`, and a console banner
//! saying so); this module is the half that removes the restart, which is the
//! only form a hosted tenant can actually act on — the control plane has no
//! "restart this tenant" button, and the container is the unit of restart.
//!
//! # The sequence
//!
//! 1. **Quiesce.** [`CompanyRuntime::quiesce`] stops the outgoing runtime
//!    accepting cycles and waits for the one in flight to drain, so the swap
//!    happens at a point with no live turn.
//! 2. **Hand over.** [`CompanyRuntime::handover`] snapshots the per-instance
//!    state a second runtime must not duplicate — the journal, the approval gate,
//!    the grant set, the event log, the stores, the harness pool, the MCP
//!    runtime, and the two serialising mutexes. See [`RuntimeHandover`] for why
//!    each one is a correctness matter rather than an optimisation.
//! 3. **Rebuild.** A host-supplied [`RuntimeRebuilder`] runs the *same* wiring
//!    boot used, with the handover attached. It lives behind a trait because that
//!    wiring (harness pool, OpenHuman RPC, managed backends, per-tenant mailbox)
//!    is assembled in the binary, above this crate's public surface.
//! 4. **Swap.** The successor replaces the outgoing runtime in the registry.
//!
//! # What happens to in-flight work
//!
//! The cycle in flight at step 1 **completes** on the outgoing runtime, against
//! the same journal, approval queue and stores the successor then adopts. It is
//! not cancelled and its effects are not replayed: the executed-key set moves
//! across with the journal, so an effect committed by the outgoing runtime stays
//! committed for the successor.
//!
//! Cycles that arrive *during* the window are refused with
//! [`OpenCompanyError::Quiescing`] (`503`) rather than queued, so a caller
//! retries against the successor instead of silently getting the brain the
//! rebuild was replacing. The window is one turn wide, and that is also the
//! bound on the *triggering* request: a rebuild started mid-turn blocks until
//! the turn drains. Giving up and swapping anyway would trade a slow response
//! for a corrupted journal, so the wait is unbounded on purpose.
//!
//! Parked approvals survive untouched: the gate itself is handed over, so an
//! approval waiting on a person keeps its id, its parked effect and its TTL, and
//! resolving it after the swap runs the follow-up on the *new* brain. The same
//! goes for single-use grants — an operator who approved a tool call a moment
//! before the rebuild does not have to approve it again.
//!
//! Orphan-run reaping is suppressed on a rebuild. At boot it is sound because
//! nothing from this process can be in flight; during a rebuild that premise is
//! false, and reaping would settle live run records as dead.
//!
//! # On failure
//!
//! A rebuild that fails leaves the outgoing runtime registered and
//! [`resume`](CompanyRuntime::resume)s it. A company stuck quiesced would refuse
//! every cycle forever, which is strictly worse than the stale brain the rebuild
//! was trying to replace.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::Result;
use crate::app::AppState;
use crate::company::CompanyManifest;
use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::ports::store::company_write_lock;
use crate::ports::types::CompanyId;
use crate::runtime::handover::RuntimeHandover;

/// The boot-only builder inputs a rebuild cannot recover from the runtime or the
/// environment, stashed at registration so a later rebuild configures the
/// successor exactly as boot configured its predecessor.
///
/// `discoverable` is the motivating case: `serve --discoverable` exists only in
/// the `serve` stack frame and mutates the manifest *before* the build, so a
/// rebuild that re-read `company.toml` from `source_dir` would silently drop it
/// and un-publish a public company.
#[derive(Clone, Debug, Default)]
pub struct BootInputs {
    /// The company's on-disk source directory (`companies/<name>`), used to seed
    /// the workspace and resolve committed skills/workflows.
    pub source_dir: Option<PathBuf>,
    /// Whether `serve --discoverable` forced this company public regardless of
    /// its manifest `[place].discoverable`.
    pub discoverable: bool,
}

/// Everything a [`RuntimeRebuilder`] needs to produce a successor runtime.
pub struct RebuildRequest {
    /// The company being rebuilt. Already registered; the successor keeps this id.
    pub id: CompanyId,
    /// The company's **materialized** manifest, read from its persisted record
    /// rather than re-parsed from disk.
    ///
    /// This is the manifest the running company actually has, which matters for
    /// two things a fresh `company.toml` read would drop: the console-created
    /// workflows merged into `[workflows].enabled`, and the `[place]` fields
    /// `serve --discoverable` mutated before the original build. A
    /// platform-provisioned tenant has no `company.toml` at all, so the record is
    /// the only source.
    pub manifest: CompanyManifest,
    /// The boot-only inputs recorded when this company was registered.
    pub boot: BootInputs,
    /// The live state the successor must adopt rather than reconstruct.
    pub handover: RuntimeHandover,
}

/// A host's ability to rebuild one of its companies with the wiring it booted
/// with.
///
/// Implemented by the binary, because the inputs (`HarnessPool`, the OpenHuman
/// RPC transport, managed media/search backends, the injected per-tenant
/// mailbox) are assembled there from the process environment and feature flags.
/// A host that wires no rebuilder keeps the pre-#290 behaviour exactly: the
/// inference status still reports `restartRequired` and the console still says
/// so, which is the honest answer when a rebuild genuinely is not available.
#[async_trait]
pub trait RuntimeRebuilder: Send + Sync + 'static {
    /// Builds the successor runtime. Must attach `request.handover` to the
    /// builder; returning a runtime built without it is a correctness bug, not a
    /// missed optimisation (see [`RuntimeHandover`]).
    ///
    /// `state` is passed in rather than captured so an implementation can read
    /// the host's opened stores, memory overlay and skill registry without
    /// holding an [`AppState`] that holds it back — that cycle would keep both
    /// alive for the life of the process.
    async fn rebuild(&self, state: &AppState, request: RebuildRequest) -> Result<CompanyRuntime>;
}

/// Rebuilds `id`'s runtime in place and swaps it into the registry.
///
/// Returns the successor. See the [module docs](self) for the sequence, what
/// happens to in-flight work, and the failure behaviour.
///
/// # Errors
///
/// - [`OpenCompanyError::CompanyNotFound`] when `id` is not registered.
/// - [`OpenCompanyError::Config`] when this host wired no [`RuntimeRebuilder`].
/// - Whatever the rebuilder returns, after the outgoing runtime has been resumed.
pub async fn rebuild_company(state: &AppState, id: &CompanyId) -> Result<Arc<CompanyRuntime>> {
    let outgoing = state
        .registry()
        .get(id)
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(id.as_ref().to_string()))?;
    let rebuilder = state.rebuilder().ok_or_else(|| {
        OpenCompanyError::Config(
            "this host cannot rebuild a company runtime in place; restart the process to pick up \
             the new configuration"
                .to_string(),
        )
    })?;

    // Stop accepting cycles and let the one in flight finish. Everything after
    // this point must either swap or resume — never leave the company quiesced.
    outgoing.quiesce().await;

    // Serialize the rebuilder's load-through-save of the company record against
    // every other `company_write_lock` holder (PR #1875 review finding).
    // `RuntimeBuilder::build` reads the persisted record near the top of
    // `build` and does not write the successor record (`store.save_importing`)
    // until near the bottom, with no lock of its own held across that span. A
    // console write racing in between — a name-confirm PATCH, a desk reorder,
    // anything else that serializes on this same lock — can land its `save`
    // inside that window and then have the rebuild's own full-record save
    // silently revert it, because the rebuild read its copy of the record
    // before the concurrent write landed.
    //
    // Taken *after* `quiesce()`, not before it: quiescing waits on `serial`,
    // and an in-flight cycle can itself take this same lock partway through
    // (the orchestrator's `add_agent` tool does), so acquiring this lock ahead
    // of the drain would deadlock — this task waiting on the cycle to finish,
    // the cycle waiting on a lock this task already holds. By the time
    // `quiesce()` returns no cycle is running or can start, so taking the lock
    // here cannot race against that path.
    let write_lock = company_write_lock(id);
    let lock = write_lock.lock().await;

    // The materialized manifest, read *under* the lock above rather than before
    // `quiesce()` (PR #1875 review finding, second pass). `quiesce()` only waits
    // on `serial`, not on `company_write_lock` — a manifest writer such as `PUT
    // …/logo` can load-modify-save the record in the gap between an earlier
    // snapshot and this lock, and every field that snapshot fed into
    // `RebuildRequest` is seed-authoritative in `RuntimeBuilder::build` (every
    // field but `[workflows].enabled`), so the rebuild's own save would silently
    // restore the pre-write value. Reading here closes that: nothing that takes
    // `company_write_lock` can land between this read and the lock this task is
    // already holding.
    //
    // A load failure here must resume `outgoing` before returning, unlike a
    // load failure would have needed to before `quiesce()` — this task already
    // quiesced it.
    let manifest = match outgoing.store().load(id).await {
        Ok(Some(record)) => record.manifest,
        Ok(None) => {
            drop(lock);
            if !state.registry().is_shutting_down() {
                outgoing.resume();
            }
            return Err(OpenCompanyError::CompanyNotFound(format!(
                "{id} is registered but has no persisted record to rebuild from"
            )));
        }
        Err(err) => {
            drop(lock);
            if !state.registry().is_shutting_down() {
                outgoing.resume();
            }
            return Err(err);
        }
    };

    let request = RebuildRequest {
        id: id.clone(),
        manifest,
        boot: state.boot_inputs(id),
        handover: outgoing.handover(),
    };
    let built = match rebuilder.rebuild(state, request).await {
        Ok(runtime) => runtime,
        Err(err) => {
            drop(lock);
            // The stale brain is a worse company; a permanently quiesced one is
            // not a company at all — unless the host is already draining. In that
            // case the shutdown drain has gated this company against new cycles,
            // and re-opening it would admit a turn nothing is waiting for in the
            // seconds before the process exits (issue #986).
            if !state.registry().is_shutting_down() {
                outgoing.resume();
            }
            return Err(err);
        }
    };
    drop(lock);

    let successor = Arc::new(built);
    // Issue #1739: this is the one thing that *changes* a host's cognition
    // after boot. A first inference config rebuilds the runtime from `echo` to
    // `harness`, and an envelope stamped at boot would keep saying `echo` for
    // the life of the process.
    state.analytics().observe_cognition(successor.cognition());
    state.registry().insert(id.clone(), successor.clone());
    tracing::info!(
        company = %id,
        cognition = %successor.cognition().path,
        "rebuilt company runtime in place",
    );
    Ok(successor)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::app::AppConfig;
    use crate::company::CompanyManifest;
    use crate::ports::types::CompanyEvent;
    use crate::runtime::RuntimeBuilder;

    /// A minimal event to drive one cycle with.
    fn tick() -> CompanyEvent {
        CompanyEvent::ScheduleFired {
            cron: "* * * * *".to_string(),
            prompt: "status".to_string(),
        }
    }

    fn manifest() -> CompanyManifest {
        toml::from_str("[company]\nname = \"Acme\"\n").expect("valid manifest")
    }

    fn tmp_home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("opencompany-rebuild-")
            .tempdir()
            .expect("tempdir")
    }

    async fn runtime(home: &std::path::Path, id: &CompanyId) -> CompanyRuntime {
        RuntimeBuilder::new(home.to_path_buf(), manifest())
            .with_id(id.clone())
            .build()
            .await
            .expect("build")
    }

    /// A rebuilder that always builds a fresh runtime over the handover.
    struct Working {
        home: PathBuf,
    }

    #[async_trait]
    impl RuntimeRebuilder for Working {
        async fn rebuild(
            &self,
            _state: &AppState,
            request: RebuildRequest,
        ) -> Result<CompanyRuntime> {
            RuntimeBuilder::new(self.home.clone(), request.manifest)
                .with_id(request.id)
                .with_handover(request.handover)
                .build()
                .await
        }
    }

    /// A rebuilder that always fails, so the failure path is exercised.
    struct Broken;

    #[async_trait]
    impl RuntimeRebuilder for Broken {
        async fn rebuild(
            &self,
            _state: &AppState,
            _request: RebuildRequest,
        ) -> Result<CompanyRuntime> {
            Err(OpenCompanyError::Config("no inference backend".to_string()))
        }
    }

    async fn state_with(home: &std::path::Path, id: &CompanyId) -> AppState {
        let state = AppState::new(AppConfig::default());
        state
            .registry()
            .insert(id.clone(), Arc::new(runtime(home, id).await));
        state.set_boot_inputs(id.clone(), BootInputs::default());
        state
    }

    #[tokio::test]
    async fn a_rebuild_swaps_the_registered_runtime_and_hands_state_over() {
        let home_dir = tmp_home();
        let home = home_dir.path();
        let id = CompanyId::new("acme");
        let state = state_with(home, &id)
            .await
            .with_rebuilder(Arc::new(Working {
                home: home.to_path_buf(),
            }));
        let before = state.registry().get(&id).expect("registered");

        let after = rebuild_company(&state, &id).await.expect("rebuilds");

        // A different runtime is registered...
        assert!(!Arc::ptr_eq(&before, &after));
        assert!(Arc::ptr_eq(
            &state.registry().get(&id).expect("registered"),
            &after
        ));
        // ...and it is accepting work, not stuck in the quiesced window.
        assert!(!after.is_quiesced());
        // The pieces a second instance must never duplicate came across intact.
        assert!(Arc::ptr_eq(before.journal(), after.journal()));
        // The gate itself, not a re-rehydrated copy: an approval waiting on a
        // person keeps its id, its parked effect and its TTL across the swap.
        assert!(Arc::ptr_eq(&before.approval_gate, &after.approval_gate));
        assert!(Arc::ptr_eq(before.events(), after.events()));
        assert!(Arc::ptr_eq(before.store(), after.store()));
        // Same serial lock, so the cycle invariant spans the swap rather than
        // lapsing at it. Read off the fields, like the gate above: every
        // production holder (`CycleRunner::run`, the desk routes, `quiesce`'s
        // drain) takes them the same way, so an accessor here would be a public
        // surface that exists only for this assertion.
        assert!(Arc::ptr_eq(&before.serial, &after.serial));
        assert!(Arc::ptr_eq(&before.per_agent, &after.per_agent));
        assert!(Arc::ptr_eq(&before.task_writes, &after.task_writes));
    }

    #[tokio::test]
    async fn the_successor_runs_cycles_the_outgoing_runtime_now_refuses() {
        let home_dir = tmp_home();
        let home = home_dir.path();
        let id = CompanyId::new("acme");
        let state = state_with(home, &id)
            .await
            .with_rebuilder(Arc::new(Working {
                home: home.to_path_buf(),
            }));
        let outgoing = state.registry().get(&id).expect("registered");

        let successor = rebuild_company(&state, &id).await.expect("rebuilds");

        // The outgoing runtime stays quiesced forever: anything still holding an
        // Arc to it must not keep driving a company that has been replaced.
        let refused = outgoing
            .run_cycle(vec![tick()])
            .await
            .expect_err("a replaced runtime accepts no cycles");
        assert!(
            matches!(refused, OpenCompanyError::Quiescing(_)),
            "{refused}"
        );
        assert_eq!(refused.code(), "quiescing");

        successor
            .run_cycle(vec![tick()])
            .await
            .expect("the successor is live");
    }

    #[tokio::test]
    async fn a_failed_rebuild_resumes_the_company_it_quiesced() {
        // The worst outcome is not a stale brain: it is a company that refuses
        // every cycle because a rebuild died between quiesce and swap.
        let home_dir = tmp_home();
        let home = home_dir.path();
        let id = CompanyId::new("acme");
        let state = state_with(home, &id).await.with_rebuilder(Arc::new(Broken));
        let outgoing = state.registry().get(&id).expect("registered");

        let err = rebuild_company(&state, &id)
            .await
            .expect_err("the rebuilder fails");
        assert!(matches!(err, OpenCompanyError::Config(_)), "{err}");

        assert!(Arc::ptr_eq(
            &state.registry().get(&id).expect("still registered"),
            &outgoing
        ));
        assert!(
            !outgoing.is_quiesced(),
            "a failed rebuild must not park the company"
        );
        outgoing
            .run_cycle(vec![tick()])
            .await
            .expect("the surviving runtime still runs cycles");
    }

    #[tokio::test]
    async fn a_failed_rebuild_during_shutdown_keeps_the_company_quiesced() {
        // The shutdown drain (issue #986) gates every registered company against
        // new cycles. A rebuild that fails *after* the drain began must not
        // `resume()` the outgoing runtime and re-open admission on a company the
        // process is about to leave — that would admit a turn nothing waits for
        // in the seconds before exit.
        let home_dir = tmp_home();
        let home = home_dir.path();
        let id = CompanyId::new("acme");
        let state = state_with(home, &id).await.with_rebuilder(Arc::new(Broken));
        state.registry().begin_shutdown();
        let outgoing = state.registry().get(&id).expect("registered");

        let err = rebuild_company(&state, &id)
            .await
            .expect_err("the rebuilder fails");
        assert!(matches!(err, OpenCompanyError::Config(_)), "{err}");

        assert!(
            outgoing.is_quiesced(),
            "a failed rebuild during shutdown must keep the company gated"
        );
        assert!(
            outgoing.run_cycle(vec![tick()]).await.is_err(),
            "a company gated by shutdown must refuse a cycle"
        );
    }

    #[tokio::test]
    async fn a_host_with_no_rebuilder_says_so_instead_of_quiescing() {
        let home_dir = tmp_home();
        let home = home_dir.path();
        let id = CompanyId::new("acme");
        let state = state_with(home, &id).await;

        let err = rebuild_company(&state, &id)
            .await
            .expect_err("no rebuilder is wired");
        assert!(matches!(err, OpenCompanyError::Config(_)), "{err}");
        // Crucially, the check happens before the quiesce.
        assert!(!state.registry().get(&id).expect("registered").is_quiesced());
    }

    /// `rebuild_company` must serialize the rebuilder's load-through-save of
    /// the company record against `company_write_lock` (PR #1875 review
    /// finding): `RuntimeBuilder::build` reads the persisted record near the
    /// top and does not save its successor until near the bottom, and
    /// without this lock a console write racing in between — a name-confirm
    /// PATCH, a desk reorder — could land its save inside that window only
    /// to have the rebuild's own full-record save silently revert it. Proven
    /// the same way `set_lifecycle_serializes_against_the_company_write_lock`
    /// (`company/runtime.rs`) proves it for that method: hold the lock
    /// externally, drive the real call, and demand it cannot finish while
    /// the lock is held.
    #[tokio::test]
    async fn rebuild_company_serializes_against_the_company_write_lock() {
        let home_dir = tmp_home();
        let home = home_dir.path();
        let id = CompanyId::new("acme");
        let state = state_with(home, &id)
            .await
            .with_rebuilder(Arc::new(Working {
                home: home.to_path_buf(),
            }));

        let lock = crate::ports::store::company_write_lock(&id);
        let guard = lock.lock().await;

        let state_for_task = state.clone();
        let id_for_task = id.clone();
        let mut task =
            tokio::spawn(async move { rebuild_company(&state_for_task, &id_for_task).await });

        // The rebuild must be blocked behind the held lock — give it every
        // chance to (wrongly) race ahead before declaring it stuck.
        let raced_ahead = tokio::time::timeout(std::time::Duration::from_millis(200), &mut task)
            .await
            .is_ok();
        assert!(
            !raced_ahead,
            "rebuild_company completed while company_write_lock was held \
             elsewhere — it is not serializing its load-through-save of the \
             company record against a concurrent writer (e.g. a racing \
             name-confirm PATCH), which can silently revert that writer's \
             save"
        );

        drop(guard);
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("rebuild_company never resumed after the lock was released")
            .expect("task panicked")
            .expect("rebuild_company failed");
    }

    /// PR #1875 review finding (second pass): a manifest snapshot taken
    /// *before* `quiesce()` — rather than under the `company_write_lock` this
    /// function already holds by the time it calls the rebuilder — leaves a
    /// window `quiesce()`'s own wait can outlast: `quiesce()` blocks on
    /// `serial`, not on `company_write_lock`, so a writer like `PUT …/logo`
    /// (load-modify-save under `company_write_lock`) can complete entirely
    /// while `quiesce()` is still draining an in-flight cycle — long before
    /// this function reaches its own lock. `RuntimeBuilder::build` treats
    /// every manifest field but `[workflows].enabled` as seed-authoritative,
    /// i.e. taken from the snapshot handed to it rather than re-read from the
    /// store, so a stale snapshot's full-record save silently reverts that
    /// write. This reproduces the race directly: hold `serial` to stand in
    /// for the in-flight cycle, let the write land while `rebuild_company` is
    /// parked in `quiesce()`, then release it and demand the write survived.
    #[tokio::test]
    async fn a_rebuild_does_not_revert_a_manifest_write_that_lands_during_quiesce() {
        let home_dir = tmp_home();
        let home = home_dir.path();
        let id = CompanyId::new("acme");
        let seed: CompanyManifest =
            toml::from_str("[company]\nname = \"Acme\"\nlogo_url = \"old-logo\"\n")
                .expect("valid manifest");
        let outgoing = Arc::new(
            RuntimeBuilder::new(home.to_path_buf(), seed)
                .with_id(id.clone())
                .build()
                .await
                .expect("build"),
        );
        let state = AppState::new(AppConfig::default());
        state.registry().insert(id.clone(), outgoing.clone());
        state.set_boot_inputs(id.clone(), BootInputs::default());
        let state = state.with_rebuilder(Arc::new(Working {
            home: home.to_path_buf(),
        }));

        // Stand in for a cycle already in flight: `quiesce()` cannot return
        // until this is released.
        let in_flight = outgoing.serial.clone().lock_owned().await;

        let state_for_task = state.clone();
        let id_for_task = id.clone();
        let task =
            tokio::spawn(async move { rebuild_company(&state_for_task, &id_for_task).await });

        // Give the spawned task every chance to reach the blocked
        // `quiesce()` await before the write below lands. A fixed yield
        // count only hopes the spawned task got scheduled in time; this
        // instead polls `is_quiesced()`, which `quiesce()` sets *before*
        // ever awaiting `serial` (`CompanyRuntime::quiesce`'s own doc), so
        // seeing it flip proves `rebuild_company` actually reached that
        // point rather than merely having had the chance to (CodeRabbit,
        // PR #1875 review). Bounded so a real regression here fails fast
        // instead of hanging the suite.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !outgoing.is_quiesced() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("rebuild_company never reached quiesce() before the timeout");

        // The concurrent write: lock, load-modify-save, unlock — exactly the
        // shape `PUT …/logo` takes (`src/server/ops/company_logo.rs`).
        {
            let write_lock = company_write_lock(&id);
            let _guard = write_lock.lock().await;
            let mut record = outgoing
                .store()
                .load(&id)
                .await
                .expect("load")
                .expect("record exists");
            record.manifest.company.logo_url = Some("new-logo".to_string());
            outgoing.store().save(&record).await.expect("save");
        }

        // Let the "in-flight cycle" finish, unblocking `quiesce()`.
        drop(in_flight);

        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("rebuild_company never finished")
            .expect("task panicked")
            .expect("rebuild_company failed");

        let persisted = outgoing
            .store()
            .load(&id)
            .await
            .expect("load")
            .expect("record exists");
        assert_eq!(
            persisted.manifest.company.logo_url.as_deref(),
            Some("new-logo"),
            "the rebuild's own save must not revert a manifest write that \
             landed under company_write_lock while quiesce() was still \
             draining the in-flight cycle"
        );
    }

    #[tokio::test]
    async fn rebuilding_an_unregistered_company_is_a_not_found() {
        let state = AppState::new(AppConfig::default());
        let err = rebuild_company(&state, &CompanyId::new("nobody"))
            .await
            .expect_err("nothing to rebuild");
        assert!(matches!(err, OpenCompanyError::CompanyNotFound(_)), "{err}");
    }
}
