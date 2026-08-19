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
use crate::ports::{EventLog, WorkflowRun, WorkflowRunContext, WorkflowRunner};
use crate::runtime::workflow_outcome::record_run_finished;
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
const PANICKED_BEFORE_FINISH: &str = concat!(
    "this run's task panicked before it finished; ",
    "the nodes recorded against it are the ones that completed before it stopped"
);

/// Everything starting a supervised workflow run needs, and nothing else.
#[derive(Clone)]
pub struct WorkflowSpawn {
    company: CompanyId,
    events: Arc<dyn EventLog>,
    supervisor: RunSupervisor,
    runner: Arc<dyn WorkflowRunner>,
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
                            Err(PANICKED_BEFORE_FINISH),
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
            if !dry_run {
                // Issue #228: journaled on BOTH arms. The caller may well have
                // closed the tab; the record is what is still there tomorrow.
                let outcome = match result.as_ref() {
                    Ok(run) => Ok(run),
                    Err(err) => Err(err.to_string()),
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
                    Err(err) => {
                        record_run_finished(
                            &self.events,
                            &self.company,
                            &workflow.id,
                            scheduled,
                            &ctx.run_id,
                            Err(err.as_str()),
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

    fn empty_workflow() -> WorkflowFile {
        WorkflowFile {
            id: "digest".to_string(),
            name: "Digest".to_string(),
            description: None,
            nodes: Vec::new(),
            edges: Vec::new(),
            global: false,
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
        let spawn = WorkflowSpawn {
            company: company.clone(),
            events: events.clone(),
            supervisor: RunSupervisor::new(),
            runner: Arc::new(PanickingRunner),
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
}
