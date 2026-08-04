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
//!   from the union of the company's seed `workflows/` directory
//!   ([`HarnessDeps::workflow_source_dir`](crate::harness::HarnessDeps)) and the
//!   record's runtime-authored graph bodies (full validation + a static cycle
//!   guard). A platform-provisioned tenant has no source directory, so every
//!   child it owns resolves from the record.
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
///
/// `run_request` is the operator's topic for this run (issue #154), threaded to
/// the agent capability so every agent node's turn message carries what was
/// actually asked, not just the node's authored instruction.
pub async fn build_capabilities(
    pool: Arc<HarnessPool>,
    deps: HarnessDeps,
    record: &CompanyRecord,
    workflow_id: &str,
    run_id: &str,
    run_request: Option<String>,
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

    // sub_workflow-by-id resolves children from the union of the company's seed
    // `workflows/` directory and the record's runtime-authored bodies — so a
    // platform tenant with no source dir still resolves the workflows it
    // created (issue #168). Read before `deps` moves into the agent runner.
    let resolver: Arc<dyn WorkflowResolver> = Arc::new(StoreWorkflowResolver::new(
        deps.workflow_source_dir.clone(),
        deps.store.clone(),
        company.clone(),
        workflow_id.to_string(),
    ));

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
        agent: Some(Arc::new(HarnessAgentRunner::new(
            pool,
            deps,
            company,
            run_request,
        ))),
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
    /// What the operator asked for on this run (issue #154), when they supplied
    /// it. A node's `prompt` is authored into the graph and is the same on every
    /// run, so without this the run's topic never reaches the teammate doing the
    /// work — the agent would run, find no subject, and ask for one.
    run_request: Option<String>,
}

impl HarnessAgentRunner {
    /// Builds a runner over an already-populated pool for `company`, carrying
    /// the operator's run request (issue #154) when one was supplied.
    pub fn new(
        pool: Arc<HarnessPool>,
        deps: HarnessDeps,
        company: CompanyId,
        run_request: Option<String>,
    ) -> Self {
        Self {
            pool,
            deps,
            company,
            run_request,
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
        let message =
            compose_turn_message(&message_from_request(&request), self.run_request.as_deref());
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
