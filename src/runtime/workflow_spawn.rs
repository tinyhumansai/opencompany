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

    /// [`spawn`](Self::spawn), for a caller that knows who is really behind the
    /// run and wants the journal to say so rather than settle for `begin`'s
    /// `scheduled`-derived default (issue #1862 prerequisite).
    ///
    /// A resumed workflow — a paused gate approved, or a blocked agent node's
    /// call approved — is exactly this caller: `scheduled` is always `false`
    /// for a resume (issue #542), which on its own would stamp every
    /// continuation `StartedBy::Operator` regardless of who or what actually
    /// triggered the run that paused. `started_by` overrides that default on
    /// the admitted context before the task is spawned, so the attribution the
    /// paused run carried (or the trigger site stamped) survives the re-run
    /// rather than resetting.
    pub fn spawn_as(
        self,
        workflow: WorkflowFile,
        input: Value,
        scheduled: bool,
        dry_run: bool,
        started_by: crate::ports::types::StartedBy,
    ) -> Result<(String, JoinHandle<Result<WorkflowRun>>)> {
        let (ctx, guard) = self.supervisor.begin(&workflow.id, scheduled)?;
        let ctx = ctx.with_started_by(started_by);
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
mod tests {
    use super::*;
    use crate::ports::types::{CompanyEvent, EventSeq};
    use crate::store::FsEventLog;
    use async_trait::async_trait;

    /// A runner whose `run` **panics** — the path-A failure issue #1009 fixes.
    /// An unwind here used to jump straight past the finish journal, leaving the
    /// run reading `running: true` until the next boot sweep.
    struct PanickingRunner;

    #[async_trait]
    impl WorkflowRunner for PanickingRunner {
        async fn run(
            &self,
            _company: &CompanyId,
            _workflow: &WorkflowFile,
            _input: Value,
            _ctx: &WorkflowRunContext,
        ) -> Result<WorkflowRun> {
            panic!("the run blew up");
        }
    }

    /// A runner whose `run` returns an `Err` carrying a distinctive, made-up
    /// internal detail — a stand-in for the kind of thing a real engine error
    /// can plausibly say. CodeRabbit review (PR #1883) flagged that this text
    /// used to be interpolated straight into a company-wide notification.
    struct EngineFailingRunner;

    /// The made-up internal detail `EngineFailingRunner` fails with. Chosen to
    /// look like something that must never fan out to every company user.
    const ENGINE_FAILURE_SECRET: &str = "token=sk-leaked-1234 at /internal/host/path";

    #[async_trait]
    impl WorkflowRunner for EngineFailingRunner {
        async fn run(
            &self,
            _company: &CompanyId,
            _workflow: &WorkflowFile,
            _input: Value,
            _ctx: &WorkflowRunContext,
        ) -> Result<WorkflowRun> {
            Err(crate::error::OpenCompanyError::Store(
                ENGINE_FAILURE_SECRET.to_string(),
            ))
        }
    }

    /// Issue #1865 (CodeRabbit review, PR #1883): a run that fails with an
    /// engine `Err` must NOT leak that error's raw text into the company-wide
    /// `workflow_run_failed` notification — `notify_run_unhealthy`'s audience
    /// is `None` (everyone), while the real error is only readable back
    /// through the authorized run-history route. Before the fix, this arm
    /// interpolated `err.to_string()` straight into the notification title,
    /// so `ENGINE_FAILURE_SECRET` would have shown up in it verbatim.
    #[tokio::test]
    async fn a_failed_run_does_not_leak_the_raw_engine_error_into_its_notification() {
        let dir = tempfile::Builder::new()
            .prefix("oc-spawn-failed-")
            .tempdir()
            .expect("tempdir");
        let events: Arc<dyn EventLog> = Arc::new(FsEventLog::new(dir.path()));
        let company = CompanyId::new("acme");
        let notifications = Arc::new(crate::store::FsOps::new(dir.path().to_path_buf()));
        let spawn = WorkflowSpawn {
            company: company.clone(),
            events: events.clone(),
            supervisor: RunSupervisor::new(),
            runner: Arc::new(EngineFailingRunner),
            runs: Arc::new(crate::store::FsOps::new(dir.path().to_path_buf())),
            notifications: notifications.clone(),
        };

        let (run_id, handle) = spawn
            .spawn(empty_workflow(), Value::Null, false, false)
            .expect("under the default cap");
        handle.await.expect("join").expect_err("the engine failed");

        use crate::ports::notifications::NotificationStore;
        let notes = notifications
            .list(&company, "anyone")
            .await
            .expect("list notifications");
        let failed = notes
            .iter()
            .find(|n| {
                n.notification.kind == "workflow_run_failed" && n.notification.subject.id == run_id
            })
            .expect("a failed run must file a durable notification");
        assert!(
            !failed.notification.title.contains(ENGINE_FAILURE_SECRET),
            "the raw engine error must never reach a company-wide notification: {:?}",
            failed.notification.title
        );
        assert!(
            failed.notification.title.contains(RUN_FAILED_DETAIL),
            "the notification must still say the run failed, using fixed text: {:?}",
            failed.notification.title
        );
    }

    fn empty_workflow() -> WorkflowFile {
        WorkflowFile {
            id: "digest".to_string(),
            name: "Digest".to_string(),
            description: None,
            nodes: Vec::new(),
            edges: Vec::new(),
            global: false,
            owner_desk: None,
        }
    }

    /// Issue #1009 (path A): a run whose task panics still journals a finish, so
    /// it stops reading `running: true`.
    ///
    /// The watchdog catches the unwind, writes the finish while the guard is
    /// still held, then re-raises — so both halves hold at once: the JoinHandle
    /// still resolves to a `JoinError` (the console's synchronous-mode 500 and
    /// the scheduler's `tracing::error` are unchanged) AND the journal now
    /// carries a `WorkflowRunFinished` for the run. Before the fix the handle
    /// still errored but nothing was journaled — this asserts the JOURNAL, which
    /// is the half the bug was about.
    #[tokio::test]
    async fn a_panicking_run_still_journals_its_finish() {
        let dir = tempfile::Builder::new()
            .prefix("oc-spawn-panic-")
            .tempdir()
            .expect("tempdir");
        let events: Arc<dyn EventLog> = Arc::new(FsEventLog::new(dir.path()));
        let company = CompanyId::new("acme");
        let notifications = Arc::new(crate::store::FsOps::new(dir.path().to_path_buf()));
        let spawn = WorkflowSpawn {
            company: company.clone(),
            events: events.clone(),
            supervisor: RunSupervisor::new(),
            runner: Arc::new(PanickingRunner),
            runs: Arc::new(crate::store::FsOps::new(dir.path().to_path_buf())),
            notifications: notifications.clone(),
        };

        let (run_id, handle) = spawn
            .spawn(empty_workflow(), Value::Null, false, false)
            .expect("under the default cap");

        let joined = handle.await;
        assert!(
            joined.is_err() && joined.unwrap_err().is_panic(),
            "the panic still propagates to the JoinHandle, preserving the sync-mode 500"
        );

        let stored = events
            .read_from(&company, EventSeq::new(0), usize::MAX)
            .await
            .expect("read journal");
        let finished = stored.iter().any(|s| {
            matches!(
                &s.event,
                CompanyEvent::WorkflowRunFinished {
                    run_id: Some(id),
                    error: Some(err),
                    ..
                } if id == &run_id && err == PANICKED_BEFORE_FINISH
            )
        });
        assert!(
            finished,
            "the watchdog journaled a WorkflowRunFinished for the panicked run"
        );

        // Issue #1865: the panic is exactly the shape `notify_run_unhealthy`
        // exists for — a run that came apart entirely, with nobody watching
        // (a detached/scheduled fire is the case this matters most for).
        use crate::ports::notifications::NotificationStore;
        let notes = notifications
            .list(&company, "anyone")
            .await
            .expect("list notifications");
        assert!(
            notes
                .iter()
                .any(|n| n.notification.kind == "workflow_run_failed"
                    && n.notification.subject.id == run_id),
            "a panicked run must file a durable notification: {notes:?}"
        );
    }

    /// A dry (test) run that panics journals **nothing** — the watchdog honours
    /// the same `dry_run` skip the clean path does, so a test run leaves no
    /// `WorkflowRunFinished` for the history to fold even when it blows up.
    #[tokio::test]
    async fn a_panicking_dry_run_journals_nothing() {
        let dir = tempfile::Builder::new()
            .prefix("oc-spawn-panic-dry-")
            .tempdir()
            .expect("tempdir");
        let events: Arc<dyn EventLog> = Arc::new(FsEventLog::new(dir.path()));
        let company = CompanyId::new("acme");
        let spawn = WorkflowSpawn {
            company: company.clone(),
            events: events.clone(),
            supervisor: RunSupervisor::new(),
            runner: Arc::new(PanickingRunner),
            runs: Arc::new(crate::store::FsOps::new(dir.path().to_path_buf())),
            notifications: Arc::new(crate::store::FsOps::new(dir.path().to_path_buf())),
        };

        let (_run_id, handle) = spawn
            .spawn(empty_workflow(), Value::Null, false, true)
            .expect("under the default cap");
        assert!(handle.await.is_err(), "the panic still propagates");

        let stored = events
            .read_from(&company, EventSeq::new(0), usize::MAX)
            .await
            .expect("read journal");
        assert!(
            !stored
                .iter()
                .any(|s| matches!(s.event, CompanyEvent::WorkflowRunFinished { .. })),
            "a dry run journals no finish, panic or not"
        );
    }

    /// A runner whose one node gated a call that failed to park — nothing is
    /// left waiting on anyone, but `blocked_nodes` is non-empty too (issue
    /// #1865 Codex review): `HarnessAgentRunner` pushes a
    /// `WorkflowBlockedNode` the moment a turn gated anything at all, parked
    /// or not, so a fully-unparkable node reads exactly like a node with a
    /// live card on that field alone.
    struct FullyStrandedRunner;

    #[async_trait]
    impl WorkflowRunner for FullyStrandedRunner {
        async fn run(
            &self,
            _company: &CompanyId,
            _workflow: &WorkflowFile,
            _input: Value,
            _ctx: &WorkflowRunContext,
        ) -> Result<WorkflowRun> {
            Ok(WorkflowRun {
                output: Value::Null,
                pending_approvals: vec!["node1".to_string()],
                deliveries: Vec::new(),
                cancelled: false,
                nodes: Vec::new(),
                notices: Vec::new(),
                board: Vec::new(),
                blocked_nodes: vec![crate::ports::WorkflowBlockedNode {
                    node_id: "node1".to_string(),
                    tools: vec!["some_tool".to_string()],
                    // Empty: every park attempt on this node failed. This is
                    // what makes `blocked_nodes.is_empty()` alone the wrong
                    // test — it is non-empty here exactly as it would be for
                    // a node with a live, decidable card.
                    approval_ids: Vec::new(),
                    unparkable: 1,
                    stranded: 0,
                }],
                approvals: vec![crate::ports::WorkflowRunApprovalRow {
                    node_id: Some("node1".to_string()),
                    tool: Some("some_tool".to_string()),
                    outcome: crate::ports::WorkflowApprovalOutcome::ParkFailed,
                    approval_id: None,
                }],
            })
        }
    }

    /// Issue #1865 (Codex review): a run whose only pending node has zero live
    /// parked calls must file a `workflow_run_stranded` notification, not
    /// `workflow_run_blocked` — nobody is actually waiting on a person to
    /// decide anything, so the "blocked" wording would send an operator
    /// looking for an Approvals card that does not exist.
    ///
    /// Before the fix, the match in `WorkflowSpawn::spawn` tested
    /// `!run.blocked_nodes.is_empty()` before the stranded arm, and that field
    /// is non-empty for this exact case (see `FullyStrandedRunner`), so the
    /// blocked arm always won and the stranded arm below it was unreachable
    /// for a fully-stranded run.
    #[tokio::test]
    async fn a_fully_stranded_run_notifies_stranded_not_blocked() {
        let dir = tempfile::Builder::new()
            .prefix("oc-spawn-stranded-")
            .tempdir()
            .expect("tempdir");
        let events: Arc<dyn EventLog> = Arc::new(FsEventLog::new(dir.path()));
        let company = CompanyId::new("acme");
        let notifications = Arc::new(crate::store::FsOps::new(dir.path().to_path_buf()));
        let spawn = WorkflowSpawn {
            company: company.clone(),
            events: events.clone(),
            supervisor: RunSupervisor::new(),
            runner: Arc::new(FullyStrandedRunner),
            runs: Arc::new(crate::store::FsOps::new(dir.path().to_path_buf())),
            notifications: notifications.clone(),
        };

        let (run_id, handle) = spawn
            .spawn(empty_workflow(), Value::Null, false, false)
            .expect("under the default cap");
        handle.await.expect("join").expect("run settles Ok");

        use crate::ports::notifications::NotificationStore;
        let notes = notifications
            .list(&company, "anyone")
            .await
            .expect("list notifications");
        assert!(
            notes
                .iter()
                .any(|n| n.notification.kind == "workflow_run_stranded"
                    && n.notification.subject.id == run_id),
            "a fully-stranded run must file a stranded notification: {notes:?}"
        );
        assert!(
            !notes
                .iter()
                .any(|n| n.notification.kind == "workflow_run_blocked"),
            "a fully-stranded run must NOT file the misleading 'blocked' \
             notification — nobody is waiting on a person to decide anything: \
             {notes:?}"
        );
    }

    /// A runner with two pending nodes: `node1` failed to park (nothing
    /// waiting on it), `node2` has a live `Parked` card. `stranded_approvals`
    /// over the whole run is `1`, which is `> 0` but not equal to the `2`
    /// pending nodes — the run is only **partly** stranded, and `node2`'s
    /// card is still there for an operator to decide.
    struct PartlyStrandedRunner;

    #[async_trait]
    impl WorkflowRunner for PartlyStrandedRunner {
        async fn run(
            &self,
            _company: &CompanyId,
            _workflow: &WorkflowFile,
            _input: Value,
            _ctx: &WorkflowRunContext,
        ) -> Result<WorkflowRun> {
            Ok(WorkflowRun {
                output: Value::Null,
                pending_approvals: vec!["node1".to_string(), "node2".to_string()],
                deliveries: Vec::new(),
                cancelled: false,
                nodes: Vec::new(),
                notices: Vec::new(),
                board: Vec::new(),
                blocked_nodes: vec![
                    crate::ports::WorkflowBlockedNode {
                        node_id: "node1".to_string(),
                        tools: vec!["some_tool".to_string()],
                        approval_ids: Vec::new(),
                        unparkable: 1,
                        stranded: 0,
                    },
                    crate::ports::WorkflowBlockedNode {
                        node_id: "node2".to_string(),
                        tools: vec!["other_tool".to_string()],
                        approval_ids: vec!["appr-2".to_string()],
                        unparkable: 0,
                        stranded: 0,
                    },
                ],
                approvals: vec![
                    crate::ports::WorkflowRunApprovalRow {
                        node_id: Some("node1".to_string()),
                        tool: Some("some_tool".to_string()),
                        outcome: crate::ports::WorkflowApprovalOutcome::ParkFailed,
                        approval_id: None,
                    },
                    crate::ports::WorkflowRunApprovalRow {
                        node_id: Some("node2".to_string()),
                        tool: Some("other_tool".to_string()),
                        outcome: crate::ports::WorkflowApprovalOutcome::Parked,
                        approval_id: Some("appr-2".to_string()),
                    },
                ],
            })
        }
    }

    /// Codex review on PR #1883 (comment 3874654376): the "stranded" arm must
    /// require the stranded count to equal the *total* pending count, matching
    /// the invariant [`crate::ports::workflow_verdict::RunVerdictFacts::fully_stranded`]
    /// documents for the sync run response ("a run only **partly** stranded
    /// keeps its old verdict: something there really is still decidable").
    /// Before the fix, this arm tested `stranded_approvals(...) > 0`, which is
    /// true here too (`node1` is stranded) even though `node2` still has a
    /// live, actionable card — misclassifying a decidable run as one with
    /// "nobody... asked".
    #[tokio::test]
    async fn a_partly_stranded_run_notifies_blocked_not_stranded() {
        let dir = tempfile::Builder::new()
            .prefix("oc-spawn-partly-stranded-")
            .tempdir()
            .expect("tempdir");
        let events: Arc<dyn EventLog> = Arc::new(FsEventLog::new(dir.path()));
        let company = CompanyId::new("acme");
        let notifications = Arc::new(crate::store::FsOps::new(dir.path().to_path_buf()));
        let spawn = WorkflowSpawn {
            company: company.clone(),
            events: events.clone(),
            supervisor: RunSupervisor::new(),
            runner: Arc::new(PartlyStrandedRunner),
            runs: Arc::new(crate::store::FsOps::new(dir.path().to_path_buf())),
            notifications: notifications.clone(),
        };

        let (run_id, handle) = spawn
            .spawn(empty_workflow(), Value::Null, false, false)
            .expect("under the default cap");
        handle.await.expect("join").expect("run settles Ok");

        use crate::ports::notifications::NotificationStore;
        let notes = notifications
            .list(&company, "anyone")
            .await
            .expect("list notifications");
        assert!(
            notes
                .iter()
                .any(|n| n.notification.kind == "workflow_run_blocked"
                    && n.notification.subject.id == run_id),
            "a partly-stranded run still has a decidable card and must file \
             'blocked', not 'stranded': {notes:?}"
        );
        assert!(
            !notes
                .iter()
                .any(|n| n.notification.kind == "workflow_run_stranded"),
            "a partly-stranded run must NOT be announced as fully stranded — \
             node2's card is still live and actionable: {notes:?}"
        );
    }

    /// Same approvals shape as `FullyStrandedRunner` — `node1` is the run's
    /// only pending node and it lost its card completely — but the run also
    /// carries a `deliveries` row parked for approval on the same run.
    struct StrandedApprovalWithPendingDeliveryRunner;

    #[async_trait]
    impl WorkflowRunner for StrandedApprovalWithPendingDeliveryRunner {
        async fn run(
            &self,
            _company: &CompanyId,
            _workflow: &WorkflowFile,
            _input: Value,
            _ctx: &WorkflowRunContext,
        ) -> Result<WorkflowRun> {
            Ok(WorkflowRun {
                output: Value::Null,
                pending_approvals: vec!["node1".to_string()],
                deliveries: vec![crate::ports::DeliveryReport {
                    node: "output1".to_string(),
                    kind: "email".to_string(),
                    target: None,
                    status: crate::ports::DeliveryStatus::Pending,
                    detail: "parked for operator approval".to_string(),
                    reason: crate::ports::DeliveryReason::default(),
                }],
                cancelled: false,
                nodes: Vec::new(),
                notices: Vec::new(),
                board: Vec::new(),
                blocked_nodes: vec![crate::ports::WorkflowBlockedNode {
                    node_id: "node1".to_string(),
                    tools: vec!["some_tool".to_string()],
                    approval_ids: Vec::new(),
                    unparkable: 1,
                    stranded: 0,
                }],
                approvals: vec![crate::ports::WorkflowRunApprovalRow {
                    node_id: Some("node1".to_string()),
                    tool: Some("some_tool".to_string()),
                    outcome: crate::ports::WorkflowApprovalOutcome::ParkFailed,
                    approval_id: None,
                }],
            })
        }
    }

    /// Codex review on PR #1883 (comment 3875617184): `node1` is fully
    /// stranded on its own — `stranded_approvals(...) == pending_approvals.len()`
    /// holds exactly as it does for `FullyStrandedRunner` — but the run also
    /// has a report parked on the deliveries queue
    /// (`DeliveryStatus::Pending`). `RunVerdictFacts::fully_stranded`
    /// excludes a run with a pending delivery because that report is a
    /// *second* thing still waiting on a person; the notification guard must
    /// apply the same exclusion so it does not claim "nobody was asked" while
    /// exactly that is true of the parked report.
    ///
    /// Before the fix, this arm ignored `run.deliveries` entirely, so it fired
    /// `workflow_run_stranded` here too. `blocked_nodes` is non-empty (same
    /// shape as `FullyStrandedRunner`), so once the stranded arm correctly
    /// declines, the run falls through to the `blocked` arm below it — same
    /// fallback the partly-stranded case uses.
    #[tokio::test]
    async fn a_stranded_run_with_a_pending_delivery_does_not_notify_stranded() {
        let dir = tempfile::Builder::new()
            .prefix("oc-spawn-stranded-pending-delivery-")
            .tempdir()
            .expect("tempdir");
        let events: Arc<dyn EventLog> = Arc::new(FsEventLog::new(dir.path()));
        let company = CompanyId::new("acme");
        let notifications = Arc::new(crate::store::FsOps::new(dir.path().to_path_buf()));
        let spawn = WorkflowSpawn {
            company: company.clone(),
            events: events.clone(),
            supervisor: RunSupervisor::new(),
            runner: Arc::new(StrandedApprovalWithPendingDeliveryRunner),
            runs: Arc::new(crate::store::FsOps::new(dir.path().to_path_buf())),
            notifications: notifications.clone(),
        };

        let (run_id, handle) = spawn
            .spawn(empty_workflow(), Value::Null, false, false)
            .expect("under the default cap");
        handle.await.expect("join").expect("run settles Ok");

        use crate::ports::notifications::NotificationStore;
        let notes = notifications
            .list(&company, "anyone")
            .await
            .expect("list notifications");
        assert!(
            !notes
                .iter()
                .any(|n| n.notification.kind == "workflow_run_stranded"
                    && n.notification.subject.id == run_id),
            "a run with a pending delivery must NOT be announced as fully \
             stranded — the parked report is still actionable, exactly like \
             `RunVerdictFacts::fully_stranded` excludes it: {notes:?}"
        );
    }

    /// A runner that returns a settled, `cancelled: true` run carrying the
    /// exact shape the clean node-boundary cancel arm in
    /// `run_workflow_inner` (`src/workflows/runner.rs`, `if
    /// outcome.cancelled`) leaves behind: `pending_approvals` is zeroed, per
    /// that arm's own doc comment ("a stop still routes nothing and parks no
    /// gate"), but `blocked_nodes` is still `blocks.take()` — whatever the
    /// run had already gated before the operator's stop landed.
    struct CancelledWithBlockedNodesRunner;

    #[async_trait]
    impl WorkflowRunner for CancelledWithBlockedNodesRunner {
        async fn run(
            &self,
            _company: &CompanyId,
            _workflow: &WorkflowFile,
            _input: Value,
            _ctx: &WorkflowRunContext,
        ) -> Result<WorkflowRun> {
            Ok(WorkflowRun {
                output: Value::Null,
                pending_approvals: Vec::new(),
                deliveries: Vec::new(),
                cancelled: true,
                nodes: Vec::new(),
                notices: Vec::new(),
                board: Vec::new(),
                blocked_nodes: vec![crate::ports::WorkflowBlockedNode {
                    node_id: "node1".to_string(),
                    tools: vec!["some_tool".to_string()],
                    approval_ids: vec!["appr-1".to_string()],
                    unparkable: 0,
                    stranded: 0,
                }],
                approvals: Vec::new(),
            })
        }
    }

    /// Issue #1865 (PR #1883 review comment 3878430677): a cancelled run
    /// must NOT file the `workflow_run_blocked` notification even when it
    /// carries a non-empty `blocked_nodes` — the clean node-boundary cancel
    /// arm in `run_workflow_inner` preserves whatever the run had already
    /// gated via `blocked_nodes: blocks.take()`, so this shape is real, not
    /// synthetic. `WorkflowRunVerdict::of` checks `cancelled` before
    /// `blocked_nodes` for the identical reason ("a stop somebody asked for
    /// is not a fault"); before the fix, this notification guard had no such
    /// check, so an operator's own stop reported "this run stopped because a
    /// step is waiting on a person to decide something" — sending them
    /// looking for an Approvals card on a run that will never continue
    /// either way.
    #[tokio::test]
    async fn a_cancelled_run_does_not_notify_blocked() {
        let dir = tempfile::Builder::new()
            .prefix("oc-spawn-cancelled-blocked-")
            .tempdir()
            .expect("tempdir");
        let events: Arc<dyn EventLog> = Arc::new(FsEventLog::new(dir.path()));
        let company = CompanyId::new("acme");
        let notifications = Arc::new(crate::store::FsOps::new(dir.path().to_path_buf()));
        let spawn = WorkflowSpawn {
            company: company.clone(),
            events: events.clone(),
            supervisor: RunSupervisor::new(),
            runner: Arc::new(CancelledWithBlockedNodesRunner),
            runs: Arc::new(crate::store::FsOps::new(dir.path().to_path_buf())),
            notifications: notifications.clone(),
        };

        let (run_id, handle) = spawn
            .spawn(empty_workflow(), Value::Null, false, false)
            .expect("under the default cap");
        handle.await.expect("join").expect("run settles Ok");

        use crate::ports::notifications::NotificationStore;
        let notes = notifications
            .list(&company, "anyone")
            .await
            .expect("list notifications");
        assert!(
            notes.iter().all(|n| n.notification.subject.id != run_id),
            "a cancelled run must file NO unhealthy notification at all — a \
             deliberate stop is not one of the failed/blocked/stranded \
             readings this mechanism exists for: {notes:?}"
        );
    }
}
