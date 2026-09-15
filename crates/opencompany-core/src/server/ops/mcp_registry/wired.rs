//! The registry routes and the store read behind them — compiled only with the
//! `mcp` feature.
//!
//! Everything here is a thin adapter: it resolves the company's
//! [`McpRuntime`](crate::harness::mcp::McpRuntime), calls one of its wrappers,
//! and hands the result to a projection from the parent module. The rules worth
//! testing (endpoint reconciliation, row naming, status mapping, catalogue
//! projection, the stdio refusal) all live in the parent and compile without
//! this feature, so they are exercised by the ungated lane rather than only by
//! the filtered belt lane (issue #770).
//!
//! **Authority.** Every mutation takes [`AdminScopedCompany`]: installing a
//! server hands *every* teammate a new set of callable tools (`build.rs` pushes
//! the registry bridge tools with no grant check), so it settles what the
//! company can reach — the same question `POST …/mcp/servers` already answers
//! under the same guard. Browsing the directory decides nothing and takes
//! [`ScopedCompany`], matching `GET …/mcp/servers`.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use oh::mcp::registry::types::{ConnStatus, InstalledServer};
use openhuman_core as oh;

use crate::company::mcp::{McpHealth, stdio_install_refusal};
use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::ports::now_millis;
use crate::server::error::ApiError;
use crate::server::ops::mcp::{McpServerDto, NEXT_TURN_NOTE, merged_rows};
use crate::server::ops::{AdminScopedCompany, ScopedCompany, not_wired};

use super::RegistryInstall;
use super::catalogue::{catalogue_detail, catalogue_search, health_from_status};

// ---------------------------------------------------------------------------
// Request and response bodies
// ---------------------------------------------------------------------------

/// `GET …/mcp/registry/search` query.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SearchQuery {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    page: Option<u32>,
    #[serde(default)]
    page_size: Option<u32>,
}

/// `GET …/mcp/registry/entry` query.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct EntryQuery {
    qualified_name: String,
}

/// `POST …/mcp/registry/install` body — the entry to install and the values for
/// the env keys it declared. Values are write-only: they are persisted into
/// OpenHuman's env table and never read back by any route here.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct InstallBody {
    qualified_name: String,
    #[serde(default)]
    env: HashMap<String, String>,
}

/// `PUT …/mcp/registry/{server_id}/env` body — a credential rotation.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct EnvBody {
    #[serde(default)]
    env: HashMap<String, String>,
}

/// The `{server_id}` path segment.
#[derive(Debug, Deserialize)]
pub(super) struct ServerIdPath {
    server_id: String,
}

/// A registry mutation's response: the resulting row, the rebuild reminder, and
/// the connection state right after the change.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RegistryMutationResponse {
    server: McpServerDto,
    note: String,
    /// The connection state after the mutation. A failed connect is **never** a
    /// rollback — a needs-credential resting state is valid here for exactly the
    /// reason it is on `POST …/mcp/servers`.
    #[serde(skip_serializing_if = "Option::is_none")]
    test: Option<McpHealth>,
}

/// Every registry install for this company, already projected.
///
/// **Degrades, never fails.** A missing runtime, an unreadable store, or a
/// registry that will not answer all resolve to "no installs", so
/// `GET …/mcp/servers` still returns List A. The MCP tab going blank because a
/// directory was down is a worse outcome than a tab that is briefly missing the
/// rows it cannot read, and List A is the half that governs what the agents
/// reach — so it is the half that must survive.
pub(in crate::server::ops) async fn installs(runtime: &CompanyRuntime) -> Vec<RegistryInstall> {
    let Some(mcp) = runtime.mcp() else {
        return Vec::new();
    };
    let servers = match mcp.list() {
        Ok(servers) => servers,
        Err(error) => {
            tracing::warn!(
                "[mcp-registry] company `{}`: install list unavailable, serving the declared \
                 servers only: {error}",
                runtime.id()
            );
            return Vec::new();
        }
    };
    if servers.is_empty() {
        return Vec::new();
    }
    let status: HashMap<String, ConnStatus> = mcp
        .status()
        .await
        .into_iter()
        .map(|state| (state.server_id.clone(), state))
        .collect();
    let now = now_millis();
    servers
        .into_iter()
        .map(|server| {
            let state = status.get(&server.server_id);
            project(server, state, now)
        })
        .collect()
}

/// One store record plus its live connection state.
fn project(server: InstalledServer, state: Option<&ConnStatus>, now: u64) -> RegistryInstall {
    let endpoint = server.transport.deployment_url().map(str::to_string);
    let transport = server.transport.dispatch_kind().to_string();
    let health = state.and_then(|state| {
        health_from_status(
            state.status.as_str(),
            state.tool_count,
            // A typed hint now; `as_code` is the same stable wire string
            // this surface has always emitted.
            state.auth_hint.map(|hint| hint.as_code()),
            now,
        )
    });
    RegistryInstall {
        server_id: server.server_id,
        qualified_name: server.qualified_name,
        display_name: server.display_name,
        description: server.description,
        icon_url: server.icon_url,
        endpoint,
        transport,
        enabled: server.enabled,
        // `env_keys` is the set of keys whose values upstream persisted, so this
        // answers "is a credential stored" without ever loading one.
        auth_configured: !server.env_keys.is_empty(),
        health,
    }
}

/// `GET …/mcp/registry/search` — browse the upstream MCP directory.
///
/// The open `modelcontextprotocol/registry` and nothing else. The Smithery
/// half, with the per-company API key that decided whether it was queried at
/// all, was removed: one vendor's credential slot on a console tab is a
/// credential to rotate, revoke and explain, and what it bought — one
/// directory's hosted listings — is not worth the surface. Entries that declare
/// no remote endpoint are filtered out here as they always were, since this
/// deployment launches no local subprocess.
pub(super) async fn search(company: ScopedCompany, Query(query): Query<SearchQuery>) -> Response {
    let Some(mcp) = company.runtime.mcp() else {
        return not_wired("mcp registry");
    };
    match mcp.search(query.q, query.page, query.page_size).await {
        Ok(raw) => Json(catalogue_search(&raw)).into_response(),
        Err(error) => ApiError(error).into_response(),
    }
}

/// `GET …/mcp/registry/entry?qualifiedName=…` — one directory entry in full,
/// with the install decision already made so the console never offers an
/// install that would be refused.
pub(super) async fn entry(company: ScopedCompany, Query(query): Query<EntryQuery>) -> Response {
    let Some(mcp) = company.runtime.mcp() else {
        return not_wired("mcp registry");
    };
    let qualified_name = query.qualified_name.trim().to_string();
    if qualified_name.is_empty() {
        return ApiError(OpenCompanyError::InvalidRequest(
            "a directory lookup needs a `qualifiedName`.".to_string(),
        ))
        .into_response();
    }
    let raw = match mcp.registry_get(qualified_name.clone()).await {
        Ok(raw) => raw,
        Err(error) => return ApiError(error).into_response(),
    };
    match catalogue_detail(&raw) {
        Some(detail) => Json(detail).into_response(),
        None => ApiError(OpenCompanyError::McpServerNotFound(qualified_name)).into_response(),
    }
}

/// `POST …/mcp/registry/install` — install a directory entry and connect it.
///
/// Refuses a stdio-only entry before writing anything: the tenant image has no
/// Node, Python or package manager to launch one with. The search filter keeps
/// such entries off the operator's screen; this check is what makes the refusal
/// true for a caller that POSTs a qualified name search never offered.
///
/// The connect that follows is **not** a gate. A server that installs and then
/// asks for a credential is at a valid resting state — the same rule
/// `POST …/mcp/servers` follows when its probe comes back `needs_config` — so
/// the install stands and the connection state rides back in `test`.
pub(super) async fn install(
    company: AdminScopedCompany,
    Json(body): Json<InstallBody>,
) -> Response {
    let runtime = company.runtime.as_ref();
    let Some(mcp) = runtime.mcp() else {
        return not_wired("mcp registry");
    };
    let qualified_name = body.qualified_name.trim().to_string();
    if qualified_name.is_empty() {
        return ApiError(OpenCompanyError::InvalidRequest(
            "an install needs a `qualifiedName`.".to_string(),
        ))
        .into_response();
    }

    let raw = match mcp.registry_get(qualified_name.clone()).await {
        Ok(raw) => raw,
        Err(error) => return ApiError(error).into_response(),
    };
    let Some(detail) = catalogue_detail(&raw) else {
        return ApiError(OpenCompanyError::McpServerNotFound(qualified_name)).into_response();
    };
    if !detail.installable {
        let refusal = detail
            .refusal
            .unwrap_or_else(|| stdio_install_refusal(&qualified_name));
        return ApiError(OpenCompanyError::InvalidRequest(refusal)).into_response();
    }

    let server = match mcp.install_from_directory(qualified_name, body.env).await {
        Ok(server) => server,
        Err(error) => return ApiError(error).into_response(),
    };
    // A connect failure is recorded as connection state, not raised: see above.
    let _ = mcp.connect(&server.server_id).await;
    mutation_response(runtime, &server.server_id).await
}

/// `POST …/mcp/registry/{server_id}/connect` — dial an installed server.
pub(super) async fn connect_server(
    company: AdminScopedCompany,
    Path(ServerIdPath { server_id }): Path<ServerIdPath>,
) -> Response {
    let runtime = company.runtime.as_ref();
    let Some(mcp) = runtime.mcp() else {
        return not_wired("mcp registry");
    };
    if let Err(error) = mcp.get(&server_id) {
        return ApiError(error).into_response();
    }
    // Same posture as install: a refused connection is a state to report, not an
    // error that hides the server.
    let _ = mcp.connect(&server_id).await;
    mutation_response(runtime, &server_id).await
}

/// `POST …/mcp/registry/{server_id}/disconnect` — drop the live session,
/// keeping the install and its stored credentials.
pub(super) async fn disconnect_server(
    company: AdminScopedCompany,
    Path(ServerIdPath { server_id }): Path<ServerIdPath>,
) -> Response {
    let runtime = company.runtime.as_ref();
    let Some(mcp) = runtime.mcp() else {
        return not_wired("mcp registry");
    };
    if let Err(error) = mcp.disconnect(&server_id).await {
        return ApiError(error).into_response();
    }
    mutation_response(runtime, &server_id).await
}

/// `PUT …/mcp/registry/{server_id}/env` — rotate an install's credentials.
///
/// Write-only in both directions: the values go into OpenHuman's env table and
/// the response carries an `authConfigured` bool, never a value. Upstream merges
/// the supplied keys over the stored ones and reconnects, so a form that sends
/// only the field the operator retyped does not erase the rest.
pub(super) async fn update_env(
    company: AdminScopedCompany,
    Path(ServerIdPath { server_id }): Path<ServerIdPath>,
    Json(body): Json<EnvBody>,
) -> Response {
    let runtime = company.runtime.as_ref();
    let Some(mcp) = runtime.mcp() else {
        return not_wired("mcp registry");
    };
    if body.env.is_empty() {
        return ApiError(OpenCompanyError::InvalidRequest(
            "a credential rotation needs at least one `env` value.".to_string(),
        ))
        .into_response();
    }
    // Establish membership before writing: upstream's update persists first and
    // reads the install record afterwards, so an unknown id would leave orphaned
    // env rows behind before failing.
    if let Err(error) = mcp.get(&server_id) {
        return ApiError(error).into_response();
    }
    if let Err(error) = mcp.update_env(server_id.clone(), body.env).await {
        return ApiError(error).into_response();
    }
    mutation_response(runtime, &server_id).await
}

/// `DELETE …/mcp/registry/{server_id}` — disconnect, then drop the install and
/// its stored env values.
pub(super) async fn uninstall(
    company: AdminScopedCompany,
    Path(ServerIdPath { server_id }): Path<ServerIdPath>,
) -> Response {
    let Some(mcp) = company.runtime.mcp() else {
        return not_wired("mcp registry");
    };
    match mcp.uninstall(&server_id).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => ApiError(error).into_response(),
    }
}

/// Builds a mutation response by re-reading the **merged** list and picking out
/// the row this install now occupies.
///
/// Re-reading rather than projecting the record in hand is what keeps the
/// response and a following `GET …/mcp/servers` from disagreeing: if the install
/// reconciles onto a manifest or runtime row, that is the row the operator will
/// see, and it is the row that comes back here — badge, name and all.
async fn mutation_response(runtime: &CompanyRuntime, server_id: &str) -> Response {
    let rows = match merged_rows(runtime).await {
        Ok(rows) => rows,
        Err(error) => return error.into_response(),
    };
    let Some(server) = rows
        .into_iter()
        .find(|row| row.server_id.as_deref() == Some(server_id))
    else {
        return ApiError(OpenCompanyError::McpServerNotFound(server_id.to_string()))
            .into_response();
    };
    // The row's health *is* the post-mutation connection state — `installs`
    // reads it from the live connection map on this very call. Echoing it as
    // `test` matches the shape `POST …/mcp/servers` already returns.
    let test = server.health.clone();
    Json(RegistryMutationResponse {
        server,
        note: NEXT_TURN_NOTE.to_string(),
        test,
    })
    .into_response()
}

/// Removes a directory install on behalf of `DELETE …/mcp/servers/{name}`.
///
/// A runtime already missing its registry is not an error here: there is then no
/// install to remove and the index-row delete that preceded this call was the
/// whole removal.
pub(in crate::server::ops) async fn remove_install(
    runtime: &CompanyRuntime,
    server_id: &str,
) -> Result<(), ApiError> {
    let Some(mcp) = runtime.mcp() else {
        return Ok(());
    };
    mcp.uninstall(server_id).await.map(|_| ()).map_err(ApiError)
}
