//! Which teammates can reach one MCP server, and the tool list that would add
//! or remove one of them.
//!
//! The grant matching stays here, beside the harness's own matchers
//! ([`grants_cover_server`], [`grants_cover_registry_server`]), so the console
//! never re-implements glob rules to decide what a `PATCH …/team/{id}` should
//! send.

use serde::Serialize;

use crate::metering::roster_display_names;
use crate::ports::types::CompanyRecord;
use crate::runtime::builder::agent_scoped_grants;
use crate::runtime::tools::{grants_cover_registry_server, grants_cover_server};

use super::RosterAgentDto;

/// One roster agent's stored tool request and what it resolves to.
#[derive(Debug, Clone)]
pub(in crate::server::ops) struct RosterAccess {
    pub(in crate::server::ops) agent: RosterAgentDto,
    /// The agent's own `tools` line; `None` inherits the ceiling.
    pub(in crate::server::ops) requested: Option<Vec<String>>,
    /// The company grant narrowed by the agent's desks.
    pub(in crate::server::ops) ceiling: Vec<String>,
    /// What the agent actually holds.
    pub(in crate::server::ops) effective: Vec<String>,
}

/// Every roster agent, walked exactly as the harness builds the roster:
/// effective manifest agents, then overlay teammates whose id no manifest agent
/// claims.
pub(in crate::server::ops) fn roster_access(record: &CompanyRecord) -> Vec<RosterAccess> {
    let allow = record.effective_tool_allow();
    let allow = &allow;
    let effective_agents = record.effective_agents();
    let names = roster_display_names(&effective_agents, &record.overlay_agents);
    let entry = |id: &str, tools: Option<&[String]>| {
        let desk_tools = record.agent_desk_tools(id);
        let desk_refs: Vec<&[String]> = desk_tools.iter().map(Vec::as_slice).collect();
        RosterAccess {
            agent: RosterAgentDto {
                id: id.to_string(),
                name: names.get(id).cloned().unwrap_or_else(|| id.to_string()),
            },
            requested: tools.map(<[String]>::to_vec),
            ceiling: agent_scoped_grants(allow, &desk_refs, None),
            effective: agent_scoped_grants(allow, &desk_refs, tools),
        }
    };
    let mut roster: Vec<RosterAccess> = effective_agents
        .iter()
        .map(|agent| entry(&agent.id, agent.tools.as_deref()))
        .collect();
    let manifest_ids: std::collections::HashSet<&str> = record
        .manifest
        .agents
        .iter()
        .map(|agent| agent.id.as_str())
        .collect();
    for overlay in &record.overlay_agents {
        if !manifest_ids.contains(overlay.id.as_str()) {
            roster.push(entry(&overlay.id, overlay.tools.as_deref()));
        }
    }
    roster
}

/// How a grant names one server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::server::ops) enum ServerKey<'a> {
    /// A declared server, granted as `mcp:<name>`.
    Declared(&'a str),
    /// A directory install, granted as `mcp_registry.<server_id>`.
    Registry(&'a str),
}

impl ServerKey<'_> {
    /// The exact grant that reaches this server and nothing else.
    pub(in crate::server::ops) fn grant(self) -> String {
        match self {
            Self::Declared(name) => format!("mcp:{name}"),
            Self::Registry(id) => format!("mcp_registry.{id}"),
        }
    }

    /// Whether `grants` reach this server, by the harness's own matcher.
    pub(in crate::server::ops) fn covered_by(self, grants: &[String]) -> bool {
        match self {
            Self::Declared(name) => grants_cover_server(grants, name),
            Self::Registry(id) => grants_cover_registry_server(grants, id),
        }
    }

    /// Whether `grant` is written in this server's own namespace, so that
    /// replacing it with exact grants changes nothing outside MCP.
    fn owns(self, grant: &str) -> bool {
        match self {
            Self::Declared(_) => grant.starts_with("mcp:"),
            Self::Registry(_) => grant == "mcp_registry" || grant.starts_with("mcp_registry."),
        }
    }
}

/// Where one agent stands with one server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::server::ops) enum AccessState {
    /// Inherits the ceiling, which covers the server.
    Inherited,
    /// Its own list covers the server.
    Included,
    /// Its own list leaves the server out.
    Excluded,
    /// The company or desk ceiling leaves the server out, so no per-agent edit
    /// can grant it.
    Blocked,
}

/// One agent's access to one server, with the whole `tools` list that would
/// flip it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::server::ops) struct AgentAccessDto {
    pub(in crate::server::ops) id: String,
    pub(in crate::server::ops) name: String,
    pub(in crate::server::ops) state: AccessState,
    /// Whether the agent can call the server now (also `false` while disabled).
    pub(in crate::server::ops) reaches: bool,
    /// The `tools` list to send to grant access; absent when no edit is needed
    /// or none can grant it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::server::ops) grant_tools: Option<Vec<String>>,
    /// The `tools` list to send to withdraw access; absent when the agent has
    /// none, or when a grant outside MCP's namespace confers it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::server::ops) revoke_tools: Option<Vec<String>>,
}

/// Every roster agent's access to `key`. `others` are the company's other
/// servers, used to keep their access when a wildcard has to be expanded.
pub(in crate::server::ops) fn access_for(
    roster: &[RosterAccess],
    key: ServerKey<'_>,
    enabled: bool,
    others: &[ServerKey<'_>],
) -> Vec<AgentAccessDto> {
    roster
        .iter()
        .map(|entry| {
            let state = if !key.covered_by(&entry.ceiling) {
                AccessState::Blocked
            } else if entry.requested.is_none() {
                AccessState::Inherited
            } else if key.covered_by(&entry.effective) {
                AccessState::Included
            } else {
                AccessState::Excluded
            };
            let grant_tools = (state == AccessState::Excluded).then(|| {
                let mut next = entry.requested.clone().unwrap_or_default();
                push_unique(&mut next, key.grant());
                next
            });
            let revoke_tools = match state {
                AccessState::Inherited => without(&entry.ceiling, key, others),
                AccessState::Included => {
                    without(entry.requested.as_deref().unwrap_or_default(), key, others)
                }
                AccessState::Excluded | AccessState::Blocked => None,
            };
            AgentAccessDto {
                id: entry.agent.id.clone(),
                name: entry.agent.name.clone(),
                state,
                reaches: enabled && key.covered_by(&entry.effective),
                grant_tools,
                revoke_tools,
            }
        })
        .collect()
}

/// `base` with every grant that reaches `key` taken out, and each MCP wildcard
/// among them replaced by the exact grants of the other servers it reached.
/// `None` when a grant outside MCP's namespace reaches `key`.
fn without(base: &[String], key: ServerKey<'_>, others: &[ServerKey<'_>]) -> Option<Vec<String>> {
    let mut next = Vec::with_capacity(base.len());
    for grant in base {
        let single = std::slice::from_ref(grant);
        if !key.covered_by(single) {
            push_unique(&mut next, grant.clone());
            continue;
        }
        if !key.owns(grant) {
            return None;
        }
        for other in others {
            if *other != key && other.covered_by(single) {
                push_unique(&mut next, other.grant());
            }
        }
    }
    Some(next)
}

fn push_unique(list: &mut Vec<String>, grant: String) {
    if !list.contains(&grant) {
        list.push(grant);
    }
}

#[cfg(test)]
#[path = "access_tests.rs"]
mod tests;
