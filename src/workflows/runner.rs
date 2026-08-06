//! Compile and drive a company workflow on the tinyflows engine.
//!
//! [`run_workflow`] is the free driver: [`translate`](super::translate) the
//! [`WorkflowFile`] into a tinyflows graph, [`compile`](tinyflows::compiler)
//! it, build the [`Capabilities`](super::caps) bundle (agent nodes → harness
//! pool), and [`run`](tinyflows::engine) it to completion. [`HarnessWorkflowRunner`]
//! is the [`WorkflowRunner`] port implementation the runtime holds: it owns the
//! shared pool/deps/record, ensures the roster is resident, then delegates.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::Result;
use crate::company::WorkflowFile;
use crate::error::OpenCompanyError;
use crate::harness::{HarnessDeps, HarnessPool};
use crate::ports::types::{CompanyEvent, CompanyId, CompanyRecord, WorkflowNodeStatus};
use crate::ports::{WorkflowRun, WorkflowRunContext, WorkflowRunner};

/// How deeply a workflow may re-enter itself before the run is refused
/// (issue #151 part a).
///
/// One level of nesting is legitimate and useful — a `sub_workflow` node, or a
/// workflow whose agent node asks the orchestrator to run a second, different
/// graph. Beyond that a chain is almost certainly a cycle rather than a plan,
/// and the cost of being wrong is asymmetric: refusing a deep run returns a
/// readable tool error, while allowing it aborts the host.
pub(crate) const MAX_WORKFLOW_DEPTH: usize = 4;

/// How long a settling run waits for its node-progress events to finish
/// reaching the journal (issue #371).
///
/// The drain is normally instant — the channel is already closed and only
/// in-flight appends remain — so this bound never fires in practice. It exists
/// to keep a progress-reporting stall from ever becoming a *run* stall.
const PROGRESS_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

tokio::task_local! {
    /// How many workflow runs are already on this call chain.
    ///
    /// A task-local, not a counter on the runner, and that distinction is the
    /// point: a workflow run, the agent turns inside it, and any tool those
    /// turns call all execute inline on **one** tokio task (the only `spawn` on
    /// the path is the progress-event collector, which runs nothing re-entrant).
    /// So this counts exactly one causal chain. A shared counter would instead
    /// count *concurrent* runs and refuse two operators running unrelated
    /// workflows at the same time.
    static WORKFLOW_DEPTH: usize;
}

/// The current re-entry depth, `0` outside any workflow run.
fn current_workflow_depth() -> usize {
    WORKFLOW_DEPTH.try_with(|d| *d).unwrap_or(0)
}

/// Runs `workflow` for the company described by `record` on the tinyflows engine
/// with the trigger `input`, returning the final run state and any nodes left
/// pending approval.
///
/// `record` (not a bare [`CompanyId`]) is threaded through so the outside-world
/// capabilities — the `tool_call` toolbelt and the `http_request` SSRF guard —
/// can read the company's `[policy].mode`, `[tools].allow` grants, and
/// `[tools].web_allowed_domains` (see [`super::caps::build_capabilities`]).
///
/// The caller is responsible for having the company's roster resident in `pool`
/// (agent nodes address it by teammate id) — [`HarnessWorkflowRunner::run`] does
/// this via [`HarnessPool::ensure`] before delegating here.
pub async fn run_workflow(
    pool: Arc<HarnessPool>,
    deps: HarnessDeps,
    record: &CompanyRecord,
    workflow: &WorkflowFile,
    input: Value,
    ctx: &WorkflowRunContext,
) -> Result<WorkflowRun> {
    // Issue #151 part a: refuse an unbounded re-entry before it takes the host
    // down. `run_workflow` is an orchestrator tool, and a workflow `agent` node
    // may address the orchestrator — so a graph whose agent node runs a
    // workflow that reaches the orchestrator again recurses with no bound. Each
    // level is a whole agent turn plus an engine run, so the process dies on a
    // stack overflow rather than returning an error, taking every other tenant
    // on the host with it. `MAX_DELEGATIONS_PER_TURN` caps fan-out *within* one
    // turn and does nothing about depth.
    let depth = current_workflow_depth();
    if depth >= MAX_WORKFLOW_DEPTH {
        tracing::warn!(
            company = %record.id,
            workflow = %workflow.id,
            depth,
            "workflow: refusing a run past the re-entry limit"
        );
        return Err(OpenCompanyError::Harness(format!(
            "workflow `{}` was not run: it is already {depth} workflow runs deep, at the \
             re-entry limit of {}. A workflow whose agent node runs another workflow that \
             reaches back here will loop forever — break the cycle, or run the inner \
             workflow on its own.",
            workflow.id, MAX_WORKFLOW_DEPTH
        )));
    }

    WORKFLOW_DEPTH
        .scope(
            depth + 1,
            run_workflow_inner(pool, deps, record, workflow, input, ctx),
        )
        .await
}

/// One node's finish, as the engine's observer callback hands it over to the
/// async collector (issue #371).
///
/// Deliberately three scalars and no `ExecutionStep`: the engine's step carries
/// the node's `output` items, and this hop exists partly to make it *impossible*
/// for that payload to reach the journal — the same stance the live turn frames
/// take on tool args. What is not carried cannot leak.
struct NodeProgress {
    node_id: String,
    status: WorkflowNodeStatus,
    elapsed_ms: u64,
}

/// A [`RunObserver`](tinyflows::observability::RunObserver) that forwards each
/// node finish onto an unbounded channel.
///
/// The channel is the whole reason this type exists. Observer callbacks are
/// **synchronous** (the engine invokes them inline, across threads) while
/// [`EventLog::append`] is async, so the callback cannot journal directly. It
/// also must not block: a node handler stalled on a disk write would make
/// observability change the run's timing, which is exactly what an observer is
/// not allowed to do. Unbounded is safe at this volume — one message per
/// non-trigger node, ~8 for a six-node graph — and it means a slow journal can
/// never apply backpressure to the engine.
struct ProgressObserver {
    tx: tokio::sync::mpsc::UnboundedSender<NodeProgress>,
}

impl tinyflows::observability::RunObserver for ProgressObserver {
    fn on_step_finish(&self, step: &tinyflows::observability::ExecutionStep) {
        // A closed receiver means the collector already stopped (the run is
        // settling, or `deps.events` was never wired). Dropping the frame is
        // correct: progress reporting must never disturb the run.
        let _ = self.tx.send(NodeProgress {
            node_id: step.node_id.clone(),
            status: match step.status {
                tinyflows::observability::StepStatus::Success => WorkflowNodeStatus::Ok,
                tinyflows::observability::StepStatus::Error => WorkflowNodeStatus::Error,
            },
            // `u128` millis is the engine's type; a node running longer than
            // 584 million years is not the failure mode worth a `Result`.
            elapsed_ms: u64::try_from(step.duration_ms).unwrap_or(u64::MAX),
        });
    }
}

/// The run itself, always executed inside a [`WORKFLOW_DEPTH`] scope so a
/// nested run sees this one on the chain.
async fn run_workflow_inner(
    pool: Arc<HarnessPool>,
    deps: HarnessDeps,
    record: &CompanyRecord,
    workflow: &WorkflowFile,
    input: Value,
    ctx: &WorkflowRunContext,
) -> Result<WorkflowRun> {
    let graph = super::translate::translate(workflow);
    let compiled = tinyflows::compiler::compile(&graph).map_err(map_engine_error)?;
    // Issue #371: the caller's run id, not a freshly minted one. Correlating the
    // run's progress events with the `WorkflowRunFinished` the caller journals
    // requires both halves to share an id, and only the caller can supply one
    // that survives the error arm (where this function returns nothing at all).
    // A side win: the run's `_workflow/` workspace directory, which is named
    // from this id, becomes correlatable with the journal for the first time.
    let run_id = ctx.run_id.clone();
    // Issue #154: the operator's run request rides the trigger payload. Pull it
    // out before the input is handed to the engine so every agent node's turn
    // message carries the topic — a node's authored `prompt` is the same on
    // every run and cannot say what was asked this time.
    let run_request = super::caps::run_request_text(&input);
    // Issue #395: the trigger payload, kept before the engine consumes it. A
    // paused gate's approval card has to carry the input the run was started
    // with, because resuming means re-running the graph with that same input
    // plus the approval — see `crate::runtime::workflow_resume`.
    let trigger_input = input.clone();
    // Issue #170: the delivery ports are read off `deps` BEFORE it moves into
    // the capability bundle. Delivery is host-side and post-engine, so it is not
    // a capability — the engine never learns a report has a destination.
    let delivery = deps.delivery.clone();
    // Issue #371: likewise read the journal off `deps` before it moves. `None`
    // (the default build, and every existing test) degrades the whole progress
    // path to a no-op — no started event, a `NoopObserver`, no collector.
    let events = deps.events.clone();
    let capabilities =
        super::caps::build_capabilities(pool, deps, record, &workflow.id, &run_id, run_request)
            .await;

    // The opening bracket, appended BEFORE the engine call so a run killed
    // mid-flight leaves a start with no finish — which is precisely the shape
    // the boot sweep looks for. Best-effort, like every other journal write on
    // this path: losing the record is worth a log line, never a failed run.
    if let Some(events) = events.as_ref() {
        let started = CompanyEvent::WorkflowRunStarted {
            workflow_id: workflow.id.clone(),
            run_id: run_id.clone(),
            scheduled: ctx.scheduled,
        };
        if let Err(err) = events.append(&record.id, started).await {
            tracing::warn!(
                company = %record.id,
                workflow = %workflow.id,
                %run_id,
                %err,
                "workflow: run-started progress event could not be journaled; the run is unaffected"
            );
        }
    }

    // Drive the engine with an observer when there is somewhere to put the
    // frames, and exactly as before when there is not.
    let outcome = match events.as_ref() {
        None => {
            // Issue #383: even with nowhere to report progress, a run stays
            // stoppable. `Box::pin` rather than `tokio::pin!` because the losing
            // branch has to be **dropped**, and a `tokio::pin!`ed local lives to
            // the end of its scope.
            let mut engine = Box::pin(tinyflows::engine::run(&compiled, input, &capabilities));
            tokio::select! {
                biased;
                () = ctx.cancel.cancelled() => {
                    drop(engine);
                    return Ok(cancelled_run());
                }
                outcome = &mut engine => outcome.map_err(map_engine_error)?,
            }
        }
        Some(events) => {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<NodeProgress>();
            let collector = tokio::spawn({
                let events = events.clone();
                let company = record.id.clone();
                let workflow_id = workflow.id.clone();
                let run_id = run_id.clone();
                async move {
                    while let Some(progress) = rx.recv().await {
                        let event = CompanyEvent::WorkflowNodeFinished {
                            workflow_id: workflow_id.clone(),
                            run_id: run_id.clone(),
                            node_id: progress.node_id,
                            status: progress.status,
                            elapsed_ms: progress.elapsed_ms,
                        };
                        if let Err(err) = events.append(&company, event).await {
                            tracing::warn!(
                                %company,
                                workflow = %workflow_id,
                                %run_id,
                                %err,
                                "workflow: node progress event could not be journaled; the run is \
                                 unaffected"
                            );
                        }
                    }
                }
            });

            let observer: Arc<dyn tinyflows::observability::RunObserver> =
                Arc::new(ProgressObserver { tx });
            // Issue #383: the engine call is raced against the run's stop signal
            // instead of awaited outright.
            //
            // # Why a host-side select rather than the engine's own token
            //
            // tinyflows has a `CancellationToken`, and no public entry point
            // takes one **with** an observer: `run_cancellable` hardcodes a
            // `NoopObserver`, `run_with_observer` hardcodes a fresh token, and
            // `build_and_run` — which takes both — is private. Using the token
            // would therefore cost every cancellable run the per-node progress
            // issue #371 just added, and fixing that properly means a change two
            // submodules deep. Filed upstream instead; see `RunCancel`.
            //
            // # What this costs, stated honestly
            //
            // Dropping the future stops the run **mid-await** — it does not let
            // the executing node finish. A node part way through an external
            // side effect stays part way through it. That is the same class of
            // outcome as the host being killed, which the boot sweep already
            // settles; this is just operator-initiated and so more frequent.
            // `Box::pin` because the losing branch must be droppable, which a
            // `tokio::pin!`ed local is not.
            let mut engine = Box::pin(tinyflows::engine::run_with_observer(
                &compiled,
                input,
                &capabilities,
                &observer,
            ));
            let outcome = tokio::select! {
                biased;
                () = ctx.cancel.cancelled() => None,
                outcome = &mut engine => Some(outcome),
            };
            // **Drop the engine future before the observer, on both arms.** The
            // engine holds observer `Arc` clones inside its per-node handlers;
            // on the completion arm they are already gone (the graph died inside
            // the call), but on the cancel arm the future still owns them, so
            // dropping `observer` alone would NOT close the channel — the
            // collector below would then block until the drain timeout on every
            // single cancel, and the timeout would hide it.
            //
            // Today the **borrow checker enforces this ordering**: the engine
            // future borrows `observer`, so removing this line does not compile.
            // That is a happy accident of the current signature taking `&Arc`,
            // not a guarantee — an engine that took the `Arc` by value would
            // close the compile-time hole and re-open the runtime one, silently.
            // `a_cancelled_run_settles_fast_keeping_only_its_completed_nodes`
            // asserts a cancel-latency bound for exactly that case; it was
            // verified to fail (at 10.004s, the full drain timeout) against a
            // deliberately leaked observer clone.
            drop(engine);

            // Drop the last sender, then join — in that order, and before
            // anything else. The drop closes the channel so the collector's
            // `recv()` returns `None` and its loop ends; the join then waits for
            // every already-sent frame to reach the journal. This is what makes
            // the ordering guarantee true: every `WorkflowNodeFinished` is
            // durably appended before the caller's `WorkflowRunFinished`, so a
            // reader folding the journal never sees a run settle before its
            // nodes land. Without the join the two would race and a fold could
            // attach a node to the run *after* it had been rendered as finished.
            //
            // The drop closes the channel only if ours is the **last** `Arc` —
            // true today, because the engine's per-node handler clones die with
            // the graph inside `run_with_observer`. The bounded wait is there so
            // that stays a *performance* assumption rather than a liveness one:
            // if a future engine ever parked a clone somewhere longer-lived,
            // this would log and move on instead of wedging the run (and the
            // whole host process behind it) forever on a join that never
            // returns. Frames already sent still reach the journal either way —
            // they would just land after the finished event.
            drop(observer);
            match tokio::time::timeout(PROGRESS_DRAIN_TIMEOUT, collector).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => tracing::warn!(
                    company = %record.id,
                    workflow = %workflow.id,
                    %run_id,
                    %err,
                    "workflow: the node-progress collector did not shut down cleanly"
                ),
                Err(_) => tracing::warn!(
                    company = %record.id,
                    workflow = %workflow.id,
                    %run_id,
                    "workflow: node progress events did not drain in time; the run's finished \
                     record may be journaled ahead of them"
                ),
            }

            // Returned only AFTER the drain above, so a cancelled run's
            // completed nodes are journaled exactly like a completed run's —
            // the trail is the whole answer to "how far did it get before I
            // stopped it?", and it must be durable before the caller writes the
            // finish.
            match outcome {
                Some(outcome) => outcome.map_err(map_engine_error)?,
                None => return Ok(cancelled_run()),
            }
        }
    };

    // Route every reached `output` node's report to its configured destination.
    // Deliberately here rather than in the HTTP handler: the orchestrator's
    // `run_workflow` tool and the trigger scheduler drive this same path, and a
    // scheduled run is exactly the case where nobody is watching the console's
    // run-result drawer. Never fails the run — each attempt is reported instead.
    //
    // Issue #438: `already_delivered` is what a continuation must NOT send
    // again. It is read off the trigger input, where the approval that started
    // this run threaded it — an empty list on a run nobody resumed, so a
    // first-run delivery is untouched.
    let already_delivered = crate::runtime::workflow_resume::delivered_in_input(&trigger_input);
    let deliveries = super::delivery::deliver_outputs(
        delivery.as_ref(),
        record,
        workflow,
        &outcome.output,
        &already_delivered,
    )
    .await;

    // Issue #395: turn every gate the engine paused on into a decidable
    // approval. Deliberately here, beside the delivery call and for the same
    // reason: all three entry points — the console route, the cron scheduler,
    // and the orchestrator's `run_workflow` tool — come through this function,
    // and a scheduled run is exactly the case where nobody is watching the
    // response that used to be the only place these ids appeared.
    //
    // **After delivery, and that ordering is load-bearing** (issue #438). The
    // parked card carries the ledger of what this run delivered, so it can only
    // be built once delivery has happened. Nothing is lost by the swap: the two
    // steps are independent — parking reads `pending_approvals` and delivery
    // reads reached `output` nodes, and a node past a gate was never reached, so
    // no delivery could ever depend on a gate having been parked first.
    //
    // Skipped for a cancelled run, which returns above: an operator who stopped
    // a run is not asking to be asked about the gates it never reached.
    park_pending_gates(
        delivery.as_ref(),
        record,
        &workflow.id,
        &run_id,
        &trigger_input,
        &outcome.pending_approvals,
        &deliveries,
    )
    .await;

    Ok(WorkflowRun {
        output: outcome.output,
        pending_approvals: outcome.pending_approvals,
        deliveries,
        cancelled: false,
    })
}

/// Parks one approval card per gate the run paused on (issue #395).
///
/// # The hole this closes
///
/// A node marked `requires_approval` pauses the run, and the engine reports the
/// gate's node id on `RunOutcome::pending_approvals`. Those ids flowed into
/// exactly two places — the run route's HTTP response and the
/// `WorkflowRunFinished` journal line — and **neither is an approval**. The
/// Approvals page reads the journal's parked [`Effect`](crate::ports::types::Effect)s,
/// so it was empty by construction: the run paused, the ids were recorded as
/// trivia, and nothing an operator could act on ever existed. That is why a QA
/// run with a `requires_approval` node left the page reading "All clear".
///
/// # Best-effort, and never fails the run
///
/// The engine has already settled by the time this runs; the graph's work is
/// done and correct whatever happens here. A park that fails is logged loudly —
/// it is the only trace of a decision the operator will never be asked for —
/// and the next gate is still attempted. Failing the run instead would discard
/// a completed run's output over an approvals-queue write.
///
/// # Dedupe
///
/// A run is re-runnable, and resuming one is itself a re-run, so the same gate
/// on the same input will be reached again and again. Without a dedupe the
/// queue fills with identical cards for one decision — which is exactly how an
/// operator learns to rubber-stamp the queue. Identity is the gate, not the run;
/// see [`already_parked`](crate::runtime::workflow_resume::already_parked).
///
/// # The delivery ledger (issue #438)
///
/// `deliveries` is what this run actually routed, and it rides the card so the
/// continuation an approval starts knows what has already left the process.
/// Without it, approving a gate re-mails every report upstream of it — the
/// re-run semantics above applied to a side effect that reaches a real person.
async fn park_pending_gates(
    delivery: Option<&super::delivery::WorkflowDeliveryDeps>,
    record: &CompanyRecord,
    workflow_id: &str,
    run_id: &str,
    trigger_input: &Value,
    pending: &[String],
    deliveries: &[crate::ports::DeliveryReport],
) {
    if pending.is_empty() {
        return;
    }
    let Some(parking) = delivery.and_then(|delivery| delivery.parking.as_ref()) else {
        // Fails closed and loud, the same stance `deliver_outputs` takes for an
        // unwired destination: the run genuinely paused, and on this build
        // nobody can be asked to un-pause it.
        tracing::error!(
            company = %record.id,
            workflow = %workflow_id,
            %run_id,
            gates = pending.len(),
            "workflow: the run paused for approval but this runtime has no approvals queue \
             wired, so the gates cannot be parked — the run cannot be continued"
        );
        return;
    };

    for node_id in pending {
        let effect = crate::runtime::workflow_resume::gate_effect(
            workflow_id,
            node_id,
            trigger_input,
            run_id,
            deliveries,
        );
        if crate::runtime::workflow_resume::already_parked(&parking.journal, &effect) {
            tracing::debug!(
                company = %record.id,
                workflow = %workflow_id,
                node = %node_id,
                "workflow: this gate is already waiting on the operator; not asking twice"
            );
            continue;
        }
        // A workflow run has no board card behind it and no conversation to
        // raise the request in — the same two facts the delivery park records
        // (#333, #379).
        match parking
            .park_and_journal(
                &record.id,
                effect,
                crate::runtime::journal::TaskLink::Unlinked,
                None,
            )
            .await
        {
            Ok(approval_id) => tracing::info!(
                company = %record.id,
                workflow = %workflow_id,
                node = %node_id,
                %run_id,
                %approval_id,
                "workflow: parked a paused gate for operator approval; approving it starts a \
                 continuation run"
            ),
            Err(err) => tracing::error!(
                company = %record.id,
                workflow = %workflow_id,
                node = %node_id,
                %run_id,
                %err,
                "workflow: a paused gate could NOT be parked for approval; the run cannot be \
                 continued"
            ),
        }
    }
}

/// What a run stopped by an operator settles with (issue #383).
///
/// Empty on every field but the flag, and each emptiness is a claim rather than
/// a shrug:
///
/// * **no `output`** — the engine future was dropped, so there is no final state
///   to report. A partial one would be a new shape nothing downstream parses;
/// * **no `deliveries`** — `deliver_outputs` is deliberately not called. A
///   cancelled run must not email anybody a report of work it did not finish,
///   and an absent row already means "not reached" everywhere else;
/// * **no `pending_approvals`** — approvals earlier nodes already parked are
///   journal-backed and independent of the run, so they stay in the queue and
///   an operator may still approve or deny them. Listing them here would imply
///   this run is still waiting on them, which it is not.
fn cancelled_run() -> WorkflowRun {
    WorkflowRun {
        output: Value::Null,
        pending_approvals: Vec::new(),
        deliveries: Vec::new(),
        cancelled: true,
    }
}

/// Maps a tinyflows [`EngineError`](tinyflows::error::EngineError) onto the crate
/// error: a structural validation failure is a caller-facing bad request; every
/// other engine/capability failure is a harness error.
fn map_engine_error(err: tinyflows::error::EngineError) -> OpenCompanyError {
    use tinyflows::error::EngineError;
    match err {
        EngineError::Validation(v) => {
            OpenCompanyError::InvalidRequest(format!("workflow graph is invalid: {v}"))
        }
        other => OpenCompanyError::Harness(other.to_string()),
    }
}

/// The [`WorkflowRunner`] port backed by the embedded harness: it holds the
/// shared pool, its deps, and the company record so it can ensure the roster is
/// built before a run and route agent nodes onto it.
pub struct HarnessWorkflowRunner {
    pool: Arc<HarnessPool>,
    deps: HarnessDeps,
    record: CompanyRecord,
}

impl HarnessWorkflowRunner {
    /// Builds a runner sharing `pool`/`deps` with the rest of the harness surface
    /// for the company described by `record`.
    pub fn new(pool: Arc<HarnessPool>, deps: HarnessDeps, record: CompanyRecord) -> Self {
        Self { pool, deps, record }
    }
}

#[async_trait]
impl WorkflowRunner for HarnessWorkflowRunner {
    async fn run(
        &self,
        _company: &CompanyId,
        workflow: &WorkflowFile,
        input: Value,
        ctx: &WorkflowRunContext,
    ) -> Result<WorkflowRun> {
        // Idempotent: builds the roster on first use, a no-op after. The run
        // addresses the record's own company; `_company` is the routed scope,
        // which the runtime resolves to this same record.
        self.pool.ensure(&self.record, &self.deps).await?;
        run_workflow(
            self.pool.clone(),
            self.deps.clone(),
            &self.record,
            workflow,
            input,
            ctx,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::company::parse_workflow;
    use crate::harness::provider::MockProvider;
    use crate::store::{FsCompanyStore, FsContextStore, FsOps};

    fn record() -> CompanyRecord {
        let manifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "ceo"
role = "Chief Executive"
description = "Runs Acme."
"#,
        )
        .expect("valid manifest");
        CompanyRecord {
            id: CompanyId::new("acme"),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            template_provenance: None,
        }
    }

    fn deps(dir: &std::path::Path) -> HarnessDeps {
        HarnessDeps {
            run_supervisor: crate::runtime::RunSupervisor::default(),
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            context: Arc::new(FsContextStore::new(dir)),
            store: Arc::new(FsCompanyStore::new(dir)),
            meter: Some(Arc::new(FsOps::new(dir))),
            workspace_root: dir.to_path_buf(),
            model_override: None,
            tasks: None,
            artifacts: None,
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: crate::harness::orchestrator::DelegationQueue::default(),
            workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            approval_requests: crate::harness::policy::ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            steer: crate::company::steer::InflightRegistry::default(),
            delivery: None,
            search: None,
            workspace: None,
        }
    }

    /// Deps with a `workflow_source_dir` wired, so `sub_workflow`-by-id resolves
    /// children from `source`'s `workflows/` directory.
    fn deps_with_source(dir: &std::path::Path, source: &std::path::Path) -> HarnessDeps {
        let mut deps = deps(dir);
        deps.workflow_source_dir = Some(source.to_path_buf());
        deps
    }

    /// Writes `src` to `<source>/workflows/<id>.toml`.
    fn write_wf(source: &std::path::Path, id: &str, src: &str) {
        let workflows = source.join("workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        std::fs::write(workflows.join(format!("{id}.toml")), src).unwrap();
    }

    /// A record whose `[tools].allow` grants every namespace, so the workflow
    /// `tool_call` capability can reach the Cell A toolbelt (policy `full` keeps
    /// the exec autonomy at Full so the tools can act).
    fn tools_record() -> CompanyRecord {
        let manifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[tools]
allow = ["*"]
"#,
        )
        .expect("valid manifest");
        CompanyRecord {
            id: CompanyId::new("acme"),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            template_provenance: None,
        }
    }

    /// The workflow workspace directory the tool_call toolbelt is sandboxed to.
    fn workflow_workspace(home: &std::path::Path, company: &str) -> std::path::PathBuf {
        let workflows = home.join(company).join("_workflow");
        let workflow = std::fs::read_dir(workflows)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let run = std::fs::read_dir(workflow)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        run.join("workspace")
    }

    /// A three-node workflow (trigger → agent → output) runs to completion with
    /// the agent node executing on the harness pool: the offline mock provider
    /// echoes the node's prompt, proving the turn went through the openhuman
    /// agent rather than being skipped.
    const GREET: &str = r#"
id = "greet"
name = "Greet"

[[node]]
id = "start"
kind = "trigger"
name = "Start"

[[node]]
id = "ceo"
kind = "agent"
name = "CEO"
summary = "say hello-marker"
agent = "ceo"

[[node]]
id = "done"
kind = "output"
name = "Report back"

[[edge]]
from = "start"
to = "ceo"

[[edge]]
from = "ceo"
to = "done"
"#;

    #[tokio::test]
    async fn agent_node_runs_on_the_harness_pool() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Arc::new(HarnessPool::new());
        let rec = record();
        let deps = deps(dir.path());
        pool.ensure(&rec, &deps).await.expect("roster builds");

        let file = parse_workflow(GREET).expect("workflow parses");
        let run = run_workflow(
            pool,
            deps,
            &rec,
            &file,
            serde_json::json!({ "brief": "launch" }),
            &WorkflowRunContext::new(false),
        )
        .await
        .expect("workflow runs");

        assert!(run.pending_approvals.is_empty());
        // The mock provider echoes the agent node's prompt into its reply, and
        // the reply flows into the run state — proof the agent node executed on
        // the pool through the engine.
        let output = run.output.to_string();
        assert!(output.contains("hello-marker"), "{output}");
    }

    // --- Output destinations, end to end (issue #170) ------------------------

    /// A graph whose terminal `output` node routes its report to the operator
    /// channel. `trigger → output` only, so it needs no roster.
    const REPORT_TO_OPERATOR: &str = r#"
id = "report"
name = "Report"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "done"
kind = "output"
name = "Owner summary"
[node.destination]
kind = "channel"
target = "operator"
[[edge]]
from = "start"
to = "done"
"#;

    /// The end-to-end proof that the RUNNER (not the HTTP handler) delivers: a
    /// run driven straight through `run_workflow` with a wired delivery bundle
    /// posts the report and reports the send on the run result. The
    /// orchestrator's `run_workflow` tool and the trigger scheduler reach this
    /// same function, which is why delivery lives here.
    #[tokio::test]
    async fn a_run_delivers_its_output_report_through_the_runner() {
        use crate::runtime::channel::OperatorChannel;

        let dir = tempfile::tempdir().unwrap();
        let channel = OperatorChannel::new();
        let mut deps = deps(dir.path());
        deps.delivery = Some(crate::workflows::WorkflowDeliveryDeps {
            mail: None,
            inbox: Arc::new(crate::store::FsInboxStore::new(dir.path())),
            users: Arc::new(FsOps::new(dir.path())),
            channels: vec![Arc::new(channel.clone())],
            // This case delivers to a channel, which never parks.
            parking: None,
        });

        let file = parse_workflow(REPORT_TO_OPERATOR).expect("parses");
        let run = run_workflow(
            Arc::new(HarnessPool::new()),
            deps,
            &record(),
            &file,
            serde_json::json!({ "brief": "quarterly numbers" }),
            &WorkflowRunContext::new(false),
        )
        .await
        .expect("workflow runs");

        assert_eq!(run.deliveries.len(), 1, "{:?}", run.deliveries);
        assert_eq!(
            run.deliveries[0].status,
            crate::ports::DeliveryStatus::Sent,
            "{:?}",
            run.deliveries
        );
        assert_eq!(run.deliveries[0].node, "done");
        assert_eq!(
            channel.sent().len(),
            1,
            "the report should have been posted"
        );
    }

    /// The #169 lesson, at the run level: with no delivery ports wired the run
    /// still SUCCEEDS (its work is valid) but the result carries a loud `failed`
    /// row — an operator can tell a working destination from a broken one
    /// without reading a log. Every other `deps()` in this suite is unwired, so
    /// this is the default-build shape.
    #[tokio::test]
    async fn an_unwired_runtime_still_runs_but_says_the_report_was_not_sent() {
        let dir = tempfile::tempdir().unwrap();
        let file = parse_workflow(REPORT_TO_OPERATOR).expect("parses");
        let run = run_workflow(
            Arc::new(HarnessPool::new()),
            deps(dir.path()),
            &record(),
            &file,
            serde_json::json!({}),
            &WorkflowRunContext::new(false),
        )
        .await
        .expect("an undeliverable report must not fail the run");

        assert_eq!(run.deliveries.len(), 1, "{:?}", run.deliveries);
        assert_eq!(
            run.deliveries[0].status,
            crate::ports::DeliveryStatus::Failed
        );
        assert!(
            run.deliveries[0].detail.contains("not wired"),
            "{:?}",
            run.deliveries
        );
    }

    /// The port implementation ensures the roster itself, so a caller need not
    /// pre-`ensure`.
    #[tokio::test]
    async fn port_impl_ensures_roster_and_runs() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Arc::new(HarnessPool::new());
        let rec = record();
        let runner = HarnessWorkflowRunner::new(pool, deps(dir.path()), rec.clone());

        let file = parse_workflow(GREET).expect("workflow parses");
        let run = WorkflowRunner::run(
            &runner,
            &rec.id,
            &file,
            serde_json::json!({}),
            &WorkflowRunContext::new(false),
        )
        .await
        .expect("workflow runs");
        assert!(run.output.to_string().contains("hello-marker"));
    }

    /// A workflow with no trigger is a caller-facing bad request, not a harness
    /// error. (Built by hand — `parse_workflow` would reject it earlier.)
    #[tokio::test]
    async fn missing_trigger_is_invalid_request() {
        use crate::company::{WorkflowFile, WorkflowNodeDef, WorkflowNodeKind};

        let dir = tempfile::tempdir().unwrap();
        let file = WorkflowFile {
            id: "bad".to_string(),
            name: "Bad".to_string(),
            description: None,
            nodes: vec![WorkflowNodeDef {
                id: "only".to_string(),
                kind: WorkflowNodeKind::Output,
                name: "Only".to_string(),
                summary: None,
                agent: None,
                schedule: None,
                config: None,
                on_error: None,
                retry: None,
                requires_approval: None,
                destination: None,
            }],
            edges: Vec::new(),
        };
        let err = run_workflow(
            Arc::new(HarnessPool::new()),
            deps(dir.path()),
            &record(),
            &file,
            serde_json::json!({}),
            &WorkflowRunContext::new(false),
        )
        .await
        .expect_err("missing trigger rejected");
        assert!(
            matches!(err, OpenCompanyError::InvalidRequest(_)),
            "{err:?}"
        );
    }

    // --- P1: real capability wiring (T1–T5) --------------------------------

    /// T1 — a config-driven `tool_call` (slug `csv_export`) executes through the
    /// real Cell A toolbelt and the CSV lands on disk in the dedicated workflow
    /// workspace (on-disk proof the tool actually ran).
    #[tokio::test]
    async fn t1_config_driven_tool_call_writes_csv_to_workflow_workspace() {
        let src = r#"
id = "csv"
name = "CSV"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "export"
kind = "tool_call"
name = "Export"
[node.config]
slug = "csv_export"
[node.config.args]
filename = "wf-out.csv"
data = "[{\"name\":\"Ada\"},{\"name\":\"Bob\"}]"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "export"
[[edge]]
from = "export"
to = "done"
"#;
        let dir = tempfile::tempdir().unwrap();
        let file = parse_workflow(src).expect("parses");
        let run = run_workflow(
            Arc::new(HarnessPool::new()),
            deps(dir.path()),
            &tools_record(),
            &file,
            serde_json::json!({ "seed": 1 }),
            &WorkflowRunContext::new(false),
        )
        .await
        .expect("workflow runs");
        assert!(run.pending_approvals.is_empty());

        let csv = workflow_workspace(dir.path(), "acme")
            .join("exports")
            .join("wf-out.csv");
        assert!(
            csv.is_file(),
            "csv_export should land the file in the workflow workspace: {}",
            csv.display()
        );
        let content = std::fs::read_to_string(&csv).unwrap();
        assert!(
            content.contains("Ada") && content.contains("Bob"),
            "{content}"
        );
    }

    /// T2 — an unknown slug with `retry.max_attempts = 2` and `on_error =
    /// "continue"` exhausts its retries then turns the failure into a data item,
    /// so the run completes (no hard error) carrying the error.
    #[tokio::test]
    async fn t2_unknown_slug_retries_then_continues_with_error_item() {
        let src = r#"
id = "t2"
name = "T2"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "call"
kind = "tool_call"
name = "Call"
on_error = "continue"
[node.config]
slug = "bogus_tool"
[node.retry]
max_attempts = 2
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "call"
[[edge]]
from = "call"
to = "done"
"#;
        let dir = tempfile::tempdir().unwrap();
        let file = parse_workflow(src).expect("parses");
        let run = run_workflow(
            Arc::new(HarnessPool::new()),
            deps(dir.path()),
            &tools_record(),
            &file,
            serde_json::json!({ "seed": 1 }),
            &WorkflowRunContext::new(false),
        )
        .await
        .expect("run completes despite the failing node");
        // `on_error = continue` turns the failure into a data item; the message
        // names the unwired slug.
        assert!(
            run.output.to_string().contains("bogus_tool"),
            "the continued error item should carry the failure: {}",
            run.output
        );
    }

    /// T3 — `on_error = "route"` plus an `error`-labeled edge routes the failure
    /// item down the recovery branch.
    #[tokio::test]
    async fn t3_on_error_route_sends_failure_down_the_error_edge() {
        let src = r#"
id = "t3"
name = "T3"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "call"
kind = "tool_call"
name = "Call"
on_error = "route"
[node.config]
slug = "bogus_tool"
[[node]]
id = "recover"
kind = "output"
name = "Recover"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "call"
[[edge]]
from = "call"
to = "done"
[[edge]]
from = "call"
to = "recover"
label = "error"
"#;
        let dir = tempfile::tempdir().unwrap();
        let file = parse_workflow(src).expect("parses");
        let run = run_workflow(
            Arc::new(HarnessPool::new()),
            deps(dir.path()),
            &tools_record(),
            &file,
            serde_json::json!({ "seed": 1 }),
            &WorkflowRunContext::new(false),
        )
        .await
        .expect("run completes via the recovery route");
        let recover_items = &run.output["nodes"]["recover"]["items"];
        assert!(
            recover_items
                .as_array()
                .map(|a| !a.is_empty())
                .unwrap_or(false),
            "the recovery node should receive the routed error item: {}",
            run.output
        );
        assert!(
            run.output.to_string().contains("bogus_tool"),
            "{}",
            run.output
        );
    }

    /// T4 — `requires_approval = true` pauses the node before it runs; the run
    /// reports it on `pending_approvals`.
    #[tokio::test]
    async fn t4_requires_approval_pauses_the_run() {
        let src = r#"
id = "t4"
name = "T4"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "gate"
kind = "tool_call"
name = "Gate"
requires_approval = true
[node.config]
slug = "csv_export"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "gate"
[[edge]]
from = "gate"
to = "done"
"#;
        let dir = tempfile::tempdir().unwrap();
        let file = parse_workflow(src).expect("parses");
        let run = run_workflow(
            Arc::new(HarnessPool::new()),
            deps(dir.path()),
            &tools_record(),
            &file,
            serde_json::json!({ "seed": 1 }),
            &WorkflowRunContext::new(false),
        )
        .await
        .expect("run pauses cleanly");
        assert!(
            run.pending_approvals.iter().any(|id| id == "gate"),
            "the approval-gated node should be pending: {:?}",
            run.pending_approvals
        );
    }

    /// T5 — an `http_request` to a loopback address is refused by the upstream
    /// `url_guard` SSRF check (the happy path is impossible offline by design, so
    /// the guard-in-path is proven via the denial). `on_error` defaults to
    /// `stop`, so the run fails with the guard error.
    #[tokio::test]
    async fn t5_http_request_to_loopback_is_ssrf_denied() {
        let src = r#"
id = "t5"
name = "T5"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "fetch"
kind = "http_request"
name = "Fetch"
[node.config]
method = "GET"
url = "http://127.0.0.1:9/"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "fetch"
[[edge]]
from = "fetch"
to = "done"
"#;
        let dir = tempfile::tempdir().unwrap();
        let file = parse_workflow(src).expect("parses");
        let err = run_workflow(
            Arc::new(HarnessPool::new()),
            deps(dir.path()),
            &tools_record(),
            &file,
            serde_json::json!({ "seed": 1 }),
            &WorkflowRunContext::new(false),
        )
        .await
        .expect_err("the SSRF guard must block the loopback request");
        assert!(
            err.to_string().contains("http_request"),
            "the failure should come from the guarded http client: {err}"
        );
    }

    // --- P2: the six new node kinds, end to end through the engine -----------

    /// Runs `src` through the full translate → compile → engine pipeline with a
    /// tools-granting record and the given `input`.
    async fn run_src(dir: &std::path::Path, src: &str, input: Value) -> Result<WorkflowRun> {
        let file = parse_workflow(src).expect("parses");
        run_workflow(
            Arc::new(HarnessPool::new()),
            deps(dir),
            &tools_record(),
            &file,
            input,
            &WorkflowRunContext::new(false),
        )
        .await
    }

    /// T-switch — each edge label is a case name; the matched case receives the
    /// item and the others don't. A missing field routes to the `default` port.
    #[tokio::test]
    async fn t_switch_routes_each_case_and_default() {
        let src = r#"
id = "sw_wf"
name = "Switch WF"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "route"
kind = "switch"
name = "Route"
[node.config]
field = "kind"
[[node]]
id = "paid_out"
kind = "output"
name = "Paid"
[[node]]
id = "free_out"
kind = "output"
name = "Free"
[[node]]
id = "default_out"
kind = "output"
name = "Default"
[[edge]]
from = "start"
to = "route"
[[edge]]
from = "route"
to = "paid_out"
label = "paid"
[[edge]]
from = "route"
to = "free_out"
label = "free"
[[edge]]
from = "route"
to = "default_out"
label = "default"
"#;
        let dir = tempfile::tempdir().unwrap();

        // A matching case value routes to just that branch.
        let run = run_src(dir.path(), src, serde_json::json!({ "kind": "paid" }))
            .await
            .expect("matched run completes");
        assert!(
            !run.output["nodes"]["paid_out"]["items"].is_null(),
            "the `paid` case should receive the item: {}",
            run.output
        );
        assert!(
            run.output["nodes"]["free_out"].is_null(),
            "the unmatched `free` case should never run: {}",
            run.output
        );

        // A missing field falls to the engine's `default` fallback port.
        let run = run_src(dir.path(), src, serde_json::json!({ "other": 1 }))
            .await
            .expect("default run completes");
        assert!(
            !run.output["nodes"]["default_out"]["items"].is_null(),
            "a null discriminant should route to the `default` branch: {}",
            run.output
        );
    }

    /// T-split_out → transform → merge over a 3-element list: the list fans out
    /// into three items, each transformed, then merged back into one stream.
    #[tokio::test]
    async fn t_split_out_transform_merge_over_a_list() {
        let src = r#"
id = "fan_wf"
name = "Fan WF"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "split"
kind = "split_out"
name = "Split"
[node.config]
path = "values"
[[node]]
id = "double"
kind = "transform"
name = "Double"
[node.config.set]
wrapped = "=item"
[[node]]
id = "join"
kind = "merge"
name = "Merge"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "split"
[[edge]]
from = "split"
to = "double"
[[edge]]
from = "double"
to = "join"
[[edge]]
from = "join"
to = "done"
"#;
        let dir = tempfile::tempdir().unwrap();
        let run = run_src(dir.path(), src, serde_json::json!({ "values": [1, 2, 3] }))
            .await
            .expect("fan-out run completes");
        let merged = run.output["nodes"]["join"]["items"]
            .as_array()
            .expect("merge emitted items");
        assert_eq!(
            merged.len(),
            3,
            "3 list elements → 3 merged items: {}",
            run.output
        );
        // Each transformed item wrapped its scalar under `wrapped`.
        let wrapped: Vec<i64> = merged
            .iter()
            .filter_map(|i| i["json"]["wrapped"].as_i64())
            .collect();
        assert_eq!(wrapped, vec![1, 2, 3], "{}", run.output);
    }

    /// T-transform — the REQUIRED proof that `=`-bindings resolve engine-side
    /// with ZERO OpenCompany evaluation: a dotted shorthand (`=item.brief`) and a
    /// jq program (`=.items | length`) both resolve against the run scope.
    #[tokio::test]
    async fn t_transform_resolves_expr_bindings_engine_side() {
        let src = r#"
id = "tf_wf"
name = "Transform WF"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "tf"
kind = "transform"
name = "Reshape"
[node.config.set]
topic = "=item.brief"
count = "=.items | length"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "tf"
[[edge]]
from = "tf"
to = "done"
"#;
        let dir = tempfile::tempdir().unwrap();
        let run = run_src(dir.path(), src, serde_json::json!({ "brief": "launch" }))
            .await
            .expect("transform run completes");
        let item = &run.output["nodes"]["tf"]["items"][0]["json"];
        assert_eq!(
            item["topic"], "launch",
            "dotted =item.brief: {}",
            run.output
        );
        assert_eq!(item["count"], 1, "jq =.items | length: {}", run.output);
    }

    /// T-output_parser — a valid item passes the schema; a malformed one with
    /// `auto_fix = false` surfaces a capability error routed by `on_error =
    /// continue` into a data item, so the run completes carrying the failure.
    #[tokio::test]
    async fn t_output_parser_validates_and_routes_failure() {
        let base = r#"
id = "op_wf"
name = "Parser WF"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "parse"
kind = "output_parser"
name = "Parse"
on_error = "continue"
[node.config]
auto_fix = false
[node.config.schema]
type = "object"
required = ["name"]
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "parse"
[[edge]]
from = "parse"
to = "done"
"#;
        let dir = tempfile::tempdir().unwrap();

        // A schema-valid item passes straight through.
        let run = run_src(dir.path(), base, serde_json::json!({ "name": "Ada" }))
            .await
            .expect("valid item passes");
        assert!(
            run.output.to_string().contains("Ada"),
            "the validated item should flow through: {}",
            run.output
        );

        // A malformed item (missing `name`) fails validation; `auto_fix = false`
        // makes it a hard error, which `on_error = continue` turns into a data
        // item so the run still completes.
        let run = run_src(dir.path(), base, serde_json::json!({ "other": 1 }))
            .await
            .expect("run completes despite the schema failure");
        assert!(
            run.output.to_string().contains("name"),
            "the continued error item should name the missing property: {}",
            run.output
        );
    }

    /// T-sub_workflow — a `sub_workflow` node runs a child saved on disk (depth
    /// 1), resolved by id through the wired source directory.
    #[tokio::test]
    async fn t_sub_workflow_runs_a_disk_child() {
        let source = tempfile::tempdir().unwrap();
        // The child stamps a distinctive marker so we can prove it ran.
        write_wf(
            source.path(),
            "child",
            r#"
id = "child"
name = "Child"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "mark"
kind = "transform"
name = "Mark"
[node.config.set]
child_marker = "=42"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "mark"
[[edge]]
from = "mark"
to = "done"
"#,
        );
        let parent = r#"
id = "parent"
name = "Parent"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "sub"
kind = "sub_workflow"
name = "Sub"
[node.config]
workflow_id = "child"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "sub"
[[edge]]
from = "sub"
to = "done"
"#;
        let home = tempfile::tempdir().unwrap();
        let file = parse_workflow(parent).expect("parent parses");
        let run = run_workflow(
            Arc::new(HarnessPool::new()),
            deps_with_source(home.path(), source.path()),
            &tools_record(),
            &file,
            serde_json::json!({ "seed": 1 }),
            &WorkflowRunContext::new(false),
        )
        .await
        .expect("sub_workflow run completes");
        assert!(
            run.output.to_string().contains("child_marker"),
            "the child workflow should have run and stamped its marker: {}",
            run.output
        );
    }

    /// T-cycle — two on-disk workflows referencing each other by id hard-reject
    /// with the static cycle message, not the depth backstop.
    #[tokio::test]
    async fn t_mutual_sub_workflows_hard_reject() {
        let source = tempfile::tempdir().unwrap();
        let flow = |id: &str, other: &str| {
            format!(
                r#"
id = "{id}"
name = "{id}"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "sub"
kind = "sub_workflow"
name = "Sub"
[node.config]
workflow_id = "{other}"
[[edge]]
from = "start"
to = "sub"
"#
            )
        };
        write_wf(source.path(), "flow_a", &flow("flow_a", "flow_b"));
        write_wf(source.path(), "flow_b", &flow("flow_b", "flow_a"));

        let home = tempfile::tempdir().unwrap();
        let file = parse_workflow(&flow("flow_a", "flow_b")).expect("parent parses");
        let err = run_workflow(
            Arc::new(HarnessPool::new()),
            deps_with_source(home.path(), source.path()),
            &tools_record(),
            &file,
            serde_json::json!({}),
            &WorkflowRunContext::new(false),
        )
        .await
        .expect_err("a mutual sub_workflow reference must be refused");
        assert!(err.to_string().contains("cycle"), "{err}");
    }

    /// T-dynamic-id — a `=expr`-bound `workflow_id` resolves the child at run
    /// time from the trigger input, proving dynamic references work.
    #[tokio::test]
    async fn t_expr_bound_workflow_id_resolves_dynamically() {
        let source = tempfile::tempdir().unwrap();
        write_wf(
            source.path(),
            "greet_child",
            r#"
id = "greet_child"
name = "Greet child"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "mark"
kind = "transform"
name = "Mark"
[node.config.set]
dynamic_marker = "=99"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "mark"
[[edge]]
from = "mark"
to = "done"
"#,
        );
        // The parent's sub_workflow reads its child id from the trigger input.
        let parent = r#"
id = "dyn_parent"
name = "Dynamic parent"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "sub"
kind = "sub_workflow"
name = "Sub"
[node.config]
workflow_id = "=item.target"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "sub"
[[edge]]
from = "sub"
to = "done"
"#;
        let home = tempfile::tempdir().unwrap();
        let file = parse_workflow(parent).expect("parent parses");
        let run = run_workflow(
            Arc::new(HarnessPool::new()),
            deps_with_source(home.path(), source.path()),
            &tools_record(),
            &file,
            serde_json::json!({ "target": "greet_child" }),
            &WorkflowRunContext::new(false),
        )
        .await
        .expect("dynamic sub_workflow run completes");
        assert!(
            run.output.to_string().contains("dynamic_marker"),
            "the expr-resolved child should have run: {}",
            run.output
        );
    }

    /// A trivial graph is enough — the guard fires before translation.
    const TRIVIAL: &str = r#"
id = "trivial"
name = "Trivial"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "done"
"#;

    /// Outside any run the chain is empty, so nothing is refused.
    #[tokio::test]
    async fn depth_is_zero_outside_a_run() {
        assert_eq!(current_workflow_depth(), 0);
    }

    /// Each nested run sees the ones already on the chain — this is what makes
    /// the guard count a causal chain rather than a moment in time.
    #[tokio::test]
    async fn depth_accumulates_down_a_nested_chain() {
        WORKFLOW_DEPTH
            .scope(1, async {
                assert_eq!(current_workflow_depth(), 1);
                WORKFLOW_DEPTH
                    .scope(2, async {
                        assert_eq!(current_workflow_depth(), 2);
                    })
                    .await;
                // Leaving the inner scope restores the outer depth.
                assert_eq!(current_workflow_depth(), 1);
            })
            .await;
        assert_eq!(current_workflow_depth(), 0);
    }

    /// Two runs side by side are not a chain. A shared counter would refuse the
    /// second; a task-local correctly sees each at depth 0.
    #[tokio::test]
    async fn concurrent_unrelated_runs_do_not_stack() {
        let a = WORKFLOW_DEPTH.scope(1, async { current_workflow_depth() });
        let b = async { current_workflow_depth() };
        let (inside, outside) = tokio::join!(a, b);
        assert_eq!(inside, 1);
        assert_eq!(
            outside, 0,
            "a concurrent run must not inherit another chain's depth"
        );
    }

    /// At the limit the run is refused with a message naming the workflow and
    /// the limit — and, critically, it returns rather than recursing.
    #[tokio::test]
    async fn a_run_at_the_limit_is_refused_with_an_actionable_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = crate::company::parse_workflow(TRIVIAL).expect("parses");

        let err = WORKFLOW_DEPTH
            .scope(MAX_WORKFLOW_DEPTH, async {
                run_workflow(
                    Arc::new(HarnessPool::new()),
                    deps(dir.path()),
                    &tools_record(),
                    &file,
                    Value::Null,
                    &WorkflowRunContext::new(false),
                )
                .await
            })
            .await
            .expect_err("a run at the re-entry limit must be refused");

        let msg = err.to_string();
        assert!(msg.contains("trivial"), "must name the workflow: {msg}");
        assert!(msg.contains("re-entry limit"), "{msg}");
        assert!(
            msg.contains(&MAX_WORKFLOW_DEPTH.to_string()),
            "must state the limit: {msg}"
        );
    }

    /// One level below the limit still runs — the guard bounds recursion, it
    /// does not ban nesting.
    #[tokio::test]
    async fn a_run_below_the_limit_still_executes() {
        let dir = tempfile::tempdir().unwrap();
        let file = crate::company::parse_workflow(TRIVIAL).expect("parses");

        let out = WORKFLOW_DEPTH
            .scope(MAX_WORKFLOW_DEPTH - 1, async {
                run_workflow(
                    Arc::new(HarnessPool::new()),
                    deps(dir.path()),
                    &tools_record(),
                    &file,
                    Value::Null,
                    &WorkflowRunContext::new(false),
                )
                .await
            })
            .await;
        assert!(out.is_ok(), "a run below the limit must execute: {out:?}");
    }

    // --- #395: a paused gate becomes a decidable approval --------------------

    /// The graph T4 uses, with the gate node reachable and an output behind it.
    const GATED: &str = r#"
id = "gated"
name = "Gated"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "gate"
kind = "tool_call"
name = "Gate"
requires_approval = true
[node.config]
slug = "csv_export"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "gate"
[[edge]]
from = "gate"
to = "done"
"#;

    /// Deps with a real approvals queue — the production gate over a `full`
    /// policy and a real on-disk journal.
    ///
    /// `full` is the mode that matters: it is the tier under which the manifest
    /// gate's `evaluate` would *allow* most effects. Parking under it proves the
    /// gate park is the already-decided path rather than a re-evaluation that
    /// would quietly let the run continue.
    fn deps_with_parking(
        dir: &std::path::Path,
    ) -> (HarnessDeps, Arc<crate::runtime::journal::RuntimeJournal>) {
        let policy = toml::from_str("mode = \"full\"\n").expect("valid [policy] block");
        let gate = Arc::new(crate::policy::ManifestApprovalGate::new(policy));
        let journal = Arc::new(crate::runtime::journal::RuntimeJournal::new(
            dir.join("journal.jsonl"),
        ));
        let mut deps = deps(dir);
        deps.delivery = Some(super::super::delivery::WorkflowDeliveryDeps {
            mail: None,
            inbox: Arc::new(crate::store::FsInboxStore::new(dir)),
            users: Arc::new(crate::store::FsOps::new(dir)),
            channels: Vec::new(),
            parking: Some(super::super::delivery::DeliveryParking {
                approvals: gate,
                journal: journal.clone(),
            }),
        });
        (deps, journal)
    }

    /// The headline regression. A run that pauses on `requires_approval` must
    /// leave a **parked effect** behind, not just an id on a response body —
    /// the Approvals page reads the journal, so before #395 it stayed empty
    /// however many gates a run paused on.
    #[tokio::test]
    async fn a_paused_gate_becomes_a_parked_approval() {
        let dir = tempfile::tempdir().unwrap();
        let (deps, journal) = deps_with_parking(dir.path());
        let file = parse_workflow(GATED).expect("parses");

        let run = run_workflow(
            Arc::new(HarnessPool::new()),
            deps,
            &tools_record(),
            &file,
            serde_json::json!({ "request": "quarterly numbers" }),
            &WorkflowRunContext::new(false),
        )
        .await
        .expect("run pauses cleanly");
        assert!(run.pending_approvals.iter().any(|id| id == "gate"));

        let pending = journal.pending();
        let card = pending
            .iter()
            .find(|p| p.effect.kind == crate::runtime::WORKFLOW_APPROVE_KIND)
            .expect("the paused gate is waiting on the operator");
        assert_eq!(card.effect.payload["workflow_id"], "gated");
        assert_eq!(card.effect.payload["node_id"], "gate");
        // Self-contained: the trigger input rides the card, which is what makes
        // approve-after-restart resume without any live state.
        assert_eq!(card.effect.payload["input"]["request"], "quarterly numbers");
        // Native — no teammate asked, so approving must not mint a tool grant.
        assert!(card.effect.agent.is_none());
        // The run that paused, so the console can tie the card to the history.
        assert!(card.effect.run_id.is_some());
    }

    /// Re-running the same graph with the same input must not stack a second
    /// card for one decision — that is how an approvals queue becomes something
    /// an operator rubber-stamps.
    #[tokio::test]
    async fn re_reaching_the_same_gate_does_not_ask_twice() {
        let dir = tempfile::tempdir().unwrap();
        let (deps, journal) = deps_with_parking(dir.path());
        let file = parse_workflow(GATED).expect("parses");
        let input = serde_json::json!({ "request": "same" });

        for _ in 0..3 {
            run_workflow(
                Arc::new(HarnessPool::new()),
                deps.clone(),
                &tools_record(),
                &file,
                input.clone(),
                &WorkflowRunContext::new(false),
            )
            .await
            .expect("run pauses cleanly");
        }

        let gates = journal
            .pending()
            .into_iter()
            .filter(|p| p.effect.kind == crate::runtime::WORKFLOW_APPROVE_KIND)
            .count();
        assert_eq!(gates, 1, "one gate, one decision, one card");
    }

    /// …but a **different** input at the same gate is a genuinely different
    /// decision and must be asked about separately.
    #[tokio::test]
    async fn the_same_gate_on_a_different_input_is_a_second_decision() {
        let dir = tempfile::tempdir().unwrap();
        let (deps, journal) = deps_with_parking(dir.path());
        let file = parse_workflow(GATED).expect("parses");

        for request in ["first", "second"] {
            run_workflow(
                Arc::new(HarnessPool::new()),
                deps.clone(),
                &tools_record(),
                &file,
                serde_json::json!({ "request": request }),
                &WorkflowRunContext::new(false),
            )
            .await
            .expect("run pauses cleanly");
        }

        let gates = journal
            .pending()
            .into_iter()
            .filter(|p| p.effect.kind == crate::runtime::WORKFLOW_APPROVE_KIND)
            .count();
        assert_eq!(gates, 2);
    }

    /// A run an operator stopped parks nothing. They are not asking to be asked
    /// about gates the run never reached, and `cancelled_run` reports no pending
    /// approvals for the same reason.
    #[tokio::test]
    async fn a_cancelled_run_parks_no_gates() {
        let dir = tempfile::tempdir().unwrap();
        let (deps, journal) = deps_with_parking(dir.path());
        let file = parse_workflow(GATED).expect("parses");
        let ctx = WorkflowRunContext::new(false);
        ctx.cancel.cancel();

        let run = run_workflow(
            Arc::new(HarnessPool::new()),
            deps,
            &tools_record(),
            &file,
            serde_json::json!({ "request": "x" }),
            &ctx,
        )
        .await
        .expect("a cancelled run is Ok");
        assert!(run.cancelled);
        assert!(journal.pending().is_empty());
    }

    /// An already-cancelled run must report `cancelled` **every** time, not most
    /// of the time.
    ///
    /// `tokio::select!` polls its branches in random order and picks among those
    /// ready. On a token that is already cancelled, the `cancelled()` arm is
    /// ready on the very first poll — so if the engine future is also ready on
    /// that poll, which arm wins is a coin flip, and the losing half settles as a
    /// completed run with `cancelled: false`. An operator's stop was reported as
    /// a completion.
    ///
    /// Both `select!` sites are `biased;` with the cancel arm first, which makes
    /// an already-signalled cancellation win deterministically.
    ///
    /// This runs the path repeatedly on purpose. A single iteration reproduced
    /// the unbiased defect only about half the time — which is why it read as a
    /// flaky test for as long as it did, and why anyone who re-ran it in
    /// isolation concluded it was not real. At this iteration count a revert to
    /// the unbiased form fails here essentially every time.
    #[tokio::test]
    async fn an_already_cancelled_run_always_reports_cancelled() {
        for iteration in 0..16 {
            let dir = tempfile::tempdir().unwrap();
            let (deps, _journal) = deps_with_parking(dir.path());
            let file = parse_workflow(GATED).expect("parses");
            let ctx = WorkflowRunContext::new(false);
            ctx.cancel.cancel();

            let run = run_workflow(
                Arc::new(HarnessPool::new()),
                deps,
                &tools_record(),
                &file,
                serde_json::json!({ "request": "x" }),
                &ctx,
            )
            .await
            .expect("a cancelled run is Ok");

            assert!(
                run.cancelled,
                "iteration {iteration}: a run cancelled before it started reported \
                 itself as not cancelled — the cancel arm lost the select race"
            );
        }
    }

    /// A graph with no gate parks nothing — the addition must be invisible to
    /// every run that was already working.
    #[tokio::test]
    async fn a_run_that_pauses_on_nothing_parks_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (deps, journal) = deps_with_parking(dir.path());
        let file = parse_workflow(GREET).expect("parses");
        let rec = record();
        let pool = Arc::new(HarnessPool::new());
        pool.ensure(&rec, &deps).await.expect("roster builds");

        let run = run_workflow(
            pool,
            deps,
            &rec,
            &file,
            Value::Null,
            &WorkflowRunContext::new(false),
        )
        .await
        .expect("run completes");
        assert!(run.pending_approvals.is_empty());
        assert!(journal.pending().is_empty());
    }

    // --- #438: a report is delivered once per approval lineage ---------------

    /// A graph that **delivers before it pauses**: the report goes out, then the
    /// gate stops the run. This is the shape that made approving a gate mail the
    /// same person twice, and it is not exotic — "summarise, send it, then ask
    /// me before doing the irreversible thing" is an ordinary workflow.
    const DELIVER_THEN_GATE: &str = r#"
id = "lineage"
name = "Lineage"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "summary"
kind = "output"
name = "Owner summary"
[node.destination]
kind = "owner"
[[node]]
id = "gate"
kind = "output"
name = "Gate"
requires_approval = true
[[edge]]
from = "start"
to = "summary"
[[edge]]
from = "summary"
to = "gate"
"#;

    /// Deps with a real approvals queue **and** a counting mail sender, plus an
    /// active admin so an `owner` destination resolves to a real address.
    ///
    /// The mail sender is the instrument the whole test rests on: the claim is
    /// not "the row says skipped", it is "the transport was called exactly
    /// once across the entire lineage".
    async fn deps_with_parking_and_mail(
        dir: &std::path::Path,
    ) -> (
        HarnessDeps,
        Arc<crate::runtime::journal::RuntimeJournal>,
        crate::server::ops::mailer::RecordingMailSender,
    ) {
        use crate::ports::{UserRecord, UserRole, UserStatus, UserStore};

        let users = Arc::new(FsOps::new(dir));
        users
            .upsert_user(
                &CompanyId::new("acme"),
                &UserRecord {
                    id: "u1".to_string(),
                    email: "ada@acme.test".to_string(),
                    display_name: None,
                    role: UserRole::Admin,
                    status: UserStatus::Active,
                    password_hash: None,
                    must_change_password: false,
                    created_at_millis: 1,
                    last_seen_at_millis: None,
                    updated_at_millis: 1,
                },
            )
            .await
            .expect("admin upserted");

        let mail = crate::server::ops::mailer::RecordingMailSender::new();
        let policy = toml::from_str("mode = \"full\"\n").expect("valid [policy] block");
        let journal = Arc::new(crate::runtime::journal::RuntimeJournal::new(
            dir.join("journal.jsonl"),
        ));
        let mut deps = deps(dir);
        deps.delivery = Some(super::super::delivery::WorkflowDeliveryDeps {
            mail: Some(crate::company::runtime::CompanyMail {
                sender: Arc::new(mail.clone()),
                smtp: crate::server::ops::smtp::SmtpCredentials {
                    host: "smtp.example.test".into(),
                    port: 587,
                    security: crate::server::ops::smtp::SmtpSecurity::Starttls,
                    username: "acme".into(),
                    password: "hunter2".into(),
                    from_name: "Acme".into(),
                    from_email: "acme@opencompany.test".into(),
                },
            }),
            inbox: Arc::new(crate::store::FsInboxStore::new(dir)),
            users,
            channels: Vec::new(),
            parking: Some(super::super::delivery::DeliveryParking {
                approvals: Arc::new(crate::policy::ManifestApprovalGate::new(policy)),
                journal: journal.clone(),
            }),
        });
        (deps, journal, mail)
    }

    /// **The headline regression for issue #438.** Across a whole lineage — the
    /// run that paused plus the continuation an approval starts — the report is
    /// delivered exactly **once**.
    ///
    /// Counted at the transport, not at the row: a `skipped` row proves the
    /// bookkeeping, and only the send count proves nobody's inbox was touched
    /// twice. Before the fix this test reads `2` on the last assertion.
    ///
    /// The continuation is built by
    /// [`continuation_input`](crate::runtime::workflow_resume::continuation_input)
    /// — the same function the Approvals path calls — rather than assembled
    /// here, so what is proven is the production path and not a lookalike. The
    /// approvals plumbing on either side of it (the card is parked, approving
    /// resolves it and spawns) is pinned by `workflow_resume`'s own suite.
    #[tokio::test]
    async fn a_report_is_delivered_once_across_a_gate_and_its_continuation() {
        let dir = tempfile::tempdir().unwrap();
        let (deps, journal, mail) = deps_with_parking_and_mail(dir.path()).await;
        let file = parse_workflow(DELIVER_THEN_GATE).expect("parses");

        // --- run 1: the report goes out, then the run pauses on the gate.
        let first = run_workflow(
            Arc::new(HarnessPool::new()),
            deps.clone(),
            &record(),
            &file,
            serde_json::json!({ "request": "quarterly numbers" }),
            &WorkflowRunContext::new(false),
        )
        .await
        .expect("run pauses cleanly");

        assert!(
            first.pending_approvals.iter().any(|id| id == "gate"),
            "the run must pause on the gate: {:?}",
            first.pending_approvals
        );
        assert_eq!(first.deliveries.len(), 1, "{:?}", first.deliveries);
        assert_eq!(
            first.deliveries[0].status,
            crate::ports::DeliveryStatus::Sent
        );
        assert_eq!(mail.sent().len(), 1, "run 1 sends the report once");

        // --- the operator approves: the card becomes a continuation input.
        let card = journal
            .pending()
            .into_iter()
            .find(|p| p.effect.kind == crate::runtime::WORKFLOW_APPROVE_KIND)
            .expect("the paused gate is waiting on the operator")
            .effect;
        let continuation = crate::runtime::workflow_resume::continuation_input(&card)
            .expect("a well-formed card continues");

        // --- run 2: the same graph, from the trigger, with the gate approved.
        let second = run_workflow(
            Arc::new(HarnessPool::new()),
            deps,
            &record(),
            &file,
            continuation,
            &WorkflowRunContext::new(false),
        )
        .await
        .expect("the continuation runs");

        // The whole point, in one number — asserted FIRST, so a regression
        // fails on the send count itself rather than on the bookkeeping that
        // describes it.
        assert_eq!(
            mail.sent().len(),
            1,
            "one report, one send, across the whole lineage: {:?}",
            mail.sent()
        );

        assert!(
            second.pending_approvals.is_empty(),
            "the approved gate must not pause again: {:?}",
            second.pending_approvals
        );
        assert_eq!(second.deliveries.len(), 1, "{:?}", second.deliveries);
        assert_eq!(
            second.deliveries[0].status,
            crate::ports::DeliveryStatus::Skipped
        );
        assert_eq!(
            second.deliveries[0].reason,
            crate::ports::DeliveryReason::AlreadyDelivered
        );
    }

    // --- issue #371: the per-node progress trail -----------------------------

    /// Deps with a real filesystem journal wired, so the progress path is
    /// exercised end to end rather than through a double: the claim under test
    /// is that these events reach disk in an order a reader can rely on.
    fn deps_with_events(dir: &std::path::Path) -> (HarnessDeps, Arc<dyn crate::ports::EventLog>) {
        let events: Arc<dyn crate::ports::EventLog> = Arc::new(crate::store::FsEventLog::new(dir));
        let mut deps = deps(dir);
        deps.events = Some(events.clone());
        (deps, events)
    }

    /// Every event journaled for `company`, oldest first.
    async fn journaled(
        events: &Arc<dyn crate::ports::EventLog>,
        company: &CompanyId,
    ) -> Vec<CompanyEvent> {
        events
            .read_from(company, crate::ports::types::EventSeq::new(0), usize::MAX)
            .await
            .expect("read")
            .into_iter()
            .map(|s| s.event)
            .collect()
    }

    /// The ordering guarantee the whole read side rests on: a run journals its
    /// start, then one event per non-trigger node **in finish order**, and all
    /// of them are durable before `run_workflow` returns — so the caller's
    /// `WorkflowRunFinished` can only ever land after them.
    ///
    /// `GREET` is `start → ceo → done`; `start` is the trigger and the engine
    /// reports no step for it, so exactly two node events are owed.
    #[tokio::test]
    async fn a_run_journals_a_start_then_one_event_per_non_trigger_node() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Arc::new(HarnessPool::new());
        let rec = record();
        let (deps, events) = deps_with_events(dir.path());
        pool.ensure(&rec, &deps).await.expect("roster builds");

        let file = parse_workflow(GREET).expect("workflow parses");
        let ctx = WorkflowRunContext::new(false);
        run_workflow(pool, deps, &rec, &file, Value::Null, &ctx)
            .await
            .expect("workflow runs");

        let journal = journaled(&events, &rec.id).await;
        let trail: Vec<String> = journal
            .iter()
            .map(|e| match e {
                CompanyEvent::WorkflowRunStarted { .. } => "started".to_string(),
                CompanyEvent::WorkflowNodeFinished { node_id, .. } => format!("node:{node_id}"),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            trail,
            vec!["started", "node:ceo", "node:done"],
            "expected start then one event per non-trigger node, in graph order"
        );

        // One run id across the whole trail — the correlation the fold groups on.
        for event in &journal {
            match event {
                CompanyEvent::WorkflowRunStarted {
                    run_id,
                    workflow_id,
                    scheduled,
                } => {
                    assert_eq!(run_id, &ctx.run_id);
                    assert_eq!(workflow_id, "greet");
                    assert!(!scheduled, "a manual run is not flagged scheduled");
                }
                CompanyEvent::WorkflowNodeFinished {
                    run_id,
                    status,
                    workflow_id,
                    ..
                } => {
                    assert_eq!(run_id, &ctx.run_id);
                    assert_eq!(workflow_id, "greet");
                    assert_eq!(*status, WorkflowNodeStatus::Ok);
                }
                other => panic!("unexpected event on the journal: {other:?}"),
            }
        }
    }

    /// The scheduled flag rides the *start*, not only the outcome — which is
    /// what lets the console mark a cron fire as such while it is still running.
    #[tokio::test]
    async fn a_scheduled_run_is_flagged_on_its_start_event() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Arc::new(HarnessPool::new());
        let rec = record();
        let (deps, events) = deps_with_events(dir.path());
        pool.ensure(&rec, &deps).await.expect("roster builds");

        let file = parse_workflow(GREET).expect("workflow parses");
        run_workflow(
            pool,
            deps,
            &rec,
            &file,
            Value::Null,
            &WorkflowRunContext::new(true),
        )
        .await
        .expect("workflow runs");

        let journal = journaled(&events, &rec.id).await;
        let CompanyEvent::WorkflowRunStarted { scheduled, .. } = &journal[0] else {
            panic!("expected the start first, got {:?}", journal[0]);
        };
        assert!(scheduled);
    }

    /// The arm the issue is really about: a run that dies partway still leaves
    /// the nodes that DID complete on the journal, under the same run id the
    /// caller will stamp on the failure. That pairing is what lets the console
    /// say how far a failed run got instead of only that it failed.
    ///
    /// The graph is `start → ceo → fetch → done`, where `fetch` is an
    /// `http_request` to loopback that the SSRF guard refuses. `on_error`
    /// defaults to `stop`, so the run ends there — with `ceo` already recorded.
    #[tokio::test]
    async fn a_failed_run_still_journals_the_nodes_that_completed() {
        let src = r#"
id = "partial"
name = "Partial"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "ceo"
kind = "agent"
name = "CEO"
agent = "ceo"
[[node]]
id = "fetch"
kind = "http_request"
name = "Fetch"
[node.config]
method = "GET"
url = "http://127.0.0.1:9/"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "ceo"
[[edge]]
from = "ceo"
to = "fetch"
[[edge]]
from = "fetch"
to = "done"
"#;
        let dir = tempfile::tempdir().unwrap();
        let pool = Arc::new(HarnessPool::new());
        let rec = record();
        let (deps, events) = deps_with_events(dir.path());
        pool.ensure(&rec, &deps).await.expect("roster builds");

        let file = parse_workflow(src).expect("parses");
        let ctx = WorkflowRunContext::new(false);
        let outcome = run_workflow(pool, deps, &rec, &file, Value::Null, &ctx).await;
        assert!(outcome.is_err(), "the loopback fetch must fail the run");

        let journal = journaled(&events, &rec.id).await;
        // The start is there, and so is the node that got through before the
        // failure. `done` is not — an unreached node contributes no row, so
        // absence means "never reached", never "silently dropped".
        assert!(matches!(
            journal.first(),
            Some(CompanyEvent::WorkflowRunStarted { .. })
        ));
        let nodes: Vec<&String> = journal
            .iter()
            .filter_map(|e| match e {
                CompanyEvent::WorkflowNodeFinished { node_id, .. } => Some(node_id),
                _ => None,
            })
            .collect();
        assert!(nodes.contains(&&"ceo".to_string()), "{nodes:?}");
        assert!(!nodes.contains(&&"done".to_string()), "{nodes:?}");

        // **The failing node names itself.** A node that dies under the default
        // `stop` policy still reports a step, with `Error` status, before the
        // run ends — so failure attribution on the canvas is exact rather than
        // inferred from "the last node we saw running". Worth pinning: if the
        // engine ever stopped reporting the failing step, the console would
        // silently fall back to guessing, and nothing else would notice.
        let statuses: Vec<(&String, &WorkflowNodeStatus)> = journal
            .iter()
            .filter_map(|e| match e {
                CompanyEvent::WorkflowNodeFinished {
                    node_id, status, ..
                } => Some((node_id, status)),
                _ => None,
            })
            .collect();
        assert_eq!(
            statuses,
            vec![
                (&"ceo".to_string(), &WorkflowNodeStatus::Ok),
                (&"fetch".to_string(), &WorkflowNodeStatus::Error),
            ],
            "the node that failed must be reported as the errored one"
        );

        // Every row shares the caller's id, so the `WorkflowRunFinished` the
        // caller journals for this failure groups with them.
        for event in &journal {
            let run_id = match event {
                CompanyEvent::WorkflowRunStarted { run_id, .. } => run_id,
                CompanyEvent::WorkflowNodeFinished { run_id, .. } => run_id,
                other => panic!("unexpected event: {other:?}"),
            };
            assert_eq!(run_id, &ctx.run_id);
        }
    }

    /// A build with no journal wired (the default runtime, and every other test
    /// in this module) runs exactly as it did before #371 — no start, no
    /// observer, no collector task. The progress path degrades to nothing
    /// rather than to a half-written trail.
    #[tokio::test]
    async fn a_runtime_without_a_journal_records_no_progress() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Arc::new(HarnessPool::new());
        let rec = record();
        let deps = deps(dir.path());
        assert!(deps.events.is_none(), "this is the default-build shape");
        pool.ensure(&rec, &deps).await.expect("roster builds");

        let file = parse_workflow(GREET).expect("workflow parses");
        let run = run_workflow(
            pool,
            deps,
            &rec,
            &file,
            Value::Null,
            &WorkflowRunContext::new(false),
        )
        .await
        .expect("workflow runs");
        assert!(run.pending_approvals.is_empty());
    }

    // --- #383: stopping a run in flight ------------------------------------

    /// A model that parks forever on its first call, after announcing that it
    /// got there.
    ///
    /// This is the lever the whole cancel test rests on, and it has to be an
    /// **agent** node rather than an `http_request` one: a loopback stall server
    /// is unreachable here by design, because the upstream `url_guard` refuses
    /// private/loopback addresses regardless of the company's allowlist (see
    /// `t5_http_request_to_loopback_is_ssrf_denied`). An agent node is the one
    /// node kind whose executor this test can hold open deterministically —
    /// which is also the realistic wedge: the run an operator actually wants to
    /// stop is one sitting on a slow inference call.
    struct StallingProvider {
        entered: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl tinyagents::harness::model::ChatModel<()> for StallingProvider {
        async fn invoke(
            &self,
            _state: &(),
            _request: tinyagents::harness::model::ModelRequest,
        ) -> tinyagents::Result<tinyagents::harness::model::ModelResponse> {
            self.entered.notify_waiters();
            // Never returns. The run is stopped by the future being dropped,
            // which is the mechanism under test.
            std::future::pending::<()>().await;
            unreachable!("the stalling provider is never released")
        }
    }

    impl crate::harness::provider::HarnessModel for StallingProvider {
        fn telemetry_provider_id(&self) -> String {
            "stalling".to_string()
        }
    }

    /// `start → shape → ceo → done`: a transform that finishes instantly, then
    /// an agent node that never will. Cancelling between the two is what proves
    /// the trail keeps the completed node and only the completed node.
    const STALLS: &str = r#"
id = "stalls"
name = "Stalls"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "shape"
kind = "transform"
name = "Shape"
[[node]]
id = "ceo"
kind = "agent"
name = "CEO"
agent = "ceo"
prompt = "Think about it."
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "shape"
[[edge]]
from = "shape"
to = "ceo"
[[edge]]
from = "ceo"
to = "done"
"#;

    /// **The keystone cancel test.** An operator stops a run wedged on an agent
    /// node, and three things have to be true at once:
    ///
    /// 1. the run settles as `cancelled`, not as an error — a deliberate stop is
    ///    not a failure and must never land in the failure count;
    /// 2. the journal keeps a node row for the node that **completed** and none
    ///    for the one that was still executing — "how far did it get before I
    ///    stopped it" is the question the trail exists to answer, and inventing
    ///    a row for the wedged node would answer it wrongly;
    /// 3. **it comes back fast.**
    ///
    /// The third is the one worth the stopwatch. If the engine future were not
    /// dropped before the observer, its per-node handlers would still hold
    /// observer `Arc` clones, the progress channel would stay open, and the
    /// collector join would block for the full `PROGRESS_DRAIN_TIMEOUT` — ten
    /// seconds, on every single cancel. And it would still *pass* the first two
    /// assertions, because the timeout swallows it and the run settles
    /// correctly in the end. Only the clock catches that bug, which is why this
    /// asserts a bound rather than just an outcome.
    #[tokio::test]
    async fn a_cancelled_run_settles_fast_keeping_only_its_completed_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Arc::new(HarnessPool::new());
        let rec = record();
        let entered = Arc::new(tokio::sync::Notify::new());
        let (mut deps, events) = deps_with_events(dir.path());
        deps.provider = Arc::new(StallingProvider {
            entered: entered.clone(),
        });
        deps.provider_slug = "stalling".to_string();
        pool.ensure(&rec, &deps).await.expect("roster builds");

        let file = parse_workflow(STALLS).expect("workflow parses");
        let ctx = WorkflowRunContext::new(false);
        let cancel = ctx.cancel.clone();

        // Registered *before* the run starts, so the wedged node cannot slip
        // past the notification and leave this test waiting forever.
        let reached_the_agent = entered.notified();

        let mut run = Box::pin(run_workflow(pool, deps, &rec, &file, Value::Null, &ctx));
        tokio::select! {
            _ = &mut run => panic!("the run finished, so the agent node did not stall"),
            () = reached_the_agent => {}
        }

        // The operator presses Cancel. From here the clock is the assertion.
        let pressed = std::time::Instant::now();
        cancel.cancel();
        let run = tokio::time::timeout(std::time::Duration::from_secs(30), run)
            .await
            .expect("the cancelled run never returned at all")
            .expect("a cancelled run is Ok, not Err");
        let elapsed = pressed.elapsed();

        assert!(run.cancelled, "the run must report that it was stopped");
        assert!(
            run.deliveries.is_empty(),
            "a cancelled run must not route reports for work it did not finish"
        );

        // **The drain-timeout guard.** `PROGRESS_DRAIN_TIMEOUT` is 10s; a
        // correctly-dropped engine future settles in milliseconds. Two seconds
        // is far below the timeout and far above a healthy cancel, so this
        // fails loudly on a missing `drop(engine)` without being flaky on a
        // loaded CI box.
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "cancelling took {elapsed:?} — the progress channel did not close, so the collector \
             join stalled until the drain timeout. Check that the engine future is dropped BEFORE \
             the observer in `run_workflow_inner`."
        );
        assert!(
            elapsed < PROGRESS_DRAIN_TIMEOUT,
            "cancel latency reached the drain timeout itself"
        );

        // The trail: `shape` completed, `ceo` was still executing. Neither the
        // wedged node nor anything downstream may appear.
        let journal = journaled(&events, &rec.id).await;
        let nodes: Vec<String> = journal
            .iter()
            .filter_map(|e| match e {
                CompanyEvent::WorkflowNodeFinished { node_id, .. } => Some(node_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            nodes,
            vec!["shape".to_string()],
            "only the node that actually finished belongs on the trail"
        );
        // The start is still there and still correlates, so the caller's
        // `WorkflowRunFinished{cancelled}` groups with this trail rather than
        // stranding it.
        let CompanyEvent::WorkflowRunStarted { run_id, .. } = &journal[0] else {
            panic!("expected the start first, got {:?}", journal[0]);
        };
        assert_eq!(run_id, &ctx.run_id);
    }

    /// The same stop works with **no journal wired** — the default-build shape,
    /// where there is no observer and no collector at all.
    ///
    /// A separate test because it is a separate `select!`: the two arms of the
    /// `events` match each drive the engine differently, and an early version of
    /// this change made only the observed one cancellable. Nothing else would
    /// have noticed.
    #[tokio::test]
    async fn a_run_with_no_journal_is_cancellable_too() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Arc::new(HarnessPool::new());
        let rec = record();
        let entered = Arc::new(tokio::sync::Notify::new());
        let mut deps = deps(dir.path());
        assert!(deps.events.is_none(), "this is the default-build shape");
        deps.provider = Arc::new(StallingProvider {
            entered: entered.clone(),
        });
        pool.ensure(&rec, &deps).await.expect("roster builds");

        let file = parse_workflow(STALLS).expect("workflow parses");
        let ctx = WorkflowRunContext::new(false);
        let cancel = ctx.cancel.clone();
        let reached_the_agent = entered.notified();

        let mut run = Box::pin(run_workflow(pool, deps, &rec, &file, Value::Null, &ctx));
        tokio::select! {
            _ = &mut run => panic!("the run finished, so the agent node did not stall"),
            () = reached_the_agent => {}
        }

        cancel.cancel();
        let run = tokio::time::timeout(std::time::Duration::from_secs(30), run)
            .await
            .expect("the cancelled run never returned")
            .expect("a cancelled run is Ok, not Err");
        assert!(run.cancelled);
    }

    /// A run whose signal is fired **before** it starts stops immediately rather
    /// than walking the graph anyway.
    ///
    /// This is the watch-vs-`Notify` property, proven through the runner rather
    /// than the primitive: with an edge-triggered signal the `select!` would
    /// miss the already-fired cancel and the run would complete normally, which
    /// is exactly the race a cancel arriving during graph compilation would hit.
    #[tokio::test]
    async fn a_run_cancelled_before_it_starts_does_not_walk_the_graph() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Arc::new(HarnessPool::new());
        let rec = record();
        let (deps, events) = deps_with_events(dir.path());
        pool.ensure(&rec, &deps).await.expect("roster builds");

        let file = parse_workflow(GREET).expect("workflow parses");
        let ctx = WorkflowRunContext::new(false);
        ctx.cancel.cancel();

        let run = run_workflow(pool, deps, &rec, &file, Value::Null, &ctx)
            .await
            .expect("a cancelled run is Ok, not Err");
        assert!(run.cancelled);

        let journal = journaled(&events, &rec.id).await;
        assert!(
            !journal
                .iter()
                .any(|e| matches!(e, CompanyEvent::WorkflowNodeFinished { .. })),
            "a run cancelled before it began must not report any node as finished"
        );
    }
}
