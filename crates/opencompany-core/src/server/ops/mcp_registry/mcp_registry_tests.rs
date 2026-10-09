//! Reconciliation, row naming, status mapping and catalogue projection.
//!
//! None of these are feature-gated. That is the point: the `mcp` feature's own
//! CI lane is filtered to two modules (`scripts/ci/feature-lanes.txt`), so a
//! test written behind `#[cfg(feature = "mcp")]` here would compile in that lane
//! and be selected by nothing — the exact silence issue #770 exists to catch.
//! Every rule worth asserting was therefore written against feature-free inputs.

use super::catalogue::*;
use super::*;
use crate::company::mcp::{McpStatus, stdio_install_refusal};

/// A List A row as `dto_from_decl` produces one.
fn declared(name: &str, endpoint: &str, source: McpSource) -> McpServerDto {
    McpServerDto {
        name: name.to_string(),
        endpoint: endpoint.to_string(),
        description: None,
        source,
        enabled: true,
        allowed_tools: Vec::new(),
        disallowed_tools: Vec::new(),
        read_only_tools: Vec::new(),
        timeout_secs: 30,
        auth_configured: false,
        server_id: None,
        qualified_name: None,
        icon_url: None,
        probed_title: None,
        probed_description: None,
        website_url: None,
        transport: None,
        reachable_by: Vec::new(),
        access_grant: String::new(),
        agent_access: Vec::new(),
        health: None,
    }
}

fn install(server_id: &str, qualified_name: &str, endpoint: Option<&str>) -> RegistryInstall {
    RegistryInstall {
        server_id: server_id.to_string(),
        qualified_name: qualified_name.to_string(),
        display_name: qualified_name.to_string(),
        description: None,
        icon_url: None,
        endpoint: endpoint.map(str::to_string),
        transport: if endpoint.is_some() {
            "http_remote".to_string()
        } else {
            "stdio".to_string()
        },
        enabled: true,
        auth_configured: false,
        health: None,
    }
}

fn agent(id: &str) -> crate::server::ops::mcp::RosterAgentDto {
    crate::server::ops::mcp::RosterAgentDto {
        id: id.to_string(),
        name: id.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Reconciliation
// ---------------------------------------------------------------------------

/// The requirement the whole merge exists for: a server the operator typed in by
/// URL **and** installed from the directory is one row, not two.
///
/// Two rows is not a cosmetic duplicate — each carries its own credential slot
/// and its own health badge, so the tab would show one server twice with
/// contradictory status and no way to tell which one the agents are using.
#[test]
fn a_server_in_both_lists_reconciles_to_one_row() {
    let mut rows = vec![declared(
        "browserbase",
        "https://api.browserbase.com/mcp",
        McpSource::Runtime,
    )];
    merge_installs(
        &mut rows,
        vec![install(
            "id-1",
            "@browserbasehq/mcp",
            Some("https://api.browserbase.com/mcp"),
        )],
    );

    assert_eq!(rows.len(), 1, "one server, one row");
    assert_eq!(rows[0].name, "browserbase", "List A keeps the row's name");
    assert_eq!(
        rows[0].source,
        McpSource::Runtime,
        "List A's provenance survives — it is what governs delete and what the \
         harness builds each agent's registry from"
    );
    assert_eq!(
        rows[0].server_id.as_deref(),
        Some("id-1"),
        "the install is still addressable on the reconciled row"
    );
    assert_eq!(
        rows[0].qualified_name.as_deref(),
        Some("@browserbasehq/mcp")
    );
    assert_eq!(rows[0].transport.as_deref(), Some("http_remote"));
}

/// The match is on the *normalised* endpoint, so the credential a List A server
/// carries in its query string cannot hide the duplicate.
#[test]
fn reconciliation_ignores_query_strings_ports_and_trailing_slashes() {
    let mut rows = vec![declared(
        "parallel",
        "HTTPS://MCP.Parallel.AI:443/search/?token=sekrit",
        McpSource::Runtime,
    )];
    merge_installs(
        &mut rows,
        vec![install(
            "id-1",
            "@parallel/search",
            Some("https://mcp.parallel.ai/search"),
        )],
    );
    assert_eq!(
        rows.len(),
        1,
        "same server despite case, port, slash, query"
    );
}

/// A manifest row must not become deletable by being installed over.
#[test]
fn a_manifest_row_keeps_its_badge_when_an_install_reconciles_onto_it() {
    let mut rows = vec![declared(
        "deepwiki",
        "https://mcp.deepwiki.com/mcp",
        McpSource::Manifest,
    )];
    merge_installs(
        &mut rows,
        vec![install(
            "id-1",
            "@deepwiki/mcp",
            Some("https://mcp.deepwiki.com/mcp"),
        )],
    );
    assert_eq!(rows[0].source, McpSource::Manifest);
}

/// Two servers that share nothing must stay two rows — the matching rule has to
/// be able to say "no".
#[test]
fn distinct_endpoints_stay_distinct_rows() {
    let mut rows = vec![declared(
        "deepwiki",
        "https://mcp.deepwiki.com/mcp",
        McpSource::Manifest,
    )];
    merge_installs(
        &mut rows,
        vec![install("id-1", "@exa/exa", Some("https://mcp.exa.ai/mcp"))],
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].source, McpSource::Registry);
}

/// A stdio install has no endpoint. "No address" must mean "matches nothing",
/// not "matches the first row" — a blank key colliding would silently graft an
/// install onto an unrelated server.
#[test]
fn an_install_without_an_endpoint_reconciles_with_nothing() {
    let mut rows = vec![declared(
        "exa",
        "https://mcp.exa.ai/mcp",
        McpSource::Runtime,
    )];
    merge_installs(&mut rows, vec![install("id-1", "@old/stdio-server", None)]);
    assert_eq!(rows.len(), 2);
    assert!(rows[0].server_id.is_none(), "the http row is untouched");
    assert_eq!(rows[1].transport.as_deref(), Some("stdio"));
    assert_eq!(rows[1].endpoint, "", "nothing to dial");
}

/// A credential on either side means one is stored. Reporting only List A's slot
/// would print "no credential" over a server that authenticates fine.
#[test]
fn auth_configured_is_the_union_on_a_reconciled_row() {
    let mut rows = vec![declared(
        "exa",
        "https://mcp.exa.ai/mcp",
        McpSource::Runtime,
    )];
    let mut with_env = install("id-1", "@exa/exa", Some("https://mcp.exa.ai/mcp"));
    with_env.auth_configured = true;
    merge_installs(&mut rows, vec![with_env]);
    assert!(rows[0].auth_configured);
}

/// List A's probe wins: it dials the endpoint the way the agents' bridge tools
/// do, credential included, so it is the more truthful badge when both exist.
#[test]
fn list_a_health_wins_and_registry_health_fills_a_gap() {
    let probed = McpHealth {
        status: McpStatus::Ok,
        message: "probed".to_string(),
        tool_count: 7,
        checked_at_millis: 1,
        auth_hint: None,
    };
    let mut rows = vec![
        declared("exa", "https://mcp.exa.ai/mcp", McpSource::Runtime),
        declared("linear", "https://mcp.linear.app/mcp", McpSource::Runtime),
    ];
    rows[0].health = Some(probed.clone());

    let from_registry = health_from_status("error", 0, None, 99);
    let mut a = install("id-1", "@exa/exa", Some("https://mcp.exa.ai/mcp"));
    a.health = from_registry.clone();
    let mut b = install("id-2", "@linear/linear", Some("https://mcp.linear.app/mcp"));
    b.health = from_registry.clone();

    merge_installs(&mut rows, vec![a, b]);
    assert_eq!(
        rows[0].health,
        Some(probed),
        "List A's probe is not replaced"
    );
    assert_eq!(
        rows[1].health, from_registry,
        "and fills a row that has none"
    );
}

// ---------------------------------------------------------------------------
// Row shape
// ---------------------------------------------------------------------------

/// The two halves of the DTO contract: a registry row carries the id its routes
/// key on, and a List A row's JSON is byte-identical to what it was before the
/// registry fields existed.
#[test]
fn a_registry_row_carries_its_server_id_and_a_declared_row_is_unchanged() {
    let mut rows = vec![declared(
        "exa",
        "https://mcp.exa.ai/mcp",
        McpSource::Runtime,
    )];
    let before = serde_json::to_value(&rows[0]).expect("serializes");

    merge_installs(
        &mut rows,
        vec![install(
            "id-1",
            "@modelcontextprotocol/server-git",
            Some("https://git.example.test/mcp"),
        )],
    );

    let declared_json = serde_json::to_value(&rows[0]).expect("serializes");
    assert_eq!(
        declared_json, before,
        "a non-registry row gains no key — every registry field is \
         skip_serializing_if = Option::is_none"
    );
    for key in ["serverId", "qualifiedName", "iconUrl", "transport"] {
        assert!(
            declared_json.get(key).is_none(),
            "`{key}` must not appear on a declared row"
        );
    }

    let registry = &rows[1];
    assert_eq!(registry.server_id.as_deref(), Some("id-1"));
    assert_eq!(
        registry.name, "modelcontextprotocol-server-git",
        "the row's name is a display slug; `serverId` is the key"
    );
    assert_eq!(registry.source, McpSource::Registry);
    assert!(
        registry.reachable_by.is_empty(),
        "reach is attached from grants afterwards, never assumed for the roster"
    );
}

fn holder(id: &str, grants: &[&str]) -> crate::server::ops::mcp::access::RosterAccess {
    let grants: Vec<String> = grants.iter().map(|g| g.to_string()).collect();
    crate::server::ops::mcp::access::RosterAccess {
        agent: agent(id),
        requested: None,
        ceiling: grants.clone(),
        effective: grants,
    }
}

#[test]
fn an_install_reaches_only_the_agents_whose_grants_name_it() {
    let mut rows = Vec::new();
    merge_installs(
        &mut rows,
        vec![install("id-1", "@exa/exa", Some("https://mcp.exa.ai/mcp"))],
    );
    let roster = [
        holder("scoped", &["mcp_registry.id-1"]),
        holder("all", &["mcp_registry"]),
        holder("declared-only", &["mcp:*"]),
        holder("wildcard", &["*"]),
        holder("other", &["mcp_registry.id-2"]),
    ];
    crate::server::ops::mcp::attach_access(&mut rows, &roster);

    let reached: Vec<&str> = rows[0].reachable_by.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(reached, ["scoped", "all"]);
    assert_eq!(rows[0].access_grant, "mcp_registry.id-1");
}

/// A disabled install hands out no tools, so it reaches nobody — the same rule
/// `reachers_of` applies to a disabled List A server.
#[test]
fn a_disabled_install_reaches_nobody() {
    let mut rows = Vec::new();
    let mut off = install("id-1", "@exa/exa", Some("https://mcp.exa.ai/mcp"));
    off.enabled = false;
    merge_installs(&mut rows, vec![off]);
    crate::server::ops::mcp::attach_access(&mut rows, &[holder("ceo", &["mcp_registry"])]);
    assert!(rows[0].reachable_by.is_empty());
    assert!(!rows[0].enabled);
}

/// The console keys rows by `name`. Two rows sharing one would render as a
/// single flickering row and delete the wrong server, so a collision has to
/// resolve — deterministically, not positionally.
#[test]
fn a_colliding_row_name_falls_back_to_the_server_id() {
    let mut rows = vec![declared(
        "modelcontextprotocol-server-git",
        "https://elsewhere.example.test/mcp",
        McpSource::Runtime,
    )];
    merge_installs(
        &mut rows,
        vec![
            install(
                "aaaaaaaa-1111",
                "@modelcontextprotocol/server-git",
                Some("https://one.example.test/mcp"),
            ),
            install(
                "bbbbbbbb-2222",
                "@modelcontextprotocol/server-git",
                Some("https://two.example.test/mcp"),
            ),
        ],
    );
    let names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "modelcontextprotocol-server-git",
            "modelcontextprotocol-server-git-aaaaaaaa",
            "modelcontextprotocol-server-git-bbbbbbbb",
        ]
    );
}

// ---------------------------------------------------------------------------
// Endpoint normalisation
// ---------------------------------------------------------------------------

#[test]
fn endpoint_normalisation_rules() {
    let same = |a: &str, b: &str| {
        assert_eq!(
            normalize_endpoint(a),
            normalize_endpoint(b),
            "`{a}` and `{b}` are one endpoint"
        );
    };
    same("https://Host.Example/mcp", "https://host.example/mcp/");
    same("https://host.example:443/mcp", "https://host.example/mcp");
    same("http://host.example:80/mcp", "http://host.example/mcp");
    same(
        "https://host.example/mcp?k=v#frag",
        "https://host.example/mcp",
    );
    same("https://host.example", "https://host.example/");

    assert_ne!(
        normalize_endpoint("https://host.example/mcp"),
        normalize_endpoint("http://host.example/mcp"),
        "scheme is part of the identity"
    );
    assert_ne!(
        normalize_endpoint("https://host.example/a"),
        normalize_endpoint("https://host.example/b"),
        "path is part of the identity"
    );
    assert_eq!(normalize_endpoint("   "), None);
    assert_eq!(normalize_endpoint(""), None);
}

// ---------------------------------------------------------------------------
// Status → health
// ---------------------------------------------------------------------------

#[test]
fn connection_status_maps_onto_the_console_badge() {
    let health = |status: &str, hint: Option<&str>| health_from_status(status, 3, hint, 42);

    let ok = health("connected", None).expect("mapped");
    assert_eq!(ok.status, McpStatus::Ok);
    assert_eq!(ok.tool_count, 3);
    assert_eq!(ok.checked_at_millis, 42);

    assert_eq!(
        health("unauthorized", Some("oauth_required"))
            .expect("mapped")
            .status,
        McpStatus::NeedsConfig,
        "a 401 is a resting state to act on, not a failure"
    );
    assert_eq!(
        health("error", None).expect("mapped").status,
        McpStatus::Error
    );
    assert_eq!(
        health("disconnected", None).expect("mapped").status,
        McpStatus::Unknown
    );
    assert_eq!(
        health("something-upstream-added", None),
        None,
        "a status this build cannot read is no badge, not a wrong one"
    );
}

/// The reason a raw `last_error` is not in [`RegistryInstall`] at all: there is
/// no known-secret set to scrub an install's error against, because its env
/// values are deliberately never loaded. Pinned so a later "let's surface the
/// error" change has to confront it.
#[test]
fn no_upstream_error_text_reaches_the_badge() {
    let hint = "token_rejected";
    let health = health_from_status("unauthorized", 0, Some(hint), 1).expect("mapped");
    assert!(
        !health.message.contains("http"),
        "no URL can ride out in the message: {}",
        health.message
    );
    assert_eq!(
        health.auth_hint.as_deref(),
        Some(hint),
        "only the stable reason code crosses the wire"
    );
}

// ---------------------------------------------------------------------------
// Catalogue projection
// ---------------------------------------------------------------------------

#[test]
fn search_results_forward_only_named_fields() {
    let raw = serde_json::json!({
        "servers": [{
            "qualified_name": "@exa/exa",
            "display_name": "Exa",
            "description": "Search",
            "icon_url": "https://icons.example/exa.png",
            "source": "smithery",
            "official": true,
            "use_count": 12,
            "internal_admin_token": "sekrit",
        }, {
            "display_name": "No identity"
        }],
        "page": 2,
        "total_pages": 5,
    });
    let projected = catalogue_search(&raw);
    assert_eq!(projected.page, 2);
    assert_eq!(projected.total_pages, 5);
    assert_eq!(
        projected.servers.len(),
        1,
        "a row with no qualified name cannot be installed and is dropped"
    );
    let json = serde_json::to_value(&projected.servers[0]).expect("serializes");
    assert!(
        json.get("internal_admin_token").is_none(),
        "upstream's flattened extras do not ride through"
    );
    assert_eq!(json["qualifiedName"], "@exa/exa");
    assert!(projected.servers[0].official);
}

/// The entry projection makes the install decision so the console never offers
/// a button that would be refused — and names the reason when it must refuse.
#[test]
fn a_stdio_only_entry_is_refused_with_a_reason_that_names_why() {
    let raw = serde_json::json!({
        "server": {
            "qualified_name": "@modelcontextprotocol/server-filesystem",
            "display_name": "Filesystem",
            "source": "mcp_official",
            "required_env_keys": ["ROOT"],
            "connections": [{
                "type": "stdio",
                "published": true,
                "example_config": { "command": "npx" }
            }]
        }
    });
    let detail = catalogue_detail(&raw).expect("projected");
    assert!(!detail.installable);
    assert_eq!(detail.endpoint, None);
    let refusal = detail.refusal.expect("a refusal says why");
    assert!(
        refusal.contains("@modelcontextprotocol/server-filesystem"),
        "names the entry: {refusal}"
    );
    for reason in ["Node", "Python", "hosted HTTP endpoint"] {
        assert!(
            refusal.contains(reason),
            "names why ({reason} missing): {refusal}"
        );
    }
}

/// The same refusal is what the install route raises, so the two surfaces cannot
/// disagree about the reason.
#[test]
fn the_entry_refusal_is_the_install_refusal() {
    let raw = serde_json::json!({
        "server": { "qualified_name": "@a/b", "connections": [{ "type": "stdio" }] }
    });
    let detail = catalogue_detail(&raw).expect("projected");
    assert_eq!(detail.refusal, Some(stdio_install_refusal("@a/b")));
}

#[test]
fn a_hosted_entry_is_installable_and_names_the_url_the_install_will_dial() {
    let raw = serde_json::json!({
        "server": {
            "qualified_name": "@exa/exa",
            "display_name": "Exa",
            "source": "smithery",
            "required_env_keys": ["EXA_API_KEY"],
            "connections": [
                { "type": "stdio", "published": true },
                { "type": "http", "published": false, "deployment_url": "https://draft.exa.ai/mcp" },
                { "type": "http", "published": true, "deployment_url": "https://mcp.exa.ai/mcp" }
            ]
        }
    });
    let detail = catalogue_detail(&raw).expect("projected");
    assert!(detail.installable);
    assert_eq!(
        detail.endpoint.as_deref(),
        Some("https://mcp.exa.ai/mcp"),
        "the published hosted connection wins, matching upstream's picker"
    );
    assert_eq!(detail.required_env_keys, vec!["EXA_API_KEY".to_string()]);
    assert_eq!(detail.refusal, None);
    let json = serde_json::to_value(&detail).expect("serializes");
    assert!(
        json.get("connections").is_none() && json.get("exampleConfig").is_none(),
        "raw connection metadata is not forwarded"
    );
}

/// `sse` is one of the wire names upstream routes into the HTTP install path.
#[test]
fn an_sse_connection_counts_as_hosted() {
    let raw = serde_json::json!({
        "connections": [{ "type": "sse", "deployment_url": "https://host.example/sse" }]
    });
    assert_eq!(
        http_deployment_url(&raw).as_deref(),
        Some("https://host.example/sse")
    );
}

/// An http connection that declares no URL is not dialable — treating it as one
/// would install a server with an empty endpoint.
#[test]
fn an_http_connection_without_a_url_is_not_dialable() {
    let raw = serde_json::json!({
        "connections": [{ "type": "http", "published": true, "deployment_url": "  " }]
    });
    assert_eq!(http_deployment_url(&raw), None);
}

// ---------------------------------------------------------------------------
// Delete dispatch
// ---------------------------------------------------------------------------

/// `DELETE …/mcp/servers/{name}` dispatches on where the row actually lives.
///
/// The case this pins is `Both`. A reconciled row that only drops its
/// runtime-index entry leaves the directory install connected, with its tools
/// still on every agent's belt — a delete the operator watches fail. Manifest
/// and default rows never reach this decision; they are refused before it.
#[test]
fn delete_dispatches_on_where_the_row_lives() {
    assert_eq!(removal_for(true, false), Removal::IndexRow, "typed in only");
    assert_eq!(removal_for(false, true), Removal::Install, "installed only");
    assert_eq!(
        removal_for(true, true),
        Removal::Both,
        "typed in AND installed — removing one half leaves the server callable"
    );
    assert_eq!(removal_for(false, false), Removal::NotFound, "no such row");
}

/// The degraded contract's other half: when the registry yields nothing — an
/// unreadable store, a directory that will not answer, a build with no `mcp` —
/// the merged read is List A, byte for byte.
#[test]
fn no_installs_leaves_the_declared_list_untouched() {
    let mut rows = vec![
        declared(
            "deepwiki",
            "https://mcp.deepwiki.com/mcp",
            McpSource::Manifest,
        ),
        declared("exa", "https://mcp.exa.ai/mcp", McpSource::Runtime),
    ];
    let before = serde_json::to_value(&rows).expect("serializes");
    merge_installs(&mut rows, Vec::new());
    assert_eq!(serde_json::to_value(&rows).expect("serializes"), before);
}

// ---------------------------------------------------------------------------
// A directory entry becomes one of this company's own servers
// ---------------------------------------------------------------------------

/// The declaration carries what the catalogue entry already told us, and
/// nothing invented.
#[test]
fn a_directory_entry_declares_the_endpoint_the_catalogue_named() {
    let server = super::declaration_from_directory(
        "exa/exa-mcp",
        "https://server.example.com/mcp",
        Some("Web search".to_string()),
    );

    assert_eq!(server.name, "exa/exa-mcp");
    assert_eq!(server.endpoint, "https://server.example.com/mcp");
    assert_eq!(server.description.as_deref(), Some("Web search"));
    assert!(server.enabled);
    assert!(
        server.command.is_none(),
        "only a hosted endpoint is ever declared from the directory"
    );
}

/// An install has always meant every tool the server offers, and an empty
/// pair is how this crate spells that. Declaring a narrowed list here would
/// silently withhold tools the operator never restricted.
#[test]
fn a_directory_entry_declares_no_tool_restrictions() {
    let server = super::declaration_from_directory("exa/exa-mcp", "https://e.example/mcp", None);

    assert!(server.allowed_tools.is_empty());
    assert!(server.disallowed_tools.is_empty());
    assert!(server.read_only_tools.is_empty());
}

/// The same default `POST …/mcp/servers` applies when the body omits one, so
/// a server added from either screen behaves the same way under load.
#[test]
fn a_directory_entry_takes_the_same_timeout_as_a_hand_added_server() {
    let server = super::declaration_from_directory("exa/exa-mcp", "https://e.example/mcp", None);

    assert_eq!(server.timeout_secs, 30);
}

/// The credential is never carried on the declaration: it is stored
/// write-only beside it, which is what keeps a read from echoing one.
#[test]
fn a_directory_entry_carries_no_credential_on_the_row() {
    let server = super::declaration_from_directory("exa/exa-mcp", "https://e.example/mcp", None);

    assert!(server.auth_secret.is_none());
}
