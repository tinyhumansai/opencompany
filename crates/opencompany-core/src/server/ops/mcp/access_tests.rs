use super::*;

fn grants(list: &[&str]) -> Vec<String> {
    list.iter().map(|g| g.to_string()).collect()
}

fn agent(id: &str, ceiling: &[&str], requested: Option<&[&str]>) -> RosterAccess {
    let ceiling = grants(ceiling);
    let requested = requested.map(grants);
    let effective = agent_scoped_grants(&ceiling, &[], requested.as_deref());
    RosterAccess {
        agent: RosterAgentDto {
            id: id.to_string(),
            name: id.to_string(),
        },
        requested,
        ceiling,
        effective,
    }
}

const NOTION: ServerKey<'static> = ServerKey::Declared("notion");
const LINEAR: ServerKey<'static> = ServerKey::Declared("linear");
const INSTALL: ServerKey<'static> = ServerKey::Registry("srv-1");
const ALL: [ServerKey<'static>; 3] = [NOTION, LINEAR, INSTALL];

fn one(entry: RosterAccess, key: ServerKey<'_>) -> AgentAccessDto {
    access_for(&[entry], key, true, &ALL).remove(0)
}

#[test]
fn an_inheriting_agent_under_an_mcp_wildcard_reaches_and_can_be_removed() {
    let access = one(agent("ceo", &["search", "mcp:*"], None), NOTION);
    assert_eq!(access.state, AccessState::Inherited);
    assert!(access.reaches);
    assert_eq!(access.grant_tools, None);
    assert_eq!(
        access.revoke_tools,
        Some(grants(&["search", "mcp:linear"])),
        "the wildcard is replaced by the other servers it reached, and nothing outside MCP is lost"
    );
}

#[test]
fn an_agent_listing_the_server_loses_only_that_grant() {
    let access = one(
        agent(
            "ceo",
            &["search", "mcp:*"],
            Some(&["search", "mcp:notion", "mcp:linear"]),
        ),
        NOTION,
    );
    assert_eq!(access.state, AccessState::Included);
    assert_eq!(access.revoke_tools, Some(grants(&["search", "mcp:linear"])));
}

#[test]
fn an_agent_leaving_the_server_out_is_offered_its_list_plus_the_grant() {
    let access = one(
        agent("ceo", &["search", "mcp:*"], Some(&["search"])),
        NOTION,
    );
    assert_eq!(access.state, AccessState::Excluded);
    assert!(!access.reaches);
    assert_eq!(access.grant_tools, Some(grants(&["search", "mcp:notion"])));
    assert_eq!(access.revoke_tools, None);
}

#[test]
fn a_ceiling_without_the_server_blocks_every_per_agent_edit() {
    let access = one(agent("ceo", &["search", "mcp:linear"], None), NOTION);
    assert_eq!(access.state, AccessState::Blocked);
    assert!(!access.reaches);
    assert_eq!(access.grant_tools, None);
    assert_eq!(access.revoke_tools, None);
}

#[test]
fn a_disabled_server_reaches_nobody_but_keeps_its_standing() {
    let access = access_for(&[agent("ceo", &["mcp:*"], None)], NOTION, false, &ALL).remove(0);
    assert_eq!(access.state, AccessState::Inherited);
    assert!(!access.reaches);
}

#[test]
fn a_grant_outside_mcp_that_reaches_the_server_cannot_be_narrowed_here() {
    let access = one(agent("ceo", &["mcp*"], None), NOTION);
    assert_eq!(access.state, AccessState::Inherited);
    assert!(access.reaches);
    assert_eq!(access.revoke_tools, None);
}

#[test]
fn a_registry_install_is_reached_through_its_own_namespace() {
    let wildcard = one(agent("ceo", &["mcp:*"], None), INSTALL);
    assert_eq!(wildcard.state, AccessState::Blocked);

    let all = one(agent("ceo", &["mcp_registry"], None), INSTALL);
    assert_eq!(all.state, AccessState::Inherited);
    assert!(all.reaches);
    assert_eq!(all.revoke_tools, Some(Vec::new()));

    let excluded = one(agent("ceo", &["mcp_registry"], Some(&[])), INSTALL);
    assert_eq!(excluded.state, AccessState::Excluded);
    assert_eq!(excluded.grant_tools, Some(grants(&["mcp_registry.srv-1"])));
}

#[test]
fn the_exact_grant_names_one_server() {
    assert_eq!(NOTION.grant(), "mcp:notion");
    assert_eq!(INSTALL.grant(), "mcp_registry.srv-1");
}
