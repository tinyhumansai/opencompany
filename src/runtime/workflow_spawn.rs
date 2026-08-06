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

use serde_json::Value;
use tokio::task::JoinHandle;

use crate::Result;
use crate::company::WorkflowFile;
use crate::company::runtime::CompanyRuntime;
use crate::ports::types::CompanyId;
use crate::ports::{EventLog, WorkflowRun, WorkflowRunner};
use crate::runtime::RunSupervisor;
use crate::runtime::workflow_outcome::record_run_finished;

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
    pub fn spawn(
        self,
        workflow: WorkflowFile,
        input: Value,
        scheduled: bool,
    ) -> (String, JoinHandle<Result<WorkflowRun>>) {
        // Issue #371 mints the id above the runner so the error arm can still
        // correlate; issue #383 mints it HERE, through the supervisor, so the
        // same id is also an address an operator can send "stop" to.
        // Deliberately not a second identifier — the run id the console already
        // correlates SSE frames on IS the cancellation handle.
        let (ctx, guard) = self.supervisor.begin(&workflow.id, scheduled);
        let run_id = ctx.run_id.clone();
        let handle = tokio::spawn(async move {
            // Held for the whole run INCLUDING the journal write below, so the
            // window in which a cancel is accepted matches the window in which
            // it can still do anything. Dropping on every exit path, unwind
            // included, is why this is a guard rather than a call at the end.
            let _guard = guard;
            let result = self.runner.run(&self.company, &workflow, input, &ctx).await;
            // Issue #228: journaled on BOTH arms. The caller may well have
            // closed the tab; the record is what is still there tomorrow.
            let outcome = match result.as_ref() {
                Ok(run) => Ok(run),
                Err(err) => Err(err.to_string()),
            };
            match outcome {
                Ok(run) => {
                    record_run_finished(
                        &self.events,
                        &self.company,
                        &workflow.id,
                        scheduled,
                        &ctx.run_id,
                        Ok(run),
                    )
                    .await;
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
                    .await;
                }
            }
            result
        });
        (run_id, handle)
    }
}
