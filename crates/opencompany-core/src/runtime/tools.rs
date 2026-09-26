//! The Phase-1 stub [`ToolProvider`].
//!
//! No real tools are wired yet (OpenHuman JSON-RPC lands later), but the
//! grant-check invariant from `ports.md` is enforced now: `invoke` MUST reject
//! any call outside the manifest grant *before* any side effect. The catalog is
//! empty; an ungranted call returns [`OpenCompanyError::ToolNotGranted`].

use async_trait::async_trait;

use crate::Result;
use crate::error::OpenCompanyError;
use crate::ports::tools::ToolProvider;
use crate::ports::types::{CompanyId, ToolCall, ToolResult, ToolSpec};

/// A stub tool provider that advertises no tools and enforces grants.
///
/// Grants are the manifest's company-wide `[tools].allow` globs. A tool is
/// granted when its name matches a glob exactly, via a trailing `*` prefix that
/// ends on a **namespace boundary** (`email.*`, `file*`), or the catch-all `*`.
#[derive(Clone, Debug, Default)]
pub struct StubToolProvider {
    grants: Vec<String>,
}

impl StubToolProvider {
    /// Builds a provider from the manifest's company-wide tool grants.
    pub fn new(grants: Vec<String>) -> Self {
        Self { grants }
    }

    fn is_granted(&self, tool: &str) -> bool {
        self.grants.iter().any(|grant| grant_matches(grant, tool))
    }
}

/// The characters that end a namespace segment in a tool name: `.` for the
/// dotted manifest namespaces (`email.send`), `_` for the snake_case tool names
/// the harness registers (`file_read`), and `:` for the MCP server namespace
/// (`mcp:notion`).
///
/// A trailing-`*` grant may only extend up to one of these — that is the whole
/// difference between `file*` granting `file_read` and `file*` granting
/// `filesystem_wipe`.
const TOOL_NAME_SEPARATORS: &[char] = &['.', '_', ':'];

/// The characters that end a namespace segment in a `[tools].allow` **grant**:
/// only `.`, because a namespace grant is written dotted (`docs.read`).
///
/// Deliberately narrower than [`TOOL_NAME_SEPARATORS`] — `files_scratch` is not
/// a grant under the `files` namespace, and never was.
///
/// Lives here, beside [`extends_on_boundary`], rather than in
/// [`harness::build`](crate::harness::build) where it started: the namespace
/// rule now has two always-compiled callers as well as the feature-gated one
/// ([`grants_files_or_docs`](crate::company::grants_files_or_docs) is read by
/// the console route, which ships without `openhuman`), and a second
/// transcription of the separator set is precisely the fork issue #461 removed.
pub(crate) const NAMESPACE_SEPARATORS: &[char] = &['.'];

/// Whether `name` *is* `prefix`, or extends it and stops on a namespace
/// boundary drawn from `separators`.
///
/// The single boundary rule behind every grant match in the crate. Both the
/// per-tool matcher ([`grant_matches`]) and the per-namespace matcher
/// ([`grants_cover`](crate::harness::build::grants_cover)) route through it, so
/// a grant cannot mean one thing when a tool is invoked and another when a tool
/// family is wired — the disagreement issue #461 reported.
///
/// A bare `starts_with` is not the same predicate: it makes `composio_list*`
/// cover every name merely *beginning* with those letters, and `file*` cover
/// `filesystem_wipe`. `[tools].allow` is a permission boundary, so the prefix
/// must land on a separator (or the grant must already end on one, as `email.*`
/// and `mcp:*` do) before the rest of the name is accepted.
pub(crate) fn extends_on_boundary(name: &str, prefix: &str, separators: &[char]) -> bool {
    if name == prefix {
        return true;
    }
    match name.strip_prefix(prefix) {
        // `prefix` already carries the separator (`email.`, `mcp:`), so the
        // whole namespace under it is inside the grant.
        Some(_) if prefix.ends_with(separators) => true,
        // Otherwise the name itself must break on one (`file` + `_read`).
        Some(rest) => rest.starts_with(separators),
        None => false,
    }
}

/// Matches a single grant glob against a tool name.
///
/// A tool is granted when the glob matches it exactly, via a trailing `*`
/// prefix that ends on a namespace boundary (`email.*` → `email.send`, `file*`
/// → `file_read` but **not** `filesystem_wipe`), or the catch-all `*`. Shared
/// with the OpenHuman-backed provider so both enforce grants identically, and
/// boundary-checked through [`extends_on_boundary`] so it agrees with
/// [`grants_cover`](crate::harness::build::grants_cover).
pub(crate) fn grant_matches(grant: &str, tool: &str) -> bool {
    if grant == "*" {
        return true;
    }
    if let Some(prefix) = grant.strip_suffix('*') {
        return extends_on_boundary(tool, prefix, TOOL_NAME_SEPARATORS);
    }
    grant == tool
}

/// Whether an agent's effective tool `grants` cover the MCP server named `name`,
/// using the same glob semantics as every other grant (`mcp:*` grants all,
/// `mcp:notion` is exact). The single primitive read by both the harness's
/// per-agent registry assembly (`registry_for_agent`) and the console's
/// reachability view (issue #568), so the two can never disagree about which
/// agents reach a server. `grants` are the *effective* grants — resolve them
/// with [`agent_effective_grants`](crate::runtime::builder::agent_effective_grants)
/// first, never the raw per-agent `tools`.
pub(crate) fn grants_cover_server(grants: &[String], name: &str) -> bool {
    let want = format!("mcp:{name}");
    // MCP is an explicit company opt-in. The generic matcher deliberately
    // treats `*` as universal, but carrying that rule into this namespace would
    // make a wildcard-only company reach every installed server.
    grants
        .iter()
        .filter(|grant| grant.as_str() != "*")
        .any(|grant| grant_matches(grant, &want))
}

/// Whether an agent's effective tool `grants` cover the directory-installed MCP
/// server identified by `server_id`, the registry-side sibling of
/// [`grants_cover_server`].
///
/// `server_id` is the install's own identifier, not its display or qualified
/// name: a scoped grant spells that identifier (`mcp_registry.<server_id>`).
/// A bare `mcp_registry` grant covers every install, which is what the grant
/// meant before scoping existed.
///
/// Serves two shapes of caller. A tool addressed by a `server_id` argument
/// gates on this before dispatching; a tool that *enumerates* installs carries
/// no such argument and must instead filter its rows through this, the way
/// `registry_for_agent` filters declared servers with
/// [`grants_cover_server`]. `grants` are the *effective* grants — resolve them
/// with [`agent_effective_grants`](crate::runtime::builder::agent_effective_grants)
/// first, never the raw per-agent `tools`.
///
/// Ungated, like [`grants_cover_server`]: the call path that enforces it ships
/// with the harness, but the agent prompt that tells a model which install it
/// may address is composed without one, and both have to answer this question
/// the same way.
#[cfg_attr(not(feature = "openhuman"), allow(dead_code))]
pub(crate) fn grants_cover_registry_server(grants: &[String], server_id: &str) -> bool {
    let want = format!("mcp_registry.{server_id}");
    // Only a grant rooted at this namespace reaches it. The catch-all `*` never
    // confers it, and neither does a prefix wildcard that merely spans into it:
    // `_` is a boundary for the shared matcher, so `mcp*` — a grant written for
    // the `mcp:<server>` bridge — would otherwise reach every third-party
    // install. `grants_mcp_registry_explicit`, which decides whether the tools
    // are wired at all, accepts neither, and two gates disagreeing about what
    // confers a namespace is how a boundary widens without anyone seeing it.
    grants.iter().any(|grant| {
        let grant = grant.as_str();
        grant == "mcp_registry"
            || (grant.starts_with("mcp_registry.") && grant_matches(grant, &want))
    })
}

#[async_trait]
impl ToolProvider for StubToolProvider {
    async fn catalog(&self, _company: &CompanyId) -> Result<Vec<ToolSpec>> {
        // Phase 1 wires no real tools; the catalog is intentionally empty.
        Ok(Vec::new())
    }

    async fn invoke(&self, _company: &CompanyId, call: ToolCall) -> Result<ToolResult> {
        // Enforce the grant before any (future) side effect.
        if !self.is_granted(&call.tool) {
            return Err(OpenCompanyError::ToolNotGranted(call.tool));
        }
        // Granted but unimplemented: report a failed-but-well-formed result
        // rather than a hard error, so a grant misconfiguration and a missing
        // implementation stay distinguishable.
        Ok(ToolResult {
            ok: false,
            output: serde_json::json!({
                "error": "tool not implemented in Phase 1",
                "tool": call.tool,
            }),
        })
    }
}

#[cfg(test)]
#[path = "tools_tests.rs"]
mod tests;
