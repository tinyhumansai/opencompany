//! Per-tenant MCP server management (issue #50): list / add / update / remove
//! the company's MCP tool servers, and (under the `openhuman` feature) live-
//! discover a server's tools.
//!
//! The effective set is the company's `[[mcp_server]]` manifest entries unioned
//! with a runtime index the console writes into
//! [`SecretStore`](crate::ports::SecretStore) (`mcp/servers`). A server's
//! outbound credential lives apart under `mcp/{name}/auth` and is **write-only**
//! over the API: it is set through `token`, stored in the secret store, and
//! never echoed back — the read shape carries only an `authConfigured` bool.
//!
//! Both scope forms (`…/companies/{id}` and the single-company alias `…/company`)
//! are registered by [`scoped`]. Agents pick up a change on their next turn with
//! no restart; every mutating response says so via `note`.

use axum::Router;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, response::Response};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::McpServer;
use crate::company::mcp::{
    self, AuthMaterial, McpHealth, McpSource, clear_auth, clear_health, endpoint_secret_advisory,
    load_health, load_runtime_index, resolve_effective, save_runtime_index, store_auth,
    validate_one,
};
use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::ports::types::CompanyRecord;
use crate::runtime::builder::agent_effective_grants;
use crate::runtime::tools::grants_cover_server;
use crate::server::error::ApiError;
use crate::server::ops::{AdminScopedCompany, ScopedCompany, scoped};

/// The reminder attached to every mutating response: the effective MCP set is
/// re-resolved and fingerprinted on every harness cycle (`HarnessPool::ensure`),
/// so an edit reaches agents on the company's next turn with no restart. The
/// `mcp_fingerprint` staleness term is what makes this a property of the design.
const NEXT_TURN_NOTE: &str = "Agents pick up this change on their next turn — no restart needed.";

/// Builds the MCP server management route fragment.
pub fn router() -> Router<AppState> {
    scoped("/mcp/servers", post(add_server).get(list_servers))
        .merge(scoped(
            "/mcp/servers/{name}",
            put(update_server).delete(delete_server),
        ))
        .merge(scoped("/mcp/servers/{name}/tools", get(discover_tools)))
        .merge(scoped("/mcp/servers/{name}/test", post(test_server)))
        .merge(scoped("/mcp/servers/{name}/oauth/start", post(start_oauth)))
}

/// One effective MCP server as the console renders it. **Never** carries a
/// credential — only a non-secret `authConfigured` flag and the last (scrubbed)
/// probe `health`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct McpServerDto {
    name: String,
    endpoint: String,
    description: Option<String>,
    /// `manifest` (committed), `runtime` (console-added), or `default`
    /// (shipped by the install — issue #527). The console renders this as the
    /// source badge, so the three stay distinguishable: a shipped default is
    /// not something this operator added, and must not be labelled as if it
    /// were.
    source: McpSource,
    enabled: bool,
    allowed_tools: Vec<String>,
    disallowed_tools: Vec<String>,
    timeout_secs: u64,
    /// Whether an outbound credential is stored — never the credential itself.
    auth_configured: bool,
    /// The ids of the company's agents whose effective tool grants cover this
    /// server — who can actually call it (issue #568). Computed over the same
    /// roster the harness builds (manifest agents + promoted overlay teammates),
    /// through the shared
    /// [`grants_cover_server`](crate::runtime::tools::grants_cover_server), so the
    /// console cannot disagree with the harness about reachability. **An empty
    /// list is meaningful**: an *enabled*, healthy server no teammate can reach
    /// is almost always a misconfiguration, and the console flags it rather than
    /// showing an empty list silently. A **disabled** server is always empty —
    /// the harness hands out no tool for it whatever the grants say — so the
    /// console reads the empty case against `enabled` and stays quiet there.
    /// Always serialized (even when empty).
    reachable_by: Vec<String>,
    /// The last recorded probe outcome (scrubbed), or `None` when never probed.
    #[serde(skip_serializing_if = "Option::is_none")]
    health: Option<McpHealth>,
}

/// A mutating response: the resulting server, the rebuild reminder, the live
/// probe result (`None` on a non-`openhuman` build), and any non-blocking
/// endpoint advisory.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MutationResponse {
    server: McpServerDto,
    note: String,
    /// The result of probing the server right after the mutation. `None` when
    /// probing isn't wired (default build). The server is **never** rolled back
    /// on a failed probe — a needs-config result is a valid resting state.
    #[serde(skip_serializing_if = "Option::is_none")]
    test: Option<McpHealth>,
    /// A non-blocking advisory (e.g. a secret-looking query string in the URL).
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
}

/// The auth scheme an intake body selects. `bearer` (default) uses `token` as
/// an `Authorization: Bearer`; `header` uses `headerName` + `token`;
/// `query_param` uses `paramName` + `token` (the BrowserBase style).
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum AuthKind {
    #[default]
    Bearer,
    Header,
    QueryParam,
}

/// Add-server body. Credential fields are write-only intake.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddServer {
    name: String,
    endpoint: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    allowed_tools: Vec<String>,
    #[serde(default)]
    disallowed_tools: Vec<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    /// The outbound credential value, stored write-only. Omit to leave auth
    /// unset. Interpreted per [`AuthKind`].
    #[serde(default)]
    token: Option<String>,
    /// The auth scheme; defaults to `bearer` (back-compat — a bare `token` is a
    /// bearer token exactly as before).
    #[serde(default)]
    auth_kind: AuthKind,
    /// The header name, when `authKind == header`.
    #[serde(default)]
    header_name: Option<String>,
    /// The query-parameter name, when `authKind == query_param`.
    #[serde(default)]
    param_name: Option<String>,
}

/// Update-server body — every field optional (only set fields are applied).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateServer {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    allowed_tools: Option<Vec<String>>,
    #[serde(default)]
    disallowed_tools: Option<Vec<String>>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    /// Rotate the outbound credential (write-only). Omit to leave it unchanged.
    #[serde(default)]
    token: Option<String>,
    /// The auth scheme for a rotated credential; defaults to `bearer`.
    #[serde(default)]
    auth_kind: AuthKind,
    /// The header name, when `authKind == header`.
    #[serde(default)]
    header_name: Option<String>,
    /// The query-parameter name, when `authKind == query_param`.
    #[serde(default)]
    param_name: Option<String>,
}

/// Builds the [`AuthMaterial`] a write-only intake describes, or `None` when no
/// credential value was supplied (leave auth unchanged). Returns a 400 when a
/// scheme is missing its companion field.
fn auth_material_from(
    token: Option<&str>,
    kind: AuthKind,
    header_name: Option<&str>,
    param_name: Option<&str>,
) -> Result<Option<AuthMaterial>, ApiError> {
    let Some(value) = non_empty(token) else {
        return Ok(None);
    };
    let value = value.to_string();
    let material = match kind {
        AuthKind::Bearer => AuthMaterial::Bearer(value),
        AuthKind::Header => {
            let name = non_empty(header_name).ok_or_else(|| {
                ApiError(OpenCompanyError::InvalidRequest(
                    "a custom-header credential needs a `headerName`.".to_string(),
                ))
            })?;
            AuthMaterial::Header {
                name: name.to_string(),
                value,
            }
        }
        AuthKind::QueryParam => {
            let name = non_empty(param_name).ok_or_else(|| {
                ApiError(OpenCompanyError::InvalidRequest(
                    "a query-parameter credential needs a `paramName`.".to_string(),
                ))
            })?;
            AuthMaterial::QueryParam {
                name: name.to_string(),
                value,
            }
        }
    };
    Ok(Some(material))
}

/// The sub-resource path (`name`).
#[derive(Debug, Deserialize)]
struct NamePath {
    name: String,
}

/// Loads the company's committed `[[mcp_server]]` entries from its record.
async fn manifest_servers(runtime: &CompanyRuntime) -> Result<Vec<McpServer>, ApiError> {
    let record = runtime.store().load(runtime.id()).await.map_err(ApiError)?;
    Ok(record.map(|r| r.manifest.mcp_servers).unwrap_or_default())
}

/// Projects an effective decl (already merged + auth-resolved) to the console
/// DTO, reducing the resolved credential to a boolean, listing the agents that
/// can reach it (issue #568), and attaching the last (scrubbed) probe health.
fn dto_from_decl(
    decl: &mcp::McpServerDecl,
    reachable_by: Vec<String>,
    health: Option<McpHealth>,
) -> McpServerDto {
    McpServerDto {
        name: decl.name.clone(),
        endpoint: decl.endpoint.clone(),
        description: decl.description.clone(),
        source: decl.source,
        enabled: decl.enabled,
        allowed_tools: decl.allowed_tools.clone(),
        disallowed_tools: decl.disallowed_tools.clone(),
        timeout_secs: decl.timeout_secs,
        auth_configured: decl.auth.is_configured(),
        reachable_by,
        health,
    }
}

/// Every roster agent's *effective* tool grants (issue #568), as
/// `(agent_id, grants)`. The roster is exactly what the harness builds in
/// `build_roster`: the manifest agents (each with its own `tools` narrowed by
/// the company `allow`), plus the promoted overlay teammates — each with **its
/// own** `tools` line narrowed the same way (issue #619), which for the
/// overwhelmingly common empty line is still the full company `allow`, the
/// standard grant `overlay_agent_to_manifest` gives it. An overlay id already
/// claimed by a manifest agent is skipped, both mirroring the harness so console
/// reachability equals what an agent is actually granted.
fn roster_grants(record: &CompanyRecord) -> Vec<(String, Vec<String>)> {
    let allow = &record.manifest.tools.allow;
    let mut grants: Vec<(String, Vec<String>)> = record
        .manifest
        .agents
        .iter()
        .map(|agent| {
            (
                agent.id.clone(),
                agent_effective_grants(allow, &agent.tools),
            )
        })
        .collect();
    let manifest_ids: std::collections::HashSet<&str> = record
        .manifest
        .agents
        .iter()
        .map(|agent| agent.id.as_str())
        .collect();
    for overlay in &record.overlay_agents {
        if manifest_ids.contains(overlay.id.as_str()) {
            continue;
        }
        // The overlay teammate's own tools line (issue #619), read through the
        // same function and with the same empty-means-inherit rule as a
        // manifest row above — matching `overlay_agent_to_manifest`. A scoped
        // teammate must not read back here as holding every granted server.
        grants.push((
            overlay.id.clone(),
            agent_effective_grants(allow, &overlay.tools),
        ));
    }
    grants
}

/// The ids of the agents whose effective `grants` reach `decl` (issue #568),
/// read through the shared [`grants_cover_server`] so this agrees with the
/// harness registry. Empty ⇒ no teammate can reach the server.
///
/// A **disabled** server reaches nobody regardless of grants: `registry_for_agent`
/// filters on `decl.enabled && grants_cover_server(..)`, so an agent granted
/// `mcp:<slug>` still gets no such tool while the server is off. Mirroring both
/// halves of that filter here is what keeps the console from claiming a
/// reachability the harness does not hand out.
fn reachers_of(roster_grants: &[(String, Vec<String>)], decl: &mcp::McpServerDecl) -> Vec<String> {
    if !decl.enabled {
        return Vec::new();
    }
    roster_grants
        .iter()
        .filter(|(_, grants)| grants_cover_server(grants, &decl.name))
        .map(|(id, _)| id.clone())
        .collect()
}

/// `GET …/mcp/servers` — the company's effective MCP servers, each with its last
/// recorded (scrubbed) probe health.
async fn list_servers(company: ScopedCompany) -> Result<Json<Vec<McpServerDto>>, ApiError> {
    let runtime = company.runtime.as_ref();
    // One record load feeds both the manifest servers (merged into the effective
    // set) and the roster used for reachability (issue #568), rather than loading
    // it twice. The install-wide defaults (issue #527) are the layer *underneath*
    // the manifest, so they come off the runtime rather than the record.
    let record = runtime.store().load(runtime.id()).await.map_err(ApiError)?;
    let manifest = record
        .as_ref()
        .map(|r| r.manifest.mcp_servers.clone())
        .unwrap_or_default();
    let decls = resolve_effective(
        runtime.id(),
        runtime.default_mcp_servers(),
        &manifest,
        runtime.secrets().as_ref(),
    )
    .await
    .map_err(ApiError)?;
    // Resolve every agent's effective grants once, then ask per server who is
    // covered — the wildcard-heavy work happens N(agents) times, not N×M.
    let grants = record.as_ref().map(roster_grants).unwrap_or_default();
    let mut out = Vec::with_capacity(decls.len());
    for decl in &decls {
        let health = load_health(runtime.id(), &decl.name, runtime.secrets().as_ref())
            .await
            .map_err(ApiError)?;
        out.push(dto_from_decl(decl, reachers_of(&grants, decl), health));
    }
    Ok(Json(out))
}

/// `POST …/mcp/servers` — add a runtime MCP server (+ optional token).
///
/// Requires authority over the company (issue #403). Registering a server hands
/// the company's agents a new set of tools and an endpoint to call them at, so
/// it settles what the company can reach — the same question the Composio
/// routes settle, reached from a different direction. `PUT`/`DELETE` follow for
/// the same reason (a `PUT` can also swap the auth material on a server the
/// company already trusts), as does `oauth/start`, which registers a client.
/// The probes — `GET …/tools`, `POST …/test` — stay open: they exercise a
/// server an admin already added and name no endpoint of their own.
async fn add_server(
    company: AdminScopedCompany,
    Json(body): Json<AddServer>,
) -> Result<Json<MutationResponse>, ApiError> {
    let runtime = company.runtime.as_ref();
    let name = body.name.trim().to_string();

    let server = McpServer {
        name: name.clone(),
        endpoint: body.endpoint.trim().to_string(),
        description: body.description.clone(),
        command: None,
        allowed_tools: body.allowed_tools.clone(),
        disallowed_tools: body.disallowed_tools.clone(),
        timeout_secs: body.timeout_secs.unwrap_or(30),
        enabled: true,
        auth_secret: None,
    };
    reject_invalid(&format!("mcp server `{name}`"), &server)?;

    // A manifest-declared name is not a runtime add — update it to override.
    let manifest = manifest_servers(runtime).await?;
    if manifest.iter().any(|m| m.name.trim() == name) {
        return Err(ApiError(OpenCompanyError::Conflict(format!(
            "`{name}` is declared in company.toml — update it to override, don't re-add it."
        ))));
    }

    let mut index = load_runtime_index(runtime.id(), runtime.secrets().as_ref())
        .await
        .map_err(ApiError)?;
    if index.iter().any(|s| s.name.trim() == name) {
        return Err(ApiError(OpenCompanyError::Conflict(format!(
            "an MCP server named `{name}` already exists."
        ))));
    }
    index.push(server.clone());
    save_runtime_index(runtime.id(), runtime.secrets().as_ref(), &index)
        .await
        .map_err(ApiError)?;

    // Persist the credential write-only, if supplied (bearer / header / query).
    if let Some(material) = auth_material_from(
        body.token.as_deref(),
        body.auth_kind,
        body.header_name.as_deref(),
        body.param_name.as_deref(),
    )? {
        store_auth(runtime.id(), &name, &material, runtime.secrets().as_ref())
            .await
            .map_err(ApiError)?;
    }

    let warning = endpoint_secret_advisory(&server.endpoint);
    mutation_response(runtime, &name, warning).await
}

/// `PUT …/mcp/servers/{name}` — update a server (enable/disable, tool lists,
/// endpoint, or rotate token). A manifest server gets a runtime override entry.
async fn update_server(
    company: AdminScopedCompany,
    Path(NamePath { name }): Path<NamePath>,
    body: Option<Json<UpdateServer>>,
) -> Result<Json<MutationResponse>, ApiError> {
    let runtime = company.runtime.as_ref();
    let patch = body.map(|Json(b)| b).unwrap_or_default();
    let name = name.trim().to_string();

    let manifest = manifest_servers(runtime).await?;
    let manifest_entry = manifest.iter().find(|m| m.name.trim() == name).cloned();
    let mut index = load_runtime_index(runtime.id(), runtime.secrets().as_ref())
        .await
        .map_err(ApiError)?;

    // The base to patch: an existing runtime entry (override or runtime server),
    // else the manifest server (creating a fresh override), else the install
    // default (creating the operator's first override — the way a default is
    // disabled, `delete_server` points the console at this route), else 404. A
    // default shadowed by a manifest entry never reaches the third arm: the
    // manifest entry is the effective declaration and is patched instead.
    let position = index.iter().position(|s| s.name.trim() == name);
    let default_entry = runtime
        .default_mcp_servers()
        .iter()
        .find(|d| d.name.trim() == name)
        .cloned();
    let mut server = match (position, &manifest_entry, &default_entry) {
        (Some(i), _, _) => index[i].clone(),
        (None, Some(m), _) => m.clone(),
        (None, None, Some(d)) => d.clone(),
        (None, None, None) => {
            return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
                "no MCP server named `{name}`."
            ))));
        }
    };

    if let Some(enabled) = patch.enabled {
        server.enabled = enabled;
    }
    if let Some(endpoint) = patch.endpoint.as_deref() {
        server.endpoint = endpoint.trim().to_string();
    }
    if patch.description.is_some() {
        server.description = patch.description.clone();
    }
    if let Some(allowed) = patch.allowed_tools.clone() {
        server.allowed_tools = allowed;
    }
    if let Some(disallowed) = patch.disallowed_tools.clone() {
        server.disallowed_tools = disallowed;
    }
    if let Some(timeout) = patch.timeout_secs {
        server.timeout_secs = timeout;
    }
    // The override entry always uses the canonical per-server credential key.
    server.name = name.clone();
    server.command = None;
    server.auth_secret = None;
    reject_invalid(&format!("mcp server `{name}`"), &server)?;
    // Capture the advisory before the value moves into the index.
    let warning = endpoint_secret_advisory(&server.endpoint);

    match position {
        Some(i) => index[i] = server,
        None => index.push(server),
    }
    save_runtime_index(runtime.id(), runtime.secrets().as_ref(), &index)
        .await
        .map_err(ApiError)?;

    if let Some(material) = auth_material_from(
        patch.token.as_deref(),
        patch.auth_kind,
        patch.header_name.as_deref(),
        patch.param_name.as_deref(),
    )? {
        store_auth(runtime.id(), &name, &material, runtime.secrets().as_ref())
            .await
            .map_err(ApiError)?;
    }

    mutation_response(runtime, &name, warning).await
}

/// `DELETE …/mcp/servers/{name}` — remove a runtime server (409 for a manifest
/// server, which can only be disabled).
async fn delete_server(
    company: AdminScopedCompany,
    Path(NamePath { name }): Path<NamePath>,
) -> Result<StatusCode, ApiError> {
    let runtime = company.runtime.as_ref();
    let name = name.trim().to_string();

    let manifest = manifest_servers(runtime).await?;
    if manifest.iter().any(|m| m.name.trim() == name) {
        return Err(ApiError(OpenCompanyError::Conflict(format!(
            "`{name}` is declared in company.toml — disable it instead of deleting."
        ))));
    }
    // Same guard, same reason, for an install-wide default (issue #527): the
    // declaration lives in the instance `config.toml`, not in this company's
    // runtime index, so deleting the index row would not remove it — the next
    // resolution would merge it straight back and the delete would read as
    // broken. Disabling writes an override that *does* persist.
    if runtime
        .default_mcp_servers()
        .iter()
        .any(|d| d.name.trim() == name)
    {
        return Err(ApiError(OpenCompanyError::Conflict(format!(
            "`{name}` ships as an install default — disable it instead of deleting."
        ))));
    }

    let mut index = load_runtime_index(runtime.id(), runtime.secrets().as_ref())
        .await
        .map_err(ApiError)?;
    let before = index.len();
    index.retain(|s| s.name.trim() != name);
    if index.len() == before {
        return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
            "no runtime MCP server named `{name}`."
        ))));
    }
    save_runtime_index(runtime.id(), runtime.secrets().as_ref(), &index)
        .await
        .map_err(ApiError)?;
    // Best-effort credential + health wipe (the store has no delete; an empty
    // value reads as unset, so a later server of the same name never inherits a
    // stale credential or badge).
    clear_auth(runtime.id(), &name, runtime.secrets().as_ref())
        .await
        .map_err(ApiError)?;
    clear_health(runtime.id(), &name, runtime.secrets().as_ref())
        .await
        .map_err(ApiError)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Builds the mutation response by re-resolving the named server's effective
/// projection (so the response reflects manifest/runtime merge + auth status),
/// then probing it once. The probe **never** rolls the mutation back — a
/// needs-config result is a valid resting state; the outcome is persisted as
/// (scrubbed) health and echoed as `test`.
async fn mutation_response(
    runtime: &CompanyRuntime,
    name: &str,
    warning: Option<String>,
) -> Result<Json<MutationResponse>, ApiError> {
    // Probe first (persists scrubbed health), then read the health back into the
    // DTO so the response and a later `GET` agree.
    let test = probe_and_persist(runtime, name).await;

    // One record load: the manifest servers merged into the effective set, and
    // the roster the mutated server's reachability is computed against (#568).
    // Install-wide defaults (#527) sit under the manifest and come off the
    // runtime, so the mutation response reflects the same three-layer merge a
    // later `GET` will.
    let record = runtime.store().load(runtime.id()).await.map_err(ApiError)?;
    let manifest = record
        .as_ref()
        .map(|r| r.manifest.mcp_servers.clone())
        .unwrap_or_default();
    let decls = resolve_effective(
        runtime.id(),
        runtime.default_mcp_servers(),
        &manifest,
        runtime.secrets().as_ref(),
    )
    .await
    .map_err(ApiError)?;
    let decl = decls.iter().find(|d| d.name == name).ok_or_else(|| {
        ApiError(OpenCompanyError::InvalidRequest(format!(
            "`{name}` not found"
        )))
    })?;
    let reachable_by = record
        .as_ref()
        .map(roster_grants)
        .map(|grants| reachers_of(&grants, decl))
        .unwrap_or_default();
    let health = load_health(runtime.id(), name, runtime.secrets().as_ref())
        .await
        .map_err(ApiError)?;
    Ok(Json(MutationResponse {
        server: dto_from_decl(decl, reachable_by, health),
        note: NEXT_TURN_NOTE.to_string(),
        test,
        warning,
    }))
}

/// Probe the named server and persist the (scrubbed) outcome as health, returning
/// it. Under the `openhuman` feature this dials the server through the same
/// registry the agent uses (auth INCLUDED); without it there is no MCP transport,
/// so no probe runs and the console falls back to the declared shape.
#[cfg(feature = "openhuman")]
async fn probe_and_persist(runtime: &CompanyRuntime, name: &str) -> Option<McpHealth> {
    let manifest = manifest_servers(runtime).await.ok()?;
    let decls = resolve_effective(
        runtime.id(),
        runtime.default_mcp_servers(),
        &manifest,
        runtime.secrets().as_ref(),
    )
    .await
    .ok()?;
    let decl = decls.iter().find(|d| d.name == name)?;
    // `probe_server` already scrubs its message; persist that scrubbed health.
    let health = crate::harness::mcp_probe::probe_server(decl).await;
    let _ = mcp::save_health(runtime.id(), name, &health, runtime.secrets().as_ref()).await;
    Some(health)
}

/// Without the `openhuman` feature there is no MCP transport, so probing is a
/// no-op (the console falls back gracefully — same `not_wired` posture as
/// discovery).
#[cfg(not(feature = "openhuman"))]
async fn probe_and_persist(_runtime: &CompanyRuntime, _name: &str) -> Option<McpHealth> {
    None
}

/// Rejects an invalid server declaration as a `400`.
fn reject_invalid(label: &str, server: &McpServer) -> Result<(), ApiError> {
    let problems = validate_one(label, server);
    if problems.is_empty() {
        Ok(())
    } else {
        Err(ApiError(OpenCompanyError::InvalidRequest(
            problems.join(" "),
        )))
    }
}

/// Returns `Some(trimmed)` when the value is a non-blank string.
fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|s| !s.is_empty())
}

/// `GET …/mcp/servers/{name}/tools` — live tool discovery through the registry.
///
/// Gated on the `openhuman` feature (the MCP client + transport live there);
/// without it the route reports `not_wired` so the console falls back gracefully.
#[cfg(feature = "openhuman")]
async fn discover_tools(
    company: ScopedCompany,
    Path(NamePath { name }): Path<NamePath>,
) -> Response {
    use axum::response::IntoResponse;

    let runtime = company.runtime.as_ref();
    let name = name.trim().to_string();
    let manifest = match manifest_servers(runtime).await {
        Ok(m) => m,
        Err(err) => return err.into_response(),
    };
    let decls = match resolve_effective(
        runtime.id(),
        runtime.default_mcp_servers(),
        &manifest,
        runtime.secrets().as_ref(),
    )
    .await
    {
        Ok(d) => d,
        Err(err) => return ApiError(err).into_response(),
    };
    match decls.iter().find(|d| d.name == name) {
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!("no MCP server named `{name}`"),
                "code": "not_found",
            })),
        )
            .into_response(),
        Some(decl) if !decl.enabled => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!("MCP server `{name}` is disabled"),
                "code": "disabled",
            })),
        )
            .into_response(),
        Some(decl) => match crate::harness::mcp::discover_tools(&decls, &name).await {
            Ok(tools) => Json(tools).into_response(),
            Err(err) => {
                // NEVER surface the raw error — it can carry a response body or a
                // full request URL (with a query-parameter credential). Classify,
                // scrub against this server's known secrets, and persist the
                // scrubbed outcome as health.
                use crate::harness::mcp_probe;
                let secrets = decl.auth.secret_values();
                let class = mcp_probe::classify_mcp_error(&err, decl.auth.is_configured(), false);
                let message =
                    mcp_probe::scrub(&mcp_probe::operator_message(&name, &class, &err), &secrets);
                let health = McpHealth {
                    status: class.status,
                    message: message.clone(),
                    tool_count: 0,
                    checked_at_millis: crate::ports::now_millis(),
                    auth_hint: class.auth_hint.clone(),
                };
                let _ = mcp::save_health(runtime.id(), &name, &health, runtime.secrets().as_ref())
                    .await;
                (
                    StatusCode::BAD_GATEWAY,
                    Json(serde_json::json!({
                        "error": message,
                        "code": class.code(),
                    })),
                )
                    .into_response()
            }
        },
    }
}

/// `POST …/mcp/servers/{name}/test` — probe a server on demand and return its
/// (scrubbed) health. Gated on the `openhuman` feature; without it the route
/// reports `not_wired` so the console's Test button degrades gracefully.
#[cfg(feature = "openhuman")]
async fn test_server(company: ScopedCompany, Path(NamePath { name }): Path<NamePath>) -> Response {
    use axum::response::IntoResponse;

    let runtime = company.runtime.as_ref();
    let name = name.trim().to_string();
    // A server that doesn't exist can't be tested.
    let manifest = match manifest_servers(runtime).await {
        Ok(m) => m,
        Err(err) => return err.into_response(),
    };
    let decls = match resolve_effective(
        runtime.id(),
        runtime.default_mcp_servers(),
        &manifest,
        runtime.secrets().as_ref(),
    )
    .await
    {
        Ok(d) => d,
        Err(err) => return ApiError(err).into_response(),
    };
    if !decls.iter().any(|d| d.name == name) {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!("no MCP server named `{name}`"),
                "code": "not_found",
            })),
        )
            .into_response();
    }
    match probe_and_persist(runtime, &name).await {
        Some(health) => Json(health).into_response(),
        None => crate::server::ops::not_wired("mcp probe"),
    }
}

/// Without the `openhuman` feature there is no MCP transport, so on-demand
/// testing is "not wired" (the console falls back to the declared shape).
#[cfg(not(feature = "openhuman"))]
async fn test_server(company: ScopedCompany, Path(NamePath { name }): Path<NamePath>) -> Response {
    let _ = (company, name);
    crate::server::ops::not_wired("mcp probe")
}

/// `POST …/mcp/servers/{name}/oauth/start` — begin the browser OAuth flow for a
/// server that advertises OAuth sign-in (issue #90).
///
/// Resolves the server's effective endpoint, discovers its authorization server,
/// dynamically registers a client (RFC 7591) + generates PKCE, parks the pending
/// state on [`AppState`] keyed by the opaque `state`, and returns
/// `{ "authorizeUrl": … }` for the console to open in a browser tab. The redirect
/// URI is derived from the host's public URL (or bind) so it matches what DCR
/// registered — see [`crate::company::mcp_oauth::callback_redirect_uri`].
///
/// A `400` with a clean operator message is returned when the server does not
/// support dynamic client registration (it can't do console OAuth — the operator
/// should paste a static token instead).
#[cfg(feature = "mcp")]
async fn start_oauth(
    axum::extract::State(state): axum::extract::State<AppState>,
    company: AdminScopedCompany,
    Path(NamePath { name }): Path<NamePath>,
) -> Result<Json<serde_json::Value>, ApiError> {
    use crate::company::mcp_oauth;

    let runtime = company.runtime.as_ref();
    let name = name.trim().to_string();

    // Resolve the effective server so OAuth uses the same endpoint agents will.
    let manifest = manifest_servers(runtime).await?;
    let decls = resolve_effective(
        runtime.id(),
        runtime.default_mcp_servers(),
        &manifest,
        runtime.secrets().as_ref(),
    )
    .await
    .map_err(ApiError)?;
    let decl = decls
        .iter()
        .find(|d| d.name == name)
        .ok_or_else(|| ApiError(OpenCompanyError::McpServerNotFound(name.clone())))?;

    let redirect_uri = mcp_oauth::callback_redirect_uri(&state.config().host_base_url());
    let begun = mcp_oauth::begin(&decl.endpoint, runtime.id(), &name, &redirect_uri)
        .await
        .map_err(ApiError)?;

    // Park the pending flow; the unauthenticated callback route reclaims it.
    state.park_oauth(begun.state.clone(), begun.pending);
    Ok(Json(
        serde_json::json!({ "authorizeUrl": begun.authorize_url }),
    ))
}

/// Without the `mcp` feature there is no OAuth transport, so starting a sign-in
/// is "not wired" (the console's Sign in button degrades gracefully).
#[cfg(not(feature = "mcp"))]
async fn start_oauth(
    company: AdminScopedCompany,
    Path(NamePath { name }): Path<NamePath>,
) -> Response {
    let _ = (company, name);
    crate::server::ops::not_wired("mcp oauth")
}

/// Without the `openhuman` feature there is no MCP transport, so discovery is
/// "not wired" (the console falls back to the declared tool lists).
#[cfg(not(feature = "openhuman"))]
async fn discover_tools(
    company: ScopedCompany,
    Path(NamePath { name }): Path<NamePath>,
) -> Response {
    let _ = (company, name);
    crate::server::ops::not_wired("mcp tool discovery")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::company::CompanyManifest;
    use crate::ports::types::{CompanyId, OverlayAgent};

    /// A company allowing two tool families, with one manifest agent that lists
    /// none (so it inherits both).
    fn record(overlay_agents: Vec<OverlayAgent>) -> CompanyRecord {
        let manifest: CompanyManifest = toml::from_str(
            r#"
[company]
name = "Acme"

[tools]
allow = ["mcp:notion", "mcp:linear"]

[[agent]]
id = "ceo"
role = "Chief Executive"
"#,
        )
        .expect("manifest parses");
        CompanyRecord {
            id: CompanyId::new("acme"),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents,
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            overlay_policy: None,
            disabled_workflows: Vec::new(),
            template_provenance: None,
        }
    }

    fn teammate(id: &str, tools: Vec<&str>) -> OverlayAgent {
        OverlayAgent {
            id: id.to_string(),
            name: id.to_string(),
            role: "Growth".to_string(),
            description: None,
            tools: tools.into_iter().map(str::to_string).collect(),
        }
    }

    /// Issue #619: a scoped overlay teammate reads back here with its own
    /// grant, not the company's.
    ///
    /// This is what `reachable_by` on every MCP server row is computed from, so
    /// an overlay teammate whose scope this ignored would be listed as reaching
    /// servers the harness does not grant it — the console asserting a
    /// connection that is not there.
    #[test]
    fn a_scoped_overlay_teammate_does_not_read_back_as_reaching_everything() {
        let scoped = record(vec![teammate("jamie", vec!["mcp:notion"])]);
        let grants = roster_grants(&scoped);
        let jamie = grants
            .iter()
            .find(|(id, _)| id == "jamie")
            .expect("the overlay teammate is on the roster");
        assert_eq!(
            jamie.1,
            vec!["mcp:notion".to_string()],
            "a scoped teammate reaches only what it was scoped to"
        );

        // The manifest agent lists nothing and still inherits everything, so
        // the narrowing above is the teammate's own and not a company change.
        let ceo = grants
            .iter()
            .find(|(id, _)| id == "ceo")
            .expect("on roster");
        assert_eq!(
            ceo.1,
            vec!["mcp:notion".to_string(), "mcp:linear".to_string()]
        );
    }

    /// The empty-means-inherit rule (#264) is untouched: a teammate written
    /// before #619 — and every teammate the console still creates — reads back
    /// holding the company's whole grant.
    #[test]
    fn an_unscoped_overlay_teammate_still_inherits_the_company_grant() {
        let grants = roster_grants(&record(vec![teammate("jamie", Vec::new())]));
        let jamie = grants.iter().find(|(id, _)| id == "jamie").expect("roster");
        assert_eq!(
            jamie.1,
            vec!["mcp:notion".to_string(), "mcp:linear".to_string()]
        );
    }
}
