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
    let graph = super::translate::translate(workflow);
    let compiled = tinyflows::compiler::compile(&graph).map_err(map_engine_error)?;
    let run_id = uuid::Uuid::new_v4().to_string();
    let capabilities =
        super::caps::build_capabilities(pool, deps, record, &workflow.id, &run_id).await;
    let outcome = tinyflows::engine::run(&compiled, input, &capabilities)
        .await
        .map_err(map_engine_error)?;
    Ok(WorkflowRun {
        output: outcome.output,
        pending_approvals: outcome.pending_approvals,
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
            overlay_desks: Vec::new(),
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
            skills: None,
            skills_source_dir: None,
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: crate::harness::orchestrator::DelegationQueue::default(),
            workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            steer: crate::company::steer::InflightRegistry::default(),
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
            overlay_desks: Vec::new(),
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
                config: None,
                on_error: None,
                retry: None,
                requires_approval: None,
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
}
