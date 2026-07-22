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
}

impl AuthMaterial {
    /// Whether any credential is configured (for the non-secret
    /// `auth_configured` status field). Never reveals the value.
    pub fn is_configured(&self) -> bool {
        !matches!(self, AuthMaterial::None)
    }
}

/// The on-disk credential envelope stored under [`auth_key`]. Kept private —
/// only [`resolve_effective`] / [`store_bearer`] cross this boundary.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredAuth {
    Bearer { token: String },
    Header { name: String, value: String },
}

impl From<StoredAuth> for AuthMaterial {
    fn from(stored: StoredAuth) -> Self {
        match stored {
            StoredAuth::Bearer { token } => AuthMaterial::Bearer(token),
            StoredAuth::Header { name, value } => AuthMaterial::Header { name, value },
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
/// `override_key` (a manifest server's `auth_secret`) is consulted first; the
/// canonical [`auth_key`] is the fallback. A missing/empty value resolves to
/// [`AuthMaterial::None`].
pub async fn load_auth(
    company: &CompanyId,
    name: &str,
    secrets: &dyn SecretStore,
    override_key: Option<&str>,
) -> Result<AuthMaterial> {
    let canonical = auth_key(name);
    let key = override_key.unwrap_or(&canonical);
    let Some(SecretValue(raw)) = secrets.get(company, key).await? else {
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

/// Writes a server's bearer token (write-only intake).
pub async fn store_bearer(
    company: &CompanyId,
    name: &str,
    token: &str,
    secrets: &dyn SecretStore,
) -> Result<()> {
    let raw = serde_json::to_string(&StoredAuth::Bearer {
        token: token.to_string(),
    })
    .map_err(|e| OpenCompanyError::Store(format!("serializing mcp auth: {e}")))?;
    secrets
        .set(company, &auth_key(name), SecretValue(raw))
        .await
}

/// Clears a server's stored credential (best-effort — the store has no delete,
/// so an empty value reads back as "not configured").
pub async fn clear_auth(company: &CompanyId, name: &str, secrets: &dyn SecretStore) -> Result<()> {
    secrets
        .set(company, &auth_key(name), SecretValue(String::new()))
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
    }

    problems
}

/// True when `url` is an absolute `http://` or `https://` URL.
fn is_http_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
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
}
