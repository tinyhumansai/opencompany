//! The MCP **directory** surface (issue #1270): browse two upstream registries,
//! install from them, and fold those installs into the one server list the
//! console already renders.
//!
//! ## Why this module exists
//!
//! There were two MCP server lists in every tenant and only one of them was
//! reachable. [`mcp`](super::mcp) serves **List A** — `company.toml`'s
//! `[[mcp_server]]` entries, the install-wide `[[default_mcp_server]]` layer
//! (issue #527), and the runtime index an operator types URLs into. List A can
//! only ever contain what somebody already knew the address of, so the tab is
//! empty until it is pasted into.
//!
//! **List B** is [`McpRuntime`](crate::harness::mcp::McpRuntime), a wrapper over
//! OpenHuman's own MCP registry: two upstream directories (Smithery.ai and
//! `modelcontextprotocol/registry`), a SQLite store of installs, named
//! write-only env credentials, and a boot-time connect + supervisor. It is
//! constructed for every company and, before this module, called by nothing in
//! `src/server/`.
//!
//! ## One list, not two sections
//!
//! `GET …/mcp/servers` returns both, each row badged with its provenance. A
//! server present in both — installed from the directory *and* typed in by URL —
//! is **one reconciled row**; see [`merge_installs`] for the matching rule and
//! for which side's provenance survives.
//!
//! ## What never crosses the wire
//!
//! Env values are write-only exactly like List A's `token`: they go in through
//! install / `PUT …/env` and come back only as an `authConfigured` bool. Nothing
//! upstream hands back is forwarded blind — the search, entry and status
//! projections below name every field they emit, so a new key appearing in an
//! upstream payload (both DTOs carry a `#[serde(flatten)] extra` map) cannot
//! reach a response by default. [`health_from_status`] explains why an install's
//! `last_error` is dropped rather than scrubbed.
//!
//! ## Feature gate
//!
//! Everything that touches the registry is `#[cfg(feature = "mcp")]`, matching
//! `…/mcp/servers/{name}/oauth/start`. A build without it registers the same
//! routes and answers `not_wired`, and List A is served unchanged.

use std::collections::{HashMap, HashSet};

use axum::Router;
use axum::routing::{delete, get, post, put};

use crate::AppState;
use crate::company::McpServer;
use crate::company::mcp::{McpHealth, McpSource};
use crate::company::mcp_endpoint::normalize_endpoint;
use crate::server::ops::mcp::{McpServerDto, RosterAgentDto};
use crate::server::ops::scoped;

/// The per-request timeout a registry row reports.
///
/// OpenHuman's install record has no timeout of its own — the connection map
/// owns that — so the row reports the same 30s `POST …/mcp/servers` defaults to,
/// rather than inventing a second number the console would have to explain.
const REGISTRY_TIMEOUT_SECS: u64 = 30;

/// Builds the registry route fragment. Merged into [`super::mcp::router`] so the
/// whole MCP surface is registered from one place.
pub(super) fn router() -> Router<AppState> {
    scoped("/mcp/registry/search", get(search))
        .merge(scoped("/mcp/registry/entry", get(entry)))
        .merge(scoped("/mcp/registry/install", post(install)))
        .merge(scoped(
            "/mcp/registry/{server_id}/connect",
            post(connect_server),
        ))
        .merge(scoped(
            "/mcp/registry/{server_id}/disconnect",
            post(disconnect_server),
        ))
        .merge(scoped("/mcp/registry/{server_id}/env", put(update_env)))
        .merge(scoped("/mcp/registry/{server_id}", delete(uninstall)))
        .merge(scoped(
            "/mcp/registry/{server_id}/tools/policy",
            get(read_tool_policy)
                .put(write_tool_policy)
                .delete(reset_tool_policy),
        ))
}

// ---------------------------------------------------------------------------
// Projection of one install — always compiled
// ---------------------------------------------------------------------------

/// One registry install, projected out of OpenHuman's store into the shape the
/// merged read needs.
///
/// Deliberately a plain struct rather than the upstream `InstalledServer`: the
/// upstream type lives behind the `mcp` feature, and every rule worth testing
/// here — endpoint matching, name minting, which side of a reconciled row wins —
/// is arithmetic over these fields. Keeping them feature-free means the
/// reconciliation tests run in the ungated lane instead of only in the filtered
/// belt lane, where a gated test can sit compiled and unexecuted (issue #770).
#[derive(Debug, Clone)]
pub(super) struct RegistryInstall {
    /// The stable install id — how every registry route addresses this server.
    pub(super) server_id: String,
    /// The directory's qualified name, e.g. `@modelcontextprotocol/server-git`.
    pub(super) qualified_name: String,
    /// The directory's display name; the seed for the row's `name` slug.
    pub(super) display_name: String,
    pub(super) description: Option<String>,
    pub(super) icon_url: Option<String>,
    /// The install's `deployment_url`. `None` for a stdio install — which this
    /// deployment refuses to create, but which a store written by an older build
    /// or by OpenHuman's own setup agent can still contain.
    pub(super) endpoint: Option<String>,
    /// `http_remote` or `stdio`.
    pub(super) transport: String,
    pub(super) enabled: bool,
    /// Whether env values were supplied for this install. Derived from the
    /// install's `env_keys`, which upstream populates from the keys whose values
    /// were persisted — so this answers "is a credential stored" without ever
    /// reading one.
    pub(super) auth_configured: bool,
    /// The live connection state, already mapped by [`health_from_status`].
    pub(super) health: Option<McpHealth>,
}

// ---------------------------------------------------------------------------
// Endpoint reconciliation — always compiled
// ---------------------------------------------------------------------------

/// A display slug for a registry row, derived from its qualified name.
///
/// Registry installs are keyed by `server_id`, not by name, so this is purely
/// the label the console prints and the `DELETE …/mcp/servers/{name}` path
/// segment. It still has to be **unique within the list**: the console keys rows
/// by `name`, and two rows sharing one would render as a single flickering row
/// and delete the wrong server. Collisions fall back to a server-id suffix,
/// which is stable across reads rather than positional.
pub(super) fn registry_row_name(install: &RegistryInstall, taken: &HashSet<String>) -> String {
    let base = slugify(&install.qualified_name)
        .or_else(|| slugify(&install.display_name))
        .unwrap_or_else(|| "mcp-server".to_string());
    if !taken.contains(&base) {
        return base;
    }
    let suffix: String = install
        .server_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect();
    let with_id = format!("{base}-{suffix}");
    if !taken.contains(&with_id) {
        return with_id;
    }
    // Two installs of the same service whose ids also share a prefix. Vanishingly
    // unlikely, but a duplicate name is a wrong-row delete, so it is not left to
    // chance.
    (2..)
        .map(|n| format!("{with_id}-{n}"))
        .find(|candidate| !taken.contains(candidate))
        .unwrap_or(with_id)
}

/// Lowercase, alphanumeric-and-dashes. `None` when nothing survives.
fn slugify(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The directory entry as this company's own server declaration.
///
/// Lives here rather than beside the route so it compiles and is tested
/// without the `mcp` feature, like every other rule in this module.
///
/// The qualified name is the row's name, which is what the directory, the
/// console's source badge and a later `PUT …/mcp/servers/{name}` all agree on.
/// Tool lists are left empty — the directory says nothing about which of a
/// server's tools this company wants, and an empty pair means "all of them",
/// which is what an install has always meant.
///
/// Its only caller is the route in `wired`, which is `#[cfg(feature = "mcp")]`
/// — so on a build without that feature the rule is exercised by the tests
/// below and by nothing else, which is the shape this module is for.
#[cfg_attr(not(feature = "mcp"), allow(dead_code))]
pub(super) fn declaration_from_directory(
    qualified_name: &str,
    endpoint: &str,
    description: Option<String>,
) -> McpServer {
    McpServer {
        name: qualified_name.to_string(),
        endpoint: endpoint.to_string(),
        description,
        command: None,
        allowed_tools: Vec::new(),
        disallowed_tools: Vec::new(),
        read_only_tools: Vec::new(),
        timeout_secs: 30,
        enabled: true,
        auth_secret: None,
    }
}

/// Folds registry installs into the List A rows, in place.
///
/// # The reconciliation rule
///
/// An install matches a List A row when their [`normalize_endpoint`] values are
/// equal. A match **adopts** — the registry's identity fields are attached to
/// the existing row and no second row is pushed. Everything else stays List A's.
///
/// ## Why List A's provenance wins
///
/// `source` decides two things the console acts on: which badge it prints, and
/// whether it offers a delete. Both have to answer to List A.
///
/// A manifest or default server **cannot be deleted**, only disabled — the
/// declaration lives in `company.toml` or the instance config, so dropping the
/// row would not remove it and the next read would merge it straight back. If a
/// directory install could capture that row and relabel it `registry`, the
/// console would offer a delete that uninstalls the directory copy and leaves
/// the declared server exactly where it was.
///
/// The deeper reason is that List A is what the *agents* actually reach.
/// `registry_for_agent` builds each agent's MCP registry from the List A decls
/// and scopes it by `mcp:<name>` grants; the row's `name`, `enabled`, tool lists
/// and credential all govern that path. A row relabelled `registry` would stop
/// offering the controls that decide what the company's agents can call.
///
/// Nothing is lost by adopting: the install is still addressable — `serverId` is
/// on the row, and every `…/mcp/registry/…` route keys on it — and
/// [`super::mcp::delete_server`] uninstalls both halves when the reconciled row
/// is a deletable one.
///
/// ## What the registry contributes to a reconciled row
///
/// Only what List A structurally has no field for: `serverId`, `qualifiedName`,
/// `iconUrl`, `transport`; a `description` when List A carries none; a `health`
/// when List A has never probed. Health prefers List A's probe because that
/// probe dials the endpoint the way the agents' bridge tools do, credential
/// included.
pub(super) fn merge_installs(
    rows: &mut Vec<McpServerDto>,
    installs: Vec<RegistryInstall>,
    roster: &[RosterAgentDto],
) {
    let mut by_endpoint: HashMap<String, usize> = HashMap::new();
    for (index, row) in rows.iter().enumerate() {
        if let Some(key) = normalize_endpoint(&row.endpoint) {
            by_endpoint.entry(key).or_insert(index);
        }
    }
    let mut taken: HashSet<String> = rows.iter().map(|row| row.name.clone()).collect();

    for install in installs {
        let key = install.endpoint.as_deref().and_then(normalize_endpoint);
        if let Some(key) = &key
            && let Some(&index) = by_endpoint.get(key)
        {
            adopt(&mut rows[index], install);
            continue;
        }
        let name = registry_row_name(&install, &taken);
        taken.insert(name.clone());
        rows.push(row_from_install(install, name, roster));
        if let Some(key) = key {
            by_endpoint.entry(key).or_insert(rows.len() - 1);
        }
    }
}

/// Attaches an install's identity to a List A row it reconciles with.
fn adopt(row: &mut McpServerDto, install: RegistryInstall) {
    row.server_id = Some(install.server_id);
    row.qualified_name = Some(install.qualified_name);
    row.transport = Some(install.transport);
    if row.icon_url.is_none() {
        row.icon_url = install.icon_url;
    }
    if row.description.is_none() {
        row.description = install.description;
    }
    if row.health.is_none() {
        row.health = install.health;
    }
    // The union, per `McpServerDto::auth_configured`.
    row.auth_configured = row.auth_configured || install.auth_configured;
}

/// Projects an install that reconciled with nothing into its own row.
fn row_from_install(
    install: RegistryInstall,
    name: String,
    roster: &[RosterAgentDto],
) -> McpServerDto {
    McpServerDto {
        name,
        endpoint: install.endpoint.unwrap_or_default(),
        description: install.description,
        source: McpSource::Registry,
        enabled: install.enabled,
        // The directory does not express per-tool allow/deny or a read-only
        // declaration (issue #1124), and OpenHuman's bridge tools do not consult
        // one here. Empty is the truthful projection, not a placeholder.
        allowed_tools: Vec::new(),
        disallowed_tools: Vec::new(),
        read_only_tools: Vec::new(),
        timeout_secs: REGISTRY_TIMEOUT_SECS,
        auth_configured: install.auth_configured,
        server_id: Some(install.server_id),
        qualified_name: Some(install.qualified_name),
        icon_url: install.icon_url,
        transport: Some(install.transport),
        // Every teammate, or nobody when the install is off — see
        // `McpServerDto::reachable_by`.
        reachable_by: if install.enabled {
            roster.to_vec()
        } else {
            Vec::new()
        },
        health: install.health,
    }
}

// ---------------------------------------------------------------------------
// Delete dispatch — always compiled
// ---------------------------------------------------------------------------

/// What `DELETE …/mcp/servers/{name}` has to remove for a row (issue #1270).
///
/// The two lists are removed from in completely different ways — a runtime
/// server is a row in this company's secret-store index, a directory install is
/// a row in OpenHuman's SQLite store keyed by `server_id` with a live connection
/// attached — so "delete" is a dispatch, not one operation. Written as a value
/// rather than a chain of `if`s because the case that gets forgotten is
/// [`Self::Both`]: dropping only the index row leaves the install connected and
/// its tools on every agent's belt, which is a delete the operator watches fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Removal {
    /// No such row — the caller named something that does not exist.
    NotFound,
    /// A typed-in server: drop the runtime-index row and wipe its credential.
    IndexRow,
    /// A directory install and nothing else: uninstall it upstream.
    Install,
    /// Both halves of a reconciled row.
    Both,
}

/// Decides [`Removal`] from what each side holds.
///
/// Manifest and default rows never reach here — they are refused earlier, and
/// must stay refused: their declaration lives in `company.toml` or the instance
/// config, so no removal here would make them go away.
pub(super) fn removal_for(had_index_entry: bool, backed_by_install: bool) -> Removal {
    match (had_index_entry, backed_by_install) {
        (true, true) => Removal::Both,
        (true, false) => Removal::IndexRow,
        (false, true) => Removal::Install,
        (false, false) => Removal::NotFound,
    }
}

/// Directory-payload projections and the connection-state badge, split out so
/// the parent keeps only what every build needs (the merge). Compiled for the
/// wired build and for tests: nothing but the registry routes projects a
/// directory payload, and a build without `mcp` has no directory to project.
#[cfg(any(feature = "mcp", test))]
pub(in crate::server::ops) mod catalogue;

#[cfg(feature = "mcp")]
mod wired;
#[cfg(feature = "mcp")]
use wired::{
    connect_server, disconnect_server, entry, install, read_tool_policy, reset_tool_policy, search,
    uninstall, update_env, write_tool_policy,
};
#[cfg(feature = "mcp")]
pub(super) use wired::{installs, remove_install};

// ---------------------------------------------------------------------------
// Unwired build
// ---------------------------------------------------------------------------

/// Without the `mcp` feature there is no registry store and no directory client,
/// so every route reports `not_wired` and the console's browse surface degrades
/// the same way its Sign in button does.
#[cfg(not(feature = "mcp"))]
mod unwired {
    use axum::response::Response;

    use super::*;
    use crate::server::ops::{AdminScopedCompany, ScopedCompany};

    /// No registry in this build, so nothing to merge — List A is the whole list.
    pub(in crate::server::ops) async fn installs(
        _runtime: &crate::company::runtime::CompanyRuntime,
    ) -> Vec<RegistryInstall> {
        Vec::new()
    }

    /// No installs exist, so no row can be backed by one and the delete
    /// dispatch never reaches here.
    pub(in crate::server::ops) async fn remove_install(
        _runtime: &crate::company::runtime::CompanyRuntime,
        _server_id: &str,
    ) -> Result<(), crate::server::error::ApiError> {
        Ok(())
    }

    pub(super) async fn search(company: ScopedCompany) -> Response {
        let _ = company;
        crate::server::ops::not_wired("mcp registry")
    }

    pub(super) async fn entry(company: ScopedCompany) -> Response {
        let _ = company;
        crate::server::ops::not_wired("mcp registry")
    }

    pub(super) async fn install(company: AdminScopedCompany) -> Response {
        let _ = company;
        crate::server::ops::not_wired("mcp registry")
    }

    pub(super) async fn connect_server(company: AdminScopedCompany) -> Response {
        let _ = company;
        crate::server::ops::not_wired("mcp registry")
    }

    pub(super) async fn disconnect_server(company: AdminScopedCompany) -> Response {
        let _ = company;
        crate::server::ops::not_wired("mcp registry")
    }

    pub(super) async fn update_env(company: AdminScopedCompany) -> Response {
        let _ = company;
        crate::server::ops::not_wired("mcp registry")
    }

    pub(super) async fn uninstall(company: AdminScopedCompany) -> Response {
        let _ = company;
        crate::server::ops::not_wired("mcp registry")
    }

    pub(super) async fn read_tool_policy(company: ScopedCompany) -> Response {
        let _ = company;
        crate::server::ops::not_wired("mcp registry")
    }

    pub(super) async fn write_tool_policy(company: AdminScopedCompany) -> Response {
        let _ = company;
        crate::server::ops::not_wired("mcp registry")
    }

    pub(super) async fn reset_tool_policy(company: AdminScopedCompany) -> Response {
        let _ = company;
        crate::server::ops::not_wired("mcp registry")
    }
}

#[cfg(not(feature = "mcp"))]
use unwired::{
    connect_server, disconnect_server, entry, install, read_tool_policy, reset_tool_policy, search,
    uninstall, update_env, write_tool_policy,
};
#[cfg(not(feature = "mcp"))]
pub(super) use unwired::{installs, remove_install};

#[cfg(test)]
#[path = "mcp_registry/mcp_registry_tests.rs"]
mod tests;
