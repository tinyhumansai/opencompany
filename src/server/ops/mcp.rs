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
use crate::metering::roster_display_names;
use crate::ports::types::CompanyRecord;
use crate::runtime::builder::agent_scoped_grants;
use crate::runtime::tools::grants_cover_server;
use crate::server::error::ApiError;
use crate::server::ops::{AdminScopedCompany, ScopedCompany, mcp_registry, scoped};

/// The reminder attached to every mutating response: the effective MCP set is
/// re-resolved and fingerprinted on every harness cycle (`HarnessPool::ensure`),
/// so an edit reaches agents on the company's next turn with no restart. The
/// `mcp_fingerprint` staleness term is what makes this a property of the design.
pub(super) const NEXT_TURN_NOTE: &str =
    "Agents pick up this change on their next turn — no restart needed.";

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
///
/// Since issue #1270 this shape also carries a registry install. The four
/// registry-only fields are all `Option` + `skip_serializing_if`, so a
/// manifest / default / runtime row serializes **byte-identically** to what it
/// did before — the console's existing readers cannot tell the change happened.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct McpServerDto {
    pub(super) name: String,
    pub(super) endpoint: String,
    pub(super) description: Option<String>,
    /// `manifest` (committed), `runtime` (console-added), `default` (shipped by
    /// the install — issue #527), or `registry` (installed from an upstream MCP
    /// directory — issue #1270). The console renders this as the source badge,
    /// so the four stay distinguishable: a shipped default is not something this
    /// operator added, and must not be labelled as if it were.
    ///
    /// On a **reconciled** row — one server that is both installed from the
    /// directory and declared/typed in List A — this is the List A provenance.
    /// See [`mcp_registry::merge_installs`](super::mcp_registry::merge_installs)
    /// for why that side wins.
    pub(super) source: McpSource,
    pub(super) enabled: bool,
    pub(super) allowed_tools: Vec<String>,
    pub(super) disallowed_tools: Vec<String>,
    /// Remote tool names the operator declared read-only on this server (issue
    /// #1124). The console renders this beside the two lists above, with the same
    /// source badge, so an operator sees which layer declared it.
    pub(super) read_only_tools: Vec<String>,
    pub(super) timeout_secs: u64,
    /// Whether an outbound credential is stored — never the credential itself.
    ///
    /// On a reconciled row this is the **union**: either List A's token slot or
    /// the registry install's env values counts. The two are separate stores
    /// dialled by separate transports, and there is no merging them; what the
    /// field claims — "a credential is stored for this server" — stays true
    /// either way, and the alternative (reporting only List A's slot) would
    /// print "no credential" over a server that authenticates fine.
    pub(super) auth_configured: bool,
    /// The stable install id, present only on a row backed by a registry
    /// install. Registry rows are keyed by this and **not** by `name`: `name` is
    /// a display slug this surface mints, while `serverId` is what every
    /// `…/mcp/registry/{serverId}/…` route and OpenHuman's own store address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) server_id: Option<String>,
    /// The directory's qualified name (`@org/server`), when this row came from
    /// one. The catalogue's stable identity, and what an install is re-keyed on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) qualified_name: Option<String>,
    /// The directory's icon, when this row came from one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) icon_url: Option<String>,
    /// How a registry install is dialled — `http_remote` or `stdio`. Absent on
    /// a List A-only row, which is always HTTP by construction (`command` is a
    /// validation error there).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) transport: Option<String>,
    /// The company's agents whose effective tool grants cover this server — who
    /// can actually call it (issue #568). Computed over the same roster the
    /// harness builds (manifest agents + promoted overlay teammates), through the
    /// shared [`grants_cover_server`](crate::runtime::tools::grants_cover_server),
    /// so the console cannot disagree with the harness about reachability. **An
    /// empty list is meaningful**: an *enabled*, healthy server no teammate can
    /// reach is almost always a misconfiguration, and the console flags it rather
    /// than showing an empty list silently. A **disabled** server is always empty
    /// — the harness hands out no tool for it whatever the grants say — so the
    /// console reads the empty case against `enabled` and stays quiet there.
    /// Always serialized (even when empty).
    ///
    /// A **registry** row lists the whole roster: `build.rs` pushes the registry
    /// bridge tools into every agent's toolbelt with no grant check, so every
    /// teammate really can call every installed server. Issue #1270 leaves that
    /// asymmetry in place deliberately and makes it operator-visible here rather
    /// than inventing a grant filter the harness does not apply.
    pub(super) reachable_by: Vec<RosterAgentDto>,
    /// The last recorded probe outcome (scrubbed), or `None` when never probed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) health: Option<McpHealth>,
}

/// One roster agent named on a coverage line, carried as an id **and** the label
/// the console prints (issue #931).
///
/// The id alone was the whole payload until #931: readable for a manifest agent,
/// whose id is an authored slug, but a minted `{millis}-{counter}` string for an
/// operator-added overlay teammate — so the console's "Reachable by" line printed
/// blueprint slugs next to raw internal ids. `name` is the same display label the
/// Team page and the usage buckets use ([`roster_display_names`]: a manifest
/// agent's `role`, an overlay teammate's `name`); the id stays so a client can
/// still key or link on it.
///
/// `pub(super)` and named for the roster rather than for MCP because it rides
/// out of [`roster_grants`], which the repositories surface reads too (issue
/// #245) — its "Readable by" line is the same sentence about a different
/// namespace and printed the same raw ids.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RosterAgentDto {
    pub(super) id: String,
    pub(super) name: String,
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
    /// Remote tool names to declare read-only on this server (issue #1124).
    #[serde(default)]
    read_only_tools: Vec<String>,
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
    /// Replace the read-only declaration (issue #1124). Omit to leave it
    /// unchanged; send `[]` to clear it.
    #[serde(default)]
    read_only_tools: Option<Vec<String>>,
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
pub(super) async fn manifest_servers(runtime: &CompanyRuntime) -> Result<Vec<McpServer>, ApiError> {
    let record = runtime.store().load(runtime.id()).await.map_err(ApiError)?;
    Ok(record.map(|r| r.manifest.mcp_servers).unwrap_or_default())
}

/// Projects an effective decl (already merged + auth-resolved) to the console
/// DTO, reducing the resolved credential to a boolean, listing the agents that
/// can reach it (issue #568), and attaching the last (scrubbed) probe health.
fn dto_from_decl(
    decl: &mcp::McpServerDecl,
    reachable_by: Vec<RosterAgentDto>,
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
        read_only_tools: decl.read_only_tools.clone(),
        timeout_secs: decl.timeout_secs,
        auth_configured: decl.auth.is_configured(),
        // A List A decl knows nothing about a directory install; the merge pass
        // fills these in when one reconciles onto this row.
        server_id: None,
        qualified_name: None,
        icon_url: None,
        transport: None,
        reachable_by,
        health,
    }
}

/// Every roster agent's *effective* tool grants (issue #568), as
/// `(agent, grants)`.
///
/// `pub(super)` since issue #245: the repositories surface answers the same
/// question about a different namespace ("who can read this?"), and a second
/// roster walk beside this one is exactly how the two consoles would come to
/// disagree with each other and with the harness. The roster is exactly what the harness builds in
/// `build_roster` — the **effective** roster, not the blueprint's: the manifest
/// agents with every operator edit applied (each with its own `tools` narrowed
/// by the company `allow` **and** by the desks it sits on — issue #1674), plus
/// the promoted overlay teammates — each narrowed by
/// **its own** `tools` line the same way (issue #661), which for the common
/// empty line is still the full company `allow`, the standard grant
/// `overlay_agent_to_manifest` gives it. An overlay id already claimed by a
/// manifest agent is skipped, both mirroring the harness so console
/// reachability equals what an agent is actually granted.
///
/// Each agent carries its display label alongside its id (issue #931), resolved
/// through [`roster_display_names`] — the same map the Team page and the usage
/// buckets read, so one teammate is named identically everywhere in the console.
/// An id absent from that map (it cannot be, over this roster) falls back to the
/// id, matching `bucket_usage`.
pub(super) fn roster_grants(record: &CompanyRecord) -> Vec<(RosterAgentDto, Vec<String>)> {
    let allow = &record.manifest.tools.allow;
    // The roster as it *effectively* stands, exactly as `build_roster` builds
    // it: a teammate an operator edited keeps its edits (an override `tools`
    // line replaces the manifest's), and a retired teammate is not on the
    // roster at all. Reading the raw manifest half here made `reachableBy`
    // track the blueprint while the harness and the Team tab's Tools card
    // tracked the edit — so granting or revoking `mcp:*` on a manifest
    // teammate never moved the Connections surface.
    let effective = record.effective_agents();
    let names = roster_display_names(&effective, &record.overlay_agents);
    let roster_agent = |id: &str| RosterAgentDto {
        id: id.to_string(),
        name: names.get(id).cloned().unwrap_or_else(|| id.to_string()),
    };
    // Three-level narrowing, exactly as `build_roster` builds the roster (issue
    // #1674): company `allow` → the desks this teammate sits on → the teammate's
    // own `tools`. `agent_desk_tools` resolves through the record's *effective*
    // desk membership, so a console-seated member is scoped by its desk as a
    // manifest one is. Without this level a teammate on a desk whose ceiling
    // omits `mcp:*` still read back here as reaching every server the company
    // grants, while the harness gives it no such tools.
    let desk_narrowed = |id: &str, tools: Option<&[String]>| {
        let desk_tools = record.agent_desk_tools(id);
        let desk_refs: Vec<&[String]> = desk_tools.iter().map(Vec::as_slice).collect();
        agent_scoped_grants(allow, &desk_refs, tools)
    };
    let mut grants: Vec<(RosterAgentDto, Vec<String>)> = effective
        .iter()
        .map(|agent| {
            (
                roster_agent(&agent.id),
                desk_narrowed(&agent.id, agent.tools.as_deref()),
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
        // The overlay teammate's **own** tools line, read through the same
        // function and with the same empty-means-inherit rule as the manifest
        // half above — matching `overlay_agent_to_manifest` (issue #740).
        //
        // This read was hard-coded empty until #661 gave `OverlayAgent` a tools
        // list. The comment that stood here ("no manifest tools row → the
        // company's standard grant") described a fact that expired with that
        // change, which is why it read as a decision rather than a stale
        // assumption: a scoped teammate reported as reaching every enabled
        // server, and the console asserted a connection the harness does not
        // grant.
        grants.push((
            roster_agent(&overlay.id),
            desk_narrowed(&overlay.id, overlay.tools.as_deref()),
        ));
    }
    grants
}

/// The agents whose effective `grants` reach `decl` (issue #568), read through
/// the shared [`grants_cover_server`] so this agrees with the harness registry.
/// Empty ⇒ no teammate can reach the server.
///
/// A **disabled** server reaches nobody regardless of grants: `registry_for_agent`
/// filters on `decl.enabled && grants_cover_server(..)`, so an agent granted
/// `mcp:<slug>` still gets no such tool while the server is off. Mirroring both
/// halves of that filter here is what keeps the console from claiming a
/// reachability the harness does not hand out.
fn reachers_of(
    roster_grants: &[(RosterAgentDto, Vec<String>)],
    decl: &mcp::McpServerDecl,
) -> Vec<RosterAgentDto> {
    if !decl.enabled {
        return Vec::new();
    }
    roster_grants
        .iter()
        .filter(|(_, grants)| grants_cover_server(grants, &decl.name))
        .map(|(agent, _)| agent.clone())
        .collect()
}

/// The company's whole MCP surface as one list: the declared servers (manifest ∪
/// install defaults ∪ runtime index) with the directory installs folded in.
///
/// Issue #1270. Every reader of this list — the `GET`, the List A mutation
/// response, the delete dispatch, and the registry mutation responses — goes
/// through here, so no two of them can disagree about a row's name, badge or
/// health. That matters most for the reconciled case: a server that is both
/// installed and declared has exactly one identity, and it has to be the same
/// identity in the response to the write that created it as in the read that
/// follows.
pub(super) async fn merged_rows(runtime: &CompanyRuntime) -> Result<Vec<McpServerDto>, ApiError> {
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
    // The directory half. A registry that cannot be read yields nothing and the
    // declared servers stand on their own — see `mcp_registry::installs`.
    let roster: Vec<RosterAgentDto> = grants.iter().map(|(agent, _)| agent.clone()).collect();
    mcp_registry::merge_installs(&mut out, mcp_registry::installs(runtime).await, &roster);
    Ok(out)
}

/// `GET …/mcp/servers` — the company's effective MCP servers, each with its last
/// recorded (scrubbed) probe health.
async fn list_servers(company: ScopedCompany) -> Result<Json<Vec<McpServerDto>>, ApiError> {
    merged_rows(company.runtime.as_ref()).await.map(Json)
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
        read_only_tools: body.read_only_tools.clone(),
        timeout_secs: body.timeout_secs.unwrap_or(30),
        enabled: true,
        auth_secret: None,
    };
    reject_invalid(&format!("mcp server `{name}`"), &server)?;

    // A manifest-declared name is not a runtime add — update it to override.
    let manifest = manifest_servers(runtime).await?;
    if manifest.iter().any(|m| m.name.trim() == name) {
        return Err(ApiError(OpenCompanyError::Conflict(format!(
            "`{name}` is declared in this company's bundle (`company.toml` or `mcp.json`) — update it to override, don't re-add it."
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
    if let Some(read_only) = patch.read_only_tools.clone() {
        server.read_only_tools = read_only;
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

/// `DELETE …/mcp/servers/{name}` — remove a server (409 for a manifest or
/// default server, which can only be disabled).
///
/// **Dispatches on where the row actually lives** (issue #1270). Dropping a
/// runtime-index row is the right removal for a server an operator typed in, and
/// the wrong one for a directory install: the install lives in OpenHuman's own
/// store, keyed by `server_id`, and it stays connected — with its tools on every
/// agent's belt — no matter what this company's index says. So a row backed by
/// an install is uninstalled there, and a **reconciled** row (typed in *and*
/// installed) has both halves removed, because a delete that leaves the server
/// callable is not a delete.
async fn delete_server(
    company: AdminScopedCompany,
    Path(NamePath { name }): Path<NamePath>,
) -> Result<StatusCode, ApiError> {
    let runtime = company.runtime.as_ref();
    let name = name.trim().to_string();

    let manifest = manifest_servers(runtime).await?;
    if manifest.iter().any(|m| m.name.trim() == name) {
        return Err(ApiError(OpenCompanyError::Conflict(format!(
            "`{name}` is declared in this company's bundle (`company.toml` or `mcp.json`) — disable it instead of deleting."
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

    // Which halves this row has. Read from the same merged list the console
    // rendered, so the delete targets the row the operator actually saw.
    let install_id = merged_rows(runtime)
        .await?
        .into_iter()
        .find(|row| row.name == name)
        .and_then(|row| row.server_id);

    let mut index = load_runtime_index(runtime.id(), runtime.secrets().as_ref())
        .await
        .map_err(ApiError)?;
    let before = index.len();
    index.retain(|s| s.name.trim() != name);
    let removal = mcp_registry::removal_for(index.len() != before, install_id.is_some());
    if removal == mcp_registry::Removal::NotFound {
        return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
            "no runtime MCP server named `{name}`."
        ))));
    }
    if matches!(
        removal,
        mcp_registry::Removal::IndexRow | mcp_registry::Removal::Both
    ) {
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
    }
    if matches!(
        removal,
        mcp_registry::Removal::Install | mcp_registry::Removal::Both
    ) && let Some(install_id) = install_id
    {
        mcp_registry::remove_install(runtime, &install_id).await?;
    }
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

    // Re-read the same merged list a later `GET` will serve — the three declared
    // layers (defaults under manifest under runtime) plus any directory install
    // that reconciles onto this endpoint. Projecting the decl alone would answer
    // an add-by-URL of an already-installed server with a row missing the
    // `serverId` the very next read shows (issue #1270).
    let server = merged_rows(runtime)
        .await?
        .into_iter()
        .find(|row| row.name == name)
        .ok_or_else(|| {
            ApiError(OpenCompanyError::InvalidRequest(format!(
                "`{name}` not found"
            )))
        })?;
    Ok(Json(MutationResponse {
        server,
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
pub(super) fn reject_invalid(label: &str, server: &McpServer) -> Result<(), ApiError> {
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
    use crate::ports::types::{CompanyId, OverlayAgent, OverlayDesk};

    /// A company allowing two MCP families, with one manifest agent that lists
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
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
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
            overlay_tool_grants: None,
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
            setup: None,
            name_confirmed: false,
            activation_completed_at: None,
            created_at_millis: None,
        }
    }

    fn teammate(id: &str, tools: Vec<&str>) -> OverlayAgent {
        // An empty argument list means "the standard grant" (`None`), matching
        // every caller's pre-#1804 intent; a non-empty list is a narrowed grant.
        let tools: Vec<String> = tools.into_iter().map(str::to_string).collect();
        OverlayAgent {
            id: id.to_string(),
            name: id.to_string(),
            role: "Growth".to_string(),
            description: None,
            tools: (!tools.is_empty()).then_some(tools),
            model: None,
            harness: None,
        }
    }

    /// Issue #740: a scoped overlay teammate must not read back here as
    /// reaching everything.
    ///
    /// `roster_grants` is what every MCP server row's `reachableBy` is computed
    /// from. #661 gave `OverlayAgent` a tools list and taught two of the three
    /// readers to honour it; this one still passed an empty grant, so a
    /// teammate scoped to one server reported as reaching all of them — the
    /// console asserting a connection the harness does not grant.
    #[test]
    fn a_scoped_overlay_teammate_does_not_read_back_as_reaching_everything() {
        let scoped = record(vec![teammate("jamie", vec!["mcp:notion"])]);
        let grants = roster_grants(&scoped);
        let jamie = grants
            .iter()
            .find(|(agent, _)| agent.id == "jamie")
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
            .find(|(agent, _)| agent.id == "ceo")
            .expect("on roster");
        assert_eq!(
            ceo.1,
            vec!["mcp:notion".to_string(), "mcp:linear".to_string()]
        );
    }

    /// Issue #931: every roster entry carries a printable label, so no console
    /// coverage line has to fall back to the id.
    ///
    /// The id is the wrong thing to print for exactly the teammates an operator
    /// created: `POST …/team` mints `{millis:012x}-{counter:012x}`, which is
    /// what "Reachable by" and "Readable by" showed. The two halves resolve
    /// differently and both are asserted — a manifest agent has no name of its
    /// own, so its label is its `role`; an overlay teammate's is its `name`.
    #[test]
    fn every_roster_entry_carries_a_printable_label() {
        let minted = "019fa75dbc9b-000000000001";
        let mut teammate = teammate(minted, Vec::new());
        teammate.name = "Jamie".to_string();
        let grants = roster_grants(&record(vec![teammate]));
        let label = |id: &str| {
            grants
                .iter()
                .find(|(agent, _)| agent.id == id)
                .unwrap_or_else(|| panic!("`{id}` is on the roster"))
                .0
                .name
                .clone()
        };
        assert_eq!(label(minted), "Jamie", "an overlay teammate's own name");
        assert_eq!(
            label("ceo"),
            "Chief Executive",
            "a manifest agent has no name of its own, so its role is the label"
        );
    }

    /// The empty-means-inherit rule (#264) is untouched: a teammate written
    /// before #661, and every teammate created without a scope, still reads
    /// back holding the company's whole grant.
    #[test]
    fn an_unscoped_overlay_teammate_still_inherits_the_company_grant() {
        let grants = roster_grants(&record(vec![teammate("jamie", Vec::new())]));
        let jamie = grants
            .iter()
            .find(|(agent, _)| agent.id == "jamie")
            .expect("roster");
        assert_eq!(
            jamie.1,
            vec!["mcp:notion".to_string(), "mcp:linear".to_string()]
        );
    }

    /// Issue #1674: a teammate seated on a desk whose `tools` ceiling omits
    /// `mcp:*` must not read back as reaching every server the company grants.
    /// `roster_grants` is what every MCP server row's `reachableBy` is computed
    /// from, and the harness scopes the same agent by its desk (`build_roster`'s
    /// three-level narrowing) — so a company that grants `mcp:*` while a desk
    /// omits MCP would otherwise list that desk's teammates as reaching servers
    /// they cannot call.
    #[test]
    fn a_desk_ceiling_that_omits_mcp_narrows_reachability() {
        let mut scoped = record(vec![teammate("jamie", Vec::new())]);
        scoped.overlay_desks.push(OverlayDesk {
            id: "creative".to_string(),
            name: "Creative".to_string(),
            description: None,
            members: vec!["jamie".to_string()],
            responder: crate::ports::types::ResponderMode::default(),
        });
        scoped
            .overlay_desk_tools
            .insert("creative".to_string(), vec!["mcp:notion".to_string()]);
        let grants = roster_grants(&scoped);
        let jamie = grants
            .iter()
            .find(|(agent, _)| agent.id == "jamie")
            .expect("the overlay teammate is on the roster");
        assert_eq!(
            jamie.1,
            vec!["mcp:notion".to_string()],
            "the desk ceiling narrows reachability below the company grant"
        );

        // The manifest agent on no desk still inherits the whole company grant,
        // so the narrowing above is the desk's and not a company change.
        let ceo = grants
            .iter()
            .find(|(agent, _)| agent.id == "ceo")
            .expect("on roster");
        assert_eq!(
            ceo.1,
            vec!["mcp:notion".to_string(), "mcp:linear".to_string()]
        );
    }

    /// An operator's `tools` edit to a **manifest** teammate is the grant the
    /// harness reads: `PATCH …/team/{id}` stores the edit as an override, and
    /// `roster_grants` derives reachability from the effective roster just like
    /// `build_roster` does — so granting or revoking `mcp:*` on the Tools card
    /// moves the Connections surface rather than leaving it pinned to the
    /// blueprint's `[[agent]].tools` line.
    #[test]
    fn a_manifest_teammates_tools_edit_reaches_the_roster() {
        let mut scoped = record(vec![]);
        scoped
            .overlay_agent_edits
            .push(crate::ports::types::AgentOverride {
                agent_id: "ceo".to_string(),
                // Double-option since #1804: `Some(Some(globs))` narrows.
                tools: Some(Some(vec!["mcp:notion".to_string()])),
                ..Default::default()
            });
        let grants = roster_grants(&scoped);
        let ceo = grants
            .iter()
            .find(|(agent, _)| agent.id == "ceo")
            .expect("on roster");
        assert_eq!(
            ceo.1,
            vec!["mcp:notion".to_string()],
            "an override `tools` line replaces the manifest's, narrowed by allow"
        );

        // Resetting the override to the standard grant (`Some(None)` since
        // #1804 — NOT `Some(Some([]))`, which is a deny-all) restores the
        // blueprint-wide reachability.
        let mut inherited = record(vec![]);
        inherited
            .overlay_agent_edits
            .push(crate::ports::types::AgentOverride {
                agent_id: "ceo".to_string(),
                tools: Some(None),
                ..Default::default()
            });
        let grants = roster_grants(&inherited);
        let ceo = grants
            .iter()
            .find(|(agent, _)| agent.id == "ceo")
            .expect("on roster");
        assert_eq!(
            ceo.1,
            vec!["mcp:notion".to_string(), "mcp:linear".to_string()]
        );
    }

    /// A retired manifest teammate is not on the effective roster, so it cannot
    /// be listed as reaching a server — the same roster `build_roster` builds,
    /// where a removed teammate is not built at all.
    #[test]
    fn a_retired_manifest_teammate_is_not_a_reacher() {
        let mut retired = record(vec![]);
        retired.overlay_retired_agents.push("ceo".to_string());
        let grants = roster_grants(&retired);
        assert!(
            !grants.iter().any(|(agent, _)| agent.id == "ceo"),
            "a retired teammate is off the effective roster"
        );
    }
}
