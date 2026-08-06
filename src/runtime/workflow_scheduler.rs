//! [`WorkflowScheduler`]: fires saved workflows whose trigger carries a cron.
//!
//! A workflow graph's `trigger` node may carry a `schedule` — a standard 5-field
//! cron expression, always UTC (issue #169). Without this scheduler that field
//! would be inert: a workflow would still only run when an operator clicked Run.
//! This module is the half that makes a saved schedule actually fire.
//!
//! It is deliberately shaped like the manifest-cron
//! [`CompanyScheduler`](super::scheduler::CompanyScheduler) — same injectable
//! [`Clock`], same minute-boundary sleep loop, same
//! [`spawn`](WorkflowScheduler::spawn) shape — and reuses the same
//! [`CronExpr`] matcher, so the two schedules speak one dialect and no new
//! dependency is introduced. Three things differ, each for a reason:
//!
//! * **One process-global task, not one per company.** Workflow schedules
//!   mutate at runtime (creating a workflow adds a cron with no reboot) and a
//!   hosted tenant can be provisioned after boot, so the tick re-reads
//!   [`CompanyRegistry::list`] every minute rather than snapshotting companies
//!   at boot.
//! * **Enumeration goes through the seed ∪ overlay union** (issue #168's
//!   [`list_workflows_union`]), never a raw `source_dir` scan and never the
//!   manifest's `[workflows].enabled` list — a console-created workflow lives
//!   only on the record overlay, and it is exactly the kind an operator attaches
//!   a schedule to. See [`WorkflowScheduler::tick`] for why the enabled list is
//!   not a filter.
//! * **Each fire runs on its own tokio task**, so one long agent run cannot
//!   starve every other company's schedule, with an in-flight guard so a slow
//!   run is never overlapped by its own next tick.
//!
//! Missed runs are skipped, never caught up — identical to the manifest cron
//! scheduler, so a restart after downtime does not burst.
//!
//! ## Report delivery on a scheduled run
//!
//! An `output` node may route its report to a person or a channel (issue #170).
//! The runner delivers it either way — the scheduler drives the same
//! [`WorkflowRunner`] port the console's Run button does — but only a manual run
//! gets the [`WorkflowRun::deliveries`](crate::ports::WorkflowRun) rows back in
//! an HTTP response for the console to render. A scheduled run has no response
//! and no one watching, so this module logs them instead.
//!
//! **Be clear about the ceiling of that.** A log line reaches whoever can read
//! the host's stdout. On a self-hosted deployment that is the operator; on a
//! hosted tenant it is emphatically not — it is us. So the log makes a failed
//! scheduled delivery *diagnosable*, not *operator-visible*.
//!
//! Issue #228 closes that gap without inventing a subsystem: every finished run
//! — this scheduler's and the console's Run button alike — is journaled as a
//! [`CompanyEvent::WorkflowRunFinished`](crate::ports::types::CompanyEvent)
//! through [`record_run_finished`](crate::runtime::record_run_finished), projected
//! live onto the operator SSE stream,
//! and read back durably from `GET …/workflows/runs`. The log lines below stay
//! exactly as they were: they remain the platform team's diagnostic, and the
//! event is the operator's surface. The two answer to different readers, so
//! neither replaces the other.
//!
//! **That split decides what the log lines may say.** Because host stdout is a
//! platform surface, the undelivered-report warning below carries only fields
//! that are safe for a reader who is not the tenant: the company, the workflow,
//! the node, the destination *kind*, whether a target resolved at all, the
//! status, and a [`DeliveryReason`](crate::ports::DeliveryReason). It does
//! **not** carry the target, and — since issue #248 — it does not carry
//! [`DeliveryReport::detail`](crate::ports::DeliveryReport) either: `detail`
//! interpolates the transport's own text on the failure arms, and a mail
//! transport quotes the mailbox it refused. `DeliveryReason` is the closed set
//! that says the same thing about the failure without the ability to carry the
//! address. The full `detail` still reaches the operator, through the run
//! response and the journaled event above.
//!
//! (A run outcome is deliberately *not* modelled as issue #242's first-class
//! `RunRecord`. That is a task-attempt record minted at the task dispatch choke
//! point and keyed to a board task with an attempt ordinal; a workflow run
//! enters through the [`WorkflowRunner`] port, has no task, and produces
//! host-side delivery rows per output node. The shapes don't meet.)

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::company::{WorkflowFile, list_workflows_union};
use crate::ports::types::CompanyId;
use crate::ports::{DeliveryReport, DeliveryStatus};
use crate::runtime::CompanyRegistry;
use crate::runtime::cron::{CivilTime, CronExpr};
use crate::runtime::scheduler::{Clock, MINUTE_MS, millis_to_next_minute};

/// Identifies one schedulable workflow: which company, which graph.
type WorkflowKey = (CompanyId, String);

/// How a scheduled run's report deliveries came out, folded for one log line.
///
/// A count per status rather than a bare total: `skipped` and `denied` mean
/// policy refused to send (a missing `email` grant, no mailbox) while `failed`
/// means something broke, and an operator reading a scheduled run's outcome
/// needs to tell those apart before deciding whether to act.
///
/// `pending` is the fourth thing entirely, and the reason this line matters
/// most on the scheduled path: the report is not lost and nothing is broken —
/// it is sitting in the approvals queue waiting for a human. A scheduled run
/// is exactly the nobody-is-watching case, so without this count an operator
/// would have no signal that a card is waiting for them.
#[derive(Debug, Default, PartialEq, Eq)]
struct DeliveryCounts {
    sent: usize,
    pending: usize,
    skipped: usize,
    denied: usize,
    failed: usize,
}

impl DeliveryCounts {
    fn of(reports: &[DeliveryReport]) -> Self {
        let mut counts = Self::default();
        for report in reports {
            match report.status {
                DeliveryStatus::Sent => counts.sent += 1,
                DeliveryStatus::Pending => counts.pending += 1,
                DeliveryStatus::Skipped => counts.skipped += 1,
                DeliveryStatus::Denied => counts.denied += 1,
                DeliveryStatus::Failed => counts.failed += 1,
            }
        }
        counts
    }

    /// Reports that did NOT reach their destination **and never will without a
    /// change** — the number worth alerting on.
    ///
    /// `pending` is deliberately excluded: it is awaiting a verdict, not a fix,
    /// and folding it in here would page someone for a queue that is working as
    /// designed. It gets its own count on the summary line.
    fn undelivered(&self) -> usize {
        self.skipped + self.denied + self.failed
    }
}

/// Drives the cron schedules authored on saved workflow graphs.
pub struct WorkflowScheduler {
    registry: CompanyRegistry,
    clock: Arc<dyn Clock>,
    /// Per-workflow last-fired epoch minute, so a workflow fires at most once
    /// per minute no matter how often [`tick`](Self::tick) is called.
    last_fired: HashMap<WorkflowKey, u64>,
    /// Scheduled runs currently executing. Shared with the spawned run tasks,
    /// which remove their key on completion, so a run that outlives its minute
    /// suppresses its own next fire instead of stacking up.
    in_flight: Arc<Mutex<HashSet<WorkflowKey>>>,
    /// Companies already warned about having scheduled workflows but no
    /// [`WorkflowRunner`](crate::ports::WorkflowRunner) to run them on. The tick
    /// is once a minute forever, so the warning is latched here and re-armed
    /// only when the situation changes — see [`note_unwired`](Self::note_unwired).
    warned_unwired: HashSet<CompanyId>,
    /// How many unwired-company warnings have been emitted, so a test can assert
    /// the latch actually suppresses the repeat.
    #[cfg(test)]
    unwired_warnings: usize,
}

impl WorkflowScheduler {
    /// Builds a scheduler over every company in `registry`, driven by `clock`.
    pub fn new(registry: CompanyRegistry, clock: Arc<dyn Clock>) -> Self {
        Self {
            registry,
            clock,
            last_fired: HashMap::new(),
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            warned_unwired: HashSet::new(),
            #[cfg(test)]
            unwired_warnings: 0,
        }
    }

    /// Runs one tick: fires every saved workflow whose trigger schedule matches
    /// the current UTC minute. Returns how many runs were started.
    ///
    /// Per company, in order, the tick skips:
    ///
    /// * a company whose `ensure_running` guard rejects (paused or archived) —
    ///   the same guard [`CompanyScheduler::tick`](super::scheduler::CompanyScheduler::tick)
    ///   uses, so schedules resume cleanly on unpause;
    /// * a company with no [`WorkflowRunner`](crate::ports::WorkflowRunner)
    ///   wired — the default build has none, so it stays inert through the same
    ///   port seam the run route reports `not_wired` on. If that company *does*
    ///   have scheduled workflows the skip is announced once (see
    ///   [`note_unwired`](Self::note_unwired)), because a configured build whose
    ///   inference source failed to resolve lands here too and would otherwise
    ///   look identical to a working one;
    /// * a workflow whose graph is malformed (skipped by the union loader with a
    ///   warning, so one bad graph never silences the rest);
    /// * a workflow already fired this minute, or whose previous scheduled run
    ///   is still in flight.
    ///
    /// **Why the manifest's `[workflows].enabled` list is not a filter here.**
    /// A trigger `schedule` is itself the operator's explicit "run this on a
    /// cron" statement — a graph that declares one has already said it wants to
    /// fire, so re-asking the enabled list adds a second switch for the same
    /// decision and a way to half-configure it. Enumeration therefore runs over
    /// the union of seed files and overlay graph bodies, both of which survive a
    /// rebuild.
    ///
    /// (An earlier version of this doc rested the argument partly on the enabled
    /// list not surviving a restart. That was true — a boot rebuild re-seeded the
    /// record's manifest from `company.toml` — but issue #208 fixed it: the
    /// rebuild now merges runtime-enabled ids forward. Gating the scheduler on
    /// `enabled` is a live design option again; it is deliberately not taken
    /// here, on the argument above.)
    pub async fn tick(&mut self) -> usize {
        let now = self.clock.now_millis();
        let minute = now / MINUTE_MS;
        let civil = CivilTime::from_unix_millis(now);

        let mut fired = 0;
        for company in self.registry.list() {
            let Some(runtime) = self.registry.get(&company) else {
                continue; // removed between listing and lookup (archive)
            };
            // Not accepting work: paused or archived.
            if runtime.ensure_running().await.is_err() {
                continue;
            }
            // The record's runtime-authored graph bodies. A company with no
            // persisted record contributes none; a store failure is logged and
            // skipped rather than aborting every other company's schedules.
            let overlays = match runtime.store().load(&company).await {
                Ok(Some(record)) => record.overlay_workflows,
                Ok(None) => Vec::new(),
                Err(err) => {
                    tracing::warn!(%company, %err, "workflow scheduler: cannot read company record");
                    continue;
                }
            };

            // Enumerate the company's scheduled workflows BEFORE checking for a
            // runner: whether any exist is exactly what decides if an unwired
            // company is misconfigured or simply has nothing to run.
            let mut scheduled: Vec<(WorkflowFile, String, CronExpr)> = Vec::new();
            for file in list_workflows_union(runtime.source_dir(), &overlays) {
                let Some(cron) = trigger_schedule(&file) else {
                    continue; // no schedule: manual-run only
                };
                // Validation already accepted this expression; a parse failure
                // here would mean a body written by an older/looser path, so
                // skip that one workflow loudly rather than panicking.
                let Ok(expr) = CronExpr::parse(&cron) else {
                    tracing::warn!(
                        %company,
                        workflow = %file.id,
                        schedule = %cron,
                        "workflow scheduler: skipping an unparsable schedule"
                    );
                    continue;
                };
                scheduled.push((file, cron, expr));
            }

            // No execution wired. This is the default build's inert seam, but it
            // is ALSO what a configured build looks like when its inference
            // source failed to resolve at boot — an operator-visible
            // misconfiguration in which a saved schedule is indistinguishable
            // from a broken one. Say so, once.
            let runner = match runtime.workflow_runner().cloned() {
                Some(runner) => {
                    // A runner appeared: re-arm the warning for this company.
                    self.warned_unwired.remove(&company);
                    runner
                }
                None => {
                    self.note_unwired(&company, scheduled.len());
                    continue;
                }
            };

            // Issue #440: the shared way to start a supervised run. Built once
            // per company (it holds cloned handles, so a per-fire clone is
            // cheap) and cloned into each fire's task below.
            //
            // This scheduler used to mint its own run id through the supervisor
            // and journal its own `WorkflowRunFinished` on both arms — a second
            // copy of the two rules `WorkflowSpawn` exists to own. The copies
            // agreed, which is exactly what made the duplication dangerous: a
            // fix to one would not have reached the other, and nothing would
            // have failed to say so.
            let spawn = crate::runtime::WorkflowSpawn::new(&runtime, runner);

            for (file, cron, expr) in scheduled {
                if !expr.matches(&civil) {
                    continue;
                }

                let key = (company.clone(), file.id.clone());
                if self.last_fired.get(&key) == Some(&minute) {
                    continue; // already fired this minute
                }
                self.last_fired.insert(key.clone(), minute);

                // Overlap guard: a previous scheduled run still executing keeps
                // this fire from stacking a second copy on top of it. Manual
                // runs go through the run route and are unaffected.
                let Some(claim) = self.claim(&key) else {
                    tracing::info!(
                        %company,
                        workflow = %file.id,
                        schedule = %cron,
                        "workflow scheduler: previous scheduled run still in flight, skipping"
                    );
                    continue;
                };

                let input = json!({
                    // `request` is what `run_request_text` reads, so every agent
                    // turn in the run knows it was started by a schedule rather
                    // than by an operator typing a topic.
                    "request": format!("Scheduled run (cron `{cron}`)"),
                    "scheduled": true,
                    "cron": cron,
                    "firedAtMs": now,
                });

                let workflow = file;
                // Cloned BEFORE the spawn: the task outlives this loop
                // iteration (and this borrow of `runtime`), so everything it
                // needs has to be moved in rather than reached for from inside.
                // The clone carries the company id, the event log (issue #228 —
                // what turns a scheduled run's outcome from a host-stdout line
                // into something the tenant's own console reads back), the run
                // supervisor (issue #383 — what puts a Cancel button on a cron
                // fire nobody chose the timing of) and the runner.
                let spawn = spawn.clone();
                // A FRESH TASK PER FIRE IS CORRECT HERE, and is not a hole in
                // the `WORKFLOW_DEPTH` re-entry guard
                // (`crate::workflows::runner`). That guard is a task-local
                // counting one *causal chain*: a run, the agent turns inside
                // it, and the tools those turns call all stay on one task, so a
                // workflow that reaches back into itself is bounded. A
                // scheduled fire starts no such chain — it is a new root, at
                // depth 0, exactly like an operator clicking Run — so spawning
                // it here is the same as any other entry point. What would
                // break the guard is spawning *inside* an existing run's chain,
                // which would reset the depth mid-chain; this scheduler never
                // runs inside a run. Spawning also keeps one slow agent run
                // from starving every other company's schedule on the tick
                // loop.
                tokio::spawn(async move {
                    // Held for the whole run so the slot is released on EVERY
                    // exit path, including an unwind. Releasing after the
                    // `await` instead would leak the claim when the runner
                    // panics — and a leaked claim is permanent, because nothing
                    // else ever removes it, so one panic would retire that
                    // schedule for the life of the process with no log line.
                    let _claim = claim;
                    let (company, workflow_id) = key;
                    // Issue #440: through the shared primitive, which owns both
                    // halves this used to copy — the supervisor-minted run id
                    // (issue #383: it is the address the console's Cancel button
                    // sends to, and #371: the id the run's progress events and
                    // its outcome share, including on the error arm where the
                    // runner hands back nothing that could carry one) and the
                    // `record_run_finished` on BOTH arms (issue #228), with the
                    // run guard held across the journal write.
                    //
                    // The handle is **awaited**, not dropped. The claim above is
                    // this scheduler's own overlap guard and has to outlive the
                    // run, and the delivery-log sweep below needs the outcome.
                    // Both remain the scheduler's job; what is shared is only
                    // how a run is started and recorded.
                    let (_run_id, handle) = spawn.spawn(workflow, input, true);
                    match handle.await {
                        Ok(Ok(run)) => {
                            // A manual run hands `deliveries` back in the HTTP
                            // response and the console renders it. A scheduled
                            // run has no response and nobody watching, so
                            // without this the exact case the operator most
                            // needs to know about — the owner summary that did
                            // NOT go out — would be the quietest thing the
                            // system does.
                            let counts = DeliveryCounts::of(&run.deliveries);
                            // One line per undelivered report: a count alone
                            // says something is wrong without saying what to
                            // fix, and `reason` is the part that names the
                            // failure class.
                            for report in &run.deliveries {
                                if report.status == DeliveryStatus::Sent {
                                    continue;
                                }
                                // A parked report is not a failure and must not
                                // be logged as one — it is a card waiting in
                                // the approvals queue. Say where to go, at
                                // info, and move on.
                                if report.status == DeliveryStatus::Pending {
                                    tracing::info!(
                                        %company,
                                        workflow = %workflow_id,
                                        node = %report.node,
                                        kind = %report.kind,
                                        // Same reason as the warn below: never
                                        // the recipient's address in a host log.
                                        target_configured = report.target.is_some(),
                                        "workflow scheduler: a scheduled run's report is parked for operator approval — see the Approvals view"
                                    );
                                    continue;
                                }
                                tracing::warn!(
                                    %company,
                                    workflow = %workflow_id,
                                    node = %report.node,
                                    kind = %report.kind,
                                    // NOT the target itself: for an `email`
                                    // destination that is the recipient's
                                    // address, and this line goes to host
                                    // stdout — which on a hosted tenant is us,
                                    // not the operator. Whether one resolved is
                                    // the part with diagnostic value anyway.
                                    target_configured = report.target.is_some(),
                                    status = ?report.status,
                                    // The classification, NOT `report.detail`.
                                    // `detail` interpolates the transport's own
                                    // words on the failure arms, and a mail
                                    // transport quotes the mailbox it refused —
                                    // so logging it walks the recipient address
                                    // onto host stdout through the back door
                                    // that scrubbing `target` left open (issue
                                    // #248). `reason` says the same thing about
                                    // what failed, out of a closed set that
                                    // cannot carry transport text; the operator
                                    // still gets the full `detail` on the run
                                    // response and in the run history their own
                                    // console reads back.
                                    reason = %report.reason,
                                    "workflow scheduler: a scheduled run's report was NOT delivered"
                                );
                            }
                            tracing::info!(
                                %company,
                                workflow = %workflow_id,
                                pending_approvals = run.pending_approvals.len(),
                                sent = counts.sent,
                                // Awaiting a human, not a fix — counted apart
                                // from `undelivered` for exactly that reason.
                                pending_approval = counts.pending,
                                skipped = counts.skipped,
                                denied = counts.denied,
                                failed = counts.failed,
                                // The one number worth alerting on, so a log
                                // query need not sum the three refusal kinds.
                                undelivered = counts.undelivered(),
                                "workflow scheduler: scheduled run finished"
                            );
                            // The operator-facing half — the journaled
                            // `WorkflowRunFinished` the tenant's own console
                            // reads back — was written by `spawn` before this
                            // handle resolved (issue #228). The log lines above
                            // stay exactly as they are: they are the platform
                            // team's diagnostic on host stdout, which on a
                            // hosted tenant is emphatically not the operator.
                        }
                        Ok(Err(err)) => {
                            tracing::warn!(
                                %company,
                                workflow = %workflow_id,
                                %err,
                                "workflow scheduler: scheduled run failed"
                            );
                        }
                        // The run task itself came apart — a panic inside the
                        // runner, which unwinds in the spawned task rather than
                        // here. Nothing was journaled for it (the outcome write
                        // lives in that task), so this line is the only trace
                        // there is, and it must not be silent. The claim still
                        // releases: it is held by this task's guard.
                        Err(err) => {
                            tracing::error!(
                                %company,
                                workflow = %workflow_id,
                                %err,
                                "workflow scheduler: a scheduled run's task did not complete; its \
                                 outcome was never recorded"
                            );
                        }
                    }
                });
                fired += 1;
            }
        }

        // --- sweep stale per-company/per-workflow state -------------------
        // Both maps are keyed on things that can disappear (a workflow is
        // deleted, a company is archived out of the registry), so both need a
        // sweep or they grow for the life of the process. One place, so there
        // is a single obvious point where this happens.

        // Only the CURRENT minute can dedupe a fire, so every older entry is
        // dead weight.
        self.last_fired.retain(|_, fired_at| *fired_at >= minute);

        // A company removed from the registry is never visited again, so
        // NEITHER re-arm path in `note_unwired` (a runner appearing, or the
        // scheduled count dropping to zero) can ever clear its latch — the
        // entry would be orphaned forever.
        let registry = &self.registry;
        self.warned_unwired
            .retain(|company| registry.get(company).is_some());

        fired
    }

    /// Records that `company` has `scheduled` workflows but no runner to fire
    /// them on, warning at most once per company.
    ///
    /// Two rules, both deliberate:
    ///
    /// * **Silence when `scheduled == 0`.** A company with no scheduled
    ///   workflows and no runner is not misconfigured — it simply has nothing to
    ///   run. The latch is cleared in that case, so if a schedule is saved later
    ///   while the company is still unwired, the operator does get told.
    /// * **Once per company, not once per tick.** The tick is every minute
    ///   forever; logging there would be ~1440 lines a day per tenant, which
    ///   buries the signal it is trying to raise. The latch is re-armed in
    ///   [`tick`](Self::tick) as soon as a runner appears, and swept there when
    ///   the company leaves the registry — neither re-arm path can reach a
    ///   company that is no longer visited.
    fn note_unwired(&mut self, company: &CompanyId, scheduled: usize) {
        if scheduled == 0 {
            self.warned_unwired.remove(company);
            return;
        }
        if !self.warned_unwired.insert(company.clone()) {
            return; // already warned for this company
        }
        #[cfg(test)]
        {
            self.unwired_warnings += 1;
        }
        tracing::warn!(
            %company,
            scheduled_workflows = scheduled,
            "workflow scheduler: {scheduled} scheduled workflow(s) will not fire — no workflow \
             runner is wired for this company (inference source unresolved?)"
        );
    }

    /// Claims `key` for a run, or `None` when a run already holds it.
    ///
    /// The returned [`Claim`] IS the hold: dropping it releases the slot, so a
    /// caller that takes a claim and then bails out cannot strand it.
    fn claim(&self, key: &WorkflowKey) -> Option<Claim> {
        if lock_in_flight(&self.in_flight).insert(key.clone()) {
            Some(Claim {
                in_flight: self.in_flight.clone(),
                key: key.clone(),
            })
        } else {
            None
        }
    }

    /// Whether any scheduled run is currently executing.
    #[cfg(test)]
    fn is_running_any(&self) -> bool {
        !lock_in_flight(&self.in_flight).is_empty()
    }

    /// Spawns a background task that ticks on every minute boundary until
    /// `shutdown` is notified. Boot holds the join handle and the shared
    /// `shutdown` so the scheduler stops cleanly when the server does.
    pub fn spawn(mut self, shutdown: Arc<Notify>) -> JoinHandle<()> {
        tokio::spawn(async move {
            // The `Notified` future is built ONCE and pinned across iterations,
            // not rebuilt inside the `select!`. Boot signals with
            // `notify_waiters()`, which wakes only the waiters registered at
            // that instant — a future created fresh each iteration is not
            // registered while `tick` is running, so a shutdown arriving
            // mid-tick would be dropped and the scheduler would sleep another
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

/// An RAII hold on one workflow's in-flight slot, released on drop.
///
/// A hold released by a statement after the `await` would survive a panic in
/// the run: the task unwinds, the statement never executes, and the key stays
/// in the set forever — permanently retiring that schedule. `Drop` runs while
/// unwinding, so tying the release to a guard closes that path.
struct Claim {
    in_flight: Arc<Mutex<HashSet<WorkflowKey>>>,
    key: WorkflowKey,
}

impl Drop for Claim {
    fn drop(&mut self) {
        lock_in_flight(&self.in_flight).remove(&self.key);
    }
}

/// Locks the in-flight set, recovering rather than panicking on a poisoned
/// mutex.
///
/// Two reasons this never unwraps. First, [`Claim::drop`] can run while
/// unwinding from the very panic that poisoned the lock, and a panic inside a
/// `Drop` during an unwind aborts the process. Second, the alternative — a
/// tolerant `if let Ok(..)` — would *skip* the release on a poisoned lock and
/// reintroduce the leak this guard exists to prevent. Recovering is safe here
/// because every critical section is a single `insert` / `remove` / `is_empty`
/// on a `HashSet`, none of which can leave it half-updated.
fn lock_in_flight(
    in_flight: &Mutex<HashSet<WorkflowKey>>,
) -> std::sync::MutexGuard<'_, HashSet<WorkflowKey>> {
    in_flight
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The cron a graph's trigger schedules itself on, if any.
///
/// Validation allows `schedule` only on a `trigger` node and permits at most one
/// scheduled trigger per graph, so the node this finds is the only one there is
/// — a graph with two schedules is rejected at parse rather than silently
/// resolving to whichever came first.
fn trigger_schedule(file: &WorkflowFile) -> Option<String> {
    file.nodes
        .iter()
        .find(|node| {
            node.kind == crate::company::WorkflowNodeKind::Trigger && node.schedule.is_some()
        })
        .and_then(|node| node.schedule.clone())
}

#[cfg(test)]
mod test {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use serde_json::Value;
    use tokio::sync::Semaphore;

    use super::*;
    use crate::company::CompanyManifest;
    use crate::ports::types::{CompanyEvent, CompanyRecord, EventSeq, OverlayWorkflow};
    use crate::ports::{DeliveryReason, WorkflowRun, WorkflowRunner};
    use crate::runtime::{FakeClock, RuntimeBuilder};

    // --- tracing capture -----------------------------------------------------
    //
    // The scheduler's only channel for a failed scheduled delivery IS a log
    // line (see the module docs), so proving it is "observable rather than
    // silent" means reading what it actually emitted. The run happens on a
    // spawned task, and a thread-local subscriber does not reach one, so the
    // capture is installed process-wide exactly once. Every test in this binary
    // therefore shares one buffer — assertions must key on something unique to
    // their own company id, never on "the buffer contains one line".

    /// The shared capture buffer. `None` until `captured_logs()` installs it.
    static CAPTURE: std::sync::OnceLock<Arc<Mutex<Vec<u8>>>> = std::sync::OnceLock::new();

    /// A writer that accumulates one event and appends it to the shared buffer
    /// in a single locked write on drop, so events from concurrent tests
    /// interleave whole-line rather than mid-line.
    struct CaptureWriter {
        buf: Vec<u8>,
        sink: Arc<Mutex<Vec<u8>>>,
    }

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.buf.extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Drop for CaptureWriter {
        fn drop(&mut self) {
            if !self.buf.is_empty() {
                self.sink.lock().unwrap().extend_from_slice(&self.buf);
            }
        }
    }

    #[derive(Clone)]
    struct MakeCapture(Arc<Mutex<Vec<u8>>>);

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for MakeCapture {
        type Writer = CaptureWriter;
        fn make_writer(&'a self) -> Self::Writer {
            CaptureWriter {
                buf: Vec::new(),
                sink: self.0.clone(),
            }
        }
    }

    /// Installs the process-wide capture on first call and returns the buffer.
    fn captured_logs() -> Arc<Mutex<Vec<u8>>> {
        CAPTURE
            .get_or_init(|| {
                let sink = Arc::new(Mutex::new(Vec::new()));
                let subscriber = tracing_subscriber::fmt()
                    .with_writer(MakeCapture(sink.clone()))
                    .with_max_level(tracing::Level::INFO)
                    .with_ansi(false)
                    .finish();
                // Only this module installs one, so it cannot lose the race to
                // another test; if some future test adds a subscriber, this
                // returns Err and the capture tests would fail loudly rather
                // than silently asserting on an empty buffer.
                tracing::subscriber::set_global_default(subscriber)
                    .expect("no other global tracing subscriber in the test binary");
                sink
            })
            .clone()
    }

    /// The captured log text so far.
    fn captured_text(sink: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8_lossy(&sink.lock().unwrap()).to_string()
    }

    fn tmp_home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("opencompany-wfsched-")
            .tempdir()
            .expect("tempdir")
    }

    fn manifest() -> CompanyManifest {
        toml::from_str(
            r#"
            [company]
            name = "Acme"

            [[agent]]
            id = "ceo"
            role = "Chief"

            [policy]
            mode = "full"
            "#,
        )
        .expect("parse manifest")
    }

    /// Unix millis for a UTC civil minute, via the cron module's own conversion.
    fn millis_at(year: i64, month: u32, day: u32, hour: u32, minute: u32) -> u64 {
        let mut probe = 0u64;
        loop {
            let c = CivilTime::from_unix_millis(probe);
            if (c.year, c.month, c.day) == (year, month, day) {
                break;
            }
            probe += 86_400_000;
            if probe > 4_102_444_800_000 {
                panic!("date out of probe range");
            }
        }
        probe + (hour as u64) * 3_600_000 + (minute as u64) * MINUTE_MS
    }

    /// What one recorded scheduled run carries.
    #[derive(Clone, Debug)]
    struct Recorded {
        company: String,
        workflow: String,
        input: Value,
    }

    /// A [`WorkflowRunner`] that records every run instead of executing one, and
    /// can be held open so a test controls exactly when a run completes.
    struct RecordingRunner {
        started: Arc<Mutex<Vec<Recorded>>>,
        completed: Arc<AtomicUsize>,
        /// When set, a run parks until the test adds a permit.
        gate: Option<Arc<Semaphore>>,
        /// The delivery rows every run reports back, so a test can drive the
        /// scheduler's undelivered-report path.
        deliveries: Vec<DeliveryReport>,
    }

    impl RecordingRunner {
        fn new() -> (Arc<Self>, Arc<Mutex<Vec<Recorded>>>, Arc<AtomicUsize>) {
            let started = Arc::new(Mutex::new(Vec::new()));
            let completed = Arc::new(AtomicUsize::new(0));
            let runner = Arc::new(Self {
                started: started.clone(),
                completed: completed.clone(),
                gate: None,
                deliveries: Vec::new(),
            });
            (runner, started, completed)
        }

        fn gated(gate: Arc<Semaphore>) -> (Arc<Self>, Arc<Mutex<Vec<Recorded>>>, Arc<AtomicUsize>) {
            let started = Arc::new(Mutex::new(Vec::new()));
            let completed = Arc::new(AtomicUsize::new(0));
            let runner = Arc::new(Self {
                started: started.clone(),
                completed: completed.clone(),
                gate: Some(gate),
                deliveries: Vec::new(),
            });
            (runner, started, completed)
        }

        /// A runner whose every run reports `deliveries` back.
        fn with_deliveries(deliveries: Vec<DeliveryReport>) -> (Arc<Self>, Arc<AtomicUsize>) {
            let completed = Arc::new(AtomicUsize::new(0));
            let runner = Arc::new(Self {
                started: Arc::new(Mutex::new(Vec::new())),
                completed: completed.clone(),
                gate: None,
                deliveries,
            });
            (runner, completed)
        }
    }

    #[async_trait]
    impl WorkflowRunner for RecordingRunner {
        async fn run(
            &self,
            company: &CompanyId,
            workflow: &WorkflowFile,
            input: Value,
            ctx: &crate::ports::WorkflowRunContext,
        ) -> crate::Result<WorkflowRun> {
            self.started.lock().unwrap().push(Recorded {
                company: company.as_ref().to_string(),
                workflow: workflow.id.clone(),
                input,
            });
            if let Some(gate) = &self.gate {
                // Issue #383: a parked run races its gate against the stop
                // signal, mirroring what the real runner does with the engine
                // future. Without this the double would ignore a cancel and the
                // scheduler test below could not observe one.
                tokio::select! {
                    permit = gate.acquire() => permit.expect("gate open").forget(),
                    () = ctx.cancel.cancelled() => {
                        return Ok(WorkflowRun {
                            output: Value::Null,
                            pending_approvals: Vec::new(),
                            deliveries: Vec::new(),
                            cancelled: true,
                        });
                    }
                }
            }
            self.completed.fetch_add(1, Ordering::SeqCst);
            Ok(WorkflowRun {
                output: Value::Null,
                pending_approvals: Vec::new(),
                deliveries: self.deliveries.clone(),
                cancelled: false,
            })
        }
    }

    /// A [`WorkflowRunner`] whose every run fails, so a test can drive the
    /// scheduler's `Err` arm — the outcome that used to leave nothing durable
    /// behind at all (issue #228).
    struct FailingRunner {
        message: String,
        attempts: Arc<AtomicUsize>,
    }

    impl FailingRunner {
        fn new(message: &str) -> (Arc<Self>, Arc<AtomicUsize>) {
            let attempts = Arc::new(AtomicUsize::new(0));
            let runner = Arc::new(Self {
                message: message.to_string(),
                attempts: attempts.clone(),
            });
            (runner, attempts)
        }
    }

    #[async_trait]
    impl WorkflowRunner for FailingRunner {
        async fn run(
            &self,
            _company: &CompanyId,
            _workflow: &WorkflowFile,
            _input: Value,
            _ctx: &crate::ports::WorkflowRunContext,
        ) -> crate::Result<WorkflowRun> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            Err(crate::error::OpenCompanyError::Config(self.message.clone()))
        }
    }

    /// A minimal valid graph body, optionally scheduled on `cron`.
    fn body(id: &str, cron: Option<&str>) -> String {
        let schedule = cron
            .map(|c| format!("schedule = \"{c}\"\n"))
            .unwrap_or_default();
        format!(
            r#"
id = "{id}"
name = "{id}"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
{schedule}
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "done"
"#
        )
    }

    fn overlay(id: &str, cron: Option<&str>) -> OverlayWorkflow {
        OverlayWorkflow {
            id: id.to_string(),
            toml: body(id, cron),
        }
    }

    /// Builds a registered company whose only workflows are record overlays —
    /// the console-created shape, with no source directory at all.
    async fn company_with_overlays(
        home: &std::path::Path,
        id: &str,
        overlays: Vec<OverlayWorkflow>,
        runner: Option<Arc<dyn WorkflowRunner>>,
        lifecycle: &str,
    ) -> CompanyRegistry {
        let company = CompanyId::new(id);
        let mut runtime = RuntimeBuilder::new(home.to_path_buf(), manifest())
            .with_id(company.clone())
            .build()
            .await
            .expect("builds");
        assert!(
            runtime.source_dir().is_none(),
            "the overlay-only case must have no source dir"
        );
        if let Some(runner) = runner {
            runtime.set_workflow_runner(runner);
        }

        // Persist the graph bodies (and lifecycle) the scheduler will read.
        let store = runtime.store().clone();
        let mut record: CompanyRecord = store
            .load(&company)
            .await
            .expect("loads")
            .expect("the builder materialized a record");
        record.overlay_workflows = overlays;
        record.lifecycle = lifecycle.to_string();
        store.save(&record).await.expect("saves");

        let registry = CompanyRegistry::new();
        registry.insert(company, Arc::new(runtime));
        registry
    }

    /// Yields until `predicate` holds, so a test can wait on a spawned run
    /// without a wall-clock sleep. Panics rather than hanging if it never does.
    async fn wait_for(mut predicate: impl FnMut() -> bool) {
        for _ in 0..10_000 {
            if predicate() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("condition never became true");
    }

    /// Waits until no scheduled run still holds its in-flight claim.
    ///
    /// **Any test that ticks a second time and expects another fire must call
    /// this first.** `tick` is not a synchronisation point: it spawns the run
    /// and returns without awaiting it, so whether the spawned task has been
    /// polled by the next tick depends on whether some await inside `tick`
    /// (the record load) happens to yield. When it does not, the previous run
    /// still holds the claim, the overlap guard correctly rejects the fire, and
    /// the tick returns 0 — which is the code working as designed and the test
    /// being wrong. That is exactly how
    /// `the_dedupe_map_does_not_grow_without_bound` passed locally and failed
    /// in CI.
    ///
    /// Waiting on the mock's `started` vec is NOT a substitute: it records the
    /// run's start, not its completion, so it proves nothing about the claim.
    /// This keys on the claim itself, which is the state the guard reads.
    async fn drain(scheduler: &WorkflowScheduler) {
        wait_for(|| !scheduler.is_running_any()).await;
    }

    /// The headline: a workflow that exists ONLY as a record overlay (no source
    /// file — the console-created shape #168 introduced) is picked up and fired
    /// on a matching minute, once. It does not re-fire in the same minute, stays
    /// silent on a non-matching minute, and fires again on the next match.
    #[tokio::test]
    async fn fires_an_overlay_only_workflow_once_per_matching_minute() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let (runner, started, _completed) = RecordingRunner::new();
        let registry = company_with_overlays(
            &home,
            "acme",
            vec![overlay("digest", Some("0 9 * * MON"))],
            Some(runner),
            "running",
        )
        .await;

        // Monday 2026-07-13 09:00 UTC — the schedule matches.
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = WorkflowScheduler::new(registry, clock.clone());

        assert_eq!(scheduler.tick().await, 1);
        wait_for(|| started.lock().unwrap().len() == 1).await;
        let run = started.lock().unwrap()[0].clone();
        assert_eq!(run.company, "acme");
        assert_eq!(run.workflow, "digest");

        // Same minute again: deduped.
        clock.advance(30_000);
        assert_eq!(scheduler.tick().await, 0);

        // A non-matching minute (09:01) is silent.
        clock.set(millis_at(2026, 7, 13, 9, 1));
        assert_eq!(scheduler.tick().await, 0);

        // The following Monday fires again — after the first run has let go of
        // its claim, or the overlap guard would rightly refuse.
        drain(&scheduler).await;
        clock.set(millis_at(2026, 7, 20, 9, 0));
        assert_eq!(scheduler.tick().await, 1);
        wait_for(|| started.lock().unwrap().len() == 2).await;
    }

    /// The seeded input tells the run — and every agent turn inside it — that a
    /// schedule started it. `request` is the key `run_request_text` reads.
    #[tokio::test]
    async fn the_seeded_input_marks_the_run_as_scheduled() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let (runner, started, _completed) = RecordingRunner::new();
        let registry = company_with_overlays(
            &home,
            "acme",
            vec![overlay("digest", Some("* * * * *"))],
            Some(runner),
            "running",
        )
        .await;
        let fired_at = millis_at(2026, 7, 13, 9, 0);
        let clock = Arc::new(FakeClock::new(fired_at));
        let mut scheduler = WorkflowScheduler::new(registry, clock);

        assert_eq!(scheduler.tick().await, 1);
        wait_for(|| started.lock().unwrap().len() == 1).await;

        let input = started.lock().unwrap()[0].input.clone();
        assert_eq!(input["scheduled"], true);
        assert_eq!(input["cron"], "* * * * *");
        assert_eq!(input["firedAtMs"], fired_at);
        // `request` is exactly the key `workflows::caps::run_request_text`
        // reads, so this string reaches every agent turn in the run.
        let request = input["request"].as_str().expect("a request string");
        assert!(request.contains("Scheduled run"), "{request}");
        assert!(request.contains("* * * * *"), "{request}");
        assert!(!request.trim().is_empty());
    }

    // --- report delivery on a scheduled run (issue #170) ---------------------

    /// A row whose classification is the plausible one for its status, so a
    /// fixture never claims something the two halves would contradict (a `sent`
    /// row reasoned as a cold recipient, say). Tests that assert on the
    /// classification itself use [`reported`] and name it.
    fn report(node: &str, status: DeliveryStatus, detail: &str) -> DeliveryReport {
        let reason = match status {
            DeliveryStatus::Sent => DeliveryReason::OwnerEmailed,
            DeliveryStatus::Pending => DeliveryReason::ParkedForApproval,
            DeliveryStatus::Skipped => DeliveryReason::RecipientNotEstablished,
            DeliveryStatus::Denied => DeliveryReason::EmailNotGranted,
            DeliveryStatus::Failed => DeliveryReason::MailTransportRefused,
        };
        reported(node, status, reason, detail)
    }

    /// `report`, with the classification spelled out — for the tests that care
    /// which half of the row the scheduler logged.
    fn reported(
        node: &str,
        status: DeliveryStatus,
        reason: DeliveryReason,
        detail: &str,
    ) -> DeliveryReport {
        DeliveryReport {
            node: node.to_string(),
            kind: "email".to_string(),
            target: Some(RECIPIENT.to_string()),
            status,
            detail: detail.to_string(),
            reason,
        }
    }

    /// The recipient address every delivery fixture in this module addresses.
    /// `.invalid` is reserved by RFC 2606 and can never resolve, so a fixture
    /// that escapes into a log or a PR body names nobody.
    const RECIPIENT: &str = "recipient@example.invalid";

    /// The fold behind the summary line counts each status separately, because
    /// "policy refused to send" and "something broke" are different problems.
    #[test]
    fn delivery_counts_separate_the_five_outcomes() {
        let counts = DeliveryCounts::of(&[
            report("a", DeliveryStatus::Sent, ""),
            report("b", DeliveryStatus::Sent, ""),
            report("c", DeliveryStatus::Skipped, ""),
            report("d", DeliveryStatus::Denied, ""),
            report("e", DeliveryStatus::Failed, ""),
            report("f", DeliveryStatus::Pending, ""),
        ]);
        assert_eq!(
            counts,
            DeliveryCounts {
                sent: 2,
                pending: 1,
                skipped: 1,
                denied: 1,
                failed: 1,
            }
        );
        // A parked report awaits a verdict, not a fix: it must NOT inflate the
        // number an operator alerts on, or a working approvals queue would page
        // someone every scheduled minute.
        assert_eq!(counts.undelivered(), 3);
        // The common case: nothing routed anywhere.
        assert_eq!(DeliveryCounts::of(&[]), DeliveryCounts::default());
        assert_eq!(DeliveryCounts::of(&[]).undelivered(), 0);
    }

    /// **The finding CodeRabbit raised.** A scheduled run whose report did not
    /// go out must not be silent. There is no HTTP response and no drawer, so
    /// the log line is the whole channel — this reads what the scheduler
    /// actually emitted and asserts the operator-actionable parts are in it:
    /// which company, which workflow, which node, and the `reason` that says
    /// what to do about it.
    #[tokio::test]
    async fn a_scheduled_run_reports_an_undelivered_report() {
        let sink = captured_logs();
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        // A company id unique to this test: the capture buffer is shared with
        // every other test in the binary, so this is what makes the assertions
        // about *this* run rather than about whatever else logged.
        let company = "undelivered-co";
        let (runner, completed) = RecordingRunner::with_deliveries(vec![
            report(
                "owner_summary",
                DeliveryStatus::Skipped,
                "this recipient has never written to the company",
            ),
            report("also_sent", DeliveryStatus::Sent, "emailed the recipient"),
        ]);
        let registry = company_with_overlays(
            &home,
            company,
            vec![overlay("digest", Some("* * * * *"))],
            Some(runner),
            "running",
        )
        .await;
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = WorkflowScheduler::new(registry, clock);

        assert_eq!(scheduler.tick().await, 1);
        wait_for(|| completed.load(Ordering::SeqCst) == 1).await;
        // The run task logs after `completed` is bumped, so wait for the line
        // itself rather than racing it.
        //
        // Qualified by BOTH this company and the marker, and scanned per line:
        // `CAPTURE` is shared across every test in the binary, and
        // `owner_summary` is the node name three tests in this module use — an
        // unqualified `contains` could be satisfied by a sibling's line before
        // this run has logged anything, and the lookup below would then fail
        // intermittently.
        wait_for(|| {
            captured_text(&sink)
                .lines()
                .any(|l| l.contains(company) && l.contains("was NOT delivered"))
        })
        .await;

        let logs = captured_text(&sink);
        let line = logs
            .lines()
            .find(|l| l.contains(company) && l.contains("was NOT delivered"))
            .unwrap_or_else(|| panic!("no undelivered-report line for {company}: {logs}"));
        assert!(line.contains("digest"), "names the workflow: {line}");
        assert!(line.contains("owner_summary"), "names the node: {line}");
        assert!(line.contains("Skipped"), "names the status: {line}");
        assert!(
            line.contains("never written to the company"),
            "carries the reason, which is the part that says what to fix: {line}"
        );
        // …but NOT the recipient's address. This line lands on host stdout,
        // which on a hosted tenant is us rather than the operator, so the
        // address must never ride it — only whether one resolved at all.
        assert!(
            !line.contains(RECIPIENT),
            "must not leak the recipient address to host stdout: {line}"
        );
        assert!(
            line.contains("target_configured=true"),
            "keeps the non-sensitive half of the diagnostic: {line}"
        );

        // The delivery that DID land gets no warning of its own — otherwise a
        // healthy run would look broken.
        assert_eq!(
            logs.lines()
                .filter(|l| l.contains(company) && l.contains("was NOT delivered"))
                .count(),
            1,
            "{logs}"
        );
        // …and the run's own summary still reports the split.
        let summary = logs
            .lines()
            .find(|l| l.contains(company) && l.contains("scheduled run finished"))
            .unwrap_or_else(|| panic!("no summary line for {company}: {logs}"));
        // Every count, including the zeroes: asserting only the non-zero ones
        // lets a regression that stops emitting `denied` or `failed` pass.
        assert!(summary.contains("sent=1"), "{summary}");
        assert!(summary.contains("skipped=1"), "{summary}");
        assert!(summary.contains("denied=0"), "{summary}");
        assert!(summary.contains("failed=0"), "{summary}");
        assert!(summary.contains("pending_approval=0"), "{summary}");
        assert!(summary.contains("undelivered=1"), "{summary}");
    }

    /// **Issue #248 — the indirect path.** Scrubbing `report.target` from the
    /// warning above closed the direct route to host stdout. This is the other
    /// one: on the transport-failure arms `report.detail` interpolates the
    /// transport's own words, and a mail transport quotes the mailbox it
    /// refused — an SMTP `550`/`553` reply is routinely of the form
    /// `<recipient@…>: Recipient address rejected`. So a row whose `target` is
    /// scrubbed can still walk the address in through `detail`.
    ///
    /// The `detail` here is verbatim what
    /// [`crate::workflows::delivery`] builds for a refused send, wrapped around
    /// a realistic SMTP reply, so the fixture fails the way production does
    /// rather than the way a mock does.
    ///
    /// **The assertion is over the emitted event**, not over a string on the way
    /// to it: it scans the captured `tracing` output for the line the scheduler
    /// actually wrote. Asserting on `report.reason` alone would prove nothing
    /// about the log — the whole bug was a field being logged that nobody meant
    /// to log.
    #[tokio::test]
    async fn a_transport_failure_does_not_log_the_address_the_transport_quoted() {
        let sink = captured_logs();
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let company = "transport-refusal-co";
        // What `delivery::deliver_one`'s `Err(err)` arm produces, with a reply
        // shaped like a real one.
        let detail = format!(
            "the mail transport refused the message: 550 5.1.1 <{RECIPIENT}>: Recipient address \
             rejected: User unknown in local recipient table"
        );
        let (runner, completed) = RecordingRunner::with_deliveries(vec![reported(
            "owner_summary",
            DeliveryStatus::Failed,
            DeliveryReason::MailTransportRefused,
            &detail,
        )]);
        let registry = company_with_overlays(
            &home,
            company,
            vec![overlay("digest", Some("* * * * *"))],
            Some(runner),
            "running",
        )
        .await;
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = WorkflowScheduler::new(registry, clock);

        assert_eq!(scheduler.tick().await, 1);
        wait_for(|| completed.load(Ordering::SeqCst) == 1).await;
        wait_for(|| {
            captured_text(&sink)
                .lines()
                .any(|l| l.contains(company) && l.contains("was NOT delivered"))
        })
        .await;

        let logs = captured_text(&sink);
        let line = logs
            .lines()
            .find(|l| l.contains(company) && l.contains("was NOT delivered"))
            .unwrap_or_else(|| panic!("no undelivered-report line for {company}: {logs}"));

        // The point of the issue: not through `target`, and not through the
        // transport's reply either.
        assert!(
            !line.contains(RECIPIENT),
            "the transport's reply must not walk the recipient address onto host stdout: {line}"
        );
        // Belt and braces — the address's local part alone is enough to
        // identify a person, so an over-eager "strip the domain" fix must fail
        // this too.
        assert!(
            !line.contains("recipient@"),
            "not even a partial address: {line}"
        );
        // The whole reply is absent, not merely masked mid-string: nothing of
        // the transport's own text reaches the line.
        assert!(
            !line.contains("Recipient address rejected"),
            "no transport-supplied text at all: {line}"
        );

        // …and the line is still worth reading. An operator paged by this needs
        // to know where to look and what class of thing broke.
        assert!(line.contains("digest"), "names the workflow: {line}");
        assert!(line.contains("owner_summary"), "names the node: {line}");
        assert!(
            line.contains("kind=email"),
            "names the destination kind: {line}"
        );
        assert!(line.contains("Failed"), "names the status: {line}");
        assert!(
            line.contains("target_configured=true"),
            "says a target resolved, without saying which: {line}"
        );
        assert!(
            line.contains("the mail transport refused the message"),
            "names the failure class: {line}"
        );
    }

    /// The operator's half is deliberately NOT scrubbed. `detail` is what makes
    /// a refused send fixable, the run response and the journaled
    /// `WorkflowRunFinished` event are tenant-scoped surfaces, and an operator
    /// is entitled to their own recipient's address. Pinned so a later "scrub
    /// it everywhere" sweep has to argue with a test.
    #[test]
    fn the_operator_facing_detail_keeps_the_transport_text() {
        let detail =
            format!("the mail transport refused the message: 550 5.1.1 <{RECIPIENT}>: rejected");
        let row = reported(
            "owner_summary",
            DeliveryStatus::Failed,
            DeliveryReason::MailTransportRefused,
            &detail,
        );
        assert!(row.detail.contains(RECIPIENT), "{row:?}");
        assert_eq!(row.target.as_deref(), Some(RECIPIENT));
        // The two halves disagree on purpose — that IS the fix.
        assert!(!row.reason.to_string().contains(RECIPIENT), "{row:?}");
        assert!(!row.reason.to_string().contains('@'), "{row:?}");
    }

    /// Issue #227: a scheduled run whose report was parked for approval is the
    /// nobody-is-watching case squared — there is no drawer AND the operator
    /// has to be told a card is waiting for them. It must be said, but not as a
    /// failure: no `was NOT delivered` warning, and it must not inflate the
    /// `undelivered` number an alert keys on.
    #[tokio::test]
    async fn a_scheduled_run_reports_a_parked_report_without_crying_wolf() {
        let sink = captured_logs();
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let company = "parked-delivery-co";
        let (runner, completed) = RecordingRunner::with_deliveries(vec![report(
            "owner_summary",
            DeliveryStatus::Pending,
            "parked for operator approval",
        )]);
        let registry = company_with_overlays(
            &home,
            company,
            vec![overlay("digest", Some("* * * * *"))],
            Some(runner),
            "running",
        )
        .await;
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = WorkflowScheduler::new(registry, clock);

        assert_eq!(scheduler.tick().await, 1);
        wait_for(|| completed.load(Ordering::SeqCst) == 1).await;
        // Qualified by BOTH this company and the marker, and scanned per line:
        // `CAPTURE` is shared across every test in the binary, so an
        // unqualified `contains("scheduled run finished")` is satisfied by any
        // sibling test's summary line — including one logged before this run
        // finished — and the lookups below would then fail intermittently.
        wait_for(|| {
            captured_text(&sink)
                .lines()
                .any(|l| l.contains(company) && l.contains("scheduled run finished"))
        })
        .await;

        let logs = captured_text(&sink);
        // Said, and pointed at the place the operator has to go.
        let line = logs
            .lines()
            .find(|l| l.contains(company) && l.contains("parked for operator approval"))
            .unwrap_or_else(|| panic!("no parked-report line for {company}: {logs}"));
        assert!(line.contains("owner_summary"), "names the node: {line}");
        assert!(line.contains("Approvals view"), "says where to go: {line}");
        // Never the recipient's address on host stdout, same as the warn path.
        assert!(
            !line.contains(RECIPIENT),
            "must not leak the recipient address to host stdout: {line}"
        );
        // Not cried wolf about.
        assert!(
            !logs
                .lines()
                .any(|l| l.contains(company) && l.contains("was NOT delivered")),
            "a parked report is not a failed delivery: {logs}"
        );
        let summary = logs
            .lines()
            .find(|l| l.contains(company) && l.contains("scheduled run finished"))
            .unwrap_or_else(|| panic!("no summary line for {company}: {logs}"));
        assert!(summary.contains("pending_approval=1"), "{summary}");
        assert!(summary.contains("undelivered=0"), "{summary}");
        assert!(summary.contains("sent=0"), "{summary}");
    }

    /// A scheduled run that delivered everything says so without crying wolf:
    /// no warning line, and a summary that still accounts for what went out.
    #[tokio::test]
    async fn a_clean_scheduled_run_logs_no_delivery_warning() {
        let sink = captured_logs();
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let company = "clean-delivery-co";
        let (runner, completed) = RecordingRunner::with_deliveries(vec![report(
            "owner_summary",
            DeliveryStatus::Sent,
            "emailed the company's admin",
        )]);
        let registry = company_with_overlays(
            &home,
            company,
            vec![overlay("digest", Some("* * * * *"))],
            Some(runner),
            "running",
        )
        .await;
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = WorkflowScheduler::new(registry, clock);

        assert_eq!(scheduler.tick().await, 1);
        wait_for(|| completed.load(Ordering::SeqCst) == 1).await;
        wait_for(|| {
            captured_text(&sink)
                .lines()
                .any(|l| l.contains(company) && l.contains("scheduled run finished"))
        })
        .await;

        let logs = captured_text(&sink);
        assert!(
            !logs
                .lines()
                .any(|l| l.contains(company) && l.contains("was NOT delivered")),
            "a fully delivered run must not warn: {logs}"
        );
        let summary = logs
            .lines()
            .find(|l| l.contains(company) && l.contains("scheduled run finished"))
            .expect("a summary line");
        assert!(summary.contains("sent=1"), "{summary}");
        assert!(summary.contains("skipped=0"), "{summary}");
        assert!(summary.contains("denied=0"), "{summary}");
        assert!(summary.contains("failed=0"), "{summary}");
        assert!(summary.contains("undelivered=0"), "{summary}");
    }

    // ── Issue #228: the run outcome reaches the journal, not just stdout ─────

    /// Every `WorkflowRunFinished` journaled for `company`.
    /// Yields until the journal holds `want` finished-run records, then returns
    /// them. The append happens after the run completes, so a test that reads
    /// once races it.
    async fn wait_for_outcomes(
        registry: &CompanyRegistry,
        company: &str,
        want: usize,
    ) -> Vec<CompanyEvent> {
        for _ in 0..10_000 {
            let outcomes = run_outcomes(registry, company).await;
            if outcomes.len() >= want {
                return outcomes;
            }
            tokio::task::yield_now().await;
        }
        panic!("only ever saw fewer than {want} finished-run records");
    }

    async fn run_outcomes(registry: &CompanyRegistry, company: &str) -> Vec<CompanyEvent> {
        let id = CompanyId::new(company);
        let runtime = registry.get(&id).expect("registered");
        runtime
            .events()
            .read_from(&id, EventSeq::new(0), usize::MAX)
            .await
            .expect("read journal")
            .into_iter()
            .map(|s| s.event)
            .filter(|e| matches!(e, CompanyEvent::WorkflowRunFinished { .. }))
            .collect()
    }

    /// **The issue.** A scheduled run's delivery rows must land somewhere the
    /// tenant's own console can read back — the log line only reaches whoever
    /// reads host stdout, which on a hosted tenant is not the operator.
    #[tokio::test]
    async fn a_scheduled_run_journals_its_delivery_rows_and_approvals() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let company = "journal-delivery-co";
        let (runner, completed) = RecordingRunner::with_deliveries(vec![
            report(
                "owner_summary",
                DeliveryStatus::Skipped,
                "this recipient has never written to the company",
            ),
            report("also_sent", DeliveryStatus::Sent, "emailed the recipient"),
        ]);
        let registry = company_with_overlays(
            &home,
            company,
            vec![overlay("digest", Some("* * * * *"))],
            Some(runner),
            "running",
        )
        .await;
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = WorkflowScheduler::new(registry.clone(), clock);

        assert_eq!(scheduler.tick().await, 1);
        wait_for(|| completed.load(Ordering::SeqCst) == 1).await;
        // The append happens after the run completes, so wait on the journal
        // rather than racing it.
        let outcomes = loop {
            let outcomes = run_outcomes(&registry, company).await;
            if !outcomes.is_empty() {
                break outcomes;
            }
            tokio::task::yield_now().await;
        };

        assert_eq!(outcomes.len(), 1);
        let CompanyEvent::WorkflowRunFinished {
            workflow_id,
            scheduled,
            deliveries,
            error,
            ..
        } = &outcomes[0]
        else {
            unreachable!("filtered above")
        };
        assert_eq!(workflow_id, "digest");
        assert!(*scheduled, "a cron fire records itself as scheduled");
        assert!(error.is_none());
        assert_eq!(deliveries.len(), 2, "both rows, not just the failed one");
        let skipped = deliveries
            .iter()
            .find(|d| d.node == "owner_summary")
            .expect("the undelivered row");
        assert_eq!(skipped.status, DeliveryStatus::Skipped);
        // The `detail` is the part that says what to fix. The log line carries
        // it too, but the log is not where the operator can look.
        assert!(
            skipped.detail.contains("never written to the company"),
            "{skipped:?}"
        );
        // The journal is operator-scoped — unlike host stdout — so the resolved
        // target rides it. This is the same field the manual run's HTTP response
        // already ships to the console today.
        assert_eq!(skipped.target.as_deref(), Some(RECIPIENT));
    }

    /// **Issue #440: the cron path and the shared path record the same thing.**
    ///
    /// The scheduler used to keep its own copy of "mint the id through the
    /// supervisor, journal the outcome on both arms" — the two rules
    /// [`WorkflowSpawn`](crate::runtime::WorkflowSpawn) owns. The copies agreed,
    /// which is what made the duplication dangerous rather than harmless: a fix
    /// to one would silently miss the other and no test would notice.
    ///
    /// So this runs the same graph through both entry points over one company
    /// (one event log, one runner, one supervisor) and asserts the journaled
    /// records differ in exactly two places: the `scheduled` flag, which is the
    /// one thing the two paths genuinely mean differently, and the run id, which
    /// is fresh per run by construction. Everything else — the workflow id, the
    /// delivery rows, the pending approvals, the error and the cancelled flag —
    /// must match, and a divergence introduced on either side fails here.
    #[tokio::test]
    async fn a_cron_fire_and_a_direct_spawn_journal_the_same_record() {
        use crate::company::parse_workflow;

        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let company = "spawn-parity-co";
        // Non-empty delivery rows, so the comparison covers the part of the
        // record most likely to be dropped by one path and not the other.
        let (runner, completed) = RecordingRunner::with_deliveries(vec![
            report(
                "owner_summary",
                DeliveryStatus::Skipped,
                "this recipient has never written to the company",
            ),
            report("also_sent", DeliveryStatus::Sent, "emailed the recipient"),
        ]);
        let registry = company_with_overlays(
            &home,
            company,
            vec![overlay("digest", Some("* * * * *"))],
            Some(runner),
            "running",
        )
        .await;
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = WorkflowScheduler::new(registry.clone(), clock);

        // --- path 1: the cron fire.
        assert_eq!(scheduler.tick().await, 1);
        wait_for(|| completed.load(Ordering::SeqCst) == 1).await;
        wait_for_outcomes(&registry, company, 1).await;

        // --- path 2: the shared primitive, driven directly over the SAME
        // runtime — same event log, same runner, same supervisor.
        let runtime = registry
            .get(&CompanyId::new(company))
            .expect("the company is registered");
        let runner = runtime
            .workflow_runner()
            .cloned()
            .expect("the same runner the scheduler used");
        let workflow = parse_workflow(&body("digest", Some("* * * * *"))).expect("parses");
        let (_run_id, handle) =
            crate::runtime::WorkflowSpawn::new(&runtime, runner).spawn(workflow, json!({}), false);
        handle.await.expect("the run task completes").expect("runs");
        let outcomes = wait_for_outcomes(&registry, company, 2).await;

        let [cron, direct] = [&outcomes[0], &outcomes[1]].map(|event| {
            let CompanyEvent::WorkflowRunFinished {
                workflow_id,
                scheduled,
                run_id,
                deliveries,
                pending_approvals,
                error,
                cancelled,
            } = event
            else {
                unreachable!("filtered above")
            };
            (
                workflow_id,
                scheduled,
                run_id,
                deliveries,
                pending_approvals,
                error,
                cancelled,
            )
        });

        // The two legitimate differences.
        assert!(*cron.1, "a cron fire is scheduled");
        assert!(!*direct.1, "a directly spawned run is not");
        assert!(cron.2.is_some() && direct.2.is_some(), "both carry an id");
        assert_ne!(cron.2, direct.2, "each run is its own causal root");

        // Everything else is the same record, written by the same code.
        assert_eq!(cron.0, direct.0, "workflow id");
        assert_eq!(cron.3, direct.3, "delivery rows");
        assert_eq!(cron.4, direct.4, "pending approvals");
        assert_eq!(cron.5, direct.5, "error");
        assert_eq!(cron.6, direct.6, "cancelled");
        assert_eq!(cron.3.len(), 2, "the fixture's rows really did survive");
    }

    /// The arm that was quietest of all: a scheduled run that failed outright
    /// produced one host-stdout warning and nothing durable. Now it records the
    /// error where an operator can find it.
    #[tokio::test]
    async fn a_failed_scheduled_run_journals_the_error() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let company = "journal-failure-co";
        let (runner, attempts) = FailingRunner::new("no inference source for agent node `worker`");
        let registry = company_with_overlays(
            &home,
            company,
            vec![overlay("digest", Some("* * * * *"))],
            Some(runner),
            "running",
        )
        .await;
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = WorkflowScheduler::new(registry.clone(), clock);

        assert_eq!(scheduler.tick().await, 1);
        wait_for(|| attempts.load(Ordering::SeqCst) == 1).await;
        let outcomes = loop {
            let outcomes = run_outcomes(&registry, company).await;
            if !outcomes.is_empty() {
                break outcomes;
            }
            tokio::task::yield_now().await;
        };

        assert_eq!(outcomes.len(), 1);
        let CompanyEvent::WorkflowRunFinished {
            scheduled,
            deliveries,
            error,
            ..
        } = &outcomes[0]
        else {
            unreachable!("filtered above")
        };
        assert!(*scheduled);
        assert!(deliveries.is_empty(), "a run that died routed nothing");
        assert!(
            error
                .as_deref()
                .is_some_and(|e| e.contains("no inference source")),
            "the failure reason must survive: {error:?}"
        );
    }

    /// A workflow with no schedule is never fired by the scheduler — it stays
    /// manual-run only.
    #[tokio::test]
    async fn an_unscheduled_workflow_never_fires() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let (runner, started, _completed) = RecordingRunner::new();
        let registry = company_with_overlays(
            &home,
            "acme",
            vec![overlay("manual", None)],
            Some(runner),
            "running",
        )
        .await;
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = WorkflowScheduler::new(registry, clock);

        assert_eq!(scheduler.tick().await, 0);
        assert!(started.lock().unwrap().is_empty());
    }

    /// A paused company fires nothing — the same `ensure_running` guard the
    /// manifest cron scheduler uses, so schedules resume on unpause.
    #[tokio::test]
    async fn a_paused_company_is_skipped() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let (runner, started, _completed) = RecordingRunner::new();
        let registry = company_with_overlays(
            &home,
            "acme",
            vec![overlay("digest", Some("* * * * *"))],
            Some(runner),
            "paused",
        )
        .await;
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = WorkflowScheduler::new(registry, clock);

        assert_eq!(scheduler.tick().await, 0);
        assert!(started.lock().unwrap().is_empty());
    }

    /// No runner wired (the default build) is a clean no-op, not an error — the
    /// same port seam the run route reports `not_wired` on. Because the company
    /// *does* have a scheduled workflow, the skip is announced — exactly once,
    /// no matter how many minutes pass, so a once-a-minute tick cannot bury the
    /// signal it is raising.
    #[tokio::test]
    async fn no_runner_wired_is_a_noop_and_warns_once() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let registry = company_with_overlays(
            &home,
            "acme",
            vec![overlay("digest", Some("* * * * *"))],
            None,
            "running",
        )
        .await;
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = WorkflowScheduler::new(registry, clock.clone());

        assert_eq!(scheduler.tick().await, 0);
        assert_eq!(scheduler.unwired_warnings, 1, "the first skip must be said");

        // Several more matching minutes: still skipped, still silent.
        for minute in 1..5 {
            clock.set(millis_at(2026, 7, 13, 9, minute));
            assert_eq!(scheduler.tick().await, 0);
        }
        assert_eq!(
            scheduler.unwired_warnings, 1,
            "the warning must not repeat every tick"
        );
    }

    /// A company with no runner AND no scheduled workflows is not
    /// misconfigured — it simply has nothing to run, so it stays silent.
    #[tokio::test]
    async fn no_runner_and_no_schedules_does_not_warn() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let registry = company_with_overlays(
            &home,
            "acme",
            vec![overlay("manual", None)],
            None,
            "running",
        )
        .await;
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = WorkflowScheduler::new(registry, clock);

        assert_eq!(scheduler.tick().await, 0);
        assert_eq!(
            scheduler.unwired_warnings, 0,
            "a company with nothing scheduled deserves silence"
        );
    }

    /// The latch is re-armed when the situation changes: a schedule saved onto a
    /// still-unwired company warns, even though an earlier tick already found
    /// that company unwired (with nothing scheduled) and said nothing.
    #[tokio::test]
    async fn a_schedule_added_later_still_warns() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let company = CompanyId::new("acme");
        let registry = company_with_overlays(
            &home,
            "acme",
            vec![overlay("manual", None)],
            None,
            "running",
        )
        .await;
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = WorkflowScheduler::new(registry.clone(), clock.clone());

        assert_eq!(scheduler.tick().await, 0);
        assert_eq!(scheduler.unwired_warnings, 0);

        // The operator saves a schedule on the (still unwired) company.
        let runtime = registry.get(&company).expect("registered");
        let store = runtime.store().clone();
        let mut record = store.load(&company).await.unwrap().unwrap();
        record.overlay_workflows = vec![overlay("digest", Some("* * * * *"))];
        store.save(&record).await.unwrap();

        clock.set(millis_at(2026, 7, 13, 9, 1));
        assert_eq!(scheduler.tick().await, 0);
        assert_eq!(
            scheduler.unwired_warnings, 1,
            "a schedule saved onto an unwired company must be reported"
        );
    }

    /// A malformed graph body skips only itself: the healthy scheduled workflow
    /// beside it still fires.
    #[tokio::test]
    async fn a_malformed_graph_skips_only_itself() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let (runner, started, _completed) = RecordingRunner::new();
        let registry = company_with_overlays(
            &home,
            "acme",
            vec![
                OverlayWorkflow {
                    id: "broken".to_string(),
                    toml: "id = \"broken\"\nname =".to_string(),
                },
                overlay("healthy", Some("* * * * *")),
            ],
            Some(runner),
            "running",
        )
        .await;
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = WorkflowScheduler::new(registry, clock);

        assert_eq!(scheduler.tick().await, 1);
        wait_for(|| started.lock().unwrap().len() == 1).await;
        assert_eq!(started.lock().unwrap()[0].workflow, "healthy");
    }

    /// **Issue #383: a cron fire is cancellable from the console.**
    ///
    /// This is the claim the scheduler's comment makes and nothing pinned. It
    /// is worth pinning because the two things that make it true are both easy
    /// to undo without breaking anything else: the run id has to be minted
    /// through the supervisor **inside** the spawned task, and its guard has to
    /// be held across `record_run_finished` rather than dropped after the run.
    /// Starting the run unregistered, or binding the guard to `_`, would still
    /// pass every other test in this module — the run would fire, complete and
    /// journal exactly as before, and simply stop being stoppable.
    ///
    /// Since issue #440 both are `WorkflowSpawn`'s to keep rather than this
    /// module's, which is the point of routing through it — but the claim is
    /// the scheduler's to make, so the test stays here.
    ///
    /// The cron case matters more than the manual one: nobody chose the timing,
    /// and a wedged nightly run holds its overlap claim, suppressing every later
    /// fire of that schedule until it ends.
    #[tokio::test]
    async fn a_scheduled_run_can_be_cancelled_while_it_is_in_flight() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let company = "cancel-cron-co";
        let gate = Arc::new(Semaphore::new(0));
        let (runner, started, _completed) = RecordingRunner::gated(gate.clone());
        let registry = company_with_overlays(
            &home,
            company,
            vec![overlay("digest", Some("* * * * *"))],
            Some(runner),
            "running",
        )
        .await;
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = WorkflowScheduler::new(registry.clone(), clock);

        assert_eq!(scheduler.tick().await, 1);
        wait_for(|| started.lock().unwrap().len() == 1).await;

        // Discover the run the way the cancel route does — off the company's
        // own supervisor. Nobody handed this id to anybody: the scheduler minted
        // it inside a spawned task, which is exactly why registration is what
        // makes a cron fire reachable at all.
        let runtime = registry
            .get(&CompanyId::new(company))
            .expect("the company is registered");
        wait_for(|| !runtime.run_supervisor().is_empty()).await;
        let live = runtime.run_supervisor().live();
        assert_eq!(live.len(), 1, "the in-flight cron run is registered");
        let (run_id, workflow_id) = live.into_iter().next().unwrap();
        assert_eq!(workflow_id, "digest");

        assert!(
            runtime.run_supervisor().cancel(&run_id),
            "an in-flight cron run is cancellable"
        );

        // It settles as stopped — never released through the gate, so the only
        // way it could have finished is the cancel.
        let outcomes = loop {
            let outcomes = run_outcomes(&registry, company).await;
            if !outcomes.is_empty() {
                break outcomes;
            }
            tokio::task::yield_now().await;
        };
        assert_eq!(outcomes.len(), 1);
        let CompanyEvent::WorkflowRunFinished {
            scheduled,
            cancelled,
            error,
            run_id: journaled_id,
            ..
        } = &outcomes[0]
        else {
            unreachable!("filtered above")
        };
        assert!(
            *scheduled,
            "a cron fire stays flagged scheduled when stopped"
        );
        assert!(*cancelled, "the outcome must record the stop");
        assert!(error.is_none(), "a stop is not a failure: {error:?}");
        assert_eq!(
            journaled_id.as_deref(),
            Some(run_id.as_str()),
            "the outcome carries the id the supervisor registered"
        );

        // And the guard let go — held across the journal write above, released
        // once the task ended.
        wait_for(|| runtime.run_supervisor().is_empty()).await;
    }

    /// A scheduled run still executing suppresses the next fire; once it
    /// completes, the workflow becomes schedulable again.
    #[tokio::test]
    async fn an_in_flight_run_suppresses_the_next_fire() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let gate = Arc::new(Semaphore::new(0));
        let (runner, started, completed) = RecordingRunner::gated(gate.clone());
        let registry = company_with_overlays(
            &home,
            "acme",
            vec![overlay("slow", Some("* * * * *"))],
            Some(runner),
            "running",
        )
        .await;
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = WorkflowScheduler::new(registry, clock.clone());

        // Minute 1: fires, and the run parks on the gate.
        assert_eq!(scheduler.tick().await, 1);
        wait_for(|| started.lock().unwrap().len() == 1).await;
        assert!(scheduler.is_running_any());

        // Minute 2: matches, but the previous run is still in flight.
        clock.set(millis_at(2026, 7, 13, 9, 1));
        assert_eq!(scheduler.tick().await, 0);
        assert_eq!(started.lock().unwrap().len(), 1);

        // Let the first run finish, then wait for its task to release the claim.
        gate.add_permits(1);
        wait_for(|| completed.load(Ordering::SeqCst) == 1).await;
        drain(&scheduler).await;

        // Minute 3: the workflow is schedulable again.
        gate.add_permits(1);
        clock.set(millis_at(2026, 7, 13, 9, 2));
        assert_eq!(
            scheduler.tick().await,
            1,
            "the workflow must be schedulable once its run completed"
        );
        wait_for(|| started.lock().unwrap().len() == 2).await;
    }

    /// A company removed from the registry (archived) is swept out of the
    /// warning latch. Neither re-arm path in `note_unwired` can reach it — both
    /// require the company to still be visited by a tick — so without the sweep
    /// its entry would be orphaned for the life of the process.
    #[tokio::test]
    async fn a_removed_company_is_swept_from_the_warning_latch() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let company = CompanyId::new("acme");
        let registry = company_with_overlays(
            &home,
            "acme",
            vec![overlay("digest", Some("* * * * *"))],
            None, // no runner, so the company latches a warning
            "running",
        )
        .await;
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = WorkflowScheduler::new(registry.clone(), clock.clone());

        assert_eq!(scheduler.tick().await, 0);
        assert_eq!(scheduler.unwired_warnings, 1);
        assert!(scheduler.warned_unwired.contains(&company));

        // The company is archived out of the registry.
        assert!(registry.remove(&company).is_some());

        clock.set(millis_at(2026, 7, 13, 9, 1));
        assert_eq!(scheduler.tick().await, 0);
        assert!(
            scheduler.warned_unwired.is_empty(),
            "a company that no longer exists must not be held in the latch"
        );
    }

    /// The dedupe map keeps only the current minute. Anything older can never
    /// dedupe again, and retaining it would grow the map forever — one entry per
    /// (company, workflow) ever fired, including deleted ones.
    #[tokio::test]
    async fn the_dedupe_map_does_not_grow_without_bound() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let (runner, started, _completed) = RecordingRunner::new();
        let registry = company_with_overlays(
            &home,
            "acme",
            vec![overlay("digest", Some("* * * * *"))],
            Some(runner),
            "running",
        )
        .await;
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = WorkflowScheduler::new(registry, clock.clone());

        assert_eq!(scheduler.tick().await, 1);
        // The entry for the minute just fired is kept — that is what dedupes a
        // second tick inside the same minute.
        assert_eq!(scheduler.last_fired.len(), 1);
        clock.advance(30_000);
        assert_eq!(scheduler.tick().await, 0, "same minute: deduped");

        // Many minutes later the map still holds exactly one entry, not one per
        // minute elapsed. Each iteration drains first: without that, the
        // previous run may still hold its claim and the overlap guard would
        // (correctly) refuse the fire.
        for minute in 1..10 {
            drain(&scheduler).await;
            clock.set(millis_at(2026, 7, 13, 9, minute));
            assert_eq!(scheduler.tick().await, 1, "minute {minute}");
            assert_eq!(scheduler.last_fired.len(), 1, "minute {minute}");
        }
        wait_for(|| started.lock().unwrap().len() == 10).await;
    }

    /// A runner that panics must not strand the in-flight claim. Releasing the
    /// slot after the `await` would leave the key set forever, silently retiring
    /// that schedule for the life of the process; the RAII [`Claim`] releases it
    /// on the unwind instead.
    #[tokio::test]
    async fn a_panicking_run_releases_its_claim() {
        struct PanickingRunner {
            calls: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl WorkflowRunner for PanickingRunner {
            async fn run(
                &self,
                _company: &CompanyId,
                _workflow: &WorkflowFile,
                _input: Value,
                _ctx: &crate::ports::WorkflowRunContext,
            ) -> crate::Result<WorkflowRun> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                panic!("the runner blew up mid-run");
            }
        }

        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let calls = Arc::new(AtomicUsize::new(0));
        let runner = Arc::new(PanickingRunner {
            calls: calls.clone(),
        });
        let registry = company_with_overlays(
            &home,
            "acme",
            vec![overlay("boom", Some("* * * * *"))],
            Some(runner),
            "running",
        )
        .await;
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = WorkflowScheduler::new(registry, clock.clone());

        // Minute 1: fires, and the run panics inside its task.
        assert_eq!(scheduler.tick().await, 1);
        wait_for(|| calls.load(Ordering::SeqCst) == 1).await;
        // The unwind must have released the slot.
        drain(&scheduler).await;

        // Minute 2: the schedule is still alive. Before the RAII guard this
        // fired 0 forever.
        clock.set(millis_at(2026, 7, 13, 9, 1));
        assert_eq!(
            scheduler.tick().await,
            1,
            "a panicking run must not retire the schedule"
        );
        wait_for(|| calls.load(Ordering::SeqCst) == 2).await;
    }

    /// A workflow committed as a seed file (not an overlay) is scheduled too, so
    /// the union really is the read path.
    #[tokio::test]
    async fn a_seed_file_workflow_is_scheduled() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let source = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(source.path().join("workflows")).unwrap();
        std::fs::write(
            source.path().join("workflows").join("seeded.toml"),
            body("seeded", Some("* * * * *")),
        )
        .unwrap();

        let company = CompanyId::new("acme");
        let (runner, started, _completed) = RecordingRunner::new();
        let mut runtime = RuntimeBuilder::new(home.clone(), manifest())
            .with_id(company.clone())
            .build()
            .await
            .unwrap();
        runtime.set_source_dir(Some(source.path().to_path_buf()));
        runtime.set_workflow_runner(runner);
        let registry = CompanyRegistry::new();
        registry.insert(company, Arc::new(runtime));

        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = WorkflowScheduler::new(registry, clock);
        assert_eq!(scheduler.tick().await, 1);
        wait_for(|| started.lock().unwrap().len() == 1).await;
        assert_eq!(started.lock().unwrap()[0].workflow, "seeded");
    }

    /// The helper reads the first scheduled trigger and ignores everything else.
    #[test]
    fn trigger_schedule_reads_the_trigger_only() {
        let scheduled = crate::company::parse_workflow(&body("wf", Some("0 * * * *"))).unwrap();
        assert_eq!(trigger_schedule(&scheduled).as_deref(), Some("0 * * * *"));

        let bare = crate::company::parse_workflow(&body("wf", None)).unwrap();
        assert!(trigger_schedule(&bare).is_none());
    }

    /// An empty registry ticks cleanly (the shape a server with no companies
    /// boots into).
    #[tokio::test]
    async fn an_empty_registry_ticks_to_zero() {
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = WorkflowScheduler::new(CompanyRegistry::new(), clock);
        assert_eq!(scheduler.tick().await, 0);
    }

    // ── Issue #259: the tick IS the reconcile ───────────────────────────────
    //
    // Editing or removing a workflow deliberately ships NO scheduler change.
    // The two tests below are why that is safe rather than an omission, and
    // they are the pin: if someone ever caches the schedule set across ticks —
    // an obvious-looking optimisation, since the record load is per-minute
    // per-company — these fail, instead of a deleted workflow quietly firing
    // forever in production.
    //
    // OpenHuman needs `reconcile_schedule_triggers_on_boot` precisely because
    // it *does* persist a registration: a schedule-trigger flow binds a row in
    // a separate `cron.db`, which can drift from `flows.db` and must be
    // re-synced. We persist no registration at all, so there is nothing to
    // drift and nothing to reconcile.

    /// Replaces a registered company's overlay bodies, standing in for the
    /// `PUT`/`DELETE` routes' record write.
    async fn rewrite_overlays(
        registry: &CompanyRegistry,
        id: &str,
        overlays: Vec<OverlayWorkflow>,
    ) {
        let company = CompanyId::new(id);
        let runtime = registry.get(&company).expect("registered");
        let store = runtime.store().clone();
        let mut record: CompanyRecord = store.load(&company).await.unwrap().unwrap();
        record.overlay_workflows = overlays;
        store.save(&record).await.unwrap();
    }

    /// **Delete needs no scheduler teardown.** The workflow fires, is removed
    /// from the record, and never fires again — no restart, no unbind call.
    #[tokio::test]
    async fn a_deleted_workflow_stops_firing_on_the_very_next_tick() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let (runner, started, _completed) = RecordingRunner::new();
        let registry = company_with_overlays(
            &home,
            "acme",
            vec![overlay("digest", Some("0 9 * * *"))],
            Some(runner),
            "running",
        )
        .await;

        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = WorkflowScheduler::new(registry.clone(), clock.clone());

        // It fires while it exists.
        assert_eq!(scheduler.tick().await, 1);
        wait_for(|| started.lock().unwrap().len() == 1).await;
        drain(&scheduler).await;

        // The operator deletes it (the route drops the overlay body).
        rewrite_overlays(&registry, "acme", Vec::new()).await;

        // Next matching minute: nothing. No process restart in between.
        clock.set(millis_at(2026, 7, 14, 9, 0));
        assert_eq!(
            scheduler.tick().await,
            0,
            "a deleted workflow must not keep firing"
        );
        assert_eq!(started.lock().unwrap().len(), 1);
    }

    /// **Edit needs no rebinding.** A corrected cron takes effect on the next
    /// tick: the old expression stops matching and the new one starts. This is
    /// the issue's own example — a typo'd schedule that used to be permanent.
    #[tokio::test]
    async fn an_edited_schedule_takes_effect_without_rebinding() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let (runner, started, _completed) = RecordingRunner::new();
        let registry = company_with_overlays(
            &home,
            "acme",
            vec![overlay("digest", Some("0 9 * * *"))],
            Some(runner),
            "running",
        )
        .await;

        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = WorkflowScheduler::new(registry.clone(), clock.clone());

        assert_eq!(scheduler.tick().await, 1);
        wait_for(|| started.lock().unwrap().len() == 1).await;
        drain(&scheduler).await;

        // The operator corrects 09:00 → 10:00.
        rewrite_overlays(
            &registry,
            "acme",
            vec![overlay("digest", Some("0 10 * * *"))],
        )
        .await;

        // The OLD cadence is dead…
        clock.set(millis_at(2026, 7, 14, 9, 0));
        assert_eq!(
            scheduler.tick().await,
            0,
            "the replaced schedule must stop firing"
        );

        // …and the NEW one is live, with no restart and no rebind call.
        clock.set(millis_at(2026, 7, 14, 10, 0));
        assert_eq!(
            scheduler.tick().await,
            1,
            "the corrected schedule must start firing"
        );
        wait_for(|| started.lock().unwrap().len() == 2).await;
        drain(&scheduler).await;
    }

    /// Adding a schedule to a previously manual workflow arms it on the next
    /// tick — no `enabled` toggle involved, because this scheduler deliberately
    /// does not gate on `[workflows].enabled` (see [`WorkflowScheduler::tick`]).
    ///
    /// This is also the honest record of what #259 does NOT ship: OpenHuman's
    /// `flows_update` forces `enabled = false` on exactly this manual→automatic
    /// transition so a schedule cannot go live unreviewed. That rule has no
    /// lever here yet — writing `enabled = false` would stop nothing — and note
    /// that create already behaves identically, so an edit is no riskier than
    /// authoring the same graph fresh. Adopting the disarm rule means first
    /// reversing the gating decision, for create and update together.
    #[tokio::test]
    async fn adding_a_schedule_by_edit_arms_the_workflow_on_the_next_tick() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let (runner, started, _completed) = RecordingRunner::new();
        let registry = company_with_overlays(
            &home,
            "acme",
            vec![overlay("digest", None)], // manual-run only
            Some(runner),
            "running",
        )
        .await;

        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = WorkflowScheduler::new(registry.clone(), clock.clone());
        assert_eq!(scheduler.tick().await, 0, "no schedule, no fire");

        rewrite_overlays(
            &registry,
            "acme",
            vec![overlay("digest", Some("0 9 * * *"))],
        )
        .await;

        clock.set(millis_at(2026, 7, 14, 9, 0));
        assert_eq!(scheduler.tick().await, 1);
        wait_for(|| started.lock().unwrap().len() == 1).await;
        drain(&scheduler).await;
    }
}
