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
    self, McpSource, clear_auth, load_runtime_index, resolve_effective, save_runtime_index,
    store_bearer, validate_one,
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
}

/// One effective MCP server as the console renders it. **Never** carries a
/// credential — only a non-secret `authConfigured` flag.
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
}

/// A mutating response: the resulting server plus the rebuild reminder.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MutationResponse {
    server: McpServerDto,
    note: String,
}

/// Add-server body. `token` is write-only intake.
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
    /// The outbound bearer token, stored write-only. Omit to leave auth unset.
    #[serde(default)]
    token: Option<String>,
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
    /// Rotate the outbound token (write-only). Omit to leave it unchanged.
    #[serde(default)]
    token: Option<String>,
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
/// DTO, reducing the resolved credential to a boolean.
fn dto_from_decl(decl: &mcp::McpServerDecl) -> McpServerDto {
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
    }
}

/// `GET …/mcp/servers` — the company's effective MCP servers.
async fn list_servers(company: ScopedCompany) -> Result<Json<Vec<McpServerDto>>, ApiError> {
    let runtime = company.runtime.as_ref();
    let manifest = manifest_servers(runtime).await?;
    let decls = resolve_effective(runtime.id(), &manifest, runtime.secrets().as_ref())
        .await
        .map_err(ApiError)?;
    Ok(Json(decls.iter().map(dto_from_decl).collect()))
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

    // Persist the token write-only, if supplied.
    if let Some(token) = non_empty(body.token.as_deref()) {
        store_bearer(runtime.id(), &name, token, runtime.secrets().as_ref())
            .await
            .map_err(ApiError)?;
    }

    mutation_response(runtime, &name).await
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

    match position {
        Some(i) => index[i] = server,
        None => index.push(server),
    }
    save_runtime_index(runtime.id(), runtime.secrets().as_ref(), &index)
        .await
        .map_err(ApiError)?;

    if let Some(token) = non_empty(patch.token.as_deref()) {
        store_bearer(runtime.id(), &name, token, runtime.secrets().as_ref())
            .await
            .map_err(ApiError)?;
    }

    mutation_response(runtime, &name).await
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
    // Best-effort credential wipe (the store has no delete; empty reads as unset).
    clear_auth(runtime.id(), &name, runtime.secrets().as_ref())
        .await
        .map_err(ApiError)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Builds the mutation response by re-resolving the named server's effective
/// projection (so the response reflects manifest/runtime merge + auth status).
async fn mutation_response(
    runtime: &CompanyRuntime,
    name: &str,
) -> Result<Json<MutationResponse>, ApiError> {
    let manifest = manifest_servers(runtime).await?;
    let decls = resolve_effective(runtime.id(), &manifest, runtime.secrets().as_ref())
        .await
        .map_err(ApiError)?;
    let decl = decls.iter().find(|d| d.name == name).ok_or_else(|| {
        ApiError(OpenCompanyError::InvalidRequest(format!(
            "`{name}` not found"
        )))
    })?;
    Ok(Json(MutationResponse {
        server: dto_from_decl(decl),
        note: REBUILD_NOTE.to_string(),
    }))
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
        Some(_) => {
            #[cfg(feature = "mcp")]
            {
                match crate::harness::mcp::discover_tools(&decls, &name).await {
                    Ok(tools) => return Json(tools).into_response(),
                    Err(err) => {
                        return (
                            StatusCode::BAD_GATEWAY,
                            Json(serde_json::json!({
                                "error": format!("MCP discovery failed: {err}"),
                                "code": "discovery_failed",
                            })),
                        )
                            .into_response();
                    }
                }
            }
            #[cfg(not(feature = "mcp"))]
            {
                return crate::server::ops::not_wired("mcp tool discovery");
            }
        }
    }
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
