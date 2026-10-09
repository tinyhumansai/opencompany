//! The registry routes and the store read behind them — compiled only with the
//! `mcp` feature.
//!
//! Everything here is a thin adapter: it resolves the company's
//! [`McpRuntime`](crate::mcp::runtime::McpRuntime), calls one of its wrappers,
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
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use oh::mcp::registry::types::{ConnStatus, InstalledServer};
use openhuman_core as oh;
use tinymcp::registry::curation::OFFICIAL_SERVERS;

use crate::company::mcp::load_runtime_index;
use crate::company::mcp::{McpHealth, stdio_install_refusal};
use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::mcp::decl::endpoint::normalize_endpoint;
use crate::mcp::decl::server_info::{self as mcp_server_info, fetch_icon};
use crate::mcp::runtime::McpRuntime;
use crate::ports::now_millis;
use crate::server::error::ApiError;
use crate::server::ops::mcp::{
    AuthKind, McpServerDto, NEXT_TURN_NOTE, auth_material_from, declare_runtime_server,
    manifest_servers, merged_rows,
};
use crate::server::ops::{AdminScopedCompany, ScopedCompany, not_wired};

use super::RegistryInstall;
use super::catalogue::{
    CatalogueEntryDto, InstallName, brand_logo, brand_name, browse_upstream_page, catalogue_detail,
    catalogue_search, featured_entry, featured_page, health_from_status, inline_icon, inline_icons,
    install_name_for, rank_catalogue, shift_browse_page,
};
use super::failure::{FEATURED_LOOKUP_BUDGET, RegistryRead, SEARCH_BUDGET, bounded};
use super::icon_cache::{ICON_BUDGET, ICON_NEGATIVE_TTL, IconCache};

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
    /// The outbound credential value, stored write-only. Omit to declare the
    /// server without one and set it later with `PUT …/mcp/servers/{name}`.
    ///
    /// The directory reports an entry's `requiredEnvKeys`, but those name a
    /// launcher's environment and a hosted HTTPS endpoint has no launcher.
    /// How a credential reaches such a server — bearer, named header, query
    /// parameter — is the same question `POST …/mcp/servers` asks, and is
    /// answered here the same way rather than guessed from a key's spelling.
    #[serde(default)]
    token: Option<String>,
    /// The auth scheme; defaults to `bearer`.
    #[serde(default)]
    auth_kind: AuthKind,
    /// The header name, when `authKind == header`.
    #[serde(default)]
    header_name: Option<String>,
    /// The query-parameter name, when `authKind == query_param`.
    #[serde(default)]
    param_name: Option<String>,
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
    let mut installs: Vec<RegistryInstall> = servers
        .into_iter()
        .map(|server| {
            let state = status.get(&server.server_id);
            project(server, state, now)
        })
        .collect();
    let icons = futures::future::join_all(
        installs
            .iter()
            .map(|install| inline_icon(install.icon_url.clone(), &cached_icon)),
    )
    .await;
    for (install, icon) in installs.iter_mut().zip(icons) {
        install.icon_url = icon;
    }
    installs
}

/// How long an official entry is served from memory before it is looked up
/// again. A failed refresh keeps serving the entry it already has.
const FEATURED_TTL: Duration = Duration::from_secs(6 * 60 * 60);

static FEATURED: LazyLock<Mutex<HashMap<&'static str, (Instant, CatalogueEntryDto)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The official connectors, in their listed order. Each is looked up once and
/// kept: the registry answers some of these lookups slower than the request
/// timeout, so fetching all of them on every visit drops a different few each
/// time.
async fn featured_servers(mcp: &McpRuntime) -> Vec<CatalogueEntryDto> {
    let now = Instant::now();
    let due: Vec<&'static str> = {
        let cache = FEATURED
            .lock()
            .map(|cache| cache.clone())
            .unwrap_or_default();
        OFFICIAL_SERVERS
            .iter()
            .copied()
            .filter(|name| {
                cache
                    .get(name)
                    .is_none_or(|(at, _)| now.duration_since(*at) > FEATURED_TTL)
            })
            .collect()
    };
    let fetched = futures::future::join_all(due.into_iter().map(|name| async move {
        let entry = bounded(
            RegistryRead::Lookup,
            Some(FEATURED_LOOKUP_BUDGET),
            mcp.registry_get(name.to_string()),
        )
        .await
        .ok()
        .and_then(|raw| featured_entry(&raw));
        (name, entry)
    }))
    .await;
    let Ok(mut cache) = FEATURED.lock() else {
        return fetched.into_iter().filter_map(|(_, entry)| entry).collect();
    };
    for (name, entry) in fetched {
        if let Some(entry) = entry {
            cache.insert(name, (now, entry));
        }
    }
    OFFICIAL_SERVERS
        .iter()
        .filter_map(|name| cache.get(name).map(|(_, entry)| entry.clone()))
        .collect()
}

static ICONS: LazyLock<Arc<IconCache>> =
    LazyLock::new(|| IconCache::new(ICON_BUDGET, ICON_NEGATIVE_TTL));

/// [`fetch_icon`] through the shared icon cache.
async fn cached_icon(url: String) -> Option<String> {
    ICONS
        .get(url, |url| async move { fetch_icon(&url).await })
        .await
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
    let display_name = brand_name(&server.qualified_name, Some(server.display_name));
    let icon_url = server
        .icon_url
        .or_else(|| brand_logo(&server.qualified_name));
    RegistryInstall {
        server_id: server.server_id,
        qualified_name: server.qualified_name,
        display_name,
        icon_url,
        description: server.description,
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
/// The open `modelcontextprotocol/registry`, plus Smithery only where the host
/// process sets `SMITHERY_API_KEY`. Entries that declare no remote endpoint are
/// filtered out, since this deployment launches no local subprocess. The first
/// page of an empty query leads with the known vendor servers, every page is
/// ordered official first and most-installed next, and icons are inlined so the
/// browser never requests a remote address.
pub(super) async fn search(company: ScopedCompany, Query(query): Query<SearchQuery>) -> Response {
    let Some(mcp) = company.runtime.mcp() else {
        return not_wired("mcp registry");
    };
    let browsing = query.q.as_deref().is_none_or(|q| q.trim().is_empty());
    let shown_page = query.page.unwrap_or(1).max(1);
    if browsing && shown_page == 1 {
        let featured = featured_servers(mcp).await;
        if !featured.is_empty() {
            let mut page = featured_page(featured);
            rank_catalogue(&mut page.servers, OFFICIAL_SERVERS);
            inline_icons(&mut page.servers, cached_icon).await;
            return Json(page).into_response();
        }
    }
    let upstream_page = if browsing {
        browse_upstream_page(shown_page)
    } else {
        shown_page
    };
    let mut results = match bounded(
        RegistryRead::Search,
        Some(SEARCH_BUDGET),
        mcp.search(query.q, Some(upstream_page), query.page_size),
    )
    .await
    {
        Ok(raw) => catalogue_search(&raw),
        Err(failure) => return failure.response(RegistryRead::Search),
    };
    if browsing {
        let already_listed: &[&str] = if shown_page == 1 {
            &[]
        } else {
            OFFICIAL_SERVERS
        };
        shift_browse_page(&mut results, upstream_page, already_listed);
    }
    rank_catalogue(&mut results.servers, OFFICIAL_SERVERS);
    inline_icons(&mut results.servers, cached_icon).await;
    Json(results).into_response()
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
    let raw = match bounded(
        RegistryRead::Lookup,
        None,
        mcp.registry_get(qualified_name.clone()),
    )
    .await
    {
        Ok(raw) => raw,
        Err(failure) => return failure.response(RegistryRead::Lookup),
    };
    match catalogue_detail(&raw) {
        Some(mut detail) => {
            detail.icon_url = inline_icon(detail.icon_url.take(), &cached_icon).await;
            Json(detail).into_response()
        }
        None => ApiError(OpenCompanyError::McpServerNotFound(qualified_name)).into_response(),
    }
}

/// `POST …/mcp/registry/install` — declare a directory entry as one of this
/// company's own servers.
///
/// Refuses a stdio-only entry before writing anything: the tenant image has no
/// Node, Python or package manager to launch one with. The search filter keeps
/// such entries off the operator's screen; this check is what makes the refusal
/// true for a caller that POSTs a qualified name search never offered.
///
/// **This writes the same runtime index `POST …/mcp/servers` writes**, so a
/// server found in the directory is an ordinary `runtime`-sourced row and the
/// whole surface — conflicts, override rules, probes, credential rotation,
/// delete — behaves for it exactly as for a server an admin typed in by hand.
/// It used to write OpenHuman's separate install store instead, through an RPC
/// upstream has since removed: the registry there is browse-only now, and a
/// server found in it is declared by the reader. Declaring it here rather than
/// making the operator retype an endpoint they just looked at is what this
/// route is for.
///
/// The probe that follows is **not** a gate. A server that is declared and
/// then asks for a credential is at a valid resting state — the same rule
/// `POST …/mcp/servers` follows when its probe comes back `needs_config` — so
/// the declaration stands and the connection state rides back in `test`.
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

    let raw = match bounded(
        RegistryRead::Lookup,
        None,
        mcp.registry_get(qualified_name.clone()),
    )
    .await
    {
        Ok(raw) => raw,
        Err(failure) => return failure.response(RegistryRead::Lookup),
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

    // Everything the declaration needs is already in the catalogue entry this
    // route just fetched: the hosted endpoint an install would have dialled,
    // and the blurb the directory shows beside it.
    let Some(endpoint) = detail.endpoint else {
        return ApiError(OpenCompanyError::InvalidRequest(stdio_install_refusal(
            &qualified_name,
        )))
        .into_response();
    };
    let name = match install_name(runtime, &detail.display_name, &qualified_name, &endpoint).await {
        Ok(name) => name,
        Err(error) => return error.into_response(),
    };
    let server = super::declaration_from_directory(&name, &endpoint, detail.description);
    let auth = match auth_material_from(
        body.token.as_deref(),
        body.auth_kind,
        body.header_name.as_deref(),
        body.param_name.as_deref(),
    ) {
        Ok(auth) => auth,
        Err(error) => return error.into_response(),
    };
    let declared = match declare_runtime_server(runtime, server, auth).await {
        Ok(response) => response,
        Err(error) => return error.into_response(),
    };
    let icon = inline_icon(detail.icon_url, &cached_icon).await;
    remember_directory_identity(runtime, &name, detail.display_name, icon).await;
    declared.into_response()
}

/// The name a directory install is saved under, or the refusal when this
/// company already declares a server at the same endpoint.
async fn install_name(
    runtime: &CompanyRuntime,
    display_name: &str,
    qualified_name: &str,
    endpoint: &str,
) -> Result<String, ApiError> {
    let manifest = manifest_servers(runtime).await?;
    let index = load_runtime_index(runtime.id(), runtime.secrets().as_ref())
        .await
        .map_err(ApiError)?;
    let existing: Vec<(String, Option<String>)> = manifest
        .iter()
        .chain(index.iter())
        .map(|server| {
            (
                server.name.trim().to_string(),
                normalize_endpoint(&server.endpoint),
            )
        })
        .collect();
    let endpoint = normalize_endpoint(endpoint);
    match install_name_for(display_name, qualified_name, endpoint.as_deref(), &existing) {
        InstallName::Free(name) => Ok(name),
        InstallName::AlreadyInstalled(name) => Err(ApiError(OpenCompanyError::Conflict(format!(
            "this server is already installed as `{name}`."
        )))),
    }
}

/// Keeps the directory's name and logo on the installed server wherever the
/// server's own handshake did not supply one.
async fn remember_directory_identity(
    runtime: &CompanyRuntime,
    name: &str,
    title: String,
    icon: Option<String>,
) {
    let secrets = runtime.secrets();
    let mut info = mcp_server_info::load(runtime.id(), name, secrets.as_ref()).await;
    info.title = info.title.or(Some(title));
    info.icon_data_url = info.icon_data_url.or(icon);
    if let Err(error) = mcp_server_info::save(runtime.id(), name, &info, secrets.as_ref()).await {
        tracing::warn!("[mcp-registry] `{name}`: directory name and logo not stored: {error}");
    }
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

// ---------------------------------------------------------------------------
// Per-tool permissions for a directory install
// ---------------------------------------------------------------------------

use crate::mcp::policy as mcp_policy;
use crate::server::ops::mcp_tool_policy::{
    AgentScope, PutToolPolicy, apply_tool_policy_patch, policy_unreadable, require_roster_agent,
    tool_policy_dto,
};

/// Reads an install's stored policy strictly, so an unreadable document is a
/// `409` rather than silently rendered as "no overrides".
async fn stored_strict(
    runtime: &crate::company::runtime::CompanyRuntime,
    server_id: &str,
) -> Result<mcp_policy::McpToolPolicies, Box<Response>> {
    mcp_policy::load_tool_policies_strict(
        runtime.id(),
        runtime.secrets().as_ref(),
        &mcp_policy::registry_tool_policies_key(server_id),
    )
    .await
    .map_err(|_| Box::new(policy_unreadable(server_id)))
    .map(Option::unwrap_or_default)
}

/// Renders an install's resolved permission document.
///
/// A directory install carries no `read_only_tools` declaration — that field is
/// a manifest affordance — so the stored document is the whole policy and there
/// is no legacy baseline to layer it over.
async fn registry_policy_response(
    runtime: &crate::company::runtime::CompanyRuntime,
    server_id: &str,
    stored: mcp_policy::McpToolPolicies,
    agent: Option<&str>,
) -> Response {
    let inventory = mcp_policy::load_tool_inventory(
        runtime.id(),
        runtime.secrets().as_ref(),
        &mcp_policy::registry_tool_inventory_key(server_id),
    )
    .await;
    let policies = mcp_policy::effective_policies(&[], mcp_policy::StoredPolicies::Stored(stored));
    Json(tool_policy_dto(server_id, &policies, &inventory, agent)).into_response()
}

/// `GET …/mcp/registry/{server_id}/tools/policy`
pub(super) async fn read_tool_policy(
    company: ScopedCompany,
    Path(ServerIdPath { server_id }): Path<ServerIdPath>,
    Query(scope): Query<AgentScope>,
) -> Response {
    let runtime = company.runtime.as_ref();
    let Some(mcp) = runtime.mcp() else {
        return not_wired("mcp registry");
    };
    if let Err(error) = mcp.get(&server_id) {
        return ApiError(error).into_response();
    }
    match stored_strict(runtime, &server_id).await {
        Ok(stored) => registry_policy_response(runtime, &server_id, stored, scope.agent()).await,
        Err(response) => *response,
    }
}

/// `PUT …/mcp/registry/{server_id}/tools/policy`
pub(super) async fn write_tool_policy(
    company: AdminScopedCompany,
    Path(ServerIdPath { server_id }): Path<ServerIdPath>,
    Query(scope): Query<AgentScope>,
    body: Option<Json<PutToolPolicy>>,
) -> Response {
    let runtime = company.runtime.as_ref();
    let Some(mcp) = runtime.mcp() else {
        return not_wired("mcp registry");
    };
    // Membership first, as every other write here does: an unknown id must not
    // leave a policy document behind for an install that does not exist.
    if let Err(error) = mcp.get(&server_id) {
        return ApiError(error).into_response();
    }
    if let Some(agent) = scope.agent()
        && let Err(response) = require_roster_agent(runtime, agent).await
    {
        return *response;
    }
    let stored = match stored_strict(runtime, &server_id).await {
        Ok(stored) => stored,
        Err(response) => return *response,
    };
    let Some(Json(patch)) = body else {
        return ApiError(OpenCompanyError::InvalidRequest(
            "send a JSON body naming `tierDefaults`, `tools`, or both.".to_string(),
        ))
        .into_response();
    };
    let merged = match apply_tool_policy_patch(stored, patch, scope.agent()) {
        Ok(merged) => merged,
        Err(reason) => return ApiError(OpenCompanyError::InvalidRequest(reason)).into_response(),
    };
    if let Err(error) = mcp_policy::save_tool_policies(
        runtime.id(),
        runtime.secrets().as_ref(),
        &mcp_policy::registry_tool_policies_key(&server_id),
        &merged,
    )
    .await
    {
        return ApiError(error).into_response();
    }
    registry_policy_response(runtime, &server_id, merged, scope.agent()).await
}

/// `DELETE …/mcp/registry/{server_id}/tools/policy`
pub(super) async fn reset_tool_policy(
    company: AdminScopedCompany,
    Path(ServerIdPath { server_id }): Path<ServerIdPath>,
    Query(scope): Query<AgentScope>,
) -> Response {
    let runtime = company.runtime.as_ref();
    let Some(mcp) = runtime.mcp() else {
        return not_wired("mcp registry");
    };
    if let Err(error) = mcp.get(&server_id) {
        return ApiError(error).into_response();
    }
    // An agent-scoped reset clears one teammate's rules and leaves the company
    // document alone, so it has to read first — which makes an unreadable
    // document a `409` here, with the company-scoped reset still the repair.
    let merged = match scope.agent() {
        Some(agent) => {
            let mut stored = match stored_strict(runtime, &server_id).await {
                Ok(stored) => stored,
                Err(response) => return *response,
            };
            stored.agents.remove(agent);
            stored.prune();
            if let Err(error) = mcp_policy::save_tool_policies(
                runtime.id(),
                runtime.secrets().as_ref(),
                &mcp_policy::registry_tool_policies_key(&server_id),
                &stored,
            )
            .await
            {
                return ApiError(error).into_response();
            }
            stored
        }
        None => {
            match mcp_policy::reset_company_policy(
                runtime.id(),
                runtime.secrets().as_ref(),
                &mcp_policy::registry_tool_policies_key(&server_id),
            )
            .await
            {
                Ok(replacement) => replacement,
                Err(error) => return ApiError(error).into_response(),
            }
        }
    };
    registry_policy_response(runtime, &server_id, merged, scope.agent()).await
}
