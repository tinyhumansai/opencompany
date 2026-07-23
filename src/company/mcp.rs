//! Per-tenant MCP tool servers: the inert, ungated data model plus the pure
//! merge/validation and the async secret-resolution used to materialize a
//! company's *effective* MCP servers (issue #50).
//!
//! A company's effective MCP servers are the union of two sources:
//!
//! 1. **Manifest** — the `[[mcp_server]]` entries committed in `company.toml`
//!    ([`McpServer`]). Declarative intent; never a credential.
//! 2. **Runtime** — servers the operator adds through the console, persisted as
//!    a single JSON index in the [`SecretStore`](crate::ports::SecretStore)
//!    under [`RUNTIME_INDEX_KEY`]. A runtime entry with the *same name* as a
//!    manifest server is an **override** (enable/disable, tool allow-list).
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
/// badge and the delete-guard (a manifest server cannot be deleted, only
/// disabled/overridden).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpSource {
    /// Declared in `company.toml`'s `[[mcp_server]]`.
    Manifest,
    /// Added at runtime through the console.
    Runtime,
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
        }
    }
}

/// The on-disk credential envelope stored under [`auth_key`]. Kept private —
/// only [`resolve_effective`] / [`store_bearer`] cross this boundary.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredAuth {
    Bearer { token: String },
    Header { name: String, value: String },
    QueryParam { name: String, value: String },
}

impl From<StoredAuth> for AuthMaterial {
    fn from(stored: StoredAuth) -> Self {
        match stored {
            StoredAuth::Bearer { token } => AuthMaterial::Bearer(token),
            StoredAuth::Header { name, value } => AuthMaterial::Header { name, value },
            StoredAuth::QueryParam { name, value } => AuthMaterial::QueryParam { name, value },
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
    /// Per-request timeout in seconds.
    pub timeout_secs: u64,
    /// Whether this server is exposed to agents.
    pub enabled: bool,
    /// Manifest vs runtime provenance.
    pub source: McpSource,
    /// Resolved outbound credential (`None` until [`resolve_effective`] fills it).
    pub auth: AuthMaterial,
}

impl McpServerDecl {
    fn from_server(server: &McpServer, source: McpSource) -> Self {
        Self {
            name: server.name.trim().to_string(),
            endpoint: server.endpoint.trim().to_string(),
            description: server.description.clone(),
            allowed_tools: normalize_tools(&server.allowed_tools),
            disallowed_tools: normalize_tools(&server.disallowed_tools),
            timeout_secs: server.timeout_secs,
            enabled: server.enabled,
            source,
            auth: AuthMaterial::None,
        }
    }
}

/// Merges the manifest servers with the runtime index into the effective set.
///
/// A runtime entry overrides a manifest server of the same name (its
/// enable/disable + tool lists win) but keeps the [`McpSource::Manifest`] badge
/// so the console still shows it as manifest-declared (and refuses to delete
/// it). Runtime-only entries append as [`McpSource::Runtime`]. Order is manifest
/// first (in declared order), then any runtime-only additions.
///
/// Auth is left [`AuthMaterial::None`]; [`resolve_effective`] fills it.
pub fn effective_mcp_servers(manifest: &[McpServer], runtime: &[McpServer]) -> Vec<McpServerDecl> {
    let mut out: Vec<McpServerDecl> = Vec::new();

    for m in manifest {
        let name = m.name.trim();
        if name.is_empty() {
            continue;
        }
        // A runtime override for this manifest name replaces the body but keeps
        // the manifest provenance.
        let decl = match runtime.iter().find(|r| r.name.trim() == name) {
            Some(override_entry) => McpServerDecl::from_server(override_entry, McpSource::Manifest),
            None => McpServerDecl::from_server(m, McpSource::Manifest),
        };
        out.push(decl);
    }

    for r in runtime {
        let name = r.name.trim();
        if name.is_empty() || manifest.iter().any(|m| m.name.trim() == name) {
            continue;
        }
        out.push(McpServerDecl::from_server(r, McpSource::Runtime));
    }

    out
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
/// Merges manifest ∪ runtime index, then fills each decl's [`AuthMaterial`] from
/// its stored secret. This is the single seam the harness builder and the ops
/// discovery route both use so agent-facing resolution and console discovery
/// stay identical.
pub async fn resolve_effective(
    company: &CompanyId,
    manifest: &[McpServer],
    secrets: &dyn SecretStore,
) -> Result<Vec<McpServerDecl>> {
    let runtime = load_runtime_index(company, secrets).await?;
    let mut decls = effective_mcp_servers(manifest, &runtime);
    for decl in &mut decls {
        // A manifest server may name a custom auth_secret key; runtime servers
        // always use the canonical per-server key.
        let override_key = manifest
            .iter()
            .find(|m| m.name.trim() == decl.name)
            .and_then(|m| m.auth_secret.clone());
        decl.auth = load_auth(company, &decl.name, secrets, override_key.as_deref()).await?;
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

/// Validates a single server declaration under a caller-supplied `label`.
pub fn validate_one(label: &str, server: &McpServer) -> Vec<String> {
    let mut problems = Vec::new();
    let name = server.name.trim();
    let endpoint = server.endpoint.trim();

    if name.is_empty() {
        problems.push(format!("{label} is missing a `name`."));
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
mod tests {
    use super::*;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use std::collections::HashMap;

    fn server(name: &str, endpoint: &str) -> McpServer {
        McpServer {
            name: name.to_string(),
            endpoint: endpoint.to_string(),
            description: None,
            command: None,
            allowed_tools: Vec::new(),
            disallowed_tools: Vec::new(),
            timeout_secs: 30,
            enabled: true,
            auth_secret: None,
        }
    }

    // ---- merge precedence -------------------------------------------------

    #[test]
    fn effective_unions_manifest_and_runtime() {
        let manifest = vec![server("notion", "https://notion.example/mcp")];
        let runtime = vec![server("linear", "https://linear.example/mcp")];
        let eff = effective_mcp_servers(&manifest, &runtime);
        let names: Vec<&str> = eff.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["notion", "linear"]);
        assert_eq!(eff[0].source, McpSource::Manifest);
        assert_eq!(eff[1].source, McpSource::Runtime);
    }

    #[test]
    fn runtime_overrides_manifest_but_keeps_manifest_source() {
        let manifest = vec![server("notion", "https://notion.example/mcp")];
        let mut override_entry = server("notion", "https://notion.example/mcp");
        override_entry.enabled = false;
        override_entry.allowed_tools = vec!["search".into()];
        let eff = effective_mcp_servers(&manifest, &[override_entry]);
        assert_eq!(eff.len(), 1, "override does not duplicate the server");
        assert_eq!(eff[0].source, McpSource::Manifest, "still manifest-badged");
        assert!(!eff[0].enabled, "override wins the enabled flag");
        assert_eq!(eff[0].allowed_tools, vec!["search".to_string()]);
    }

    // ---- validation -------------------------------------------------------

    #[test]
    fn valid_http_server_passes() {
        assert!(validate_servers(&[server("notion", "https://notion.example/mcp")]).is_empty());
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let problems = validate_servers(&[
            server("dup", "https://a.example/mcp"),
            server("dup", "https://b.example/mcp"),
        ]);
        assert!(
            problems.iter().any(|p| p.contains("more than once")),
            "{problems:?}"
        );
    }

    #[test]
    fn non_http_endpoint_is_rejected() {
        let problems = validate_servers(&[server("bad", "ftp://x.example/mcp")]);
        assert!(problems.iter().any(|p| p.contains("http")), "{problems:?}");
    }

    #[test]
    fn missing_endpoint_is_rejected() {
        let problems = validate_servers(&[server("bare", "")]);
        assert!(
            problems.iter().any(|p| p.contains("endpoint")),
            "{problems:?}"
        );
    }

    #[test]
    fn stdio_command_is_rejected_in_hosted_v1() {
        let mut s = server("local", "https://x.example/mcp");
        s.command = Some("npx some-mcp".into());
        let problems = validate_servers(&[s]);
        assert!(
            problems
                .iter()
                .any(|p| p.contains("stdio") && p.contains("hosted v1")),
            "{problems:?}"
        );
    }

    // ---- secret resolution (write-only auth) ------------------------------

    #[derive(Default)]
    struct MemSecrets {
        map: Mutex<HashMap<String, String>>,
    }

    #[async_trait]
    impl SecretStore for MemSecrets {
        async fn get(&self, _c: &CompanyId, key: &str) -> Result<Option<SecretValue>> {
            Ok(self
                .map
                .lock()
                .unwrap()
                .get(key)
                .map(|v| SecretValue(v.clone())))
        }
        async fn set(&self, _c: &CompanyId, key: &str, value: SecretValue) -> Result<()> {
            self.map.lock().unwrap().insert(key.to_string(), value.0);
            Ok(())
        }
    }

    #[tokio::test]
    async fn resolve_effective_fills_bearer_and_index_roundtrips() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();

        // Runtime-add a server + write its token (write-only).
        save_runtime_index(
            &company,
            &secrets,
            &[server("notion", "https://notion.example/mcp")],
        )
        .await
        .unwrap();
        store_bearer(&company, "notion", "sk-secret-123", &secrets)
            .await
            .unwrap();

        let decls = resolve_effective(&company, &[], &secrets).await.unwrap();
        assert_eq!(decls.len(), 1);
        assert_eq!(decls[0].auth, AuthMaterial::Bearer("sk-secret-123".into()));
        assert_eq!(decls[0].source, McpSource::Runtime);

        // The token is never exposed by the status helper — only a bool.
        assert!(
            auth_configured(
                &company,
                &server("notion", "https://notion.example/mcp"),
                &secrets
            )
            .await
            .unwrap()
        );
    }

    #[tokio::test]
    async fn cleared_auth_reads_back_as_unconfigured() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        store_bearer(&company, "notion", "tok", &secrets)
            .await
            .unwrap();
        clear_auth(&company, "notion", &secrets).await.unwrap();
        let material = load_auth(&company, "notion", &secrets, None).await.unwrap();
        assert_eq!(material, AuthMaterial::None);
    }

    // ---- query-param auth (BrowserBase style) -----------------------------

    #[tokio::test]
    async fn store_and_resolve_query_param_auth_round_trips() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        save_runtime_index(
            &company,
            &secrets,
            &[server(
                "browserbase",
                "https://api.browserbase.com/mcp?projectId=pid",
            )],
        )
        .await
        .unwrap();
        store_auth(
            &company,
            "browserbase",
            &AuthMaterial::QueryParam {
                name: "apiKey".into(),
                value: "qp-secret".into(),
            },
            &secrets,
        )
        .await
        .unwrap();

        let decls = resolve_effective(&company, &[], &secrets).await.unwrap();
        assert_eq!(
            decls[0].auth,
            AuthMaterial::QueryParam {
                name: "apiKey".into(),
                value: "qp-secret".into(),
            }
        );
        // The non-secret project id stays in the endpoint URL, unchanged.
        assert!(decls[0].endpoint.contains("projectId=pid"));
    }

    #[test]
    fn secret_values_lists_the_credential_for_scrubbing() {
        assert_eq!(
            AuthMaterial::Bearer("tok".into()).secret_values(),
            vec!["tok".to_string()]
        );
        assert_eq!(
            AuthMaterial::QueryParam {
                name: "apiKey".into(),
                value: "qp".into(),
            }
            .secret_values(),
            vec!["qp".to_string()]
        );
        assert!(AuthMaterial::None.secret_values().is_empty());
    }

    // ---- health persistence -----------------------------------------------

    #[tokio::test]
    async fn health_round_trips_and_clears() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        assert_eq!(
            load_health(&company, "notion", &secrets).await.unwrap(),
            None
        );

        let health = McpHealth {
            status: McpStatus::Ok,
            message: "8 tools available".into(),
            tool_count: 8,
            checked_at_millis: 123,
            auth_hint: None,
        };
        save_health(&company, "notion", &health, &secrets)
            .await
            .unwrap();
        assert_eq!(
            load_health(&company, "notion", &secrets).await.unwrap(),
            Some(health)
        );

        clear_health(&company, "notion", &secrets).await.unwrap();
        assert_eq!(
            load_health(&company, "notion", &secrets).await.unwrap(),
            None
        );
    }

    // ---- endpoint validation ----------------------------------------------

    #[test]
    fn userinfo_endpoint_is_rejected() {
        let problems = validate_servers(&[server("creds", "https://user:pass@host/mcp")]);
        assert!(
            problems
                .iter()
                .any(|p| p.contains("must not embed credentials")),
            "{problems:?}"
        );
    }

    #[test]
    fn email_in_query_is_not_mistaken_for_userinfo() {
        // The '@' lives in the query, not the authority — must stay valid.
        assert!(validate_servers(&[server("ok", "https://host/mcp?to=a@b.com")]).is_empty());
    }

    #[test]
    fn secret_in_query_is_a_non_blocking_advisory() {
        // A key-ish query param yields an advisory but NOT a validation error.
        assert!(endpoint_secret_advisory("https://host/mcp?apiKey=sk-123").is_some());
        assert!(
            validate_servers(&[server("browserbase", "https://host/mcp?apiKey=sk-123")]).is_empty()
        );
        // A non-secret id (BrowserBase's projectId) is fine — no advisory.
        assert!(endpoint_secret_advisory("https://host/mcp?projectId=pid").is_none());
        // No query string at all — no advisory.
        assert!(endpoint_secret_advisory("https://host/mcp").is_none());
    }
}
