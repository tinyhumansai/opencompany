//! Per-agent MCP registry assembly + a credential-redacting list-servers tool
//! (issue #50).
//!
//! [`registry_for_agent`] folds a company's effective [`McpServerDecl`]s into an
//! OpenHuman [`McpServerRegistry`](oh::mcp::config_servers::McpServerRegistry) scoped to
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
use oh::mcp::config_servers::{McpRegistrySource, McpServerRegistry};
use oh::mcp::registry::types::{ConnStatus, InstalledServer, McpTool};
use oh::security::{SecurityPolicy, ToolOperation};
use oh::tools::traits::{PermissionLevel, Tool, ToolCallOptions, ToolResult};

use crate::company::mcp::{AuthMaterial, McpServerDecl, stdio_install_refusal};
use crate::error::OpenCompanyError;
use crate::harness::mcp_probe::{
    McpFailure, McpFailureQueue, classify_mcp_error, operator_message, scrub, strip_endpoint,
};
use crate::ports::types::CompanyId;
use crate::ports::usage::UsageMeter;
use crate::runtime::tools::grants_cover_server;

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
    // `from_config` takes `tinymcp`'s own client config now, not OpenHuman's
    // `Config`. `host::static_registry` is the conversion, and it already
    // degrades an unbuildable set to an empty one rather than failing.
    oh::mcp::host::static_registry(&config)
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

/// The credential substrings from the (enabled, grant-matched) servers this
/// agent reaches — the known-secret set fed to
/// [`scrub`](crate::harness::mcp_probe::scrub) so no configured credential can
/// survive into an agent-visible error. `grants` must be the same effective
/// grants passed to [`registry_for_agent`], rather than the raw manifest
/// request, because an empty request inherits the company belt and therefore
/// reaches every server that belt grants.
/// Never serialized anywhere.
pub fn granted_secrets(decls: &[McpServerDecl], grants: &[String]) -> Vec<String> {
    decls
        .iter()
        .filter(|decl| decl.enabled && grants_cover_server(grants, &decl.name))
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
                    "auth_configured": !matches!(server.auth, tinymcp::McpAuthConfig::None),
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
                    // Renamed upstream: the host-seeded source is no longer
                    // gitbooks-specific. The wire value is unchanged so an
                    // operator's existing filters keep matching.
                    McpRegistrySource::Host => "legacy_gitbooks",
                    // `#[non_exhaustive]`: a source this build does not know
                    // still has to render as something.
                    _ => "unknown",
                };
                let auth = if matches!(server.auth, tinymcp::McpAuthConfig::None) {
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

/// What `mcp_call_tool` needs to record an `OauthCall` usage sample.
///
/// Mirrors [`ComposioMetering`](crate::harness::composio::ComposioMetering):
/// the company and agent the sample is scoped to, and a meter that may be
/// absent because the harness wires none in some embeddings — in which case the
/// tool still works and simply is not metered.
#[derive(Clone)]
pub struct McpMetering {
    /// The company the sample is scoped to.
    pub company: CompanyId,
    /// The agent whose turn made the call.
    pub agent: String,
    /// The usage meter. `None` leaves metering off entirely.
    pub meter: Option<Arc<dyn UsageMeter>>,
}

impl McpMetering {
    /// A handle that records nothing — for embeddings and tests that wire no
    /// meter. Named rather than spelled out at each call site so "unmetered" is
    /// a visible decision instead of a `None` a reader has to interpret.
    pub fn off() -> Self {
        Self {
            company: CompanyId::new("unmetered"),
            agent: String::new(),
            meter: None,
        }
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
    /// Where a completed call is counted (issue #698). See
    /// [`McpMetering`].
    metering: McpMetering,
}

impl OcMcpCallTool {
    /// Builds the decorator over the agent's registry, the (permissive) MCP
    /// security policy, the granted servers' credential substrings, the shared
    /// failure queue, and the metering handle.
    pub fn new(
        registry: Arc<McpServerRegistry>,
        security: Arc<SecurityPolicy>,
        secrets: Vec<String>,
        failures: McpFailureQueue,
        metering: McpMetering,
    ) -> Self {
        Self {
            registry,
            security,
            secrets,
            failures,
            metering,
        }
    }

    /// Whether the named server has a credential configured (drives the
    /// 401-vs-rejected classification without reading the credential).
    fn auth_configured(&self, server: &str) -> bool {
        self.registry
            .get(server)
            .map(|s| !matches!(s.auth, tinymcp::McpAuthConfig::None))
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
                // Metered on success only, mirroring `composio_execute`: a call
                // that actually reached the server. `connections` in the read
                // model is the count of providers seen, so counting a failed
                // call would mint a connection row for a server that never
                // answered (issue #698). One line by design — see the module
                // docs on `crate::metering::oauth` for why the shape and the
                // swallow live there rather than here.
                if let Some(meter) = &self.metering.meter {
                    crate::metering::record_oauth_call(
                        meter.as_ref(),
                        &self.metering.company,
                        &self.metering.agent,
                        &crate::metering::mcp_provider(&server),
                        crate::ports::now_millis(),
                    )
                    .await;
                }
                // A free function, not `.into()`. `ToolResult` moved into the
                // shared `tinytools` vocabulary, and `McpToolResult` belongs to
                // `tinymcp-bus` — two foreign types, so the orphan rule forbids
                // the `From` impl this used to call. OpenHuman spells the
                // conversion once, in `skills::types`, rather than at each call
                // site, because written out by hand it is three chances to get
                // the error flag the wrong way round.
                let mut result: ToolResult =
                    oh::skills::types::tool_result_from_mcp(result.rendered);
                if options.prefer_markdown && result.markdown_formatted.is_none() {
                    result.markdown_formatted = Some(result.output());
                }
                Ok(result)
            }
            Err(err) => Ok(self.handle_failure(&server, &tool, &anyhow::Error::new(err))),
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
    // Models routinely wrap identifiers in markdown emphasis when they answer
    // in prose style (`server: \`werkplaats\``). A trailing backtick is part of
    // the markdown, not the name: strip wrapping / trailing fence characters
    // so the registry lookup matches the configured server name. Only *leading
    // and trailing* occurrences are removed — a legitimate name never starts
    // or ends with one of these, so stripping cannot mangle a real id.
    let cleaned = value
        .trim_start_matches(['`', '*', '_'])
        .trim_end_matches(['`', '*', '_', '.', ',', ';', ':', '!']);
    if cleaned.is_empty() {
        return Err(anyhow::anyhow!("missing required `{key}`"));
    }
    Ok(cleaned.to_string())
}

// ---------------------------------------------------------------------------
// Company-scoped MCP lifecycle (McpRuntime)
// ---------------------------------------------------------------------------

/// The transport filter every directory search is pinned to — upstream's
/// vocabulary for "has a hosted HTTP endpoint" (`registry::apply_transport`
/// keeps `is_deployed` rows for `"hosted"`, drops them for `"stdio"`).
///
/// **Hardcoded, not a parameter the console may set.** A stdio entry launches a
/// local subprocess through `npx` / `uvx`, and the tenant image is
/// `debian:bookworm-slim` plus `ca-certificates`, `curl`, `libssl3` and X11 —
/// no Node, no Python, no package manager (issue #1270). So there is no caller
/// for whom `"stdio"` or `"all"` would produce an installable row, and offering
/// the knob would only let the console show an operator servers that fail at
/// install time. Widening it is one edit here, on the day a sidecar that can
/// actually run stdio servers exists; until then the honest surface is the one
/// that cannot express the broken request.
const HOSTED_TRANSPORT: &str = "hosted";

/// Company-home-scoped persistence and access to OpenHuman's live MCP registry.
pub struct McpRuntime {
    config: oh::config::Config,
}

impl McpRuntime {
    /// Creates a runtime whose MCP SQLite store lives beneath `workspace_dir`.
    pub fn new(workspace_dir: PathBuf) -> Self {
        Self {
            config: Self::config_for(workspace_dir),
        }
    }

    /// The config that selects the MCP store beneath `workspace_dir`.
    ///
    /// Public because the agent toolbelt needs the *same* one: OpenHuman's
    /// `mcp_registry_*` tools take a config now rather than reading a process
    /// global, and a tool built over a different config would quietly read a
    /// different SQLite store than REST does — the installs would be there in
    /// the console and absent from the turn.
    #[must_use]
    pub fn config_for(workspace_dir: PathBuf) -> oh::config::Config {
        oh::config::Config {
            workspace_dir,
            ..Default::default()
        }
    }

    /// The config the three **directory** calls run against.
    ///
    /// Upstream carries a `registry_auth.smithery_api_key`, and this deployment
    /// deliberately never sets one. A per-company Smithery key was a credential
    /// slot on a console tab — to store, rotate, revoke and explain — and what
    /// it bought was one vendor's hosted listings; the open
    /// `modelcontextprotocol/registry` is queried without any credential at all.
    /// Left unset, upstream still falls back to the host's `SMITHERY_API_KEY`
    /// where an operator has set one on the process, which is the whole of the
    /// Smithery story now.
    fn directory_config(&self) -> oh::config::Config {
        self.config.clone()
    }

    /// Search the upstream MCP directory — the official
    /// `modelcontextprotocol/registry` — paged and SQLite-cached upstream
    /// (issue #1270).
    ///
    /// The console's only way to answer "what could I add?". The static server
    /// list cannot: an operator has to arrive already knowing an endpoint, so
    /// that surface is empty until somebody pastes a URL into it.
    ///
    /// The transport filter is fixed at [`HOSTED_TRANSPORT`] rather than exposed
    /// as a parameter — see that constant for why.
    pub async fn search(
        &self,
        query: Option<String>,
        page: Option<u32>,
        page_size: Option<u32>,
    ) -> crate::Result<serde_json::Value> {
        oh::mcp::registry::ops::mcp_clients_registry_search(
            &self.directory_config(),
            query,
            Some(HOSTED_TRANSPORT.to_string()),
            page,
            page_size,
        )
        .await
        .map(|outcome| outcome.value)
        .map_err(|e| OpenCompanyError::Harness(format!("mcp registry search failed: {e}")))
    }

    /// One directory entry in full, routed back to the registry it came from.
    pub async fn registry_get(&self, qualified_name: String) -> crate::Result<serde_json::Value> {
        oh::mcp::registry::ops::mcp_clients_registry_get(&self.directory_config(), qualified_name)
            .await
            .map(|outcome| outcome.value)
            .map_err(|e| OpenCompanyError::Harness(format!("mcp registry lookup failed: {e}")))
    }

    /// Installs a directory entry by qualified name and returns the resulting
    /// record. Idempotent upstream: re-installing a server already present
    /// refreshes its env/config onto the existing row rather than writing a
    /// second one.
    ///
    /// **Refuses a stdio install.** Upstream's picker already prefers a hosted
    /// HTTP connection over a local subprocess, so this only fires for an entry
    /// that offers *nothing but* stdio — which this deployment cannot launch
    /// (see [`stdio_install_refusal`]). The search filter above keeps such
    /// entries off the operator's screen in the first place; this is the belt to
    /// that braces, because a caller can POST a qualified name the search never
    /// offered. A refused install that we ourselves created is rolled back; one
    /// that was already on disk is left alone, since it is not ours to remove.
    pub async fn install_from_directory(
        &self,
        qualified_name: String,
        env: HashMap<String, String>,
    ) -> crate::Result<InstalledServer> {
        let outcome = oh::mcp::registry::ops::mcp_clients_install(
            &self.directory_config(),
            qualified_name.clone(),
            env,
            None,
        )
        .await
        .map_err(|e| OpenCompanyError::Harness(format!("mcp install failed: {e}")))?;
        let already_installed = outcome
            .value
            .get("already_installed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let record = outcome.value.get("server").cloned().ok_or_else(|| {
            OpenCompanyError::Harness("mcp install returned no server record".to_string())
        })?;
        let server: InstalledServer = serde_json::from_value(record).map_err(|e| {
            OpenCompanyError::Harness(format!("mcp install record is unreadable: {e}"))
        })?;
        if server.transport.deployment_url().is_none() {
            if !already_installed {
                let _ = self.uninstall(&server.server_id).await;
            }
            return Err(OpenCompanyError::InvalidRequest(stdio_install_refusal(
                &qualified_name,
            )));
        }
        Ok(server)
    }

    /// Rotate an install's environment values (write-only, never read back).
    pub async fn update_env(
        &self,
        server_id: String,
        env: HashMap<String, String>,
    ) -> crate::Result<()> {
        oh::mcp::registry::ops::mcp_clients_update_env(&self.config, server_id, env)
            .await
            .map(|_| ())
            .map_err(|e| OpenCompanyError::Harness(format!("mcp env update failed: {e}")))
    }

    /// Reconnects enabled installed servers. Failures are logged by OpenHuman
    /// per server and never prevent the company runtime from booting.
    pub async fn boot(&self) {
        oh::mcp::registry::boot::spawn_installed_servers(&self.config).await;
    }

    /// The `tinymcp` service backing this runtime's registry.
    ///
    /// The store and connection map used to be reachable as free functions on
    /// `oh::mcp::registry`; the registry moved into `tinymcp` and both are now
    /// accessors on the one service the process holds for a config. Opening is
    /// per-config and cached upstream, so this is a lookup rather than a build.
    fn host(&self) -> crate::Result<std::sync::Arc<oh::mcp::host::McpHost>> {
        oh::mcp::host::for_config(&self.config).map_err(store_error)
    }

    /// Returns every persisted install without loading secret environment values.
    pub fn list(&self) -> crate::Result<Vec<InstalledServer>> {
        self.host()?
            .dynamic()
            .store()
            .list_servers()
            .map_err(store_error)
    }

    /// Persists an install and its write-only environment values.
    pub fn install(
        &self,
        server: &InstalledServer,
        env: &HashMap<String, String>,
    ) -> crate::Result<()> {
        let store = self.host()?;
        let store = store.dynamic().store();
        store.insert_server(server).map_err(store_error)?;
        // `set_env_values` takes an ordered map now; the write is the same one.
        let env: std::collections::BTreeMap<String, String> =
            env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        if let Err(error) = store.set_env_values(&server.server_id, &env) {
            let _ = store.delete_server(&server.server_id);
            return Err(store_error(error));
        }
        Ok(())
    }

    /// Loads an installed server, establishing the company-store membership
    /// check before touching OpenHuman's process-global connection registry.
    pub fn get(&self, server_id: &str) -> crate::Result<InstalledServer> {
        self.host()?
            .dynamic()
            .store()
            .get_server(server_id)
            // Only a genuinely absent install is "not found". A store that
            // fails to read must not be reported as a missing server — the
            // caller would be told to reinstall something that is there.
            .map_err(|error| match error {
                tinymcp::Error::UnknownServer { .. } => {
                    OpenCompanyError::McpServerNotFound(server_id.to_string())
                }
                other => store_error(other),
            })
    }

    /// Connects an installed server and returns its advertised tools.
    pub async fn connect(&self, server_id: &str) -> crate::Result<Vec<McpTool>> {
        let server = self.get(server_id)?;
        oh::mcp::registry::connections::connect(&self.config, &server)
            .await
            .map_err(harness_error)
    }

    /// Disconnects an installed server after verifying it belongs to this store.
    ///
    /// Goes through this runtime's own service rather than the
    /// `oh::mcp::registry::connections` free function, which reads the
    /// *process-global* one. `connect` above is per-config, so the free
    /// function would look for the connection in a service that never holds it
    /// — this runtime never calls `host::init` — and answer a truthful-looking
    /// `false` for a server that is in fact connected.
    pub async fn disconnect(&self, server_id: &str) -> crate::Result<bool> {
        self.get(server_id)?;
        Ok(self
            .host()?
            .dynamic()
            .connections()
            .disconnect(server_id)
            .await)
    }

    /// Disconnects and deletes an installed server and its environment values.
    pub async fn uninstall(&self, server_id: &str) -> crate::Result<bool> {
        self.get(server_id)?;
        // Same per-config service as `disconnect`, for the same reason.
        let host = self.host()?;
        host.dynamic().connections().disconnect(server_id).await;
        host.dynamic()
            .store()
            .delete_server(server_id)
            .map_err(store_error)
    }

    /// Returns connection state joined by OpenHuman against this runtime's store.
    ///
    /// Reporting status must not fail a caller that is only rendering it, so a
    /// service that will not open — or a store that will not list — reports
    /// "nothing installed" rather than an error, which is what the free
    /// function this replaced did.
    pub async fn status(&self) -> Vec<ConnStatus> {
        let Ok(host) = self.host() else {
            return Vec::new();
        };
        let registry = host.dynamic();
        match registry.connections().all_status(registry.store()).await {
            Ok(statuses) => statuses,
            Err(error) => {
                log::warn!("[mcp] could not summarize connection status: {error}");
                Vec::new()
            }
        }
    }

    /// Returns the cached tool list for a connected installed server.
    pub async fn tools(&self, server_id: &str) -> crate::Result<Vec<McpTool>> {
        self.get(server_id)?;
        self.host()?
            .dynamic()
            .connections()
            .tools_for(server_id)
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
        // The transport returns a structured result now; the raw JSON payload
        // is the field this surface has always handed back.
        self.host()?
            .dynamic()
            .connections()
            .call_tool(server_id, tool_name, arguments)
            .await
            .map(|result| result.raw_result)
            .map_err(harness_error)
    }
}

fn store_error(error: impl std::fmt::Display) -> OpenCompanyError {
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
            read_only_tools: Vec::new(),
            timeout_secs: 30,
            enabled: true,
            source: crate::company::mcp::McpSource::Runtime,
            auth: AuthMaterial::None,
        }
    }

    fn grants(g: &[&str]) -> Vec<String> {
        g.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_decls_yield_no_registry() {
        assert!(registry_for_agent(&[], &grants(&["mcp:*"])).is_none());
    }

    #[test]
    fn server_name_strips_markdown_fences() {
        // Models wrap identifiers in markdown when answering in prose style;
        // the fence characters belong to the answer, not the server name
        // (seen live: server="werkplaats`" -> "unknown mcp server `werkplaats``").
        let mk = |v: &str| serde_json::json!({ "server": v });
        let parsed = |v: &str| required_string_arg(&mk(v), "server").unwrap();
        assert_eq!(parsed("werkplaats"), "werkplaats");
        assert_eq!(parsed("werkplaats`"), "werkplaats");
        assert_eq!(parsed("`werkplaats`"), "werkplaats");
        assert_eq!(parsed("*werkplaats*"), "werkplaats");
        assert_eq!(parsed("werkplaats."), "werkplaats");
        assert_eq!(parsed("werk"), "werk");
        assert!(required_string_arg(&mk("```"), "server").is_err());
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

    /// An empty raw request inherits the company belt at the builder seam. The
    /// scrubber must receive those effective grants too, or an MCP credential
    /// echoed by a server can reach the agent-visible failure even though the
    /// registry correctly wires that server.
    #[test]
    fn granted_secrets_follows_effective_grants() {
        let mut server = decl("fixture", "http://127.0.0.1:1/mcp");
        server.auth = AuthMaterial::Bearer("inherited-canary".into());
        let inherited = granted_secrets(std::slice::from_ref(&server), &grants(&["*", "mcp:*"]));
        assert_eq!(inherited, vec!["inherited-canary"]);

        let omitted = granted_secrets(std::slice::from_ref(&server), &grants(&["*"]));
        assert!(omitted.is_empty());
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
        let secrets = granted_secrets(std::slice::from_ref(&d), &grants(&["mcp:*"]));
        let registry = registry_for_agent(&[d], &grants(&["mcp:*"])).expect("registry");

        let queue = McpFailureQueue::default();
        let tool = OcMcpCallTool::new(
            registry,
            Arc::new(SecurityPolicy::default()),
            secrets,
            queue.clone(),
            McpMetering::off(),
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

    /// A completed MCP call is counted, and a failed one is not (issue #698).
    ///
    /// The rule this exercises — `mcp:` namespacing — is unit-tested in
    /// `crate::metering::oauth`. What only this test can reach is the wiring:
    /// that the success branch calls the meter at all, that it passes *this*
    /// company and agent rather than a default, and that the failure branch
    /// stays silent. Deleting the `if let Some(meter)` block, moving it to the
    /// `Err` arm, or threading the wrong field all pass every other test in the
    /// tree.
    ///
    /// Both outcomes are driven through one fixture whose `tools/call` succeeds
    /// or fails on the tool name, because "counts a success" is only half the
    /// contract: `connections` is the count of providers seen, so a metered
    /// failure would mint a connection row for a server that never answered.
    #[tokio::test]
    async fn a_completed_mcp_call_is_metered_and_a_failed_one_is_not() {
        use axum::extract::State;
        use axum::routing::post;
        use axum::{Json, Router};
        use std::sync::Mutex;

        use crate::ports::usage::{SampleKind, UsageMeter, UsageSample};

        #[derive(Default)]
        struct RecordingMeter {
            samples: Mutex<Vec<(String, UsageSample)>>,
        }

        #[async_trait]
        impl UsageMeter for RecordingMeter {
            async fn record(&self, company: &CompanyId, sample: &UsageSample) -> crate::Result<()> {
                self.samples
                    .lock()
                    .unwrap()
                    .push((company.to_string(), sample.clone()));
                Ok(())
            }
            async fn query(
                &self,
                _company: &CompanyId,
                _since: u64,
            ) -> crate::Result<Vec<UsageSample>> {
                Ok(Vec::new())
            }
        }

        async fn handler(
            State(()): State<()>,
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
                    "result": { "tools": [
                        { "name": "echo", "description": "e", "inputSchema": { "type": "object" } },
                        { "name": "boom", "description": "b", "inputSchema": { "type": "object" } }
                    ] }
                }))
                .into_response(),
                "tools/call" => {
                    let called = body
                        .get("params")
                        .and_then(|p| p.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if called == "boom" {
                        return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom")
                            .into_response();
                    }
                    Json(json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": { "content": [{ "type": "text", "text": "ok" }] }
                    }))
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

        let endpoint = format!("http://{addr}/mcp");
        let registry = registry_for_agent(&[decl("fixture", &endpoint)], &grants(&["mcp:*"]))
            .expect("registry");

        let meter = Arc::new(RecordingMeter::default());
        let tool = OcMcpCallTool::new(
            registry,
            Arc::new(SecurityPolicy::default()),
            Vec::new(),
            McpFailureQueue::default(),
            McpMetering {
                company: CompanyId::new("acme"),
                agent: "ceo".to_string(),
                meter: Some(meter.clone()),
            },
        );

        let ok = tool
            .execute(json!({ "server": "fixture", "tool": "echo", "arguments": {} }))
            .await
            .expect("mcp_call_tool");
        assert!(!ok.is_error, "the fixture's `echo` succeeds: {ok:?}");

        {
            let samples = meter.samples.lock().unwrap();
            assert_eq!(samples.len(), 1, "one completed call, one sample");
            let (company, sample) = &samples[0];
            assert_eq!(company, "acme", "the sample is scoped to this company");
            assert_eq!(sample.agent, "ceo", "attributed to the calling agent");
            assert_eq!(sample.kind, SampleKind::OauthCall);
            // Namespaced, so this row cannot merge with a Composio toolkit that
            // happens to share the server's name.
            assert_eq!(sample.provider, "mcp:fixture");
            assert_eq!(sample.input_tokens, 0);
            assert_eq!(sample.output_tokens, 0);
            assert_eq!(sample.cost_usd, 0.0);
        }

        let failed = tool
            .execute(json!({ "server": "fixture", "tool": "boom", "arguments": {} }))
            .await
            .expect("mcp_call_tool");
        assert!(failed.is_error, "the fixture's `boom` fails: {failed:?}");
        assert_eq!(
            meter.samples.lock().unwrap().len(),
            1,
            "a call that never reached the server must not mint a connection row"
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

    use oh::mcp::registry::types::{CommandKind, Transport};

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

    /// `get` on an install that was never persisted reports `McpServerNotFound`
    /// — the "genuinely absent" half of the store-error split.
    #[test]
    fn get_on_an_absent_server_reports_not_found() {
        let temp = tempfile::tempdir().expect("tempdir");
        let runtime = McpRuntime::new(temp.path().join("workspace"));
        let error = runtime
            .get("no-such-server")
            .expect_err("an absent install must not resolve");
        assert!(
            matches!(error, OpenCompanyError::McpServerNotFound(ref id) if id == "no-such-server"),
            "absent install must be McpServerNotFound, got: {error:?}"
        );
    }

    /// `get` on a store that fails to read must NOT be reported as a missing
    /// server — the caller would be told to reinstall something that is there.
    /// Truncating the SQLite file beneath the runtime's open connection forces
    /// the next `get_server` read to fail, and the error must surface as a
    /// `Store` error rather than the blanket `McpServerNotFound` the pre-split
    /// code produced for every failure.
    #[test]
    fn get_on_a_store_that_fails_to_read_reports_store_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workspace = temp.path().join("workspace");
        let runtime = McpRuntime::new(workspace.clone());
        // Open the host so the store and its schema exist on disk.
        runtime.list().expect("open the mcp store");
        // Corrupt the file beneath the open connection: the query can no longer
        // be satisfied, so `get_server` must fail with a store error.
        let db = workspace.join("mcp_clients").join("mcp_clients.db");
        std::fs::write(&db, b"this is not a sqlite database").expect("corrupt the store file");
        let error = runtime
            .get("some-server")
            .expect_err("a store that cannot read must not resolve");
        assert!(
            matches!(error, OpenCompanyError::Store(_)),
            "a failing store read must surface as Store, got: {error:?}"
        );
    }
}
