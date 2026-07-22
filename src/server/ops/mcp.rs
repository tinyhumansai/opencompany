//! Company-scoped MCP server installation, lifecycle, and tool calls.

use std::collections::HashMap;

use axum::extract::Path;
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use openhuman_core::openhuman::mcp_registry::types::{
    CommandKind, ConnStatus, InstalledServer, McpTool, Transport,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::AppState;
use crate::error::OpenCompanyError;
use crate::harness::mcp::McpRuntime;
use crate::ports::now_millis;
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// Builds the MCP route fragment under both company scope forms.
pub fn router() -> Router<AppState> {
    scoped("/mcp/servers", get(list_servers).post(install_server))
        .merge(scoped(
            "/mcp/servers/{server_id}/connect",
            post(connect_server),
        ))
        .merge(scoped(
            "/mcp/servers/{server_id}/disconnect",
            post(disconnect_server),
        ))
        .merge(scoped("/mcp/servers/{server_id}", delete(uninstall_server)))
        .merge(scoped("/mcp/servers/{server_id}/tools", get(list_tools)))
        .merge(scoped(
            "/mcp/servers/{server_id}/tools/{tool}/call",
            post(call_tool),
        ))
}

#[derive(Debug, Serialize)]
struct ServersResponse {
    servers: Vec<ServerResponse>,
}

#[derive(Debug, Serialize)]
struct ServerResponse {
    server_id: String,
    name: String,
    transport: &'static str,
    command: String,
    args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    env_keys: Vec<String>,
    enabled: bool,
    status: String,
    tool_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
}

impl ServerResponse {
    fn from_install(server: InstalledServer, status: Option<&ConnStatus>) -> Self {
        let (transport, url) = match &server.transport {
            Transport::Stdio => ("stdio", None),
            Transport::HttpRemote { url } => ("http", Some(url.clone())),
        };
        Self {
            server_id: server.server_id,
            name: server.display_name,
            transport,
            command: server.command,
            args: server.args,
            url,
            env_keys: server.env_keys,
            enabled: server.enabled,
            status: status
                .map(|entry| entry.status.as_str())
                .unwrap_or("disconnected")
                .to_string(),
            tool_count: status.map_or(0, |entry| entry.tool_count),
            last_error: status.and_then(|entry| entry.last_error.clone()),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum InstallTransport {
    Stdio,
    Http,
}

#[derive(Debug, Deserialize)]
struct InstallRequest {
    name: String,
    transport: InstallTransport,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default)]
    url: Option<String>,
}

#[derive(Debug, Serialize)]
struct InstallResponse {
    server_id: String,
}

#[derive(Debug, Serialize)]
struct ToolsResponse {
    tools: Vec<McpTool>,
}

#[derive(Debug, Serialize)]
struct DisconnectResponse {
    disconnected: bool,
}

#[derive(Debug, Deserialize)]
struct ServerPath {
    server_id: String,
}

#[derive(Debug, Deserialize)]
struct ToolPath {
    server_id: String,
    tool: String,
}

#[derive(Debug, Deserialize)]
struct CallRequest {
    #[serde(default = "empty_object")]
    arguments: Value,
}

#[derive(Debug, Serialize)]
struct CallResponse {
    result: Value,
}

fn empty_object() -> Value {
    serde_json::json!({})
}

fn runtime(company: &ScopedCompany) -> Result<&McpRuntime, ApiError> {
    company.runtime.mcp().map(AsRef::as_ref).ok_or_else(|| {
        ApiError(OpenCompanyError::Unimplemented(
            "MCP is not enabled on this host",
        ))
    })
}

async fn list_servers(company: ScopedCompany) -> Result<Json<ServersResponse>, ApiError> {
    let mcp = runtime(&company)?;
    let statuses = mcp.status().await;
    let servers = mcp
        .list()?
        .into_iter()
        .map(|server| {
            let status = statuses
                .iter()
                .find(|entry| entry.server_id == server.server_id);
            ServerResponse::from_install(server, status)
        })
        .collect();
    Ok(Json(ServersResponse { servers }))
}

async fn install_server(
    company: ScopedCompany,
    Json(request): Json<InstallRequest>,
) -> Result<(StatusCode, Json<InstallResponse>), ApiError> {
    let mcp = runtime(&company)?;
    let name = request.name.trim();
    if name.is_empty() {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "MCP server name is required".to_string(),
        )));
    }

    let (transport, command, args) = match request.transport {
        InstallTransport::Stdio => {
            let command = request
                .command
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    ApiError(OpenCompanyError::InvalidRequest(
                        "stdio MCP servers require a command".to_string(),
                    ))
                })?;
            (Transport::Stdio, command.to_string(), request.args)
        }
        InstallTransport::Http => {
            let url = request
                .url
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    ApiError(OpenCompanyError::InvalidRequest(
                        "HTTP MCP servers require a URL".to_string(),
                    ))
                })?;
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err(ApiError(OpenCompanyError::InvalidRequest(
                    "HTTP MCP server URL must use http:// or https://".to_string(),
                )));
            }
            (
                Transport::HttpRemote {
                    url: url.to_string(),
                },
                String::new(),
                Vec::new(),
            )
        }
    };

    let server_id = uuid::Uuid::new_v4().to_string();
    let mut env_keys: Vec<String> = request.env.keys().cloned().collect();
    env_keys.sort();
    let server = InstalledServer {
        server_id: server_id.clone(),
        qualified_name: format!("manual/{server_id}"),
        display_name: name.to_string(),
        description: None,
        icon_url: None,
        command_kind: CommandKind::Binary,
        command,
        args,
        env_keys,
        config: None,
        installed_at: now_millis() as i64,
        last_connected_at: None,
        transport,
        enabled: true,
    };
    mcp.install(&server, &request.env)?;

    // Installation is durable even when the process or endpoint is currently
    // unavailable. The failure is recorded by OpenHuman and appears in status.
    let _ = mcp.connect(&server_id).await;

    Ok((StatusCode::CREATED, Json(InstallResponse { server_id })))
}

async fn connect_server(
    company: ScopedCompany,
    Path(ServerPath { server_id }): Path<ServerPath>,
) -> Result<Json<ToolsResponse>, ApiError> {
    let tools = runtime(&company)?.connect(&server_id).await?;
    Ok(Json(ToolsResponse { tools }))
}

async fn disconnect_server(
    company: ScopedCompany,
    Path(ServerPath { server_id }): Path<ServerPath>,
) -> Result<Json<DisconnectResponse>, ApiError> {
    let disconnected = runtime(&company)?.disconnect(&server_id).await?;
    Ok(Json(DisconnectResponse { disconnected }))
}

async fn uninstall_server(
    company: ScopedCompany,
    Path(ServerPath { server_id }): Path<ServerPath>,
) -> Result<StatusCode, ApiError> {
    runtime(&company)?.uninstall(&server_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_tools(
    company: ScopedCompany,
    Path(ServerPath { server_id }): Path<ServerPath>,
) -> Result<Json<ToolsResponse>, ApiError> {
    let tools = runtime(&company)?.tools(&server_id).await?;
    Ok(Json(ToolsResponse { tools }))
}

async fn call_tool(
    company: ScopedCompany,
    Path(ToolPath { server_id, tool }): Path<ToolPath>,
    Json(request): Json<CallRequest>,
) -> Result<Json<CallResponse>, ApiError> {
    let result = runtime(&company)?
        .call_tool(&server_id, &tool, request.arguments)
        .await?;
    Ok(Json(CallResponse { result }))
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use serde_json::json;
    use tower::ServiceExt;

    use super::*;
    use crate::company::CompanyManifest;
    use crate::ports::types::CompanyId;
    use crate::runtime::RuntimeBuilder;
    use crate::server;
    use crate::{AppConfig, AppState};

    const NODE_STUB: &str = r#"
const readline = require('node:readline');
const rl = readline.createInterface({ input: process.stdin });
const send = (value) => process.stdout.write(JSON.stringify(value) + '\n');
rl.on('line', (line) => {
  const r = JSON.parse(line);
  if (!r.id) return;
  if (r.method === 'initialize') send({jsonrpc:'2.0',id:r.id,result:{protocolVersion:'2024-11-05',capabilities:{tools:{}},serverInfo:{name:'api-test',version:'1'}}});
  else if (r.method === 'tools/list') send({jsonrpc:'2.0',id:r.id,result:{tools:[{name:'echo',description:'Echo text',inputSchema:{type:'object'}},{name:'add',description:'Add values',inputSchema:{type:'object'}}]}});
  else if (r.method === 'tools/call') send({jsonrpc:'2.0',id:r.id,result:{content:[{type:'text',text:r.params.name === 'echo' ? 'echo: ' + r.params.arguments.text : String(r.params.arguments.a + r.params.arguments.b)}]}});
});
"#;

    fn manifest() -> CompanyManifest {
        toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n").unwrap()
    }

    async fn test_state(home: &std::path::Path) -> AppState {
        let runtime = RuntimeBuilder::new(home, manifest())
            .with_id(CompanyId::new("acme"))
            .build()
            .await
            .expect("runtime");
        let state = AppState::new(AppConfig::default());
        state
            .registry()
            .insert(CompanyId::new("acme"), std::sync::Arc::new(runtime));
        server::test_support::seed_fixed_admin(&state, "acme").await;
        state
    }

    async fn send(
        state: &AppState,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", server::test_support::fixed_cookie("acme"));
        if body.is_some() {
            request = request.header("content-type", "application/json");
        }
        let response = server::router(state.clone())
            .oneshot(
                request
                    .body(Body::from(body.map_or_else(String::new, |v| v.to_string())))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, value)
    }

    #[tokio::test]
    async fn lifecycle_and_tool_call_round_trip_without_leaking_env_values() {
        if Command::new("node").arg("--version").output().is_err() {
            eprintln!("skipping MCP API test because node is unavailable");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("mcp-api-stub.cjs");
        std::fs::write(&script, NODE_STUB).unwrap();
        let state = test_state(temp.path()).await;

        let (status, installed) = send(
            &state,
            "POST",
            "/api/v1/company/mcp/servers",
            Some(json!({
                "name": "simple",
                "transport": "stdio",
                "command": "node",
                "args": [script],
                "env": {"API_TOKEN": "never-return-this"}
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{installed}");
        let server_id = installed["server_id"].as_str().unwrap();

        let (status, listed) = send(&state, "GET", "/api/v1/company/mcp/servers", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(listed["servers"][0]["status"], "connected");
        assert_eq!(listed["servers"][0]["tool_count"], 2);
        assert_eq!(listed["servers"][0]["env_keys"][0], "API_TOKEN");
        assert!(!listed.to_string().contains("never-return-this"));

        let (status, called) = send(
            &state,
            "POST",
            &format!("/api/v1/company/mcp/servers/{server_id}/tools/echo/call"),
            Some(json!({"arguments": {"text": "api"}})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{called}");
        assert_eq!(called["result"]["content"][0]["text"], "echo: api");

        let (status, disconnected) = send(
            &state,
            "POST",
            &format!("/api/v1/company/mcp/servers/{server_id}/disconnect"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(disconnected["disconnected"], true);

        let (status, _) = send(
            &state,
            "DELETE",
            &format!("/api/v1/company/mcp/servers/{server_id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn rejects_server_id_from_another_registry_store() {
        let temp = tempfile::tempdir().unwrap();
        let state = test_state(&temp.path().join("company-a")).await;
        let foreign = McpRuntime::new(temp.path().join("company-b/mcp"));
        let server = InstalledServer {
            server_id: uuid::Uuid::new_v4().to_string(),
            qualified_name: "foreign".to_string(),
            display_name: "Foreign".to_string(),
            description: None,
            icon_url: None,
            command_kind: CommandKind::Binary,
            command: "node".to_string(),
            args: vec![],
            env_keys: vec![],
            config: None,
            installed_at: 0,
            last_connected_at: None,
            transport: Transport::Stdio,
            enabled: true,
        };
        foreign.install(&server, &HashMap::new()).unwrap();

        let (status, body) = send(
            &state,
            "POST",
            &format!(
                "/api/v1/company/mcp/servers/{}/disconnect",
                server.server_id
            ),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["code"], "mcp_server_not_found");
    }
}
