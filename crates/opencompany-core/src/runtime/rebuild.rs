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
#[derive(Clone, Debug, Default)]
pub struct BootInputs {
    /// The company's on-disk source directory (`companies/<name>`), used to seed
    /// the workspace and resolve committed skills/workflows.
    pub source_dir: Option<PathBuf>,
}

/// Everything a [`RuntimeRebuilder`] needs to produce a successor runtime.
pub struct RebuildRequest {
    /// The company being rebuilt. Already registered; the successor keeps this id.
    pub id: CompanyId,
    /// The company's **materialized** manifest, read from its persisted record
    /// rather than re-parsed from disk.
    ///
    /// This is the manifest the running company actually has, which matters
    /// because a fresh `company.toml` read would drop the console-created
    /// workflows merged into `[workflows].enabled`. A platform-provisioned
    /// tenant has no `company.toml` at all, so the record is the only source.
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
#[path = "rebuild_tests.rs"]
mod tests;
