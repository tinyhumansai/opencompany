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
//! are registered by [`scoped`]. Agents pick up a change on their next harness
//! rebuild; every mutating response says so via `note`.

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
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// The reminder attached to every mutating response: a live agent's tool set is
/// rebuilt lazily, so an edit reaches agents on the next harness rebuild.
const REBUILD_NOTE: &str =
    "Agents pick up this change on their next harness rebuild (restart the company).";

/// Builds the MCP server management route fragment.
pub fn router() -> Router<AppState> {
    scoped("/mcp/servers", post(add_server).get(list_servers))
        .merge(scoped(
            "/mcp/servers/{name}",
            put(update_server).delete(delete_server),
        ))
        .merge(scoped("/mcp/servers/{name}/tools", get(discover_tools)))
        .merge(scoped("/mcp/servers/{name}/test", post(test_server)))
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
    /// `manifest` (committed) or `runtime` (console-added).
    source: McpSource,
    enabled: bool,
    allowed_tools: Vec<String>,
    disallowed_tools: Vec<String>,
    timeout_secs: u64,
    /// Whether an outbound credential is stored — never the credential itself.
    auth_configured: bool,
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
/// DTO, reducing the resolved credential to a boolean and attaching the last
/// (scrubbed) probe health.
fn dto_from_decl(decl: &mcp::McpServerDecl, health: Option<McpHealth>) -> McpServerDto {
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
        health,
    }
}

/// `GET …/mcp/servers` — the company's effective MCP servers, each with its last
/// recorded (scrubbed) probe health.
async fn list_servers(company: ScopedCompany) -> Result<Json<Vec<McpServerDto>>, ApiError> {
    let runtime = company.runtime.as_ref();
    let manifest = manifest_servers(runtime).await?;
    let decls = resolve_effective(runtime.id(), &manifest, runtime.secrets().as_ref())
        .await
        .map_err(ApiError)?;
    let mut out = Vec::with_capacity(decls.len());
    for decl in &decls {
        let health = load_health(runtime.id(), &decl.name, runtime.secrets().as_ref())
            .await
            .map_err(ApiError)?;
        out.push(dto_from_decl(decl, health));
    }
    Ok(Json(out))
}

/// `POST …/mcp/servers` — add a runtime MCP server (+ optional token).
async fn add_server(
    company: ScopedCompany,
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
    company: ScopedCompany,
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
    // else the manifest server (creating a fresh override), else 404.
    let position = index.iter().position(|s| s.name.trim() == name);
    let mut server = match (position, &manifest_entry) {
        (Some(i), _) => index[i].clone(),
        (None, Some(m)) => m.clone(),
        (None, None) => {
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
    company: ScopedCompany,
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

    let manifest = manifest_servers(runtime).await?;
    let decls = resolve_effective(runtime.id(), &manifest, runtime.secrets().as_ref())
        .await
        .map_err(ApiError)?;
    let decl = decls.iter().find(|d| d.name == name).ok_or_else(|| {
        ApiError(OpenCompanyError::InvalidRequest(format!(
            "`{name}` not found"
        )))
    })?;
    let health = load_health(runtime.id(), name, runtime.secrets().as_ref())
        .await
        .map_err(ApiError)?;
    Ok(Json(MutationResponse {
        server: dto_from_decl(decl, health),
        note: REBUILD_NOTE.to_string(),
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
    let decls = resolve_effective(runtime.id(), &manifest, runtime.secrets().as_ref())
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
    let decls = match resolve_effective(runtime.id(), &manifest, runtime.secrets().as_ref()).await {
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
    let decls = match resolve_effective(runtime.id(), &manifest, runtime.secrets().as_ref()).await {
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
