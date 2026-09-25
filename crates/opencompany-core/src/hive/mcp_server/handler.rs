//! The request side of the `opencompany` MCP server: bearer check, JSON-RPC
//! dispatch, and the four things a call can be — a speech tool folded into
//! the in-flight turn, a `read` served from the journal, an OpenCompany tool
//! decided under the agent's policy, or a hand-off of that decision to the
//! turn's own task. The host, the agents and the spec attachment are the
//! parent module's.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use openhuman_core::agent::tool_policy::{
    ToolCallContext, ToolPolicy, ToolPolicyDecision, ToolPolicyRequest,
};
use serde_json::{Value, json};

use super::{HEADER_SESSION_ID, McpAgent, McpHost, SERVER_SLUG, constant_time_eq};
use crate::hive::tools::{
    InFlight, InFlightContext, Speech, ToolJob, is_speech_tool, read_conversation,
};

fn bearer_of(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|token| !token.is_empty())
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer realm=\"opencompany\"")],
        "",
    )
        .into_response()
}

fn rpc_result(id: Value, result: Value) -> Response {
    axum::Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response()
}

fn rpc_error(id: Value, code: i64, message: impl Into<String>) -> Response {
    axum::Json(json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.into() },
    }))
    .into_response()
}

/// An MCP tool result: text content plus the `isError` flag the client
/// renders into `McpToolResult::is_error`.
fn tool_result(text: impl Into<String>, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text.into() }], "isError": is_error })
}

pub(super) async fn handle(
    State(host): State<Arc<McpHost>>,
    Path((company, runtime_agent_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(agent) = host.agent(&runtime_agent_id) else {
        return unauthorized();
    };
    let presented = bearer_of(&headers).unwrap_or_default();
    if !constant_time_eq(presented.as_bytes(), agent.bearer.as_bytes())
        || agent.company.to_string() != company
    {
        return unauthorized();
    }
    let accepts = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("application/json"));
    if !accepts {
        return (
            StatusCode::NOT_ACCEPTABLE,
            "Accept must include application/json",
        )
            .into_response();
    }
    let body: Value = match serde_json::from_slice(&body) {
        Ok(body) => body,
        Err(error) => return rpc_error(Value::Null, -32700, format!("parse error: {error}")),
    };
    let method = body
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    let params = body.get("params").cloned().unwrap_or(Value::Null);
    if id.is_null() {
        // A notification: `notifications/initialized`, `notifications/cancelled`.
        return StatusCode::ACCEPTED.into_response();
    }
    match method.as_str() {
        "initialize" => initialize(id, &params),
        "ping" => rpc_result(id, json!({})),
        "tools/list" => rpc_result(id, json!({ "tools": agent.catalogue() })),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let result = call(&host, &agent, name, arguments).await;
            rpc_result(id, result)
        }
        other => rpc_error(id, -32601, format!("method not found: {other}")),
    }
}

fn initialize(id: Value, params: &Value) -> Response {
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    let version = requested
        .filter(|v| tinymcp::tinymcp_bus::SUPPORTED_PROTOCOL_VERSIONS.contains(v))
        .unwrap_or(tinymcp::tinymcp_bus::LATEST_PROTOCOL_VERSION);
    (
        [(HEADER_SESSION_ID, uuid::Uuid::new_v4().simple().to_string())],
        axum::Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": {
                    "name": SERVER_SLUG,
                    "version": env!("CARGO_PKG_VERSION"),
                },
            },
        })),
    )
        .into_response()
}

async fn call(host: &McpHost, agent: &McpAgent, name: &str, arguments: Value) -> Value {
    if is_speech_tool(name) {
        if !agent.speech_tools.iter().any(|allowed| allowed == name) {
            return tool_result(format!("refused: '{name}' is not available to you"), true);
        }
        return call_speech(host, agent, name, &arguments).await;
    }
    if agent.tool(name).is_none() {
        return tool_result(format!("refused: unknown tool '{name}'"), true);
    }
    let turn = host.in_flight.snapshot(&agent.runtime_agent_id);
    // A turn that is running hands the call to its own task (see `ToolJob`);
    // anything else — a test over a bare agent, a registered turn nobody is
    // driving — is served here.
    if let Some(executor) = turn.as_ref().and_then(|turn| turn.executor.clone()) {
        let (reply, answer) = tokio::sync::oneshot::channel();
        let job = ToolJob {
            tool: name.to_string(),
            arguments: arguments.clone(),
            reply,
        };
        match executor.send(job).await {
            Ok(()) => {
                return answer.await.unwrap_or_else(|_| {
                    tool_result(
                        format!("'{name}' did not run: the turn ended before it could"),
                        true,
                    )
                });
            }
            Err(_) => {
                tracing::debug!(
                    agent = %agent.runtime_agent_id,
                    tool = name,
                    "[hive::mcp] the turn's executor is gone; serving the call on the server"
                );
            }
        }
    }
    agent.serve_call(name, arguments, turn).await
}

impl McpAgent {
    /// Decides `name` under this agent's [`ApprovalPolicy`] and, when allowed,
    /// runs it under `turn`'s context — returning the MCP tool result either
    /// way. Called on the turn's own task when one is running (so the policy's
    /// parks and the tool's own claims file where that turn reads them), else
    /// on the server's.
    pub async fn serve_call(&self, name: &str, arguments: Value, turn: Option<InFlight>) -> Value {
        let Some(tool) = self.tool(name) else {
            return tool_result(format!("refused: unknown tool '{name}'"), true);
        };
        if let Some(policy) = &self.policy {
            let channel = turn
                .as_ref()
                .map_or_else(|| "mcp".to_string(), |turn| turn.surface.id.clone());
            let request = ToolPolicyRequest::new(
                name,
                arguments.clone(),
                ToolCallContext::session(
                    self.runtime_agent_id.clone(),
                    channel,
                    self.agent_id.clone(),
                    uuid::Uuid::new_v4().simple().to_string(),
                    0,
                ),
            );
            match policy.check(&request).await {
                ToolPolicyDecision::Allow => {}
                ToolPolicyDecision::Deny { reason } => {
                    return tool_result(format!("refused: {reason}"), true);
                }
                ToolPolicyDecision::RequireApproval { reason } => {
                    return tool_result(
                        format!(
                            "awaiting approval: {reason}. The request has been parked for the \
                             operator; stop and wait for the decision."
                        ),
                        true,
                    );
                }
            }
        }
        let context = InFlightContext::new(turn, self.workspace.clone());
        let result = tool.execute(arguments, &context).await;
        let is_error = result.is_error;
        tool_result(result.output(), is_error)
    }
}

async fn call_speech(host: &McpHost, agent: &McpAgent, name: &str, arguments: &Value) -> Value {
    let Some(speech) = host
        .in_flight
        .with(&agent.runtime_agent_id, |turn| turn.speak(name, arguments))
    else {
        return tool_result(
            "refused: no turn is in flight for this agent, so nothing can be said",
            true,
        );
    };
    match speech {
        Speech::Recorded(receipt) => {
            // Phase 4's `resolve_dm` outranks the membership snapshot when it
            // is installed: a `dm` it refuses is unrecorded again.
            if name == "dm"
                && let Some(reason) = resolve_dm_via_host(host, agent, arguments)
            {
                host.in_flight
                    .with(&agent.runtime_agent_id, |turn| turn.outbox.clear());
                return tool_result(format!("refused: {reason}"), true);
            }
            tool_result(receipt, false)
        }
        Speech::Refused(text) => tool_result(text, true),
        Speech::Read { limit } => read(host, agent, limit).await,
    }
}

fn resolve_dm_via_host(host: &McpHost, agent: &McpAgent, arguments: &Value) -> Option<String> {
    let resolver = host
        .dm_resolver
        .read()
        .expect("dm resolver poisoned")
        .clone()?;
    let desk_id = host
        .in_flight
        .with(&agent.runtime_agent_id, |turn| {
            turn.hive.as_ref().map(|hive| hive.desk_id.clone())
        })
        .flatten()?;
    let to: Vec<String> = arguments
        .get("to")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|id| id.trim().trim_start_matches('@').to_string())
                .collect()
        })
        .unwrap_or_default();
    resolver
        .resolve_dm(&agent.company, &desk_id, &agent.agent_id, &to)
        .err()
}

/// Serves `read`: the conversation's recent rows, oldest first, narrowed to
/// what this agent may see.
async fn read(host: &McpHost, agent: &McpAgent, limit: usize) -> Value {
    let Some(events) = agent.events.clone() else {
        return tool_result("refused: this agent has no journal to read from", true);
    };
    let Some(surface) = host
        .in_flight
        .with(&agent.runtime_agent_id, |turn| turn.surface.clone())
    else {
        return tool_result("refused: no turn is in flight for this agent", true);
    };
    match read_conversation(events, &agent.company, &agent.agent_id, &surface, limit).await {
        Ok(body) => tool_result(body, false),
        Err(text) => tool_result(text, true),
    }
}
