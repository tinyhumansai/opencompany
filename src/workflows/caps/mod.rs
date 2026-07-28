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
//!   `web`) scoped to a dedicated per-company workflow workspace, fail-closed on
//!   the company's `[tools].allow` grants.
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
//!   from the company's on-disk `workflows/` directory (full validation + a
//!   static cycle guard), when a source directory is wired
//!   ([`HarnessDeps::workflow_source_dir`](crate::harness::HarnessDeps)). A
//!   platform-provisioned tenant with no source directory keeps the
//!   [`UnwiredResolver`] stub.
//!
//! Still **not wired**: the bare-completion `LlmProvider` fallback and `code`
//! nodes. They are explicit stubs that return a clear capability error rather
//! than a silent no-op, so a workflow that reaches one fails loudly; a workflow
//! that never reaches one is unaffected.

mod http;
mod resolver;
mod state;
mod tools;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tinyflows::caps::{
    AgentRunner, Capabilities, CodeLanguage, CodeRunner, LlmProvider, StateStore, WorkflowResolver,
};
use tinyflows::error::{EngineError, Result as TfResult};
use tinyflows::model::WorkflowGraph;

use crate::harness::policy::PolicyMode;
use crate::harness::{HarnessDeps, HarnessPool, toolbelt};
use crate::ports::types::{CompanyId, CompanyRecord};

use self::http::GuardedHttpClient;
use self::resolver::StoreWorkflowResolver;
use self::state::{CompanyStateStore, NoopState};
use self::tools::WorkflowToolInvoker;

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
pub async fn build_capabilities(
    pool: Arc<HarnessPool>,
    deps: HarnessDeps,
    record: &CompanyRecord,
    workflow_id: &str,
    run_id: &str,
) -> Capabilities {
    let company = record.id.clone();
    let mode = PolicyMode::parse(&record.manifest.policy.mode);
    let workflow_ws = workflow_workspace(&deps.workspace_root, &company, workflow_id, run_id);
    if let Err(err) = tokio::fs::create_dir_all(&workflow_ws).await {
        tracing::warn!(
            company = %company,
            workspace = %workflow_ws.display(),
            %err,
            "workflow: could not create the per-run workspace"
        );
    }

    // ONE exec-security policy shared by the tool_call toolbelt and the
    // http_request client, sandboxed to the workflow workspace with the
    // company's autonomy tier — exactly the shape a roster agent's exec tools get.
    let exec_security = Arc::new(toolbelt::exec_security(&workflow_ws, mode));
    let web_allowed_domains = record.manifest.tools.web_allowed_domains.clone();
    let grants = record.manifest.tools.allow.clone();

    let tools = WorkflowToolInvoker::new(
        exec_security.clone(),
        &workflow_ws,
        web_allowed_domains.clone(),
        grants,
        &deps.capabilities,
    );
    let http = GuardedHttpClient::new(exec_security, web_allowed_domains);

    // Durable run state over the per-company secret store, namespaced by
    // workflow id. `None` (default/tests) keeps the inert no-op with a warning —
    // no node OpenCompany emits reads state in P1, so this never blocks a run.
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

    // sub_workflow-by-id resolves children from the company's on-disk
    // `workflows/` directory when a source dir is wired; a platform tenant with
    // none keeps the loud stub. Read before `deps` moves into the agent runner.
    let resolver: Arc<dyn WorkflowResolver> = match &deps.workflow_source_dir {
        Some(source_dir) => Arc::new(StoreWorkflowResolver::new(
            source_dir.clone(),
            workflow_id.to_string(),
        )),
        None => Arc::new(UnwiredResolver),
    };

    Capabilities {
        llm: Arc::new(UnwiredLlm),
        tools: Arc::new(tools),
        http: Arc::new(http),
        code: Arc::new(UnwiredCode),
        state,
        resolver,
        // `deps` moves in last — the borrows above (`deps.capabilities`,
        // `deps.secrets`, `deps.workspace_root`, `deps.workflow_source_dir`) are
        // all done by here.
        agent: Some(Arc::new(HarnessAgentRunner::new(pool, deps, company))),
    }
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
pub struct HarnessAgentRunner {
    pool: Arc<HarnessPool>,
    deps: HarnessDeps,
    company: CompanyId,
}

impl HarnessAgentRunner {
    /// Builds a runner over an already-populated pool for `company`.
    pub fn new(pool: Arc<HarnessPool>, deps: HarnessDeps, company: CompanyId) -> Self {
        Self {
            pool,
            deps,
            company,
        }
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
        let message = message_from_request(&request);
        tracing::debug!(
            company = %self.company,
            agent = agent_ref,
            "workflow agent node: routing to harness pool"
        );
        let outcome = self
            .pool
            .run_background(&self.company, agent_ref, &message, &self.deps)
            .await
            .map_err(|e| EngineError::Capability(format!("harness agent '{agent_ref}': {e}")))?;
        // Mirror the engine's `{ json, text, raw }` envelope shape: expose the
        // reply as `text` so a downstream `=item.text` binding resolves. A
        // workflow node carries no chat bubble, so the turn's steps are dropped
        // here (they surface only on operator/desk chat replies).
        Ok(json!({ "text": outcome.reply, "agent_ref": agent_ref }))
    }
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

/// The bare-completion fallback. An `agent` node with no `agent_ref` would land
/// here; [`translate`](crate::workflows::translate) always sets `agent_ref` for a
/// roster agent, so reaching this means an agent node with no teammate assigned.
struct UnwiredLlm;

#[async_trait]
impl LlmProvider for UnwiredLlm {
    async fn complete(&self, _request: Value, _conn: Option<&str>) -> TfResult<Value> {
        Err(EngineError::Capability(
            "workflow agent node has no roster agent; bare LLM completion is not wired for \
             company workflows"
                .to_string(),
        ))
    }
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

/// The `sub_workflow`-by-id fallback for a deployment with no source directory
/// (platform-provisioned mode): there is nowhere on disk to resolve a child
/// graph from, so a reached `sub_workflow` node fails loudly rather than
/// silently. A deployment WITH a source directory uses
/// [`StoreWorkflowResolver`](resolver::StoreWorkflowResolver) instead.
struct UnwiredResolver;

#[async_trait]
impl WorkflowResolver for UnwiredResolver {
    async fn resolve(&self, workflow_id: &str) -> TfResult<WorkflowGraph> {
        Err(EngineError::Capability(format!(
            "sub_workflow reference '{workflow_id}' is not supported for company workflows"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_prefers_prompt_then_input_then_message() {
        assert_eq!(
            message_from_request(&json!({ "prompt": "P", "input": "I" })),
            "P"
        );
        assert_eq!(message_from_request(&json!({ "input": "I" })), "I");
        assert_eq!(message_from_request(&json!({ "message": "M" })), "M");
    }

    #[test]
    fn message_falls_back_to_serialized_request() {
        // No known string key: fall back to the serialized object.
        let out = message_from_request(&json!({ "agent_ref": "x" }));
        assert!(out.contains("agent_ref"));
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
}
