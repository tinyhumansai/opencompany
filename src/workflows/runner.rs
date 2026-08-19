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

/// How long a stopped run is given to wind down cleanly at a node boundary
/// before the runner falls back to the hard abort (issue #398).
///
/// When an operator stops a run, the runner flips the engine's
/// [`CancellationToken`](tinyflows::engine::CancellationToken): the engine checks
/// it before each node and, once a node in flight finishes, winds the run down
/// and returns a real (partial) [`RunOutcome`] with `cancelled` set. That is the
/// clean path — the collected node trail is kept and nothing is dropped
/// mid-await.
///
/// But a node wedged mid-await on a stalled external call never reaches the next
/// boundary, so the token alone could hang the stop forever (see the
/// `StallingProvider` test). This bound caps the wait: if the run has not wound
/// down within it, the runner drops the engine future — the pre-#398 hard abort
/// — so a wedged run stays killable. Generous enough that a healthy node
/// crossing a boundary always makes it, short enough that a stuck stop is not an
/// eternity.
const CANCEL_HARD_ABORT_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

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

/// One per-node progress frame, as the engine's observer callbacks hand it over
/// to the async collector (issue #371 for the finish, issue #382 for the start).
///
/// A node produces a [`Started`](Self::Started) frame just before its first
/// attempt and a [`Finished`](Self::Finished) frame as it settles, both on the
/// **same** channel, so the collector sees them in that order and a node's
/// started event always journals ahead of its finished one.
///
/// Deliberately no whole `ExecutionStep`. A `Started` frame carries the node id
/// alone; the node has not run, so there is no status, duration, or output to
/// carry either. What is not carried cannot leak.
///
/// A `Finished` frame carries the node's `output` items (issue #1008) **as well
/// as** its status and duration — but that payload is used for exactly one thing
/// and is walled off from the journal: the collector accumulates it into a
/// side map that feeds ONLY the durable, console-facing run-output store on the
/// **failure/blocked arms**, where the engine returns no `outcome.output` to
/// persist from. The journal still receives only `{node_id, status,
/// elapsed_ms}` (see the `Finished` arm of the collector, which builds its
/// `WorkflowNodeFinished` from those three scalars and never touches `output`) —
/// the same stance the live turn frames take on tool args. Before #1008 the
/// failure paths threw this output away, so a failed/blocked run's inspector
/// wrongly claimed the run predated output capture.
enum NodeProgress {
    /// A node began executing (issue #382) — the opening bracket. Id only.
    Started { node_id: String },
    /// A node finished (issue #371) — its status, wall-clock duration, and the
    /// items it emitted (issue #1008; [`Value::Null`] on an error step). The
    /// output is consumed only by the run-output persist on the failure arms;
    /// it never reaches the journal.
    Finished {
        node_id: String,
        status: WorkflowNodeStatus,
        elapsed_ms: u64,
        output: Value,
    },
}

/// A [`RunObserver`](tinyflows::observability::RunObserver) that forwards each
/// node start and finish onto an unbounded channel.
///
/// The channel is the whole reason this type exists. Observer callbacks are
/// **synchronous** (the engine invokes them inline, across threads) while
/// [`EventLog::append`] is async, so the callback cannot journal directly. It
/// also must not block: a node handler stalled on a disk write would make
/// observability change the run's timing, which is exactly what an observer is
/// not allowed to do. Unbounded is safe at this volume — two messages per
/// non-trigger node, ~16 for a six-node graph — and it means a slow journal can
/// never apply backpressure to the engine.
struct ProgressObserver {
    tx: tokio::sync::mpsc::UnboundedSender<NodeProgress>,
}

impl tinyflows::observability::RunObserver for ProgressObserver {
    fn on_step_start(&self, node_id: &str) {
        // Issue #382: the node's opening bracket, sent on the SAME channel and
        // therefore BEFORE its finish — the collector processes the channel in
        // order, so a node's started event is always journaled ahead of its
        // finished one. A closed receiver (the run is settling, or `deps.events`
        // was never wired) drops the frame, exactly as the finish arm does:
        // progress reporting must never disturb the run.
        let _ = self.tx.send(NodeProgress::Started {
            node_id: node_id.to_string(),
        });
    }

    fn on_step_finish(&self, step: &tinyflows::observability::ExecutionStep) {
        // A closed receiver means the collector already stopped (the run is
        // settling, or `deps.events` was never wired). Dropping the frame is
        // correct: progress reporting must never disturb the run.
        let _ = self.tx.send(NodeProgress::Finished {
            node_id: step.node_id.clone(),
            status: match step.status {
                tinyflows::observability::StepStatus::Success => WorkflowNodeStatus::Ok,
                tinyflows::observability::StepStatus::Error => WorkflowNodeStatus::Error,
            },
            // `u128` millis is the engine's type; a node running longer than
            // 584 million years is not the failure mode worth a `Result`.
            elapsed_ms: u64::try_from(step.duration_ms).unwrap_or(u64::MAX),
            // Issue #1008: the node's emitted items, carried so the collector can
            // accumulate a partial per-node output map for the failure/blocked
            // arms (which have no `outcome.output` to persist from). A success
            // step's `output` is the items array; an error step's is
            // `Value::Null`. This clone rides the same channel as the scalars
            // and never touches the journal.
            output: step.output.clone(),
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
    let mut graph = super::translate::translate(workflow);
    // Issue #460: the company's `ApprovalPolicy` decides which `tool_call`
    // nodes stop for an operator, and says so by marking them with the engine's
    // own `requires_approval` flag — so a gated tool call inherits #395's whole
    // pause → park → resume path instead of needing a second one. BEFORE
    // `compile`, because the flag is read off the compiled node config.
    //
    // Skipped for a dry run: every effect is stubbed, so there is nothing to
    // approve, and pausing would stop the dry run walking the rest of the graph
    // — the one thing it exists to do. See `super::gate` for why the gate is
    // not in the invoker, and for the deviations this takes deliberately.
    let gated = if ctx.dry_run {
        Vec::new()
    } else {
        super::gate::apply_policy_gates(&mut graph, record, &workflow.id, &ctx.run_id).await
    };
    // Issue #846: a node whose call already left the building in an earlier run
    // of this lineage replays its recorded result instead of calling again.
    // Driven entirely off the trigger input's ledger, so a first run rewrites
    // nothing and the graph stays byte-identical.
    //
    // **After the gate pass, and the order is load-bearing.** The gate pass
    // classifies a node by its slug; running it second would have it classify
    // the host's replay sentinel — an inert slug no policy has an opinion about
    // — and either gate a node that does nothing or fail to gate one that does.
    // Nothing is lost by this order: a replayed node was necessarily executed by
    // an earlier run, so its id is already in the input's `approvals` array and
    // the gate it still carries falls straight through.
    let replayed = super::replay::replay_performed(&mut graph, &input);
    if !replayed.is_empty() {
        tracing::info!(
            company = %record.id,
            workflow = %workflow.id,
            run_id = %ctx.run_id,
            nodes = ?replayed,
            "workflow: this continuation replays calls an earlier run in its lineage already \
             made, rather than repeating them"
        );
    }
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
    // Issue #596: the durable, console-facing run-output store, read off `deps`
    // before it moves into the capability bundle — like `events`/`delivery`
    // above. `None` (default build, unwired tests) degrades the persist to a
    // no-op. Kept beside `events` so it rides the same "read the host-side ports
    // out before the engine takes deps" pattern.
    let run_output_store = deps.run_output_store.clone();
    // Issue #542: the mode. A dry run walks the same real graph over stubbed
    // effectful capabilities (see `caps::dry_run`) and, host-side, skips every
    // durable effect around the engine — the started/finished/node journal
    // writes, the delivery dispatch, and gate parking. Read once here.
    let dry_run = ctx.dry_run;
    // Issue #661 (L2): a failed per-run-workspace mkdir aborts the run here,
    // BEFORE the WorkflowRunStarted journal append below — so a workspace the
    // effects cannot be rooted at leaves no orphaned started row, and the caller
    // sees the real cause instead of a later, further-removed effect failure.
    // Issue #638: where an agent node leaves an operator-facing notice. Owned
    // here, by the run, because that is the only scope that outlives the nodes
    // and reaches `WorkflowRun`.
    let notices = super::caps::RunNotices::default();
    // Issue #661 (M5): where the run's board writes are recorded. Owned here for
    // the same reason `notices` is — the nodes come and go, and this is the only
    // scope that outlives them and reaches `WorkflowRun`. Critically it is owned
    // *outside* the capability bundle, so a hard abort that drops the engine future
    // (and with it the bundle and its board claim) still leaves every row already
    // collected readable here: a card is real once written, so it must stay listed.
    // Issue #976: a graph whose only node is its trigger has nothing to execute.
    // The engine runs it happily — there is no stage to fail — so it settles as
    // an ordinary finished run, and a run row that says nothing is its own small
    // lie: `QA Test Pipeline` on staging banked six of them. Said here, through
    // the channel #638 built for exactly this shape of fact.
    //
    // A notice rather than an error, for the reason `notices` exists at all: an
    // empty graph is not a failure. Nothing broke, nothing was attempted, and
    // marking it failed would put a half-authored stub into the failure count
    // next to runs that genuinely went wrong. Same call `NoDestinationConfigured`
    // makes one level down (#925) — state the reason instead of leaving it to be
    // inferred from an absence.
    if !workflow.has_runnable_node() {
        notices.push(crate::company::STAGELESS_WORKFLOW_NOTICE.to_string());
        tracing::warn!(
            company = %record.id,
            workflow = %workflow.id,
            "workflow run: the graph has no runnable node, so this run could not do anything"
        );
    }
    let board = super::caps::RunBoard::default();
    // Issue #881 / #880: the two sideways channels an agent node reports through
    // — that it blocked, and what it parked. Owned out here for exactly the
    // reason `board` is: a blocked node halts the run, which drops the engine
    // future and the capability bundle with it, and both of these facts have to
    // survive that. An approval card is durable the moment it is written, so a
    // run that ended badly must still be able to say it opened one.
    let blocks = super::caps::RunBlocks::default();
    let approvals = super::caps::RunApprovals::default();
    let capabilities = super::caps::build_capabilities(
        pool,
        deps,
        record,
        super::caps::RunContext {
            workflow_id: &workflow.id,
            run_id: &run_id,
            run_request,
            dry_run,
            notices: notices.clone(),
            board: board.clone(),
            blocks: blocks.clone(),
            approvals: approvals.clone(),
        },
    )
    .await?;

    // The opening bracket, appended BEFORE the engine call so a run killed
    // mid-flight leaves a start with no finish — which is precisely the shape
    // the boot sweep looks for. Best-effort, like every other journal write on
    // this path: losing the record is worth a log line, never a failed run.
    //
    // Issue #542: skipped entirely for a dry run. A test run writes NOTHING
    // durable — no started row, so no boot sweep ever adopts it and no `running`
    // row ever appears in the history; the settled response body is its whole
    // record.
    if let Some(events) = events.as_ref().filter(|_| !dry_run) {
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

    // Issue #371 + #542: ALWAYS drive with a progress observer, so the per-node
    // timeline is collected for **every** run — it feeds `WorkflowRun.nodes` on
    // all paths, and for a dry run (which journals nothing) it is the only trail
    // the run leaves. The collector accumulates one `WorkflowRunNodeRow` per
    // node and, *additionally*, appends a `WorkflowNodeFinished` to the journal
    // only when there is a journal AND this is not a dry run.
    //
    // The journal is passed to the collector only in that case; `None` there
    // means "collect the rows, write nothing" — the shape the default build and
    // every dry run take.
    //
    // Issue #1008: the collector *additionally* accumulates a per-node output
    // map (`{ "<id>": { "items": [ … ] } }`) from each `Finished` frame's
    // output. This map exists solely to feed the durable run-output persist on
    // the failure/blocked arms, where the engine returns no `outcome.output`. It
    // is returned beside the rows and never journaled — the journal invariant
    // (no node output) is preserved because the `WorkflowNodeFinished` event is
    // still built from the three scalars alone.
    let journal_nodes = events.clone().filter(|_| !dry_run);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<NodeProgress>();
    let collector = tokio::spawn({
        let company = record.id.clone();
        let workflow_id = workflow.id.clone();
        let run_id = run_id.clone();
        async move {
            let mut rows: Vec<crate::ports::WorkflowRunNodeRow> = Vec::new();
            // Issue #1008: node_id -> `{ "items": [ … ] }`, canonical-shaped so
            // the console's `parseNodeMessages` reads it exactly like a clean
            // run's `outcome.output["nodes"]`.
            let mut partial_nodes = serde_json::Map::new();
            while let Some(progress) = rx.recv().await {
                match progress {
                    // Issue #382: the node's opening bracket. Journaled only when
                    // there is a journal AND this is not a dry run — the same
                    // gate the finish arm applies. It contributes NO response
                    // row: `WorkflowRun.nodes` stays finished-only, so a started
                    // node with no finish never masquerades as a completed one.
                    NodeProgress::Started { node_id } => {
                        if let Some(events) = journal_nodes.as_ref() {
                            let event = CompanyEvent::WorkflowNodeStarted {
                                workflow_id: workflow_id.clone(),
                                run_id: run_id.clone(),
                                node_id,
                            };
                            if let Err(err) = events.append(&company, event).await {
                                tracing::warn!(
                                    %company,
                                    workflow = %workflow_id,
                                    %run_id,
                                    %err,
                                    "workflow: node-started progress event could not be journaled; \
                                     the run is unaffected"
                                );
                            }
                        }
                    }
                    NodeProgress::Finished {
                        node_id,
                        status,
                        elapsed_ms,
                        output,
                    } => {
                        if let Some(events) = journal_nodes.as_ref() {
                            let event = CompanyEvent::WorkflowNodeFinished {
                                workflow_id: workflow_id.clone(),
                                run_id: run_id.clone(),
                                node_id: node_id.clone(),
                                status,
                                elapsed_ms,
                            };
                            if let Err(err) = events.append(&company, event).await {
                                tracing::warn!(
                                    %company,
                                    workflow = %workflow_id,
                                    %run_id,
                                    %err,
                                    "workflow: node progress event could not be journaled; the run \
                                     is unaffected"
                                );
                            }
                        }
                        // Issue #1008: accumulate this node's output into the
                        // partial map under the canonical `{ "items": [ … ] }`
                        // shape. A success step's `output` is the items array; an
                        // error (or any non-array) step contributes an empty
                        // `items`, so the failing node still appears as "produced
                        // none" rather than vanishing. Keyed before `node_id`
                        // moves into the row below.
                        let items = match output {
                            Value::Array(_) => output,
                            _ => Value::Array(Vec::new()),
                        };
                        partial_nodes
                            .insert(node_id.clone(), serde_json::json!({ "items": items }));
                        // Collected for the response on every path — status is
                        // `Copy`, `node_id` moves in after its clone (if any)
                        // went to the event.
                        rows.push(crate::ports::WorkflowRunNodeRow {
                            node_id,
                            status,
                            elapsed_ms,
                        });
                    }
                }
            }
            (rows, partial_nodes)
        }
    });

    let observer: Arc<dyn tinyflows::observability::RunObserver> =
        Arc::new(ProgressObserver { tx });
    // Issue #383/#398: the engine call is raced against the run's stop signal.
    //
    // # The engine's own token, with a bounded hard-abort fallback
    //
    // tinyflows exposes `run_cancellable_with_observer`, which takes a
    // `CancellationToken` **and** an observer — so a cancellable run keeps the
    // per-node progress trail (#371/#382) instead of trading it away, which the
    // old host-side "drop the future" race had to. The engine checks the token
    // before each node, so cancelling stops the run at the next **node boundary**
    // rather than mid-await: a node already executing runs to completion, its
    // finish is journaled, and the run winds down carrying a real (partial)
    // `RunOutcome` with `cancelled` set. That is the clean path.
    //
    // # Why the fallback survives (decision locked)
    //
    // A node wedged mid-await on a stalled external call never reaches the next
    // boundary, so the token alone could hang the stop forever (the
    // `StallingProvider` test is exactly this). So the stop path flips the token,
    // gives the run a bounded `CANCEL_HARD_ABORT_GRACE` to wind down cleanly, and
    // ONLY if it does not settle in that window drops the engine future — the
    // pre-#398 hard abort. Dropping stops the run mid-await: a node part way
    // through an external side effect stays part way through it, the same class
    // of outcome as the host being killed, which the boot sweep already settles.
    // Keeping this fallback is what guarantees a wedged run stays killable.
    //
    // `Box::pin` because the losing branch must be droppable, which a
    // `tokio::pin!`ed local is not.
    let token = tinyflows::engine::CancellationToken::new();
    let mut engine = Box::pin(tinyflows::engine::run_cancellable_with_observer(
        &compiled,
        input,
        &capabilities,
        token.clone(),
        &observer,
    ));
    let outcome_opt = tokio::select! {
        biased;
        () = ctx.cancel.cancelled() => {
            // Node-boundary stop: flip the engine's token so it winds down
            // cleanly, then bound the wait. A run that crosses a boundary within
            // the grace returns its real `cancelled` outcome; a wedged one times
            // out — the `Err` becomes `None`, falling through to the hard abort
            // below.
            token.cancel();
            tokio::time::timeout(CANCEL_HARD_ABORT_GRACE, &mut engine)
                .await
                .ok()
        }
        outcome = &mut engine => Some(outcome),
    };
    // **Drop the engine future before the observer.** It only matters on the
    // hard-abort arm (`None`): there the future still owns the observer `Arc`
    // clones its per-node handlers hold, so dropping `observer` alone would NOT
    // close the channel — the collector below would then block until the drain
    // timeout on every wedged stop, and the timeout would hide it. On the clean
    // arms (completion or a wound-down cancel) the graph already died inside the
    // call and those clones are gone, so this drop is a no-op.
    //
    // Today the **borrow checker enforces this ordering**: the engine future
    // borrows `observer`, so removing this line does not compile. That is a
    // happy accident of the current signature taking `&Arc`, not a guarantee —
    // an engine that took the `Arc` by value would close the compile-time hole
    // and re-open the runtime one, silently.
    // `a_cancelled_run_settles_fast_keeping_only_its_completed_nodes` asserts a
    // cancel-latency bound for exactly that case; it was verified to fail (at
    // 10.004s, the full drain timeout) against a deliberately leaked observer
    // clone.
    drop(engine);

    // Drop the last sender, then join — in that order, and before anything else.
    // The drop closes the channel so the collector's `recv()` returns `None` and
    // its loop ends; the join then waits for every already-sent frame to reach
    // the journal (when journaling) and returns the collected rows either way.
    // This is what makes the ordering guarantee true: every
    // `WorkflowNodeFinished` is durably appended before the caller's
    // `WorkflowRunFinished`, so a reader folding the journal never sees a run
    // settle before its nodes land.
    //
    // The drop closes the channel only if ours is the **last** `Arc` — true
    // today, because the engine's per-node handler clones die with the graph
    // inside `run_with_observer`. The bounded wait keeps that a *performance*
    // assumption rather than a liveness one: if a future engine ever parked a
    // clone somewhere longer-lived, this would log and move on with an empty
    // trail instead of wedging the run (and the host behind it) forever.
    drop(observer);
    // Issue #1008: the collector now returns both the response rows and the
    // accumulated per-node output map. The map feeds ONLY the run-output persist
    // on the failure/blocked arms below; `nodes` stays the output-free row list
    // the journal and `WorkflowRun.nodes` carry. A drain failure yields an empty
    // map, so a persist on that path simply records "produced none".
    let (nodes, partial_nodes): (
        Vec<crate::ports::WorkflowRunNodeRow>,
        serde_json::Map<String, Value>,
    ) = match tokio::time::timeout(PROGRESS_DRAIN_TIMEOUT, collector).await {
        Ok(Ok(collected)) => collected,
        Ok(Err(err)) => {
            tracing::warn!(
                company = %record.id,
                workflow = %workflow.id,
                %run_id,
                %err,
                "workflow: the node-progress collector did not shut down cleanly"
            );
            (Vec::new(), serde_json::Map::new())
        }
        Err(_) => {
            tracing::warn!(
                company = %record.id,
                workflow = %workflow.id,
                %run_id,
                "workflow: node progress events did not drain in time; the run's finished \
                 record may be journaled ahead of them"
            );
            (Vec::new(), serde_json::Map::new())
        }
    };

    // Resolved only AFTER the drain above, so a cancelled run's completed nodes
    // are journaled exactly like a completed run's before the caller writes the
    // finish.
    //
    // `None` is the **hard-abort** arm (issue #398): the run was wedged past the
    // grace window and its future was dropped, so there is no outcome to read —
    // `cancelled_run()` reports the stop with an empty body, and the trail is the
    // journal, not this return.
    let outcome = match outcome_opt {
        Some(Ok(outcome)) => outcome,
        // Issue #881: the engine failed the run. Before deciding that is what
        // happened, ask whether every node that errored was one the host
        // *blocked* on an approval — because `on_error` defaults to `"stop"`,
        // a blocked agent node reaches the caller as exactly this `Err`.
        //
        // The containment check is what keeps this from hiding a real failure:
        // a run whose blocked node is joined by a genuinely broken one still
        // reports the error, and the block survives on the approval receipts.
        Some(Err(err)) => {
            let blocked = blocks.take();
            // Issue #1008: the engine returns no `outcome.output` on this arm, so
            // the run's per-node output lives ONLY in the map the progress
            // observer accumulated. Persist that, flagged `partial`, on BOTH the
            // genuine-failure and the blocked branches — before #1008 both threw
            // it away, so the inspector wrongly reported "this run predates output
            // capture" for every failed or blocked run.
            let partial_output = Value::Object(partial_nodes);
            if blocked.is_empty() || !only_blocked_nodes_errored(&nodes, &blocked) {
                // A genuine failure. Persist the partial capture so the inspector
                // shows what the nodes that ran produced. Log-only on a write
                // error (Part 3): this branch returns `Err`, so there is no
                // `WorkflowRun` to hang a notice on.
                let _ = persist_run_output(
                    run_output_store.as_deref(),
                    &record.id,
                    &workflow.id,
                    &run_id,
                    &partial_output,
                    true,
                )
                .await;
                return Err(map_engine_error(err));
            }
            tracing::info!(
                company = %record.id,
                workflow = %workflow.id,
                %run_id,
                nodes = ?blocked.iter().map(|b| &b.node_id).collect::<Vec<_>>(),
                "workflow: the run stopped because a node is waiting on an operator, not because \
                 it failed"
            );
            // A blocked run DOES return a `WorkflowRun`, so a failed persist adds
            // an operator-facing notice rather than only a log line (Part 6).
            if !persist_run_output(
                run_output_store.as_deref(),
                &record.id,
                &workflow.id,
                &run_id,
                &partial_output,
                true,
            )
            .await
            {
                notices.push(run_output_persist_failed_notice());
            }
            return Ok(blocked_run(BlockedRun {
                nodes,
                blocked,
                notices,
                board: board.take(),
                approvals: approvals.take(),
            }));
        }
        None => {
            return Ok(cancelled_run(
                notices.take(),
                board.take(),
                approvals.take(),
            ));
        }
    };

    // Issue #398: the **clean** node-boundary cancel. The engine observed the
    // flipped token and wound down at a boundary, so unlike the hard-abort arm
    // above there IS a real (partial) outcome and the collected node rows are
    // meaningful — carry them. A stop still routes nothing and parks no gate: an
    // operator who stopped a run is asking neither to deliver its half-finished
    // reports nor to be asked about gates it never reached. `pending_approvals`
    // is emptied for the same reason `cancelled_run()` empties it — listing gates
    // this run will not continue would imply it is still waiting on them.
    if outcome.cancelled {
        // Issue #596: a cleanly-cancelled run still produced real partial output
        // for the nodes that completed — persist it so the console inspector can
        // show how far the run got and what each finished node made. This is an
        // outcome-bearing arm (unlike the hard-abort return above, which has no
        // outcome and persists nothing).
        //
        // Issue #1008: this is a CLEAN cancel with a real `outcome.output`, so it
        // persists that canonical map with `partial = false` — the "partial"
        // flag is reserved for the failure/blocked arms that have no outcome and
        // fall back to the observer's accumulated capture. A failed write adds an
        // operator notice (Part 6), since this arm returns a `WorkflowRun`.
        let raw_nodes = outcome.output.get("nodes").cloned().unwrap_or(Value::Null);
        if !persist_run_output(
            run_output_store.as_deref(),
            &record.id,
            &workflow.id,
            &run_id,
            &raw_nodes,
            false,
        )
        .await
        {
            notices.push(run_output_persist_failed_notice());
        }
        return Ok(WorkflowRun {
            output: outcome.output,
            pending_approvals: Vec::new(),
            deliveries: Vec::new(),
            cancelled: true,
            nodes,
            // A stopped run still reports what its completed nodes had to say
            // — a discarded-overflow notice describes calls that were already
            // refused before the stop, and withholding it would leave the
            // operator with fewer cards than were gated and no explanation.
            notices: notices.take(),
            // Issue #661 (M5): a cancelled run's board writes SURVIVE and stay
            // listed. A card is a durable write the moment the drain performs it,
            // so an operator who stopped the run still has the card in front of
            // them — dropping the row would leave a card on the board that no run
            // admits to opening. (A run cancelled *mid-turn* staged writes that
            // were never drained, so it has no card and no row: consistent, and the
            // same judgement `park_gated_calls` documents for gated calls.)
            board: board.take(),
            // Issue #881: a node the host blocked cannot be reached on this arm
            // — a block halts the run, which is the `Err` arm above, not a
            // clean node-boundary cancel. Taken rather than hard-coded empty so
            // the claim stays a test's to make.
            blocked_nodes: blocks.take(),
            // Issue #880: NOT zeroed, unlike `deliveries` / `pending_approvals`
            // one field up. Those describe what the run would still do; this
            // describes what it already did. A run stopped after parking two
            // approvals really did park them — the cards are on the operator's
            // Approvals page right now — so dropping the rows would leave two
            // cards no run admits to opening. Same argument as `board` above.
            approvals: approvals.take(),
        });
    }

    // Issue #542: a dry run STOPS here on the effect side. Route the reached
    // `output` nodes so the operator sees WHERE each report would have gone —
    // that routing is exactly what a test run is meant to prove — but dispatch
    // nothing, journal nothing, and park no gate. The per-node timeline in
    // `nodes` and this settled body are the whole record.
    if dry_run {
        let deliveries = super::delivery::deliver_outputs_dry(record, workflow, &outcome.output);
        return Ok(WorkflowRun {
            output: outcome.output,
            pending_approvals: outcome.pending_approvals,
            deliveries,
            cancelled: false,
            nodes,
            notices: notices.take(),
            // Issue #661 (M5): empty by construction, not by this line. A dry run's
            // bundle wires `DryRunAgent`, so `HarnessAgentRunner` — the only thing
            // that ever takes a board claim or drains one — is never built. Taken
            // rather than hard-coded empty so the claim is a *test's* to make (see
            // `a_dry_run_of_a_spawning_graph_writes_no_card`), following #542.
            board: board.take(),
            // Issues #881 / #880: empty by the same construction. `DryRunAgent`
            // runs no turn, so no tool call is gated, so nothing blocks and
            // nothing parks. Taken rather than hard-coded for the same reason.
            blocked_nodes: blocks.take(),
            approvals: approvals.take(),
        });
    }

    // Route every reached `output` node's report to its configured destination.
    // Deliberately here rather than in the HTTP handler: the orchestrator's
    // `run_workflow` tool and the trigger scheduler drive this same path, and a
    // scheduled run is exactly the case where nobody is watching the console's
    // run-result drawer. Never fails the run — each attempt is reported instead.
    //
    // Issue #438: the per-lineage half of the guard. `delivered_in_input` is
    // what a *continuation* must NOT send again — read off the trigger input,
    // where the approval that started this run threaded it. Empty on a run nobody
    // resumed.
    //
    // Issue #529: unioned with the durable half. A crashed run journals its
    // sends write-behind but never its finish, so its deliveries are stranded in
    // the journal with nothing on any trigger input to carry them. An
    // *independent* re-run (the operator pressing Run again, or a schedule
    // firing) has an empty per-lineage ledger and would re-mail every already-
    // sent report. `delivered_by_unsettled_runs` folds those stranded deliveries
    // back so the re-run skips them. Consulted only when the journal is wired
    // (`events` is `Some`) — the default build and every unwired test degrade to
    // the pre-#529 per-lineage behaviour, delivering as before.
    let mut already_delivered = crate::runtime::workflow_resume::delivered_in_input(&trigger_input);
    if let Some(events) = events.as_ref() {
        for entry in
            crate::runtime::delivered_by_unsettled_runs(events, &record.id, &workflow.id).await
        {
            // Deduped by node — the two ledgers may name the same report, and a
            // reached node is skipped on the first match regardless.
            if !already_delivered
                .iter()
                .any(|prior| prior.node == entry.node)
            {
                already_delivered.push(entry);
            }
        }
    }
    let deliveries = super::delivery::deliver_outputs(
        delivery.as_ref(),
        record,
        workflow,
        &run_id,
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
    // a run is not asking to be asked about the gates it never reached. (A dry
    // run also never reaches here — it returned above, having parked nothing.)
    // Issue #846: what this run's `tool_call` nodes sent outside the company,
    // and what it could not record. Computed here, beside the delivery ledger
    // and for the same reason — the card an approval is decided from has to
    // carry both, or approving repeats one of them.
    //
    // Only when the run actually paused: a run that reached the end has no
    // continuation coming, so there is nothing to guard against and nothing to
    // warn about.
    let (performed, unreplayable) = if outcome.pending_approvals.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        super::replay::outward_calls_performed(&graph, &outcome.output)
    };
    for call in &unreplayable {
        tracing::warn!(
            company = %record.id,
            workflow = %workflow.id,
            %run_id,
            node = %call.node_id,
            tool = %call.slug,
            why = call.why,
            "workflow: an outward call this run made cannot be replayed, so approving a gate \
             below it will repeat it"
        );
        notices.push(call.notice());
    }
    park_pending_gates(
        delivery.as_ref(),
        record,
        &workflow.id,
        &run_id,
        PausedGates {
            trigger_input: &trigger_input,
            pending: &outcome.pending_approvals,
            deliveries: &deliveries,
            performed: &performed,
            gated: &gated,
            graph: &graph,
            // Issue #596: the reached-node output + the graph's edges, so each
            // parked gate can carry the verbatim upstream content awaiting
            // sign-off.
            output: &outcome.output,
            edges: &workflow.edges,
        },
    )
    .await;

    // Issue #596: persist this settled run's per-node output durably (normal
    // completion AND the paused-with-`pending_approvals` case both reach here).
    // Best-effort, upstream of the WorkflowRun below so console/scheduled/
    // agent-tool runs all persist through this one site.
    //
    // Issue #1008: a clean settle carries the real `outcome.output`, so it
    // persists that canonical map with `partial = false`; a failed write adds an
    // operator notice (Part 6) since this arm returns a `WorkflowRun`.
    let raw_nodes = outcome.output.get("nodes").cloned().unwrap_or(Value::Null);
    if !persist_run_output(
        run_output_store.as_deref(),
        &record.id,
        &workflow.id,
        &run_id,
        &raw_nodes,
        false,
    )
    .await
    {
        notices.push(run_output_persist_failed_notice());
    }

    // Issue #881: a node can block and the run still reach here — an author who
    // wrote `on_error = "continue"` or `"route"` asked for the branch to survive
    // the block, and gets it. The run-level record stays truthful either way, so
    // the post-pass runs on this arm too rather than only on the halted one.
    let mut nodes = nodes;
    let mut pending_approvals = outcome.pending_approvals;
    let blocked_nodes = blocks.take();
    reclassify_blocked(&mut nodes, &mut pending_approvals, &blocked_nodes);
    // Issue #900: `blocked_run` (the halt arm) tells the operator what blocked
    // via a `notices` sentence, not only via the node's own status — the
    // per-node chip is easy to miss on a run that otherwise looks fine, and a
    // continued run finishing "green" beside a blocked node is exactly that
    // case. Same sentence, same source (`blocked_notice`), so the two arms
    // cannot drift into disagreement about what a block reads as.
    for b in &blocked_nodes {
        notices.push(blocked_notice(b));
    }

    Ok(WorkflowRun {
        output: outcome.output,
        pending_approvals,
        deliveries,
        cancelled: false,
        nodes,
        // Issue #638: whatever the nodes had to tell the operator. Empty for
        // every run that did not overflow the approval cap, which is nearly all
        // of them.
        notices: notices.take(),
        // Issue #661 (M5): every card this run's nodes opened or re-owned. Empty
        // for every run whose nodes touched no card, which is nearly all of them.
        board: board.take(),
        blocked_nodes,
        // Issue #880: every approval this run's nodes parked, whether or not
        // any node blocked. A run can park a card and still finish — the
        // author's `on_error` decides — and the receipt is owed in both cases.
        approvals: approvals.take(),
    })
}

/// Whether every node that reported an error is one the host blocked.
///
/// The guard on reclassifying an engine failure as a block (issue #881). A run
/// where a blocked node is joined by a genuinely broken one must still report
/// the failure: hiding a real error behind "waiting on approval" is the same
/// class of lie #881 exists to remove, pointed the other way.
///
/// Requires **at least one** errored row before it will vouch for the
/// reclassification (issue #900). `Iterator::all` is vacuously `true` on an
/// empty iterator, so without this an engine failure that named no node at
/// all — a setup or validation error the engine raised before any node ran —
/// would satisfy the check by default and get relabelled as a plain block,
/// dropping the real failure exactly as the doc comment above says this guard
/// exists to prevent.
fn only_blocked_nodes_errored(
    nodes: &[crate::ports::WorkflowRunNodeRow],
    blocked: &[crate::ports::WorkflowBlockedNode],
) -> bool {
    let mut errored = nodes
        .iter()
        .filter(|row| row.status == WorkflowNodeStatus::Error)
        .peekable();
    errored.peek().is_some() && errored.all(|row| blocked.iter().any(|b| b.node_id == row.node_id))
}

/// Reclassifies a blocked node's row and lists it as something the run is
/// waiting on (issue #881).
///
/// Host-side on purpose. The engine reported `Error` and that report is honest —
/// the capability *did* return an error, which is what halted the branch. What
/// the engine cannot know is *why*, so the host, which does, relabels the row
/// on the way out. The
/// [`ExecutionStep` → `NodeProgress`](ProgressObserver::on_step_finish) mapping
/// is deliberately left alone: it is the engine's own account of what happened,
/// and rewriting it there would make the live progress frames disagree with the
/// engine.
///
/// The blocked ids are **unioned into** `pending_approvals` rather than replacing
/// it: a run can both pause at a `requires_approval` gate and block an agent
/// node, and the console renders every entry as a node name.
fn reclassify_blocked(
    nodes: &mut [crate::ports::WorkflowRunNodeRow],
    pending_approvals: &mut Vec<String>,
    blocked: &[crate::ports::WorkflowBlockedNode],
) {
    if blocked.is_empty() {
        return;
    }
    for row in nodes.iter_mut() {
        if blocked.iter().any(|b| b.node_id == row.node_id) {
            row.status = WorkflowNodeStatus::Blocked;
        }
    }
    for b in blocked {
        if !pending_approvals.contains(&b.node_id) {
            pending_approvals.push(b.node_id.clone());
        }
    }
}

/// What a run halted by a blocked node settles with (issue #881).
///
/// Bundled rather than passed as five arguments to [`blocked_run`], the same
/// choice [`super::caps::RunContext`] makes and for the same reason: every field
/// is one run's, and none of them means anything without the others.
struct BlockedRun {
    nodes: Vec<crate::ports::WorkflowRunNodeRow>,
    blocked: Vec<crate::ports::WorkflowBlockedNode>,
    notices: super::caps::RunNotices,
    board: Vec<crate::ports::WorkflowRunBoardRow>,
    approvals: Vec<crate::ports::WorkflowRunApprovalRow>,
}

/// Settles a run that stopped because a node is waiting on an operator (issue
/// #881).
///
/// **`Ok`, and no `error`.** The engine returned `Err`, because a capability
/// error under the default `on_error = "stop"` is how a branch halts — but a
/// node waiting for a human is not a node that failed, and journalling it as one
/// would put every blocked run in the failure count and hide real failures among
/// them. This is precisely the reclassification
/// [`WorkflowRun::cancelled`](crate::ports::WorkflowRun) already performs for a
/// deliberate stop: "a cancelled run is not a failed one", and neither is a
/// blocked one.
///
/// Each emptiness below is a claim rather than a shrug:
///
/// * **no `output`** — the engine returned an error, not a final state. There is
///   no partial state to report, and the per-node output snapshot the console
///   inspector reads is likewise not written: nothing settled to persist. The
///   blocked node in particular produced nothing, which is the entire point;
/// * **no `deliveries`** — `deliver_outputs` runs off the settled output, which
///   does not exist here. An absent row already means "not reached" everywhere
///   else, and a run that stopped short must not mail anybody a report of work
///   it did not finish.
///
/// `notices` carries the operator-facing sentence, composed from the structural
/// blocked rows so the wording lives in one place and no model prose or store
/// error text can ride it.
fn blocked_run(settled: BlockedRun) -> WorkflowRun {
    let BlockedRun {
        mut nodes,
        blocked,
        notices,
        board,
        approvals,
    } = settled;
    for b in &blocked {
        notices.push(blocked_notice(b));
    }
    let mut pending_approvals = Vec::new();
    reclassify_blocked(&mut nodes, &mut pending_approvals, &blocked);
    WorkflowRun {
        output: Value::Null,
        pending_approvals,
        deliveries: Vec::new(),
        cancelled: false,
        nodes,
        notices: notices.take(),
        board,
        blocked_nodes: blocked,
        approvals,
    }
}

/// The operator's sentence for one blocked node (issue #881).
///
/// Composed here, from the structural row, rather than lifted off the
/// capability's error string: that string reaches host logs, and the two
/// audiences want different things. Worded as a **receipt** — "parked N
/// approvals", never "waiting on N" — because a settle-time count of what is
/// still outstanding is stale the moment the operator approves one, while a
/// record of what this run parked is true forever.
pub(super) fn blocked_notice(blocked: &crate::ports::WorkflowBlockedNode) -> String {
    let tools = if blocked.tools.is_empty() {
        "a tool call".to_string()
    } else {
        blocked
            .tools
            .iter()
            .map(|t| format!("\"{t}\""))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let parked = blocked.approval_ids.len();
    let mut tail = String::new();
    if parked > 0 {
        tail.push_str(&format!(
            " It parked {parked} approval{}; decide {} in Approvals and run the workflow again.",
            if parked == 1 { "" } else { "s" },
            if parked == 1 { "it" } else { "them" }
        ));
    }
    if blocked.unparkable > 0 {
        tail.push_str(&format!(
            " {} call{} could not be queued for approval at all, so you will not be asked about \
             {}.",
            blocked.unparkable,
            if blocked.unparkable == 1 { "" } else { "s" },
            if blocked.unparkable == 1 {
                "it"
            } else {
                "them"
            }
        ));
    }
    format!(
        "The step \"{}\" needed your approval for {tools}, so it produced nothing and the steps \
         after it did not run.{tail}",
        blocked.node_id
    )
}

/// Persists a run's per-node output to the durable, console-facing store (issue
/// #596; failure/blocked capture added in #1008), best-effort.
///
/// One helper, called from every outcome-bearing arm of `run_workflow_inner`, so
/// the bounding + write live in exactly one place. `store` is `None` on the
/// default build and every unwired test — then this is a no-op. A write failure
/// is logged at `warn` and never fails the run: the run's work is already done
/// and correct, and losing an inspector snapshot must not discard it.
///
/// The caller hands `raw_nodes` — the `{ "<id>": { "items": [ … ] } }` map — in
/// directly: on the clean arms that is `outcome.output["nodes"]` (the same
/// capture the in-process
/// [`RunOutputCache`](crate::harness::orchestrator::RunOutputCache) reads), and
/// on the failure/blocked arms (issue #1008) it is the map the progress observer
/// accumulated, flagged `partial`. Bounding happens inside
/// [`WorkflowRunOutputRecord::from_raw_nodes`](crate::ports::WorkflowRunOutputRecord::from_raw_nodes),
/// which clips (never refuses) so the durable record always exists.
///
/// Returns `true` when the snapshot is safely stored (or there is no store to
/// store it in — a no-op is not a failure), and `false` only when a store was
/// present and the write errored. The notice-bearing callers (which return a
/// `WorkflowRun`) use `false` to add an operator-facing notice; the genuine-`Err`
/// caller has no `WorkflowRun` to hang one on and lets the `warn!` stand alone
/// (issue #1008, Part 3).
async fn persist_run_output(
    store: Option<&dyn crate::ports::run_output::WorkflowRunOutputStore>,
    company: &CompanyId,
    workflow_id: &str,
    run_id: &str,
    raw_nodes: &Value,
    partial: bool,
) -> bool {
    let Some(store) = store else {
        return true;
    };
    let record = crate::ports::WorkflowRunOutputRecord::from_raw_nodes(
        run_id,
        workflow_id,
        crate::ports::now_millis(),
        raw_nodes,
        partial,
    );
    match store.put_run_output(company, &record).await {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!(
                company = %company,
                workflow = %workflow_id,
                %run_id,
                %err,
                "workflow: could not persist the run's per-node output; the run is unaffected"
            );
            false
        }
    }
}

/// The operator-facing notice a notice-bearing arm adds when
/// [`persist_run_output`] could not write the snapshot (issue #1008, Part 3).
///
/// The run's result itself is unaffected — this only warns that reopening the
/// run later may show no output for its nodes, so the black-box silence has a
/// visible cause instead of masquerading as "this run predates output capture".
fn run_output_persist_failed_notice() -> String {
    "This run's per-node output could not be saved, so reopening it later may show no output \
     for some steps. The run itself was unaffected."
        .to_string()
}

/// What a settled run left for the operator to decide.
///
/// Grouped rather than passed as four more parameters because they only make
/// sense together: the gates the engine paused on, the input a continuation has
/// to be started with, what this run already delivered (so approving does not
/// re-send it), and which of those gates the company's policy raised rather
/// than an author (issue #460), so the card can name the call.
struct PausedGates<'a> {
    /// The trigger payload the paused run was started with.
    trigger_input: &'a Value,
    /// The node ids the engine reported on `pending_approvals`.
    pending: &'a [String],
    /// What this run actually routed (issue #438).
    deliveries: &'a [crate::ports::DeliveryReport],
    /// What this run already sent outside the company (issue #846), so
    /// approving replays it rather than repeating it.
    performed: &'a [crate::runtime::workflow_resume::PerformedCall],
    /// The policy-raised gates, so a card can say which tool and **why**. An
    /// authored gate has no entry here — nobody stated a reason — and issue #846
    /// reads its call off the graph instead, so the card still names it.
    gated: &'a [super::gate::GatedCall],
    /// The run's graph, for the authored-gate description above (issue #846).
    graph: &'a tinyflows::model::WorkflowGraph,
    /// Issue #596: the run's reached-node output and the graph's edges, so a
    /// parked gate's card can carry the verbatim upstream content awaiting
    /// sign-off. Additive to the #460 struct — the pre-existing fields are
    /// untouched.
    output: &'a Value,
    edges: &'a [crate::company::WorkflowEdgeDef],
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
    paused: PausedGates<'_>,
) {
    let PausedGates {
        trigger_input,
        pending,
        deliveries,
        performed,
        gated,
        graph,
        // Issue #596: the reached-node output + the graph's edges, so each parked
        // gate's card can carry the verbatim upstream content awaiting sign-off.
        output,
        edges,
    } = paused;
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

    // Issue #978: the gates this lineage has already refused. A denied node is
    // decided and final — replaying into it must not raise the question a second
    // time, or a mixed verdict still nets new cards and "approving never
    // increases pending approvals" is false again.
    let denied = crate::runtime::workflow_resume::denied_in_input(trigger_input);

    // Issue #978: every gate this run parks shares ONE turn key, so the N of a
    // fan-out are one decision batch owed exactly one continuation. Keyed on the
    // run because the run is what gets re-dispatched.
    let turn = crate::runtime::workflow_resume::workflow_turn_key(run_id);

    for node_id in pending {
        if denied.iter().any(|refused| refused == node_id) {
            tracing::info!(
                company = %record.id,
                workflow = %workflow_id,
                node = %node_id,
                %run_id,
                "workflow: this gate was already refused, so it is not asked about again"
            );
            continue;
        }
        // Issue #460: when the policy is what stopped this node, the card says
        // which tool and why.
        //
        // Issue #846: when the **author** stopped it, the card still says which
        // tool — read off the graph, which has known the node's slug and
        // arguments all along. Only the reason is policy-specific, and it is the
        // one thing an authored gate genuinely does not have. Falling back rather
        // than merging: a policy-raised gate already carries the same call, so
        // consulting the graph for it would be a second answer to a question that
        // already has one.
        let described;
        let gate = match gated.iter().find(|gate| gate.node_id == *node_id) {
            Some(gate) => Some(gate),
            None => {
                described = super::gate::describe_call(graph, node_id);
                described.as_ref()
            }
        };
        let call = gate.map(|gate| crate::runtime::workflow_resume::GateCall {
            tool: gate.slug.as_str(),
            // Empty means "nobody wrote one", which `describe_call` documents;
            // the key is then absent from the payload rather than present and
            // blank, so a console can tell an unstated reason from an empty one.
            reason: Some(gate.reason.as_str()).filter(|reason| !reason.is_empty()),
            args: Some(&gate.args),
            target: gate.target.as_deref(),
        });
        let mut effect = crate::runtime::workflow_resume::gate_effect(
            workflow_id,
            node_id,
            trigger_input,
            run_id,
            deliveries,
            performed,
            call,
        );
        // Issue #596: enrich the card with the verbatim output of this gate's
        // upstream nodes — the content awaiting sign-off. A self-contained
        // addition on top of the effect `gate_effect` already built; the dedupe
        // below keys on explicit payload keys only (NOT this content), so two
        // parks differing only in content still collapse to one card.
        crate::runtime::workflow_resume::attach_upstream_content(
            &mut effect,
            output,
            edges,
            node_id,
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
                Some(turn.clone()),
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

/// What a run stopped by an operator settles with on the **hard-abort** arm
/// (issue #383/#398) — the wedged-node path, where the engine future was dropped
/// because the run did not wind down within `CANCEL_HARD_ABORT_GRACE`.
///
/// (A run that stopped **cleanly** at a node boundary does not come here: it has
/// a real partial outcome and is settled inline in `run_workflow_inner`, carrying
/// its collected node rows. This is only the dropped-future case.)
///
/// Empty on every field but the flag and the two the caller threads in, and each
/// emptiness is a claim rather than a shrug:
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
///
/// # The two arguments are the exceptions, and they are the point
///
/// `notices` and `board` are **threaded in rather than emptied** (issue #661 /
/// M5). Everything above is empty because it describes the run's *result*, which
/// a dropped future does not have. These two describe what its nodes already
/// **did** before it wedged, and both are durable facts by the time this is
/// reached: a notice records tool calls that were already refused, and a board row
/// records a card that is already on the operator's board. Emptying them would
/// leave a card nothing admits to opening — which is why this signature changed
/// instead of the constructor keeping its convenient `Vec::new()`s. (`notices` was
/// dropped here before, silently; that is fixed by the same change.)
fn cancelled_run(
    notices: Vec<String>,
    board: Vec<crate::ports::WorkflowRunBoardRow>,
    approvals: Vec<crate::ports::WorkflowRunApprovalRow>,
) -> WorkflowRun {
    WorkflowRun {
        output: Value::Null,
        pending_approvals: Vec::new(),
        deliveries: Vec::new(),
        cancelled: true,
        // Empty for the same reason as the fields above: a stopped run reports
        // no result. Its completed nodes were still journaled as they finished
        // (the drain runs before this returns), so "how far did it get?" is
        // answered by the history, not by this settled body.
        nodes: Vec::new(),
        notices,
        board,
        // Issue #881: a hard abort drops the engine future mid-await, so no
        // node ever reported a block — and a block is reported only by a node
        // that finished its turn. Empty by construction, not by omission.
        blocked_nodes: Vec::new(),
        // Issue #880: threaded in, NOT emptied, for exactly the reason `board`
        // is. An approval card is durable the moment it is written, so a run
        // that parked two and was then hard-aborted really did park them —
        // zeroing the rows would leave two cards on the operator's Approvals
        // page that no run admits to opening.
        approvals,
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
    use crate::ports::run_output::WorkflowRunOutputStore;
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
            overlay_policy: None,
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
        }
    }

    fn deps(dir: &std::path::Path) -> HarnessDeps {
        HarnessDeps {
            ledgers: None,
            ledger_registry: Default::default(),
            run_supervisor: crate::runtime::RunSupervisor::default(),
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            context: Arc::new(FsContextStore::new(dir)),
            store: Arc::new(FsCompanyStore::new(dir)),
            meter: Some(Arc::new(FsOps::new(dir))),
            workspace_root: dir.to_path_buf(),
            workspace_git_enabled: false,
            audit_root: dir.to_path_buf(),
            model_override: None,
            tasks: None,
            artifacts: None,
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            default_mcp_servers: Vec::new(),
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: crate::harness::orchestrator::DelegationQueue::default(),
            workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_revisions: None,
            approval_requests: crate::harness::policy::ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            #[cfg(feature = "chargebee")]
            chargebee: None,
            #[cfg(feature = "paypal")]
            paypal: None,
            hosting: None,
            steer: crate::company::steer::InflightRegistry::default(),
            delivery: None,
            search: None,
            workspace: None,
            repos: None,
            repo_bindings: Vec::new(),
            checkouts: crate::harness::repo::CheckoutLedger::default(),
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
            overlay_policy: None,
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
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

    // --- Durable per-node output persist-at-settle (issue #596) ---------------

    /// A completed run persists its per-node output to the durable store, so a
    /// later console read can show what each node produced. The agent node's
    /// text is present in the stored snapshot.
    #[tokio::test]
    async fn a_completed_run_persists_its_per_node_output() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FsOps::new(dir.path()));
        let pool = Arc::new(HarnessPool::new());
        let rec = record();
        let mut deps = deps(dir.path());
        deps.run_output_store = Some(store.clone());
        // The GREET graph has an agent node, so the roster must be resident and
        // the record loadable — exactly like `agent_node_runs_on_the_harness_pool`.
        pool.ensure(&rec, &deps).await.expect("roster builds");
        let file = parse_workflow(GREET).expect("parses");
        let ctx = WorkflowRunContext::new(false);
        let run_id = ctx.run_id.clone();

        run_workflow(
            pool,
            deps,
            &rec,
            &file,
            serde_json::json!({ "brief": "launch" }),
            &ctx,
        )
        .await
        .expect("workflow runs");

        let stored = store
            .get_run_output(&rec.id, &run_id)
            .await
            .expect("store read")
            .expect("a completed run must persist its output");
        assert_eq!(stored.workflow_id, "greet");
        assert_eq!(stored.run_id, run_id);
        assert!(
            stored.nodes.to_string().contains("hello-marker"),
            "the agent node's produced text must be in the durable snapshot: {}",
            stored.nodes
        );
    }

    /// A paused (`requires_approval`) run still settles with an outcome and so
    /// persists the output of the nodes it reached before the gate.
    #[tokio::test]
    async fn a_paused_run_persists_the_output_it_reached() {
        let src = r#"
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
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FsOps::new(dir.path()));
        let mut deps = deps(dir.path());
        deps.run_output_store = Some(store.clone());
        let rec = tools_record();
        let file = parse_workflow(src).expect("parses");
        let ctx = WorkflowRunContext::new(false);
        let run_id = ctx.run_id.clone();

        let run = run_workflow(
            Arc::new(HarnessPool::new()),
            deps,
            &rec,
            &file,
            serde_json::json!({ "seed": 1 }),
            &ctx,
        )
        .await
        .expect("run pauses cleanly");
        assert!(run.pending_approvals.iter().any(|id| id == "gate"));

        assert!(
            store
                .get_run_output(&rec.id, &run_id)
                .await
                .unwrap()
                .is_some(),
            "a paused-with-pending-approvals run must persist its reached output"
        );
    }

    /// Issue #661 (M5): the **hard-abort** arm lists the board writes its nodes
    /// already performed.
    ///
    /// A wedged run's future is dropped, so there is no outcome to read and every
    /// other field of `cancelled_run` is empty as a claim about the run's
    /// *result*. Its board rows are not a result — they record a card that is
    /// already on the operator's board by the time this is reached. Emptying them
    /// would leave a card that no run admits to opening.
    ///
    /// Unit-level rather than a wedged end-to-end run on purpose: reaching this
    /// arm for real means outlasting `CANCEL_HARD_ABORT_GRACE`, and a five-second
    /// sleep in the suite buys nothing this does not pin — the arm's whole
    /// behaviour is what it threads through.
    ///
    /// `notices` rides along for the same reason, and that half is a fix: this
    /// constructor used to hard-code `Vec::new()` for it, so a wedged run silently
    /// dropped notices its completed nodes had raised.
    #[test]
    fn a_hard_aborted_run_still_lists_its_board_writes() {
        let row = crate::ports::WorkflowRunBoardRow {
            action: crate::ports::WorkflowBoardAction::Spawned,
            task_id: Some("card-1".to_string()),
            title: Some("Reply to the auditor".to_string()),
            assignee: None,
        };
        // Issue #880: a parked approval is threaded in for the same reason the
        // board row is — the card is already on the operator's Approvals page,
        // so a hard abort must not un-say that the run opened it.
        let parked = crate::ports::WorkflowRunApprovalRow {
            node_id: Some("work".to_string()),
            tool: Some("publish_artifact".to_string()),
            outcome: crate::ports::WorkflowApprovalOutcome::Parked,
            approval_id: Some("appr-1".to_string()),
        };
        let run = cancelled_run(
            vec!["something was discarded".to_string()],
            vec![row.clone()],
            vec![parked.clone()],
        );

        assert_eq!(
            run.approvals,
            vec![parked],
            "a run stopped after parking an approval really did park it; zeroing the receipt \
             would leave a card no run admits to opening"
        );

        assert!(run.cancelled);
        assert_eq!(
            run.board,
            vec![row],
            "the card is durable, so the stopped run must still list it"
        );
        assert_eq!(
            run.notices,
            vec!["something was discarded".to_string()],
            "and the notices its nodes raised are not the run's result either"
        );
        // Everything that IS the run's result stays empty, unchanged.
        assert_eq!(run.output, Value::Null);
        assert!(run.deliveries.is_empty());
        assert!(run.pending_approvals.is_empty());
        assert!(run.nodes.is_empty());
    }

    /// Issue #900's regression: `Iterator::all` is vacuously `true` on an
    /// empty iterator, so before this guard required at least one errored row,
    /// an engine failure that named no node at all satisfied
    /// `only_blocked_nodes_errored` by default and would have been
    /// relabelled as a plain block — exactly the "hide a real error behind
    /// waiting on approval" lie the function's own doc comment says it exists
    /// to prevent.
    #[test]
    fn no_errored_nodes_never_counts_as_only_blocked_nodes_errored() {
        let blocked = vec![crate::ports::WorkflowBlockedNode {
            node_id: "work".to_string(),
            tools: vec!["shell".to_string()],
            approval_ids: vec!["appr-1".to_string()],
            unparkable: 0,
        }];
        // No node row reported `Error` at all — a setup/validation failure the
        // engine raised before any node ran, for instance.
        let nodes: Vec<crate::ports::WorkflowRunNodeRow> = Vec::new();
        assert!(
            !only_blocked_nodes_errored(&nodes, &blocked),
            "an engine error naming no errored node must never be waved through as \
             a plain block"
        );
    }

    /// The guard's positive case still holds: when every errored row is one the
    /// host blocked, reclassification is safe.
    #[test]
    fn every_errored_node_blocked_counts_as_only_blocked_nodes_errored() {
        let blocked = vec![crate::ports::WorkflowBlockedNode {
            node_id: "work".to_string(),
            tools: vec!["shell".to_string()],
            approval_ids: vec!["appr-1".to_string()],
            unparkable: 0,
        }];
        let nodes = vec![crate::ports::WorkflowRunNodeRow {
            node_id: "work".to_string(),
            status: WorkflowNodeStatus::Error,
            elapsed_ms: 10,
        }];
        assert!(only_blocked_nodes_errored(&nodes, &blocked));
    }

    /// The guard's whole reason to exist: a genuinely broken node alongside a
    /// blocked one must still fail the check, so the real error is not hidden.
    #[test]
    fn a_genuinely_errored_node_alongside_a_blocked_one_fails_the_guard() {
        let blocked = vec![crate::ports::WorkflowBlockedNode {
            node_id: "work".to_string(),
            tools: vec!["shell".to_string()],
            approval_ids: vec!["appr-1".to_string()],
            unparkable: 0,
        }];
        let nodes = vec![
            crate::ports::WorkflowRunNodeRow {
                node_id: "work".to_string(),
                status: WorkflowNodeStatus::Error,
                elapsed_ms: 10,
            },
            crate::ports::WorkflowRunNodeRow {
                node_id: "other".to_string(),
                status: WorkflowNodeStatus::Error,
                elapsed_ms: 5,
            },
        ];
        assert!(
            !only_blocked_nodes_errored(&nodes, &blocked),
            "a genuinely broken node must not be masked by an unrelated block"
        );
    }

    /// A dry run writes NOTHING durable — no output snapshot, matching its "the
    /// settled response body is the whole record" contract (#542).
    #[tokio::test]
    async fn a_dry_run_persists_no_output() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FsOps::new(dir.path()));
        let pool = Arc::new(HarnessPool::new());
        let rec = record();
        let mut deps = deps(dir.path());
        deps.run_output_store = Some(store.clone());
        pool.ensure(&rec, &deps).await.expect("roster builds");
        let file = parse_workflow(GREET).expect("parses");
        let mut ctx = WorkflowRunContext::new(false);
        ctx.dry_run = true;
        let run_id = ctx.run_id.clone();

        run_workflow(
            pool,
            deps,
            &rec,
            &file,
            serde_json::json!({ "brief": "launch" }),
            &ctx,
        )
        .await
        .expect("dry run completes");

        assert!(
            store
                .get_run_output(&rec.id, &run_id)
                .await
                .unwrap()
                .is_none(),
            "a dry run must persist nothing durable"
        );
    }

    /// Issue #1008 (the decisive change): a run whose first node SUCCEEDS and a
    /// later node FAILS hard (default `on_error = "stop"`, so the engine returns
    /// `Err`) must STILL persist the per-node output the observer captured before
    /// the failure — flagged `partial`. Before #1008 this arm returned `Err`
    /// without persisting anything, so the inspector wrongly claimed the run
    /// predated output capture.
    ///
    /// `start → export (csv_export, succeeds) → boom (bogus_tool, unknown slug →
    /// hard stop)`: the export node's frame is captured off the progress
    /// observer, and the run fails on `boom`.
    #[tokio::test]
    async fn a_failed_run_persists_the_partial_output_it_reached() {
        let src = r#"
id = "partial_fail"
name = "Partial fail"
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
id = "boom"
kind = "tool_call"
name = "Boom"
[node.config]
slug = "bogus_tool"
[[edge]]
from = "start"
to = "export"
[[edge]]
from = "export"
to = "boom"
"#;
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FsOps::new(dir.path()));
        let mut deps = deps(dir.path());
        deps.run_output_store = Some(store.clone());
        let rec = tools_record();
        let file = parse_workflow(src).expect("parses");
        let ctx = WorkflowRunContext::new(false);
        let run_id = ctx.run_id.clone();

        let err = run_workflow(
            Arc::new(HarnessPool::new()),
            deps,
            &rec,
            &file,
            serde_json::json!({ "seed": 1 }),
            &ctx,
        )
        .await
        .expect_err("the run fails hard on the unknown-slug node");
        assert!(
            err.to_string().contains("bogus_tool") || err.to_string().contains("boom"),
            "the failure should come from the failing node: {err}"
        );

        // The decisive assertion: the snapshot exists (was `None` pre-#1008,
        // because the `Err` arm persisted nothing).
        let stored = store
            .get_run_output(&rec.id, &run_id)
            .await
            .expect("store read")
            .expect("a failed run must still persist the output it reached before failing");
        assert!(
            stored.partial,
            "a failure-arm capture must be flagged partial: {stored:?}"
        );
        assert_eq!(stored.workflow_id, "partial_fail");
        assert_eq!(stored.run_id, run_id);
        // The successful `export` node's output is present under the canonical
        // `{ items: [...] }` shape the console renders.
        assert!(
            stored
                .nodes
                .get("export")
                .and_then(|n| n.get("items"))
                .is_some(),
            "the node that succeeded before the failure must be in the partial snapshot: {}",
            stored.nodes
        );
    }

    // --- Output destinations, end to end (issue #170) ------------------------

    /// A graph whose terminal `output` node routes its report to a desk
    /// channel. `trigger → output` only, so it needs no roster.
    const REPORT_TO_DESK: &str = r#"
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
target = "engineering"
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
        use crate::runtime::channel::RecordingChannel;

        let dir = tempfile::tempdir().unwrap();
        let channel = RecordingChannel::new("engineering");
        let mut deps = deps(dir.path());
        deps.delivery = Some(crate::workflows::WorkflowDeliveryDeps {
            mail: None,
            inbox: Arc::new(crate::store::FsInboxStore::new(dir.path())),
            users: Arc::new(FsOps::new(dir.path())),
            bootstrap_admin: None,
            channels: vec![Arc::new(channel.clone())],
            // This case delivers to a channel, which never parks.
            parking: None,
            events: Arc::new(crate::store::FsEventLog::new(dir.path())),
        });

        let file = parse_workflow(REPORT_TO_DESK).expect("parses");
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
        let file = parse_workflow(REPORT_TO_DESK).expect("parses");
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
            global: false,
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

    /// T-output_parser AUTO-FIX (issue #661, M4) — the vendored-engine drift
    /// catcher. With `auto_fix` DEFAULTED (true) and no roster LLM wired, a
    /// schema failure sends the engine to the `llm` capability to *repair* the
    /// value. The unwired `llm` must surface the SCHEMA failure, so the
    /// `on_error = continue` error item names the missing property — NOT the
    /// generic "no roster agent" message that used to mask it.
    ///
    /// This exercises the real request the engine builds
    /// (`task = "coerce_to_schema"` with the schema `errors`), so a future
    /// tinyflows pin that reshapes that request fails here rather than silently
    /// reverting to the masked message.
    #[tokio::test]
    async fn t_output_parser_auto_fix_surfaces_schema_failure_not_no_roster_agent() {
        // Note: NO `auto_fix = false` — the default (true) is exactly the path
        // that reaches the `llm` auto-fix capability.
        let src = r#"
id = "op_af_wf"
name = "Parser Auto-fix WF"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "parse"
kind = "output_parser"
name = "Parse"
on_error = "continue"
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
        let run = run_src(dir.path(), src, serde_json::json!({ "other": 1 }))
            .await
            .expect("run completes despite the schema failure");

        let message = run.output["nodes"]["parse"]["items"][0]["json"]["error"]["message"]
            .as_str()
            .unwrap_or_else(|| panic!("expected a routed error item: {}", run.output));
        assert!(
            message.contains("schema validation") && message.contains("name"),
            "the auto-fix path must surface the schema failure: {message}"
        );
        assert!(
            !message.contains("no roster agent"),
            "the schema failure must not be masked by the bare-LLM message: {message}"
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

    /// A provider that records the last user message of every inference call and
    /// **holds the `slow` node open until the operator cancels** (bounded), so a
    /// cancel deterministically lands while a child `sub_workflow` node is
    /// mid-flight. It distinguishes child nodes by a marker string authored into
    /// each node's `prompt`: the node after `slow` must never be invoked once a
    /// parent cancel has propagated into the child run.
    struct RecordingSlowProvider {
        seen: Arc<std::sync::Mutex<Vec<String>>>,
        entered_slow: Arc<tokio::sync::Notify>,
        cancel: crate::ports::workflow_runner::RunCancel,
    }

    #[async_trait]
    impl tinyagents::harness::model::ChatModel<()> for RecordingSlowProvider {
        async fn invoke(
            &self,
            _state: &(),
            request: tinyagents::harness::model::ModelRequest,
        ) -> tinyagents::Result<tinyagents::harness::model::ModelResponse> {
            // Scan the whole conversation, not just the last user turn: the
            // openhuman harness reshapes an agent node's authored instruction
            // into a multi-message prompt, so the node's marker can land in any
            // role. Matching the joined text keeps the probe robust to that.
            let all_text = request
                .messages
                .iter()
                .map(|m| m.text())
                .collect::<Vec<_>>()
                .join("\n");
            self.seen.lock().expect("seen mutex").push(all_text.clone());
            if all_text.contains("SLOW-NODE") {
                // Announce arrival, then hold the child at this node until the run
                // is cancelled (bounded, so a broken build cannot hang CI). This
                // pins the cancel to land while `slow` is in flight and makes the
                // wind-down a clean node-boundary stop, not a hard abort.
                self.entered_slow.notify_waiters();
                tokio::select! {
                    () = self.cancel.cancelled() => {}
                    () = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                }
            }
            Ok(tinyagents::harness::model::ModelResponse::assistant(
                "acknowledged".to_string(),
            ))
        }
    }

    impl crate::harness::provider::HarnessModel for RecordingSlowProvider {
        fn telemetry_provider_id(&self) -> String {
            "recording-slow".to_string()
        }
    }

    /// **Full-stack cancel propagation into a `sub_workflow` child (issue #675).**
    /// Parent `trigger → sub_workflow(child) → done`, child
    /// `trigger → slow → marker → done` where `slow`/`marker` are agent nodes.
    /// The operator cancels while the child's `slow` node is mid-flight; the
    /// parent's `CancellationToken` must reach the child run so its `marker` node
    /// never executes, the run settles `cancelled`, and it comes back promptly
    /// (the clean node-boundary wind-down bounded by `slow`'s remainder — not the
    /// hard-abort grace). Before the fix the child ran behind a fresh token, so
    /// the cancel never crossed the boundary and `marker` executed.
    #[tokio::test]
    async fn a_parent_cancel_propagates_into_a_sub_workflow_child() {
        let source = tempfile::tempdir().unwrap();
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
id = "slow"
kind = "agent"
name = "Slow"
agent = "ceo"
summary = "SLOW-NODE hold here until cancelled"
prompt = "SLOW-NODE hold here until cancelled"
[[node]]
id = "marker"
kind = "agent"
name = "Marker"
agent = "ceo"
summary = "MARKER-NODE must never run once cancellation propagates"
prompt = "MARKER-NODE must never run once cancellation propagates"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "slow"
[[edge]]
from = "slow"
to = "marker"
[[edge]]
from = "marker"
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
        let pool = Arc::new(HarnessPool::new());
        let rec = record();
        let ctx = WorkflowRunContext::new(false);
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let entered = Arc::new(tokio::sync::Notify::new());

        let mut deps = deps_with_source(home.path(), source.path());
        deps.provider = Arc::new(RecordingSlowProvider {
            seen: seen.clone(),
            entered_slow: entered.clone(),
            cancel: ctx.cancel.clone(),
        });
        deps.provider_slug = "recording-slow".to_string();
        pool.ensure(&rec, &deps).await.expect("roster builds");

        let file = parse_workflow(parent).expect("parent parses");
        // Registered before the run starts so `slow` cannot slip past it.
        let reached_slow = entered.notified();

        let mut run = Box::pin(run_workflow(pool, deps, &rec, &file, Value::Null, &ctx));
        tokio::select! {
            _ = &mut run => panic!(
                "the run finished before `slow` was reached; provider saw: {:?}",
                seen.lock().expect("seen mutex")
            ),
            () = reached_slow => {}
        }

        // The operator presses Cancel while the child's `slow` node is in flight.
        let pressed = std::time::Instant::now();
        ctx.cancel.cancel();
        let run = tokio::time::timeout(std::time::Duration::from_secs(30), run)
            .await
            .expect("the cancelled run never returned")
            .expect("a cancelled run is Ok, not Err");
        let elapsed = pressed.elapsed();

        assert!(run.cancelled, "the run must report that it was stopped");

        let seen = seen.lock().expect("seen mutex").clone();
        assert!(
            seen.iter().any(|m| m.contains("SLOW-NODE")),
            "the in-flight child `slow` node ran: {seen:?}"
        );
        assert!(
            !seen.iter().any(|m| m.contains("MARKER-NODE")),
            "cancellation must propagate into the child: its `marker` node should never run, \
             got {seen:?}"
        );
        // A clean node-boundary wind-down bounded by `slow`'s remainder — nowhere
        // near the hard-abort grace, which only a wedged (never-returning) node
        // would reach.
        assert!(
            elapsed < CANCEL_HARD_ABORT_GRACE,
            "the child wound down cleanly, so settle time should be well under the hard-abort \
             grace; took {elapsed:?}"
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
            bootstrap_admin: None,
            channels: Vec::new(),
            parking: Some(super::super::delivery::DeliveryParking {
                approvals: gate,
                journal: journal.clone(),
                // Issue #978: a test fixture parks into its own queues. The
                // production wiring is `RuntimeBuilder`, which hands the
                // runtime's own handles in so a park arms what the resolve
                // path releases.
                continuations: Default::default(),
                gates: Default::default(),
            }),
            events: Arc::new(crate::store::FsEventLog::new(dir)),
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
            bootstrap_admin: None,
            channels: Vec::new(),
            parking: Some(super::super::delivery::DeliveryParking {
                approvals: Arc::new(crate::policy::ManifestApprovalGate::new(policy)),
                journal: journal.clone(),
                // Issue #978: a test fixture parks into its own queues. The
                // production wiring is `RuntimeBuilder`, which hands the
                // runtime's own handles in so a park arms what the resolve
                // path releases.
                continuations: Default::default(),
                gates: Default::default(),
            }),
            events: Arc::new(crate::store::FsEventLog::new(dir)),
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
        let continuation = crate::runtime::workflow_resume::continuation_input(
            &card,
            &[
                card.payload[crate::runtime::workflow_resume::PAYLOAD_NODE_ID]
                    .as_str()
                    .expect("the card names its gate")
                    .to_string(),
            ],
            &[],
        )
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

    // --- issue #529: the durable delivery ledger across a crash --------------

    /// Deps that deliver a report to the operator channel, sharing an event log
    /// and a channel across runs — so a run 1's write-behind record and the
    /// count of sends are both visible to run 2.
    fn deps_delivering_to_channel(
        dir: &std::path::Path,
        events: Arc<dyn crate::ports::EventLog>,
        channel: crate::runtime::channel::RecordingChannel,
        consult_journal: bool,
    ) -> HarnessDeps {
        let mut deps = deps(dir);
        // `consult_journal` toggles ONLY whether the runner reads the durable
        // ledger — the delivery bundle always journals write-behind to the same
        // log, so the journal state is identical between the two. This is what
        // makes the negative control a true control: same journal, guard off.
        deps.events = consult_journal.then(|| events.clone());
        deps.delivery = Some(crate::workflows::WorkflowDeliveryDeps {
            mail: None,
            inbox: Arc::new(crate::store::FsInboxStore::new(dir)),
            users: Arc::new(FsOps::new(dir)),
            bootstrap_admin: None,
            channels: vec![Arc::new(channel)],
            parking: None,
            events,
        });
        deps
    }

    /// **The headline regression for issue #529.** A run delivers a report and
    /// then crashes — `run_workflow` returns, but the caller never journals the
    /// finish (exactly what a host kill leaves). The boot sweep settles it with
    /// the synthetic interrupted finish, and an operator re-runs the workflow.
    /// The report must NOT go out a second time: the transport count stays at 1
    /// and the re-run's row reads `Skipped` / `AlreadyDelivered`.
    ///
    /// Breaking the union/fold (the durable consult in `run_workflow_inner`)
    /// turns this into the negative control below — the count becomes 2 and this
    /// assertion fails, which is what makes the guard provably load-bearing.
    #[tokio::test]
    async fn a_crashed_runs_delivery_is_not_repeated_on_an_independent_re_run() {
        use crate::runtime::channel::RecordingChannel;

        let dir = tempfile::tempdir().unwrap();
        let events: Arc<dyn crate::ports::EventLog> =
            Arc::new(crate::store::FsEventLog::new(dir.path()));
        let channel = RecordingChannel::new("engineering");
        let rec = record();
        let file = parse_workflow(REPORT_TO_DESK).expect("parses");

        // Run 1 delivers, then "crashes": run_workflow returns, but nothing
        // journals a WorkflowRunFinished for it.
        let deps1 = deps_delivering_to_channel(dir.path(), events.clone(), channel.clone(), true);
        let ctx1 = WorkflowRunContext::new(false);
        let run1 = run_workflow(
            Arc::new(HarnessPool::new()),
            deps1,
            &rec,
            &file,
            serde_json::json!({ "brief": "quarterly numbers" }),
            &ctx1,
        )
        .await
        .expect("run 1 runs");
        assert_eq!(
            run1.deliveries[0].status,
            crate::ports::DeliveryStatus::Sent
        );
        assert_eq!(channel.sent().len(), 1, "run 1 delivered exactly once");

        // The boot sweep settles the crashed run with the synthetic interrupted
        // finish — an error, which must NOT clear the durable ledger.
        crate::runtime::sweep_interrupted_runs(&events, &rec.id).await;

        // Run 2 is an independent re-run of the same workflow.
        let deps2 = deps_delivering_to_channel(dir.path(), events.clone(), channel.clone(), true);
        let ctx2 = WorkflowRunContext::new(false);
        let run2 = run_workflow(
            Arc::new(HarnessPool::new()),
            deps2,
            &rec,
            &file,
            serde_json::json!({ "brief": "quarterly numbers" }),
            &ctx2,
        )
        .await
        .expect("run 2 runs");

        assert_eq!(
            channel.sent().len(),
            1,
            "the durable ledger stopped the crashed run's report from being re-delivered"
        );
        assert_eq!(run2.deliveries.len(), 1, "{:?}", run2.deliveries);
        assert_eq!(
            run2.deliveries[0].status,
            crate::ports::DeliveryStatus::Skipped
        );
        assert_eq!(
            run2.deliveries[0].reason,
            crate::ports::DeliveryReason::AlreadyDelivered
        );
    }

    /// **The committed negative control.** The identical journal state as the
    /// test above — run 1 delivered and crashed — but run 2 runs with the
    /// durable consult bypassed (`deps.events` unwired, so the fold never runs).
    /// The report goes out a second time: the count reaches 2. This proves the
    /// guard is load-bearing rather than incidental — without the consult, the
    /// re-delivery the whole issue is about happens.
    #[tokio::test]
    async fn without_the_durable_consult_a_crashed_runs_report_is_re_delivered() {
        use crate::runtime::channel::RecordingChannel;

        let dir = tempfile::tempdir().unwrap();
        let events: Arc<dyn crate::ports::EventLog> =
            Arc::new(crate::store::FsEventLog::new(dir.path()));
        let channel = RecordingChannel::new("engineering");
        let rec = record();
        let file = parse_workflow(REPORT_TO_DESK).expect("parses");

        let deps1 = deps_delivering_to_channel(dir.path(), events.clone(), channel.clone(), true);
        let ctx1 = WorkflowRunContext::new(false);
        run_workflow(
            Arc::new(HarnessPool::new()),
            deps1,
            &rec,
            &file,
            serde_json::json!({ "brief": "quarterly numbers" }),
            &ctx1,
        )
        .await
        .expect("run 1 runs");
        assert_eq!(channel.sent().len(), 1);
        crate::runtime::sweep_interrupted_runs(&events, &rec.id).await;

        // Run 2: SAME journal, but the guard is off (`consult_journal = false`).
        let deps2 = deps_delivering_to_channel(dir.path(), events.clone(), channel.clone(), false);
        let ctx2 = WorkflowRunContext::new(false);
        let run2 = run_workflow(
            Arc::new(HarnessPool::new()),
            deps2,
            &rec,
            &file,
            serde_json::json!({ "brief": "quarterly numbers" }),
            &ctx2,
        )
        .await
        .expect("run 2 runs");

        assert_eq!(
            channel.sent().len(),
            2,
            "without consulting the durable ledger, the crashed run's report goes out again"
        );
        assert_eq!(
            run2.deliveries[0].status,
            crate::ports::DeliveryStatus::Sent,
            "the unguarded re-run delivers rather than skips"
        );
    }

    /// The cadence guarantee: a run that delivers AND finishes cleanly must not
    /// suppress the next scheduled run. Run 1 delivers and its clean finish is
    /// journaled; run 2 delivers again — a daily digest keeps going out every
    /// day, because a clean finish clears the durable ledger.
    #[tokio::test]
    async fn a_clean_finish_lets_the_next_run_deliver_again() {
        use crate::runtime::channel::RecordingChannel;

        let dir = tempfile::tempdir().unwrap();
        let events: Arc<dyn crate::ports::EventLog> =
            Arc::new(crate::store::FsEventLog::new(dir.path()));
        let channel = RecordingChannel::new("engineering");
        let rec = record();
        let file = parse_workflow(REPORT_TO_DESK).expect("parses");

        // Run 1 delivers…
        let deps1 = deps_delivering_to_channel(dir.path(), events.clone(), channel.clone(), true);
        let ctx1 = WorkflowRunContext::new(false);
        let run1 = run_workflow(
            Arc::new(HarnessPool::new()),
            deps1,
            &rec,
            &file,
            serde_json::json!({ "brief": "quarterly numbers" }),
            &ctx1,
        )
        .await
        .expect("run 1 runs");
        assert_eq!(channel.sent().len(), 1);
        // …and the caller journals its clean finish, the way a real entry point
        // does once the run returns.
        crate::runtime::record_run_finished(
            &events,
            &rec.id,
            &file.id,
            true,
            &ctx1.run_id,
            Ok(&run1),
        )
        .await;

        // Run 2 (the next day's fire) delivers again — never suppressed.
        let deps2 = deps_delivering_to_channel(dir.path(), events.clone(), channel.clone(), true);
        let ctx2 = WorkflowRunContext::new(false);
        let run2 = run_workflow(
            Arc::new(HarnessPool::new()),
            deps2,
            &rec,
            &file,
            serde_json::json!({ "brief": "quarterly numbers" }),
            &ctx2,
        )
        .await
        .expect("run 2 runs");

        assert_eq!(
            channel.sent().len(),
            2,
            "a clean finish must not suppress the next legitimate delivery"
        );
        assert_eq!(
            run2.deliveries[0].status,
            crate::ports::DeliveryStatus::Sent
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
    /// start, then — for each non-trigger node, in execution order — a
    /// `WorkflowNodeStarted` immediately followed by its `WorkflowNodeFinished`
    /// (issue #382), and all of them are durable before `run_workflow` returns —
    /// so the caller's `WorkflowRunFinished` can only ever land after them.
    ///
    /// `GREET` is `start → ceo → done`; `start` is the trigger and the engine
    /// reports no step for it, so exactly two nodes are owed, each with its own
    /// started/finished pair. The **started-before-finished** ordering is the
    /// #382 invariant: both frames ride one unbounded channel and the collector
    /// drains it in order, so a node cannot settle on the journal before it
    /// opens.
    #[tokio::test]
    async fn a_run_journals_a_start_then_a_started_finished_pair_per_non_trigger_node() {
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
                CompanyEvent::WorkflowNodeStarted { node_id, .. } => format!("nodestart:{node_id}"),
                CompanyEvent::WorkflowNodeFinished { node_id, .. } => format!("node:{node_id}"),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            trail,
            vec![
                "started",
                "nodestart:ceo",
                "node:ceo",
                "nodestart:done",
                "node:done"
            ],
            "expected the run start, then a started→finished pair per non-trigger node in order"
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
                CompanyEvent::WorkflowNodeStarted {
                    run_id,
                    workflow_id,
                    ..
                } => {
                    assert_eq!(run_id, &ctx.run_id);
                    assert_eq!(workflow_id, "greet");
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
                CompanyEvent::WorkflowNodeStarted { run_id, .. } => run_id,
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

    /// **The keystone cancel test — the HARD-ABORT arm (issue #383/#398).** An
    /// operator stops a run wedged on an agent node that never returns, so it can
    /// never reach a node boundary and the clean token path (below) cannot settle
    /// it. Four things have to be true at once:
    ///
    /// 1. the run settles as `cancelled`, not as an error — a deliberate stop is
    ///    not a failure and must never land in the failure count;
    /// 2. the journal keeps a node row for the node that **completed** and none
    ///    for the one that was still executing — "how far did it get before I
    ///    stopped it" is the question the trail exists to answer, and inventing
    ///    a row for the wedged node would answer it wrongly;
    /// 3. **the grace window is actually spent** — a wedged node cannot be
    ///    hard-aborted before `CANCEL_HARD_ABORT_GRACE`, because the runner first
    ///    flips the engine token and waits that long for a clean wind-down;
    /// 4. **once the grace is up, it comes back fast** — the hard abort must drop
    ///    the engine future *before* the observer, or the per-node handlers keep
    ///    their observer `Arc` clones, the progress channel stays open, and the
    ///    collector join blocks for the full `PROGRESS_DRAIN_TIMEOUT` on TOP of
    ///    the grace. That bug still passes 1–2 (the timeout swallows it and the
    ///    run settles correctly in the end); only the clock catches it, which is
    ///    why this asserts a bound rather than just an outcome.
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

        // **The grace was actually spent.** A wedged node cannot reach a boundary,
        // so the runner flips the token and waits the full `CANCEL_HARD_ABORT_GRACE`
        // before dropping the future. Landing below that would mean the token path
        // was skipped — a wedged run must never hard-abort early.
        assert!(
            elapsed >= CANCEL_HARD_ABORT_GRACE,
            "cancelling took {elapsed:?} — shorter than the grace window, so the clean node-boundary \
             wind-down was not attempted before the hard abort"
        );
        // **The drain-timeout guard.** Once the grace is up, the hard abort drops
        // the engine future, which must close the progress channel so the
        // collector join returns in milliseconds. If `drop(engine)` were missing
        // the join would stall for the full `PROGRESS_DRAIN_TIMEOUT` (10s) ON TOP
        // of the grace. This bounds the total at grace + a healthy drain, far
        // below grace + the drain timeout, so it fails loudly on that bug without
        // being flaky on a loaded CI box.
        assert!(
            elapsed < CANCEL_HARD_ABORT_GRACE + std::time::Duration::from_secs(2),
            "cancelling took {elapsed:?} — past the grace the progress channel did not close, so \
             the collector join stalled until the drain timeout. Check that the engine future is \
             dropped BEFORE the observer in `run_workflow_inner`."
        );
        assert!(
            elapsed < CANCEL_HARD_ABORT_GRACE + PROGRESS_DRAIN_TIMEOUT,
            "cancel latency reached the grace plus the full drain timeout"
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

    /// A model that blocks its first call until the test releases it, after
    /// announcing that it got there — but then, unlike [`StallingProvider`],
    /// **returns normally**. That is what makes a *clean* cancel possible: the
    /// agent node completes, the engine hits the next boundary, sees the flipped
    /// token, and winds the run down rather than being dropped mid-await.
    struct GatedProvider {
        inner: MockProvider,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl tinyagents::harness::model::ChatModel<()> for GatedProvider {
        async fn invoke(
            &self,
            state: &(),
            request: tinyagents::harness::model::ModelRequest,
        ) -> tinyagents::Result<tinyagents::harness::model::ModelResponse> {
            self.entered.notify_waiters();
            // `notify_one` carries a permit, so a release that lands before this
            // registers is not lost — no ordering race with the test.
            self.release.notified().await;
            tinyagents::harness::model::ChatModel::invoke(&self.inner, state, request).await
        }
    }

    impl crate::harness::provider::HarnessModel for GatedProvider {
        fn telemetry_provider_id(&self) -> String {
            "gated".to_string()
        }
    }

    /// **The clean-cancel arm (issue #398).** The counterpart to the hard-abort
    /// keystone: here the wedged node is *released* right after the stop, so the
    /// agent node finishes, the engine reaches the next boundary, observes the
    /// flipped token, and winds the run down cleanly — returning a real
    /// `RunOutcome` with `cancelled` set instead of having its future dropped.
    ///
    /// Three things distinguish this from the hard abort:
    ///
    /// 1. the run still settles `cancelled` and still routes nothing;
    /// 2. **the completed nodes ride the run response** — `run.nodes` carries the
    ///    trail, because a clean wind-down keeps the collected rows the dropped
    ///    future had to throw away; and
    /// 3. the node past the stop point (`done`) never runs — the token halts the
    ///    graph at the boundary, so it is neither started nor finished.
    #[tokio::test]
    async fn a_cleanly_cancelled_run_winds_down_at_the_boundary_keeping_its_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Arc::new(HarnessPool::new());
        let rec = record();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let (mut deps, events) = deps_with_events(dir.path());
        deps.provider = Arc::new(GatedProvider {
            inner: MockProvider::new("mock: "),
            entered: entered.clone(),
            release: release.clone(),
        });
        deps.provider_slug = "gated".to_string();
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

        // Stop the run, THEN let the wedged node complete. The token is already
        // flipped by the time the agent finishes, so the engine winds down at the
        // boundary before `done` — well within the grace, so no hard abort.
        let pressed = std::time::Instant::now();
        cancel.cancel();
        release.notify_one();
        let run = tokio::time::timeout(std::time::Duration::from_secs(30), run)
            .await
            .expect("the cleanly cancelled run never returned")
            .expect("a cancelled run is Ok, not Err");
        let elapsed = pressed.elapsed();

        assert!(
            run.cancelled,
            "a clean node-boundary stop still reports cancelled"
        );
        assert!(
            run.deliveries.is_empty(),
            "a stopped run routes nothing, clean or not"
        );
        // Settled by winding down, NOT by the hard-abort fallback: it must come
        // back well inside the grace window.
        assert!(
            elapsed < CANCEL_HARD_ABORT_GRACE,
            "a clean wind-down took {elapsed:?} — it should settle before the grace, not fall back \
             to the hard abort"
        );

        // The clean arm carries the trail on the RESPONSE (the hard-abort arm
        // returns an empty one). `shape` and `ceo` finished; `done` is past the
        // stop boundary and never ran.
        let ran: Vec<&str> = run.nodes.iter().map(|n| n.node_id.as_str()).collect();
        assert!(ran.contains(&"shape"), "the transform completed: {ran:?}");
        assert!(
            ran.contains(&"ceo"),
            "the agent node finished before the wind-down: {ran:?}"
        );
        assert!(
            !ran.contains(&"done"),
            "the node past the stop boundary must never run: {ran:?}"
        );

        // The journal agrees, and every node that finished shows both brackets.
        let journal = journaled(&events, &rec.id).await;
        let finished: Vec<String> = journal
            .iter()
            .filter_map(|e| match e {
                CompanyEvent::WorkflowNodeFinished { node_id, .. } => Some(node_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(finished, vec!["shape".to_string(), "ceo".to_string()]);
        assert!(
            !journal
                .iter()
                .any(|e| matches!(e, CompanyEvent::WorkflowNodeStarted { node_id, .. } if node_id == "done")),
            "the halted node must not even open a started bracket"
        );
    }

    // ── Issue #542: dry run / test mode ─────────────────────────────────────

    /// A run context flagged dry, built off the unregistered constructor.
    fn dry_ctx() -> WorkflowRunContext {
        let mut ctx = WorkflowRunContext::new(false);
        ctx.dry_run = true;
        ctx
    }

    /// A [`MockProvider`] wrapper that counts every inference `invoke`, so a
    /// test can prove a dry agent node makes **zero** of them (T2).
    #[derive(Clone)]
    struct CountingProvider {
        inner: MockProvider,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl tinyagents::harness::model::ChatModel<()> for CountingProvider {
        async fn invoke(
            &self,
            state: &(),
            request: tinyagents::harness::model::ModelRequest,
        ) -> tinyagents::Result<tinyagents::harness::model::ModelResponse> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tinyagents::harness::model::ChatModel::invoke(&self.inner, state, request).await
        }
    }

    impl crate::harness::provider::HarnessModel for CountingProvider {
        fn telemetry_provider_id(&self) -> String {
            "counting-mock".to_string()
        }
    }

    /// A two-way branching graph: a `switch` on `kind` routes to one of two
    /// output arms. Only the matched arm's node finishes.
    const BRANCH: &str = r#"
id = "branch"
name = "Branch"
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
"#;

    /// T1 — a dry run walks the REAL graph with real branch selection: the taken
    /// arm's node appears in the per-node trail, the untaken one never does.
    #[tokio::test]
    async fn t1_dry_run_reports_only_the_taken_branch_in_its_node_trail() {
        let dir = tempfile::tempdir().unwrap();
        let file = parse_workflow(BRANCH).expect("parses");
        let run = run_workflow(
            Arc::new(HarnessPool::new()),
            deps(dir.path()),
            &tools_record(),
            &file,
            serde_json::json!({ "kind": "paid" }),
            &dry_ctx(),
        )
        .await
        .expect("dry run completes");

        let ran: Vec<&str> = run.nodes.iter().map(|n| n.node_id.as_str()).collect();
        assert!(
            ran.contains(&"paid_out"),
            "the taken branch should be in the trail: {ran:?}"
        );
        assert!(
            !ran.contains(&"free_out"),
            "the untaken branch must never appear: {ran:?}"
        );
    }

    /// T2 — a dry agent node makes ZERO inference calls; the live control makes
    /// at least one. The flag alone separates the two.
    #[tokio::test]
    async fn t2_dry_agent_node_makes_no_inference_calls() {
        let dir = tempfile::tempdir().unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut deps = deps(dir.path());
        deps.provider = Arc::new(CountingProvider {
            inner: MockProvider::new("mock: "),
            calls: calls.clone(),
        });
        let pool = Arc::new(HarnessPool::new());
        let rec = record();
        pool.ensure(&rec, &deps).await.expect("roster builds");
        let file = parse_workflow(GREET).expect("parses");

        // Live control: the agent node runs a real turn, so the counter moves.
        run_workflow(
            pool.clone(),
            deps.clone(),
            &rec,
            &file,
            serde_json::json!({}),
            &WorkflowRunContext::new(false),
        )
        .await
        .expect("live run completes");
        let after_live = calls.load(std::sync::atomic::Ordering::SeqCst);
        assert!(after_live > 0, "the live control must invoke inference");

        // Dry run: the stub agent echoes, so the counter does NOT move.
        let dry = run_workflow(pool, deps, &rec, &file, serde_json::json!({}), &dry_ctx())
            .await
            .expect("dry run completes");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            after_live,
            "a dry agent node must make no inference calls"
        );
        // …and its output carries the dry marker rather than a real reply.
        assert!(
            dry.output.to_string().contains("[dry run]"),
            "dry output should echo the stub fixture: {}",
            dry.output
        );
    }

    /// T3 — a dry `tool_call` executes NOTHING: the CSV the live tool would write
    /// never appears, and no per-run workspace is even created.
    #[tokio::test]
    async fn t3_dry_tool_call_writes_nothing_to_disk() {
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
data = "[{\"name\":\"Ada\"}]"
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
            &dry_ctx(),
        )
        .await
        .expect("dry run completes");
        assert!(run.pending_approvals.is_empty());
        // A dry run creates no workspace at all — so the tool could not have run.
        assert!(
            !dir.path().join("acme").join("_workflow").exists(),
            "a dry run must not create a per-run workspace"
        );
    }

    /// T4 — a dry run delivers NOTHING and journals NOTHING: the recording
    /// channel gets zero dispatches, the delivery row is `Skipped`/`DryRun`, and
    /// the journal holds no Started / NodeFinished / Finished / ReportDelivered
    /// for the run.
    #[tokio::test]
    async fn t4_dry_run_delivers_nothing_and_journals_nothing() {
        use crate::runtime::channel::RecordingChannel;

        let dir = tempfile::tempdir().unwrap();
        let channel = RecordingChannel::new("engineering");
        let events: Arc<dyn crate::ports::EventLog> =
            Arc::new(crate::store::FsEventLog::new(dir.path()));
        let mut deps = deps(dir.path());
        deps.events = Some(events.clone());
        deps.delivery = Some(crate::workflows::WorkflowDeliveryDeps {
            mail: None,
            inbox: Arc::new(crate::store::FsInboxStore::new(dir.path())),
            users: Arc::new(FsOps::new(dir.path())),
            bootstrap_admin: None,
            channels: vec![Arc::new(channel.clone())],
            parking: None,
            events: events.clone(),
        });

        let file = parse_workflow(REPORT_TO_DESK).expect("parses");
        let run = run_workflow(
            Arc::new(HarnessPool::new()),
            deps,
            &record(),
            &file,
            serde_json::json!({ "brief": "numbers" }),
            &dry_ctx(),
        )
        .await
        .expect("dry run completes");

        assert_eq!(channel.sent().len(), 0, "a dry run must post nothing");
        assert_eq!(run.deliveries.len(), 1, "{:?}", run.deliveries);
        assert_eq!(
            run.deliveries[0].status,
            crate::ports::DeliveryStatus::Skipped
        );
        assert_eq!(
            run.deliveries[0].reason,
            crate::ports::DeliveryReason::DryRun
        );

        let journal = journaled(&events, &record().id).await;
        assert!(
            !journal.iter().any(|e| matches!(
                e,
                CompanyEvent::WorkflowRunStarted { .. }
                    | CompanyEvent::WorkflowNodeStarted { .. }
                    | CompanyEvent::WorkflowNodeFinished { .. }
                    | CompanyEvent::WorkflowRunFinished { .. }
                    | CompanyEvent::WorkflowReportDelivered { .. }
            )),
            "a dry run must journal nothing: {journal:?}"
        );
    }

    /// T5 — the REQUIRED negative control: the SAME graph run with `dry_run =
    /// false` DOES dispatch and DOES journal. The flag alone separates the two
    /// behaviours.
    #[tokio::test]
    async fn t5_the_same_graph_run_for_real_dispatches_and_journals() {
        use crate::runtime::channel::RecordingChannel;

        let dir = tempfile::tempdir().unwrap();
        let channel = RecordingChannel::new("engineering");
        let events: Arc<dyn crate::ports::EventLog> =
            Arc::new(crate::store::FsEventLog::new(dir.path()));
        let mut deps = deps(dir.path());
        deps.events = Some(events.clone());
        deps.delivery = Some(crate::workflows::WorkflowDeliveryDeps {
            mail: None,
            inbox: Arc::new(crate::store::FsInboxStore::new(dir.path())),
            users: Arc::new(FsOps::new(dir.path())),
            bootstrap_admin: None,
            channels: vec![Arc::new(channel.clone())],
            parking: None,
            events: events.clone(),
        });

        let file = parse_workflow(REPORT_TO_DESK).expect("parses");
        let run = run_workflow(
            Arc::new(HarnessPool::new()),
            deps,
            &record(),
            &file,
            serde_json::json!({ "brief": "numbers" }),
            &WorkflowRunContext::new(false),
        )
        .await
        .expect("real run completes");

        assert_eq!(channel.sent().len(), 1, "a real run posts the report");
        assert_eq!(run.deliveries[0].status, crate::ports::DeliveryStatus::Sent);

        let journal = journaled(&events, &record().id).await;
        assert!(
            journal
                .iter()
                .any(|e| matches!(e, CompanyEvent::WorkflowRunStarted { .. })),
            "a real run journals its start: {journal:?}"
        );
        assert!(
            journal
                .iter()
                .any(|e| matches!(e, CompanyEvent::WorkflowReportDelivered { .. })),
            "a real run journals its delivery: {journal:?}"
        );
    }

    /// T6 — an ungranted `tool_call` refuses in a dry run EXACTLY as it does live
    /// (the grant gate is pure and kept). `record()` grants no tools, so the
    /// `code`-namespace `csv_export` is denied and, with `on_error` defaulting to
    /// stop, the run fails with the same "not granted" error.
    #[tokio::test]
    async fn t6_ungranted_tool_refuses_identically_in_dry_mode() {
        let src = r#"
id = "t6"
name = "T6"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "call"
kind = "tool_call"
name = "Call"
[node.config]
slug = "csv_export"
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
        // A company that grants `web` but NOT `code`, so the `code`-namespace
        // `csv_export` is refused — the same gate the live invoker applies.
        let web_only: CompanyRecord = {
            let mut rec = tools_record();
            rec.manifest.tools.allow = vec!["web.*".to_string()];
            rec
        };
        let dir = tempfile::tempdir().unwrap();
        let file = parse_workflow(src).expect("parses");
        let err = run_workflow(
            Arc::new(HarnessPool::new()),
            deps(dir.path()),
            &web_only, // grants web, not code
            &file,
            serde_json::json!({ "seed": 1 }),
            &dry_ctx(),
        )
        .await
        .expect_err("an ungranted tool must fail the dry run too");
        assert!(
            err.to_string().contains("not granted"),
            "the dry grant gate must refuse identically: {err}"
        );
    }

    /// T7 — a dry run reports the gate on `pending_approvals` but parks NOTHING
    /// durable: the journal stays empty (park_pending_gates is skipped).
    #[tokio::test]
    async fn t7_dry_gate_reports_pending_but_parks_nothing() {
        let src = r#"
id = "t7"
name = "T7"
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
        let (deps, events) = deps_with_events(dir.path());
        let file = parse_workflow(src).expect("parses");
        let run = run_workflow(
            Arc::new(HarnessPool::new()),
            deps,
            &tools_record(),
            &file,
            serde_json::json!({ "seed": 1 }),
            &dry_ctx(),
        )
        .await
        .expect("dry run pauses cleanly");
        assert!(
            run.pending_approvals.iter().any(|id| id == "gate"),
            "the gate should be reported pending: {:?}",
            run.pending_approvals
        );
        let journal = journaled(&events, &tools_record().id).await;
        assert!(
            journal.is_empty(),
            "a dry run parks nothing and journals nothing: {journal:?}"
        );
    }

    /// T8 (issue #382) — a dry run emits **no `WorkflowNodeStarted`** either, for
    /// the same reason it emits no finish: the started event is journaling-gated
    /// (`events` wired AND not dry), not observer-gated. The observer still fires
    /// — the per-node trail on the RESPONSE proves the nodes ran — but nothing
    /// durable is written. The negative control that a real run of the same graph
    /// DOES journal starts is `a_run_journals_a_start_then_a_started_finished_pair…`.
    #[tokio::test]
    async fn t8_dry_run_collects_the_node_trail_but_journals_no_node_started() {
        let dir = tempfile::tempdir().unwrap();
        let (deps, events) = deps_with_events(dir.path());
        let file = parse_workflow(GREET).expect("parses");
        let run = run_workflow(
            Arc::new(HarnessPool::new()),
            deps,
            &record(),
            &file,
            serde_json::json!({}),
            &dry_ctx(),
        )
        .await
        .expect("dry run completes");

        // The observer ran — the response carries the trail even for a dry run.
        let ran: Vec<&str> = run.nodes.iter().map(|n| n.node_id.as_str()).collect();
        assert!(ran.contains(&"ceo") && ran.contains(&"done"), "{ran:?}");

        // …but nothing durable, node-started included.
        let journal = journaled(&events, &record().id).await;
        assert!(
            !journal
                .iter()
                .any(|e| matches!(e, CompanyEvent::WorkflowNodeStarted { .. })),
            "a dry run must journal no node-started event: {journal:?}"
        );
        assert!(
            journal.is_empty(),
            "a dry run journals nothing at all: {journal:?}"
        );
    }
}
