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
//! Its only production caller was the per-company
//! [`CompanyScheduler`](super::scheduler::CompanyScheduler)'s minute loop — and
//! the scheduler is spawned only for a company whose manifest declares a
//! `[[schedule]]`. **A company that declares no `[[schedule]]` never spawned a
//! scheduler and therefore never swept approvals, grants or fire claims, at any
//! age.** The tenant that
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
use crate::ports::types::{ApprovalId, CompanyId, EvictionPolicy};
use crate::runtime::CompanyRegistry;
use crate::runtime::scheduler::{Clock, MINUTE_MS, PRUNE_CUTOFF_MINUTES, millis_to_next_minute};

/// Number of completed-cycle traces retained for one company.
///
/// Trace summaries are not yet a recall mechanism (#1175), but they remain
/// useful in the export bundle and through the inspection route. Keeping this
/// small, fixed window bounds every backend until a real compression and recall
/// design supplies a policy with stronger product semantics.
pub(crate) const TRACE_RETENTION_LIMIT: usize = 32;

/// Retires the registry entry, and both ownership records, of a company whose
/// archive left it registered.
///
/// The maintenance loop owns no `AppState`, so the cleanup archive performs
/// inline is injected as this trait. The production implementation lives beside
/// `archive` in `server::provision` and runs the same three removals.
#[async_trait::async_trait]
pub trait CompanyEvictor: Send + Sync {
    /// Removes `company` from the registry and drops its ownership rows —
    /// but only if the runtime still registered under `company` is the exact
    /// instance `expected` names. `expected` is the runtime this call site
    /// itself just read `status()` as `"archived"` from; passing it lets the
    /// implementation refuse to remove a replacement that has since taken
    /// the id over (a rebuild swap, the production case) instead of evicting
    /// whatever it finds by id alone (codex review on #1943, PR comment
    /// 3894439351).
    async fn evict(&self, company: &CompanyId, expected: &Arc<CompanyRuntime>);
}

/// Retires expired approvals, expired grants and stale fire claims for every
/// company in the registry, once a minute.
pub struct MaintenanceTicker {
    registry: CompanyRegistry,
    clock: Arc<dyn Clock>,
    evictor: Option<Arc<dyn CompanyEvictor>>,
}

impl MaintenanceTicker {
    /// Builds a ticker over every company in `registry`, driven by `clock`.
    pub fn new(registry: CompanyRegistry, clock: Arc<dyn Clock>) -> Self {
        Self {
            registry,
            clock,
            evictor: None,
        }
    }

    /// Wires the eviction hook that retires a company left registered after its
    /// archive persisted `lifecycle: "archived"` but skipped registry cleanup.
    pub fn with_evictor(mut self, evictor: Arc<dyn CompanyEvictor>) -> Self {
        self.evictor = Some(evictor);
        self
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

            // Retire a company whose archive persisted `lifecycle: "archived"`
            // but left it registered (the stranded-cleanup path).
            if let Some(evictor) = &self.evictor {
                let status = runtime.status().await;
                if let Err(err) = &status {
                    tracing::warn!(%company, %err, "[maintenance] archive-eviction status read failed");
                }
                if should_evict_archived(&status) {
                    tracing::info!(
                        %company,
                        "[maintenance] evicting a company left registered after archive"
                    );
                    evictor.evict(&company, &runtime).await;
                }
            }
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

/// Whether a re-read `status()` marks a still-registered company for eviction.
///
/// `archived` alone qualifies — `running`/`paused`/`suspended` are left
/// registered — and a read failure (`Err`) defers rather than evicting, so an
/// unproven failure never removes a company, mirroring `archive`'s own default.
fn should_evict_archived(status: &crate::Result<crate::runtime::types::CompanyStatus>) -> bool {
    matches!(status, Ok(status) if status.lifecycle == "archived")
}

/// One company's maintenance pass: retire overdue approvals, expire unredeemed
/// grants, prune stale fire claims. Returns the approvals that were retired.
///
/// **The single implementation**, driven by [`MaintenanceTicker::tick`].
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
    if let Err(err) = runtime
        .memory
        .evict(
            company,
            EvictionPolicy::KeepRecent {
                n: TRACE_RETENTION_LIMIT,
            },
        )
        .await
    {
        tracing::warn!(%company, %err, "[maintenance] trace retention sweep failed");
    }
    retired
}

#[cfg(test)]
#[path = "maintenance_tests.rs"]
mod tests;
