//! The tinyflows [`Capabilities`] bundle for a company workflow run.
//!
//! tinyflows is host-agnostic: every outside-world effect is a trait the host
//! implements. This module supplies that bundle for an OpenCompany run. The one
//! capability that is actually wired is the **agent** runner: an `agent` node
//! (config `agent_ref` = a roster teammate id) routes to the company's
//! [`HarnessPool`](crate::harness::HarnessPool) via [`HarnessAgentRunner`], so
//! the step runs on the same live openhuman agent as chat/task dispatch —
//! inheriting its persona, model, [`OcMemory`](crate::harness::memory), approval
//! policy, and cost metering. No second pool is constructed, and nothing boots
//! an OpenHuman global `Config`.
//!
//! The remaining capabilities (`tool_call`, `http_request`, `code`,
//! `sub_workflow`, and the bare-completion `LlmProvider` fallback) are **not yet
//! wired** for company workflows. They are represented by explicit stubs that
//! return a clear capability error rather than a silent no-op, so a workflow that
//! reaches one fails loudly and legibly; a workflow that never reaches one (the
//! agent-only path) is unaffected. Wiring these to the company's real tool
//! provider / HTTP surface is a documented follow-on.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tinyflows::caps::{
    AgentRunner, Capabilities, CodeLanguage, CodeRunner, HttpClient, LlmProvider, StateStore,
    ToolInvoker, WorkflowResolver,
};
use tinyflows::error::{EngineError, Result as TfResult};
use tinyflows::model::WorkflowGraph;

use crate::harness::{HarnessDeps, HarnessPool};
use crate::ports::types::CompanyId;

/// Assembles the [`Capabilities`] bundle for a run: the harness-backed agent
/// runner plus the not-yet-wired stubs for every other capability.
///
/// `pool`/`deps`/`company` are shared with the rest of the harness surface —
/// the roster the agent nodes address is the one already resident in `pool`.
pub fn build_capabilities(
    pool: Arc<HarnessPool>,
    deps: HarnessDeps,
    company: CompanyId,
) -> Capabilities {
    Capabilities {
        llm: Arc::new(UnwiredLlm),
        tools: Arc::new(UnwiredTools),
        http: Arc::new(UnwiredHttp),
        code: Arc::new(UnwiredCode),
        state: Arc::new(NoopState),
        resolver: Arc::new(UnwiredResolver),
        agent: Some(Arc::new(HarnessAgentRunner::new(pool, deps, company))),
    }
}

/// A tinyflows [`AgentRunner`] that executes an `agent` node on the company's
/// [`HarnessPool`].
///
/// The engine calls [`run_agent`](AgentRunner::run_agent) with the node's
/// resolved config as `request` and the (trusted) `agent_ref` as the roster
/// teammate id. This extracts the turn message from the request and runs it
/// through [`HarnessPool::run`], which meters the turn's cost through `deps` —
/// so a workflow step and a chat turn account identically.
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
            .run(&self.company, agent_ref, &message, &self.deps)
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
/// string when present (what [`translate`](super::translate) writes), else the
/// `input`/`message` string, else the whole request serialized as a fallback.
fn message_from_request(request: &Value) -> String {
    for key in ["prompt", "input", "message"] {
        if let Some(text) = request.get(key).and_then(Value::as_str) {
            return text.to_string();
        }
    }
    request.to_string()
}

/// The bare-completion fallback. An `agent` node with no `agent_ref` would land
/// here; [`translate`](super::translate) always sets `agent_ref` for a roster
/// agent, so reaching this means an agent node with no teammate assigned.
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

/// `tool_call` execution is not yet wired for company workflows.
struct UnwiredTools;

#[async_trait]
impl ToolInvoker for UnwiredTools {
    async fn invoke(&self, slug: &str, _args: Value, _conn: Option<&str>) -> TfResult<Value> {
        Err(EngineError::Capability(format!(
            "tool_call '{slug}' is not yet wired for company workflows"
        )))
    }
}

/// `http_request` execution is not yet wired for company workflows.
struct UnwiredHttp;

#[async_trait]
impl HttpClient for UnwiredHttp {
    async fn request(&self, _request: Value, _conn: Option<&str>) -> TfResult<Value> {
        Err(EngineError::Capability(
            "http_request is not yet wired for company workflows".to_string(),
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

/// The engine requires a [`StateStore`] in the bundle; no OpenCompany node kind
/// uses durable run state, so this is an inert no-op (a miss reads as `None`,
/// a store is dropped).
struct NoopState;

#[async_trait]
impl StateStore for NoopState {
    async fn load(&self, _key: &str) -> TfResult<Option<Value>> {
        Ok(None)
    }
    async fn store(&self, _key: &str, _value: Value) -> TfResult<()> {
        Ok(())
    }
}

/// `sub_workflow`-by-id is never emitted by translation (OpenCompany has no such
/// node kind); wired to an error for completeness.
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
}
