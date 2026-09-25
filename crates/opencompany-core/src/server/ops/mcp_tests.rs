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
        overlay_desk_hive: Vec::new(),
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
        provider: None,
        id: id.to_string(),
        name: id.to_string(),
        role: "Growth".to_string(),
        description: None,
        tools: (!tools.is_empty()).then_some(tools),
        skills: None,
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
        hive: Default::default(),
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
