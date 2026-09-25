//! Starting a supervised workflow run, in one place (issue #395).
//!
//! Two things have to happen around every workflow run an entry point starts,
//! and neither is optional:
//!
//! 1. the run id is minted **through the [`RunSupervisor`]**, which registers
//!    its stop signal — that id is the address `POST …/workflows/runs/{id}/cancel`
//!    sends to, so a run started any other way is one an operator cannot stop;
//! 2. the outcome is journaled through
//!    [`record_run_finished`] on **both** arms, holding the
//!    [`RunGuard`](super::RunGuard) across the write, so a run that failed is
//!    recorded exactly as loudly as one that succeeded.
//!
//! That discipline lived inside the console run route's private
//! `spawn_workflow_run`. Issue #395 added a second entry point — resuming a
//! `requires_approval` node the operator signed off, which is a **new run** and
//! therefore owes the same two things — and a second copy of a rule this
//! specific is a rule that drifts. So it moves here, and every caller constructs
//! a [`WorkflowSpawn`] instead.
//!
//! # The cron scheduler too (issue #440)
//!
//! The cron [`WorkflowScheduler`](super::WorkflowScheduler) used to keep its own
//! spawn body, on the argument that its schedule *claim* and its per-delivery
//! log sweep would make this helper a union of two jobs. That argument was
//! wrong about where the seam is. The claim and the sweep are the scheduler's
//! and stay there — it takes the claim before starting, holds it across an
//! **awaited** [`spawn`](Self::spawn) handle, and folds the returned outcome
//! into its host-stdout summary. What it no longer keeps is a second copy of
//! the two rules above.
//!
//! The copies agreed at the time, and that is precisely what made them
//! dangerous: two identical implementations of a discipline mean a fix to
//! either one silently misses the other, with nothing failing to say so.
//!
//! Awaiting the handle is what makes that sharing work for a scheduled fire:
//! the outcome is journaled inside the task, so by the time the handle resolves
//! the record exists and the claim can be released.
//!
//! # Owned parts, not the runtime
//!
//! [`WorkflowSpawn`] holds four cloned handles rather than an
//! `Arc<CompanyRuntime>`. The spawned task genuinely needs nothing else, and
//! taking only what it needs is what lets the resume arm — which reaches this
//! from `perform_effect`, holding a bare `&CompanyRuntime` — start a run at all
//! without threading a self-referential `Arc` through the runtime.

use std::sync::Arc;

use futures::future::FutureExt;
use serde_json::Value;
use tokio::task::JoinHandle;

use crate::Result;
use crate::company::WorkflowFile;
use crate::company::runtime::CompanyRuntime;
use crate::ports::types::CompanyId;
use crate::ports::{EventLog, RunStore, WorkflowRun, WorkflowRunContext, WorkflowRunner};
use crate::runtime::workflow_outcome::{FailedRun, record_run_finished};
use crate::runtime::{RunGuard, RunSupervisor};

/// The error stamped on a run whose task **panicked** before it could journal a
/// finish (issue #1009).
///
/// Phrased, like [`INTERRUPTED_BY_RESTART`](crate::runtime::INTERRUPTED_BY_RESTART),
/// as a host fact rather than a workflow fault the operator can act on at the
/// node level: the run's task came apart, and the nodes recorded against it are
/// the ones that completed before it did. A caught unwind journals this so the
/// run stops reading `running: true` forever, then re-raises so the run's task
/// still resolves to a `JoinError`.
///
/// Shared with the orchestrator's own `run_workflow` tool path (issue #1865):
/// that call sits inside an agent turn rather than its own supervised task, so
/// it cannot re-raise the way this module's catch does — but the run it
/// panicked out of owes the exact same honest finish, worded identically
/// rather than a second, possibly-drifting sentence for the same fact.
pub(crate) const PANICKED_BEFORE_FINISH: &str = concat!(
    "this run's task panicked before it finished; ",
    "the nodes recorded against it are the ones that completed before it stopped"
);

/// The `detail` a company-wide "workflow run failed" notification carries when
/// the engine itself returned an `Err` (issue #1865, CodeRabbit review PR
/// #1883).
///
/// Deliberately fixed rather than `err.to_string()`: this notification's
/// `audience` is `None` (every company user), while the real error — already
/// stamped onto this run's own `WorkflowRunFinished` a few lines above — is
/// only readable back through the authorized run-history route. Interpolating
/// the raw engine error here would broadcast whatever internal detail it
/// happens to carry to everyone in the company instead of just the people who
/// open that run.
///
/// Shared with the orchestrator's own `run_workflow` tool path (PR #1883
/// review comment 3877185396): that call is the second run-outcome
/// chokepoint [`WorkflowSpawn::notifications`] names as not routing through
/// here, and its failure arm owes the identical wording rather than a second,
/// possibly-drifting sentence for the same fact — the same reasoning
/// [`PANICKED_BEFORE_FINISH`] above already applies.
pub(crate) const RUN_FAILED_DETAIL: &str =
    "the run errored before it could finish; open its run history for the reason";

/// Everything starting a supervised workflow run needs, and nothing else.
#[derive(Clone)]
pub struct WorkflowSpawn {
    company: CompanyId,
    events: Arc<dyn EventLog>,
    supervisor: RunSupervisor,
    runner: Arc<dyn WorkflowRunner>,
    runs: Arc<dyn RunStore>,
    /// Issue #1865: where a settled-but-unhealthy run (failed, blocked,
    /// stranded) is announced, since this is one of the two chokepoints every
    /// supervised run's outcome passes through on its way to the journal — the
    /// other is the orchestrator's own `run_workflow` tool path, which is not
    /// spawned through here at all (see that call site's own comment).
    notifications: Arc<dyn crate::ports::notifications::NotificationStore>,
}

impl WorkflowSpawn {
    /// Reads the three shared handles off `runtime` and pairs them with the
    /// runner the caller already resolved.
    ///
    /// The runner is a parameter rather than read from
    /// [`CompanyRuntime::workflow_runner`] because the caller has to decide what
    /// its absence *means* — the console route distinguishes "this build has no
    /// workflow execution" from "this boot has none because inference was
    /// configured after start", and answers two different statuses. Swallowing
    /// that distinction into an `Option<Self>` here would push both callers back
    /// onto one unhelpful message.
    pub fn new(runtime: &CompanyRuntime, runner: Arc<dyn WorkflowRunner>) -> Self {
        Self {
            company: runtime.id().clone(),
            events: runtime.events().clone(),
            supervisor: runtime.run_supervisor().clone(),
            runner,
            runs: runtime.runs().clone(),
            notifications: runtime.notifications().clone(),
        }
    }

    /// Registers a run, spawns it, and returns its id alongside the task.
    ///
    /// `scheduled` says whether a cron started it, and rides both the run's
    /// `WorkflowRunStarted` and its `WorkflowRunFinished` — one parameter
    /// feeding both, so the pair can never disagree about what kind of run it
    /// was.
    ///
    /// The returned [`JoinHandle`] may be awaited (the console's synchronous
    /// mode, and the cron scheduler, which has to hold its overlap claim for
    /// the length of the run) or dropped (the console's detached mode, and the
    /// resume arm). Dropping it abandons the *waiting*, never the work: the
    /// task holds its own guard, journals its own outcome, and deregisters
    /// itself on every exit path including an unwind. Awaiting it therefore
    /// resolves only once the outcome is already durable.
    ///
    /// `dry_run` (issue #542) makes this a **test run**: the flag is stamped
    /// onto the run's [`WorkflowRunContext`] (the supervisor still registers it,
    /// so a dry run stays cancellable and free), and the outcome journal write
    /// below is skipped on **both** arms — a test run leaves nothing durable, so
    /// [`record_run_finished`] must not write a `WorkflowRunFinished` for it any
    /// more than the runner writes a `WorkflowRunStarted`. Every entry point but
    /// the run route passes `false`; a scheduled or resumed run is always real.
    ///
    /// # Fallible before it spawns (issue #401)
    ///
    /// [`begin`](RunSupervisor::begin) admits the run against the company's
    /// concurrency ceiling *before* any task exists, so a company at its cap
    /// gets an `Err(WorkflowRunLimit)` here and **nothing is started** — no
    /// task, no `WorkflowRunStarted`, no run id. A dry run counts too: it drives
    /// the real engine and spends real inference, so it is registered like any
    /// other run and the flag is only stamped afterwards.
    pub fn spawn(
        self,
        workflow: WorkflowFile,
        input: Value,
        scheduled: bool,
        dry_run: bool,
    ) -> Result<(String, JoinHandle<Result<WorkflowRun>>)> {
        // Issue #371 mints the id above the runner so the error arm can still
        // correlate; issue #383 mints it HERE, through the supervisor, so the
        // same id is also an address an operator can send "stop" to.
        // Deliberately not a second identifier — the run id the console already
        // correlates SSE frames on IS the cancellation handle.
        //
        // Issue #401: `begin` is the concurrency choke point and is fallible —
        // over the cap it refuses here, before the `tokio::spawn` in
        // `spawn_admitted`, so a rejected run leaves nothing behind to journal
        // or reap.
        let (ctx, guard) = self.supervisor.begin(&workflow.id, scheduled)?;
        Ok(self.spawn_admitted(ctx, guard, workflow, input, dry_run))
    }

    /// Spawns a run whose slot the caller has **already** admitted through
    /// [`RunSupervisor::begin`], threading in the resulting `(ctx, guard)`.
    ///
    /// [`spawn`](Self::spawn) is `begin` + this. The split exists for the cron
    /// [`WorkflowScheduler`](super::WorkflowScheduler) (issue #661): it must
    /// order admission *before* its durable minute-claim, so that a company at
    /// its in-flight cap never claims (and durably burns) a minute it cannot
    /// run. It therefore calls `begin` itself, on the tick thread, holds the
    /// guard across `claim_fire`, and only then hands the admitted `(ctx, guard)`
    /// here to start the run — with the guard already counting against the cap,
    /// so a same-tick sibling schedule sees an exact count rather than a stale
    /// one. Every other caller uses [`spawn`](Self::spawn) and never sees the
    /// guard.
    ///
    /// Infallible: the fallible step is `begin`, which the caller has already
    /// passed. `scheduled` for the journal is read off the admitted `ctx`, so it
    /// cannot disagree with what `begin` registered.
    ///
    /// `dry_run` (issue #542) is stamped on the admitted context here rather than
    /// at `begin`, so the supervisor is untouched — a dry run registers and
    /// cancels exactly like a real one.
    pub(crate) fn spawn_admitted(
        self,
        mut ctx: WorkflowRunContext,
        guard: RunGuard,
        workflow: WorkflowFile,
        input: Value,
        dry_run: bool,
    ) -> (String, JoinHandle<Result<WorkflowRun>>) {
        let scheduled = ctx.scheduled;
        ctx.dry_run = dry_run;
        let run_id = ctx.run_id.clone();
        let handle = tokio::spawn(async move {
            // Held for the whole run INCLUDING the journal write below, so the
            // window in which a cancel is accepted matches the window in which
            // it can still do anything. Dropping on every exit path, unwind
            // included, is why this is a guard rather than a call at the end.
            let _guard = guard;
            // Issue #1009 (path A): the runner future can **unwind** — a panic
            // in a node, a poisoned lock — and an unwind jumps straight past the
            // journal write below, so the run's `WorkflowRunStarted` never gets a
            // matching finish and `GET …/workflows/runs` folds it `running: true`
            // forever, until the next boot sweep settles it. The console shows a
            // run that will never stop with a Stop button that cannot help.
            //
            // Catching the unwind *here* — inside the task, while `_guard` is
            // still held, so the finish lands BEFORE the supervisor entry drops
            // and no read-side rebuild race can open — lets us journal a finish
            // for the panicked run, then re-raise the exact payload so the task
            // still resolves to a `JoinError`. That keeps the synchronous mode's
            // 500 (at the console run route) and the scheduler's `tracing::error`
            // unchanged: nothing downstream can tell the panic was intercepted.
            //
            // Deliberately an in-task catch, not a separate watchdog task: only
            // this way does the write stay inside the guard's lifetime and change
            // no sync/detach contract.
            let result = match std::panic::AssertUnwindSafe(self.runner.run(
                &self.company,
                &workflow,
                input,
                &ctx,
            ))
            .catch_unwind()
            .await
            {
                Ok(result) => result,
                Err(payload) => {
                    // A dry run journals nothing on panic either, exactly as the
                    // clean path below skips its finish — a test run must leave
                    // no `WorkflowRunFinished` for the history to fold.
                    if !dry_run {
                        let journaled = record_run_finished(
                            &self.events,
                            &self.company,
                            &workflow.id,
                            scheduled,
                            &ctx.run_id,
                            Err(PANICKED_BEFORE_FINISH.into()),
                        )
                        .await;
                        // Issue #1009 (path B, surfaced): if even this finish
                        // could not be appended, the run is right back to reading
                        // in-flight until the next boot sweep — a state worth an
                        // error line naming the run, not a swallowed warn.
                        if !journaled {
                            tracing::error!(
                                company = %self.company,
                                workflow = %workflow.id,
                                run_id = %ctx.run_id,
                                "a panicked workflow run's finish could not be journaled; \
                                 it will read as in-flight until the next boot sweep settles it"
                            );
                        }
                        // Issue #1865: a panic is unambiguously the worst
                        // reading a run can settle with — notify without
                        // needing a verdict computation.
                        self.notify_run_unhealthy(
                            &workflow.id,
                            &ctx.run_id,
                            "failed",
                            PANICKED_BEFORE_FINISH,
                        )
                        .await;
                    }
                    // Re-raise: the JoinHandle still resolves to a JoinError, so
                    // the synchronous caller still 500s. The finish is durable.
                    std::panic::resume_unwind(payload);
                }
            };
            // Issue #542: a dry run journals NOTHING. The runner already skipped
            // the started + per-node rows; skipping the finish here keeps the
            // pair honest, so a test run leaves no `WorkflowRunFinished` for the
            // history to fold and no boot sweep to adopt. The settled result is
            // the whole record, and it still flows back to the awaiting caller.
            // `result` can be absent when the runner's hard-abort path drops the
            // engine future. Settle every workflow-node attempt that is still
            // active before publishing the run outcome, so cancellation cannot
            // leave Observatory showing a permanently running attempt.
            if !dry_run && ctx.cancel.is_cancelled() {
                settle_cancelled_workflow_attempts(self.runs.as_ref(), &self.company, &ctx.run_id)
                    .await;
            }
            if !dry_run {
                // closed the tab; the record is what is still there tomorrow.
                let outcome = match result.as_ref() {
                    Ok(run) => Ok(run),
                    // Issue #1008: the message AND whatever the run had already
                    // done. `partial_run` is `Some` only when the engine broke
                    // after nodes had run, so a run refused before it started
                    // still journals an honestly empty row.
                    Err(err) => Err((err.to_string(), err.partial_run())),
                };
                let journaled = match outcome {
                    Ok(run) => {
                        record_run_finished(
                            &self.events,
                            &self.company,
                            &workflow.id,
                            scheduled,
                            &ctx.run_id,
                            Ok(run),
                        )
                        .await
                    }
                    Err((err, partial)) => {
                        record_run_finished(
                            &self.events,
                            &self.company,
                            &workflow.id,
                            scheduled,
                            &ctx.run_id,
                            Err(FailedRun {
                                error: err.as_str(),
                                partial,
                            }),
                        )
                        .await
                    }
                };
                // Issue #1009 (path B, surfaced): a swallowed append leaves the
                // run reading `running: true` until the next boot sweep. The
                // helper still swallows the append error itself (it must never
                // fail the run), but a finish that did not land is worth an error
                // line naming the run — not only the helper's warn — so the hole
                // is visible in telemetry rather than only at the next restart.
                // Issue #1865: failed/blocked/stranded, the unhealthy readings
                // this run can settle with — an operator who was not watching
                // (this is the scheduled/detached path) learns from here
                // rather than by opening Run History. `undelivered`,
                // `degraded` and `awaiting-approval` are real but softer
                // readings the run's own surfaces (the delivery badge, the
                // node chip, the Approvals page) already carry; this
                // notification is reserved for the three that leave a run
                // with nothing more it can do on its own.
                match &result {
                    // Issue #1865 (PR #1883 review comment 3878430677): tested
                    // BEFORE the stranded/blocked arms below. The clean
                    // node-boundary cancel arm in `run_workflow_inner`
                    // (`src/workflows/runner.rs`, `if outcome.cancelled`)
                    // carries `blocked_nodes: blocks.take()` — whatever the
                    // run had already gated before the operator's stop landed
                    // — so a cancelled run can reach this match with a
                    // non-empty `blocked_nodes` exactly like a run that is
                    // genuinely waiting on a person. Without this arm the
                    // `blocked` arm below would fire "this run stopped
                    // because a step is waiting on a person to decide
                    // something" for a run an operator already decided to
                    // stop — nobody needs to go decide anything, because the
                    // run will not continue either way. This mirrors
                    // `WorkflowRunVerdict::of`, which checks `cancelled`
                    // before `blocked_nodes` for the identical reason ("a
                    // stop somebody asked for is not a fault"); this guard's
                    // own doc comment above lists only `failed`/`blocked`/
                    // `stranded` as the readings it notifies for, and a
                    // cancelled run is none of those.
                    Ok(run) if run.cancelled => {}
                    // Issue #1865 (Codex review): tested BEFORE the generic
                    // `blocked_nodes` arm below. `HarnessAgentRunner` pushes a
                    // `WorkflowBlockedNode` whenever a turn gated anything at
                    // all — `ParkedCalls::is_empty` is false the moment
                    // `unparkable > 0`, even with zero `approval_ids` — so a
                    // node that failed to park a single call still lands in
                    // `blocked_nodes` exactly like one with a live card. The
                    // call-level `unparkable()` check this arm replaced had
                    // the same blind spot the other direction: one parked call
                    // beside one failed park on the same node is not stranded
                    // (an operator can still act on the card), which
                    // `stranded_approvals`'s per-node grouping — the same
                    // reconciliation the sync run response uses — gets right
                    // and a bare `.any()` over `approvals` cannot.
                    //
                    // Codex review (PR #1883): `> 0` alone is also wrong when
                    // only SOME pending nodes are stranded — e.g. one
                    // `ParkFailed` node beside one node with a live `Parked`
                    // card. That run is only partly stranded: the live card
                    // is still decidable, so it must read `blocked`, not
                    // `stranded`. This mirrors the equality check
                    // `RunVerdictFacts::fully_stranded` uses for the sync run
                    // response — `stranded_approvals` can never exceed
                    // `pending_approvals.len()`, so `==` is "every pending
                    // node lost its card", the only case "nobody was asked"
                    // is honest.
                    //
                    // Codex review (PR #1883, comment 3875617184): a `Pending`
                    // delivery row is a *second* thing waiting on a person,
                    // on the approvals queue rather than the gate join, and
                    // `fully_stranded` excludes it for exactly that reason —
                    // see its doc comment in `workflow_verdict.rs`. This arm
                    // must apply the same exclusion, or a run whose gates are
                    // all lost but whose report is parked for delivery still
                    // gets told "nothing is waiting on it any more, and
                    // nobody was asked" while a report is parked waiting on
                    // exactly that.
                    Ok(run)
                        if !run.pending_approvals.is_empty()
                            && crate::ports::workflow_runner::stranded_approvals(
                                &run.pending_approvals,
                                &run.approvals,
                            ) == run.pending_approvals.len()
                            && !run.deliveries.iter().any(|d| {
                                matches!(d.status, crate::ports::DeliveryStatus::Pending)
                            }) =>
                    {
                        self.notify_run_unhealthy(
                            &workflow.id,
                            &ctx.run_id,
                            "stranded",
                            "This run tried to park an approval and could not — nothing is \
                             waiting on it any more, and nobody was asked.",
                        )
                        .await;
                    }
                    Ok(run) if !run.blocked_nodes.is_empty() => {
                        self.notify_run_unhealthy(
                            &workflow.id,
                            &ctx.run_id,
                            "blocked",
                            "This run stopped because a step is waiting on a person to decide \
                             something.",
                        )
                        .await;
                    }
                    Ok(_) => {}
                    // CodeRabbit review (PR #1883): the raw engine error can
                    // carry internal detail (tool arguments, host paths,
                    // dependency errors) that must not fan out to every
                    // company user — `notify_run_unhealthy` is company-wide
                    // (`audience: None`). The real text is already durable in
                    // this run's own `WorkflowRunFinished` above, which is
                    // read back through the authorized run-history route; the
                    // notification only needs to say a run failed and point
                    // at it, the same discipline the panic arm above already
                    // applies via `PANICKED_BEFORE_FINISH`.
                    Err(_) => {
                        self.notify_run_unhealthy(
                            &workflow.id,
                            &ctx.run_id,
                            "failed",
                            RUN_FAILED_DETAIL,
                        )
                        .await;
                    }
                }
                if !journaled {
                    tracing::error!(
                        company = %self.company,
                        workflow = %workflow.id,
                        run_id = %ctx.run_id,
                        "a finished workflow run could not be journaled; \
                         it will read as in-flight until the next boot sweep settles it"
                    );
                }
            }
            result
        });
        (run_id, handle)
    }

    /// Files a durable notification that a supervised run settled unhealthy —
    /// failed, blocked, or stranded (issue #1865). Best-effort and after the
    /// journal write, matching every other notification producer in the tree:
    /// a notification that could not be filed must not touch the run's own
    /// outcome, which has already landed.
    ///
    /// `kind` is one of `"failed"` / `"blocked"` / `"stranded"`, not
    /// [`WorkflowRunVerdict`] itself — this fires off the settle's own shape
    /// (an `Err`, a non-empty `blocked_nodes`, an unparkable approval), not a
    /// full verdict read, which needs the live-approvals queue join this
    /// hot path deliberately does not make (see the sync run response's own
    /// `stranded_approvals` comment in `server::ops::workflows`).
    ///
    /// Thin wrapper over [`file_run_unhealthy_notification`] — see that
    /// function for why the write itself is free-standing rather than kept
    /// only here.
    async fn notify_run_unhealthy(
        &self,
        workflow_id: &str,
        run_id: &str,
        kind: &str,
        detail: &str,
    ) {
        file_run_unhealthy_notification(
            self.notifications.as_ref(),
            &self.company,
            workflow_id,
            run_id,
            kind,
            detail,
        )
        .await;
    }
}

/// Files a durable notification that a workflow run settled unhealthy —
/// failed, blocked, or stranded (issue #1865). Best-effort and after the
/// journal write, matching every other notification producer in the tree: a
/// notification that could not be filed must not touch the run's own
/// outcome, which has already landed.
///
/// Free-standing rather than a [`WorkflowSpawn`] method so the orchestrator's
/// `run_workflow` tool (PR #1883 review comment 3877185396) can reach the
/// identical write. That tool is the second run-outcome chokepoint
/// [`WorkflowSpawn::notifications`]'s own doc comment names as not routing
/// through this type at all — it never builds a `WorkflowSpawn` (no spawned
/// task, no `RunStore` handle; see that call site's own comment on why it
/// cannot re-raise the way this module's catch does) — so duplicating this
/// write inline there, instead of sharing it, would be exactly the kind of
/// second copy of a rule this module's file-level doc comment already warns
/// drifts.
pub(crate) async fn file_run_unhealthy_notification(
    notifications: &dyn crate::ports::notifications::NotificationStore,
    company: &CompanyId,
    workflow_id: &str,
    run_id: &str,
    kind: &str,
    detail: &str,
) {
    let note = crate::ports::notifications::Notification {
        id: crate::ports::generate_id(),
        kind: format!("workflow_run_{kind}"),
        subject: crate::ports::notifications::Subject {
            kind: crate::ports::notifications::SubjectKind::Run,
            id: run_id.to_string(),
        },
        created_at: crate::ports::now_millis(),
        title: format!("Workflow `{workflow_id}` {kind}: {detail}"),
        audience: None,
        context: None,
    };
    if let Err(err) = notifications.append(company, &note).await {
        tracing::warn!(
            company = %company,
            workflow = %workflow_id,
            run = %run_id,
            error = %err,
            "a workflow-run-unhealthy notification could not be recorded; the run's own \
             outcome is unaffected, but nobody is badged for it"
        );
    }
}

async fn settle_cancelled_workflow_attempts(
    runs: &dyn RunStore,
    company: &CompanyId,
    workflow_run_id: &str,
) {
    let active = match runs
        .list_runs(
            company,
            &crate::ports::RunFilter::for_workflow_run(workflow_run_id.to_string()),
        )
        .await
    {
        Ok(active) => active,
        Err(err) => {
            tracing::error!(
                %company,
                %workflow_run_id,
                %err,
                "cancelled workflow: could not list active agent attempts"
            );
            return;
        }
    };
    for attempt in active {
        if let Err(err) = runs
            .finish_run(
                company,
                &attempt.id,
                crate::ports::RunOutcome::new(crate::ports::RunStatus::Cancelled)
                    .with_error("the workflow run was cancelled before this attempt settled"),
            )
            .await
        {
            tracing::error!(
                %company,
                attempt = %attempt.id,
                %workflow_run_id,
                %err,
                "cancelled workflow: could not settle agent attempt"
            );
        }
    }
}

#[cfg(test)]
#[path = "workflow_spawn_tests.rs"]
mod tests;
