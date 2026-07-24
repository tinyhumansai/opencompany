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

/// Runs `workflow` for `company` on the tinyflows engine with the trigger
/// `input`, returning the final run state and any nodes left pending approval.
///
/// The caller is responsible for having the company's roster resident in `pool`
/// (agent nodes address it by teammate id) — [`HarnessWorkflowRunner::run`] does
/// this via [`HarnessPool::ensure`] before delegating here.
pub async fn run_workflow(
    pool: Arc<HarnessPool>,
    deps: HarnessDeps,
    company: CompanyId,
    workflow: &WorkflowFile,
    input: Value,
) -> Result<WorkflowRun> {
    let graph = super::translate::translate(workflow);
    let compiled = tinyflows::compiler::compile(&graph).map_err(map_engine_error)?;
    let capabilities = super::caps::build_capabilities(pool, deps, company);
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
            self.record.id.clone(),
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
        }
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
            rec.id.clone(),
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
            }],
            edges: Vec::new(),
        };
        let err = run_workflow(
            Arc::new(HarnessPool::new()),
            deps(dir.path()),
            CompanyId::new("acme"),
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
}
