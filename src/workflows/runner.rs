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
use crate::ports::types::{CompanyId, CompanyRecord};
use crate::ports::{WorkflowRun, WorkflowRunner};

/// How deeply a workflow may re-enter itself before the run is refused
/// (issue #151 part a).
///
/// One level of nesting is legitimate and useful — a `sub_workflow` node, or a
/// workflow whose agent node asks the orchestrator to run a second, different
/// graph. Beyond that a chain is almost certainly a cycle rather than a plan,
/// and the cost of being wrong is asymmetric: refusing a deep run returns a
/// readable tool error, while allowing it aborts the host.
pub(crate) const MAX_WORKFLOW_DEPTH: usize = 4;

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
            run_workflow_inner(pool, deps, record, workflow, input),
        )
        .await
}

/// The run itself, always executed inside a [`WORKFLOW_DEPTH`] scope so a
/// nested run sees this one on the chain.
async fn run_workflow_inner(
    pool: Arc<HarnessPool>,
    deps: HarnessDeps,
    record: &CompanyRecord,
    workflow: &WorkflowFile,
    input: Value,
) -> Result<WorkflowRun> {
    let graph = super::translate::translate(workflow);
    let compiled = tinyflows::compiler::compile(&graph).map_err(map_engine_error)?;
    let run_id = uuid::Uuid::new_v4().to_string();
    // Issue #154: the operator's run request rides the trigger payload. Pull it
    // out before the input is handed to the engine so every agent node's turn
    // message carries the topic — a node's authored `prompt` is the same on
    // every run and cannot say what was asked this time.
    let run_request = super::caps::run_request_text(&input);
    // Issue #170: the delivery ports are read off `deps` BEFORE it moves into
    // the capability bundle. Delivery is host-side and post-engine, so it is not
    // a capability — the engine never learns a report has a destination.
    let delivery = deps.delivery.clone();
    let capabilities =
        super::caps::build_capabilities(pool, deps, record, &workflow.id, &run_id, run_request)
            .await;
    let outcome = tinyflows::engine::run(&compiled, input, &capabilities)
        .await
        .map_err(map_engine_error)?;

    // Route every reached `output` node's report to its configured destination.
    // Deliberately here rather than in the HTTP handler: the orchestrator's
    // `run_workflow` tool and the trigger scheduler drive this same path, and a
    // scheduled run is exactly the case where nobody is watching the console's
    // run-result drawer. Never fails the run — each attempt is reported instead.
    let deliveries =
        super::delivery::deliver_outputs(delivery.as_ref(), record, workflow, &outcome.output)
            .await;

    Ok(WorkflowRun {
        output: outcome.output,
        pending_approvals: outcome.pending_approvals,
        deliveries,
    })
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
        let run = WorkflowRunner::run(&runner, &rec.id, &file, serde_json::json!({}))
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
                )
                .await
            })
            .await;
        assert!(out.is_ok(), "a run below the limit must execute: {out:?}");
    }
}
