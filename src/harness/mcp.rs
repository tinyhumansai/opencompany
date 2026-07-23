//! Per-agent MCP registry assembly + a credential-redacting list-servers tool
//! (issue #50), and company-scoped MCP server lifecycle management.
//!
//! ## Agent bridge
//!
//! [`registry_for_agent`] folds a company's effective [`McpServerDecl`]s into an
//! OpenHuman [`McpServerRegistry`](oh::mcp_client::McpServerRegistry) scoped to
//! one agent's `mcp:*` tool grants. The registry reuses upstream's HTTP
//! transport and its input-validation safety filter (`apply_safety_filter`),
//! so remote tool metadata is scanned for prompt-injection before an agent ever
//! sees it.
//!
//! **Security**: upstream's [`McpListServersTool`](oh::tools::McpListServersTool)
//! serializes `server.auth` — including bearer tokens — into agent-visible
//! output. [`OcMcpListServersTool`] is a drop-in replacement that emits the same
//! shape **minus** any credential (only a non-secret `auth_configured` bool).
//!
//! ## Company-scoped lifecycle
//!
//! [`McpRuntime`] owns a company-home-scoped OpenHuman config and provides
//! durable install/connect/disconnect/uninstall operations over the live
//! registry, plus tool discovery and tool calls.
//!
//! Compiled only under `feature = "openhuman"` (the whole `harness` module is).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use openhuman_core::openhuman as oh;

use oh::config::{Config, McpAuthConfig, McpServerConfig};
use oh::mcp_client::{McpRegistrySource, McpServerRegistry};
use oh::mcp_registry::types::{ConnStatus, InstalledServer, McpTool};
use oh::tools::traits::{PermissionLevel, Tool, ToolResult};

use crate::company::Agent as ManifestAgent;
use crate::company::mcp::{AuthMaterial, McpServerDecl};
use crate::error::OpenCompanyError;
use crate::runtime::tools::grant_matches;

// ---------------------------------------------------------------------------
// Agent bridge: per-agent registry + credential-redacting tools
// ---------------------------------------------------------------------------

/// Builds a registry from a set of decls, keeping only the enabled ones.
///
/// Sets `gitbooks.enabled = false` — **critical**: OpenHuman's `Config::default`
/// seeds a `gitbooks` MCP server, which would otherwise leak into every tenant
/// agent's server list. `command` is always empty, so the registry always
/// selects the HTTP transport (hosted-v1 boundary). Returns an empty registry
/// when nothing survives.
pub fn registry_from_decls(decls: &[McpServerDecl]) -> McpServerRegistry {
    let mut config = Config::default();
    // Do NOT inherit upstream's default gitbooks server.
    config.gitbooks.enabled = false;
    config.mcp_client.enabled = true;
    config.mcp_client.servers = decls
        .iter()
        .filter(|decl| decl.enabled)
        .map(server_config)
        .collect();
    McpServerRegistry::from_config(&config)
}

/// The MCP registry scoped to one agent, or `None` when the agent is granted no
/// (enabled) MCP servers.
///
/// An agent reaches a server named `<slug>` only when its manifest `tools`
/// grants match `mcp:<slug>` (a bare `mcp:*` grants all). Disabled servers are
/// excluded. Returns `None` (not an empty registry) so the caller can skip
/// pushing the MCP bridge tools entirely for an agent with no MCP surface.
pub fn registry_for_agent(
    decls: &[McpServerDecl],
    agent: &ManifestAgent,
) -> Option<Arc<McpServerRegistry>> {
    let granted: Vec<McpServerDecl> = decls
        .iter()
        .filter(|decl| decl.enabled && agent_grants_server(agent, &decl.name))
        .cloned()
        .collect();
    if granted.is_empty() {
        return None;
    }
    let registry = registry_from_decls(&granted);
    if registry.is_empty() {
        None
    } else {
        Some(Arc::new(registry))
    }
}

/// Whether `agent`'s tool grants reach the MCP server named `name`, using the
/// same glob semantics as every other tool grant (`mcp:*` = all, `mcp:notion` =
/// exact).
fn agent_grants_server(agent: &ManifestAgent, name: &str) -> bool {
    let want = format!("mcp:{name}");
    agent.tools.iter().any(|grant| grant_matches(grant, &want))
}

/// Projects a [`McpServerDecl`] onto an OpenHuman [`McpServerConfig`], mapping
/// the resolved [`AuthMaterial`] onto the transport's auth config. `command`
/// stays empty so the registry always builds the HTTP transport.
fn server_config(decl: &McpServerDecl) -> McpServerConfig {
    McpServerConfig {
        name: decl.name.clone(),
        endpoint: decl.endpoint.clone(),
        description: decl.description.clone(),
        enabled: true,
        allowed_tools: decl.allowed_tools.clone(),
        disallowed_tools: decl.disallowed_tools.clone(),
        timeout_secs: decl.timeout_secs,
        auth: auth_config(&decl.auth),
        ..McpServerConfig::default()
    }
}

/// Maps resolved [`AuthMaterial`] onto the transport's [`McpAuthConfig`].
fn auth_config(material: &AuthMaterial) -> McpAuthConfig {
    match material {
        AuthMaterial::None => McpAuthConfig::None,
        AuthMaterial::Bearer(token) => McpAuthConfig::BearerToken {
            token: token.clone(),
        },
        AuthMaterial::Header { name, value } => McpAuthConfig::Header {
            name: name.clone(),
            value: value.clone(),
        },
    }
}

/// One remote tool advertised by an MCP server, projected for the console's
/// live-discovery view. Sanitized: the `title`/`description` are read through
/// OpenHuman's `display_*` accessors (control-char strip + injection fence +
/// length cap), never the raw remote fields.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolInfo {
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub input_schema: Value,
}

/// Live-discovers the tools a single server exposes, through a one-server
/// registry built from `decls`. Inherits the registry's per-server allow-list
/// and the input-validation safety filter. `server` names the decl to query.
pub async fn discover_tools(
    decls: &[McpServerDecl],
    server: &str,
) -> anyhow::Result<Vec<McpToolInfo>> {
    let registry = registry_from_decls(decls);
    let tools = registry.list_tools(server).await?;
    Ok(tools
        .iter()
        .map(|tool| McpToolInfo {
            name: tool.name.clone(),
            title: tool.display_title(),
            description: tool.display_description(),
            input_schema: tool.input_schema.clone(),
        })
        .collect())
}

/// A credential-redacting replacement for OpenHuman's `mcp_list_servers` tool.
///
/// Emits the same agent-facing shape (name / endpoint / description / timeout /
/// tool lists / source) but **never** the `auth` block — only a non-secret
/// `auth_configured` flag. Keeps the upstream tool name so agent prompts and the
/// bridge contract are unchanged.
pub struct OcMcpListServersTool {
    registry: Arc<McpServerRegistry>,
}

impl OcMcpListServersTool {
    pub fn new(registry: Arc<McpServerRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for OcMcpListServersTool {
    fn name(&self) -> &str {
        "mcp_list_servers"
    }

    fn description(&self) -> &str {
        "List named remote MCP servers available to you. Use this before browsing tools on a specific MCP server."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    fn supports_markdown(&self) -> bool {
        true
    }

    async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
        let servers = self
            .registry
            .list()
            .into_iter()
            .map(|server| {
                json!({
                    "name": server.name,
                    "endpoint": server.endpoint,
                    "description": server.description,
                    "timeout_secs": server.timeout_secs,
                    "allowed_tools": server.allowed_tools,
                    "disallowed_tools": server.disallowed_tools,
                    // Non-secret status ONLY — the credential is never emitted.
                    "auth_configured": !matches!(server.auth, McpAuthConfig::None),
                })
            })
            .collect::<Vec<_>>();

        let markdown = if servers.is_empty() {
            "# MCP Servers\n\nNo remote MCP servers are available.".to_string()
        } else {
            let mut md = String::from("# MCP Servers\n");
            for server in self.registry.list() {
                let source = match server.source {
                    McpRegistrySource::Config => "config",
                    McpRegistrySource::LegacyGitbooks => "legacy_gitbooks",
                };
                let auth = if matches!(server.auth, McpAuthConfig::None) {
                    "none"
                } else {
                    "configured"
                };
                md.push_str(&format!(
                    "\n- **{}** ({source})\n  - endpoint: `{}`\n  - auth: {auth}",
                    server.name, server.endpoint,
                ));
                if let Some(description) = server.description.as_deref() {
                    md.push_str(&format!("\n  - {description}"));
                }
                if !server.allowed_tools.is_empty() {
                    md.push_str(&format!(
                        "\n  - allowed tools: `{}`",
                        server.allowed_tools.join("`, `")
                    ));
                }
                if !server.disallowed_tools.is_empty() {
                    md.push_str(&format!(
                        "\n  - disallowed tools: `{}`",
                        server.disallowed_tools.join("`, `")
                    ));
                }
            }
            md
        };

        Ok(ToolResult::success_with_markdown(
            json!({ "servers": servers }),
            markdown,
        ))
    }
}

// ---------------------------------------------------------------------------
// Company-scoped MCP lifecycle (McpRuntime)
// ---------------------------------------------------------------------------

/// Company-home-scoped persistence and access to OpenHuman's live MCP registry.
pub struct McpRuntime {
    config: oh::config::Config,
}

impl McpRuntime {
    /// Creates a runtime whose MCP SQLite store lives beneath `workspace_dir`.
    pub fn new(workspace_dir: PathBuf) -> Self {
        let config = oh::config::Config {
            workspace_dir,
            ..Default::default()
        };
        Self { config }
    }

    /// Reconnects enabled installed servers. Failures are logged by OpenHuman
    /// per server and never prevent the company runtime from booting.
    pub async fn boot(&self) {
        oh::mcp_registry::boot::spawn_installed_servers(&self.config).await;
    }

    /// Returns every persisted install without loading secret environment values.
    pub fn list(&self) -> crate::Result<Vec<InstalledServer>> {
        oh::mcp_registry::store::list_servers(&self.config).map_err(store_error)
    }

    /// Persists an install and its write-only environment values.
    pub fn install(
        &self,
        server: &InstalledServer,
        env: &HashMap<String, String>,
    ) -> crate::Result<()> {
        oh::mcp_registry::store::insert_server(&self.config, server).map_err(store_error)?;
        if let Err(error) =
            oh::mcp_registry::store::set_env_values(&self.config, &server.server_id, env)
        {
            let _ = oh::mcp_registry::store::delete_server(&self.config, &server.server_id);
            return Err(store_error(error));
        }
        Ok(())
    }

    /// Loads an installed server, establishing the company-store membership
    /// check before touching OpenHuman's process-global connection registry.
    pub fn get(&self, server_id: &str) -> crate::Result<InstalledServer> {
        oh::mcp_registry::store::get_server(&self.config, server_id)
            .map_err(|_| OpenCompanyError::McpServerNotFound(server_id.to_string()))
    }

    /// Connects an installed server and returns its advertised tools.
    pub async fn connect(&self, server_id: &str) -> crate::Result<Vec<McpTool>> {
        let server = self.get(server_id)?;
        oh::mcp_registry::connections::connect(&self.config, &server)
            .await
            .map_err(harness_error)
    }

    /// Disconnects an installed server after verifying it belongs to this store.
    pub async fn disconnect(&self, server_id: &str) -> crate::Result<bool> {
        self.get(server_id)?;
        Ok(oh::mcp_registry::connections::disconnect(server_id).await)
    }

    /// Disconnects and deletes an installed server and its environment values.
    pub async fn uninstall(&self, server_id: &str) -> crate::Result<bool> {
        self.get(server_id)?;
        oh::mcp_registry::connections::disconnect(server_id).await;
        oh::mcp_registry::store::delete_server(&self.config, server_id).map_err(store_error)
    }

    /// Returns connection state joined by OpenHuman against this runtime's store.
    pub async fn status(&self) -> Vec<ConnStatus> {
        oh::mcp_registry::connections::all_status(&self.config).await
    }

    /// Returns the cached tool list for a connected installed server.
    pub async fn tools(&self, server_id: &str) -> crate::Result<Vec<McpTool>> {
        self.get(server_id)?;
        oh::mcp_registry::connections::tools_for(server_id)
            .await
            .ok_or_else(|| {
                OpenCompanyError::InvalidRequest(format!(
                    "MCP server '{server_id}' is not connected"
                ))
            })
    }

    /// Calls one tool after verifying the server belongs to this runtime's store.
    pub async fn call_tool(
        &self,
        server_id: &str,
        tool_name: &str,
        arguments: Value,
    ) -> crate::Result<Value> {
        self.get(server_id)?;
        oh::mcp_registry::connections::call_tool(server_id, tool_name, arguments)
            .await
            .map_err(harness_error)
    }
}

fn store_error(error: anyhow::Error) -> OpenCompanyError {
    OpenCompanyError::Store(format!("MCP registry: {error}"))
}

fn harness_error(error: impl std::fmt::Display) -> OpenCompanyError {
    OpenCompanyError::Harness(format!("MCP registry: {error}"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Agent-bridge tests (HEAD) --

    fn decl(name: &str, endpoint: &str) -> McpServerDecl {
        McpServerDecl {
            name: name.to_string(),
            endpoint: endpoint.to_string(),
            description: None,
            allowed_tools: Vec::new(),
            disallowed_tools: Vec::new(),
            timeout_secs: 30,
            enabled: true,
            source: crate::company::mcp::McpSource::Runtime,
            auth: AuthMaterial::None,
        }
    }

    fn agent(grants: &[&str]) -> ManifestAgent {
        ManifestAgent {
            id: "ceo".into(),
            role: "Chief".into(),
            description: None,
            tier: None,
            tools: grants.iter().map(|g| g.to_string()).collect(),
            budget_usd_daily: None,
        }
    }

    #[test]
    fn empty_decls_yield_no_registry() {
        assert!(registry_for_agent(&[], &agent(&["mcp:*"])).is_none());
    }

    #[test]
    fn ungranted_agent_gets_no_registry() {
        let decls = vec![decl("notion", "https://notion.example/mcp")];
        // No mcp grant at all.
        assert!(registry_for_agent(&decls, &agent(&["email.send"])).is_none());
    }

    #[test]
    fn wildcard_grant_admits_all_enabled_servers() {
        let decls = vec![
            decl("notion", "https://notion.example/mcp"),
            decl("linear", "https://linear.example/mcp"),
        ];
        let reg = registry_for_agent(&decls, &agent(&["mcp:*"])).expect("registry");
        let mut names: Vec<&str> = reg.list().iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["linear", "notion"]);
    }

    #[test]
    fn named_grant_scopes_to_that_server() {
        let decls = vec![
            decl("notion", "https://notion.example/mcp"),
            decl("linear", "https://linear.example/mcp"),
        ];
        let reg = registry_for_agent(&decls, &agent(&["mcp:notion"])).expect("registry");
        let names: Vec<&str> = reg.list().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["notion"]);
    }

    #[test]
    fn disabled_server_is_excluded() {
        let mut d = decl("notion", "https://notion.example/mcp");
        d.enabled = false;
        assert!(registry_for_agent(&[d], &agent(&["mcp:*"])).is_none());
    }

    #[test]
    fn gitbooks_default_server_never_leaks_in() {
        // OpenHuman's Config::default seeds a `gitbooks` server; the registry we
        // build for a tenant agent must NOT contain it.
        let decls = vec![decl("notion", "https://notion.example/mcp")];
        let reg = registry_for_agent(&decls, &agent(&["mcp:*"])).expect("registry");
        assert!(reg.get("gitbooks").is_none(), "gitbooks must not leak in");
    }

    #[test]
    fn auth_material_maps_onto_transport_config() {
        let bearer = auth_config(&AuthMaterial::Bearer("tok".into()));
        assert!(matches!(bearer, McpAuthConfig::BearerToken { .. }));
        let header = auth_config(&AuthMaterial::Header {
            name: "X-Key".into(),
            value: "v".into(),
        });
        assert!(matches!(header, McpAuthConfig::Header { .. }));
        assert!(matches!(
            auth_config(&AuthMaterial::None),
            McpAuthConfig::None
        ));
    }

    #[tokio::test]
    async fn list_servers_tool_never_emits_a_credential() {
        let mut d = decl("notion", "https://notion.example/mcp");
        d.auth = AuthMaterial::Bearer("sk-super-secret-token".into());
        let reg = registry_for_agent(&[d], &agent(&["mcp:*"])).expect("registry");
        let tool = OcMcpListServersTool::new(reg);
        let result = tool.execute(json!({})).await.expect("execute");

        // The whole serialized result (JSON + markdown) must not carry the token.
        let json_out = serde_json::to_string(&result).unwrap();
        assert!(
            !json_out.contains("sk-super-secret-token"),
            "list-servers output leaked a credential: {json_out}"
        );
        // But it still reports the server + that auth is configured.
        assert!(json_out.contains("notion"));
        assert!(json_out.contains("auth_configured"));
    }

    /// End-to-end: drive `mcp_call_tool` against an in-process axum MCP server
    /// (plain JSON `initialize` / `tools/list` / `tools/call`, no new deps). The
    /// bearer token reaches the *server* over the wire (auth is wired), but the
    /// agent-visible `ToolResult` never carries it. This is the regression guard
    /// for the "credentials never surface to the agent" invariant.
    #[tokio::test]
    async fn call_tool_through_agent_path_never_leaks_bearer() {
        use std::sync::Mutex as StdMutex;

        use axum::extract::State;
        use axum::http::HeaderMap;
        use axum::routing::post;
        use axum::{Json, Router};
        use oh::security::SecurityPolicy;
        use oh::tools::McpCallTool;

        #[derive(Default)]
        struct Seen {
            auth: StdMutex<Option<String>>,
        }

        async fn handler(
            State(seen): State<Arc<Seen>>,
            headers: HeaderMap,
            Json(body): Json<Value>,
        ) -> Json<Value> {
            if let Some(auth) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
                *seen.auth.lock().unwrap() = Some(auth.to_string());
            }
            let id = body.get("id").cloned().unwrap_or(Value::Null);
            let method = body.get("method").and_then(Value::as_str).unwrap_or("");
            let result = match method {
                "initialize" => json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "serverInfo": { "name": "fixture", "version": "0" }
                }),
                "tools/list" => json!({
                    "tools": [{
                        "name": "echo",
                        "description": "Echoes input.",
                        "inputSchema": { "type": "object" }
                    }]
                }),
                "tools/call" => json!({
                    "content": [{ "type": "text", "text": "remote ran ok, no secrets here" }],
                    "isError": false
                }),
                // A notification (e.g. notifications/initialized) — ack only.
                _ => return Json(json!({ "jsonrpc": "2.0" })),
            };
            Json(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        }

        let seen = Arc::new(Seen::default());
        let app = Router::new()
            .route("/mcp", post(handler))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let endpoint = format!("http://{addr}/mcp");
        let mut d = decl("fixture", &endpoint);
        d.auth = AuthMaterial::Bearer("sk-super-secret-xyz".into());
        let registry = registry_for_agent(&[d], &agent(&["mcp:*"])).expect("registry");
        let tool = McpCallTool::new(registry, Arc::new(SecurityPolicy::default()));

        let result = tool
            .execute(json!({ "server": "fixture", "tool": "echo", "arguments": {} }))
            .await
            .expect("mcp_call_tool");

        // Auth WAS wired: the server received the bearer over the wire.
        assert_eq!(
            seen.auth.lock().unwrap().as_deref(),
            Some("Bearer sk-super-secret-xyz"),
            "the transport must send the configured bearer"
        );
        // But the agent-visible result never carries the token.
        let out = serde_json::to_string(&result).unwrap();
        assert!(
            !out.contains("sk-super-secret-xyz"),
            "mcp_call_tool result leaked a credential: {out}"
        );
        assert!(result.output().contains("remote ran ok"));
    }

    // -- McpRuntime tests (origin/main) --

    use std::process::Command;

    use oh::mcp_registry::types::{CommandKind, Transport};

    const NODE_STUB: &str = r#"
const readline = require('node:readline');
const rl = readline.createInterface({ input: process.stdin });
const send = (value) => process.stdout.write(JSON.stringify(value) + '\n');
rl.on('line', (line) => {
  const request = JSON.parse(line);
  if (!request.id) return;
  if (request.method === 'initialize') {
    send({ jsonrpc: '2.0', id: request.id, result: { protocolVersion: '2024-11-05', capabilities: { tools: {} }, serverInfo: { name: 'test', version: '1' } } });
  } else if (request.method === 'tools/list') {
    send({ jsonrpc: '2.0', id: request.id, result: { tools: [{ name: 'echo', description: 'Echo text', inputSchema: { type: 'object', properties: { text: { type: 'string' } }, required: ['text'] } }] } });
  } else if (request.method === 'tools/call') {
    send({ jsonrpc: '2.0', id: request.id, result: { content: [{ type: 'text', text: 'echo: ' + request.params.arguments.text }] } });
  }
});
"#;

    #[tokio::test]
    async fn install_connect_call_disconnect_round_trip() {
        if Command::new("node").arg("--version").output().is_err() {
            eprintln!("skipping MCP runtime test because node is unavailable");
            return;
        }

        let temp = tempfile::tempdir().expect("tempdir");
        let script = temp.path().join("mcp-stub.cjs");
        std::fs::write(&script, NODE_STUB).expect("write node stub");
        let runtime = McpRuntime::new(temp.path().join("workspace"));
        let server = InstalledServer {
            server_id: uuid::Uuid::new_v4().to_string(),
            qualified_name: "test-node-echo".to_string(),
            display_name: "Test Node Echo".to_string(),
            description: None,
            icon_url: None,
            command_kind: CommandKind::Binary,
            command: "node".to_string(),
            args: vec![script.to_string_lossy().into_owned()],
            env_keys: vec![],
            config: None,
            installed_at: 0,
            last_connected_at: None,
            transport: Transport::Stdio,
            enabled: true,
        };

        runtime.install(&server, &HashMap::new()).expect("install");
        let tools = runtime.connect(&server.server_id).await.expect("connect");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");

        let result = runtime
            .call_tool(
                &server.server_id,
                "echo",
                serde_json::json!({"text": "hello"}),
            )
            .await
            .expect("call");
        assert_eq!(result["content"][0]["text"], "echo: hello");

        assert!(
            runtime
                .disconnect(&server.server_id)
                .await
                .expect("disconnect")
        );
        assert!(
            runtime
                .uninstall(&server.server_id)
                .await
                .expect("uninstall")
        );
        assert!(runtime.list().expect("list").is_empty());
    }
}