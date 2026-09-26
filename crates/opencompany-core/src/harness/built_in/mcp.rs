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

use openhuman_core as oh;

use oh::config::{Config, McpAuthConfig, McpServerConfig};
use oh::mcp::config_servers::{McpRegistrySource, McpServerRegistry};
use oh::mcp::registry::types::{ConnStatus, InstalledServer, McpTool};
use oh::security::{SecurityPolicy, ToolOperation};
use tinytools::{PermissionLevel, Tool, ToolCallOptions, ToolResult};

use crate::company::mcp::{AuthMaterial, McpServerDecl};
use crate::error::OpenCompanyError;
use crate::harness::mcp_probe::{
    McpFailure, McpFailureQueue, classify_mcp_error, operator_message, scrub, strip_endpoint,
};
use crate::ports::types::CompanyId;
use crate::ports::usage::UsageMeter;
use crate::runtime::tools::grants_cover_server;

mod registry_list;
mod registry_scoped;

pub use registry_list::OcMcpRegistryInstalledListTool;
pub use registry_scoped::OcMcpRegistryScopedTool;

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

/// The per-tool policies for the servers an agent's grants reach, narrowed the
/// same way [`granted_secrets`] narrows credential substrings so the refusal and
/// the toolbelt cannot disagree about which servers an agent can name.
pub fn granted_policies(
    decls: &[McpServerDecl],
    grants: &[String],
) -> crate::company::mcp_policy::McpToolPolicySet {
    crate::company::mcp_policy::McpToolPolicySet::from_declarations(
        decls
            .iter()
            .filter(|decl| grants_cover_server(grants, &decl.name)),
    )
}

/// The names of the enabled servers `grants` reach — the same narrowing
/// [`registry_for_agent`] applies, with nothing but the names.
pub fn granted_server_names(decls: &[McpServerDecl], grants: &[String]) -> Vec<String> {
    decls
        .iter()
        .filter(|decl| decl.enabled && grants_cover_server(grants, &decl.name))
        .map(|decl| decl.name.clone())
        .collect()
}

/// A persona brief appended when an agent is granted MCP servers: names the
/// servers it may reach and directs it to answer capability questions from a
/// **live** `mcp_list_tools` call on one of them, never from memory.
///
/// Carries server names only. Endpoints and credentials never reach the
/// persona, and there is no server-listing tool to recover them from.
pub fn capability_brief(servers: &[String]) -> String {
    let named = servers
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        " Your connected MCP servers: {named}. When you are asked what tools or integrations you have — or whether you can do something that would use one — ALWAYS call `mcp_list_tools` with the server's name to check what it offers right now. Never answer such questions from memory: a server's tools can change between turns."
    )
}

/// The company's granted MCP servers, rendered as [`openhuman_embed::McpServer`]
/// attachments an [`openhuman_embed::AgentSpec`] can carry directly (plan
/// hive-desks, Phase 2 follow-up).
///
/// # Why this exists alongside [`registry_for_agent`]
///
/// [`host_loop`](crate::harness::host_loop)'s module doc says it plainly:
/// "the company agents run on the embedded OpenHuman runtime, whose tool set
/// is its own (plus MCP servers) — there is no seam for a `Tool` this crate
/// built" for a company AGENT (as opposed to an in-process auxiliary pass).
/// [`OcMcpCallTool`] and upstream's `McpListToolsTool` are exactly such
/// tools — pushed onto
/// [`AgentBlueprint::tools`](crate::harness::built_in::build::AgentBlueprint::tools)
/// under the reserved names `mcp_call_tool` / `mcp_list_tools` so the OLD native-dispatch builder (`tool_dispatcher.rs`,
/// removed when the runtime moved to the hosted pipeline) would run OC's
/// decorator instead of OpenHuman's own implementation of those names.
///
/// That dispatch seam is gone. A name in
/// [`OPENHUMAN_NATIVE_TOOLS`](crate::harness::built_in::build::OPENHUMAN_NATIVE_TOOLS)
/// is now *always* OpenHuman's own implementation — reaching only whatever
/// [`McpServer`](openhuman_embed::McpServer)s were attached to the spec via
/// [`AgentSpec::mcp`](openhuman_embed::AgentSpec::mcp) — so `OcMcpCallTool`'s
/// registry (built from these same `decls`/`grants`) was never being called at
/// all: a company's own registered servers were unreachable, and
/// `mcp_call_tool` only ever found the internal `opencompany` hive server
/// (issue tracked alongside plan hive-desks Phase 3/4). This function is the
/// other half of that fix: it hands the SAME granted servers to
/// `agent_spec_for` so they reach the spec the way `AgentSpec::mcp` (plural —
/// "call repeatedly to add several") is meant to be used, alongside the
/// `opencompany` attachment.
///
/// **Known gap left open by this fix**: OpenHuman's own `mcp_call_tool` does
/// not scrub credentials the way `OcMcpCallTool`'s `handle_failure` does (see
/// this module's security note above) — a transport failure can surface a
/// configured bearer/token verbatim to the agent for a directly-attached
/// company server. `mcp_list_servers` is kept out of every company agent's
/// tool scope for the same reason. Restoring that hardening needs a real
/// seam into the hosted pipeline (a job for hive-desks Phase 4), not a
/// band-aid here; it is called out rather than silently reintroduced.
pub fn embed_servers_for_agent(
    decls: &[McpServerDecl],
    grants: &[String],
) -> Vec<openhuman_embed::McpServer> {
    decls
        .iter()
        .filter(|decl| decl.enabled && grants_cover_server(grants, &decl.name))
        .map(|decl| {
            // A blocked tool is denied here, not only in `OcMcpCallTool`: this
            // attachment is the path a company agent actually takes, and the
            // deny list is what the transport filters on. Deny outranks allow
            // there, so a server with an allow list cannot re-admit one.
            let mut denied = decl.disallowed_tools.clone();
            for tool in crate::company::mcp_policy::blocked_tool_names(
                &decl.tool_policies,
                &decl.tool_inventory,
            ) {
                if !denied.contains(&tool) {
                    denied.push(tool);
                }
            }
            openhuman_embed::McpServer::http(decl.name.clone(), decl.endpoint.clone())
                .auth(auth_config(&decl.auth))
                .allow_tools(decl.allowed_tools.clone())
                .deny_tools(denied)
                .timeout_secs(decl.timeout_secs)
                .description(decl.description.clone().unwrap_or_default())
        })
        .collect()
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
    /// The granted servers' per-tool policies, consulted before dialling.
    policies: crate::company::mcp_policy::McpToolPolicySet,
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
        policies: crate::company::mcp_policy::McpToolPolicySet,
    ) -> Self {
        Self {
            registry,
            security,
            secrets,
            failures,
            metering,
            policies,
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
                    "description": "Registered MCP server name, from the granted servers named in your persona brief."
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
        // Gated on the cleaned names, which are the ones that would be
        // dispatched. Placed here rather than on one of the other two entry
        // points because both default to this one.
        if self.policies.is_blocked(&server, &tool) {
            return Ok(ToolResult::error(
                crate::company::mcp_policy::blocked_refusal(&server, &tool),
            ));
        }
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
#[path = "mcp_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "mcp_blocked_tests.rs"]
mod blocked_tests;
