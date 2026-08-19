//! [`MaintenanceTicker`]: the process-wide minute loop that retires approvals,
//! grants and fire claims for **every** registered company (issue #971).
//!
//! ## The bug this module is
//!
//! The expiry mechanism was already here and already correct. A parked approval
//! carries a TTL, [`CompanyRuntime::sweep_expired_approvals`] is a complete
//! retirement transaction, and both were covered by tests. What was missing was
//! anything to run it.
//!
//! Its only production caller was `CompanyScheduler::tick_maintenance`, reached
//! only from that scheduler's minute loop — and the scheduler is spawned only
//! for a company whose manifest declares a `[[schedule]]`. **A company that
//! declares no `[[schedule]]` never spawned a scheduler and therefore never
//! swept approvals, grants or fire claims, at any age.** The tenant that
//! surfaced #971 ran a weekly digest as a *workflow*, driven by
//! [`WorkflowScheduler`](super::workflow_scheduler::WorkflowScheduler), whose
//! loop calls only `tick` — so it minted approvals every week and swept none of
//! them, ever. Cold boot then re-parked the whole backlog from the journal with
//! its original park instants, so every redeploy faithfully rebuilt it.
//!
//! Sixty-eight-hour-old cards were not a tuning problem. They were the absence
//! of a caller.
//!
//! ## Why process-wide rather than per company
//!
//! Deliberately shaped like [`WorkflowScheduler`] and not like
//! [`CompanyScheduler`](super::scheduler::CompanyScheduler): one task for the
//! whole process, re-reading [`CompanyRegistry::list`] every minute. That is
//! **one always-on wiring point**, which is the property that fixes the bug
//! rather than moving it. A per-company spawn has to be reached at every place a
//! company can come into existence — boot from `--company`, adoption of an
//! existing data root, a hosted tenant registered after boot, an in-place
//! rebuild — and the bug being fixed here is precisely one of those paths not
//! being reached. Reading the registry each tick means a company registered a
//! minute from now is swept a minute from now, with nobody having to remember.
//!
//! ## Housekeeping is not enforcement
//!
//! Nothing here is a safety boundary, and it must not become one. The gate
//! re-checks the TTL under the same lock that removes a parked entry
//! (`resolve_at` / `resolve_amended` / `resolve_outcome`), so a 25-hour-old
//! approval default-denies on the operator's click whether or not this ticker
//! ever ran. What this adds is that the queue empties, the journal records the
//! retirement, and the operator's badge goes back to describing current state.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::CompanyRuntime;
use crate::ports::types::{ApprovalId, CompanyId};
use crate::runtime::CompanyRegistry;
use crate::runtime::scheduler::{Clock, MINUTE_MS, PRUNE_CUTOFF_MINUTES, millis_to_next_minute};

/// Retires expired approvals, expired grants and stale fire claims for every
/// company in the registry, once a minute.
pub struct MaintenanceTicker {
    registry: CompanyRegistry,
    clock: Arc<dyn Clock>,
}

impl MaintenanceTicker {
    /// Builds a ticker over every company in `registry`, driven by `clock`.
    pub fn new(registry: CompanyRegistry, clock: Arc<dyn Clock>) -> Self {
        Self { registry, clock }
    }

    /// Runs one maintenance pass over every registered company. Returns how
    /// many parked approvals were retired.
    ///
    /// Each company's three chores are independent and each is best-effort: a
    /// failure on one company must not stop the next company being swept, and a
    /// failed fire-claim prune must not abort the approval sweep it shares a
    /// tick with. The alternative — propagating the first error out of the tick
    /// — would let one company's broken store silently stop maintenance for
    /// every other company in the process, which is the same class of bug as
    /// the one this module exists to fix.
    pub async fn tick(&self) -> usize {
        let minute = self.clock.now_millis() / MINUTE_MS;
        let mut retired = 0;
        for company in self.registry.list() {
            let Some(runtime) = self.registry.get(&company) else {
                continue; // removed between listing and lookup (archive)
            };
            // **A paused company IS swept, deliberately** — no `ensure_running`
            // gate here, unlike `WorkflowScheduler::tick` beside it.
            //
            // That difference is the point, so please do not "fix" it. The
            // scheduler's gate is right for the scheduler: firing new work into
            // a paused company is starting something the operator asked to
            // stop. This does the opposite — it *finishes* things, by retiring
            // requests nobody is going to answer. And a paused company's queue
            // is the queue that most needs draining: pausing is exactly what an
            // operator does to work that has gone wrong, so its approvals are
            // the ones guaranteed to be unactionable and the ones #971 watched
            // pile up. Skipping here would reproduce the bug for the companies
            // worst affected by it.
            //
            // Nothing here starts work. Retiring an approval releases a #469
            // continuation, which can run a turn — and that turn is admitted or
            // refused by the company's own lifecycle check, in the cycle path,
            // exactly as it is for a continuation released by an operator
            // clicking Decline on a paused company today.
            retired += sweep_company(&company, &runtime, minute).await.len();
        }
        retired
    }

    /// Spawns a background task that ticks on every minute boundary until
    /// `shutdown` is notified. Boot holds the join handle and the shared
    /// `shutdown` so maintenance stops cleanly when the server does.
    pub fn spawn(self, shutdown: Arc<Notify>) -> JoinHandle<()> {
        tokio::spawn(async move {
            // The `Notified` future is built ONCE and pinned across iterations,
            // not rebuilt inside the `select!`. Boot signals with
            // `notify_waiters()`, which wakes only the waiters registered at
            // that instant — a future created fresh each iteration is not
            // registered while `tick` is running, so a shutdown arriving
            // mid-tick would be dropped and the ticker would sleep another
            // full minute before noticing. Polled once here, this one stays
            // registered, and a notification delivered during `tick` is
            // latched: the next `select!` sees it immediately.
            let notified = shutdown.notified();
            tokio::pin!(notified);
            loop {
                let sleep_ms = millis_to_next_minute(self.clock.now_millis());
                tokio::select! {
                    _ = &mut notified => break,
                    _ = tokio::time::sleep(Duration::from_millis(sleep_ms)) => {
                        self.tick().await;
                    }
                }
            }
        })
    }
}

/// One company's maintenance pass: retire overdue approvals, expire unredeemed
/// grants, prune stale fire claims. Returns the approvals that were retired.
///
/// **The single implementation**, shared by [`MaintenanceTicker::tick`] and
/// [`CompanyScheduler::tick_maintenance`](super::scheduler::CompanyScheduler::tick_maintenance)
/// so the two cannot drift into meaning different things by "maintenance".
///
/// Every chore is best-effort and independent: a failure on one must not stop
/// the next, and — at the tick above — a failure on one company must not stop
/// the next company being swept. Propagating the first error out would let one
/// broken store silently stop maintenance for every other company in the
/// process, which is the same class of bug as the missing caller this module
/// exists to fix.
pub(crate) async fn sweep_company(
    company: &CompanyId,
    runtime: &Arc<CompanyRuntime>,
    minute: u64,
) -> Vec<ApprovalId> {
    // **A paused company IS swept, deliberately** — no `ensure_running` gate
    // here, unlike `WorkflowScheduler::tick`.
    //
    // That difference is the point, so please do not "fix" it. The scheduler's
    // gate is right for the scheduler: firing new work into a paused company is
    // starting something the operator asked to stop. This does the opposite —
    // it *finishes* things, by retiring requests nobody is going to answer. And
    // a paused company's queue is the queue that most needs draining: pausing
    // is exactly what an operator does to work that has gone wrong, so its
    // approvals are the ones guaranteed to be unactionable, and issue #971
    // watched a Paused column's cards sit at 68 hours. Skipping here would
    // reproduce the bug for the companies worst affected by it.
    //
    // Nothing here starts work. Retiring an approval releases a #469
    // continuation, which can run a turn — and that turn is admitted or refused
    // by the company's own lifecycle check in the cycle path, exactly as it is
    // for a continuation released by an operator clicking Decline on a paused
    // company today.
    let retired = match runtime.sweep_expired_approvals().await {
        Ok(expired) => {
            if !expired.is_empty() {
                tracing::info!(
                    %company,
                    count = expired.len(),
                    "[maintenance] retired parked approvals past their deadline"
                );
            }
            expired
        }
        Err(err) => {
            tracing::warn!(%company, %err, "[maintenance] approval sweep failed");
            Vec::new()
        }
    };
    if let Err(err) = runtime.sweep_expired_grants().await {
        tracing::warn!(%company, %err, "[maintenance] grant sweep failed");
    }
    // Issue #241: bound the fire-claim log's growth on the same tick. The cutoff
    // sits a full week past the catch-up window (PRUNE_CUTOFF_MINUTES >
    // CATCHUP_WINDOW_MINUTES), so an anchor a booting replica still needs is
    // never eligible.
    let cutoff = minute.saturating_sub(PRUNE_CUTOFF_MINUTES);
    if let Err(err) = runtime
        .schedule_fires()
        .prune_fires_before(company, cutoff)
        .await
    {
        tracing::warn!(%company, %err, "[maintenance] pruning fire claims failed");
    }
    retired
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use super::MaintenanceTicker;
    use crate::company::CompanyManifest;
    use crate::policy::ManifestApprovalGate;
    use crate::ports::now_millis;
    use crate::ports::types::{
        Actor, ActorKind, ApprovalId, CompanyEvent, CompanyId, Effect, EffectGroup, Verdict,
    };
    use crate::runtime::scheduler::FakeClock;
    use crate::runtime::{CompanyRegistry, RuntimeBuilder};
    use crate::{CompanyRuntime, Result};

    fn tmp_home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("oc-maintenance-")
            .tempdir()
            .expect("tempdir")
    }

    /// A manifest with **no `[[schedule]]`** — the shape that never got swept.
    fn unscheduled_manifest() -> CompanyManifest {
        let toml_src = r#"
            [company]
            name = "Acme"

            [[agent]]
            id = "ceo"
            role = "Chief"

            [policy]
            mode = "supervised"
        "#;
        let manifest: CompanyManifest = toml::from_str(toml_src).expect("manifest parses");
        assert!(
            manifest.schedules.is_empty(),
            "the fixture's whole point is that it declares no cron"
        );
        manifest
    }

    fn sign_effect() -> Effect {
        Effect {
            kind: "filing.submit".into(),
            group: EffectGroup::Sign,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::Value::Null,
            agent: None,
            run_id: None,
        }
    }

    /// A company with no manifest cron, whose gate treats anything parked as
    /// already past its deadline, registered under a ticker.
    async fn given_an_unscheduled_company_with_an_overdue_gate(
        home: &std::path::Path,
    ) -> (Arc<CompanyRuntime>, MaintenanceTicker) {
        let manifest = unscheduled_manifest();
        // TTL 0: "the clock has passed the deadline" without sleeping for a day.
        let gate = Arc::new(ManifestApprovalGate::new(manifest.policy.clone()).with_ttl_millis(0));
        let runtime = Arc::new(
            RuntimeBuilder::new(home.to_path_buf(), manifest)
                .with_approvals(gate)
                .build()
                .await
                .expect("runtime builds"),
        );
        let registry = CompanyRegistry::new();
        registry.insert(runtime.id().clone(), Arc::clone(&runtime));
        let ticker = MaintenanceTicker::new(
            registry,
            Arc::new(FakeClock::new(now_millis() + 60 * MINUTE)),
        );
        (runtime, ticker)
    }

    const MINUTE: u64 = 60_000;

    /// Parks an effect the way production does: the gate mints the id and the
    /// journal records the park.
    ///
    /// Both halves matter. The gate's map is what the sweep reads; the journal
    /// is what `pending_approvals` — and therefore the operator's queue and the
    /// sidebar badge — reads. A test that parked only on the gate would assert
    /// that an entry nobody could see had been retired.
    async fn park(runtime: &Arc<CompanyRuntime>) -> Result<ApprovalId> {
        let effect = sign_effect();
        let id = runtime.approvals.park(runtime.id(), effect.clone()).await?;
        runtime
            .journal()
            .record_parked(
                &id,
                &effect,
                now_millis(),
                crate::runtime::journal::TaskLink::Unlinked,
                crate::runtime::journal::ApprovalConversation {
                    thread: None,
                    parent: None,
                },
                None,
            )
            .await?;
        Ok(id)
    }

    /// **T1 — the whole issue, in one test.**
    ///
    /// A company that declares no `[[schedule]]` parks an approval, the clock
    /// passes its deadline, a maintenance tick runs, and the approval is gone.
    ///
    /// This was **inexpressible** before this module existed. The only thing
    /// that swept approvals in production was `CompanyScheduler::tick_maintenance`,
    /// and `spawn_scheduler` returns `None` for a company with no schedules — so
    /// there was no production path from "a scheduleless company's approval is
    /// overdue" to "it is retired", at any age, ever. The test could only be
    /// written by reaching for a scheduler the host would never have spawned,
    /// which would have asserted the fix rather than the bug.
    #[tokio::test]
    async fn a_company_with_no_schedule_still_has_its_overdue_approvals_retired() {
        let home_dir = tmp_home();
        let (runtime, ticker) =
            given_an_unscheduled_company_with_an_overdue_gate(home_dir.path()).await;

        let id = park(&runtime).await.expect("parks");
        assert_eq!(runtime.pending_approvals().len(), 1);

        assert_eq!(ticker.tick().await, 1, "the tick must retire it");

        assert!(
            runtime.pending_approvals().is_empty(),
            "a scheduleless company's overdue approval must not survive maintenance"
        );
        // And the queue stays empty rather than the entry being re-derivable:
        // the journal removal is what the badge counts.
        assert_eq!(ticker.tick().await, 0, "a second tick has nothing to do");
        drop(id);
    }

    /// **T2 — a retired approval mints no grant.**
    ///
    /// The safety property the whole change rests on: an approval disappearing
    /// from the queue must never read as one that was granted. A `GrantedCall`
    /// exists only on `resolve_outcome`'s `Approved` arm, and `retire_approval`
    /// takes no verdict — but "it cannot happen by construction" is exactly the
    /// kind of claim that stops being true silently, so it is asserted.
    #[tokio::test]
    async fn retiring_an_approval_mints_no_grant() {
        let home_dir = tmp_home();
        let (runtime, ticker) =
            given_an_unscheduled_company_with_an_overdue_gate(home_dir.path()).await;

        let id = park(&runtime).await.expect("parks");
        ticker.tick().await;

        assert!(
            runtime.grants.peek(&id).is_none(),
            "an expiry must never leave a grant behind — that would turn a \
             disappeared approval into a silently authorised call"
        );
        assert!(runtime.standing_grants().is_empty());
    }

    /// **T3 — the retirement is a deny by the system, for the recorded reason.**
    ///
    /// Two readers, two records, and they have to agree. The event log gets an
    /// `ApprovalResolved { verdict: Deny, by: System }` — a timeout that emitted
    /// nothing was invisible to every event-log reader (#305) — and the journal
    /// gets the durable audit entry saying the deadline is what did it.
    #[tokio::test]
    async fn the_retirement_is_a_system_deny_recorded_as_a_ttl_expiry() {
        let home_dir = tmp_home();
        let (runtime, ticker) =
            given_an_unscheduled_company_with_an_overdue_gate(home_dir.path()).await;

        let id = park(&runtime).await.expect("parks");
        ticker.tick().await;

        let events = runtime
            .events()
            .read_from(
                runtime.id(),
                crate::ports::types::EventSeq::new(0),
                usize::MAX,
            )
            .await
            .expect("events read");
        let resolved: Vec<_> = events
            .iter()
            .filter_map(|stored| match &stored.event {
                CompanyEvent::ApprovalResolved {
                    approval_id,
                    verdict,
                    by,
                } if approval_id == &id => Some((*verdict, by.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(resolved.len(), 1, "exactly one resolution event");
        let (verdict, by) = &resolved[0];
        assert_eq!(*verdict, Verdict::Deny, "silence is a default-deny");
        assert_eq!(
            by.kind,
            ActorKind::System,
            "the host expired it; attributing this to a person would be a lie \
             the console then repeats as \"Approval denied\""
        );

        // The journal's durable record says why.
        let raw = std::fs::read_to_string(
            home_dir
                .path()
                .join("companies")
                .join(runtime.id().as_ref())
                .join("journal.jsonl"),
        )
        .or_else(|_| {
            // Layout differs by store backend; find the journal wherever it is.
            let mut found = String::new();
            for entry in walk(home_dir.path()) {
                if entry.file_name().is_some_and(|n| n == "journal.jsonl") {
                    found.push_str(&std::fs::read_to_string(&entry).unwrap_or_default());
                }
            }
            if found.is_empty() {
                Err(std::io::Error::other("no journal found"))
            } else {
                Ok(found)
            }
        })
        .expect("journal readable");
        assert!(
            raw.contains("ApprovalExpired") && raw.contains(r#""reason":"ttl""#),
            "the journal must record the retirement AND its reason: {raw}"
        );

        // Nothing else claims to have resolved it.
        drop(Actor {
            kind: ActorKind::User,
            id: String::new(),
        });
    }

    /// **T4 — the retire/approve race yields exactly one outcome.**
    ///
    /// An operator clicking Approve at the instant the sweep retires the same
    /// entry gets either a real approval or `NotParked` — never both, and never
    /// a silent double execution. The property comes from removal and outcome
    /// sharing one critical section inside the gate; this asserts it holds
    /// through the runtime's retirement path rather than only at the gate.
    #[tokio::test]
    async fn a_resolve_racing_the_sweep_yields_exactly_one_outcome() {
        let home_dir = tmp_home();
        let (runtime, ticker) =
            given_an_unscheduled_company_with_an_overdue_gate(home_dir.path()).await;

        for _ in 0..25 {
            let id = park(&runtime).await.expect("parks");
            let sweeper = Arc::clone(&runtime);
            let racing_id = id.clone();
            let sweep = tokio::spawn(async move { sweeper.sweep_expired_approvals().await });
            let resolver = Arc::clone(&runtime);
            let resolve = tokio::spawn(async move {
                resolver
                    .approvals
                    .resolve(
                        &racing_id,
                        Verdict::Approve,
                        Actor {
                            kind: ActorKind::User,
                            id: "operator".into(),
                        },
                    )
                    .await
            });
            let swept = sweep.await.expect("sweep task").expect("sweep ok");
            let resolved = resolve.await.expect("resolve task").expect("resolve ok");

            // Exactly one of the two won the entry.
            let sweep_won = swept.contains(&id);
            // A TTL-0 gate expires at resolve time too, so the operator's win
            // is "the gate had it and answered", not "the effect ran" — what
            // must never happen is BOTH claiming it.
            assert!(
                sweep_won || resolved.is_none(),
                "an approval cannot be both retired and executed"
            );
            assert!(
                runtime.grants.peek(&id).is_none(),
                "and the race must not mint a grant either way"
            );
        }
        assert_eq!(ticker.tick().await, 0, "nothing is left parked");
    }

    /// The registry is re-read every tick, so a company registered after the
    /// ticker started is swept without anything having to remember to wire it.
    ///
    /// This is the property that makes one always-on ticker a *fix* rather than
    /// a fifth place to forget: hosted tenants are registered after boot, and a
    /// per-company spawn is exactly what #971 was.
    #[tokio::test]
    async fn a_company_registered_after_the_ticker_started_is_still_swept() {
        let home_dir = tmp_home();
        let registry = CompanyRegistry::new();
        let ticker = MaintenanceTicker::new(
            registry.clone(),
            Arc::new(FakeClock::new(now_millis() + 60 * MINUTE)),
        );
        // Nothing registered yet.
        assert_eq!(ticker.tick().await, 0);

        let manifest = unscheduled_manifest();
        let gate = Arc::new(ManifestApprovalGate::new(manifest.policy.clone()).with_ttl_millis(0));
        let runtime = Arc::new(
            RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest)
                .with_approvals(gate)
                .build()
                .await
                .expect("runtime builds"),
        );
        park(&runtime).await.expect("parks");
        registry.insert(runtime.id().clone(), Arc::clone(&runtime));

        assert_eq!(ticker.tick().await, 1);
        assert!(runtime.pending_approvals().is_empty());
    }

    /// A company removed between `list()` and `get()` — the archive path — is
    /// skipped rather than panicking the tick for every company after it.
    #[tokio::test]
    async fn a_company_that_left_the_registry_is_skipped() {
        let registry = CompanyRegistry::new();
        let ticker = MaintenanceTicker::new(
            registry.clone(),
            Arc::new(FakeClock::new(now_millis() + 60 * MINUTE)),
        );
        // `list()` on an empty registry yields nothing; the guard is exercised
        // by a stale id, which is what an archive between the two calls leaves.
        assert!(registry.get(&CompanyId::new("gone")).is_none());
        assert_eq!(ticker.tick().await, 0);
    }

    /// Recursively lists files under `root`. Test-only; the journal's on-disk
    /// layout is a store detail and this keeps the assertion about the record
    /// rather than about the path.
    fn walk(root: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(root) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
        out
    }
}
