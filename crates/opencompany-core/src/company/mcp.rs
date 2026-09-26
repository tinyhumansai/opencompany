//! Per-tenant MCP tool servers: the inert, ungated data model plus the pure
//! merge/validation and the async secret-resolution used to materialize a
//! company's *effective* MCP servers (issue #50).
//!
//! A company's effective MCP servers are the union of three sources, merged
//! lowest-precedence first by [`effective_mcp_servers`]:
//!
//! 1. **Default** — the install-wide `[[default_mcp_server]]` entries in the
//!    instance `config.toml`, shipped enabled by a packaged Open Company so a
//!    fresh install has working tools with no user setup (issue #527). They
//!    apply to *every* company on the install and are normalized once, at the
//!    config boundary, by [`normalize_default_servers`].
//! 2. **Manifest** — the `[[mcp_server]]` entries committed in `company.toml`
//!    ([`McpServer`]). Declarative intent; never a credential. A manifest entry
//!    shadows a default of the same name: the company said something specific.
//! 3. **Runtime** — servers the operator adds through the console, persisted as
//!    a single JSON index in the [`SecretStore`](crate::ports::SecretStore)
//!    under [`RUNTIME_INDEX_KEY`]. A runtime entry with the *same name* as a
//!    manifest **or default** server is an **override** (enable/disable, tool
//!    allow-list) — the body wins, the lower layer keeps the provenance badge.
//!
//! Credentials live apart from the declarations: a server's outbound token is
//! written to its own per-server key ([`auth_key`]) — never inline in the index
//! or the manifest — and is resolved into [`AuthMaterial`] only at harness build
//! time by [`resolve_effective`]. Nothing here ever serializes a credential into
//! an API response, log line, or agent-visible output.
//!
//! Hosted v1 boundary: **HTTP transport only**. A server that declares a stdio
//! `command` is rejected by [`validate_servers`].

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::company::types::McpServer;
use crate::error::OpenCompanyError;
use crate::ports::SecretStore;
use crate::ports::types::{CompanyId, SecretValue};

/// The [`SecretStore`](crate::ports::SecretStore) key holding the JSON runtime
/// server index (a `Vec<McpServer>` of console-added servers + manifest
/// overrides).
pub const RUNTIME_INDEX_KEY: &str = "mcp/servers";

/// The per-request timeout a server declaration gets when it names none.
///
/// Lives here rather than beside [`McpServer`] because every declaration path
/// defaults it — the manifest through serde, `mcp.json` through
/// [`super::mcp_file`] — and two spellings of "30" is how they come to disagree.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// The canonical per-server credential key. A server's outbound token is stored
/// here (write-only via the console); the value is a JSON [`StoredAuth`].
pub fn auth_key(name: &str) -> String {
    format!("mcp/{name}/auth")
}

/// The per-server health key. Holds the last probe outcome as a JSON
/// [`McpHealth`]. **Invariant**: the value written here is always scrubbed — it
/// is a non-secret status record and MUST NEVER carry a credential (see
/// [`save_health`]). Distinct from [`auth_key`], which holds the write-only
/// credential and is never read back out.
pub fn health_key(name: &str) -> String {
    format!("mcp/{name}/health")
}

/// Where an effective server declaration came from — drives the console's source
/// badge and the delete-guard (a manifest or default server cannot be deleted,
/// only disabled/overridden).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpSource {
    /// Declared in `company.toml`'s `[[mcp_server]]`.
    Manifest,
    /// Added at runtime through the console.
    Runtime,
    /// Shipped enabled by the packaged install: a `[[default_mcp_server]]` entry
    /// in the instance `config.toml` (issue #527). Present for every company on
    /// the install, which is what makes it a distinct provenance rather than a
    /// flavour of [`Self::Manifest`] — nobody wrote it into *this* company, and
    /// the console must not label it as operator-added.
    Default,
    /// Installed from an upstream MCP directory — Smithery.ai or the official
    /// `modelcontextprotocol/registry` — through the console's browse surface
    /// (issue #1270).
    ///
    /// Distinct from [`Self::Runtime`] because the two are keyed differently and
    /// deleted differently: a runtime server is addressed by `name` and lives in
    /// this company's runtime index, while a registry install is addressed by a
    /// stable `server_id` and lives in OpenHuman's own store, so removing one
    /// means uninstalling it there rather than dropping an index row.
    Registry,
}

/// The operator-facing refusal for a directory entry that can only run as a
/// local stdio subprocess (issue #1270).
///
/// Says *why* rather than "unsupported": the blocker is not the read-only root
/// filesystem (tenants mount a writable `/data` and an emptyDir `/tmp`) but that
/// the tenant image carries no Node, Python or package manager to launch one
/// with — a stdio install would fail on `npx: not found`. One function so the
/// two places that can refuse an install — the catalogue pre-check at the route
/// and the post-install belt in
/// [`McpRuntime`](crate::harness::mcp::McpRuntime) — say the same sentence, and
/// so the day a sidecar makes stdio runnable there is one message to retire.
pub fn stdio_install_refusal(qualified_name: &str) -> String {
    format!(
        "`{qualified_name}` offers no hosted HTTP endpoint — it can only run as a local \
         subprocess, and this deployment ships no Node, Python or package manager to launch \
         one. Pick a server with a hosted endpoint, or add it by URL if you host it yourself."
    )
}

/// Resolved outbound auth material for one MCP server.
///
/// This is the *in-process* resolved credential, filled from the
/// [`SecretStore`](crate::ports::SecretStore) at harness-build time. It defaults
/// to [`AuthMaterial::None`] and is **never** serialized anywhere agent- or
/// operator-visible (it derives no `Serialize`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum AuthMaterial {
    /// No outbound auth.
    #[default]
    None,
    /// `Authorization: Bearer <token>`.
    Bearer(String),
    /// A single custom request header.
    Header { name: String, value: String },
    /// A credential carried as a URL query parameter (`?<name>=<value>`), the
    /// BrowserBase / Parallel-Search style. The upstream transport already
    /// applies this via `request.query()` (`mcp_client/client.rs`), so wiring it
    /// needs zero vendor changes — but it means the credential ends up in the
    /// request URL, which is exactly why the error-surfacing seams strip query
    /// strings before persisting or emitting anything (see
    /// [`crate::harness::mcp_probe::scrub`]).
    QueryParam { name: String, value: String },
    /// An OAuth 2.0 (authorization-code + PKCE) credential obtained through the
    /// console's browser sign-in flow ([`crate::company::mcp_oauth`]). The
    /// resolved `access_token` is sent to the transport as an
    /// `Authorization: Bearer` (the harness `auth_config` mapping); the
    /// remaining fields are the bookkeeping the OAuth refresh path needs to mint
    /// a fresh access token without another browser round-trip.
    ///
    /// **Security**: every token field is enumerated by [`Self::secret_values`],
    /// so the access token, the refresh token, and any confidential
    /// `client_secret` all feed the scrubber and can never survive into an
    /// agent-visible error, health record, or API response.
    OAuth {
        /// The bearer access token sent to the server (short-lived, ≈1h).
        access_token: String,
        /// The refresh token, when the authorization server issued one. Absent
        /// servers force a fresh browser sign-in once the access token expires.
        refresh_token: Option<String>,
        /// The dynamically-registered client id (RFC 7591).
        client_id: String,
        /// The confidential client secret, when the server issued one.
        client_secret: Option<String>,
        /// The token endpoint a refresh POSTs to.
        token_endpoint: String,
        /// Unix seconds when `access_token` expires (best-effort).
        expires_at: u64,
    },
}

impl AuthMaterial {
    /// Whether any credential is configured (for the non-secret
    /// `auth_configured` status field). Never reveals the value.
    pub fn is_configured(&self) -> bool {
        !matches!(self, AuthMaterial::None)
    }

    /// The concrete credential substrings this material carries, for the
    /// scrubber's known-secret set. Never surfaced to any caller that
    /// serializes — used only to feed [`crate::harness::mcp_probe::scrub`].
    pub fn secret_values(&self) -> Vec<String> {
        match self {
            AuthMaterial::None => Vec::new(),
            AuthMaterial::Bearer(token) => vec![token.clone()],
            AuthMaterial::Header { value, .. } => vec![value.clone()],
            AuthMaterial::QueryParam { value, .. } => vec![value.clone()],
            // CRITICAL: every OAuth token substring must feed the scrubber —
            // the access token (sent as the bearer), the refresh token, and any
            // confidential client secret. Missing one would let it survive into
            // an error/health/agent-visible surface.
            AuthMaterial::OAuth {
                access_token,
                refresh_token,
                client_secret,
                ..
            } => {
                let mut out = vec![access_token.clone()];
                if let Some(refresh) = refresh_token {
                    out.push(refresh.clone());
                }
                if let Some(secret) = client_secret {
                    out.push(secret.clone());
                }
                out
            }
        }
    }
}

/// The on-disk credential envelope stored under [`auth_key`]. Kept private —
/// only [`resolve_effective`] / [`store_bearer`] cross this boundary.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredAuth {
    Bearer {
        token: String,
    },
    Header {
        name: String,
        value: String,
    },
    QueryParam {
        name: String,
        value: String,
    },
    /// The persisted OAuth bundle: the access token plus everything a silent
    /// refresh needs. Written by the callback exchange and the refresh path;
    /// read only by [`load_auth`] at harness-build / probe time.
    Oauth {
        access_token: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refresh_token: Option<String>,
        client_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_secret: Option<String>,
        token_endpoint: String,
        expires_at: u64,
    },
}

impl From<StoredAuth> for AuthMaterial {
    fn from(stored: StoredAuth) -> Self {
        match stored {
            StoredAuth::Bearer { token } => AuthMaterial::Bearer(token),
            StoredAuth::Header { name, value } => AuthMaterial::Header { name, value },
            StoredAuth::QueryParam { name, value } => AuthMaterial::QueryParam { name, value },
            StoredAuth::Oauth {
                access_token,
                refresh_token,
                client_id,
                client_secret,
                token_endpoint,
                expires_at,
            } => AuthMaterial::OAuth {
                access_token,
                refresh_token,
                client_id,
                client_secret,
                token_endpoint,
                expires_at,
            },
        }
    }
}

/// One effective MCP server declaration for a company — the merge of a manifest
/// [`McpServer`] and any runtime override, with auth resolved to
/// [`AuthMaterial`] at harness-build time.
#[derive(Clone, Debug)]
pub struct McpServerDecl {
    /// Stable slug used by the bridge tools + console.
    pub name: String,
    /// HTTP(S) endpoint URL.
    pub endpoint: String,
    /// Optional human-readable description.
    pub description: Option<String>,
    /// Allow-list of remote tool names (empty = all, minus `disallowed_tools`).
    pub allowed_tools: Vec<String>,
    /// Deny-list of remote tool names (takes precedence).
    pub disallowed_tools: Vec<String>,
    /// Remote tool names the operator declares **read-only** on this server
    /// (issue #1124). Merged through [`effective_mcp_servers`] and editable
    /// through the runtime-override layer exactly as the two lists above are, so
    /// an operator expresses it without hand-editing the manifest. A call to a
    /// tool named here does not park under `auto`; every other bridge call does.
    pub read_only_tools: Vec<String>,
    /// Per-request timeout in seconds.
    pub timeout_secs: u64,
    /// Whether this server is exposed to agents.
    pub enabled: bool,
    /// Manifest vs runtime provenance.
    pub source: McpSource,
    /// Resolved outbound credential (`None` until [`resolve_effective`] fills it).
    pub auth: AuthMaterial,
    /// The operator's stored per-tool approval policy, layered over
    /// [`read_only_tools`](Self::read_only_tools) by
    /// [`effective_policies`](super::mcp_policy::effective_policies). Empty
    /// until [`resolve_effective`] fills it.
    pub tool_policies: super::mcp_policy::McpToolPolicies,
    /// The tools discovery last saw on this server and the tier each was
    /// suggested under. Empty until [`resolve_effective`] fills it, and empty
    /// for a server discovery has never reached.
    pub tool_inventory: super::mcp_policy::McpToolInventory,
}

impl McpServerDecl {
    fn from_server(server: &McpServer, source: McpSource) -> Self {
        Self {
            name: server.name.trim().to_string(),
            endpoint: server.endpoint.trim().to_string(),
            description: server.description.clone(),
            allowed_tools: normalize_tools(&server.allowed_tools),
            disallowed_tools: normalize_tools(&server.disallowed_tools),
            read_only_tools: normalize_tools(&server.read_only_tools),
            timeout_secs: server.timeout_secs,
            enabled: server.enabled,
            source,
            auth: AuthMaterial::None,
            tool_policies: super::mcp_policy::McpToolPolicies::default(),
            tool_inventory: super::mcp_policy::McpToolInventory::default(),
        }
    }
}

/// Merges the install defaults, the manifest servers and the runtime index into
/// the effective set.
///
/// Three layers, lowest to highest: **default** (install-wide, issue #527) →
/// **manifest** (this company's `company.toml`) → **runtime** (console edits).
/// A higher layer overriding a lower one replaces the body — its
/// enable/disable + tool lists win — but the declaration keeps the **lowest**
/// layer's badge, so the console still shows where the server came from and
/// still refuses to delete it. That is the rule the manifest/runtime pair
/// already followed; defaults join it rather than introducing a second one.
///
/// A name in both the defaults and the manifest resolves to the manifest: a
/// company that declares a server has said something specific about it, and the
/// install-wide default is the fallback it overrides.
///
/// Order is manifest first (in declared order), then defaults the manifest did
/// not shadow, then runtime-only additions. Manifest stays first so an install
/// with no defaults configured produces a byte-identical list to before.
///
/// Auth is left [`AuthMaterial::None`]; [`resolve_effective`] fills it.
pub fn effective_mcp_servers(
    defaults: &[McpServer],
    manifest: &[McpServer],
    runtime: &[McpServer],
) -> Vec<McpServerDecl> {
    let mut out: Vec<McpServerDecl> = Vec::new();

    // The body that actually applies for `name`: the runtime override when the
    // console has one, else the layer's own declaration. Shared by the manifest
    // and default passes so the override rule cannot drift between them.
    let with_override = |own: &McpServer, name: &str, source: McpSource| match runtime
        .iter()
        .find(|r| r.name.trim() == name)
    {
        Some(override_entry) => McpServerDecl::from_server(override_entry, source),
        None => McpServerDecl::from_server(own, source),
    };

    for m in manifest {
        let name = m.name.trim();
        if name.is_empty() {
            continue;
        }
        out.push(with_override(m, name, McpSource::Manifest));
    }

    for d in defaults {
        let name = d.name.trim();
        if name.is_empty() || manifest.iter().any(|m| m.name.trim() == name) {
            continue;
        }
        out.push(with_override(d, name, McpSource::Default));
    }

    for r in runtime {
        let name = r.name.trim();
        if name.is_empty()
            || manifest.iter().any(|m| m.name.trim() == name)
            || defaults.iter().any(|d| d.name.trim() == name)
        {
            continue;
        }
        out.push(McpServerDecl::from_server(r, McpSource::Runtime));
    }

    out
}

/// Flattens a company's effective MCP servers into the `(server, tool)` pairs
/// their `read_only_tools` declarations name.
///
/// Test-only: it ignores the stored tool policy, so it is the differential
/// oracle for [`mcp_allow_set`](super::mcp_policy::mcp_allow_set), which is
/// what every gate reads. A disabled server contributes nothing.
#[cfg(test)]
pub fn mcp_read_set(servers: &[McpServerDecl]) -> crate::policy::McpReadSet {
    crate::policy::McpReadSet::from_pairs(servers.iter().filter(|s| s.enabled).flat_map(|server| {
        server
            .read_only_tools
            .iter()
            .map(move |tool| (server.name.clone(), tool.clone()))
    }))
}

/// Loads the runtime server index from the secret store. A missing/empty key
/// yields an empty vec; a malformed index is a store error (surfaced, not
/// silently dropped, so corruption is visible).
pub async fn load_runtime_index(
    company: &CompanyId,
    secrets: &dyn SecretStore,
) -> Result<Vec<McpServer>> {
    let Some(SecretValue(raw)) = secrets.get(company, RUNTIME_INDEX_KEY).await? else {
        return Ok(Vec::new());
    };
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&raw)
        .map_err(|e| OpenCompanyError::Store(format!("mcp runtime index is not valid JSON: {e}")))
}

/// Persists the runtime server index.
pub async fn save_runtime_index(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    index: &[McpServer],
) -> Result<()> {
    let raw = serde_json::to_string(index)
        .map_err(|e| OpenCompanyError::Store(format!("serializing mcp runtime index: {e}")))?;
    secrets
        .set(company, RUNTIME_INDEX_KEY, SecretValue(raw))
        .await
}

/// Reads a server's stored credential and resolves it to [`AuthMaterial`].
///
/// The canonical [`auth_key`] (`mcp/{name}/auth`) is tried first — the API
/// (`PUT /mcp/servers/{name}`) writes rotated tokens there. When the canonical
/// key is empty/missing, `override_key` (a manifest server's `auth_secret`)
/// is the fallback for the initial commit-time credential. If neither holds a
/// non-empty value, the result is [`AuthMaterial::None`].
pub async fn load_auth(
    company: &CompanyId,
    name: &str,
    secrets: &dyn SecretStore,
    override_key: Option<&str>,
) -> Result<AuthMaterial> {
    let canonical = auth_key(name);
    // Try the canonical key first — the API writes rotated tokens there.
    let mut raw = None;
    if let Some(SecretValue(r)) = secrets.get(company, &canonical).await?
        && !r.trim().is_empty()
    {
        raw = Some(r);
    }
    // Fall back to the manifest's override key when the canonical key is cold.
    if raw.is_none()
        && let Some(ov) = override_key
        && let Some(SecretValue(r)) = secrets.get(company, ov).await?
    {
        raw = Some(r);
    }
    let Some(raw) = raw else {
        return Ok(AuthMaterial::None);
    };
    if raw.trim().is_empty() {
        return Ok(AuthMaterial::None);
    }
    let stored: StoredAuth = serde_json::from_str(&raw)
        .map_err(|e| OpenCompanyError::Store(format!("mcp auth for `{name}` is not valid: {e}")))?;
    Ok(stored.into())
}

/// Whether a server currently has a credential configured — the non-secret
/// status surfaced by the read APIs. Never returns the value.
pub async fn auth_configured(
    company: &CompanyId,
    server: &McpServer,
    secrets: &dyn SecretStore,
) -> Result<bool> {
    let material = load_auth(
        company,
        &server.name,
        secrets,
        server.auth_secret.as_deref(),
    )
    .await?;
    Ok(material.is_configured())
}

/// Writes a server's outbound credential (write-only intake). The credential is
/// serialized to the canonical [`auth_key`] and never read back out over any
/// API — only [`load_auth`] (harness-build + probe) crosses that boundary.
pub async fn store_auth(
    company: &CompanyId,
    name: &str,
    material: &AuthMaterial,
    secrets: &dyn SecretStore,
) -> Result<()> {
    let stored = match material {
        AuthMaterial::None => {
            // Nothing to store — clear instead so the read-back is "unset".
            return clear_auth(company, name, secrets).await;
        }
        AuthMaterial::Bearer(token) => StoredAuth::Bearer {
            token: token.clone(),
        },
        AuthMaterial::Header { name, value } => StoredAuth::Header {
            name: name.clone(),
            value: value.clone(),
        },
        AuthMaterial::QueryParam { name, value } => StoredAuth::QueryParam {
            name: name.clone(),
            value: value.clone(),
        },
        AuthMaterial::OAuth {
            access_token,
            refresh_token,
            client_id,
            client_secret,
            token_endpoint,
            expires_at,
        } => StoredAuth::Oauth {
            access_token: access_token.clone(),
            refresh_token: refresh_token.clone(),
            client_id: client_id.clone(),
            client_secret: client_secret.clone(),
            token_endpoint: token_endpoint.clone(),
            expires_at: *expires_at,
        },
    };
    let raw = serde_json::to_string(&stored)
        .map_err(|e| OpenCompanyError::Store(format!("serializing mcp auth: {e}")))?;
    secrets
        .set(company, &auth_key(name), SecretValue(raw))
        .await
}

/// Writes a server's bearer token (write-only intake). Thin back-compat wrapper
/// over [`store_auth`]; new callers should build an [`AuthMaterial`] and use
/// [`store_auth`] directly so custom-header / query-param intake share one path.
pub async fn store_bearer(
    company: &CompanyId,
    name: &str,
    token: &str,
    secrets: &dyn SecretStore,
) -> Result<()> {
    store_auth(
        company,
        name,
        &AuthMaterial::Bearer(token.to_string()),
        secrets,
    )
    .await
}

/// Clears a server's stored credential (best-effort — the store has no delete,
/// so an empty value reads back as "not configured").
pub async fn clear_auth(company: &CompanyId, name: &str, secrets: &dyn SecretStore) -> Result<()> {
    secrets
        .set(company, &auth_key(name), SecretValue(String::new()))
        .await
}

/// Clears a server's stored health (best-effort — an empty value reads back as
/// "never probed"). Called when a runtime server is deleted so a later server of
/// the same name never inherits a stale badge.
pub async fn clear_health(
    company: &CompanyId,
    name: &str,
    secrets: &dyn SecretStore,
) -> Result<()> {
    secrets
        .set(company, &health_key(name), SecretValue(String::new()))
        .await
}

/// The coarse health status of an MCP server, shown as the console badge.
///
/// Serialized `snake_case`; the frontend maps it to a green/amber/red tier. Kept
/// deliberately small — a single actionable status plus an operator-facing
/// (scrubbed) message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpStatus {
    /// Reached the server and listed its tools — fully working.
    Ok,
    /// The server is reachable but needs a credential the operator hasn't
    /// supplied (401 with no/rejected credential, or an OAuth challenge). A
    /// valid, expected resting state for a just-added server — never a rollback.
    NeedsConfig,
    /// The server could not be used: unreachable, wrong URL, not an MCP
    /// endpoint, a 5xx, a TLS failure, or a rejected call.
    Error,
    /// The probe did not run (non-`openhuman` build, or never attempted).
    Unknown,
}

impl McpStatus {
    /// The stable wire string.
    pub fn as_str(self) -> &'static str {
        match self {
            McpStatus::Ok => "ok",
            McpStatus::NeedsConfig => "needs_config",
            McpStatus::Error => "error",
            McpStatus::Unknown => "unknown",
        }
    }
}

/// The last probe outcome for one MCP server.
///
/// **Security invariant**: `message` is always scrubbed before it reaches this
/// struct (via [`crate::harness::mcp_probe::scrub`]) and this struct is the only
/// thing [`save_health`] persists — so a credential can never land in the health
/// key, the console, or an API response. `auth_hint` is a stable reason code
/// (`oauth_required` / `token_rejected` / `credential_required`), never a URL or
/// raw challenge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpHealth {
    /// The coarse status tier.
    pub status: McpStatus,
    /// A short, scrubbed, operator-facing message.
    pub message: String,
    /// How many tools the server advertised on a successful probe.
    pub tool_count: u32,
    /// Epoch-millis timestamp of the probe.
    pub checked_at_millis: u64,
    /// Stable auth-failure reason code, when the status is a credential problem.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_hint: Option<String>,
}

/// Loads a server's last recorded health, or `None` when it has never been
/// probed (missing/empty key). A malformed record degrades to `None` rather than
/// erroring — a stale badge is never worth bricking a status read.
pub async fn load_health(
    company: &CompanyId,
    name: &str,
    secrets: &dyn SecretStore,
) -> Result<Option<McpHealth>> {
    let Some(SecretValue(raw)) = secrets.get(company, &health_key(name)).await? else {
        return Ok(None);
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    Ok(serde_json::from_str(&raw).ok())
}

/// Persists a server's probe outcome under [`health_key`].
///
/// The caller is responsible for having scrubbed `health.message` first; this
/// function does not re-scrub (the scrubber needs the known-secret set, which
/// lives at the probe seam). Nothing secret should ever reach here.
pub async fn save_health(
    company: &CompanyId,
    name: &str,
    health: &McpHealth,
    secrets: &dyn SecretStore,
) -> Result<()> {
    let raw = serde_json::to_string(health)
        .map_err(|e| OpenCompanyError::Store(format!("serializing mcp health: {e}")))?;
    secrets
        .set(company, &health_key(name), SecretValue(raw))
        .await
}

/// The company's effective MCP servers with credentials resolved.
///
/// Merges defaults ∪ manifest ∪ runtime index, then fills each decl's
/// [`AuthMaterial`] from its stored secret. This is the single seam the harness
/// builder and the ops discovery route both use so agent-facing resolution and
/// console discovery stay identical.
///
/// `defaults` is the install-wide `[[default_mcp_server]]` list (issue #527),
/// reached at call sites as
/// [`CompanyRuntime::default_mcp_servers`](crate::company::runtime::CompanyRuntime::default_mcp_servers).
/// An install that configures none passes an empty slice, which leaves
/// resolution byte-identical to the two-layer behaviour.
pub async fn resolve_effective(
    company: &CompanyId,
    defaults: &[McpServer],
    manifest: &[McpServer],
    secrets: &dyn SecretStore,
) -> Result<Vec<McpServerDecl>> {
    let runtime = load_runtime_index(company, secrets).await?;
    let mut decls = effective_mcp_servers(defaults, manifest, &runtime);
    for decl in &mut decls {
        // A manifest server may name a custom auth_secret key; runtime servers
        // always use the canonical per-server key. Defaults never carry one —
        // `normalize_default_servers` rejects any entry with a credential-shaped
        // field — so they fall through to the canonical key, which is where the
        // console writes a token the operator adds for a default server later.
        let override_key = manifest
            .iter()
            .find(|m| m.name.trim() == decl.name)
            .and_then(|m| m.auth_secret.clone());
        decl.auth = load_auth(company, &decl.name, secrets, override_key.as_deref()).await?;
        // Never `?`: a caller treats an error out of here as "this company gets
        // no MCP servers", so one unreadable policy key would strip every
        // server from every agent. The gate's face degrades that one server to
        // all-park instead.
        let stored = super::mcp_policy::load_tool_policies(
            company,
            secrets,
            &super::mcp_policy::tool_policies_key(&decl.name),
        )
        .await;
        decl.tool_policies = super::mcp_policy::effective_policies(&decl.read_only_tools, stored);
        // Threaded here, before any route can store a tier default. Without it
        // a tier default resolves against nothing: the tool is never
        // enumerated, so the operator's decision is silently not enforced.
        decl.tool_inventory = super::mcp_policy::load_tool_inventory(
            company,
            secrets,
            &super::mcp_policy::tool_inventory_key(&decl.name),
        )
        .await;
    }
    Ok(decls)
}

/// Validates a set of MCP server declarations, returning every problem in
/// prosumer language. Enforces unique names, an `http(s)://` endpoint, and the
/// hosted-v1 no-stdio boundary. Shared by manifest validation and the ops
/// add/update routes.
pub fn validate_servers(servers: &[McpServer]) -> Vec<String> {
    let mut problems = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (index, server) in servers.iter().enumerate() {
        let name = server.name.trim();
        let label = if name.is_empty() {
            format!("mcp server #{}", index + 1)
        } else {
            format!("mcp server `{name}`")
        };
        problems.extend(validate_one(&label, server));
        if !name.is_empty() && !seen.insert(name.to_string()) {
            problems.push(format!(
                "mcp server `name` `{name}` is used more than once — names must be unique."
            ));
        }
    }
    problems
}

/// Server names a company may not declare: the runtime merges servers by
/// name, last write wins, so a company server under one of these would stand
/// in for OpenCompany's own MCP server or OpenHuman's docs server.
pub const RESERVED_SERVER_NAMES: &[&str] = &["opencompany", "gitbooks"];

/// Validates a single server declaration under a caller-supplied `label`.
pub fn validate_one(label: &str, server: &McpServer) -> Vec<String> {
    let mut problems = Vec::new();
    let name = server.name.trim();
    let endpoint = server.endpoint.trim();

    if name.is_empty() {
        problems.push(format!("{label} is missing a `name`."));
    } else if let Some(reserved) = RESERVED_SERVER_NAMES
        .iter()
        .find(|reserved| name.eq_ignore_ascii_case(reserved))
    {
        problems.push(format!(
            "{label} uses the name `{reserved}`, which is reserved for a server OpenCompany runs itself — choose another name."
        ));
    }

    if server
        .command
        .as_deref()
        .is_some_and(|c| !c.trim().is_empty())
    {
        problems.push(format!(
            "{label} sets a stdio `command`, which is not supported in hosted v1 — declare an HTTP `endpoint` instead."
        ));
    }

    if endpoint.is_empty() {
        problems.push(format!(
            "{label} is missing an `endpoint` — an MCP server needs an `http(s)://` URL."
        ));
    } else if !is_http_url(endpoint) {
        problems.push(format!(
            "{label} `endpoint` must be an `http://` or `https://` URL — you wrote `{endpoint}`."
        ));
    } else if has_userinfo(endpoint) {
        // A `user:pass@host` endpoint smuggles a credential into the URL, which
        // then leaks into every log line and transport error. Reject it — the
        // operator should use a token / custom-header / query-parameter
        // credential (stored write-only) instead.
        problems.push(format!(
            "{label} `endpoint` must not embed credentials in the URL (the `user:pass@host` form) — leave the endpoint credential-free and set a token or query-parameter credential instead."
        ));
    }

    problems
}

/// True when `url` is an absolute `http://` or `https://` URL.
fn is_http_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// True when `url`'s query string carries something credential-shaped.
///
/// Distinct from [`has_userinfo`], which catches the `user:pass@host` form. A
/// token in a query parameter is the other way a credential reaches a URL, and
/// it is the shape a default is most likely to arrive in — an operator copying
/// a "your MCP URL" string out of a vendor dashboard.
pub(crate) fn has_query_credential(url: &str) -> bool {
    let Some(query) = url.split_once('?').map(|(_, q)| q) else {
        return false;
    };
    query.split('&').any(|pair| {
        // Decode before matching: `api%4Bey` is `apiKey`, and matching the raw
        // key would let an encoded credential name past the block to ship a
        // token-bearing default to every company (CWE-200).
        let key = percent_decode(pair.split('=').next().unwrap_or(""));
        let key = key.trim().to_ascii_lowercase().replace(['-', '_'], "");
        matches!(
            key.as_str(),
            "apikey" | "token" | "accesstoken" | "secret" | "password" | "auth" | "authorization"
        )
    })
}

/// Percent-decodes `s`, turning each `%XX` escape into its byte.
///
/// Inline rather than pulled from the `url` crate because this module compiles
/// in the default build, where `url` is gated behind the `mcp` feature. Only
/// used for the query-key scan above, so `+` is left literal (it has no
/// meaning in a parameter *name*) and a malformed or truncated escape is
/// passed through untouched rather than rejected.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]))
        {
            out.push(hi << 4 | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    // Every `%XX` decodes to fewer bytes than it occupies, so lossy UTF-8
    // repair here can only shorten the input — never invent characters.
    String::from_utf8_lossy(&out).into_owned()
}

/// The nibble a hex digit encodes.
fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

/// Normalizes the install-wide `[[default_mcp_server]]` list (issue #527) into
/// the entries that may actually ship, dropping each one that cannot.
///
/// # Why entries are dropped rather than the list rejected
///
/// These servers auto-enable on every company of the install with no user
/// action, so one malformed row must not cost an operator the rows that are
/// fine — and it must not abort boot either, which would turn a typo in a
/// packaged config into an install that does not start. Every rejection is
/// returned alongside the survivors so the caller can log it: a default that
/// silently fails to ship looks exactly like one nobody configured.
///
/// # Why a credential is refused rather than stripped
///
/// A default is handed to every agent on the install unprompted, so it must be
/// safe unattended: public, and carrying no secret. An entry with a token in its
/// endpoint's query string, or an `auth_secret` naming a key, is **rejected, not
/// scrubbed** — scrubbing would ship a server whose auth silently no longer
/// works, which is worse than not shipping it, because it fails at an agent's
/// first tool call instead of here. A server that needs auth is added per
/// company at runtime, where its token goes to that company's own secret store.
///
/// [`McpServer`] is a typed struct with no free-form fields, so the two above
/// are the whole surface: an inline `token = "…"` in the TOML cannot reach a
/// field and is dropped by deserialization. That is why this does not carry the
/// dynamic "is any key credential-shaped?" scan a schemaless config would need —
/// the type already provides it.
///
/// The shared rules (name, `http(s)` endpoint, no stdio `command`, no
/// `user:pass@` userinfo) come from [`validate_one`], so defaults and every
/// other declaration path stay on one validator.
pub fn normalize_default_servers(raw: &[McpServer]) -> (Vec<McpServer>, Vec<String>) {
    let mut kept: Vec<McpServer> = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for (index, server) in raw.iter().enumerate() {
        let name = server.name.trim();
        let label = if name.is_empty() {
            format!("default mcp server #{}", index + 1)
        } else {
            format!("default mcp server `{name}`")
        };

        let shared = validate_one(&label, server);
        if !shared.is_empty() {
            problems.extend(shared);
            continue;
        }

        if has_query_credential(&server.endpoint) {
            problems.push(format!(
                "{label} has a credential in its `endpoint` query string — a default ships to every company unattended and must carry no secret. Add it per company from the console instead."
            ));
            continue;
        }

        if server
            .auth_secret
            .as_deref()
            .is_some_and(|k| !k.trim().is_empty())
        {
            problems.push(format!(
                "{label} names an `auth_secret` — a default must not depend on a credential. Declare it in the company's `company.toml`, or add it from the console, where the token is stored per company."
            ));
            continue;
        }

        // A duplicate name would put two rows in the list claiming one slug, and
        // which won would depend on merge order rather than on this config.
        if !seen.insert(name.to_string()) {
            problems.push(format!(
                "{label} repeats a `name` used earlier in the list — keeping the first."
            ));
            continue;
        }

        kept.push(server.clone());
    }

    (kept, problems)
}

/// Whether an endpoint's authority carries a `user[:pass]@` userinfo section.
/// Uses the same cheap authority-splitting as [`crate::harness::mcp_probe::scrub`]
/// so a `?email=a@b` query never trips it.
fn has_userinfo(url: &str) -> bool {
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    authority.contains('@')
}

/// A **non-blocking** advisory when an endpoint's query string carries a
/// key-ish parameter (`apiKey` / `token` / `secret` / …).
///
/// This is not an error: some providers legitimately put a *non-secret* id in
/// the URL (BrowserBase's `projectId`). But a real secret in the endpoint URL
/// leaks into logs and transport errors, so the ops layer surfaces this as a
/// gentle nudge toward the write-only query-parameter credential intake. Returns
/// `None` when nothing key-ish is present.
pub fn endpoint_secret_advisory(endpoint: &str) -> Option<String> {
    let (_, query) = endpoint.split_once('?')?;
    const KEYISH: [&str; 8] = [
        "apikey", "token", "secret", "password", "passwd", "access", "auth", "key",
    ];
    let hit = query
        .split(['&', ';'])
        .filter_map(|kv| kv.split('=').next())
        .any(|param| {
            let param = param.trim().to_ascii_lowercase();
            KEYISH.iter().any(|needle| param.contains(needle))
        });
    hit.then(|| {
        "the endpoint URL looks like it carries a secret in its query string — a credential in the URL can leak into logs and errors, so prefer the write-only query-parameter credential (only a non-secret id like a project id belongs in the URL)."
            .to_string()
    })
}

/// De-dupes and trims a tool-name list, dropping blanks.
fn normalize_tools(tools: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tool in tools {
        let tool = tool.trim();
        if !tool.is_empty() && !out.iter().any(|existing| existing == tool) {
            out.push(tool.to_string());
        }
    }
    out
}

#[cfg(test)]
#[path = "mcp_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "mcp_store_tests.rs"]
mod store_tests;
