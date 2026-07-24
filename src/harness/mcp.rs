//! Per-agent MCP registry assembly + a credential-redacting list-servers tool
//! (issue #50).
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
use oh::security::{SecurityPolicy, ToolOperation};
use oh::tools::traits::{PermissionLevel, Tool, ToolCallOptions, ToolResult};

use crate::company::Agent as ManifestAgent;
use crate::company::mcp::{AuthMaterial, McpServerDecl};
use crate::error::OpenCompanyError;
use crate::harness::mcp_probe::{
    McpFailure, McpFailureQueue, classify_mcp_error, operator_message, scrub, strip_endpoint,
};
use crate::runtime::tools::grant_matches;

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
/// An agent reaches a server named `<slug>` only when its effective `grants`
/// (already narrowed by [`agent_effective_grants`]) match `mcp:<slug>` (a bare
/// `mcp:*` grants all). Disabled servers are excluded. Returns `None` (not an
/// empty registry) so the caller can skip pushing the MCP bridge tools entirely
/// for an agent with no MCP surface.
///
/// [`agent_effective_grants`]: crate::runtime::builder::agent_effective_grants
pub fn registry_for_agent(
    decls: &[McpServerDecl],
    grants: &[String],
) -> Option<Arc<McpServerRegistry>> {
    let granted: Vec<McpServerDecl> = decls
        .iter()
        .filter(|decl| decl.enabled && grants_cover_server(grants, &decl.name))
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

/// Whether an agent's effective `grants` cover the MCP server named `name`,
/// using the same glob semantics as every other tool grant (`mcp:*` = all,
/// `mcp:notion` = exact).
fn grants_cover_server(grants: &[String], name: &str) -> bool {
    let want = format!("mcp:{name}");
    grants.iter().any(|grant| grant_matches(grant, &want))
}

/// Whether `agent`'s tool grants reach the MCP server named `name`, using the
/// same glob semantics as every other tool grant (`mcp:*` = all, `mcp:notion` =
/// exact).
fn agent_grants_server(agent: &ManifestAgent, name: &str) -> bool {
    let want = format!("mcp:{name}");
    agent.tools.iter().any(|grant| grant_matches(grant, &want))
}

/// The credential substrings from the (enabled, grant-matched) servers this
/// agent reaches — the known-secret set fed to
/// [`scrub`](crate::harness::mcp_probe::scrub) so no configured credential can
/// survive into an agent-visible error. Never serialized anywhere.
pub fn granted_secrets(decls: &[McpServerDecl], agent: &ManifestAgent) -> Vec<String> {
    decls
        .iter()
        .filter(|decl| decl.enabled && agent_grants_server(agent, &decl.name))
        .flat_map(|decl| decl.auth.secret_values())
        .collect()
}

/// A persona brief appended when an agent is granted MCP tools: a stale-memory
/// mitigation directing the agent to answer capability questions from a **live**
/// `mcp_list_servers` / `mcp_list_tools` call, never from memory (the effective
/// server set can change between turns — the MCP-freshness path). The root fix
/// for stale answers lives in the Memory cell; this is the mitigation.
pub fn capability_brief() -> String {
    " When you are asked what tools, integrations, or MCP servers you have — or whether you can do something that would use one — ALWAYS call `mcp_list_servers` (and `mcp_list_tools` for a specific server) to check what is available right now. Never answer such questions from memory: your available servers and tools can change between turns.".to_string()
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
        // The upstream HTTP transport already applies this via `request.query()`
        // (`mcp_client/client.rs`), so BrowserBase-style URL auth needs zero
        // vendor changes — just this mapping.
        AuthMaterial::QueryParam { name, value } => McpAuthConfig::QueryParam {
            name: name.clone(),
            value: value.clone(),
        },
        // The whole trick behind console OAuth: an OAuth credential resolves to
        // exactly the bearer path the static registry already knows how to send.
        // The freshness of `access_token` is the caller's responsibility — the
        // harness builder refreshes an expired token before this mapping runs
        // (see `crate::company::mcp_oauth::refresh` + `resolve_effective`).
        AuthMaterial::OAuth { access_token, .. } => McpAuthConfig::BearerToken {
            token: access_token.clone(),
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
                    // Strip the query string: a query-parameter credential rides
                    // in the endpoint URL, so the agent-visible endpoint must
                    // never carry it.
                    "endpoint": strip_endpoint(&server.endpoint),
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
                    server.name,
                    strip_endpoint(&server.endpoint),
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

/// A hardening decorator around upstream's [`McpCallTool`](oh::tools::McpCallTool)
/// that keeps the same tool name + schema but turns a raw transport failure into
/// a **scrubbed, actionable** result and records it on a shared
/// [`McpFailureQueue`] the brain drains after the turn.
///
/// Upstream's tool surfaces `mcp_call_tool failed: {err}` verbatim — which can
/// carry a response body or (with query-parameter auth) the full request URL
/// including the credential. This decorator classifies the error, scrubs it
/// against the granted servers' known credentials, rewrites the agent-facing
/// text into a "don't retry blindly, tell the operator" directive, and pushes an
/// [`McpFailure`] so the operator sees a warning after the turn.
pub struct OcMcpCallTool {
    registry: Arc<McpServerRegistry>,
    security: Arc<SecurityPolicy>,
    /// Known credential substrings from the agent's granted servers, fed to
    /// [`scrub`] so no configured secret can survive into agent-visible output.
    secrets: Vec<String>,
    /// The shared failure queue the brain drains after the turn.
    failures: McpFailureQueue,
}

impl OcMcpCallTool {
    /// Builds the decorator over the agent's registry, the (permissive) MCP
    /// security policy, the granted servers' credential substrings, and the
    /// shared failure queue.
    pub fn new(
        registry: Arc<McpServerRegistry>,
        security: Arc<SecurityPolicy>,
        secrets: Vec<String>,
        failures: McpFailureQueue,
    ) -> Self {
        Self {
            registry,
            security,
            secrets,
            failures,
        }
    }

    /// Whether the named server has a credential configured (drives the
    /// 401-vs-rejected classification without reading the credential).
    fn auth_configured(&self, server: &str) -> bool {
        self.registry
            .get(server)
            .map(|s| !matches!(s.auth, McpAuthConfig::None))
            .unwrap_or(false)
    }

    /// Classify + scrub + record a failed call, returning the agent-facing error
    /// result. The pushed [`McpFailure`] and the returned text are both scrubbed.
    fn handle_failure(&self, server: &str, tool: &str, err: &anyhow::Error) -> ToolResult {
        let class = classify_mcp_error(err, self.auth_configured(server), true);
        let scrubbed = scrub(&operator_message(server, &class, err), &self.secrets);
        self.failures.push(McpFailure {
            server: server.to_string(),
            tool: tool.to_string(),
            status: class.code(),
            hint: class.auth_hint.clone(),
            scrubbed_message: scrubbed.clone(),
        });
        // The agent-facing directive: don't retry blindly, surface to operator.
        let agent_text = scrub(
            &format!(
                "The MCP call to '{server}' (tool '{tool}') did not succeed. {scrubbed} Do not retry blindly — surface this to the operator."
            ),
            &self.secrets,
        );
        ToolResult::error(agent_text)
    }
}

#[async_trait]
impl Tool for OcMcpCallTool {
    fn name(&self) -> &str {
        "mcp_call_tool"
    }

    fn description(&self) -> &str {
        "Call a tool on a named remote MCP server. First inspect available tools with `mcp_list_tools`, then pass the remote tool name and its JSON arguments here."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server": {
                    "type": "string",
                    "description": "Registered MCP server name from `mcp_list_servers`."
                },
                "tool": {
                    "type": "string",
                    "description": "Remote MCP tool name from `mcp_list_tools`."
                },
                "arguments": {
                    "type": "object",
                    "description": "Arguments object passed through to the remote MCP tool."
                }
            },
            "required": ["server", "tool", "arguments"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Execute
    }

    fn supports_markdown(&self) -> bool {
        true
    }

    async fn execute_with_options(
        &self,
        args: Value,
        options: ToolCallOptions,
    ) -> anyhow::Result<ToolResult> {
        self.security
            .enforce_tool_operation(ToolOperation::Act, self.name())
            .map_err(|err| anyhow::anyhow!(err))?;

        let server = required_string_arg(&args, "server")?;
        let tool = required_string_arg(&args, "tool")?;
        let arguments = args
            .get("arguments")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing required `arguments` object"))?;
        if !arguments.is_object() {
            return Ok(ToolResult::error("`arguments` must be an object"));
        }

        match self.registry.call_tool(&server, &tool, arguments).await {
            Ok(result) => {
                let mut result = result.rendered;
                if options.prefer_markdown && result.markdown_formatted.is_none() {
                    result.markdown_formatted = Some(result.output());
                }
                Ok(result)
            }
            Err(err) => Ok(self.handle_failure(&server, &tool, &err)),
        }
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        self.execute_with_options(args, ToolCallOptions::default())
            .await
    }
}

/// Pulls a required, non-empty string argument (mirrors upstream's private
/// helper of the same name).
fn required_string_arg(args: &Value, key: &str) -> anyhow::Result<String> {
    let value = args
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing required `{key}`"))?;
    Ok(value.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

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

    fn grants(g: &[&str]) -> Vec<String> {
        g.iter().map(|s| s.to_string()).collect()
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
        assert!(registry_for_agent(&[], &grants(&["mcp:*"])).is_none());
    }

    #[test]
    fn ungranted_agent_gets_no_registry() {
        let decls = vec![decl("notion", "https://notion.example/mcp")];
        // No mcp grant at all.
        assert!(registry_for_agent(&decls, &grants(&["email.send"])).is_none());
    }

    #[test]
    fn wildcard_grant_admits_all_enabled_servers() {
        let decls = vec![
            decl("notion", "https://notion.example/mcp"),
            decl("linear", "https://linear.example/mcp"),
        ];
        let reg = registry_for_agent(&decls, &grants(&["mcp:*"])).expect("registry");
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
        let reg = registry_for_agent(&decls, &grants(&["mcp:notion"])).expect("registry");
        let names: Vec<&str> = reg.list().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["notion"]);
    }

    #[test]
    fn disabled_server_is_excluded() {
        let mut d = decl("notion", "https://notion.example/mcp");
        d.enabled = false;
        assert!(registry_for_agent(&[d], &grants(&["mcp:*"])).is_none());
    }

    #[test]
    fn gitbooks_default_server_never_leaks_in() {
        // OpenHuman's Config::default seeds a `gitbooks` server; the registry we
        // build for a tenant agent must NOT contain it.
        let decls = vec![decl("notion", "https://notion.example/mcp")];
        let reg = registry_for_agent(&decls, &grants(&["mcp:*"])).expect("registry");
        assert!(reg.get("gitbooks").is_none(), "gitbooks must not leak in");
    }

    #[test]
    fn auth_material_maps_onto_transport_config() {
        let bearer = auth_config(&AuthMaterial::Bearer("tok".into()));
        assert!(matches!(bearer, McpAuthConfig::BearerToken { .. }));
        // An OAuth credential resolves to the same bearer path, carrying exactly
        // its (already-refreshed) access token and nothing else.
        let oauth = auth_config(&AuthMaterial::OAuth {
            access_token: "at".into(),
            refresh_token: None,
            client_id: "cid".into(),
            client_secret: None,
            token_endpoint: "https://as.example/token".into(),
            expires_at: 0,
        });
        assert!(matches!(oauth, McpAuthConfig::BearerToken { token } if token == "at"));
        let header = auth_config(&AuthMaterial::Header {
            name: "X-Key".into(),
            value: "v".into(),
        });
        assert!(matches!(header, McpAuthConfig::Header { .. }));
        let query = auth_config(&AuthMaterial::QueryParam {
            name: "apiKey".into(),
            value: "qp".into(),
        });
        assert!(matches!(query, McpAuthConfig::QueryParam { .. }));
        assert!(matches!(
            auth_config(&AuthMaterial::None),
            McpAuthConfig::None
        ));
    }

    #[tokio::test]
    async fn list_servers_tool_never_emits_a_credential() {
        let mut d = decl("notion", "https://notion.example/mcp");
        d.auth = AuthMaterial::Bearer("sk-super-secret-token".into());
        let reg = registry_for_agent(&[d], &grants(&["mcp:*"])).expect("registry");
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
        let registry = registry_for_agent(&[d], &grants(&["mcp:*"])).expect("registry");
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

    /// SECURITY CANARY: a server that **reflects the submitted credential** in a
    /// non-401 error body must not leak it anywhere the `OcMcpCallTool` decorator
    /// surfaces — not the agent-visible result, and not the drained failure. This
    /// is the regression guard for leak vector #1 (upstream `MCP HTTP {status} —
    /// {body}` echoing the body) driven through the REAL vendored transport.
    #[tokio::test]
    async fn oc_call_tool_scrubs_reflected_credential() {
        use axum::extract::State;
        use axum::http::HeaderMap;
        use axum::routing::post;
        use axum::{Json, Router};
        use oh::security::SecurityPolicy;

        // On tools/call, reflect the Authorization header back in a 500 body — the
        // exact hostile shape that would leak the token through upstream's
        // `MCP HTTP 500 — {body}` surfacing.
        async fn handler(
            State(()): State<()>,
            headers: HeaderMap,
            Json(body): Json<Value>,
        ) -> axum::response::Response {
            use axum::response::IntoResponse;
            let id = body.get("id").cloned().unwrap_or(Value::Null);
            let method = body.get("method").and_then(Value::as_str).unwrap_or("");
            match method {
                "initialize" => Json(json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": { "protocolVersion": "2025-11-25", "capabilities": {},
                                "serverInfo": { "name": "fixture", "version": "0" } }
                }))
                .into_response(),
                "tools/list" => Json(json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": { "tools": [{ "name": "echo", "description": "e",
                                            "inputSchema": { "type": "object" } }] }
                }))
                .into_response(),
                "tools/call" => {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        format!("boom — received {auth}"),
                    )
                        .into_response()
                }
                _ => Json(json!({ "jsonrpc": "2.0" })).into_response(),
            }
        }

        let app = Router::new().route("/mcp", post(handler)).with_state(());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        const CANARY: &str = "sk-canary-REFLECTED-9999";
        let endpoint = format!("http://{addr}/mcp");
        let mut d = decl("fixture", &endpoint);
        d.auth = AuthMaterial::Bearer(CANARY.into());
        let agent = agent(&["mcp:*"]);
        let secrets = granted_secrets(std::slice::from_ref(&d), &agent);
        let registry = registry_for_agent(&[d], &grants(&["mcp:*"])).expect("registry");

        let queue = McpFailureQueue::default();
        let tool = OcMcpCallTool::new(
            registry,
            Arc::new(SecurityPolicy::default()),
            secrets,
            queue.clone(),
        );

        let result = tool
            .execute(json!({ "server": "fixture", "tool": "echo", "arguments": {} }))
            .await
            .expect("mcp_call_tool");

        // The agent-visible result is an error, but carries NO canary.
        assert!(result.is_error, "a failed call must be an error result");
        let out = serde_json::to_string(&result).unwrap();
        assert!(
            !out.contains(CANARY),
            "OcMcpCallTool result leaked the reflected credential: {out}"
        );

        // The drained failure is recorded, classified, and scrubbed.
        let failures = queue.drain();
        assert_eq!(failures.len(), 1, "the failure was queued");
        assert_eq!(failures[0].server, "fixture");
        assert_eq!(failures[0].status, "server_error");
        let serialized = format!("{:?}", failures[0]);
        assert!(
            !serialized.contains(CANARY),
            "the drained failure leaked the reflected credential: {serialized}"
        );
    }

    /// The query-parameter credential is **appended** to an endpoint that already
    /// carries a (non-secret) query string, reaching the server on the wire —
    /// the BrowserBase shape (`?projectId=…` in the URL, `apiKey` as the
    /// credential). Proves the upstream `request.query()` path composes rather
    /// than replaces, and that our mapping wires it.
    #[tokio::test]
    async fn query_param_auth_appends_to_existing_query_on_the_wire() {
        use std::sync::Mutex as StdMutex;

        use axum::extract::State;
        use axum::http::Uri;
        use axum::routing::post;
        use axum::{Json, Router};

        #[derive(Default)]
        struct Seen {
            query: StdMutex<Option<String>>,
        }

        async fn handler(
            State(seen): State<Arc<Seen>>,
            uri: Uri,
            Json(body): Json<Value>,
        ) -> Json<Value> {
            if let Some(q) = uri.query() {
                *seen.query.lock().unwrap() = Some(q.to_string());
            }
            let id = body.get("id").cloned().unwrap_or(Value::Null);
            let method = body.get("method").and_then(Value::as_str).unwrap_or("");
            let result = match method {
                "initialize" => json!({
                    "protocolVersion": "2025-11-25", "capabilities": {},
                    "serverInfo": { "name": "fixture", "version": "0" }
                }),
                "tools/list" => json!({ "tools": [] }),
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

        // The non-secret project id stays in the endpoint; the secret rides as a
        // query-parameter credential.
        let endpoint = format!("http://{addr}/mcp?projectId=pid-123");
        let mut d = decl("browserbase", &endpoint);
        d.auth = AuthMaterial::QueryParam {
            name: "apiKey".into(),
            value: "qp-secret-abc".into(),
        };
        let registry = registry_for_agent(&[d], &grants(&["mcp:*"])).expect("registry");
        // list_tools drives initialize + tools/list over the wire.
        let _ = registry
            .list_tools("browserbase")
            .await
            .expect("list_tools");

        let query = seen
            .query
            .lock()
            .unwrap()
            .clone()
            .expect("server saw a query");
        assert!(
            query.contains("projectId=pid-123"),
            "kept the existing id: {query}"
        );
        assert!(
            query.contains("apiKey=qp-secret-abc"),
            "appended the credential: {query}"
        );
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
