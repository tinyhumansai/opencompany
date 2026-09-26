//! Which of the two MCP dispatch tools reaches which connected server.
//!
//! An agent can hold two tools that call MCP servers, addressing two disjoint
//! catalogues: `mcp_call_tool` names a server the company declared, by name,
//! while `mcp_registry_tool_call` names a directory install, by `server_id`.
//! Nothing in the belt says which server belongs to which catalogue, so the
//! choice is a guess and a wrong guess fails for a reason the model cannot see.
//!
//! This renders one line per reachable server naming the tool and the key that
//! addresses it. It is deliberately a **positive mapping, not a partition**:
//! `mcp_call_tool` also reaches the internal `opencompany` server, which is
//! attached to the spec elsewhere and described by its own brief, so a claim
//! here that these are all of an agent's MCP servers would be false.
//!
//! Lives here rather than beside the harness that calls it because the whole
//! `harness` tree is behind `feature = "openhuman"`. The call site is gated
//! twice over — no dispatch tool is wired without `feature = "mcp"` either —
//! but the rule itself is pure company data, so keeping it here is what lets
//! its tests run in the default lane instead of only in a filtered one.

use std::collections::BTreeMap;

use super::mcp::McpServerDecl;
use super::mcp_endpoint::normalize_endpoint;
use crate::runtime::tools::{grants_cover_registry_server, grants_cover_server};

/// How many servers a brief names before it stops naming them.
///
/// A bulk directory install is the only way this list grows without bound, and
/// an agent prompt is not the place to pay for it. The overflow is a count plus
/// a pointer at live enumeration, never a truncated name: a half-written name is
/// one a model may pass verbatim and get refused for.
const CAP: usize = 25;

const HEADING: &str = "\n\n## MCP servers connected to this company";

/// One directory install, in the shape this renderer needs.
///
/// An allowlist rather than the vendored install record, for the reason
/// `registry_list` gives for its own: a field added upstream should have to be
/// opted in here, not arrive in an agent's prompt because nobody noticed it.
pub(crate) struct RegistryServerRow {
    /// What `mcp_registry_tool_call` addresses this install by.
    pub(crate) server_id: String,
    /// The label a human recognises the install by.
    pub(crate) display_name: String,
    /// The install's remote address, absent for a stdio install.
    pub(crate) endpoint: Option<String>,
    /// Whether this install is exposed to agents at all.
    pub(crate) enabled: bool,
}

/// Whether `value` can appear in the brief exactly as an agent must pass it back.
///
/// Each value is rendered inside a code span *and* quoted as the key the model
/// sends, so it has to be both safe to render and exact. A control character
/// breaks the line out of its list item and a backtick closes the span early;
/// either way the appended text stops reading as a list of servers. A declared
/// name is only checked for emptiness, and an install's `server_id` and
/// `display_name` arrive from the directory unchanged, so none can be assumed
/// clean.
///
/// A value that fails this cannot be named faithfully — mangling it would print a
/// key that gets refused — so its server joins the overflow count and is left to
/// live enumeration.
fn renderable(value: &str) -> bool {
    !value.is_empty() && !value.contains('`') && !value.chars().any(char::is_control)
}

/// The brief naming each server this agent can reach and the tool that reaches
/// it, or an empty string when it can reach none.
///
/// `decls` and `installs` are the same inputs the toolbelt was wired from, so
/// the brief is exactly as accurate as the belt it describes. Both are scoped by
/// the same grant predicates the call paths enforce, so enumeration here cannot
/// name a server a call would refuse.
pub(crate) fn server_family_brief(
    decls: &[McpServerDecl],
    installs: &[RegistryServerRow],
    grants: &[String],
) -> String {
    let reachable_decls: Vec<&McpServerDecl> = decls
        .iter()
        .filter(|decl| decl.enabled && grants_cover_server(grants, &decl.name))
        .collect();
    let reachable_installs: Vec<&RegistryServerRow> = installs
        .iter()
        .filter(|row| row.enabled && grants_cover_registry_server(grants, &row.server_id))
        .collect();

    let declared: Vec<&McpServerDecl> = reachable_decls
        .iter()
        .copied()
        .filter(|decl| renderable(&decl.name))
        .collect();
    let installed: Vec<&RegistryServerRow> = reachable_installs
        .iter()
        .copied()
        .filter(|row| renderable(&row.server_id) && renderable(&row.display_name))
        .collect();
    let unnameable =
        (reachable_decls.len() - declared.len()) + (reachable_installs.len() - installed.len());

    if declared.is_empty() && installed.is_empty() {
        return String::new();
    }

    let mut by_endpoint: BTreeMap<String, usize> = BTreeMap::new();
    for (index, decl) in declared.iter().enumerate() {
        if let Some(key) = normalize_endpoint(&decl.endpoint) {
            by_endpoint.entry(key).or_insert(index);
        }
    }

    let mut also_installed_as: BTreeMap<usize, &str> = BTreeMap::new();
    let mut registry_only: Vec<&RegistryServerRow> = Vec::new();
    for row in &installed {
        let paired = row
            .endpoint
            .as_deref()
            .and_then(normalize_endpoint)
            .and_then(|key| by_endpoint.get(&key).copied())
            .filter(|index| !also_installed_as.contains_key(index));
        match paired {
            Some(index) => {
                also_installed_as.insert(index, &row.server_id);
            }
            None => registry_only.push(row),
        }
    }

    let mut lines: Vec<String> = Vec::new();
    for (index, decl) in declared.iter().enumerate() {
        let mut line = format!(
            "- `{}` — `mcp_call_tool` with `\"server\": \"{}\"`",
            decl.name, decl.name
        );
        if let Some(server_id) = also_installed_as.get(&index) {
            line.push_str(&format!(
                ", or `mcp_registry_tool_call` with `\"server_id\": \"{server_id}\"` (the same \
                 server)"
            ));
        }
        lines.push(line);
    }
    for row in registry_only {
        lines.push(format!(
            "- `{}` — `mcp_registry_tool_call` with `\"server_id\": \"{}\"`",
            row.display_name, row.server_id
        ));
    }

    let hidden = lines.len().saturating_sub(CAP) + unnameable;
    lines.truncate(CAP);

    let mut brief = String::from(HEADING);
    brief.push_str(
        "\n\nReach each server below with the tool named on its line, using the key it shows. \
         Other MCP servers may be attached to you as well; these are the ones connected to this \
         company.\n\n",
    );
    brief.push_str(&lines.join("\n"));
    if hidden > 0 {
        let enumerate = match (!reachable_decls.is_empty(), !reachable_installs.is_empty()) {
            (true, true) => "`mcp_list_servers` and `mcp_registry_installed_list`",
            (false, true) => "`mcp_registry_installed_list`",
            _ => "`mcp_list_servers`",
        };
        brief.push_str(&format!(
            "\n- …and {hidden} more. Call {enumerate} for the rest."
        ));
    }
    brief.push('\n');
    brief
}

#[cfg(test)]
#[path = "mcp_families_tests.rs"]
mod tests;
