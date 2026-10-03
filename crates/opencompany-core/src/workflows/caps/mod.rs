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
//!   the company memory ports, approval policy, and cost metering.
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
/// Issue #1866: the deterministic postcondition tier of the sufficiency gate —
/// mechanical predicates over a node's output, evaluated before the node's
/// success settles.
mod postcondition;
pub(crate) mod resolver;
mod state;
mod tools;
/// Issue #849: how much upstream output one agent node's turn may carry, and
/// what to say when a provider refuses the turn on its context window anyway.
mod upstream;

use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{Value, json};
use tinyflows::caps::{
    AgentRunOutcome, AgentRunRequest, AgentRunner, Capabilities, CodeLanguage, CodeRunner,
    HttpClient, LlmProvider, StateStore, StopReason, ToolInvoker, WorkflowResolver,
};
use tinyflows::error::{EngineError, Result as TfResult};
use tinyflows::transcript::TranscriptEntry;

use crate::harness::orchestrator::MAX_DELEGATIONS_PER_TURN;
use crate::harness::policy::{ApprovalScope, MAX_APPROVAL_REQUESTS_PER_TURN, PolicyMode};
use crate::harness::{HarnessDeps, toolbelt};
use crate::ports::types::{CompanyId, CompanyRecord};
use crate::runtime::delegation::RunTurn;

use self::http::GuardedHttpClient;
use self::resolver::StoreWorkflowResolver;
use self::state::{CompanyStateStore, NoopState};
use self::tools::WorkflowToolInvoker;
pub(crate) use self::tools::{
    MissingReason, WORKFLOW_TOOL_CATALOG, WORKFLOW_TOOL_NAMESPACES, WorkflowToolInfo,
    WorkflowToolWiring, grants_workflow_namespace, workflow_tool_info, workflow_tool_wiring,
};
/// Issue #849: the ceiling on what one agent node's turn carries from upstream.
/// Re-exported so the end-to-end fan-in proof
/// ([`agent_upstream_input_tests`](crate::workflows::agent_upstream_input_tests))
/// asserts against the shipped number rather than a copy of it — which is the
/// only caller outside this module, hence the `cfg`.
#[cfg(test)]
pub(crate) use self::upstream::DEFAULT_UPSTREAM_BUDGET_CHARS;
// `WORKFLOW_TOOL_SLUGS` stays module-private to `tools` since #813: the catalogue
// (`WORKFLOW_TOOL_CATALOG`) is what callers ground and validate against, and the
// slug table is now only its in-module pinning cross-check.

tokio::task_local! {
    /// Set for the span of a peer consultation. A consulted orchestrator can
    /// run a whole graph, whose nodes reach this same recover path, so the
    /// "one peer turn" bound holds down the stack only while this is read.
    static PEER_CONSULT_ACTIVE: ();
}

/// Whether a peer consultation is already running on this task.
fn peer_consult_active() -> bool {
    PEER_CONSULT_ACTIVE.try_with(|_| ()).is_ok()
}

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
    /// Stable tinyflows lineage used by checkpointed node continuations.
    pub checkpoint_thread_id: &'a str,
    /// The graph's [`content_fingerprint`](crate::company::WorkflowFile::content_fingerprint)
    /// as loaded for this run attempt (issue #1991 review, `3904397452`/
    /// `3904304754`). Threaded to [`HarnessAgentRunner`] so
    /// [`park_gated_calls`](HarnessAgentRunner::park_gated_calls) can stamp it
    /// onto a blocked node's stash the same way `park_pending_gates` stamps
    /// one onto a parked gate's effect — the fact
    /// `spawn_blocked_node_continuation` needs to refuse a checkpoint resume
    /// into a graph an editor changed while the block sat pending.
    pub workflow_fingerprint: &'a str,
    /// The operator's topic for this run (issue #154), threaded to the agent
    /// capability so a node's turn carries what was actually asked.
    pub run_request: Option<String>,
    /// The trigger payload this run was started with (issue #1825, P1
    /// follow-up). Threaded to [`HarnessAgentRunner`] so a blocked node's
    /// in-memory continuation stash can be armed at park time — see
    /// [`park_gated_calls`](HarnessAgentRunner::park_gated_calls) — rather than
    /// only after the engine settles, which is what let an approval decided in
    /// that window be consumed with nothing to release.
    pub trigger_input: &'a Value,
    /// This run's own attribution (issue #1862 prerequisite), threaded to
    /// [`HarnessAgentRunner`] so [`park_gated_calls`](HarnessAgentRunner::park_gated_calls)
    /// arms a blocked node's continuation stash with the run's real
    /// `started_by` at park time — `arm` is first-write-wins, and park time
    /// runs before the runner's block-settle pass, so this call is the one
    /// that actually sticks; leaving it defaulted here would silently pin
    /// every blocked node to `Operator` regardless of who triggered the run.
    pub started_by: crate::ports::types::StartedBy,
    /// Issue #542: stub every effectful slot and journal nothing.
    pub dry_run: bool,
    /// Where an agent node leaves an operator-facing notice (issue #638).
    pub notices: RunNotices,
    /// Where an agent node's board writes are recorded (issue #661 / M5).
    pub board: RunBoard,
    /// Where an agent node records that it blocked on a human (issue #881).
    pub blocks: RunBlocks,
    /// Where an agent node records that its turn truncated at the
    /// `max_tool_iterations` cap (issue #1865), so the runner can relabel that
    /// node's row `Error` and agree with the attempt, which already settles
    /// `Failed` for exactly this signal.
    pub capped: RunCappedNodes,
    /// Where a semantic judge records an intentional no-work conclusion.
    pub halted: RunHaltedNodes,
    /// Where an agent node records the approvals its turn parked (issue #880).
    pub approvals: RunApprovals,
    /// Files agent nodes wrote during this run, keyed by node for durable output.
    pub artifacts: RunArtifacts,
    /// Where each `agent` node's turn is recorded as an attempt. `None` on a
    /// dry run and in tests, which then behave exactly as they did before
    /// attempts existed.
    pub runs: Option<Arc<dyn crate::ports::RunStore>>,
    /// The unredacted companion store for those attempts' steps.
    pub deep: Option<Arc<dyn crate::ports::deep_trace::DeepTraceStore>>,
    /// Collects which attempt each node ran as, for the run's journal events.
    pub attempts: RunAttempts,
    /// Per-run record of every child graph the resolver gated, so the parent's
    /// parking path can name a child pause (issue #617). Created by the runner
    /// before the engine call, handed to the resolver through `ChildPolicyGates`,
    /// and read back when the run pauses. Crate-internal plumbing: the registry
    /// type is not part of the public surface, so the field is not `pub`.
    pub(crate) child_gates: Arc<resolver::ChildGateRegistry>,
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
/// `turn`/`deps` are shared with the rest of the harness surface — the roster the
/// agent nodes address is the one resident in the harness(es) the turn routes to.
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
    turn: Arc<dyn RunTurn>,
    deps: HarnessDeps,
    record: &CompanyRecord,
    run: RunContext<'_>,
) -> crate::error::Result<Capabilities> {
    let RunContext {
        workflow_id,
        run_id,
        checkpoint_thread_id,
        workflow_fingerprint,
        run_request,
        trigger_input,
        started_by,
        dry_run,
        notices,
        board,
        blocks,
        capped,
        halted,
        approvals,
        artifacts,
        runs,
        deep,
        attempts,
        child_gates,
    } = run;
    let company = record.id.clone();
    // Issue #562: the tier actually in force — the operator's console override
    // when one is set, the manifest's otherwise. Reading `manifest.policy` here
    // would leave a workflow run on the shipped tier while the roster ran on the
    // operator's, which is the disagreement `effective_policy` exists to prevent.
    let mode = PolicyMode::parse(&record.effective_policy().mode);
    let grants = record.manifest.tools.allow.clone();
    // The process-wide handle may intentionally be present without a
    // deployment credential so a company key added later inherits its shared
    // ledger. Resolve the effective credential before using presence as the
    // workflow-wiring verdict.
    let search_daily_call_cap = record
        .manifest
        .tools
        .search_daily_calls
        .unwrap_or(crate::company::DEFAULT_SEARCH_DAILY_CALLS);
    let search_granted = crate::company::grants_search_explicit(&grants);
    let managed_search = if dry_run {
        // Dry runs install inert effect slots and deliberately never consult
        // the secret store. Preserve the deployment wiring verdict only.
        deps.search.clone()
    } else if !search_granted {
        None
    } else {
        match (&deps.search, &deps.secrets) {
            (Some(backend), Some(secrets)) => {
                let company_key =
                    crate::company::search::load_managed_key(&company, secrets.as_ref()).await?;
                (backend.credential.configured() || company_key.is_some()).then(|| {
                    backend
                        .clone()
                        .with_daily_call_cap(search_daily_call_cap)
                        .with_company_credential(company.clone(), secrets.clone())
                })
            }
            (Some(backend), None) if backend.credential.configured() => {
                Some(backend.clone().with_daily_call_cap(search_daily_call_cap))
            }
            _ => None,
        }
    };
    let mut wiring_deps = deps.clone();
    wiring_deps.search = managed_search.clone();
    let wiring = workflow_tool_wiring(&wiring_deps);

    // sub_workflow-by-id resolves children from the union of the company's seed
    // `workflows/` directory and the record's runtime-authored bodies — so a
    // platform tenant with no source dir still resolves the workflows it
    // created (issue #168). Read before `deps` may move into the agent runner.
    // REAL in both modes: it is a read, and a dry sub_workflow child runs under
    // this same (dry) bundle, so dry propagates rather than stopping here.
    // Issue #617: child graphs are translated inside the engine, after the
    // top-level gate pass. Give the resolver the same live policy and grants so
    // it can mark those graphs before tinyflows runs them. `None` for a dry run
    // because every effect slot is inert there.
    let gates = (!dry_run).then(|| self::resolver::ChildPolicyGates {
        policy_hitl_enabled: false,
        policy: record.effective_policy(),
        run_id: run_id.to_string(),
        grants: deps.approval_requests.grants(),
        registry: child_gates.clone(),
    });
    let resolver: Arc<dyn WorkflowResolver> = Arc::new(StoreWorkflowResolver::new(
        deps.workflow_source_dir.clone(),
        deps.store.clone(),
        company.clone(),
        workflow_id.to_string(),
        gates,
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
            Arc::new(dry_run::DryRunHttp::new(
                record.manifest.tools.web_allowed_domains.clone(),
            )),
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
            managed_search.as_ref(),
            deps.tenant_search.as_ref(),
            search_metering,
            wiring,
        )
        .with_emergency_gate(deps.emergency_gate.clone());
        let http = GuardedHttpClient::new(exec_security, web_allowed_domains)
            .with_emergency_gate(deps.emergency_gate.clone());

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
        // The publish tool is captured by the fingerprint-cached roster agent,
        // so a workflow run cannot replace its queue handle. Scope only refused
        // publishes: staged publishes remain unavailable to runs because they
        // still have no destination to claim.
        let publish_refusal_claim = Arc::new(
            deps.pending_publishes
                .claim_refusals_for_run(run_id.to_string()),
        );

        // `deps` moves in last — the borrows above (`deps.capabilities`,
        // `deps.search`, `deps.meter`, `deps.secrets`, `deps.workspace_root`,
        // `deps.delegations`) are all done by here.
        let agent: Arc<dyn AgentRunner> = Arc::new(
            HarnessAgentRunner::new(
                turn,
                deps,
                record.clone(),
                company.clone(),
                workflow_id.to_string(),
                run_id.to_string(),
                run_request,
                trigger_input.clone(),
                started_by,
                notices,
                board,
                blocks,
                capped,
                approvals,
                artifacts,
                board_claim,
                publish_refusal_claim,
            )
            .with_halted(halted)
            .with_checkpoint_thread_id(checkpoint_thread_id)
            .with_workflow_fingerprint(workflow_fingerprint)
            .with_runs(runs, deep, attempts),
        );
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
///
/// What changed in issue #1192 is who hears about that refusal. It used to be
/// told only to the model, which meant the only operator-visible trace was
/// whatever prose the model wrote in reaction — and that prose became the node's
/// output and rode the `=items` binding downstream while the run scored clean,
/// which is the same shape #881 fixed for the *gated* case one paragraph up.
/// The refusal is now recorded as a typed fact on the queue where it is raised
/// and drained after every node turn into a run notice; see
/// [`drain_publish_refusals`](HarnessAgentRunner::drain_publish_refusals). The
/// tool's answer is unchanged — only the silence around it is.
///
/// Whether a run *should* be able to publish is a separate question this does
/// not settle: `origin_run_id` (M5 / issue #661) taught runs to open cards,
/// which arguably makes the "a run has nowhere to file one" premise stale.
pub struct HarnessAgentRunner {
    /// The turn a workflow agent node runs on: the lane-aware router in a
    /// multi-harness company, the default lane over the pool in a
    /// single-harness one (see `run_workflow`'s single-pool entrypoint).
    turn: Arc<dyn RunTurn>,
    /// Where a node's turn is recorded as an attempt, when this host records
    /// one. `None` leaves every node exactly as it behaved before attempts
    /// existed.
    runs: Option<Arc<dyn crate::ports::RunStore>>,
    /// The unredacted companion store, handed to the sink so a node's reasoning
    /// and raw tool I/O are kept beside its scrubbed steps.
    deep: Option<Arc<dyn crate::ports::deep_trace::DeepTraceStore>>,
    /// Where each node reports the attempt it ran as, so the run's journal
    /// events can carry the join.
    attempts: RunAttempts,
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
    checkpoint_thread_id: String,
    /// The graph's [`content_fingerprint`](crate::company::WorkflowFile::content_fingerprint)
    /// at the moment this run started (issue #1991 review, `3904397452`/
    /// `3904304754`), stamped onto every blocked-node stash [`park_gated_calls`]
    /// arms so `spawn_blocked_node_continuation` can refuse a checkpoint resume
    /// into a graph an editor changed while the block sat pending — the same
    /// check `graph_unchanged_since_park` already applies to the gate path.
    /// `None` only in a build/test that never set it, which behaves exactly as
    /// it did before this field existed.
    workflow_fingerprint: Option<String>,
    /// What the operator asked for on this run (issue #154), when they supplied
    /// it. A node's `prompt` is authored into the graph and is the same on every
    /// run, so without this the run's topic never reaches the teammate doing the
    /// work — the agent would run, find no subject, and ask for one.
    run_request: Option<String>,
    /// The trigger payload this run was started with (issue #1825, P1
    /// follow-up). Carried so [`park_gated_calls`](Self::park_gated_calls) can
    /// arm a blocked node's continuation stash itself, at park time, instead
    /// of leaving that to the runner's block-settle pass — see that method's
    /// doc for the window this closes.
    trigger_input: Value,
    /// This run's own attribution (issue #1862 prerequisite), carried so
    /// [`park_gated_calls`](Self::park_gated_calls) can arm a blocked node's
    /// continuation stash with it at park time — see that method's doc for
    /// why the value stamped here, not the settle-time call, is the one that
    /// survives `arm`'s first-write-wins semantics.
    started_by: crate::ports::types::StartedBy,
    /// Where this node leaves an operator-facing notice (issue #638).
    notices: RunNotices,
    /// Where this node's board writes are recorded (issue #661 / M5).
    board: RunBoard,
    /// Where this node records that it blocked on a human (issue #881).
    blocks: RunBlocks,
    /// Where this node records that its turn truncated at the
    /// `max_tool_iterations` cap (issue #1865).
    capped: RunCappedNodes,
    /// Nodes the judge concluded were benignly unnecessary.
    halted: RunHaltedNodes,
    /// Where this node records the approvals its turn parked (issue #880).
    approvals: RunApprovals,
    /// Run-scoped files captured after each node turn, including failed turns.
    artifacts: RunArtifacts,
    /// The run's [`DrainClaim::Board`](crate::harness::orchestrator::DrainClaim)
    /// claim, taken once by [`build_capabilities`] and held for the whole run.
    ///
    /// Shared rather than per-turn because `claim_as` clears the scope's bucket on
    /// acquire and the engine runs same-superstep nodes concurrently — a per-turn
    /// claim would let one node destroy a sibling's staged writes. See the
    /// acquisition site for the full reasoning.
    board_claim: Arc<crate::harness::orchestrator::DelegationClaim>,
    /// The run's refusal bucket on the shared publish queue. The cached
    /// `PublishArtifactTool` reads this task-local scope when it refuses a
    /// publish, so sibling runs cannot drain each other's notices.
    publish_refusal_claim: Arc<crate::harness::publish::PublishRefusalClaim>,
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

/// One file a workflow agent node wrote during its turn.
///
/// A workflow run has no card, so this metadata rides beside the engine result
/// and is folded into the durable per-node output snapshot by the runner. The
/// body itself lives in the shared workspace node named here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct RunArtifact {
    source: String,
    title: String,
    kind: crate::ports::ArtifactKind,
    workspace_node_id: String,
    captured_at_millis: u64,
}

/// Run-scoped collector for node-written files.
///
/// Owned by the runner rather than the capability bundle so entries survive an
/// engine failure or block, both of which drop the bundle before persistence.
#[derive(Clone, Default)]
pub struct RunArtifacts {
    inner: Arc<std::sync::Mutex<std::collections::BTreeMap<String, Vec<RunArtifact>>>>,
}

impl RunArtifacts {
    fn push(&self, node_id: &str, artifact: RunArtifact) {
        let mut guard = self.inner.lock().expect("run artifacts poisoned");
        let rows = guard.entry(node_id.to_string()).or_default();
        if let Some(existing) = rows.iter_mut().find(|row| row.source == artifact.source) {
            *existing = artifact;
        } else {
            rows.push(artifact);
        }
    }

    /// Takes every captured row as JSON, keyed by graph node id.
    pub fn take(&self) -> serde_json::Map<String, Value> {
        std::mem::take(&mut *self.inner.lock().expect("run artifacts poisoned"))
            .into_iter()
            .map(|(node, rows)| {
                let value = serde_json::to_value(rows).unwrap_or_else(|_| Value::Array(Vec::new()));
                (node, value)
            })
            .collect()
    }
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

/// Where an agent node records that its turn truncated at the
/// `max_tool_iterations` cap (issue #1865).
///
/// [`RunBlocks`]' shape, and for a sibling reason: `tinyflows::observability`
/// reports a capped turn's step as `Success` — the model produced a reply,
/// [`HarnessAgentRunner::run`](AgentRunner::run) returned `Ok`, the edge
/// fired — so nothing at that boundary can tell a finished answer from a
/// truncated one apart. `run_turn` is the one place that already tells them
/// apart, on the exact signal (`outcome.hit_iteration_cap`) that settles this
/// node's attempt row [`RunStatus::Failed`](crate::ports::RunStatus::Failed)
/// a few lines above where this is pushed — so this collector carries that
/// SAME fact sideways to the runner rather than a second detector re-deriving
/// its own reading of the same turn, which is exactly the kind of disagreement
/// issue #1865 exists to close. The runner's `reclassify_capped_nodes` reads
/// it back and relabels the matching row [`WorkflowNodeStatus::Error`], the
/// same host-side move `reclassify_blocked` makes for a parked node.
///
/// PR #1883 review: also carries a budget-paused node's id, for the same
/// reason — `outcome.budget_paused` settles the attempt row `Failed` right
/// beside `hit_iteration_cap` a few lines below, and the engine's boundary
/// cannot tell that turn apart from a finished one any more than it can a
/// capped one. One channel, one reconciliation pass; the name stayed
/// `RunCappedNodes` rather than widening to something like
/// `RunDegradedNodes` because renaming a `pub` type mid-fix is its own
/// review surface and every caller already reads it as "this row disagrees
/// with its attempt," not literally "hit the iteration cap."
///
/// Cheap to clone; every clone appends to the same list.
#[derive(Clone, Default)]
pub struct RunCappedNodes {
    inner: Arc<std::sync::Mutex<Vec<String>>>,
}

impl RunCappedNodes {
    /// Records that `node_id`'s turn truncated at the iteration cap, or (PR
    /// #1883) paused for lack of inference budget — either way, a turn whose
    /// row must be relabeled to agree with its `Failed` attempt.
    pub fn push(&self, node_id: String) {
        self.inner
            .lock()
            .expect("run capped-nodes poisoned")
            .push(node_id);
    }

    /// Whether `node_id`'s turn was recorded here, **without** draining
    /// (issue #1865, CodeRabbit review on #1905).
    ///
    /// The progress collector needs this at the moment it journals a node's
    /// `WorkflowNodeFinished`, and [`take`](Self::take) cannot serve it: the
    /// settle-time `reclassify_capped_nodes` still has to see the same list
    /// afterwards. Reading rather than draining is what lets the durable event
    /// and the in-memory row agree about the same node.
    pub fn contains(&self, node_id: &str) -> bool {
        self.inner
            .lock()
            .expect("run capped-nodes poisoned")
            .iter()
            .any(|id| id == node_id)
    }

    /// Takes everything recorded so far, leaving the collector empty.
    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.inner.lock().expect("run capped-nodes poisoned"))
    }
}

/// Node ids whose semantic judge returned a benign halt.
#[derive(Clone, Default)]
pub struct RunHaltedNodes {
    inner: Arc<std::sync::Mutex<Vec<String>>>,
}

impl RunHaltedNodes {
    pub fn push(&self, node_id: String) {
        self.inner
            .lock()
            .expect("run halted-nodes poisoned")
            .push(node_id);
    }

    pub fn contains(&self, node_id: &str) -> bool {
        self.inner
            .lock()
            .expect("run halted-nodes poisoned")
            .iter()
            .any(|id| id == node_id)
    }

    /// Drops any earlier record of `node_id`.
    ///
    /// Codex review on #1990: when `retry.max_attempts > 1`, tinyflows re-runs
    /// this node's whole turn on the same [`HarnessAgentRunner`] — the same
    /// `self.halted` an earlier attempt may have already pushed into if that
    /// attempt's judge answered `halt_benign`. Called at the start of every
    /// fresh attempt for a node, so a later attempt that actually succeeds is
    /// never shadowed by a stale benign-halt marker a prior, retried attempt
    /// left behind.
    pub fn retract(&self, node_id: &str) {
        self.inner
            .lock()
            .expect("run halted-nodes poisoned")
            .retain(|id| id != node_id);
    }

    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.inner.lock().expect("run halted-nodes poisoned"))
    }
}

/// Which attempt each `agent` node ran as.
///
/// The fourth channel in the [`RunNotices`] / [`RunBoard`] / [`RunBlocks`]
/// family, and it exists for the structural reason the first three do: the
/// engine's observer is handed an `ExecutionStep`, which knows a node's id,
/// status and duration but nothing about an attempt row the host minted inside
/// the node's own turn. The id has to travel sideways or not at all.
///
/// It could have ridden the node's output instead — and that is exactly the
/// mistake [`RunBlocks`] documents: `run_agent`'s return value *becomes* the
/// node's output, so a non-output fact placed there lands in the next node's
/// `=items` binding.
///
/// Cheap to clone; every clone writes to the same map, which is what lets a
/// run's several agent nodes — including concurrent siblings — each record their
/// own.
#[derive(Clone, Default)]
pub struct RunAttempts {
    inner: Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
}

impl RunAttempts {
    /// Records that `node_id` ran as attempt `run_id`.
    pub fn record(&self, node_id: impl Into<String>, run_id: impl Into<String>) {
        self.inner
            .lock()
            .expect("run attempts poisoned")
            .insert(node_id.into(), run_id.into());
    }

    /// The attempt a node ran as, if it opened one.
    #[must_use]
    pub fn get(&self, node_id: &str) -> Option<String> {
        self.inner
            .lock()
            .expect("run attempts poisoned")
            .get(node_id)
            .cloned()
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
    /// How many of [`approval_ids`](Self::approval_ids) came from an agent's
    /// **blocker** (`escalate_to_human`) rather than from a gated tool call
    /// (CodeRabbit review on #1905).
    ///
    /// The two ride the same list and settle the node the same way, but they
    /// promise different things. A gated call resumes on approval: the park
    /// carries the node's turn key, so a verdict re-runs the turn. A blocker
    /// is parked `Unlinked`, with `agent: None` and no continuation, precisely
    /// because answering a question is not the same act as authorising a call —
    /// so deciding it resumes nothing until #1863/#1864 land. Counted here so
    /// [`blocked_diagnosis`] can stop telling the operator and the model that
    /// the run continues on approval when, for these ids, it will not.
    pub blockers: usize,
}

impl ParkedCalls {
    /// Whether this turn gated anything at all — the block trigger.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty() && self.unparkable == 0
    }
}

impl HarnessAgentRunner {
    /// Builds a runner over `turn` for `company`, carrying the run's id (issue
    /// #395) and the operator's run request (issue #154) when one was supplied.
    /// The turn is the lane-aware router where lanes are declared, or the
    /// default lane over the pool otherwise, so a workflow agent node addressing
    /// a named-harness agent reaches that harness's engine.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        turn: Arc<dyn RunTurn>,
        deps: HarnessDeps,
        record: CompanyRecord,
        company: CompanyId,
        workflow_id: String,
        run_id: String,
        run_request: Option<String>,
        trigger_input: Value,
        started_by: crate::ports::types::StartedBy,
        notices: RunNotices,
        board: RunBoard,
        blocks: RunBlocks,
        capped: RunCappedNodes,
        approvals: RunApprovals,
        artifacts: RunArtifacts,
        board_claim: Arc<crate::harness::orchestrator::DelegationClaim>,
        publish_refusal_claim: Arc<crate::harness::publish::PublishRefusalClaim>,
    ) -> Self {
        let checkpoint_thread_id = run_id.clone();
        Self {
            runs: None,
            deep: None,
            attempts: RunAttempts::default(),
            turn,
            deps,
            record,
            company,
            workflow_id,
            run_id,
            checkpoint_thread_id,
            workflow_fingerprint: None,
            run_request,
            trigger_input,
            started_by,
            notices,
            board,
            blocks,
            capped,
            halted: RunHaltedNodes::default(),
            approvals,
            artifacts,
            board_claim,
            publish_refusal_claim,
        }
    }

    pub fn with_checkpoint_thread_id(mut self, thread_id: &str) -> Self {
        self.checkpoint_thread_id = thread_id.to_string();
        self
    }

    pub fn with_workflow_fingerprint(mut self, fingerprint: &str) -> Self {
        self.workflow_fingerprint = Some(fingerprint.to_string());
        self
    }

    /// Settles this node's attempt row, if it opened one.
    ///
    /// Called on every arm — success, block and failure alike — because a row
    /// left `Running` is indistinguishable from a host that died mid-turn, and
    /// the boot reaper would later fail it with a message about an orphan that
    /// was nothing of the kind.
    async fn settle_attempt(
        &self,
        sink: Option<&Arc<crate::harness::run_trace::RunTraceSink>>,
        status: crate::ports::RunStatus,
        error: Option<String>,
    ) {
        let (Some(runs), Some(sink)) = (&self.runs, sink) else {
            return;
        };
        let outcome = crate::ports::RunOutcome {
            status,
            error,
            usage: sink.usage(),
            step_count: sink.step_count(),
        };
        if let Err(err) = runs.finish_run(&self.company, sink.run_id(), outcome).await {
            tracing::warn!(
                company = %self.company,
                run = %sink.run_id(),
                %err,
                "workflow agent node: could not settle the attempt row"
            );
        }
    }

    /// Record every node's turn as a first-class attempt.
    ///
    /// Optional because a bundle built without a run store (tests, the dry-run
    /// path) must behave exactly as it did before: no row, no trace, and the
    /// node's own success or failure completely unchanged. A run row is
    /// observability, never the work.
    #[must_use]
    pub fn with_runs(
        mut self,
        runs: Option<Arc<dyn crate::ports::RunStore>>,
        deep: Option<Arc<dyn crate::ports::deep_trace::DeepTraceStore>>,
        attempts: RunAttempts,
    ) -> Self {
        self.runs = runs;
        self.deep = deep;
        self.attempts = attempts;
        self
    }

    #[must_use]
    pub fn with_halted(mut self, halted: RunHaltedNodes) -> Self {
        self.halted = halted;
        self
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
    /// Runs the bounded recovery ladder for this node, lending it the peer rung.
    ///
    /// The consultation runs inside this run's own delegation and
    /// publish-refusal scopes, and everything it stages there is discarded
    /// before the next node's drain could execute it. Its approval requests get
    /// a scope of their own instead: unclaimed pushes fall into the bucket the
    /// chat cycle drains, which would put a card the consulted peer asked for
    /// in front of the operator as if their own turn had raised it. The claim
    /// discards that bucket on drop.
    ///
    /// The nested scopes must stay `Box::pin`ed: `TaskLocalFuture` holds its
    /// inner future inline, and an agent turn inside three of them unboxed
    /// overflows the thread's stack.
    async fn recover_context(
        &self,
        question: &str,
        agent_ref: &str,
    ) -> crate::workflows::judge::RecoveryResult {
        if peer_consult_active() {
            return crate::workflows::judge::ask_around(&self.deps, &self.company, question, None)
                .await;
        }
        let consult = crate::workflows::judge::PeerConsult {
            turn: self.turn.as_ref(),
            record: &self.record,
            exclude_agent: agent_ref,
            workflow_id: &self.workflow_id,
        };
        let approval_claim = self
            .deps
            .approval_requests
            .claim(ApprovalScope::Run(format!("{}::consult", self.run_id)));
        let ladder = Box::pin(async {
            let recovered = crate::workflows::judge::ask_around(
                &self.deps,
                &self.company,
                question,
                Some(consult),
            )
            .await;
            self.discard_consultation_writes();
            recovered
        });
        let ladder = Box::pin(self.publish_refusal_claim.scoped(ladder));
        let ladder = Box::pin(self.board_claim.scoped(ladder));
        let ladder = Box::pin(approval_claim.scoped(ladder));
        PEER_CONSULT_ACTIVE.scope((), ladder).await
    }

    /// Throws away every board write and staged publish a peer consultation
    /// left behind: a consultation was asked a question, not given authority.
    ///
    /// Runs inside the run's own scopes, so the delegation drains empty this
    /// run's bucket and no other claimant's.
    fn discard_consultation_writes(&self) {
        let delegations = self.deps.delegations.drain(MAX_DELEGATIONS_PER_TURN).len();
        let refused_delegations = self
            .deps
            .delegations
            .drain_refusals(MAX_DELEGATIONS_PER_TURN)
            .len();
        let publishes = self.deps.pending_publishes.drain().len();
        let refused_publishes = self.deps.pending_publishes.drain_refusals().len();
        let approvals = self.deps.approval_requests.queued();
        let total = delegations + refused_delegations + publishes + refused_publishes + approvals;
        if total > 0 {
            tracing::info!(
                company = %self.company,
                workflow = %self.workflow_id,
                run_id = %self.run_id,
                delegations,
                refused_delegations,
                publishes,
                refused_publishes,
                approvals,
                "workflow agent node: discarded the board writes a peer consultation staged; a \
                 consultation carries no authority to act for this run"
            );
        }
    }

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
                crate::ports::WorkflowBoardAction::BoardUnwired => Some(format!(
                    "A step in this workflow could not write to the board because this runtime \
                     has no task board, so the card \"{}\" was not opened or changed.",
                    row.title
                        .as_deref()
                        .or(row.task_id.as_deref())
                        .unwrap_or("(unknown)")
                )),
                _ => None,
            };
            if let Some(notice) = notice {
                self.notices.push(notice);
            }
        }
        self.board.extend(rows);
    }

    /// Issue #1192: say on the run that a node's publish was refused.
    ///
    /// The `Unclaimed` refusal is honest and stays — a run has no card to attach
    /// a version to — but before this its **only** operator-visible record was
    /// the model's own reaction to it. The tool refused, the model wrote an
    /// apology, the apology became the node's `text` output, the `=items`
    /// binding delivered it downstream as though it were the deliverable, and
    /// the run scored clean. `caps`'s own doc already named this failure for the
    /// *gated* case ("that is exactly how a gated `publish_artifact` came to hand
    /// the model's apology downstream"), which #881 fixed with a structural
    /// notice; the `Unclaimed` case never got the same treatment.
    ///
    /// # A notice, deliberately not a block
    ///
    /// [`RunNotices`] rather than [`RunBlocks`]: a refused publish did **not**
    /// stop the node. The turn ran, the branch
    /// continued, and whatever else the node produced is real. `Blocked` halts
    /// the branch and is not auto-resumable — there is no approval to give here
    /// and nothing to release, so promoting this to a block would tell the
    /// operator to go answer a card that will never exist.
    ///
    /// # Structural wording only
    ///
    /// The sentence is composed from the source path and nothing else — never
    /// the tool's refusal text and never the model's prose — the same split
    /// [`drain_board_writes`](Self::drain_board_writes) keeps. Notices reach host
    /// logs.
    ///
    /// Deduped by path: a turn that called `publish_artifact` on the same file
    /// three times should name it once, for the reason
    /// [`push_tool`] gives.
    ///
    /// # Scope
    ///
    /// The queue handle is shared across every path in the company because the
    /// cached roster tool captures it at construction time. A run therefore
    /// claims a task-local refusal scope around its node turn and this drain;
    /// `push_refusal` reads that scope at call time. Concurrent runs write to
    /// and drain distinct buckets while chat and task turns retain the default
    /// bucket and their existing behavior.
    fn drain_publish_refusals(&self, captured: &[String]) {
        let refusals = self.deps.pending_publishes.drain_refusals();
        let mut seen: Vec<String> = Vec::new();
        for source in refusals {
            if seen.iter().any(|s| s == &source) {
                continue;
            }
            // The tool was built before runs had a card-less artifact target,
            // so it may still have returned its historical refusal — telling
            // the model mid-turn that the file was not published. The
            // post-turn workspace capture below can catch that same file
            // anyway, which would otherwise leave the node's own turn reply
            // ("I could not publish this") unreconciled against a run
            // inspector that shows the file delivered. Say both are true
            // rather than silently dropping one: the tool's refusal was real
            // at call time, and the capture is what actually landed it.
            if captured.iter().any(|path| path == &source) {
                tracing::info!(
                    company = %self.company,
                    workflow = %self.workflow_id,
                    run_id = %self.run_id,
                    path = %source,
                    "workflow agent node: a publish the tool refused was captured anyway by \
                     the post-turn workspace scan; reconciling the notice"
                );
                self.notices.push(format!(
                    "A step in this workflow was told \"{source}\" could not be published — a \
                     workflow run had no destination for that tool call — but the file was \
                     captured from that teammate's sandbox after the turn and is available as a \
                     run artifact."
                ));
                seen.push(source);
                continue;
            }
            tracing::warn!(
                company = %self.company,
                workflow = %self.workflow_id,
                run_id = %self.run_id,
                path = %source,
                "workflow agent node: a publish was refused because a run claims no publish \
                 destination; reporting it as a run notice"
            );
            self.notices.push(format!(
                "A step in this workflow wrote \"{source}\" and could not hand it over as a \
                 deliverable — a workflow run has nowhere to file one. The file is still in that \
                 teammate's sandbox."
            ));
            seen.push(source);
        }
    }

    /// Captures every file this node wrote and mirrors it into the run tree.
    ///
    /// The snapshot/diff is the same bounded mechanism task dispatch uses. Any
    /// explicitly staged publish is drained first and its already-captured body
    /// wins; the remaining changed paths are the unpublished files and are read
    /// directly from the node's sandbox. Nothing here can change the node's
    /// success/failure result: a mirror error is logged and the remaining files
    /// continue, because the turn has already happened.
    async fn capture_run_artifacts(
        &self,
        agent_ref: &str,
        node_id: &str,
        workspace: &std::path::Path,
        before: &crate::harness::publish::WorkspaceSnapshot,
    ) -> Vec<String> {
        use crate::harness::publish;

        let changed = before.changed_since(workspace);
        let staged = self.deps.pending_publishes.drain();
        let staged_sources: Vec<String> = staged.iter().map(|row| row.source.clone()).collect();
        let unpublished = publish::unpublished(&changed.files, &staged_sources);

        let mut candidates: std::collections::BTreeMap<String, publish::PendingPublish> = staged
            .into_iter()
            .map(|pending| (pending.source.clone(), pending))
            .collect();
        for source in unpublished {
            let file = workspace.join(&source);
            let inferred = publish::kind_for_extension(&file);
            let payload = match publish::capture_body(&file, &source, inferred) {
                Ok(payload) => payload,
                Err(err) => {
                    tracing::warn!(
                        company = %self.company,
                        workflow = %self.workflow_id,
                        run_id = %self.run_id,
                        node = node_id,
                        path = %source,
                        %err,
                        "workflow agent node: could not read a changed file for run capture"
                    );
                    continue;
                }
            };
            let kind = payload.forced_kind(inferred);
            let title = file
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| source.clone());
            candidates.insert(
                source.clone(),
                publish::PendingPublish {
                    agent: agent_ref.to_string(),
                    source,
                    title,
                    kind,
                    note: None,
                    payload,
                },
            );
        }

        if changed.partial {
            tracing::warn!(
                company = %self.company,
                workflow = %self.workflow_id,
                run_id = %self.run_id,
                node = node_id,
                "workflow agent node: the workspace scan was partial; run artifact capture may \
                 be incomplete"
            );
        }

        let Some(store) = self.deps.workspace.as_ref() else {
            if !candidates.is_empty() {
                tracing::warn!(
                    company = %self.company,
                    workflow = %self.workflow_id,
                    run_id = %self.run_id,
                    node = node_id,
                    files = candidates.len(),
                    "workflow agent node: files changed but no shared workspace store is wired"
                );
            }
            return Vec::new();
        };

        let mut captured = Vec::new();
        for pending in candidates.into_values() {
            let payload = match &pending.payload {
                publish::PublishPayload::Text(text) => {
                    crate::company::artifact_mirror::MirrorPayload::Text(text)
                }
                publish::PublishPayload::Bytes { bytes, mime } => {
                    crate::company::artifact_mirror::MirrorPayload::Bytes { bytes, mime }
                }
            };
            let target = crate::company::artifact_mirror::RunTarget {
                agent_id: agent_ref,
                run_id: &self.run_id,
                node_id,
                source: &pending.source,
                payload,
            };
            match crate::company::artifact_mirror::materialize_run(
                store.as_ref(),
                &self.company,
                target,
            )
            .await
            {
                Ok(mirrored) => {
                    captured.push(pending.source.clone());
                    self.artifacts.push(
                        node_id,
                        RunArtifact {
                            source: pending.source,
                            title: pending.title,
                            kind: pending.kind,
                            workspace_node_id: mirrored.node_id,
                            captured_at_millis: crate::ports::now_millis(),
                        },
                    );
                }
                Err(err) => tracing::error!(
                    company = %self.company,
                    workflow = %self.workflow_id,
                    run_id = %self.run_id,
                    node = node_id,
                    path = %pending.source,
                    %err,
                    "workflow agent node: could not materialize a changed file as a run artifact"
                ),
            }
        }
        captured
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
    /// Parks a **host-classified** blocker for a node that failed on something
    /// a person can answer (issue #1861), returning the approval id.
    ///
    /// `None` means "not a blocker" and the caller settles the node `Failed`
    /// exactly as before — the error was not one the classifier is willing to
    /// name, or it was transient, or this runtime has no approvals queue wired.
    /// Every one of those keeps today's behaviour, which is the conservative
    /// direction: a missed question surfaces through issue #1865's honest
    /// verdicts, while a false one holds a run open on a question nobody can
    /// answer until the TTL expires it.
    ///
    /// # Why the turn key is `None`
    ///
    /// The gated-call path above passes `Some(node_turn)` so that deciding the
    /// last of a node's calls re-dispatches the run (#899). Deliberately not
    /// here, and not only because carrying an answer back is #1863's: approving
    /// a blocker is not the same act as *fixing* what it named. Saying "yes, I
    /// have seen that the model id is wrong" does not make the model id right,
    /// so a re-dispatch on approve would re-run the node into the identical
    /// failure and park the identical question. The gated-call case does not
    /// have that problem — approving there mints the grant that makes the
    /// retry succeed.
    /// Stashes what re-entering `resolved_node_id` will need once its blocker
    /// is answered (issue #2005): the workflow, this run's trigger input, its
    /// attribution, and the checkpoint lineage #1864's node-level restart
    /// resumes from.
    ///
    /// A blocker park cannot borrow the gated-call path's stash. That one is
    /// armed in [`park_gated_calls`](Self::park_gated_calls) only once there is
    /// a gated call to park — a turn that parked nothing else returns before
    /// reaching it — and the runner's settle-time pass deliberately refuses to
    /// arm a turn that is not already armed, so a blocker-only node reached the
    /// resume with no run to continue. Written here, at park time and before
    /// any card is clickable, on exactly the ordering the gated-call arm keeps.
    ///
    /// Keyed per (run, node) like its sibling, so a node that parked a gated
    /// call *and* then blocked shares one stash and one dispatch marker: either
    /// answer re-enters the node once, and the second finds the continuation
    /// already dispatched rather than launching a duplicate. Arming is
    /// first-write-wins, so the two cannot disagree.
    ///
    /// No [`ContinuationQueue`](crate::runtime::continuation::ContinuationQueue)
    /// arm accompanies it. A blocker parks with no turn key by design — see
    /// [`park_node_blocker`](Self::park_node_blocker) — and its resume is driven
    /// by the answer itself, not by a decision batch emptying.
    ///
    /// Best-effort on the durable half, matching every other park-time write
    /// here: a failed journal append leaves the in-memory stash serving the
    /// no-restart case, and failing the node over it would be the wrong trade.
    async fn stash_node_blocker_resume(
        &self,
        parking: &crate::workflows::delivery::DeliveryParking,
        resolved_node_id: &str,
    ) {
        let turn =
            crate::runtime::workflow_resume::workflow_node_turn_key(&self.run_id, resolved_node_id);
        parking.blocked_nodes.arm_checkpointed(
            &turn,
            &self.workflow_id,
            &self.trigger_input,
            &self.started_by,
            Some(&self.checkpoint_thread_id),
            self.workflow_fingerprint.as_deref(),
        );
        if let Err(error) = parking
            .journal
            .record_blocked_node_stashed_checkpointed(
                &turn,
                &self.workflow_id,
                &self.trigger_input,
                &self.started_by,
                Some(&self.checkpoint_thread_id),
                self.workflow_fingerprint.as_deref(),
            )
            .await
        {
            tracing::warn!(
                company = %self.company,
                run_id = %self.run_id,
                node = resolved_node_id,
                %error,
                "workflow agent node: a parked blocker's continuation facts could not be \
                 durably stashed; the in-memory stash still covers an answer without a restart"
            );
        }
    }

    async fn park_node_blocker(&self, resolved_node_id: &str, message: &str) -> Option<String> {
        let class = crate::harness::built_in::blockers::classify_blocker_message(message)?;
        self.park_node_blocker_as(
            resolved_node_id,
            message,
            class.kind,
            class.source,
            class.needed,
        )
        .await
    }

    async fn park_node_blocker_as(
        &self,
        resolved_node_id: &str,
        message: &str,
        kind: crate::ports::blockers::BlockerKind,
        source: crate::ports::blockers::BlockerSource,
        needed: &str,
    ) -> Option<String> {
        if !kind.parks() {
            return None;
        }
        let parking = self
            .deps
            .delivery
            .as_ref()
            .and_then(|delivery| delivery.parking.as_ref())?;
        let payload = crate::ports::blockers::BlockerPayload {
            kind,
            source,
            // The one case an approval's own task link cannot express — see
            // `BlockerPayload::step`. #1864's node-level restart needs to know
            // which node inside which run stopped, and a workflow run has no
            // card behind it to name instead. Use the resolved node id (with
            // agent_ref fallback) to match BlockerStep::Node with
            // WorkflowBlockedNode.
            step: Some(crate::ports::blockers::BlockerStep::Node {
                run_id: self.run_id.clone(),
                node_id: resolved_node_id.to_string(),
            }),
            reason: message.to_string(),
            needed: needed.to_string(),
            // Nodes across runs stalled on one broken integration read as one
            // question — populated only when the reason names a connection.
            group_key: crate::harness::built_in::blockers::connection_group_key(message),
        };
        let effect = crate::ports::types::Effect {
            kind: payload.effect_kind(),
            group: crate::ports::types::EffectGroup::Other,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::to_value(&payload).unwrap_or(serde_json::Value::Null),
            agent: None,
            run_id: Some(self.run_id.clone()),
        };

        let turn =
            crate::runtime::workflow_resume::workflow_node_turn_key(&self.run_id, resolved_node_id);
        let already_stashed = parking.blocked_nodes.is_armed(&turn);
        self.stash_node_blocker_resume(parking, resolved_node_id)
            .await;

        match parking
            .park_and_journal(
                &self.company,
                effect,
                // A workflow run has no board card behind it and no
                // conversation to raise the question in — the same delivery
                // precedent the gated-call park follows (#333, #379).
                crate::runtime::journal::TaskLink::Unlinked,
                None,
                None,
            )
            .await
        {
            Ok(approval_id) => {
                tracing::info!(
                    company = %self.company,
                    run_id = %self.run_id,
                    node = resolved_node_id,
                    approval_id = %approval_id,
                    kind = kind.as_str(),
                    "workflow agent node: parked a blocker for the operator instead of failing"
                );
                Some(approval_id.to_string())
            }
            Err(err) => {
                if !already_stashed {
                    parking.blocked_nodes.release(&turn);
                    if let Err(release_err) =
                        parking.journal.record_blocked_node_released(&turn).await
                    {
                        tracing::warn!(
                            company = %self.company,
                            run_id = %self.run_id,
                            node = resolved_node_id,
                            error = %release_err,
                            "workflow agent node: a blocker's stash could not be durably \
                             retired after its park failed; a stale entry may linger until \
                             a manual sweep"
                        );
                    }
                }
                // Loud, and then the node settles `Failed` as it did before:
                // holding a run open on a question that reached nobody would be
                // strictly worse than the failure it replaced.
                tracing::error!(
                    company = %self.company,
                    run_id = %self.run_id,
                    error = %err,
                    "workflow agent node: could not park a blocker; the node fails instead"
                );
                None
            }
        }
    }

    /// `node_id` is the graph's own id and stays `Option` — a hand-built
    /// request or a graph compiled before #881 has none, and the approval rows
    /// report that honestly rather than inventing one.
    ///
    /// `resolved_node_id` is the identity the **run** knows the node by:
    /// `node_id` when there is one, the agent ref otherwise, which is exactly
    /// what [`WorkflowBlockedNode::node_id`] carries for the same node. A
    /// parked blocker's [`BlockerStep::Node`] must use that one and not the
    /// bare option (CodeRabbit review on #1905) — it used to fall back to `"-"`,
    /// so on the no-`node_id` path the blocker named a node that appears
    /// nowhere in the run, leaving #1864's node-level restart with no target to
    /// resolve.
    ///
    /// [`WorkflowBlockedNode::node_id`]: crate::ports::WorkflowBlockedNode::node_id
    /// [`BlockerStep::Node`]: crate::ports::blockers::BlockerStep::Node
    async fn park_gated_calls(
        &self,
        node_id: Option<&str>,
        resolved_node_id: &str,
        node_turn: &str,
    ) -> ParkedCalls {
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
        let mut requests = drained.requests;

        // Issue #1861: extract and park blocker requests directly, then filter them
        // out of the gated-call path. They are parked via their already-classified
        // effect payload (not re-classified) without a node-turn continuation (they are
        // questions, not gated tool calls), so they must not pass through this gated-call
        // path which journals with Some(node_turn).
        let (blocker_requests, remaining): (Vec<_>, Vec<_>) = requests
            .into_iter()
            .partition(|r| r.effect.kind.starts_with("blocker."));
        requests = remaining;

        let blocker_stash_pre_existing = !blocker_requests.is_empty()
            && self
                .deps
                .delivery
                .as_ref()
                .and_then(|d| d.parking.as_ref())
                .is_some_and(|parking| parking.blocked_nodes.is_armed(node_turn));
        let mut blocker_stash_armed_this_call = false;
        if !blocker_requests.is_empty()
            && let Some(parking) = self.deps.delivery.as_ref().and_then(|d| d.parking.as_ref())
        {
            self.stash_node_blocker_resume(parking, resolved_node_id)
                .await;
            blocker_stash_armed_this_call = true;
        }
        for mut blocker_request in blocker_requests {
            // Extract the blocker payload from the effect, add the node step, and park it.
            let mut payload: crate::ports::blockers::BlockerPayload =
                match serde_json::from_value(blocker_request.effect.payload.clone()) {
                    Ok(p) => p,
                    Err(_) => {
                        // Malformed payload—treat as unparkable
                        summary.unparkable += 1;
                        rows.push(row(
                            Some(blocker_request.tool.clone()),
                            crate::ports::WorkflowApprovalOutcome::ParkFailed,
                            None,
                        ));
                        continue;
                    }
                };
            // The identity the run knows this node by, so the blocker points at
            // a node that is actually in the run — see `resolved_node_id`.
            payload.step = Some(crate::ports::blockers::BlockerStep::Node {
                run_id: self.run_id.clone(),
                node_id: resolved_node_id.to_string(),
            });
            // Update the effect with the augmented payload.
            blocker_request.effect.payload =
                serde_json::to_value(&payload).unwrap_or(serde_json::Value::Null);
            // Park the blocker directly using the delivery system.
            let parking = match self.deps.delivery.as_ref().and_then(|d| d.parking.as_ref()) {
                Some(p) => p,
                None => {
                    summary.unparkable += 1;
                    rows.push(row(
                        Some(blocker_request.tool.clone()),
                        crate::ports::WorkflowApprovalOutcome::ParkFailed,
                        None,
                    ));
                    continue;
                }
            };
            match parking
                .park_and_journal(
                    &self.company,
                    blocker_request.effect,
                    crate::runtime::journal::TaskLink::Unlinked,
                    None,
                    None,
                )
                .await
            {
                Ok(approval_id) => {
                    push_tool(&mut summary.tools, &blocker_request.tool);
                    summary.approval_ids.push(approval_id.to_string());
                    summary.blockers += 1;
                    rows.push(row(
                        Some(blocker_request.tool.clone()),
                        crate::ports::WorkflowApprovalOutcome::Parked,
                        Some(approval_id.to_string()),
                    ));
                }
                Err(_) => {
                    summary.unparkable += 1;
                    rows.push(row(
                        Some(blocker_request.tool.clone()),
                        crate::ports::WorkflowApprovalOutcome::ParkFailed,
                        None,
                    ));
                }
            }
        }

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
            if blocker_stash_armed_this_call
                && !blocker_stash_pre_existing
                && summary.approval_ids.is_empty()
                && let Some(parking) = self.deps.delivery.as_ref().and_then(|d| d.parking.as_ref())
            {
                parking.blocked_nodes.release(node_turn);
                if let Err(error) = parking
                    .journal
                    .record_blocked_node_released(node_turn)
                    .await
                {
                    tracing::warn!(
                        company = %self.company,
                        run_id = %self.run_id,
                        node_turn,
                        %error,
                        "workflow agent node: every blocker for this node failed to park, but \
                         retiring the stash armed for it also failed durably; a stale entry may \
                         linger until a manual sweep"
                    );
                }
            }
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

        // Issue #1825 (P1 follow-up): arm this node's in-memory continuation
        // stash BEFORE the loop below parks a single call, not after the
        // runner's block-settle pass — which is `stash_blocked_agent_nodes`,
        // and runs only once the agent has returned, the engine has settled,
        // and (on the halt path) the run's output has been persisted. The
        // first `park_and_journal` call below is what makes this turn's
        // approval card durable and clickable; an operator who acts on it in
        // the window between that and block-settle used to have their
        // decision consumed by `continue_turn` against an empty stash — the
        // turn retired with nothing to release, and the batch-settle arm then
        // stashed facts a spent decision would never come back for. Arming
        // here, before any card exists to act on, closes the window instead
        // of narrowing it. `arm` is first-write-wins and cheap (one HashMap
        // insert under a `Mutex`), so a redundant call from the settle pass
        // below is a harmless no-op, not a second source of truth.
        parking.blocked_nodes.arm_checkpointed(
            node_turn,
            &self.workflow_id,
            &self.trigger_input,
            &self.started_by,
            Some(&self.checkpoint_thread_id),
            self.workflow_fingerprint.as_deref(),
        );

        // Issue #1825 (P1, second follow-up — found by chatgpt-codex-connector):
        // the in-memory arm above only helps the no-restart case. The durable
        // mirror this node's stash needs to survive a restart —
        // `record_blocked_node_stashed`'s `BlockedNodeStashed` — used to be
        // written only from `stash_blocked_agent_nodes`, after settle, same as
        // the in-memory arm was before the fix above. `park_and_journal` below
        // is what makes the first card in this node's batch host-durable and
        // clickable; a process that dies after that write lands but before the
        // settle pass's durable stash runs leaves a restart with a recoverable
        // card and no matching stash for it to release — approving it then
        // consumes the card against nothing, identical in shape to the
        // in-memory race the arm above closes, one durability tier up. Writing
        // it here, before any card is durable, closes that window the same
        // way. Best-effort, matching every other park-time write in this
        // function: a failed durable stash still leaves the in-memory arm
        // above serving the common (no-restart) case, and failing the node
        // over an approvals-queue write would be the wrong trade.
        if let Err(error) = parking
            .journal
            .record_blocked_node_stashed_checkpointed(
                node_turn,
                &self.workflow_id,
                &self.trigger_input,
                &self.started_by,
                Some(&self.checkpoint_thread_id),
                self.workflow_fingerprint.as_deref(),
            )
            .await
        {
            tracing::warn!(
                company = %self.company,
                run_id = %self.run_id,
                node_turn,
                %error,
                "workflow agent node: a blocked node's continuation facts could not be \
                 durably stashed at park time; the in-memory stash still covers a resolve \
                 without a restart, and the block-settle pass's own stash write remains as \
                 a fallback"
            );
        }

        // Issue #1825 (P1, fourth follow-up — found by chatgpt-codex-connector):
        // for a node parking MORE than one gated call, hold this turn's
        // `ContinuationQueue` counter open across the whole loop below, so
        // approving the first card the loop parks cannot complete the batch
        // before the remaining calls have even been attempted. Each
        // successful `park_and_journal` call arms this same counter (issue
        // #469/#978's original mechanism, unchanged): with no hold, that
        // per-call increment means outstanding briefly equals exactly the
        // number of calls parked *so far*, not the batch's true size — a
        // decision on the very first card can zero it out while later
        // iterations are still awaiting store I/O. `blocked_nodes.arm` above
        // already makes the workflow id and trigger input available the
        // instant that first card exists (closing the single-call race this
        // node's stash used to hit), so a premature zero here does not find
        // an empty stash and safely fall back to "re-run the workflow" the
        // way it once did — it finds a real one and re-dispatches, duplicating
        // whatever the run does next. The hold pins outstanding at least 1
        // above the count of *decided* cards until every request has been
        // attempted, released only after the loop below (see there).
        //
        // Skipped for a single-call node (the overwhelmingly common case):
        // there is no "rest of the batch" to protect against, and holding
        // would only insert an extra decrement between that lone card's
        // approval and its release — reopening, for the common case, the
        // exact empty-window this function's first follow-up (above) exists
        // to close. See the release site for what happens on the rare batch
        // that is fully decided before this loop finishes attempting it.
        let holds_continuation = requests.len() > 1;
        if holds_continuation {
            parking.continuations.arm(node_turn);
        }

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
                    // Issue #899 (Stage 1): the per-(run, node) continuation turn
                    // key. Issue #978 deliberately passed `None` here because a
                    // tool-call-shaped card (`ApprovalPolicy::effect_for` stamps
                    // an `agent`) only ever minted a grant on approve and nothing
                    // re-dispatched the run — the run settled Blocked and stayed
                    // there. Keying the node's calls as one batch is what lets the
                    // resolve path re-dispatch the run once, when the last of them
                    // is decided (the runner's stash carries the workflow id and
                    // trigger input the spawn needs). The grant is still minted, so
                    // the identical call passes on the re-run; a diverging re-run
                    // re-asks, which Stage 2 closes.
                    Some(node_turn.to_string()),
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

        // Issue #1825 (P1, fourth follow-up): release the hold armed above,
        // now that every request in this batch has actually been attempted —
        // whether it parked or failed. From here on, `outstanding` for this
        // turn again means exactly what `ContinuationQueue::decide` assumes it
        // means: the count of *real, parked* cards left undecided.
        //
        // `Some(batch)` back means this release was itself the batch's last
        // decision — every card this loop parked was already approved or
        // denied by the time the loop finished attempting the rest, which
        // needs an operator (or an API caller) faster than this function's
        // own sequential parks. `park_gated_calls` runs on `HarnessAgentRunner`,
        // deep inside the agent's own turn, with no path back to
        // `CompanyRuntime::resume_blocked_agent_node` — the only place that
        // spawns a continuation — short of re-entering this run's own
        // execution while it is still mid-turn, which is a worse hazard than
        // the one this hold exists to close (double-dispatch again, just
        // moved). So this rare batch is left exactly as `blocked_nodes.arm`
        // above already left it: approved and durably stashed. The decisions
        // themselves are not lost — each was already resolved and journaled
        // independently of this counter — only the automatic re-dispatch is
        // deferred, to the next boot's `reconcile_stranded_blocked_nodes`
        // (see `resume_blocked_agent_node`'s doc for that path). An empty
        // batch (every request below failed to park, so nothing was ever
        // decided) is silently fine — the cleanup right after this handles
        // that case.
        if holds_continuation
            && let Some(batch) = parking.continuations.decide(node_turn, None)
            && !batch.is_empty()
        {
            tracing::warn!(
                company = %self.company,
                run_id = %self.run_id,
                node_turn,
                decisions = batch.len(),
                "workflow agent node: every gated call this node parked was already decided \
                 before the rest of the batch finished parking; the approval is recorded but \
                 the run will not auto-resume until the next boot's stranded-block \
                 reconciliation"
            );
        }

        // Issue #1825 (P2, third follow-up — found by chatgpt-codex-connector):
        // the arm and the durable stash above run unconditionally, before this
        // loop even attempts a single park — that ordering is the whole point
        // (it is what closes the race the P1 follow-up fixed). But if every
        // request in this node's batch then fails to park or journal,
        // `summary.approval_ids` comes back empty: nothing was ever parked for
        // an operator to decide, so nothing will ever call `continue_turn` for
        // this turn, and the stash this function just armed and durably wrote
        // sits forever — one workflow id and complete trigger payload retained
        // in memory for the life of the process, and durably on every replay,
        // per store outage this hits. Retire what was just armed the same way
        // the background-retry cleanup above does: release the in-memory
        // stash, then append one `BlockedNodeReleased` so a durable stash (if
        // the write landed) does not outlive the batch it was for either.
        if summary.approval_ids.is_empty() {
            parking.blocked_nodes.release(node_turn);
            if let Err(error) = parking
                .journal
                .record_blocked_node_released(node_turn)
                .await
            {
                tracing::warn!(
                    company = %self.company,
                    run_id = %self.run_id,
                    node_turn,
                    %error,
                    "workflow agent node: every gated call for this node failed to park, but \
                     retiring the stash armed for it also failed durably; a stale entry may \
                     linger until a manual sweep"
                );
            }
        }

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

impl HarnessAgentRunner {
    /// The whole turn, keeping what the trait's legacy return throws away.
    ///
    /// [`AgentRunner::run_agent`] can only hand back a `Value`, so before this
    /// existed the turn's folded steps died at the end of this function — the
    /// comment at the bottom used to say so. They are the only record of what a
    /// workflow's agent actually did, and a workflow node has no chat bubble to
    /// render them in, so dropping them meant a run could be inspected only as
    /// pass/fail. Both trait methods below call this; the typed one keeps the
    /// transcript.
    async fn run_turn(
        &self,
        agent_ref: &str,
        request: Value,
    ) -> TfResult<(Value, crate::harness::built_in::TurnOutcome)> {
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
        let mut message = compose_turn_message(&instruction, self.run_request.as_deref());
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
        // Issue #899 (Stage 1): the continuation turn key every gated call this
        // node parks will share, so approving them re-dispatches the run once —
        // the whole hole this closes. Keyed on the block's RESOLVED node id (the
        // graph node when there is one, else the agent ref), which is exactly the
        // id the runner's block-settle stashes under, so the two agree by
        // construction. `park_gated_calls` arms `ContinuationQueue` with it via
        // `park_and_journal`; the runner arms the sibling stash that carries the
        // workflow id and trigger input the release needs.
        let lineage_node = node_id.clone().unwrap_or_else(|| agent_ref.to_string());
        // Codex review on #1990: a fresh attempt at this node — whether this is
        // the node's first attempt ever, or `retry.max_attempts > 1` re-running
        // it after an earlier attempt's judge answered `halt_benign` — must not
        // inherit that earlier attempt's benign-halt marker. Left in place, a
        // later attempt that genuinely succeeds would still have its row
        // relabeled Declined by `reclassify_halted_nodes` reading the stale
        // entry. Re-added below only if THIS attempt halts too.
        self.halted.retract(&lineage_node);
        let node_turn =
            crate::runtime::workflow_resume::workflow_node_turn_key(&self.run_id, &lineage_node);

        // ── Issue #2005: the operator's answer to this node's blocker ────────
        //
        // The engine-side half of the blocker family. #1863 banked the verdict
        // and delivered it into the DM; a workflow node's re-entry rides the
        // trigger input instead, because a paused run is settled and the only
        // way back in is a fresh run carrying what the last one learned. The
        // answer is read here, before an attempt row is minted or a token is
        // spent, so a `skip` costs nothing.
        //
        // A malformed answer fails the node rather than degrading to "nobody
        // answered". The degrade is the exact silent drop this family exists to
        // close: the node would spend a turn on the identical failure, park the
        // identical question, and the operator's decision would be gone.
        let blocker_answer = match crate::runtime::workflow_resume::blocker_answer_for(
            &self.trigger_input,
            &lineage_node,
        ) {
            Ok(answer) => answer,
            Err(err) => {
                let message = format!("workflow node `{lineage_node}`: {err}");
                tracing::error!(
                    company = %self.company,
                    workflow = %self.workflow_id,
                    run_id = %self.run_id,
                    node = %lineage_node,
                    "workflow agent node: {message}"
                );
                return Err(EngineError::Capability(message));
            }
        };
        if let Some(answer) = &blocker_answer {
            use crate::ports::blockers::BlockerVerdict;
            match answer.verdict {
                // Waived: the node does not run at all, and the branch below it
                // proceeds on a host-authored output. Running it would re-park
                // the very question the operator just declined to answer.
                BlockerVerdict::Skip => {
                    let reply = "This step was skipped: an operator waived the blocker it \
                                 stopped on."
                        .to_string();
                    tracing::info!(
                        company = %self.company,
                        workflow = %self.workflow_id,
                        run_id = %self.run_id,
                        node = %lineage_node,
                        "workflow agent node: skipped on the operator's answer to its blocker"
                    );
                    return Ok((
                        json!({ "text": reply, "agent_ref": agent_ref }),
                        crate::harness::built_in::TurnOutcome {
                            reply,
                            steps: Vec::new(),
                            hit_iteration_cap: false,
                            abnormal_stop: None,
                            halted_for_spend: None,
                            budget_paused: None,
                            ceiling_paused: None,
                        },
                    ));
                }
                // Corrected: the node runs again carrying the operator's words,
                // the same shape `resume_task_card` gives an amended card — the
                // correction has to reach the turn, or the re-run repeats the
                // failure it was answering.
                BlockerVerdict::Amend if !answer.answer.trim().is_empty() => {
                    message = format!(
                        "{message}\n\n## Answer from the operator\n{}",
                        answer.answer.trim()
                    );
                }
                // A bare amend and a retry both re-run the step as it was.
                BlockerVerdict::Amend | BlockerVerdict::Retry => {}
                // `blocker_answer_for` refuses this arm before it can be built,
                // and a cancel starts no run in the first place.
                BlockerVerdict::Cancel => {
                    let message = format!(
                        "workflow node `{lineage_node}` was reached carrying a cancelled \
                         blocker answer"
                    );
                    return Err(EngineError::Capability(message));
                }
            }
        }

        // The node runs in its roster agent's sandbox, not the workflow tool
        // workspace. Snapshot it immediately before inference so the post-turn
        // drain can distinguish this node's writes from files already there.
        let workspace = crate::harness::build::agent_workspace(
            &self.deps.workspace_root,
            &self.company,
            agent_ref,
        );
        let workspace_before = crate::harness::publish::WorkspaceSnapshot::take(&workspace);

        // The attempt row. This is the thing that did not exist: a workflow
        // node's turn had no card and no conversation, so `RunStore` — keyed on
        // exactly those two — could not name it, and nothing downstream could
        // ask what the node's agent actually did.
        //
        // Minted before the turn so the trace has somewhere to land from the
        // first event, and settled on BOTH arms below. A failure to mint is
        // logged and the node runs anyway: observability must never be able to
        // fail the work it is observing.
        let run_sink = match &self.runs {
            Some(runs) => {
                let spec = crate::ports::NewRun::for_workflow_node(
                    crate::ports::generate_id(),
                    &self.run_id,
                    &lineage_node,
                    agent_ref,
                );
                match runs.create_run(&self.company, spec).await {
                    Ok(row) => {
                        if let Err(err) = runs.begin_run_untriggered(&self.company, &row.id).await {
                            tracing::warn!(
                                company = %self.company,
                                run = %row.id,
                                %err,
                                "workflow agent node: could not mark the attempt running"
                            );
                        }
                        self.attempts.record(&lineage_node, &row.id);
                        Some(Arc::new(
                            crate::harness::run_trace::RunTraceSink::new(
                                self.company.clone(),
                                row.id,
                                Arc::clone(runs),
                            )
                            .with_deep(self.deep.clone()),
                        ))
                    }
                    Err(err) => {
                        tracing::warn!(
                            company = %self.company,
                            workflow = %self.workflow_id,
                            run_id = %self.run_id,
                            node = %lineage_node,
                            %err,
                            "workflow agent node: could not open an attempt row; the node still runs"
                        );
                        None
                    }
                }
            }
            None => None,
        };
        tracing::debug!(
            company = %self.company,
            agent = agent_ref,
            "workflow agent node: routing through harness turn"
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
        // Issue #2150: this node's trust window, named by the agent it
        // dispatched to and the workflow it belongs to.
        let origin_claim = crate::harness::built_in::run_origin::claim(
            crate::harness::built_in::run_origin::RunOrigin::Dispatched {
                agent: agent_ref.to_string(),
                source: crate::harness::built_in::run_origin::DispatchSource::Workflow {
                    workflow_id: self.workflow_id.clone(),
                },
                scope: None,
            },
        );
        // Issue #661 (M5): the turn AND its post-turn drains run inside the run's
        // board scope, so a `spawn_task` the model calls files into this run's
        // bucket and the drain below reads that same bucket back. The claim itself
        // was taken once for the whole run (see `build_capabilities`); this only
        // installs its ambient scope for the span of this node's turn.
        //
        // **Every layer here is `Box::pin`ed, and that is load-bearing.** This
        // nests one task-local scope inside another (`ApprovalScope` inside
        // `DelegationScope`, now also the dispatch-origin scope), and
        // `TaskLocalFuture` stores its inner future *inline* — so without
        // boxing, an openhuman agent turn (already a very large future) is held
        // by value inside these nested wrappers and the composed state blows
        // the thread's stack. Verified: it overflows on the first spawning run
        // without these.
        let turn = Box::pin(async {
            let outcome = claim
                .scoped(Box::pin(origin_claim.scoped(Box::pin(
                    self.turn.run_background_workflow(
                        &self.company,
                        agent_ref,
                        &message,
                        run_sink.clone(),
                        // The workflow run + node this turn belongs to (issue
                        // #1702): its live tool-call frames stream tagged with
                        // these so the console's run-trace sheet appends them
                        // under the right run while the node is still
                        // executing. `lineage_node` is the resolved node id
                        // (graph node, else the agent ref) — the same id the
                        // durable trace attributes the node's steps to.
                        &self.run_id,
                        &lineage_node,
                    ),
                ))))
                .await;
            // Drained on BOTH arms, deliberately. A turn that errored may still have
            // had a tool call gated before it failed, and that request is just as
            // real — dropping the claim without parking would discard it, which is
            // the exact disappearance this issue is about.
            //
            // Inside the scope, so the drain reads this run's bucket.
            //
            // Issue #880: the receipts it files therefore survive a failed turn
            // too. Only the #881 *block* below is gated on the turn having
            // succeeded — a turn that genuinely errored is a failure, and
            // reclassifying it as "blocked" would hide one behind an approval
            // nobody has answered.
            let parked = claim
                .scoped(Box::pin(self.park_gated_calls(
                    node_id.as_deref(),
                    &lineage_node,
                    &node_turn,
                )))
                .await;
            // Issue #661 (M5): likewise on both arms, and for the same reason. A
            // turn that failed after calling `spawn_task` had already been told the
            // card would be opened; refusing to drain would make that receipt false
            // and destroy the write when the scope ends.
            Box::pin(self.drain_board_writes()).await;
            // Run artifacts are drained on BOTH arms. A provider/tool failure or
            // a later approval block does not undo files the turn already wrote,
            // so capture happens before either return below can discard them.
            let captured = self
                .capture_run_artifacts(agent_ref, &lineage_node, &workspace, &workspace_before)
                .await;
            // Issue #1192: likewise on both arms. A refusal is still surfaced
            // when capture failed, but a file successfully mirrored above is no
            // longer described as stranded.
            self.drain_publish_refusals(&captured);
            (outcome, parked)
        });
        let turn = Box::pin(self.board_claim.scoped(turn));
        let (outcome, parked) = self.publish_refusal_claim.scoped(turn).await;
        // Issue #849: a provider context-window refusal reaches the operator as
        // the run's error text, and the vendor's own wording ("Please start a
        // new chat") is unfollowable in a workflow — there is no chat and no
        // button. Rewrite that one class into what is actually too big and what
        // to do about it, keeping the provider's words at the end. Every other
        // failure passes through exactly as before.
        let mut outcome = match outcome {
            Ok(outcome) => outcome,
            Err(e) => {
                let raw = e.to_string();
                let reported = upstream::context_overflow_advice(&raw).unwrap_or(raw);
                let message = format!("harness agent '{agent_ref}': {reported}");
                // Issue #1861: the same question the task path asks. A node
                // that died on a rejected model id or a dead integration is
                // answerable by a person, and the #881 machinery below already
                // knows how to hold a node open for one — it just had no way in
                // except an agent's own blocked tool call. This is that way in.
                //
                // Only the park differs from #881's; everything after it is
                // shared, so a host-classified blocker and an agent-declared
                // one reach the operator as one shape.
                if let Some(approval_id) = self.park_node_blocker(&lineage_node, &message).await {
                    self.blocks.push(crate::ports::WorkflowBlockedNode {
                        node_id: lineage_node.clone(),
                        // No tools: nothing the agent called was gated. What
                        // stopped this node is the node itself.
                        tools: Vec::new(),
                        approval_ids: vec![approval_id],
                        unparkable: 0,
                        stranded: 0,
                        blockers: 1,
                    });
                    // `Blocked`, where #881's sibling says `WaitingApproval`:
                    // both hold the node open for a person, but one is a
                    // decision about a call that is ready to run and this is a
                    // question with nothing behind it yet.
                    self.settle_attempt(
                        run_sink.as_ref(),
                        crate::ports::RunStatus::Blocked,
                        Some(message.clone()),
                    )
                    .await;
                    return Err(EngineError::Capability(message));
                }
                self.settle_attempt(
                    run_sink.as_ref(),
                    crate::ports::RunStatus::Failed,
                    Some(message.clone()),
                )
                .await;
                return Err(EngineError::Capability(message));
            }
        };

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
                stranded: 0,
                blockers: parked.blockers,
            });
            let diagnosis = blocked_diagnosis(node_id.as_deref(), agent_ref, &parked);
            // `WaitingApproval`, not `Failed`: a person still has to decide, and
            // the row must not read as an error nor be reaped as an orphan.
            self.settle_attempt(
                run_sink.as_ref(),
                crate::ports::RunStatus::WaitingApproval,
                Some(diagnosis.clone()),
            )
            .await;
            return Err(EngineError::Capability(diagnosis));
        }

        // PR #1880 review: an ACP turn that stopped abnormally — a `refusal`,
        // a `cancelled` turn, or an unrecognized `stopReason` — is not a cap
        // pause (there is no resumable checkpoint to report, unlike
        // `hit_iteration_cap` below) and is not a clean finish either.
        // `hit_iteration_cap` alone could not say so: it stays `false` on
        // every one of these, and until `abnormal_stop` existed this method
        // read only that flag, so the node settled `Succeeded` here and
        // `run` below reported `StopReason::Finished` — indistinguishable
        // from the agent having actually answered, letting a declined or
        // interrupted turn's reply advance the workflow graph as if it were
        // the deliverable. `Err`, the same channel the #881 block above
        // uses, rather than folding into the `LimitStop` shape below: a
        // `LimitStop` still lets the engine bind the node's output
        // downstream (with a warning) because a capped turn's checkpoint is
        // real, partial work — there is no equivalent partial-but-real
        // claim to make about a refusal or a cancellation, so `on_error`'s
        // default "stop" is the honest outcome, not a tagged pass-through.
        if let Some(reason) = &outcome.abnormal_stop {
            let message = format!("harness agent '{agent_ref}': {reason}");
            self.settle_attempt(
                run_sink.as_ref(),
                crate::ports::RunStatus::Failed,
                Some(message.clone()),
            )
            .await;
            return Err(EngineError::Capability(message));
        }

        // ── Issue #1866: the deterministic postcondition gate ────────────────
        //
        // A node whose declared `postcondition` the output fails does not feed
        // downstream, full stop — checked BEFORE the hit_iteration_cap decision
        // below and before the attempt row settles Succeeded. A capped turn's
        // partial reply is exactly the truncation class this gate is meant to
        // catch, so it is deliberately not special-cased here: if a
        // postcondition is declared, it is checked regardless of whether the
        // cap already would have failed the attempt on its own. This runs
        // AFTER the `abnormal_stop` check above: a refusal/cancellation has
        // already returned `Err` with no reply worth evaluating by the time
        // this is reached.
        //
        // `on_error` defaults to `"stop"` and `retry.max_attempts` to `1` (the
        // same contract issue #881's block above leans on), so returning `Err`
        // halts the branch at this node with no retry re-running the turn, and
        // nothing downstream ever sees the insufficient output.
        // Codex review on #1937: `require = "field_present"`/`"non_empty_list"`
        // document a dotted `field` like `json.items`, which only ever
        // resolves against the engine's `{ json, text, raw }` capability-node
        // envelope — so this best-effort parses the agent's reply as JSON (an
        // agent prompted to answer with structured output does), giving those
        // two predicates real structured content to check instead of an
        // object that can never be a `Value::Array`. A reply that is not
        // valid JSON (the common case — agent nodes are prose by default)
        // parses to `Null`, so `field_present`/`non_empty_list` fail with
        // their ordinary "missing"/"not a list" gap message rather than
        // crashing or silently passing.
        //
        // Codex #3893330383 on #1937: the gate's evaluation envelope needs
        // this parse, but the node's own emitted output must see the SAME
        // parsed value too, or the gate can certify `field_present`/
        // `non_empty_list` while a downstream `=item.json.<field>` binding
        // still resolves to null.
        //
        // CodeRabbit #3893565788 review: gated on `postcondition_declared`,
        // computed only when a postcondition is actually declared — this
        // parse (and the merge it feeds, below) must not run for the vast
        // majority of agent nodes that never opted into structured-output
        // evaluation. Before this gate, ANY agent node whose reply happened
        // to parse as a JSON object had that object's keys merged into its
        // emitted output, changing the output contract for every existing
        // workflow whether or not it ever declared a postcondition — a
        // `=item.json.<field>` binding that reliably resolved to null for
        // every past run could start resolving to model-controlled content
        // depending on what the agent happened to reply this run, for a node
        // that never asked for structured output at all.
        let postcondition_declared = request.get("postcondition").is_some();
        let mut parsed_reply = if postcondition_declared {
            serde_json::from_str::<Value>(outcome.reply.trim()).unwrap_or(Value::Null)
        } else {
            Value::Null
        };

        if let Some(spec) = request.get("postcondition") {
            let envelope = json!({
                "text": outcome.reply,
                "agent_ref": agent_ref,
                "json": parsed_reply.clone(),
            });
            if let Err(gap) = postcondition::evaluate_postcondition(spec, &envelope) {
                let message = format!(
                    "workflow node `{}` failed its postcondition: {gap}",
                    node_id.as_deref().unwrap_or(agent_ref)
                );
                self.settle_attempt(
                    run_sink.as_ref(),
                    crate::ports::RunStatus::Failed,
                    Some(message.clone()),
                )
                .await;
                return Err(EngineError::Capability(message));
            }
        }

        // Issue #1680: a ceiling hit joins the two stops that skip the judge.
        // There is no output to judge -- the draft never existed -- so running
        // the gate would spend a model call to be told that the pause copy is
        // not a sufficient answer, which is already known here.
        if outcome.budget_paused.is_none()
            && outcome.halted_for_spend.is_none()
            && outcome.ceiling_paused.is_none()
            && let Some(verify) = request.get("verify")
        {
            let criteria = verify
                .get("criteria")
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty());
            let verdict = crate::workflows::judge::judge_sufficiency(
                &self.deps,
                &self.company,
                crate::workflows::judge::JudgeInput {
                    instruction: &message,
                    output: &outcome.reply,
                    criteria,
                    execution_failed: outcome.hit_iteration_cap
                        || outcome.budget_paused.is_some()
                        || outcome.ceiling_paused.is_some(),
                },
            )
            .await;
            match verdict {
                crate::workflows::judge::SufficiencyVerdict::Continue => {}
                crate::workflows::judge::SufficiencyVerdict::Retry => {
                    let message = format!(
                        "workflow node `{lineage_node}` did not produce a semantically sufficient output"
                    );
                    self.settle_attempt(
                        run_sink.as_ref(),
                        crate::ports::RunStatus::Failed,
                        Some(message.clone()),
                    )
                    .await;
                    return Err(EngineError::Capability(message));
                }
                crate::workflows::judge::SufficiencyVerdict::Recover => {
                    let question = criteria.unwrap_or(&instruction);
                    let recovered = self.recover_context(question, agent_ref).await;
                    let recovered_and_sufficient = match &recovered.evidence {
                        Some(evidence) => {
                            let verified = crate::workflows::judge::augment_with_recovery(
                                &outcome.reply,
                                evidence,
                            );
                            let reverdict = crate::workflows::judge::judge_sufficiency(
                                &self.deps,
                                &self.company,
                                crate::workflows::judge::JudgeInput {
                                    instruction: &message,
                                    output: &verified,
                                    criteria,
                                    execution_failed: false,
                                },
                            )
                            .await;
                            (reverdict == crate::workflows::judge::SufficiencyVerdict::Continue)
                                .then_some(verified)
                        }
                        None => None,
                    };
                    if let Some(verified) = recovered_and_sufficient {
                        if let Some(spec) = request.get("postcondition") {
                            let recovered_parsed = serde_json::from_str::<Value>(verified.trim())
                                .unwrap_or(Value::Null);
                            let envelope = json!({
                                "text": &verified,
                                "agent_ref": agent_ref,
                                "json": recovered_parsed,
                            });
                            if let Err(gap) = postcondition::evaluate_postcondition(spec, &envelope)
                            {
                                let message = format!(
                                    "workflow node `{}` recovered a reply that still fails its postcondition: {gap}",
                                    node_id.as_deref().unwrap_or(agent_ref)
                                );
                                self.settle_attempt(
                                    run_sink.as_ref(),
                                    crate::ports::RunStatus::Failed,
                                    Some(message.clone()),
                                )
                                .await;
                                return Err(EngineError::Capability(message));
                            }
                        }
                        outcome.reply = verified;
                    } else {
                        let message = format!(
                            "workflow node `{lineage_node}` needs missing information; recovery tried {}",
                            recovered.log
                        );
                        if let Some(approval_id) = self
                            .park_node_blocker_as(
                                &lineage_node,
                                &message,
                                crate::ports::blockers::BlockerKind::Information,
                                crate::ports::blockers::BlockerSource::AgentQuestion,
                                "Provide the missing information the workflow node needs.",
                            )
                            .await
                        {
                            self.blocks.push(crate::ports::WorkflowBlockedNode {
                                node_id: lineage_node.clone(),
                                tools: Vec::new(),
                                approval_ids: vec![approval_id],
                                unparkable: 0,
                                stranded: 0,
                                blockers: 1,
                            });
                            self.settle_attempt(
                                run_sink.as_ref(),
                                crate::ports::RunStatus::Blocked,
                                Some(message.clone()),
                            )
                            .await;
                            return Err(EngineError::Capability(message));
                        }
                        self.settle_attempt(
                            run_sink.as_ref(),
                            crate::ports::RunStatus::Failed,
                            Some(message.clone()),
                        )
                        .await;
                        return Err(EngineError::Capability(message));
                    }
                }
                crate::workflows::judge::SufficiencyVerdict::Escalate { gap } => {
                    let message = format!(
                        "workflow node `{lineage_node}` needs {} intervention after semantic verification",
                        gap.as_str()
                    );
                    if gap.parks()
                        && let Some(approval_id) = self
                            .park_node_blocker_as(
                                &lineage_node,
                                &message,
                                gap,
                                crate::ports::blockers::BlockerSource::AgentQuestion,
                                "Resolve the gap identified by the workflow sufficiency judge.",
                            )
                            .await
                    {
                        self.blocks.push(crate::ports::WorkflowBlockedNode {
                            node_id: lineage_node.clone(),
                            tools: Vec::new(),
                            approval_ids: vec![approval_id],
                            unparkable: 0,
                            stranded: 0,
                            blockers: 1,
                        });
                        self.settle_attempt(
                            run_sink.as_ref(),
                            crate::ports::RunStatus::Blocked,
                            Some(message.clone()),
                        )
                        .await;
                        return Err(EngineError::Capability(message));
                    }
                    self.settle_attempt(
                        run_sink.as_ref(),
                        crate::ports::RunStatus::Failed,
                        Some(message.clone()),
                    )
                    .await;
                    return Err(EngineError::Capability(message));
                }
                crate::workflows::judge::SufficiencyVerdict::HaltBenign => {
                    let message = format!(
                        "workflow node `{lineage_node}` concluded that no further work was needed"
                    );
                    self.halted.push(lineage_node.clone());
                    self.settle_attempt(
                        run_sink.as_ref(),
                        crate::ports::RunStatus::Declined,
                        Some(message.clone()),
                    )
                    .await;
                    return Err(EngineError::Capability(message));
                }
            }
        }

        // A recovering verify pass rewrites `outcome.reply`, so the parse the
        // emitted value merges has to describe the reply this node actually
        // ships.
        if postcondition_declared {
            parsed_reply =
                serde_json::from_str::<Value>(outcome.reply.trim()).unwrap_or(Value::Null);
        }

        // Mirror the engine's `{ json, text, raw }` envelope shape: expose the
        // reply as `text` so a downstream `=item.text` binding resolves. A
        // workflow node carries no chat bubble, so the turn's steps are dropped
        // here (they surface only on operator/desk chat replies).
        // Mirror the engine's `{ json, text, raw }` envelope shape: expose the
        // reply as `text` so a downstream `=item.text` binding resolves.
        // A capped turn is a real, partial checkpoint rather than a completed
        // answer. Keep the engine's typed `LimitStop` outcome below, but do not
        // let the durable attempt claim that this node finished successfully.
        //
        // Issue #1846 review (Codex #3864988168): a budget pause gets the same
        // treatment, for the same reason. The model call itself errored, so
        // `outcome.reply` is the pause notice, not an answer — before this it
        // fell into the `else` arm below, the attempt settled `Succeeded`, and
        // the pause text flowed downstream through `=items` as if it were the
        // node's real output. There is no engine-level resume for this today
        // (see `StopReason::Paused`'s own doc — an agent node is not
        // re-enterable), so this reuses the already-supported `LimitStop`
        // shape rather than inventing a resume path that PR did not wire: the
        // node blocks the branch exactly as a capped turn does, and the durable
        // per-agent marker `run_background_workflow` already parked is what the
        // console's "Add credits & resend" redeems — outside the engine, via
        // the same `OperatorMessage` cycle path every redeem takes.
        let (status, error) = if outcome.hit_iteration_cap {
            // Issue #1865: the SAME signal that settles this attempt row
            // `Failed` also tells the runner which node's row to relabel —
            // see `RunCappedNodes` for why this must not be a second detector
            // re-deriving the same fact from somewhere else. `lineage_node` is
            // the resolved node id this whole turn ran as (the graph node id
            // when there is one, else the agent ref), the same id `nodes`
            // carries its row under.
            self.capped.push(lineage_node.clone());
            (
                crate::ports::RunStatus::Failed,
                Some("agent stopped at the max_tool_iterations cap before finishing".to_string()),
            )
        } else if let Some(pause) = &outcome.budget_paused {
            // PR #1883 review (Codex #3874941288): the same disagreement
            // #1865 closes for a capped turn exists here too.
            // `tinyflows::observability` reports `StepStatus::Success` for a
            // budget-paused turn exactly as it does for a capped one — the
            // engine already routes both through the identical `LimitStop`
            // envelope (see `AgentRunner::run` below) — so the row lands `Ok`
            // while this settle marks the attempt `Failed`. Feed the same
            // `RunCappedNodes` channel `reclassify_capped_nodes` reads,
            // rather than leave this arm as a second, unreconciled failure
            // mode: a `capped` node id is a "the row and the attempt must
            // agree" signal, not literally "hit the iteration cap", and a
            // budget pause makes the identical partial-checkpoint claim.
            self.capped.push(lineage_node.clone());
            (
                crate::ports::RunStatus::Failed,
                Some(format!(
                    "agent paused for lack of inference budget/credits: {}",
                    pause.summary
                )),
            )
        } else if let Some(halt) = &outcome.halted_for_spend {
            self.capped.push(lineage_node.clone());
            (
                crate::ports::RunStatus::Failed,
                Some(crate::harness::built_in::brain::spend_halt_notice(halt)),
            )
        } else if let Some(pause) = &outcome.ceiling_paused {
            // Issue #1680. Same `capped` channel as the three arms above, for
            // the reason the budget arm states: the engine routes every one of
            // these through `LimitStop`, so `tinyflows::observability` reports
            // the step `Success` while this settle marks the attempt `Failed`,
            // and `reclassify_capped_nodes` is what reconciles the two.
            //
            // Before this issue a ceiling hit never reached here at all — it
            // left `run_turn` as an `Err` and took the run down with it. The
            // node now settles `Failed` with an explanation, and the run
            // continues to whatever the graph does next, which is what
            // "**Send update** is reached" on #1680 asked for.
            self.capped.push(lineage_node.clone());
            (
                crate::ports::RunStatus::Failed,
                // The LONG copy, not the chat notice: `RunHistoryPanel` renders
                // an attempt's error as the row's headline, and the leaf
                // #1761 appends verbatim is the only thing that names which
                // call was in flight. The short, actionable notice is for the
                // chat bubble, where a debugging leaf would be noise.
                Some(pause.summary.clone()),
            )
        } else {
            (crate::ports::RunStatus::Succeeded, None)
        };
        self.settle_attempt(run_sink.as_ref(), status, error).await;
        // `value` becomes `AgentRunOutcome.json` in `run` below, which lands at
        // the engine's item envelope `json` (tinyflows' `finish_agent_run` —
        // `Value::Object`/`Value::Array` pass through unchanged, anything else
        // becomes `Null`) — i.e. this literal object IS what a downstream
        // `=item.json.<field>` binding reads. Reflecting `parsed_reply` here
        // (Codex #3893330383 on #1937) makes that binding resolve to the SAME
        // value the postcondition gate above just certified, instead of a
        // wrapper that never carried it.
        //
        // Scoped to `postcondition_declared` (CodeRabbit #3893565788): every
        // agent node without a declared postcondition keeps the exact
        // `{text, agent_ref}` shape it always had, unaffected by whatever the
        // model happened to reply — the behavior change is confined to the
        // population that opted into structured-output evaluation.
        let mut value = json!({ "text": outcome.reply, "agent_ref": agent_ref });
        if postcondition_declared {
            match &parsed_reply {
                // `text`/`agent_ref` are already in `value` and merged with
                // `or_insert` (base wins on any key collision) rather than the
                // other way around: `delivery.rs::report_text` reads a
                // delivered report's body via `item.json.text` (falling back
                // to a nested `item.json.json.text`, its own doc names this
                // exact double-wrap), so `value["text"]` must always stay the
                // raw reply string — a reply that happens to parse as JSON
                // must not make the delivered report lose its prose to the
                // parsed object's own (irrelevant) `text` key.
                Value::Object(parsed_map) => {
                    if let Value::Object(out_map) = &mut value {
                        for (k, v) in parsed_map {
                            out_map.entry(k.clone()).or_insert_with(|| v.clone());
                        }
                    }
                }
                // Codex #3893541856 review: a bare JSON array (the no-`field`
                // `non_empty_list` case) can't merge into the `{text,
                // agent_ref}` object shape — so replace `value` wholesale
                // with the array itself rather than dropping it, the same
                // "downstream must see what the gate certified" reasoning as
                // the object case. `=item.json` (the whole value) then
                // resolves to the exact array `non_empty_list` validated.
                // `item.text` (a separate, top-level field on the emitted
                // outcome, not nested under `json`) still independently
                // carries the raw reply string, so nothing that reads the
                // prose loses it — only `item.json.text` specifically stops
                // resolving for this one node, and only because this node
                // declared it wants list-shaped output, not prose.
                Value::Array(_) => {
                    value = parsed_reply.clone();
                }
                // Codex #3894162757 on #1937 — a scalar reply does NOT get
                // the same wholesale-replace treatment as an array, despite
                // looking like the same case. An earlier round tried exactly
                // that (`value = parsed_reply.clone()` here too) and it was
                // wrong: unlike an array, a bare scalar can never survive to
                // a downstream binding regardless of what this function
                // does. tinyflows' own envelope construction
                // (`finish_agent_run` / `envelope::structured_of`, vendored)
                // clamps `AgentRunOutcome.json` to `Value::Null` for
                // anything that is not an `Object`/`Array` — "scalars carry
                // no structure" is that crate's own stated invariant, not
                // something this function can opt out of. Setting `value` to
                // a bare `42` here just moves the wrong-value-downstream bug
                // one layer out: `run_turn` would return the certified `42`,
                // but the ENGINE would still null it before any `=item.json`
                // binding ever saw it — proven end-to-end by
                // `workflows::runner::tests_node_output::
                // a_scalar_reply_cannot_satisfy_field_present_on_the_bare_json_root`.
                // The gate itself now refuses to certify this shape in the
                // first place (`postcondition::evaluate_postcondition`'s
                // `field_present` arm rejects a scalar under the bare `json`
                // root) — an author who wants a scalar delivered needs the
                // agent to reply with an object naming it
                // (`{"score": 42}`) and target the dotted path
                // (`field = "json.score"`), which already works via the
                // `Value::Object` merge arm above. So this arm is
                // deliberately absent: a scalar falls to the catch-all below,
                // same as any other reply that cannot merge cleanly.
                //
                // Not valid JSON (the common case, even among nodes that
                // declared a postcondition — e.g. `non_empty` needs only
                // prose), a literal JSON `null` reply, or — per the above — a
                // bare scalar: leave `value` as the ordinary `{text,
                // agent_ref}` shape. Nothing here can usefully replace it.
                _ => {}
            }
        }
        Ok((value, outcome))
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
        // The legacy shape, unchanged: a bare value, transcript discarded. Kept
        // because the trait requires it, but nothing in this host calls it —
        // `run` below is what the engine reaches.
        self.run_turn(agent_ref, request)
            .await
            .map(|(value, _)| value)
    }

    async fn run(&self, request: AgentRunRequest) -> TfResult<AgentRunOutcome> {
        let (value, outcome) = self
            .run_turn(&request.agent.id, request.config.clone())
            .await?;

        // Issue #926, now expressible. A turn that stopped at its tool-iteration
        // cap returns an ordinary reply that *reads* like a finished answer, and
        // until this override the node reported `Finished` for it — the engine
        // then bound a half-done answer downstream as if it were the whole one.
        // `LimitStop` is the honest word and the engine already warns on it.
        //
        // The parked-approval case is deliberately NOT reported as
        // `StopReason::Paused`: `run_turn` returns `Err` for it so the runner can
        // reclassify the node as Blocked, and an agent node is not re-enterable
        // (see the #881 block above). Nothing here changes that.
        // A budget pause is not a finish either — it settles `Failed` where
        // `run_turn` closes above, so it gets the same `LimitStop` override
        // rather than falling into `Finished` and binding the pause notice
        // downstream as a real result.
        let stop = if outcome.hit_iteration_cap {
            StopReason::LimitStop {
                limit: "max_tool_iterations".to_string(),
            }
        } else if outcome.budget_paused.is_some() {
            StopReason::LimitStop {
                limit: "budget_exhausted".to_string(),
            }
        } else if outcome.halted_for_spend.is_some() {
            StopReason::LimitStop {
                limit: "spend_halt".to_string(),
            }
        } else if outcome.ceiling_paused.is_some() {
            // Issue #1680: a ceiling hit is not a finish either. Reporting
            // `Finished` here would bind the pause copy downstream as if it
            // were the node's deliverable -- exactly the failure mode #1880's
            // review identified for a refused or cancelled turn.
            StopReason::LimitStop {
                limit: "wall_clock_ceiling".to_string(),
            }
        } else {
            StopReason::Finished
        };

        let transcript = transcript_from_steps(&outcome.steps);
        Ok(AgentRunOutcome {
            stop,
            text: Some(outcome.reply.clone()),
            json: value.clone(),
            raw: value,
            usage: None,
            transcript,
        })
    }
}

/// The snake_case wire word for a step failure, matching how it serializes.
fn failure_word(failure: crate::ports::types::TurnStepFailure) -> &'static str {
    failure.wire_word()
}

/// Folds a turn's scrubbed [`TurnStep`]s into engine transcript entries.
///
/// The host half of the contract `tinyflows::transcript` describes: the engine
/// carries entries and never invents them, because only the harness knows what
/// counts as one. This is the narrow, faithful projection — one entry per step,
/// in the order the steps were folded.
///
/// What it deliberately does NOT do is widen what a step already carries.
/// `detail` is the redacted argument summary and `result` the shape of what came
/// back; both arrive here already scrubbed by `harness::built_in::steps`, and a
/// transcript is a *record*, not a second, laxer disclosure surface. A reader
/// wanting the raw bodies goes to the deep-trace store, which is company-scoped
/// for exactly that reason.
///
/// `at_ms` is 0 on every entry: a `TurnStep` carries an elapsed duration, not a
/// wall-clock stamp, and inventing one from `now()` would put a timestamp on the
/// *fold* rather than on the event. The order is the truth here; the duration
/// rides in the text where it is honest.
fn transcript_from_steps(steps: &[crate::ports::types::TurnStep]) -> Vec<TranscriptEntry> {
    use crate::ports::types::{TurnStepKind, TurnStepStatus};

    steps
        .iter()
        .map(|step| {
            // The engine's vocabulary, which is an open set on purpose — a kind
            // it does not recognise still renders as a timestamped line.
            let kind = match (step.kind, step.status) {
                (TurnStepKind::Thinking, _) => "agent_thinking",
                (TurnStepKind::Note, _) => "agent_message",
                (TurnStepKind::ToolCall, TurnStepStatus::Ok) => "tool_result",
                (TurnStepKind::ToolCall, TurnStepStatus::Error) => "error",
                // Neither finished nor failed: the two states an operator can
                // still act on, and the two a bare "tool_call" would hide.
                (TurnStepKind::ToolCall, TurnStepStatus::AwaitingApproval) => {
                    "tool_awaiting_approval"
                }
                (TurnStepKind::ToolCall, TurnStepStatus::Running) => "tool_call",
            };

            let mut text = step.label.clone();
            if let Some(detail) = step.detail.as_deref().filter(|d| !d.is_empty()) {
                text.push_str(": ");
                text.push_str(detail);
            }
            if let Some(result) = step.result.as_deref().filter(|r| !r.is_empty()) {
                text.push_str(" → ");
                text.push_str(result);
            }
            if step.truncated {
                text.push_str(" [truncated]");
            }
            if let Some(failure) = step.failure {
                // `TurnStepFailure` has no `as_str`, and Debug would print
                // PascalCase where every other wire name here is snake_case.
                text.push_str(&format!(" [{}]", failure_word(failure)));
            }
            if let Some(elapsed) = step.elapsed_ms {
                text.push_str(&format!(" ({elapsed}ms)"));
            }

            // `bounded` applies the crate's own per-entry ceiling, so a long
            // result summary cannot make one entry the whole record's budget.
            TranscriptEntry::bounded(0, kind, text)
        })
        .collect()
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
    // Gated calls and blocker questions resume differently: a gated call's
    // approval re-runs the turn, a blocker's verdict re-enters, waives or stops
    // the step — the sentence below has to name only the verdicts the console
    // can actually send for whichever shape (or mix) is waiting. Both blocker
    // arms name them from one fragment, so neither can be reworded alone.
    let choices = crate::ports::blockers::BLOCKER_VERDICT_CHOICES;
    let resume = if waiting == 0 {
        String::new()
    } else if parked.blockers == 0 {
        " Approving the card continues this run automatically; because approving re-runs the \
         agent's turn, a changed decision may ask again."
            .to_string()
    } else if parked.blockers == waiting {
        format!(
            " {} a question the agent raised, not a call waiting to be authorised: answering it \
             re-enters this step — {choices}.",
            if waiting == 1 {
                "The card is"
            } else {
                "The cards are"
            }
        )
    } else {
        format!(
            " Some of these are gated tool calls, which continue this run when approved; the \
             rest are questions the agent raised, which re-enter the step they stopped — \
             {choices}."
        )
    };
    format!(
        "workflow node '{node}' is blocked: {tools} needed approval before {agent_ref} could \
         finish, so the node produced no deliverable and nothing after it ran. {}.{resume}",
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
#[path = "caps_budget_postcondition_tests.rs"]
mod tests_budget_postcondition;
#[cfg(test)]
#[path = "caps_fan_in_truncation_tests.rs"]
mod tests_fan_in_truncation;
#[cfg(test)]
#[path = "caps_field_present_tests.rs"]
mod tests_field_present;
#[cfg(test)]
#[path = "caps_multi_call_notices_tests.rs"]
mod tests_multi_call_notices;
#[cfg(test)]
#[path = "caps_node_failure_tests.rs"]
mod tests_node_failure;
#[cfg(test)]
#[path = "caps_notices_parking_tests.rs"]
mod tests_notices_parking;
#[cfg(test)]
#[path = "caps_peer_ladder_tests.rs"]
mod tests_peer_ladder;
#[cfg(test)]
#[path = "caps_peer_record_tests.rs"]
mod tests_peer_record;
#[cfg(test)]
#[path = "caps_recovery_evidence_tests.rs"]
mod tests_recovery_evidence;
#[cfg(test)]
#[path = "caps_turn_dispatch_tests.rs"]
mod tests_turn_dispatch;
