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
mod test {
    use super::*;

    fn company() -> CompanyId {
        CompanyId::new("acme")
    }

    fn call(tool: &str) -> ToolCall {
        ToolCall {
            tool: tool.into(),
            args: serde_json::Value::Null,
        }
    }

    #[tokio::test]
    async fn ungranted_tool_is_rejected() {
        let provider = StubToolProvider::new(vec!["email.send".into()]);
        let err = provider
            .invoke(&company(), call("payment.send"))
            .await
            .unwrap_err();
        assert!(matches!(err, OpenCompanyError::ToolNotGranted(t) if t == "payment.send"));
    }

    #[tokio::test]
    async fn granted_tool_passes_the_gate() {
        let provider = StubToolProvider::new(vec!["email.*".into()]);
        let result = provider
            .invoke(&company(), call("email.send"))
            .await
            .unwrap();
        assert!(!result.ok);
    }

    #[tokio::test]
    async fn wildcard_grants_everything() {
        let provider = StubToolProvider::new(vec!["*".into()]);
        assert!(provider.invoke(&company(), call("anything")).await.is_ok());
    }

    #[tokio::test]
    async fn empty_catalog() {
        let provider = StubToolProvider::default();
        assert!(provider.catalog(&company()).await.unwrap().is_empty());
    }

    // --- Prefix grants stop on a namespace boundary (issue #461) -------------

    /// The defect verbatim: a trailing-`*` grant used to be a bare
    /// `starts_with`, so `file*` reached every tool whose *name* merely began
    /// with those letters. `filesystem_wipe` is the shape that makes it a
    /// permission bug rather than a cosmetic one.
    #[test]
    fn prefix_grant_stops_at_the_snake_case_boundary() {
        assert!(grant_matches("file*", "file_read"));
        assert!(grant_matches("file*", "file_write"));
        // The grant itself, with nothing under it, is still covered.
        assert!(grant_matches("file*", "file"));
        // …but a longer *word* is a different namespace, not a sub-tool.
        assert!(!grant_matches("file*", "filesystem_wipe"));
        assert!(!grant_matches("file*", "filed"));
    }

    /// The issue's second example: `composio_list*` may cover the list family
    /// and nothing else.
    #[test]
    fn prefix_grant_covers_only_its_own_family() {
        assert!(grant_matches("composio_list*", "composio_list"));
        assert!(grant_matches("composio_list*", "composio_list_apps"));
        assert!(grant_matches("composio_list*", "composio_list_toolkits"));
        assert!(!grant_matches("composio_list*", "composio_listen"));
        assert!(!grant_matches("composio_list*", "composio_execute"));
    }

    #[test]
    fn wildcard_does_not_cover_mcp_servers() {
        assert!(!grants_cover_server(&["*".into()], "notion"));
        assert!(grants_cover_server(&["mcp:*".into()], "notion"));
        assert!(grants_cover_server(&["mcp:notion".into()], "notion"));
        assert!(!grants_cover_server(&["mcp:notion".into()], "linear"));
    }

    /// Every grant shape the shipped `companies/*/company.toml` manifests use
    /// keeps working exactly as before — the fix must be invisible to them.
    #[test]
    fn shipped_grant_shapes_are_unchanged() {
        // Dotted namespace globs.
        assert!(grant_matches("email.*", "email.send"));
        assert!(grant_matches("web.*", "web.fetch"));
        assert!(grant_matches("workspace.*", "workspace.read"));
        assert!(!grant_matches("email.*", "payment.send"));
        // Colon-namespaced MCP servers.
        assert!(grant_matches("mcp:*", "mcp:notion"));
        assert!(grant_matches("mcp:*", "mcp:any-server"));
        assert!(grant_matches("mcp:notion", "mcp:notion"));
        assert!(!grant_matches("mcp:notion", "mcp:slack"));
        assert!(!grant_matches("mcp:*", "mcpx:notion"));
        // Exact tokens and the catch-all.
        assert!(grant_matches("composio", "composio"));
        assert!(!grant_matches("composio", "composio.execute"));
        assert!(grant_matches("*", "anything_at_all"));
    }

    /// A bare `*` is a broad grant, not an unlimited one: the metered
    /// `web_search` surface (issue #238) must still be opted into by name.
    /// Pinned here too because the boundary fix touches the same grant lists.
    #[test]
    fn catch_all_still_confers_no_web_search() {
        assert!(!crate::company::grants_search_explicit(&["*".into()]));
        assert!(crate::company::grants_search_explicit(&["search".into()]));
    }

    /// The whole point of issue #461: one rule, two matchers. Over a shared
    /// corpus, `grant_matches` (tool names) and `grants_cover` (namespaces)
    /// must never disagree about whether a dotted grant reaches a namespace.
    ///
    /// Gated with the harness, which is where `grants_cover` is compiled.
    #[cfg(feature = "openhuman")]
    #[test]
    fn both_matchers_agree_over_a_shared_corpus() {
        use crate::harness::build::grants_cover;

        // (grant, namespace) pairs written the way a manifest writes them.
        let corpus = [
            ("docs", "docs"),
            ("docs.*", "docs"),
            ("docs.read", "docs"),
            ("*", "docs"),
            ("web.*", "docs"),
            ("documentation.*", "docs"),
            ("doc.*", "docs"),
            ("workspace", "workspace"),
            ("workspace.write", "workspace"),
            ("workspaces.write", "workspace"),
            ("search", "search"),
            ("searching", "search"),
        ];

        for (grant, namespace) in corpus {
            let by_namespace = grants_cover(&[grant.to_string()], namespace);
            // The per-tool matcher asked the same question: does this grant
            // reach *something* in the namespace? A namespace grant is probed
            // as the namespace's own glob.
            let by_tool = grant_matches(&format!("{namespace}.*"), grant)
                || grant_matches(grant, namespace)
                || grant == "*";
            assert_eq!(
                by_namespace, by_tool,
                "matchers disagree on grant {grant:?} vs namespace {namespace:?}"
            );
        }
    }

    #[test]
    fn extends_on_boundary_is_exact_prefix_or_separator() {
        // Identity.
        assert!(extends_on_boundary("docs", "docs", &['.']));
        // The prefix already carries the separator.
        assert!(extends_on_boundary("email.send", "email.", &['.']));
        assert!(extends_on_boundary("mcp:notion", "mcp:", &[':']));
        // The name breaks on one.
        assert!(extends_on_boundary("docs.read", "docs", &['.']));
        assert!(extends_on_boundary("file_read", "file", &['_']));
        // Mid-word extension is not a boundary.
        assert!(!extends_on_boundary("filesystem", "file", &['_']));
        assert!(!extends_on_boundary("docs.read", "docs", &['_']));
        // Not a prefix at all.
        assert!(!extends_on_boundary("web.fetch", "docs", &['.']));
        // A shorter name never covers a longer prefix.
        assert!(!extends_on_boundary("doc", "docs", &['.']));
    }
}
