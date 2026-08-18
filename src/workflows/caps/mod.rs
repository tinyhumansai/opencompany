//! The tinyflows [`Capabilities`] bundle for a company workflow run.
//!
//! tinyflows is host-agnostic: every outside-world effect is a trait the host
//! implements. This module supplies that bundle for an OpenCompany run.
//!
//! Wired capabilities (P1):
//!
//! * **agent** ([`HarnessAgentRunner`]) — an `agent` node (config `agent_ref` =
//!   a roster teammate id) routes to the company's
//!   [`HarnessPool`](crate::harness::HarnessPool), so the step runs on the same
//!   live openhuman agent as chat/task dispatch — inheriting its persona, model,
//!   [`OcMemory`](crate::harness::memory), approval policy, and cost metering.
//! * **tool_call** ([`WorkflowToolInvoker`](tools::WorkflowToolInvoker)) — a
//!   `tool_call` node executes a real Cell A toolbelt tool (`shell` / `code` /
//!   `web`, plus the metered `search` family behind an explicit `search` grant)
//!   scoped to a dedicated per-company workflow workspace, fail-closed on the
//!   company's `[tools].allow` grants.
//! * **http_request** ([`GuardedHttpClient`](http::GuardedHttpClient)) — an
//!   `http_request` node routes through OpenHuman's `HttpRequestTool` so every
//!   request (and redirect) passes the upstream `url_guard` SSRF check.
//! * **state** ([`CompanyStateStore`](state::CompanyStateStore)) — durable
//!   per-run key/value over the [`SecretStore`](crate::ports::SecretStore) seam.
//!   No tinyflows node OpenCompany emits consumes it yet; it is deliberate
//!   contract-plumbing a later phase (P3) consumes.
//!
//! Wired in P2:
//!
//! * **sub_workflow** ([`StoreWorkflowResolver`](resolver::StoreWorkflowResolver))
//!   — a `sub_workflow` node referencing a child by `workflow_id` resolves it
//!   from the union of the company's seed `workflows/` directory
//!   ([`HarnessDeps::workflow_source_dir`](crate::harness::HarnessDeps)) and the
//!   record's runtime-authored graph bodies (full validation + a static cycle
//!   guard). A platform-provisioned tenant has no source directory, so every
//!   child it owns resolves from the record.
//!
//! Still **not wired**: the bare-completion `LlmProvider` fallback and `code`
//! nodes. They are explicit stubs that return a clear capability error rather
//! than a silent no-op, so a workflow that reaches one fails loudly; a workflow
//! that never reaches one is unaffected. The `llm` stub takes care to report the
//! *right* failure: the engine's output_parser auto-fix (default on) calls `llm`
//! to repair a schema mismatch, so that stub surfaces the schema errors rather
//! than masking them behind "bare LLM completion is not wired" (issue #661).
//!
//! Also not wired, and for a different reason: **memory**, which tinyflows 0.6
//! added with the #499 pin bump. The other two are unbuilt; this one is
//! *undecided*. A `MemoryProvider` would give a workflow read and **write**
//! access to agent memory, and which scopes a workflow may touch has not been
//! settled — so it is left `None` until it is, and
//! [`the_memory_capability_is_left_unwired_on_purpose`](tests) pins that so the
//! answer has to be given rather than defaulted into.

mod dry_run;
mod http;
mod resolver;
mod state;
mod tools;
/// Issue #849: how much upstream output one agent node's turn may carry, and
/// what to say when a provider refuses the turn on its context window anyway.
mod upstream;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tinyflows::caps::{
    AgentRunner, Capabilities, CodeLanguage, CodeRunner, HttpClient, LlmProvider, StateStore,
    ToolInvoker, WorkflowResolver,
};
use tinyflows::error::{EngineError, Result as TfResult};

use crate::harness::orchestrator::MAX_DELEGATIONS_PER_TURN;
use crate::harness::policy::{ApprovalScope, MAX_APPROVAL_REQUESTS_PER_TURN, PolicyMode};
use crate::harness::{HarnessDeps, HarnessPool, toolbelt};
use crate::ports::types::{CompanyId, CompanyRecord};

use self::http::GuardedHttpClient;
use self::resolver::StoreWorkflowResolver;
use self::state::{CompanyStateStore, NoopState};
use self::tools::WorkflowToolInvoker;
pub(crate) use self::tools::{
    WORKFLOW_TOOL_CATALOG, WORKFLOW_TOOL_NAMESPACES, wired_workflow_namespaces, workflow_tool_info,
    workflow_tool_wiring,
};
/// Issue #849: the ceiling on what one agent node's turn carries from upstream.
/// Re-exported so the end-to-end fan-in proof
/// ([`agent_upstream_input_test`](crate::workflows::agent_upstream_input_test))
/// asserts against the shipped number rather than a copy of it — which is the
/// only caller outside this module, hence the `cfg`.
#[cfg(test)]
pub(crate) use self::upstream::DEFAULT_UPSTREAM_BUDGET_CHARS;
// `WORKFLOW_TOOL_SLUGS` stays module-private to `tools` since #813: the catalogue
// (`WORKFLOW_TOOL_CATALOG`) is what callers ground and validate against, and the
// slug table is now only its in-module pinning cross-check.

/// The four effectful capability slots [`build_capabilities`] chooses by mode:
/// `tool_call`, `http_request`, `state`, and the optional `agent` runner. The
/// dry and live branches each build one of these; the read-only `resolver` and
/// the always-stub `llm`/`code`/`memory` slots are assembled outside it.
type EffectSlots = (
    Arc<dyn ToolInvoker>,
    Arc<dyn HttpClient>,
    Arc<dyn StateStore>,
    Option<Arc<dyn AgentRunner>>,
);

/// What one run needs the capability bundle to know about *itself*.
///
/// Bundled rather than passed as five more parameters (issue #638 added the
/// fifth and tipped `build_capabilities` over clippy's arity limit). They
/// genuinely travel together — every one is scoped to this run and meaningless
/// without the others — so a struct is the honest shape rather than a way of
/// making the lint quiet.
pub struct RunContext<'a> {
    /// The workflow being run.
    pub workflow_id: &'a str,
    /// This run's id (issue #395), the key its approvals are stamped with.
    pub run_id: &'a str,
    /// The operator's topic for this run (issue #154), threaded to the agent
    /// capability so a node's turn carries what was actually asked.
    pub run_request: Option<String>,
    /// Issue #542: stub every effectful slot and journal nothing.
    pub dry_run: bool,
    /// Where an agent node leaves an operator-facing notice (issue #638).
    pub notices: RunNotices,
    /// Where an agent node's board writes are recorded (issue #661 / M5).
    pub board: RunBoard,
    /// Where an agent node records that it blocked on a human (issue #881).
    pub blocks: RunBlocks,
    /// Where an agent node records the approvals its turn parked (issue #880).
    pub approvals: RunApprovals,
}

/// Assembles the [`Capabilities`] bundle for a run of `workflow_id`.
///
/// `record` carries everything the outside-world capabilities need: the company
/// id, the `[policy].mode` (the exec-security autonomy tier), the `[tools].allow`
/// grants (the fail-closed `tool_call` gate), and the `[tools].web_allowed_domains`
/// SSRF allowlist. The tool_call / http_request capabilities are scoped to a
/// dedicated per-run workflow workspace
/// (`{workspace_root}/{company}/_workflow/{workflow}/{run}/workspace`) — the
/// `_` prefix keeps it from ever colliding with a roster agent's own workspace
/// directory.
///
/// `pool`/`deps` are shared with the rest of the harness surface — the roster the
/// agent nodes address is the one already resident in `pool`.
///
/// `run_request` is the operator's topic for this run (issue #154), threaded to
/// the agent capability so every agent node's turn message carries what was
/// actually asked, not just the node's authored instruction.
///
/// `dry_run` (issue #542) selects the **mode**, one assembly point so the two
/// bundles cannot drift. When `true`, every *effectful* slot is a stub from
/// [`dry_run`]: the agent echoes with no inference, `tool_call` keeps its
/// fail-closed grant check but executes nothing, `http_request` sends nothing,
/// and `state` is [`NoopState`] rather than the durable
/// [`CompanyStateStore`](state::CompanyStateStore). The read-only `resolver`
/// stays real in both modes, so a `sub_workflow` child runs under this same
/// bundle and a dry run propagates into it. The per-run workspace, exec-security
/// policy and search backend are not built at all for a dry run — nothing needs
/// them. Because every effect is stubbed, a future node kind cannot reach a real
/// effect through a dry bundle: the engine only calls what is on the bundle.
///
/// # Errors
///
/// Live mode (issue #661) creates the per-run workspace directory the
/// `tool_call` / `http_request` slots are rooted at; if that mkdir fails this
/// returns [`OpenCompanyError::Harness`](crate::error::OpenCompanyError::Harness)
/// rather than proceeding with effects pointed at a directory that does not
/// exist. A dry run builds no workspace and is infallible.
pub async fn build_capabilities(
    pool: Arc<HarnessPool>,
    deps: HarnessDeps,
    record: &CompanyRecord,
    run: RunContext<'_>,
) -> crate::error::Result<Capabilities> {
    let RunContext {
        workflow_id,
        run_id,
        run_request,
        dry_run,
        notices,
        board,
        blocks,
        approvals,
    } = run;
    let company = record.id.clone();
    // Issue #562: the tier actually in force — the operator's console override
    // when one is set, the manifest's otherwise. Reading `manifest.policy` here
    // would leave a workflow run on the shipped tier while the roster ran on the
    // operator's, which is the disagreement `effective_policy` exists to prevent.
    let mode = PolicyMode::parse(&record.effective_policy().mode);
    let grants = record.manifest.tools.allow.clone();
    let wiring = workflow_tool_wiring(&deps);

    // sub_workflow-by-id resolves children from the union of the company's seed
    // `workflows/` directory and the record's runtime-authored bodies — so a
    // platform tenant with no source dir still resolves the workflows it
    // created (issue #168). Read before `deps` may move into the agent runner.
    // REAL in both modes: it is a read, and a dry sub_workflow child runs under
    // this same (dry) bundle, so dry propagates rather than stopping here.
    // Issue #617: a child's nodes never reach the gate pass, so the resolver
    // carries the policy in order to *say* which of its calls were never
    // offered for approval. `None` for a dry run — nothing executes, so there
    // is nothing to disclose, and the resolver behaves exactly as before.
    let audit = (!dry_run).then(|| self::resolver::ChildCallAudit {
        policy: record.manifest.policy.clone(),
        run_id: run_id.to_string(),
        events: deps.events.clone(),
    });
    let resolver: Arc<dyn WorkflowResolver> = Arc::new(StoreWorkflowResolver::new(
        deps.workflow_source_dir.clone(),
        deps.store.clone(),
        company.clone(),
        workflow_id.to_string(),
        audit,
    ));

    // The four effectful slots, chosen by mode at this one point.
    let (tools, http, state, agent): EffectSlots = if dry_run {
        // DRY: stub every effect. No workspace mkdir, no exec-security, no pool
        // routing, no secret store. The grant check is KEPT (pure) so an
        // ungranted `tool_call` refuses identically; state is the inert no-op so
        // a dry run cannot persist either.
        tracing::debug!(
            company = %company,
            workflow = workflow_id,
            "workflow: building DRY capability bundle — no real effects will run"
        );
        (
            Arc::new(dry_run::DryRunTools::new(grants, wiring.clone())),
            Arc::new(dry_run::DryRunHttp),
            Arc::new(NoopState),
            Some(Arc::new(dry_run::DryRunAgent)),
        )
    } else {
        let workflow_ws = workflow_workspace(&deps.workspace_root, &company, workflow_id, run_id);
        // L2 (issue #661): a workspace the `tool_call` / `http_request` slots
        // cannot create is not something to warn past and keep going — the run
        // would proceed with those effects rooted at a directory that does not
        // exist, failing later and further from the cause. Abort here so the
        // caller sees the real reason. The failure precedes the WorkflowRunStarted
        // journal append, so a failed mkdir leaves no orphaned started row.
        tokio::fs::create_dir_all(&workflow_ws)
            .await
            .map_err(|err| {
                crate::error::OpenCompanyError::Harness(format!(
                    "workflow run could not create its workspace directory {}: {err}",
                    workflow_ws.display()
                ))
            })?;

        // ONE exec-security policy shared by the tool_call toolbelt and the
        // http_request client, sandboxed to the workflow workspace with the
        // company's autonomy tier — exactly the shape a roster agent's exec
        // tools get.
        let exec_security = Arc::new(toolbelt::exec_security(&workflow_ws, mode));
        let web_allowed_domains = record.manifest.tools.web_allowed_domains.clone();

        // The metered `search` family is threaded through the invoker the same
        // way `build_agent` wires it onto a roster agent — explicit `search`
        // grant + managed backend, fail-closed. Read `deps.search` / `deps.meter`
        // here, before `deps` moves into `HarnessAgentRunner` below. The agent
        // label names the run so a search sample is attributed to the workflow,
        // not a chat turn.
        let search_metering = crate::harness::search::SearchMetering {
            company: company.clone(),
            agent: format!("workflow:{workflow_id}"),
            meter: deps.meter.clone(),
        };
        // Issue #775: the host-owned shell audit sink, keyed per WORKFLOW (not
        // per run) under `companies/<slug>/audit/_workflow-<id>/`. Per-workflow
        // because the vendored logger registry caches one logger per directory
        // and every run of one workflow should append to one file — per-run would
        // mint a directory per execution and shard the trail. Never under
        // `workflow_ws`: that is the `workspace_only` policy root the node's own
        // tools are sandboxed to, so a sink there would be a permitted write
        // target for the `shell` it records.
        let workflow_audit_dir = crate::harness::build::agent_audit_dir(
            &deps.audit_root,
            &company,
            &format!("_workflow-{}", hex_segment(workflow_id)),
        );
        let tools = WorkflowToolInvoker::new(
            exec_security.clone(),
            &workflow_ws,
            &workflow_audit_dir,
            web_allowed_domains.clone(),
            grants,
            &deps.capabilities,
            deps.search.as_ref(),
            search_metering,
            wiring,
        );
        let http = GuardedHttpClient::new(exec_security, web_allowed_domains);

        // Durable run state over the per-company secret store, namespaced by
        // workflow id. `None` (default/tests) keeps the inert no-op with a
        // warning — no node OpenCompany emits reads state in P1, so this never
        // blocks a run.
        let state: Arc<dyn StateStore> = match &deps.secrets {
            Some(secrets) => Arc::new(CompanyStateStore::new(
                secrets.clone(),
                company.clone(),
                workflow_id.to_string(),
            )),
            None => {
                tracing::warn!(
                    company = %company,
                    workflow = workflow_id,
                    "workflow: no secret store wired; run state is a no-op (deliberate — no P1 node uses it)"
                );
                Arc::new(NoopState)
            }
        };

        // Issue #661 (M5): the run's board claim, taken ONCE here and held for the
        // whole run rather than per node turn.
        //
        // **Per-run is the correctness requirement, not a convenience.** The
        // engine runs same-superstep nodes concurrently (`with_parallel(true)`),
        // and `claim_as` CLEARS the scope's bucket on acquire — so a second claim
        // taken by a sibling node mid-superstep would destroy whatever the first
        // node had staged and not yet drained. One claim per run, acquired before
        // any node runs, is what makes the bucket safe for the concurrent nodes
        // that share it.
        //
        // The consequence to know: siblings share one bucket, so node A's
        // post-turn drain may execute (and report) a write node B staged. Nothing
        // is lost or duplicated — every staged write is executed exactly once and
        // contributes exactly one row — but a row is attributed to the RUN rather
        // than to a node, which is why `WorkflowRunBoardRow` carries no node id.
        // The per-turn `MAX_DELEGATIONS_PER_TURN` cap likewise becomes a per-drain
        // bound over the shared bucket, refused honestly at the tool boundary.
        //
        // `Arc` because the runner is shared across the engine's node tasks and
        // `DelegationClaim` is deliberately not `Clone` — one claim, one owner of
        // the promise. It releases when the capability bundle drops, which on the
        // hard-abort path is when the engine future is dropped: staged-but-undrained
        // writes die with the run, exactly as `ApprovalClaim` treats gated calls.
        let board_claim = Arc::new(deps.delegations.claim_board(run_id.to_string()));

        // `deps` moves in last — the borrows above (`deps.capabilities`,
        // `deps.search`, `deps.meter`, `deps.secrets`, `deps.workspace_root`,
        // `deps.delegations`) are all done by here.
        let agent: Arc<dyn AgentRunner> = Arc::new(HarnessAgentRunner::new(
            pool,
            deps,
            record.clone(),
            company.clone(),
            workflow_id.to_string(),
            run_id.to_string(),
            run_request,
            notices,
            board,
            blocks,
            approvals,
            board_claim,
        ));
        (Arc::new(tools), Arc::new(http), state, Some(agent))
    };

    Ok(Capabilities {
        llm: Arc::new(UnwiredLlm),
        tools,
        http,
        code: Arc::new(UnwiredCode),
        state,
        resolver,
        agent,
        // New in tinyflows 0.6.1, pinned here via the #675 cancel-token chain
        // (openhuman #5520 → tinyflows #31). A `shell` node runs an inline
        // POSIX script through a host-configured runner; OC wires none, and
        // whether a workflow may spawn shell processes on the company host is a
        // policy question this repo has not answered. `None` fails such a node
        // at run time with a capability error — the honest answer until that
        // decision is made, and no company manifest can currently author a
        // `shell` node (mirrors `memory` below).
        shell: None,
        // New in tinyflows 0.6, which arrived with the #499 pin bump. Left
        // unwired deliberately rather than pointed at the company's context
        // store: a `memory` node would then read and WRITE agent memory on
        // behalf of a workflow, and which scopes a workflow may touch is a
        // policy question this repo has not answered — `remember`/`forget`
        // especially. `None` fails such a node at run time with a capability
        // error, which is the honest answer until that decision is made, and
        // no company manifest can currently produce one (`NodeKind` has no
        // `memory` variant on our side).
        memory: None,
        // New in tinyflows 0.8.0: `spawn` can hand work to a host `TaskRunner`.
        // Keep `None` here so spawned work runs inline and emits an
        // already-settled ticket for a downstream `gate`; real overlap belongs
        // in the later concurrency adoption phase.
        tasks: None,
        // New in a later tinyflows 0.8.x: `approval` nodes can push a request
        // at a host-registered `ApprovalProvider`. `None` here leaves the
        // fallback behaviour intact — an `approval` node still pauses the run
        // for the host to settle through `engine::resume`; wiring a provider
        // that proactively notifies a reviewer is a separate policy decision
        // this repo has not made.
        approvals: None,
    })
}

/// Builds a traversal-safe workspace path unique to one workflow execution.
fn workflow_workspace(
    root: &std::path::Path,
    company: &CompanyId,
    workflow_id: &str,
    run_id: &str,
) -> std::path::PathBuf {
    root.join(company.as_ref())
        .join("_workflow")
        .join(hex_segment(workflow_id))
        .join(hex_segment(run_id))
        .join("workspace")
}

/// Encodes an arbitrary identifier as one safe, reversible path segment.
fn hex_segment(value: &str) -> String {
    use std::fmt::Write;
    value
        .as_bytes()
        .iter()
        .fold(String::with_capacity(value.len() * 2), |mut out, byte| {
            write!(out, "{byte:02x}").expect("writing to String cannot fail");
            out
        })
}

/// A tinyflows [`AgentRunner`] that executes an `agent` node on the company's
/// [`HarnessPool`].
///
/// The engine calls [`run_agent`](AgentRunner::run_agent) with the node's
/// resolved config as `request` and the (trusted) `agent_ref` as the roster
/// teammate id. This extracts the turn message from the request and runs it
/// through [`HarnessPool::run`], which meters the turn's cost through `deps` — so
/// a workflow step and a chat turn account identically.
///
/// # It claims the delegation queue for board writes, and nothing else (issue #661 / M5)
///
/// A node's turn carries the whole toolbelt, so an orchestrator-tier `agent_ref`
/// can reach `review_task`, `assign_task`, `spawn_task` and `delegate_to_desk`,
/// and a granted one can reach `publish_artifact`.
///
/// This path now holds a
/// [`DrainClaim::Board`](crate::harness::orchestrator::DrainClaim) claim on the
/// delegation queue for the whole run and drains it after every node turn, so a
/// run may **open a card and set who owns it** — which is what makes the shipped
/// `→ task cards` seed able to produce one, and what M5 was filed for. The card
/// is a lineage root carrying a run reference
/// ([`TaskRecord::origin_run_id`](crate::ports::TaskRecord::origin_run_id)):
/// `parent_task_id` and `origin_chat_id` stay `None` because a run has neither a
/// card nor a conversation behind it — the same absence that makes
/// [`park_gated_calls`](HarnessAgentRunner::park_gated_calls) record approvals
/// explicitly unlinked.
///
/// The two other delegations stay **refused at the tool boundary**, for unrelated
/// reasons the queue reports separately: `review_task`'s `in_review → done` is the
/// operator's accept lane, and a hand-off's only value is a synchronous reply a run
/// has nowhere to land. `assign_task` moves no column either — the arm the drain
/// reuses leaves it untouched — so **run → card → dispatch → run cycles stay
/// bounded precisely because every dispatch still requires an operator act**. That
/// is the loop bound, and relaxing the column rule would take it with it.
///
/// The claim also closes a **live** misattribution defect PR #771 identified.
/// `DelegateToDeskTool` calls `push_refusal` before it consults the claim, so an
/// ungrounded hand-off from a workflow node landed in the shared bucket and a
/// concurrent chat turn's `drain_refusals` took it, recorded it on *that* turn's
/// card, and cleared it. The scoped claim files it into the run's own bucket, and
/// the drain below surfaces it as this run's notice.
///
/// It still takes no [`PublishClaim`](crate::harness::publish::PublishClaim):
/// `publish_artifact` needs a card to attach a version to, which a run does not
/// have, so a refusal there remains the truthful answer.
pub struct HarnessAgentRunner {
    pool: Arc<HarnessPool>,
    deps: HarnessDeps,
    /// The company record, for the board drain's desk/assignee resolution (issue
    /// #661 / M5) — the same record the rest of this bundle was built from, so a
    /// node's board write and its tool grants cannot disagree about the roster.
    record: CompanyRecord,
    company: CompanyId,
    /// The workflow this run is of (issue #661 / M5): stamped onto every card the
    /// run opens, and the voice its notes are recorded under
    /// (`workflow:<workflow_id>`).
    workflow_id: String,
    /// The run these agent nodes belong to (issue #395), stamped onto every
    /// approval this node's turn parks so the Approvals page can say which
    /// workflow run is waiting on the operator.
    run_id: String,
    /// What the operator asked for on this run (issue #154), when they supplied
    /// it. A node's `prompt` is authored into the graph and is the same on every
    /// run, so without this the run's topic never reaches the teammate doing the
    /// work — the agent would run, find no subject, and ask for one.
    run_request: Option<String>,
    /// Where this node leaves an operator-facing notice (issue #638).
    notices: RunNotices,
    /// Where this node's board writes are recorded (issue #661 / M5).
    board: RunBoard,
    /// Where this node records that it blocked on a human (issue #881).
    blocks: RunBlocks,
    /// Where this node records the approvals its turn parked (issue #880).
    approvals: RunApprovals,
    /// The run's [`DrainClaim::Board`](crate::harness::orchestrator::DrainClaim)
    /// claim, taken once by [`build_capabilities`] and held for the whole run.
    ///
    /// Shared rather than per-turn because `claim_as` clears the scope's bucket on
    /// acquire and the engine runs same-superstep nodes concurrently — a per-turn
    /// claim would let one node destroy a sibling's staged writes. See the
    /// acquisition site for the full reasoning.
    board_claim: Arc<crate::harness::orchestrator::DelegationClaim>,
}

/// Where an agent node leaves a notice for the operator (issue #638).
///
/// A shared handle rather than a return value because there is nowhere to
/// return it to: `AgentRunner::run_agent` hands the engine a `Value` that
/// becomes the node's output, and a system notice is emphatically not node
/// output — it would ride into a downstream `=item` binding and into the run's
/// persisted output snapshot. So the notice goes sideways, out to the runner
/// that owns the run, and lands on [`WorkflowRun::notices`].
///
/// Cheap to clone; every clone appends to the same list, which is what lets one
/// run's several agent nodes each contribute.
#[derive(Clone, Default)]
pub struct RunNotices {
    inner: Arc<std::sync::Mutex<Vec<String>>>,
}

impl RunNotices {
    /// Records one notice.
    pub fn push(&self, notice: String) {
        self.inner
            .lock()
            .expect("run notices poisoned")
            .push(notice);
    }

    /// Takes everything recorded so far, leaving the collector empty.
    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.inner.lock().expect("run notices poisoned"))
    }
}

/// Where an agent node's board writes are recorded (issue #661 / M5).
///
/// [`RunNotices`]' shape, beside it and for the same structural reason: a board
/// row is not node output and must not become one — it would ride a downstream
/// `=item` binding and land in the run's persisted output snapshot, where a
/// card id is neither wanted nor meaningful. So the rows go sideways, out to the
/// runner that owns the run, and land on [`WorkflowRun::board`].
///
/// Cheap to clone; every clone appends to the same list, which is what lets a
/// run's several agent nodes — including concurrent siblings — each contribute.
#[derive(Clone, Default)]
pub struct RunBoard {
    inner: Arc<std::sync::Mutex<Vec<crate::ports::WorkflowRunBoardRow>>>,
}

impl RunBoard {
    /// Records the rows one post-turn drain produced, in order.
    pub fn extend(&self, rows: Vec<crate::ports::WorkflowRunBoardRow>) {
        if rows.is_empty() {
            return;
        }
        self.inner.lock().expect("run board poisoned").extend(rows);
    }

    /// Takes everything recorded so far, leaving the collector empty.
    pub fn take(&self) -> Vec<crate::ports::WorkflowRunBoardRow> {
        std::mem::take(&mut *self.inner.lock().expect("run board poisoned"))
    }
}

/// Where an agent node records that it **blocked** on a human (issue #881).
///
/// [`RunNotices`]' shape, beside it and for the structural reason spelled out
/// there — and here that reason is not incidental, it is the bug. `run_agent`'s
/// return value *becomes the node's output*, so a non-output fact riding it
/// lands in the next node's `=items` binding. That is exactly how a gated
/// `publish_artifact` came to hand the model's apology downstream: the node had
/// nowhere but its output to say "I was blocked", so it said it there. The
/// blockage goes sideways instead, and the node's output goes nowhere at all.
///
/// Cheap to clone; every clone appends to the same list, which is what lets a
/// run's several agent nodes — including concurrent siblings — each contribute.
#[derive(Clone, Default)]
pub struct RunBlocks {
    inner: Arc<std::sync::Mutex<Vec<crate::ports::WorkflowBlockedNode>>>,
}

impl RunBlocks {
    /// Records that one node blocked.
    pub fn push(&self, blocked: crate::ports::WorkflowBlockedNode) {
        self.inner
            .lock()
            .expect("run blocks poisoned")
            .push(blocked);
    }

    /// Takes everything recorded so far, leaving the collector empty.
    pub fn take(&self) -> Vec<crate::ports::WorkflowBlockedNode> {
        std::mem::take(&mut *self.inner.lock().expect("run blocks poisoned"))
    }
}

/// Where an agent node records the approvals its turn **parked** (issue #880).
///
/// The third channel in the [`RunNotices`] / [`RunBoard`] family, for the same
/// structural reason and one more: these rows are *receipts*, and a receipt has
/// to survive the thing it is a receipt for. A card is durable the moment
/// `park_and_journal` writes it, so the row must outlive the node — and outlive
/// the run's failure too, which is why the collector is owned by the runner
/// rather than by the capability bundle the engine future drops.
///
/// Cheap to clone; every clone appends to the same list.
#[derive(Clone, Default)]
pub struct RunApprovals {
    inner: Arc<std::sync::Mutex<Vec<crate::ports::WorkflowRunApprovalRow>>>,
}

impl RunApprovals {
    /// Records the rows one post-turn drain produced, in order.
    pub fn extend(&self, rows: Vec<crate::ports::WorkflowRunApprovalRow>) {
        if rows.is_empty() {
            return;
        }
        self.inner
            .lock()
            .expect("run approvals poisoned")
            .extend(rows);
    }

    /// Takes everything recorded so far, leaving the collector empty.
    pub fn take(&self) -> Vec<crate::ports::WorkflowRunApprovalRow> {
        std::mem::take(&mut *self.inner.lock().expect("run approvals poisoned"))
    }
}

/// What one node's post-turn approval drain did (issue #881).
///
/// [`park_gated_calls`](HarnessAgentRunner::park_gated_calls) used to return
/// `()`, so everything it learned died at its own log line — which is the whole
/// of why a blocked node reported `ok`. This is that knowledge, handed back.
///
/// **Emptiness is the trigger.** A summary with no tools means the turn gated
/// nothing and the node is ordinary; anything else means the node produced no
/// deliverable and its branch must not continue. Keyed off the bare presence of
/// gated calls rather than a guess about which of them "mattered" — the same
/// bare-count trigger
/// [`settled_run_status`](crate::harness::lifecycle::settled_run_status) uses,
/// and for the same reason: the alternative is a heuristic about intent, or
/// sniffing the model's prose for an apology.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ParkedCalls {
    /// The tools whose calls were gated, deduplicated, in first-seen order.
    pub tools: Vec<String>,
    /// The approvals actually opened — what the operator can decide.
    pub approval_ids: Vec<String>,
    /// How many gated calls could not be parked at all: the park failed, or
    /// this runtime has no approvals queue wired, or the call was in the excess
    /// the drain discarded. **Nobody will be asked about these**, which is
    /// strictly worse than being blocked on a card, and the node's diagnosis
    /// says so separately.
    pub unparkable: usize,
}

impl ParkedCalls {
    /// Whether this turn gated anything at all — the block trigger.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty() && self.unparkable == 0
    }
}

impl HarnessAgentRunner {
    /// Builds a runner over an already-populated pool for `company`, carrying
    /// the run's id (issue #395) and the operator's run request (issue #154)
    /// when one was supplied.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: Arc<HarnessPool>,
        deps: HarnessDeps,
        record: CompanyRecord,
        company: CompanyId,
        workflow_id: String,
        run_id: String,
        run_request: Option<String>,
        notices: RunNotices,
        board: RunBoard,
        blocks: RunBlocks,
        approvals: RunApprovals,
        board_claim: Arc<crate::harness::orchestrator::DelegationClaim>,
    ) -> Self {
        Self {
            pool,
            deps,
            record,
            company,
            workflow_id,
            run_id,
            run_request,
            notices,
            board,
            blocks,
            approvals,
            board_claim,
        }
    }

    /// Drains this run's delegation bucket after a node's turn and records what it
    /// did (issue #661 / M5).
    ///
    /// Runs inside the run's board scope, so both drains read **this run's**
    /// bucket rather than whatever `Unscoped` happens to hold.
    ///
    /// Two drains, in this order and both mandatory:
    ///
    /// 1. **Refusals** — desks a `delegate_to_desk` named that the company does not
    ///    have. This is the live half of the defect PR #771 identified: the tool
    ///    pushes these *before* consulting the claim, so before the scoped claim
    ///    they landed in the shared bucket and a concurrent chat turn recorded them
    ///    on its own card. Surfaced as a run notice — the run's own surface, and the
    ///    only one it has.
    /// 2. **Board writes** — executed through
    ///    [`DelegationRunner::execute_board_writes`](crate::runtime::delegation),
    ///    which is infallible by signature, so this cannot fail the node.
    ///
    /// # Never fails the node
    ///
    /// Nothing here returns a `Result`. The turn already happened and its output is
    /// valid; a store hiccup must not discard it. Same stance
    /// [`park_gated_calls`](Self::park_gated_calls) takes, arrived at the same way.
    async fn drain_board_writes(&self) {
        let queue = &self.deps.delegations;

        // Issue #661: the run's own refusals, on the run's own surface.
        for desk in queue.drain_refusals(MAX_DELEGATIONS_PER_TURN) {
            tracing::warn!(
                company = %self.company,
                workflow = %self.workflow_id,
                run_id = %self.run_id,
                "workflow agent node: a hand-off named a desk this company does not have; nothing \
                 was handed off"
            );
            self.notices.push(format!(
                "A step in this workflow tried to hand work to the \"{desk}\" desk, which this \
                 company does not have. Nothing was handed off."
            ));
        }

        let staged = queue.drain(MAX_DELEGATIONS_PER_TURN);
        if staged.is_empty() {
            return;
        }
        let runner = crate::runtime::delegation::DelegationRunner::for_workflow_run(
            &self.record,
            self.deps.tasks.as_ref(),
            // Never touched: the only registration site is the `delegate_to_desk`
            // arm, which a board claim makes unstageable. Threaded because the
            // shared runner needs one, and the company's own rather than a fresh
            // one so a future reachable path would surface in the operator's
            // in-flight list rather than in a registry nobody can see.
            &self.deps.steer,
            &self.company,
            queue,
            crate::runtime::delegation::WorkflowRunRef {
                run_id: self.run_id.clone(),
                workflow_id: self.workflow_id.clone(),
            },
        );
        let rows = runner.execute_board_writes(staged).await;
        // Issue #661 (M5): a write that did not land is told to the operator, not
        // only logged and rowed. A run has no conversation to say it in, so the
        // notice channel is the only surface that reaches somebody — and this is
        // the one board outcome an operator cannot infer from the board itself,
        // because the evidence of it is precisely the card that is missing.
        //
        // Structural wording only: the card's own title, never the store's error
        // text. Same split `DeliveryReason` keeps against `DeliveryReport::detail`.
        for row in &rows {
            let notice = match row.action {
                crate::ports::WorkflowBoardAction::SpawnFailed => Some(format!(
                    "A step in this workflow could not open the card \"{}\" on the board.",
                    row.title.as_deref().unwrap_or("(untitled)")
                )),
                crate::ports::WorkflowBoardAction::AssignFailed => Some(format!(
                    "A step in this workflow could not set the owner of card {}.",
                    row.task_id.as_deref().unwrap_or("(unknown)")
                )),
                _ => None,
            };
            if let Some(notice) = notice {
                self.notices.push(notice);
            }
        }
        self.board.extend(rows);
    }

    /// Parks every approval-gated tool call this node's turn just recorded
    /// (issue #395) — the drain the workflow path never had.
    ///
    /// # The hole this closes
    ///
    /// [`ApprovalPolicy`](crate::harness::policy::ApprovalPolicy) is installed
    /// pool-wide on every roster agent with the shared
    /// [`ApprovalRequestQueue`](crate::harness::policy::ApprovalRequestQueue),
    /// so a gated tool call inside a workflow agent node **was** recorded. But
    /// the only drain in the codebase is
    /// [`park_approval_requests`](crate::harness::HarnessBrain), which lives
    /// inside `run_cycle` and needs a
    /// [`CycleHost`](crate::ports::brain::CycleHost). This path —
    /// `run_agent` → [`HarnessPool::run_background`] → `run_inner` — never goes
    /// near a cycle, so nothing drained, and the next chat cycle's
    /// [`clear`](crate::harness::policy::ApprovalRequestQueue::clear) threw the
    /// request away. The queue's own doc names this case. The leak was
    /// prevented; the parking was never added. That is why the Approvals page
    /// stayed "All clear" through a run an operator watched get gated.
    ///
    /// # Scope, not boundary (issue #439)
    ///
    /// This block used to describe a boundary index: `from` taken before the
    /// turn, only the tail above it claimed, because the queue was shared with
    /// whatever chat cycle happened to be running and `drain` would have taken
    /// that cycle's entries and cleared the rest.
    ///
    /// It also said, accurately, that this **narrowed** the race rather than
    /// eliminating it — a chat turn pushing while this node ran landed above
    /// the boundary and was parked here with this run's id on it — and that the
    /// real fix was a per-run queue, deferred out of #395.
    ///
    /// That is now done, though **not** in the shape that sentence predicted.
    /// One queue per run is unbuildable: `ApprovalPolicy` is installed by
    /// `build_roster` inside a fingerprint-cached, per-company
    /// `HarnessPool::ensure` with no run id in scope, and is then sealed into
    /// the vendored agent with no setter, so there is nowhere to hand a
    /// per-run queue *to*. The separation is in the key instead — the run takes
    /// an [`ApprovalScope::Run`](crate::harness::policy::ApprovalScope) claim
    /// and pushes route into its own bucket — which yields the same property
    /// the issue asked for: a turn sees only its own requests.
    ///
    /// It also closes a race the boundary never addressed. Two workflow runs
    /// overlap (they are spawned, not under the cycle lock), and both took a
    /// boundary against the same vector, so the later `split_off` swallowed the
    /// earlier run's tail. Scopes are disjoint, so that cannot happen.
    ///
    /// # Never fails the node
    ///
    /// A park that errors is logged per entry and the loop continues. The turn
    /// already happened, the model was already told it was refused, and failing
    /// the node here would discard a completed turn's work over a queue write.
    /// Same stance `park_approval_requests` takes, for the same reason.
    ///
    /// # A run cancelled mid-turn parks nothing, deliberately
    ///
    /// Stopping a run drops the engine future *mid-await* (issue #383), which
    /// takes this call with it — so a call the policy had already gated is
    /// discarded rather than parked. That is the intended outcome, not a
    /// residual leak: an operator who stopped a run is not asking to be asked
    /// about the work they stopped. It is the same judgement `cancelled_run`
    /// makes in reporting no `pending_approvals`, and the same one
    /// `park_pending_gates` makes in skipping a cancelled run.
    ///
    /// Issue #439 made this **cleaner, and no longer anyone else's business**.
    /// The discard used to be performed by the next chat cycle's `clear`, which
    /// only worked because the queue was shared — the cancelled run's leftovers
    /// were sitting in the cycle's way. Now the claim's `Drop` takes them as the
    /// dropped future unwinds, so the entries never outlive the run and no
    /// other turn has to sweep up after it.
    ///
    /// # It returns what it learned (issues #881, #880)
    ///
    /// This used to return `()`, and every one of its three outcomes ended at a
    /// `tracing` line. That is the whole mechanism behind #881: the one function
    /// that knew a node's deliverable had been parked told nobody, so `run_agent`
    /// returned `Ok` and the engine marked the node green. It now hands back a
    /// [`ParkedCalls`] summary the caller turns into a blocked node, and files a
    /// per-call [`WorkflowRunApprovalRow`](crate::ports::WorkflowRunApprovalRow)
    /// receipt on [`RunApprovals`] for every one of the three outcomes —
    /// including, especially, the two that failed.
    ///
    /// No differencing is needed to know which requests are "ours": the bucket
    /// is already run-scoped ([`ApprovalScope::Run`], issue #439), so everything
    /// the drain returns was queued by this run's own turn.
    async fn park_gated_calls(&self, node_id: Option<&str>) -> ParkedCalls {
        let mut summary = ParkedCalls::default();
        let mut rows: Vec<crate::ports::WorkflowRunApprovalRow> = Vec::new();
        let row = |tool: Option<String>,
                   outcome: crate::ports::WorkflowApprovalOutcome,
                   approval_id: Option<String>| {
            crate::ports::WorkflowRunApprovalRow {
                node_id: node_id.map(str::to_string),
                tool,
                outcome,
                approval_id,
            }
        };
        let queue = &self.deps.approval_requests;
        // Issue #242's stamp. The `from` is now 0 because the scope *is* the
        // entitlement: every entry in this bucket was pushed by this run's own
        // turn, so there is no prefix belonging to anyone else to skip past.
        // That is what #439 bought — the boundary index encoded a guess about
        // who wrote what, and the scope encodes the fact.
        queue.stamp_run(0, &self.run_id);
        // The discard count comes off the drain itself (issue #561): `drain`
        // caps and drops the remainder in one step, so by the time it returns,
        // how many went is not recoverable from what came back. Reading it
        // here is what keeps the overflow warning below reachable — without a
        // count, a run that flooded the gate looks identical to one that did
        // not.
        let drained = queue.drain(MAX_APPROVAL_REQUESTS_PER_TURN);
        let notice = drained.overflow_notice();
        let discarded = drained.discarded;
        let requests = drained.requests;

        // Issue #638: told to the operator, not only logged. Raised BEFORE the
        // parking guard below, and that ordering is a fix in itself — the guard
        // `return`s, so on a runtime with no approvals gate the overflow was
        // not even reaching the log. The notice is about calls that were
        // *discarded*, which is true whether or not the survivors could be
        // parked; if anything it matters more when they could not.
        if let Some(notice) = notice {
            // `overflow_notice` rather than a sentence of our own: the wording
            // lives on `DrainedRequests` (#561) precisely so the chat path and
            // this one cannot tell an operator the same thing two ways.
            self.notices.push(notice);
        }
        if discarded > 0 {
            tracing::warn!(
                company = %self.company,
                run_id = %self.run_id,
                discarded,
                "workflow agent node: more gated tool calls than one run may park; the excess \
                 was discarded"
            );
            // Issue #880: a receipt per dropped call, with no tool name — the
            // drain caps and drops in one step, so by the time the count is
            // known the entries are gone. A row that says "one more was
            // dropped" is honest; a tool guessed from the survivors would not
            // be. These are the worst rows on the surface: nobody is being
            // asked about these calls at all.
            summary.unparkable += discarded;
            for _ in 0..discarded {
                rows.push(row(
                    None,
                    crate::ports::WorkflowApprovalOutcome::Discarded,
                    None,
                ));
            }
        }
        if requests.is_empty() {
            // Issue #900: this used to return before the discarded handling
            // above ran, so a drain that discarded every request and parked
            // none filed no receipt at all — `summary.unparkable` stayed 0 and
            // the node read as clean. The discard bookkeeping now happens
            // first; this only has to flush what it recorded.
            self.approvals.extend(rows);
            return summary;
        }

        let Some(parking) = self
            .deps
            .delivery
            .as_ref()
            .and_then(|delivery| delivery.parking.as_ref())
        else {
            // Loud: these requests are already off the queue and are the only
            // trace of calls the operator will never be asked about.
            tracing::error!(
                company = %self.company,
                run_id = %self.run_id,
                requests = requests.len(),
                "workflow agent node: gated tool calls could NOT be parked — this runtime has \
                 no approvals queue wired; the operator will not be asked about them"
            );
            // Issue #880: the loudest arm, and the one that had no record at
            // all beyond the line above. Every request here is a call the
            // operator will never see a card for, so each gets a receipt and
            // each counts as unparkable — which is what makes the node's
            // diagnosis say "could not be parked" rather than the much softer
            // "waiting on approval".
            summary.unparkable += requests.len();
            for request in requests {
                push_tool(&mut summary.tools, &request.tool);
                rows.push(row(
                    Some(request.tool.clone()),
                    crate::ports::WorkflowApprovalOutcome::ParkFailed,
                    None,
                ));
            }
            self.approvals.extend(rows);
            return summary;
        };

        for request in requests {
            push_tool(&mut summary.tools, &request.tool);
            // The delivery precedent: a workflow run has no board card behind it
            // and no conversation to raise the request in, so it is recorded
            // explicitly unlinked (#333) and stays Approvals-page-only (#379).
            match parking
                .park_and_journal(
                    &self.company,
                    request.effect,
                    crate::runtime::journal::TaskLink::Unlinked,
                    None,
                )
                .await
            {
                Ok(approval_id) => {
                    tracing::info!(
                        company = %self.company,
                        run_id = %self.run_id,
                        tool = %request.tool,
                        approval_id = %approval_id,
                        "workflow agent node: parked a gated tool call for operator approval"
                    );
                    // Issue #881: the knowledge that used to die on the line
                    // above. This id is what the node's block is decidable
                    // through, and what the run's receipt names.
                    summary.approval_ids.push(approval_id.to_string());
                    rows.push(row(
                        Some(request.tool.clone()),
                        crate::ports::WorkflowApprovalOutcome::Parked,
                        Some(approval_id.to_string()),
                    ));
                }
                Err(err) => {
                    tracing::error!(
                        company = %self.company,
                        run_id = %self.run_id,
                        tool = %request.tool,
                        %err,
                        "workflow agent node: failed to park a gated tool call; the operator will \
                         not be asked about it"
                    );
                    summary.unparkable += 1;
                    rows.push(row(
                        Some(request.tool.clone()),
                        crate::ports::WorkflowApprovalOutcome::ParkFailed,
                        None,
                    ));
                }
            }
        }
        self.approvals.extend(rows);
        summary
    }
}

/// Appends `tool` to a first-seen-ordered, deduplicated list.
///
/// A turn that calls the same gated tool three times should say the tool's name
/// once — the count of *calls* is on the approval rows, and repeating the name
/// in the blocked-node summary would only make the console's sentence read as
/// though three different things were blocked.
fn push_tool(tools: &mut Vec<String>, tool: &str) {
    if !tools.iter().any(|seen| seen == tool) {
        tools.push(tool.to_string());
    }
}

#[async_trait]
impl AgentRunner for HarnessAgentRunner {
    async fn run_agent(
        &self,
        agent_ref: &str,
        request: Value,
        _conn: Option<&str>,
    ) -> TfResult<Value> {
        // Issue #782: fold the resolved upstream node output into the turn.
        // `translate` binds `input = "=items"` on every agent node, so the engine
        // resolves it to the previous step's output before calling us; without
        // this fold that output had no channel into the agent's turn and was
        // dropped (a `agent -> agent` pipeline's second teammate saw nothing).
        // The static `prompt` still leads (`message_from_request`); the upstream
        // input is appended under a labelled heading, then the #154 run topic.
        //
        // Issue #849: bounded on the way in. Nothing used to limit what a fan-in
        // folded here, so three `web_fetch` payloads were concatenated verbatim
        // and the turn intermittently died on a provider context-window 400 —
        // after the fetches were already paid for. The budget is applied before
        // the request is composed, so the boundary is decided by us rather than
        // discovered by the provider.
        let budget = upstream::budget_chars(
            self.deps
                .provider
                .profile()
                .and_then(|profile| profile.max_input_tokens),
        );
        let (instruction, upstream_report) =
            append_upstream_input(&message_from_request(&request), &request, budget);
        if let Some(notice) = upstream_report.notice() {
            tracing::warn!(
                company = %self.company,
                workflow = %self.workflow_id,
                run_id = %self.run_id,
                agent = agent_ref,
                budget,
                "workflow agent node: upstream input exceeded this step's budget and was truncated"
            );
            self.notices.push(notice);
        }
        let message = compose_turn_message(&instruction, self.run_request.as_deref());
        // Issue #881: which node this is. `translate` writes it in the
        // first-class config layer beside `agent_ref` (config cannot shadow
        // it), because the vendored `AgentRunner` boundary carries no node
        // identity of its own. `None` for a hand-built request in a test, or a
        // graph compiled before #881 — the block is then recorded at run level,
        // which is the honest fallback rather than a name invented here.
        let node_id = request
            .get("node_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string);
        tracing::debug!(
            company = %self.company,
            agent = agent_ref,
            "workflow agent node: routing to harness pool"
        );
        // Issue #439: this run's own approval scope, replacing #395's boundary
        // index. The index was only ever a narrowing — it was taken against a
        // vector any concurrent turn could append to, so a chat cycle pushing
        // inside the window landed above the boundary and was parked here with
        // this run's id on it, and two concurrent runs each took a boundary
        // against the same vector so the later `split_off` swallowed the
        // earlier one's tail. A scope removes both by construction: nothing
        // else can write into this bucket.
        let claim = self
            .deps
            .approval_requests
            .claim(ApprovalScope::Run(self.run_id.clone()));
        // Issue #661 (M5): the turn AND its post-turn drains run inside the run's
        // board scope, so a `spawn_task` the model calls files into this run's
        // bucket and the drain below reads that same bucket back. The claim itself
        // was taken once for the whole run (see `build_capabilities`); this only
        // installs its ambient scope for the span of this node's turn.
        //
        // **Every layer here is `Box::pin`ed, and that is load-bearing.** This
        // nests one task-local scope inside another (`ApprovalScope` inside
        // `DelegationScope`), and `TaskLocalFuture` stores its inner future
        // *inline* — so without boxing, an openhuman agent turn (already a very
        // large future) is held by value inside two nested wrappers and the
        // composed state blows the thread's stack. Verified: it overflows on the
        // first spawning run without these.
        let turn = Box::pin(async {
            let outcome = claim
                .scoped(Box::pin(self.pool.run_background(
                    &self.company,
                    agent_ref,
                    &message,
                    &self.deps,
                )))
                .await;
            // Drained on BOTH arms, deliberately. A turn that errored may still have
            // had a tool call gated before it failed, and that request is just as
            // real — dropping the claim without parking would discard it, which is
            // the exact disappearance this issue is about.
            //
            // Inside the scope, so the drain reads this run's bucket rather than
            // whatever `Unscoped` happens to hold.
            //
            // Issue #880: the receipts it files therefore survive a failed turn
            // too. Only the #881 *block* below is gated on the turn having
            // succeeded — a turn that genuinely errored is a failure, and
            // reclassifying it as "blocked" would hide one behind an approval
            // nobody has answered.
            let parked = claim
                .scoped(Box::pin(self.park_gated_calls(node_id.as_deref())))
                .await;
            // Issue #661 (M5): likewise on both arms, and for the same reason. A
            // turn that failed after calling `spawn_task` had already been told the
            // card would be opened; refusing to drain would make that receipt false
            // and destroy the write when the scope ends.
            Box::pin(self.drain_board_writes()).await;
            (outcome, parked)
        });
        let (outcome, parked) = self.board_claim.scoped(turn).await;
        // Issue #849: a provider context-window refusal reaches the operator as
        // the run's error text, and the vendor's own wording ("Please start a
        // new chat") is unfollowable in a workflow — there is no chat and no
        // button. Rewrite that one class into what is actually too big and what
        // to do about it, keeping the provider's words at the end. Every other
        // failure passes through exactly as before.
        let outcome = outcome.map_err(|e| {
            let raw = e.to_string();
            let reported = upstream::context_overflow_advice(&raw).unwrap_or(raw);
            EngineError::Capability(format!("harness agent '{agent_ref}': {reported}"))
        })?;

        // ── Issue #881: a node whose deliverable was parked is BLOCKED ───────
        //
        // The turn "succeeded" — the model was refused inside its own tool
        // loop, wrote prose about it, and ended normally — so before this the
        // node returned `Ok`, the engine recorded `Success`, the edge fired,
        // and the `=items` binding delivered the apology to the next node as
        // if it were the spec it was supposed to be. Every node green, nothing
        // delivered.
        //
        // `Err` is the right channel and not merely a convenient one: `on_error`
        // defaults to `"stop"` and `retry.max_attempts` to `1`, so the branch
        // halts at this node with no retry re-running the turn. What it is NOT
        // is a failure — `WorkflowRun` reclassifies the node's row to
        // [`WorkflowNodeStatus::Blocked`](crate::ports::types::WorkflowNodeStatus)
        // and settles the run without an error, exactly as `cancelled` keeps a
        // deliberate stop out of the failure count.
        //
        // Deliberately NOT an engine pause. `NodeControl::Interrupt` discards
        // the activation's state and re-runs the node from the top on resume —
        // and an agent node is not re-enterable, so resuming would spend
        // another whole inference turn, call the same gated tool, and park a
        // NEW card. Approve → re-run → re-park, forever. The engine says so
        // itself: `StopReason::Paused` maps to "resuming a paused agent is not
        // supported yet".
        if !parked.is_empty() {
            self.blocks.push(crate::ports::WorkflowBlockedNode {
                node_id: node_id.clone().unwrap_or_else(|| agent_ref.to_string()),
                tools: parked.tools.clone(),
                approval_ids: parked.approval_ids.clone(),
                unparkable: parked.unparkable,
            });
            return Err(EngineError::Capability(blocked_diagnosis(
                node_id.as_deref(),
                agent_ref,
                &parked,
            )));
        }

        // Mirror the engine's `{ json, text, raw }` envelope shape: expose the
        // reply as `text` so a downstream `=item.text` binding resolves. A
        // workflow node carries no chat bubble, so the turn's steps are dropped
        // here (they surface only on operator/desk chat replies).
        Ok(json!({ "text": outcome.reply, "agent_ref": agent_ref }))
    }
}

/// The sentence a blocked node fails with (issue #881).
///
/// Modelled on the standard the `weekly-competitor-analysis` diagnosis set: name
/// the node, the teammate, the tool(s), the policy surface, how many approvals
/// are involved, and — the part an operator most needs — that the node produced
/// no deliverable, so nothing downstream of it ran.
///
/// **"Waiting on N approvals" and "N calls could not be parked" are different
/// sentences on purpose.** The second is strictly worse: there is no card, so
/// there is nothing to approve and no way to unblock the node short of changing
/// the policy and re-running. Collapsing them would tell an operator to go look
/// at an Approvals page that has nothing on it.
///
/// Composed from the structural summary, never from a store's error text or the
/// model's prose — this string reaches host logs.
fn blocked_diagnosis(node_id: Option<&str>, agent_ref: &str, parked: &ParkedCalls) -> String {
    let node = node_id.unwrap_or(agent_ref);
    let tools = if parked.tools.is_empty() {
        "a tool call".to_string()
    } else {
        parked
            .tools
            .iter()
            .map(|t| format!("`{t}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let waiting = parked.approval_ids.len();
    let mut what = Vec::new();
    if waiting > 0 {
        what.push(format!(
            "{waiting} approval{} {} waiting for you",
            if waiting == 1 { "" } else { "s" },
            if waiting == 1 { "is" } else { "are" }
        ));
    }
    if parked.unparkable > 0 {
        what.push(format!(
            "{} call{} could NOT be queued for approval at all, so nobody will be asked about \
             {}",
            parked.unparkable,
            if parked.unparkable == 1 { "" } else { "s" },
            if parked.unparkable == 1 { "it" } else { "them" }
        ));
    }
    format!(
        "workflow node '{node}' is blocked: {tools} needed approval before {agent_ref} could \
         finish, so the node produced no deliverable and nothing after it ran. {}. Approving \
         does not continue this run — decide the card, then run the workflow again.",
        what.join("; ")
    )
}

/// Extracts the turn message from an agent node's resolved config: the `prompt`
/// string when present (what [`translate`](crate::workflows::translate) writes),
/// else the `input`/`message` string, else the whole request serialized.
fn message_from_request(request: &Value) -> String {
    for key in ["prompt", "input", "message"] {
        if let Some(text) = request.get(key).and_then(Value::as_str) {
            return text.to_string();
        }
    }
    request.to_string()
}

/// The heading under which an agent node's turn carries its upstream step's
/// output (issue #782).
const UPSTREAM_INPUT_HEADING: &str = "## Input from the previous step";

/// Folds the resolved upstream node output into `instruction`.
///
/// `translate` binds `input = "=items"` on every agent node, so by the time the
/// engine calls [`run_agent`](AgentRunner::run_agent) the request's `input` field
/// holds the `json` of every predecessor item (the `=items` set). This appends a
/// rendering of that output under [`UPSTREAM_INPUT_HEADING`], *after* the node's
/// static instruction, so both the standing job (`prompt`) and this run's actual
/// upstream data reach the teammate.
///
/// # The byte-identical no-upstream path
///
/// When there is no renderable upstream output — the `input` key is absent (a
/// hand-built request, or a node whose binding an author cleared), it resolved to
/// `null`/`[]`, or every item was empty — `instruction` is returned **unchanged**.
/// A single-agent workflow with no predecessor therefore composes exactly the
/// message it did before #782, never a dangling empty heading. `message` shape is
/// then decided by [`compose_turn_message`] alone, as before.
///
/// # Bounded (issue #849)
///
/// `budget` is the most upstream text this turn may carry, and it is enforced
/// here rather than discovered by the provider. The returned
/// [`UpstreamReport`](upstream::UpstreamReport) says what the budget did — it is
/// empty of truncations for nearly every run, and the caller raises an operator
/// notice only when it is not.
fn append_upstream_input(
    instruction: &str,
    request: &Value,
    budget: usize,
) -> (String, upstream::UpstreamReport) {
    let Some((section, report)) = request
        .get("input")
        .and_then(|input| render_upstream_input(input, budget))
    else {
        return (instruction.to_string(), upstream::UpstreamReport::default());
    };
    let instruction = instruction.trim_end();
    let folded = if instruction.is_empty() {
        format!("{UPSTREAM_INPUT_HEADING}\n{section}")
    } else {
        format!("{instruction}\n\n{UPSTREAM_INPUT_HEADING}\n{section}")
    };
    (folded, report)
}

/// Renders the upstream envelope(s) an agent node received (`request["input"]`,
/// the resolved `=items` set) into the text the agent reads, bounded to `budget`
/// characters in total (issue #849).
///
/// Each predecessor item is a stable `{ json, text, raw }` envelope (see the
/// tinyflows `envelope` module), so the human-readable `text` (an upstream
/// agent's prose) is preferred; a non-agent node whose output carries no `text`
/// (a `tool_call` / `transform` / `output` node) is rendered as pretty JSON.
/// Multiple predecessors — a fan-in (`merge -> agent`) or several edges into one
/// agent — are all rendered, separated by a rule, so none is lost.
///
/// # Why the bound lives here and not at the `tool_call` node's own output
///
/// This is the **join**, and it is the only place the whole set is visible at
/// once. A cap at a `tool_call` node's output would bound each fetch separately
/// and still let three bounded fetches sum to an oversized turn, and it would
/// have to spend its cap blind to how many siblings were about to arrive. It
/// would also miss every other producer — an upstream *agent* node's reply is
/// unbounded in exactly the same way, and a `transform` node can manufacture a
/// large payload from a small one. Bounding at the join covers a single enormous
/// `web_fetch` and a three-way fan-in with one rule: the same
/// [`allocate_fairly`](upstream::allocate_fairly) call handles one source and N.
///
/// Returns `None` when nothing is renderable — an empty set, all-`null` items, or
/// empty containers — which is what keeps the no-upstream path byte-identical.
fn render_upstream_input(
    input: &Value,
    budget: usize,
) -> Option<(String, upstream::UpstreamReport)> {
    let items: Vec<&Value> = match input {
        Value::Array(items) => items.iter().collect(),
        Value::Null => return None,
        other => vec![other],
    };
    let rendered: Vec<String> = items.into_iter().filter_map(render_upstream_item).collect();
    if rendered.is_empty() {
        return None;
    }

    // Composition, budgeting and the markers that account for both live together
    // in `upstream`, so the guarantee — the section is never longer than `budget`,
    // *including* every marker and separator — is one function's postcondition
    // rather than a property spread across this loop and that module.
    Some(upstream::bound_sections(&rendered, budget))
}

/// Renders one upstream item into the text an agent reads.
///
/// A capability node's output is the stable `{ json, text, raw }` envelope, so an
/// envelope is unwrapped to its meaningful content — the prose `text` when it
/// carries any, else the structured `json` — never the whole envelope (whose
/// `raw` merely duplicates one of the two). A non-envelope value (a trigger
/// payload, a bare scalar) is rendered directly. `None` for anything with nothing
/// worth showing — a `null`, a blank string, an empty container, or an envelope
/// whose text is blank AND whose json is empty — so it is skipped rather than
/// emitting a blank block or an empty heading.
fn render_upstream_item(item: &Value) -> Option<String> {
    match item {
        Value::Null => None,
        Value::String(text) => {
            let text = text.trim();
            (!text.is_empty()).then(|| text.to_string())
        }
        // The `{ json, text, raw }` envelope: prefer prose, fall back to the
        // structured payload, skip when both are empty.
        Value::Object(map) if is_envelope(map) => {
            if let Some(text) = map.get("text").and_then(Value::as_str) {
                let text = text.trim();
                if !text.is_empty() {
                    return Some(text.to_string());
                }
            }
            map.get("json").and_then(render_upstream_item)
        }
        Value::Object(map) => {
            if map.is_empty() {
                return None;
            }
            Some(serde_json::to_string_pretty(item).unwrap_or_else(|_| item.to_string()))
        }
        Value::Array(inner) if inner.is_empty() => None,
        _ => Some(serde_json::to_string_pretty(item).unwrap_or_else(|_| item.to_string())),
    }
}

/// Whether `map` is a capability node's stable `{ json, text, raw }` output
/// envelope (the tinyflows `envelope` contract every capability node emits), so
/// [`render_upstream_item`] can unwrap it to its content rather than dumping the
/// redundant `raw`. A plain upstream object (a trigger payload) has no such shape
/// and is rendered whole.
fn is_envelope(map: &serde_json::Map<String, Value>) -> bool {
    map.contains_key("json") && map.contains_key("text") && map.contains_key("raw")
}

/// Combines a node's authored instruction with the operator's run request
/// (issue #154).
///
/// A node's `prompt` is baked into the graph, so it is identical on every run —
/// it says *what this step does*, never *what was asked this time*. Before this,
/// the run's topic stopped at the trigger node and the agent had no subject to
/// work on, which is what made a run end with the agent asking the operator for
/// a topic they had no field to supply.
///
/// The instruction stays first so the node's job still leads; the request is
/// appended under a labelled heading so a teammate can tell the standing
/// instruction from this run's subject. A blank or whitespace-only request is
/// treated as absent, leaving the message byte-identical to the previous
/// behaviour — runs that supply no topic are unchanged.
fn compose_turn_message(instruction: &str, run_request: Option<&str>) -> String {
    let request = run_request.map(str::trim).filter(|r| !r.is_empty());
    match request {
        Some(request) => {
            let instruction = instruction.trim();
            if instruction.is_empty() {
                return request.to_string();
            }
            format!("{instruction}\n\nRequest for this run:\n{request}")
        }
        None => instruction.to_string(),
    }
}

/// Extracts a human-readable run request from the trigger input (issue #154).
///
/// The console posts `{"request": "…"}`, but the run endpoint accepts an
/// arbitrary JSON trigger payload, so this also accepts a bare string and the
/// nearby key spellings a hand-written call or an older client may use. Anything
/// else (an object with no recognised key, a number, `null`) yields `None` and
/// the run proceeds exactly as it did before — the topic is an addition, not a
/// new requirement.
pub(super) fn run_request_text(input: &Value) -> Option<String> {
    let text = match input {
        Value::String(s) => s.as_str(),
        Value::Object(_) => ["request", "input", "topic", "message", "text"]
            .iter()
            .find_map(|key| input.get(*key).and_then(Value::as_str))?,
        _ => return None,
    };
    let trimmed = text.trim();
    (!trimmed.is_empty()).then_some(trimmed.to_string())
}

/// The message a bare-LLM call that is NOT the engine's schema auto-fix gets: an
/// `agent` node reached `llm` with no `agent_ref`.
/// [`translate`](crate::workflows::translate) always sets `agent_ref` for a roster
/// agent, so reaching that path means an agent node with no teammate assigned.
const BARE_LLM_UNWIRED_MESSAGE: &str = "workflow agent node has no roster agent; bare LLM \
     completion is not wired for company workflows";

/// The bare-completion fallback (`llm` capability), left unwired for company
/// workflows.
///
/// Two distinct callers reach this, and issue #661 (M4) is that they used to get
/// the same message:
///
/// * The engine's **output_parser auto-fix** (default on) calls `llm` to *repair*
///   a value that failed schema validation — handing us
///   `{ "task": "coerce_to_schema", "schema", "value", "errors": [ … ] }`. Because
///   this path errors here, the generic "bare LLM completion is not wired"
///   message used to **mask** the real failure (the schema errors) the operator
///   needed to see. Now that shape surfaces the schema failures and merely *notes*
///   that the repair path is unavailable.
/// * An **`agent` node with no `agent_ref`** lands here with the node config as
///   the request. That genuinely is "no roster agent", and keeps the original
///   message byte-identical.
struct UnwiredLlm;

#[async_trait]
impl LlmProvider for UnwiredLlm {
    async fn complete(&self, request: Value, _conn: Option<&str>) -> TfResult<Value> {
        // The engine's output-parser auto-fix asked us to coerce a value to a
        // schema and handed us the schema failures. Surface THOSE — the real
        // cause — rather than a message about the (unavailable) repair path,
        // which is what masked them before #661. Same `output_parser: value
        // failed schema validation:` lead the engine's non-auto-fix arm uses, so
        // the on_error-routed error reads identically whichever arm produced it.
        if let Some(errors) = auto_fix_schema_errors(&request) {
            return Err(EngineError::Capability(format!(
                "output_parser: value failed schema validation: {errors} (LLM auto-fix is not \
                 available: bare LLM completion is not wired for company workflows)"
            )));
        }
        // Any other request — an agent node with no `agent_ref`, whose request is
        // the node config — keeps the original message unchanged.
        Err(EngineError::Capability(
            BARE_LLM_UNWIRED_MESSAGE.to_string(),
        ))
    }
}

/// Extracts the joined schema-validation failures from the engine's output-parser
/// auto-fix request, if this is one.
///
/// The engine's `schema::parse_and_validate` asks `llm` to coerce a value to a
/// schema with `{ "task": "coerce_to_schema", "schema", "value", "errors": [ … ] }`,
/// where `errors` is the non-empty list of human-readable schema failures. Returns
/// those failures joined by `; ` for exactly that shape, and `None` for anything
/// else — including a `coerce_to_schema` request with a missing / empty / non-string
/// `errors` — so the caller falls back to the generic bare-LLM message. Matching on
/// the request shape (not the node kind) also covers the `agent` node's own
/// output-parser sub-port, which routes through the same engine helper.
fn auto_fix_schema_errors(request: &Value) -> Option<String> {
    if request.get("task").and_then(Value::as_str) != Some("coerce_to_schema") {
        return None;
    }
    let errors: Vec<&str> = request
        .get("errors")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    (!errors.is_empty()).then(|| errors.join("; "))
}

/// `code` nodes are not part of the OpenCompany model and never emitted by
/// translation; wired to an error for completeness.
struct UnwiredCode;

#[async_trait]
impl CodeRunner for UnwiredCode {
    async fn run(&self, _language: CodeLanguage, _source: &str, _input: Value) -> TfResult<Value> {
        Err(EngineError::Capability(
            "code execution is not supported for company workflows".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #638: a node that gates more calls than the cap allows leaves the
    /// operator a **notice**, not only a log line.
    ///
    /// Asserted on `RunNotices` — the value that becomes `WorkflowRun::notices`
    /// and then the journaled outcome the history panel reads — rather than on
    /// a log, which is what the issue asks for and what the chat path already
    /// had via #561.
    #[tokio::test]
    async fn an_overflowing_node_leaves_the_operator_a_notice() {
        let over = MAX_APPROVAL_REQUESTS_PER_TURN + 3;
        let (notices, queue) = overflowing_runner_notices(over, true).await;

        assert_eq!(notices.len(), 1, "one notice for one overflow: {notices:?}");
        let notice = &notices[0];
        assert!(
            notice.contains(&format!("at most {MAX_APPROVAL_REQUESTS_PER_TURN}")),
            "it must quote the cap that did the discarding: {notice}"
        );
        assert!(notice.contains('3'), "…and how many went past it: {notice}");
        assert_eq!(
            queue.drain(MAX_APPROVAL_REQUESTS_PER_TURN).requests.len(),
            0,
            "the drain already emptied this run's scope"
        );
    }

    /// The ordering fix that rides with it. The `parking`-is-`None` guard
    /// `return`s, and it used to sit **above** the overflow branch — so on a
    /// runtime with no approvals gate the discard was not even reaching the
    /// log, let alone the operator.
    ///
    /// That is the worst case, not a corner: the survivors could not be parked
    /// either, so the notice is the *only* thing the operator can be told.
    #[tokio::test]
    async fn the_notice_survives_a_runtime_with_no_approvals_gate() {
        let over = MAX_APPROVAL_REQUESTS_PER_TURN + 2;
        let (notices, _) = overflowing_runner_notices(over, false).await;
        assert_eq!(
            notices.len(),
            1,
            "no gate to park into is exactly when the operator most needs telling: {notices:?}"
        );
    }

    /// A node that stayed under the cap says nothing — the notice must be the
    /// exception, not a line on every run.
    #[tokio::test]
    async fn a_node_within_the_cap_raises_no_notice() {
        let (notices, _) = overflowing_runner_notices(MAX_APPROVAL_REQUESTS_PER_TURN, true).await;
        assert!(notices.is_empty(), "nothing was discarded: {notices:?}");
    }

    /// Queues `count` gated calls in a run's scope, drains them through
    /// `park_gated_calls`, and returns whatever the run was told.
    ///
    /// `with_gate` selects whether a `parking` sink is wired, which is the axis
    /// the guard-order test needs.
    async fn overflowing_runner_notices(
        count: usize,
        with_gate: bool,
    ) -> (Vec<String>, crate::harness::policy::ApprovalRequestQueue) {
        use crate::harness::policy::{ApprovalRequest, ApprovalScope};
        use crate::ports::types::{Effect, EffectGroup};

        let dir = tempfile::Builder::new()
            .prefix("oc-638-")
            .tempdir()
            .expect("tempdir");
        let (mut deps, _journal) =
            crate::workflows::gated_tool_turn_test::deps(String::new(), dir.path());
        if !with_gate {
            deps.delivery = None;
        }
        let queue = deps.approval_requests.clone();
        let notices = RunNotices::default();
        let board_claim = Arc::new(deps.delegations.claim_board("run-1"));
        let runner = HarnessAgentRunner::new(
            Arc::new(HarnessPool::new()),
            deps,
            crate::workflows::gated_tool_turn_test::record(),
            CompanyId::new("acme"),
            "wf-1".to_string(),
            "run-1".to_string(),
            None,
            notices.clone(),
            RunBoard::default(),
            RunBlocks::default(),
            RunApprovals::default(),
            board_claim,
        );

        // Pushed inside the run's own scope, exactly as its turn would.
        let claim = queue.claim(ApprovalScope::Run("run-1".to_string()));
        claim
            .scoped(async {
                for i in 0..count {
                    queue.push(ApprovalRequest {
                        tool: "shell".to_string(),
                        reason: "gated".to_string(),
                        effect: Effect {
                            kind: "shell".to_string(),
                            group: EffectGroup::Other,
                            amount_usd: None,
                            established_thread: false,
                            first_time_counterparty: false,
                            payload: json!({ "n": i }),
                            agent: Some("ceo".to_string()),
                            run_id: None,
                        },
                    });
                }
            })
            .await;
        claim.scoped(runner.park_gated_calls(Some("work"))).await;
        (notices.take(), queue)
    }

    #[test]
    fn message_prefers_prompt_then_input_then_message() {
        assert_eq!(
            message_from_request(&json!({ "prompt": "P", "input": "I" })),
            "P"
        );
        assert_eq!(message_from_request(&json!({ "input": "I" })), "I");
        assert_eq!(message_from_request(&json!({ "message": "M" })), "M");
    }

    // ── Issue #154: the operator's run request reaches the agent ──

    #[test]
    fn run_request_is_appended_under_a_labelled_heading() {
        let out = compose_turn_message("Draft the launch post.", Some("dark mode for iOS"));
        // The node's standing instruction still leads.
        assert!(out.starts_with("Draft the launch post."), "{out}");
        // …and this run's subject is distinguishable from it.
        assert!(out.contains("Request for this run:"), "{out}");
        assert!(out.contains("dark mode for iOS"), "{out}");
    }

    #[test]
    fn a_run_with_no_request_is_byte_identical_to_the_old_message() {
        // The guarantee that makes this safe to land: runs that supply no topic
        // must behave exactly as they did before.
        for empty in [None, Some(""), Some("   "), Some("\n\t ")] {
            assert_eq!(
                compose_turn_message("Draft the launch post.", empty),
                "Draft the launch post.",
                "empty request {empty:?} must not alter the message"
            );
        }
    }

    #[test]
    fn a_request_with_no_instruction_stands_on_its_own() {
        // No dangling heading when the node carries no usable instruction.
        assert_eq!(
            compose_turn_message("", Some("ship dark mode")),
            "ship dark mode"
        );
        assert_eq!(
            compose_turn_message("   ", Some("ship dark mode")),
            "ship dark mode"
        );
    }

    #[test]
    fn run_request_text_reads_the_console_payload_and_a_bare_string() {
        assert_eq!(
            run_request_text(&json!({ "request": "dark mode" })).as_deref(),
            Some("dark mode")
        );
        assert_eq!(
            run_request_text(&json!("dark mode")).as_deref(),
            Some("dark mode")
        );
        // Tolerated spellings from a hand-written call or an older client.
        for key in ["input", "topic", "message", "text"] {
            let mut payload = serde_json::Map::new();
            payload.insert(key.to_string(), json!("dark mode"));
            assert_eq!(
                run_request_text(&Value::Object(payload)).as_deref(),
                Some("dark mode"),
                "key {key} should be accepted"
            );
        }
        // Trimmed.
        assert_eq!(
            run_request_text(&json!({ "request": "  dark mode  " })).as_deref(),
            Some("dark mode")
        );
    }

    #[test]
    fn run_request_text_is_none_for_payloads_that_carry_no_topic() {
        // These are the shapes an existing caller already sends — none may start
        // injecting a topic into agent messages.
        for payload in [
            json!({}),
            json!(null),
            json!(42),
            json!({ "request": "" }),
            json!({ "request": "   " }),
            json!({ "unrelated": "value" }),
            json!({ "request": 7 }),
            json!(["dark mode"]),
        ] {
            assert_eq!(
                run_request_text(&payload),
                None,
                "payload {payload} must carry no topic"
            );
        }
    }

    #[test]
    fn message_falls_back_to_serialized_request() {
        // No known string key: fall back to the serialized object.
        let out = message_from_request(&json!({ "agent_ref": "x" }));
        assert!(out.contains("agent_ref"));
    }

    // ── Issue #782: the upstream node's output reaches the next agent's turn ──

    /// [`append_upstream_input`] under the shipped budget, keeping the #782
    /// tests reading about *what reaches the turn* rather than about the #849
    /// budget they are all far below. The truncation report those calls discard
    /// has its own tests below.
    fn folded(request: &Value) -> String {
        append_upstream_input(
            &message_from_request(request),
            request,
            upstream::DEFAULT_UPSTREAM_BUDGET_CHARS,
        )
        .0
    }

    /// The headline. An `agent -> agent` pipeline's second teammate must receive
    /// the first's output. `translate` binds `input = "=items"`, the engine
    /// resolves it to the predecessor envelope, and this proves the runner folds
    /// that envelope's prose into the turn — under a heading, AFTER the node's
    /// own instruction, so both survive.
    #[test]
    fn upstream_output_is_folded_into_the_turn() {
        // The shape the engine hands us: `prompt` (the node's static job) plus
        // `input` (the resolved `=items`) — one predecessor agent envelope.
        let request = json!({
            "prompt": "Write the launch post.",
            "input": [{ "json": {}, "text": "The analyst found a 20% MoM jump.", "raw": {} }],
        });
        let message = folded(&request);
        // The node's own instruction still leads.
        assert!(message.starts_with("Write the launch post."), "{message}");
        // …the upstream output is present, under its heading…
        assert!(message.contains(UPSTREAM_INPUT_HEADING), "{message}");
        assert!(
            message.contains("The analyst found a 20% MoM jump."),
            "the previous step's output must reach the turn: {message}"
        );
    }

    /// Fan-in: a `merge -> agent` (or several edges into one agent) resolves
    /// `=items` to EVERY predecessor, and all of them must be delivered — the
    /// "loses all but the first" failure is exactly what `=items` (not `=item`)
    /// guards against.
    #[test]
    fn fan_in_delivers_every_predecessor() {
        let request = json!({
            "prompt": "Combine the research.",
            "input": [
                { "json": {}, "text": "Predecessor A: market is up.", "raw": {} },
                { "json": {}, "text": "Predecessor B: sentiment is positive.", "raw": {} },
            ],
        });
        let message = folded(&request);
        assert!(
            message.contains("Predecessor A: market is up."),
            "first predecessor missing: {message}"
        );
        assert!(
            message.contains("Predecessor B: sentiment is positive."),
            "second predecessor missing — a fan-in must not lose all but the first: {message}"
        );
    }

    /// A non-agent predecessor (a `tool_call` / `transform` / `output` node) has
    /// no prose `text`, so its structured output is rendered as JSON rather than
    /// dropped.
    #[test]
    fn a_structured_predecessor_is_rendered_as_json() {
        let request = json!({
            "prompt": "Summarise the fetch.",
            "input": [{ "json": { "rows": 3 }, "text": null, "raw": { "rows": 3 } }],
        });
        let message = folded(&request);
        assert!(message.contains(UPSTREAM_INPUT_HEADING), "{message}");
        assert!(
            message.contains("\"rows\""),
            "structured output rendered: {message}"
        );
    }

    /// The byte-identical guarantee. A single-agent workflow with no predecessor
    /// (no `input`, or an empty / all-null / empty-container `input`) composes
    /// exactly the pre-#782 message — never a dangling empty heading.
    #[test]
    fn no_upstream_output_is_byte_identical() {
        let base = "Draft the launch post.";
        for input in [
            None,
            Some(json!(null)),
            Some(json!([])),
            Some(json!([null])),
            Some(json!([{}])),
            Some(json!([{ "json": {}, "text": "   ", "raw": {} }])),
        ] {
            let mut request = serde_json::Map::new();
            request.insert("prompt".to_string(), json!(base));
            if let Some(input) = input.clone() {
                request.insert("input".to_string(), input);
            }
            let request = Value::Object(request);
            assert_eq!(
                folded(&request),
                base,
                "input {input:?} must not alter the message or add an empty heading"
            );
            // And the whole composition (including the #154 run topic) is
            // unchanged from what `compose_turn_message` alone would produce.
            let instruction = folded(&request);
            assert_eq!(
                compose_turn_message(&instruction, Some("ship dark mode")),
                compose_turn_message(base, Some("ship dark mode")),
                "the no-upstream path must leave the run-topic composition untouched"
            );
        }
    }

    /// Upstream output and the #154 run topic coexist: the node's instruction
    /// leads, the previous step's output follows under its heading, and the run's
    /// subject follows under its own — all three reach the teammate.
    #[test]
    fn upstream_output_and_run_topic_coexist() {
        let request = json!({
            "prompt": "Write the post.",
            "input": [{ "json": {}, "text": "ANALYST_SAID_THIS", "raw": {} }],
        });
        let instruction = folded(&request);
        let message = compose_turn_message(&instruction, Some("dark mode launch"));
        assert!(message.starts_with("Write the post."), "{message}");
        assert!(message.contains(UPSTREAM_INPUT_HEADING), "{message}");
        assert!(message.contains("ANALYST_SAID_THIS"), "{message}");
        assert!(message.contains("Request for this run:"), "{message}");
        assert!(message.contains("dark mode launch"), "{message}");
    }

    // ── Issue #849: nothing may hand an agent node an unbounded payload ──
    //
    // Driven by synthetic oversized payloads, never by a live page: the reported
    // failure is intermittent *because* it depends on how much text a sports
    // section happened to return that minute, so a test that reproduced it that
    // way would be a coin flip too.

    /// One predecessor envelope carrying `chars` characters of page-like text —
    /// the shape a `web_fetch` `tool_call` node emits (its non-JSON output is
    /// wrapped as `{"text": …}`, which the tinyflows envelope lifts to `text`).
    fn source_envelope(marker: &str, chars: usize) -> Value {
        let body = format!("{marker}{}", "x".repeat(chars.saturating_sub(marker.len())));
        json!({ "json": { "text": body.clone() }, "text": body, "raw": { "text": body } })
    }

    /// How much slack above the budget the markers, the heading and the section
    /// rules are allowed to add. They sit **outside** the budget deliberately —
    /// the budget exists to bound upstream *text*, and letting our own accounting
    /// compete for room would mean the truncation marker could itself be the
    /// thing squeezed out (the reasoning `memory_loop`'s skipped-hit marker
    /// arrived at first).
    const MARKER_SLACK: usize = 2_000;

    /// The reported shape: three fetched sources fan in to one ranking agent.
    /// Every source must still be represented, the turn must be bounded, and
    /// every cut must be visible.
    #[test]
    fn a_three_way_fan_in_is_bounded_and_no_source_is_lost() {
        let budget = upstream::DEFAULT_UPSTREAM_BUDGET_CHARS;
        let request = json!({
            "prompt": "Rank today's stories.",
            "input": [
                source_envelope("SOURCE_ONE", 200_000),
                source_envelope("SOURCE_TWO", 200_000),
                source_envelope("SOURCE_THREE", 200_000),
            ],
        });
        let (message, report) =
            append_upstream_input(&message_from_request(&request), &request, budget);

        assert!(
            message.chars().count() <= budget + MARKER_SLACK,
            "600k characters of upstream input must not reach a turn: {} characters",
            message.chars().count()
        );
        // …and every source is still *there*, which is what separates a bound
        // from "drop everything after the first".
        for marker in ["SOURCE_ONE", "SOURCE_TWO", "SOURCE_THREE"] {
            assert!(message.contains(marker), "{marker} was lost entirely");
        }
        // Each cut is visible to the agent.
        assert_eq!(
            message.matches("TRUNCATED BY OPENCOMPANY").count(),
            3,
            "every truncated source carries its own marker: {message}"
        );
        assert!(message.contains("source 3 of 3"), "{message}");

        // …and to the operator.
        assert_eq!(report.sources.len(), 3);
        assert!(report.truncated_any());
        let notice = report.notice().expect("the operator is told");
        assert!(notice.contains("3 sources"), "{notice}");
        assert!(notice.contains("3 of them were truncated"), "{notice}");
    }

    /// The "is it only a fan-in?" question, answered: it is not. A **single**
    /// enormous `web_fetch` into one agent runs the same unbounded path, and the
    /// bound at the join covers it with no second rule.
    #[test]
    fn a_single_enormous_source_is_bounded_too() {
        let budget = upstream::DEFAULT_UPSTREAM_BUDGET_CHARS;
        let request = json!({
            "prompt": "Summarise this page.",
            "input": [source_envelope("ONLY_SOURCE", 500_000)],
        });
        let (message, report) =
            append_upstream_input(&message_from_request(&request), &request, budget);

        assert!(
            message.chars().count() <= budget + MARKER_SLACK,
            "a single 500k-character page must not reach a turn whole: {} characters",
            message.chars().count()
        );
        assert!(message.contains("ONLY_SOURCE"), "the source still arrives");
        assert!(message.contains("source 1 of 1"), "{message}");
        assert_eq!(report.sources.len(), 1);
        assert!(report.truncated_any());
    }

    /// A large sibling must not starve a small one — the fan-in failure mode a
    /// flat per-source cap would not fix and a running total would make
    /// order-dependent.
    #[test]
    fn a_short_source_survives_whole_beside_an_enormous_one() {
        let budget = upstream::DEFAULT_UPSTREAM_BUDGET_CHARS;
        let short = "SHORT_SOURCE: the wire service filed three lines today.";
        let request = json!({
            "prompt": "Rank today's stories.",
            "input": [
                source_envelope("HUGE_SOURCE", 400_000),
                json!({ "json": {}, "text": short, "raw": {} }),
            ],
        });
        let (message, report) =
            append_upstream_input(&message_from_request(&request), &request, budget);

        assert!(
            message.contains(short),
            "the short source must arrive intact, not be crowded out: {message}"
        );
        assert_eq!(
            message.matches("TRUNCATED BY OPENCOMPANY").count(),
            1,
            "only the enormous source is cut: {message}"
        );
        assert_eq!(report.sources[1].produced, report.sources[1].kept);
        assert!(report.sources[0].kept < report.sources[0].produced);
    }

    /// The overwhelmingly common run: everything fits, so the fold is exactly
    /// what #782 produced and the operator is told nothing new.
    #[test]
    fn an_ordinary_fan_in_is_untouched_and_says_nothing() {
        let budget = upstream::DEFAULT_UPSTREAM_BUDGET_CHARS;
        let request = json!({
            "prompt": "Combine the research.",
            "input": [
                { "json": {}, "text": "Predecessor A: market is up.", "raw": {} },
                { "json": {}, "text": "Predecessor B: sentiment is positive.", "raw": {} },
            ],
        });
        let (message, report) =
            append_upstream_input(&message_from_request(&request), &request, budget);

        assert!(!message.contains("TRUNCATED"), "{message}");
        assert!(!report.truncated_any());
        assert_eq!(report.notice(), None);
        assert!(
            message.contains("Predecessor A: market is up."),
            "{message}"
        );
        assert!(
            message.contains("Predecessor B: sentiment is positive."),
            "{message}"
        );
    }

    /// The bound survives composition: the marker is still in the message the
    /// teammate is actually sent, alongside the node's instruction and the #154
    /// run topic.
    #[test]
    fn the_truncation_marker_survives_into_the_composed_turn() {
        let request = json!({
            "prompt": "Rank today's stories.",
            "input": [source_envelope("BIG_SOURCE", 200_000)],
        });
        let (instruction, _) = append_upstream_input(
            &message_from_request(&request),
            &request,
            upstream::DEFAULT_UPSTREAM_BUDGET_CHARS,
        );
        let message = compose_turn_message(&instruction, Some("today's sport"));
        assert!(message.starts_with("Rank today's stories."), "{message}");
        assert!(message.contains("TRUNCATED BY OPENCOMPANY"), "{message}");
        assert!(message.contains("Request for this run:"), "{message}");
        assert!(message.contains("today's sport"), "{message}");
    }

    /// A thousand-way fan-in — a `split_out` over a large array is all it takes —
    /// must not smuggle a thousand truncation markers past the budget. This is
    /// the fold-level twin of `upstream`'s
    /// `a_thousand_oversized_sources_stay_inside_the_budget`, driven through the
    /// real envelope shape rather than pre-rendered strings, because that is the
    /// path a graph actually takes.
    #[test]
    fn a_thousand_way_fan_in_cannot_smuggle_its_markers_past_the_budget() {
        let budget = upstream::DEFAULT_UPSTREAM_BUDGET_CHARS;
        let inputs: Vec<Value> = (0..1_000)
            .map(|n| source_envelope(&format!("SOURCE_{n}"), 5_000))
            .collect();
        let request = json!({ "prompt": "Rank today's stories.", "input": inputs });
        let (message, report) =
            append_upstream_input(&message_from_request(&request), &request, budget);

        // The section itself is bounded by `budget`; the message adds only the
        // node's own instruction and the heading, which are not upstream text.
        assert!(
            message.chars().count() <= budget + MARKER_SLACK,
            "5,000,000 characters of upstream input across 1,000 sources produced a {}-character \
             turn",
            message.chars().count()
        );
        assert_eq!(report.sources.len(), 1_000, "every input is accounted for");
        let notice = report.notice().expect("the operator is told");
        assert!(notice.contains("1000 sources"), "{notice}");
    }

    /// A source rendered as JSON (a `transform` / structured `tool_call` output,
    /// which has no prose `text`) is bounded on the same path — the bound is on
    /// what the turn carries, not on which node kind produced it.
    #[test]
    fn a_structured_source_is_bounded_on_the_same_path() {
        let budget = upstream::DEFAULT_UPSTREAM_BUDGET_CHARS;
        let rows: Vec<Value> = (0..20_000)
            .map(|n| json!({ "headline": format!("story {n}"), "score": n }))
            .collect();
        let request = json!({
            "prompt": "Rank these.",
            "input": [{ "json": { "rows": rows }, "text": null, "raw": {} }],
        });
        let (message, report) =
            append_upstream_input(&message_from_request(&request), &request, budget);

        assert!(
            message.chars().count() <= budget + MARKER_SLACK,
            "a structured payload is bounded too: {} characters",
            message.chars().count()
        );
        assert!(message.contains("TRUNCATED BY OPENCOMPANY"), "{message}");
        assert!(report.truncated_any());
    }

    #[test]
    fn workflow_workspace_is_unique_per_run_and_traversal_safe() {
        let root = std::path::Path::new("/tmp/workspaces");
        let company = CompanyId::new("acme");
        let first = workflow_workspace(root, &company, "../billing", "run:1");
        let second = workflow_workspace(root, &company, "../billing", "run:2");

        assert_ne!(first, second);
        assert!(first.starts_with(root.join("acme").join("_workflow")));
        assert!(!first.to_string_lossy().contains("../billing"));
        assert_eq!(
            first.file_name().and_then(|part| part.to_str()),
            Some("workspace")
        );
    }

    /// Issue #499. tinyflows 0.6 added `Capabilities::memory`, and this pins the
    /// answer we gave it.
    ///
    /// `None` is a decision, not an omission — see the comment at the field. A
    /// `MemoryProvider` here would let a workflow read and *write* agent memory
    /// (`remember`/`forget` are on the trait), and which scopes a workflow may
    /// touch is a policy question this repo has not answered. Until it is,
    /// unwired is the honest state: a `memory` node fails with a capability
    /// error rather than quietly writing somewhere nobody authorised.
    ///
    /// So this test is here to make wiring it a *deliberate* act. Whoever
    /// changes it has to change this line too, which is where they will find the
    /// question they need to answer first.
    #[tokio::test]
    async fn the_memory_capability_is_left_unwired_on_purpose() {
        let dir = tempfile::tempdir().expect("tempdir");
        // No endpoint is spawned: `build_capabilities` assembles a struct of
        // handles and never calls the provider, so a base URL that answers
        // nothing is sufficient and keeps this off the network.
        let (deps, _journal) = crate::workflows::gated_tool_turn_test::deps(
            "http://127.0.0.1:1/unused".to_string(),
            dir.path(),
        );
        let record = crate::workflows::gated_tool_turn_test::record();

        let caps = build_capabilities(
            Arc::new(HarnessPool::new()),
            deps,
            &record,
            RunContext {
                workflow_id: "wf",
                run_id: "run:1",
                run_request: None,
                dry_run: false,
                notices: RunNotices::default(),
                board: RunBoard::default(),
                blocks: Default::default(),
                approvals: Default::default(),
            },
        )
        .await
        .expect("build_capabilities");

        assert!(
            caps.memory.is_none(),
            "wiring `Capabilities::memory` gives workflows read AND write access \
             to agent memory — settle which scopes a workflow may touch before \
             changing this, and say so at the field"
        );
        // The neighbouring optional capability IS wired, so this is a statement
        // about `memory` specifically rather than about the bundle being empty.
        assert!(
            caps.agent.is_some(),
            "agent capability should still be wired"
        );
    }

    /// Issue #542 — T9: a dry bundle wires the effect STUBS (agent / tools / http
    /// all echo with the `dry_run` marker) and the inert `NoopState`, while the
    /// read-only resolver stays real. Pinned behaviourally through the marker, so
    /// a future refactor that quietly wired a real effect into a dry bundle fails
    /// here.
    #[tokio::test]
    async fn a_dry_bundle_wires_stubs_and_noop_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (deps, _journal) = crate::workflows::gated_tool_turn_test::deps(
            "http://127.0.0.1:1/unused".to_string(),
            dir.path(),
        );
        let record = crate::workflows::gated_tool_turn_test::record();

        let caps = build_capabilities(
            Arc::new(HarnessPool::new()),
            deps,
            &record,
            RunContext {
                workflow_id: "wf",
                run_id: "run:1",
                run_request: None,
                dry_run: true,
                notices: RunNotices::default(),
                board: RunBoard::default(),
                blocks: Default::default(),
                approvals: Default::default(),
            },
        )
        .await
        .expect("build_capabilities");

        // http: the stub echoes without sending, carrying the marker.
        let http_out = caps
            .http
            .request(json!({ "url": "http://127.0.0.1:9/" }), None)
            .await
            .expect("dry http never fails");
        assert_eq!(
            http_out["dry_run"],
            json!(true),
            "http slot should be the dry stub"
        );

        // agent: the stub echoes with no pool routing.
        let agent = caps.agent.as_ref().expect("agent stub is wired");
        let agent_out = agent
            .run_agent("ceo", json!({ "prompt": "hi" }), None)
            .await
            .expect("dry agent never fails");
        assert_eq!(
            agent_out["dry_run"],
            json!(true),
            "agent slot should be the dry stub"
        );

        // state: NoopState — a load reads None and a store is dropped.
        assert_eq!(caps.state.load("k").await.expect("noop load"), None);
        caps.state.store("k", json!(1)).await.expect("noop store");
        assert_eq!(
            caps.state.load("k").await.expect("noop load"),
            None,
            "dry state must be the inert NoopState, never durable"
        );
    }

    // ── Issue #661 (M4): the unwired `llm` stub reports the RIGHT failure ──

    /// T1 — the engine's output_parser auto-fix request (it calls `llm` to repair
    /// a schema mismatch) surfaces the SCHEMA errors, not the generic bare-LLM
    /// lead that used to mask them.
    #[tokio::test]
    async fn unwired_llm_surfaces_schema_errors_on_auto_fix_request() {
        let request = json!({
            "task": "coerce_to_schema",
            "schema": { "type": "object", "required": ["name", "age"] },
            "value": { "other": 1 },
            "errors": [
                "$: missing required property `name`",
                "$: missing required property `age`",
            ],
        });
        let EngineError::Capability(msg) = UnwiredLlm
            .complete(request, None)
            .await
            .expect_err("an unwired llm must error")
        else {
            panic!("expected a capability error");
        };
        // The real cause is present…
        assert!(
            msg.contains("failed schema validation"),
            "should carry the schema-validation lead: {msg}"
        );
        assert!(
            msg.contains("missing required property `name`")
                && msg.contains("missing required property `age`"),
            "should carry the specific schema failures: {msg}"
        );
        // …and it does NOT lead with the generic bare-LLM message that hid them.
        assert!(
            !msg.starts_with("workflow agent node has no roster agent"),
            "the schema failure must not be masked by the generic lead: {msg}"
        );
    }

    /// T2 — any other request (here an agent node with no `agent_ref`, whose
    /// request is the node config) keeps the generic message byte-identical.
    #[tokio::test]
    async fn unwired_llm_keeps_generic_message_for_non_auto_fix_request() {
        let EngineError::Capability(msg) = UnwiredLlm
            .complete(json!({ "prompt": "hi" }), None)
            .await
            .expect_err("an unwired llm must error")
        else {
            panic!("expected a capability error");
        };
        assert_eq!(
            msg, BARE_LLM_UNWIRED_MESSAGE,
            "a non-auto-fix request must get the byte-identical generic message"
        );
    }

    /// T4 — a `coerce_to_schema` request whose `errors` is empty or missing (or
    /// not an array of strings) falls back to the generic message rather than
    /// emitting an empty schema-error string or panicking.
    #[tokio::test]
    async fn unwired_llm_falls_back_when_auto_fix_carries_no_errors() {
        for request in [
            json!({ "task": "coerce_to_schema" }),
            json!({ "task": "coerce_to_schema", "errors": [] }),
            json!({ "task": "coerce_to_schema", "errors": "oops" }),
            json!({ "task": "coerce_to_schema", "errors": [1, 2] }),
        ] {
            let EngineError::Capability(msg) = UnwiredLlm
                .complete(request.clone(), None)
                .await
                .expect_err("an unwired llm must error")
            else {
                panic!("expected a capability error for {request}");
            };
            assert_eq!(
                msg, BARE_LLM_UNWIRED_MESSAGE,
                "a coerce_to_schema request with no usable errors must fall back \
                 to the generic message: {request}"
            );
        }
    }

    // ── Issue #661 (L2): a workspace mkdir failure aborts the live build ──

    /// T5 — live mode with an impossible `workspace_root` (a path rooted under a
    /// regular file) fails the build with a `Harness` error naming the path and
    /// the underlying I/O cause, instead of warning past it and handing back a
    /// bundle whose effects are rooted at a directory that does not exist.
    #[tokio::test]
    async fn build_capabilities_live_errors_when_workspace_cannot_be_created() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A regular file where a directory would need to be: `create_dir_all`
        // under it fails with ENOTDIR.
        let not_a_dir = dir.path().join("not-a-dir");
        std::fs::write(&not_a_dir, b"x").expect("write file");

        let (mut deps, _journal) = crate::workflows::gated_tool_turn_test::deps(
            "http://127.0.0.1:1/unused".to_string(),
            dir.path(),
        );
        deps.workspace_root = not_a_dir.clone();
        let record = crate::workflows::gated_tool_turn_test::record();

        // `Capabilities` is not `Debug`, so match rather than `expect_err`.
        let err = match build_capabilities(
            Arc::new(HarnessPool::new()),
            deps,
            &record,
            RunContext {
                workflow_id: "wf",
                run_id: "run:1",
                run_request: None,
                dry_run: false, // live: the workspace mkdir runs
                notices: RunNotices::default(),
                board: RunBoard::default(),
                blocks: Default::default(),
                approvals: Default::default(),
            },
        )
        .await
        {
            Ok(_) => panic!("an uncreatable workspace must fail the build"),
            Err(err) => err,
        };

        let crate::error::OpenCompanyError::Harness(msg) = &err else {
            panic!("expected a Harness error, got {err:?}");
        };
        assert!(
            msg.contains("could not create its workspace directory"),
            "message should name the failure: {msg}"
        );
        assert!(
            msg.contains("not-a-dir"),
            "message should name the offending path: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("not a directory"),
            "message should carry the underlying I/O cause: {msg}"
        );
    }

    /// T6 — the same impossible root is harmless for a dry run: it builds no
    /// workspace, so the bundle assembles fine.
    #[tokio::test]
    async fn build_capabilities_dry_ignores_an_impossible_workspace_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let not_a_dir = dir.path().join("not-a-dir");
        std::fs::write(&not_a_dir, b"x").expect("write file");

        let (mut deps, _journal) = crate::workflows::gated_tool_turn_test::deps(
            "http://127.0.0.1:1/unused".to_string(),
            dir.path(),
        );
        deps.workspace_root = not_a_dir;
        let record = crate::workflows::gated_tool_turn_test::record();

        build_capabilities(
            Arc::new(HarnessPool::new()),
            deps,
            &record,
            RunContext {
                workflow_id: "wf",
                run_id: "run:1",
                run_request: None,
                dry_run: true, // dry: no workspace mkdir at all
                notices: RunNotices::default(),
                board: RunBoard::default(),
                blocks: Default::default(),
                approvals: Default::default(),
            },
        )
        .await
        .expect("a dry build never touches the workspace");
    }
}
