//! The workflow [`ToolInvoker`]: a `tool_call` node runs a real Cell A toolbelt
//! tool, fail-closed on the company's `[tools].allow` grants.
//!
//! On construction the invoker builds the same Cell A toolbelt a roster agent
//! gets — `shell` (+ `read_workspace_state`), `code` (`apply_patch`,
//! `git_operations`, `csv_export`), and `web` (`web_fetch`, `http_request`,
//! `curl`, `image_info`) — under ONE exec-security policy scoped to a dedicated
//! per-company workflow workspace, then indexes the tools by their runtime
//! [`name()`](openhuman_core::openhuman::tools::Tool::name). A `tool_call` node's
//! `slug` selects one by name.
//!
//! It also wires the metered `search` family (`web_search`) — the discovery tool
//! the `web` namespace never had (`web_fetch` / `http_request` / `curl` only read
//! a URL the agent already has) — on the same two gates the agent builder uses
//! ([`crate::harness::build::build_agent`]): an **explicit** `search` grant
//! (`grants_search_explicit`; the catch-all `*` never confers it, because each
//! call is a priced managed request) AND a managed search backend on the deps.
//! Granted-but-uncredentialed wires nothing and warns, so `web_search` degrades
//! gracefully when no managed credential is configured (fail-closed).
//!
//! Every invocation is **fail-closed**: the slug's grant namespace (via
//! [`toolbelt::namespace_of`]) must be granted by the company's `[tools].allow`
//! before the tool is even looked up. Construction above and refusal below both
//! ask [`grants_workflow_namespace`] — the grant-intersection rule an agent's
//! exec tools use, except for the priced `search` namespace, which requires an
//! **explicit** `search` grant rather than glob coverage. So `*` never buys a
//! managed search call, and the invoke-time gate matches construction because
//! neither can answer the question differently.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tinyflows::caps::ToolInvoker;
use tinyflows::error::{EngineError, Result as TfResult};

use oh::security::SecurityPolicy;
use oh::tools::{Tool, ToolResult};
use openhuman_core::openhuman as oh;

use crate::harness::search::{SearchBackend, SearchMetering};
use crate::harness::toolbelt::{self, CapabilityFilter};

/// The grant namespaces a workflow `tool_call` can actually reach — the exec
/// belt (`shell` / `code` / `web`) plus the metered `search` family, exactly what
/// [`WorkflowToolInvoker::new`] wires.
///
/// It is deliberately a STRICT subset of
/// [`GATEABLE_NAMESPACES`](crate::company::GATEABLE_NAMESPACES): `media` and
/// `composio` map to a namespace via [`toolbelt::namespace_of`] but are
/// agent-turn tool families this invoker never builds, and `subagent` is not a
/// toolbelt tool at all. A slug in one of those namespaces would pass
/// [`invoke`](WorkflowToolInvoker::invoke)'s grant gate and then ALWAYS miss the
/// tool lookup. Author-time validation (`validate_tool_call_node`) rejects any
/// slug whose namespace falls outside this set, so a save can't green-light a
/// slug the run would always fail to look up — keep the two in lockstep.
pub(crate) const WORKFLOW_TOOL_NAMESPACES: [&str; 4] = ["shell", "code", "web", "search"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MissingReason {
    SearchBackendNotConfigured,
    CapabilityTierFiltered,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct WorkflowToolWiring {
    pub(crate) wired_namespaces: BTreeSet<&'static str>,
    pub(crate) missing: BTreeMap<&'static str, MissingReason>,
}

fn capability_filtered(filter: &CapabilityFilter, namespace: &'static str) -> bool {
    match filter {
        CapabilityFilter::AllowAll => false,
        CapabilityFilter::DenyNamespaces(denied) => denied.contains(namespace),
    }
}

/// Whether `[tools].allow` grants a **workflow-tool namespace** (issue #874).
///
/// The one grant rule every workflow-tool decision keys off: the priced `search`
/// family needs an EXPLICIT `search` grant — a `*` wildcard never confers it,
/// because each call is a priced managed request — while every other namespace
/// uses the ordinary grant-glob intersection.
///
/// Three places used to spell this split out by hand, and they must agree or the
/// system contradicts itself: author-time validation
/// (`company::validate_tool_call_node`), the grounding lists
/// (`workflow_effective_tool_slugs` / `workflow_granted_but_unwired_tool_slugs`,
/// via `grants_workflow_tool`), and [`refusal_for`] below. A slug offered for
/// grounding that validation would reject, or accepted at save and refused at
/// run for a *grant* reason, is a bug in the seam rather than in any one of
/// them — so the split lives here once, beside the wiring rule it pairs with.
///
/// It cannot live with its siblings in the always-compiled `company::types`:
/// `grants_cover` is behind the `openhuman` feature, as is every caller.
pub(crate) fn grants_workflow_namespace(grants: &[String], namespace: &str) -> bool {
    if namespace == "search" {
        crate::company::grants_search_explicit(grants)
    } else {
        crate::harness::build::grants_cover(grants, namespace)
    }
}

pub(crate) fn workflow_tool_wiring(deps: &crate::harness::HarnessDeps) -> WorkflowToolWiring {
    let mut wiring = WorkflowToolWiring::default();
    for namespace in WORKFLOW_TOOL_NAMESPACES {
        let missing = if namespace == "search" && deps.search.is_none() {
            Some(MissingReason::SearchBackendNotConfigured)
        } else if capability_filtered(&deps.capabilities, namespace) {
            Some(MissingReason::CapabilityTierFiltered)
        } else {
            None
        };
        if let Some(reason) = missing {
            wiring.missing.insert(namespace, reason);
        } else {
            wiring.wired_namespaces.insert(namespace);
        }
    }
    wiring
}

pub(crate) fn refusal_for(
    slug: &str,
    grants: &[String],
    wiring: &WorkflowToolWiring,
) -> Option<String> {
    let Some(namespace) = toolbelt::namespace_of(slug) else {
        return Some(format!("tool_call '{slug}' is not a wired workflow tool"));
    };
    if !grants_workflow_namespace(grants, namespace) {
        return Some(format!(
            "tool_call '{slug}' (namespace '{namespace}') is not granted by this company's [tools].allow"
        ));
    }
    match wiring.missing.get(namespace) {
        Some(MissingReason::SearchBackendNotConfigured) => Some(format!(
            "tool_call '{slug}' is granted, but no managed search backend is configured on this deployment; ask the platform operator to configure search or remove the node"
        )),
        Some(MissingReason::CapabilityTierFiltered) => Some(format!(
            "tool_call '{slug}' is granted, but the deployment's capability tier filtered it; ask the platform operator to raise the capability tier or remove the node"
        )),
        None if !wiring.wired_namespaces.contains(namespace) => Some(format!(
            "tool_call '{slug}' is not available in company workflows"
        )),
        None => None,
    }
}

/// The wired workflow-tool slugs, paired with the grant namespace each maps to —
/// the reverse of [`toolbelt::namespace_of`], restricted to the families
/// [`WorkflowToolInvoker::new`] actually builds ([`WORKFLOW_TOOL_NAMESPACES`]).
///
/// [`namespace_of`](toolbelt::namespace_of) answers "which namespace gates this
/// slug", but nothing enumerates the slugs a namespace contains — and the
/// create-time copilot (issue #753) needs exactly that, so it can ground the
/// model in the real tool names a company's `[tools].allow` reaches rather than
/// bare namespace words. This is that enumeration, and it is a **strict
/// derivative** of `namespace_of`, not a second source of truth: every entry's
/// namespace is asserted to match `namespace_of(slug)` by
/// [`the_slug_table_agrees_with_namespace_of`], so a tool added to the toolbelt
/// (and its `namespace_of` arm) without a row here fails the test rather than
/// silently narrowing what the copilot can propose.
///
/// `media` / `composio` / `repo` slugs are deliberately absent — they map to a
/// namespace but are agent-turn families the workflow invoker never wires (the
/// same reason they are excluded from [`WORKFLOW_TOOL_NAMESPACES`]).
/// Since #813 this table is a **test-only** pin: [`WORKFLOW_TOOL_CATALOG`] is the
/// one source callers ground and validate against, and
/// [`the_catalog_agrees_with_the_slug_table_and_namespace_of`] asserts the
/// catalogue names exactly these slugs — so a tool added to the belt (and its
/// `namespace_of` arm) without a catalogue row still fails a test rather than
/// silently narrowing what the copilot can propose.
#[cfg(test)]
pub(crate) const WORKFLOW_TOOL_SLUGS: &[(&str, &str)] = &[
    ("shell", "shell"),
    ("read_workspace_state", "shell"),
    ("apply_patch", "code"),
    ("git_operations", "code"),
    ("csv_export", "code"),
    ("web_fetch", "web"),
    ("http_request", "web"),
    ("curl", "web"),
    ("image_info", "web"),
    ("web_search", "search"),
];

/// One row of the create-time copilot's tool catalogue (issue #813): a wired
/// `tool_call` slug, the grant namespace it maps to, an **honest** one-line
/// capability, and the args a proposed node must carry.
///
/// The capability line states what the tool can AND plainly cannot do, so the
/// model is grounded in the real shape of the tool rather than its name alone —
/// the `read_workspace_state` "overview only, cannot read a file" gap is the case
/// that motivated this. `required_args` names the keys the engine reads from a
/// node's `config.args` (tinyflows resolves a `tool_call`'s arguments from
/// `config.args`, not from the node config root — see
/// `vendor/openhuman/vendor/tinyflows/src/nodes/integration/tool_call.rs`), so a
/// slug that runs but does nothing useful with empty args is caught at author
/// time by [`validate_tool_call_node`](crate::company::workflow_create).
pub(crate) struct WorkflowToolInfo {
    /// The runtime tool name (== a `tool_call` node's `config.slug`).
    pub(crate) slug: &'static str,
    /// The grant namespace [`toolbelt::namespace_of`] maps `slug` to.
    pub(crate) namespace: &'static str,
    /// One honest sentence: what the tool does, and what it cannot.
    pub(crate) capability: &'static str,
    /// The `config.args` keys a proposed node must set for the tool to do
    /// anything — empty when the tool has no required arg.
    pub(crate) required_args: &'static [&'static str],
}

/// The wired workflow tools, each with the honest capability line and required
/// args the create-time copilot grounds the model in (issue #813).
///
/// A **strict** companion of [`WORKFLOW_TOOL_SLUGS`]: it names the exact same
/// slugs (asserted both directions by
/// [`the_catalog_agrees_with_the_slug_table_and_namespace_of`]) and each row's
/// namespace is what [`toolbelt::namespace_of`] returns, so the catalogue cannot
/// drift from what the invoker actually wires or from what a proposed `tool_call`
/// clears at courtesy validation. The capability lines and `required_args` are
/// pinned to the vendored tool schemas: `required_args` is each tool's schema
/// `required` list, and the "cannot" halves state a real limit of the tool
/// (`read_workspace_state` reads no file; `web_fetch` searches for nothing).
pub(crate) const WORKFLOW_TOOL_CATALOG: &[WorkflowToolInfo] = &[
    WorkflowToolInfo {
        slug: "shell",
        namespace: "shell",
        capability: "runs a shell command in the company workspace (create/edit/run files); the \
                     workspace starts empty each run",
        required_args: &["command"],
    },
    WorkflowToolInfo {
        slug: "read_workspace_state",
        namespace: "shell",
        capability: "an overview ONLY — git status, recent commits and the top-level file tree; it \
                     CANNOT read a file's contents or run a command",
        required_args: &[],
    },
    WorkflowToolInfo {
        slug: "apply_patch",
        namespace: "code",
        capability: "applies exact-string edits to files already in the workspace",
        required_args: &["edits"],
    },
    WorkflowToolInfo {
        slug: "git_operations",
        namespace: "code",
        capability: "structured git actions (status/diff/log/branch/commit/add/checkout/stash) in \
                     the workspace",
        required_args: &["operation"],
    },
    WorkflowToolInfo {
        slug: "csv_export",
        namespace: "code",
        capability: "writes a JSON array of objects to a CSV file in the workspace",
        required_args: &["data", "filename"],
    },
    WorkflowToolInfo {
        slug: "web_fetch",
        namespace: "web",
        capability: "GETs a URL you already have and returns its page text (truncated); it cannot \
                     search for a URL",
        required_args: &["url"],
    },
    WorkflowToolInfo {
        slug: "http_request",
        namespace: "web",
        capability: "makes an HTTP request (GET/POST/…) to an allowlisted API URL you already have",
        required_args: &["url"],
    },
    WorkflowToolInfo {
        slug: "curl",
        namespace: "web",
        capability: "downloads a file from an http(s) URL into the workspace",
        required_args: &["url"],
    },
    WorkflowToolInfo {
        slug: "image_info",
        namespace: "web",
        capability: "reads image metadata (format, dimensions, size) from a workspace file",
        required_args: &["path"],
    },
    WorkflowToolInfo {
        slug: "web_search",
        namespace: "search",
        capability: "runs a metered web search for a query and returns result links and snippets",
        required_args: &["query"],
    },
];

/// The catalogue row for `slug`, if it is a wired workflow tool (issue #813).
pub(crate) fn workflow_tool_info(slug: &str) -> Option<&'static WorkflowToolInfo> {
    WORKFLOW_TOOL_CATALOG.iter().find(|info| info.slug == slug)
}

/// A [`ToolInvoker`] over the Cell A toolbelt (plus the metered `search` family),
/// scoped to a per-company workflow workspace and gated by the company's
/// `[tools].allow` grants.
pub struct WorkflowToolInvoker {
    /// The wired toolbelt tools, indexed by runtime `name()` (== the node slug).
    tools: HashMap<String, Arc<dyn Tool>>,
    /// The company's `[tools].allow` grant globs — the fail-closed gate.
    grants: Vec<String>,
    wiring: WorkflowToolWiring,
}

impl WorkflowToolInvoker {
    /// Builds the invoker: assemble the Cell A toolbelt under `security`
    /// (sandboxed to `workspace`), run it through the capability `filter`, and
    /// index the survivors by name. `grants` is the company's `[tools].allow`.
    ///
    /// `audit_dir` is the host-owned shell audit sink (issue #775) and is
    /// **separate from `workspace`** on purpose: `workspace` is the
    /// `workspace_only` policy root a `tool_call` node's file/exec tools are
    /// sandboxed to, so a sink inside it would be a policy-permitted write
    /// target for the workflow's own `shell`. It is passed in rather than
    /// derived here for the same reason
    /// [`HarnessDeps::audit_root`](crate::harness::HarnessDeps::audit_root) is
    /// an explicit field.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        security: Arc<SecurityPolicy>,
        workspace: &Path,
        audit_dir: &Path,
        web_allowed_domains: Vec<String>,
        grants: Vec<String>,
        filter: &CapabilityFilter,
        search: Option<&SearchBackend>,
        search_metering: SearchMetering,
        wiring: WorkflowToolWiring,
    ) -> Self {
        // Mirror `build_agent`: do not initialize a tool family (or its audit
        // state) unless the company's grants can invoke that namespace.
        //
        // Every arm reads `grants_workflow_namespace`, the same rule
        // `refusal_for` applies below. Construction and refusal disagreeing is
        // the one failure this file cannot afford: a family wired here but
        // refused there is a tool that exists and always errors, and the reverse
        // is a refusal for a tool that is sitting right there.
        let mut tools: Vec<Box<dyn Tool>> = Vec::new();
        if grants_workflow_namespace(&grants, "shell") && wiring.wired_namespaces.contains("shell")
        {
            tools.extend(toolbelt::shell_tools(
                security.clone(),
                toolbelt::native_runtime(),
                toolbelt::shell_audit(audit_dir),
                workspace,
            ));
        }
        if grants_workflow_namespace(&grants, "code") && wiring.wired_namespaces.contains("code") {
            tools.extend(toolbelt::code_tools(security.clone(), workspace));
        }
        if grants_workflow_namespace(&grants, "web") && wiring.wired_namespaces.contains("web") {
            tools.extend(toolbelt::web_tools(
                security,
                web_allowed_domains,
                workspace,
            ));
        }
        // Metered web search (issue #238) — mirror `build_agent`'s two-gate
        // wiring exactly: an EXPLICIT `search` grant (which is what
        // `grants_workflow_namespace` resolves to for this namespace; the
        // catch-all `*` never confers it, because each call is a priced managed
        // request) AND a managed search backend on the deps. Granted-but-
        // uncredentialed wires nothing and warns, so `web_search` degrades
        // gracefully when no managed credential is configured (fail-closed).
        if grants_workflow_namespace(&grants, "search")
            && wiring.wired_namespaces.contains("search")
        {
            match search {
                Some(backend) => {
                    tools.extend(crate::harness::search::search_tools(
                        backend,
                        search_metering,
                    ));
                }
                None => tracing::warn!(
                    "[workflow] company explicitly grants `search` but no managed search backend \
                     is configured; web_search NOT wired (fail-closed)"
                ),
            }
        }
        // The namespaces wired above (shell / code / web / search) are the
        // canonical [`WORKFLOW_TOOL_NAMESPACES`] set author-time validation gates
        // tool_call slugs against — a family added here must be added there too.
        //
        // Apply the capability-tier filter (identity in production) just as the
        // agent builder does, so the workflow surface never exceeds the agent one.
        let tools = toolbelt::filter_by_capabilities(tools, filter);

        let tools = tools
            .into_iter()
            .map(|tool| (tool.name().to_string(), Arc::<dyn Tool>::from(tool)))
            .collect();

        Self {
            tools,
            grants,
            wiring,
        }
    }
}

#[async_trait]
impl ToolInvoker for WorkflowToolInvoker {
    /// Executes the toolbelt tool named `slug`.
    ///
    /// `conn` is ignored in P1: OpenCompany has no per-account connection
    /// registry yet, so a `tool_call` acts as the company itself (the toolbelt
    /// tools are workspace/company scoped, not per-external-account). Threading a
    /// real connection is a documented follow-on.
    async fn invoke(&self, slug: &str, args: Value, _conn: Option<&str>) -> TfResult<Value> {
        // Issue #846: a call this lineage already made, replayed rather than
        // repeated. Answered from the arguments the host wrote onto the node at
        // translation time; nothing is looked up and nothing executes.
        //
        // Deliberately ABOVE the grant check, and that is not a hole. The check
        // exists to stop a call reaching a capability the company did not grant,
        // and this arm reaches no capability at all — there is no tool, no
        // namespace and no network. Below the check it would have to be granted
        // a namespace of its own, which would be a real widening in exchange for
        // nothing.
        if let Some(result) = super::super::replay::replayed_result(slug, &args) {
            return Ok(result);
        }
        // FAIL-CLOSED grant check FIRST, before any lookup or execution.
        if let Some(message) = refusal_for(slug, &self.grants, &self.wiring) {
            return Err(EngineError::Capability(message));
        }
        // The priced `search` namespace needs an EXPLICIT `search` grant — the
        // catch-all `*` must never confer a managed search call — so this gate
        // matches the construction gate in `new` (and `build::build_agent`).
        // Every other namespace uses the ordinary grant-glob intersection.
        let tool = self.tools.get(slug).ok_or_else(|| {
            EngineError::Capability(format!(
                "tool_call '{slug}' is not available in company workflows"
            ))
        })?;

        tracing::debug!(slug, "workflow tool_call: invoking toolbelt tool");
        let result = tool
            .execute(args)
            .await
            .map_err(|err| EngineError::Capability(format!("tool_call '{slug}' failed: {err}")))?;
        tool_result_to_value(slug, result)
    }
}

/// Maps a toolbelt [`ToolResult`] onto the engine's JSON. An error result
/// becomes an [`EngineError::Capability`] (so the node's `on_error`/retry policy
/// governs it); a success whose text is a single JSON-parsable block passes that
/// JSON through, else it is wrapped as `{ "text": … }`.
fn tool_result_to_value(slug: &str, result: ToolResult) -> TfResult<Value> {
    if result.is_error {
        return Err(EngineError::Capability(format!(
            "tool_call '{slug}': {}",
            result.output()
        )));
    }
    let text = result.output();
    match serde_json::from_str::<Value>(&text) {
        Ok(value) => Ok(value),
        Err(_) => Ok(json!({ "text": text })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`WORKFLOW_TOOL_SLUGS`] is a derivative of
    /// [`toolbelt::namespace_of`](crate::harness::toolbelt::namespace_of), not a
    /// second source of truth: every row's namespace must be what `namespace_of`
    /// returns for that slug, and must be one the invoker actually wires
    /// ([`WORKFLOW_TOOL_NAMESPACES`]). A toolbelt tool added or re-namespaced
    /// without updating this table fails here rather than silently changing what
    /// the create-time copilot (issue #753) can ground the model in.
    #[test]
    fn the_slug_table_agrees_with_namespace_of() {
        for (slug, namespace) in WORKFLOW_TOOL_SLUGS {
            assert_eq!(
                toolbelt::namespace_of(slug),
                Some(*namespace),
                "slug `{slug}` is listed under `{namespace}` but namespace_of disagrees"
            );
            assert!(
                WORKFLOW_TOOL_NAMESPACES.contains(namespace),
                "slug `{slug}`'s namespace `{namespace}` is not a wired workflow namespace"
            );
        }
    }

    /// [`WORKFLOW_TOOL_CATALOG`] is a strict companion of
    /// [`WORKFLOW_TOOL_SLUGS`], not a second source of truth: it names the exact
    /// same slugs (both directions), every row's namespace is what
    /// [`toolbelt::namespace_of`] returns and is a wired workflow namespace, and
    /// no capability line or required-arg name is blank. A tool added to (or
    /// dropped from) the slug table without a matching catalogue edit fails here
    /// rather than silently narrowing — or mis-describing — what the create-time
    /// copilot (issue #813) grounds the model in.
    #[test]
    fn the_catalog_agrees_with_the_slug_table_and_namespace_of() {
        use std::collections::HashSet;
        let catalog: HashSet<&str> = WORKFLOW_TOOL_CATALOG.iter().map(|info| info.slug).collect();
        let table: HashSet<&str> = WORKFLOW_TOOL_SLUGS.iter().map(|(slug, _)| *slug).collect();
        assert_eq!(
            catalog, table,
            "the tool catalogue and the slug table must name the same slugs"
        );
        for info in WORKFLOW_TOOL_CATALOG {
            assert_eq!(
                toolbelt::namespace_of(info.slug),
                Some(info.namespace),
                "catalogue slug `{}` is listed under `{}` but namespace_of disagrees",
                info.slug,
                info.namespace
            );
            assert!(
                WORKFLOW_TOOL_NAMESPACES.contains(&info.namespace),
                "catalogue slug `{}`'s namespace `{}` is not a wired workflow namespace",
                info.slug,
                info.namespace
            );
            assert!(
                !info.capability.trim().is_empty(),
                "catalogue slug `{}` has an empty capability line",
                info.slug
            );
            assert!(
                info.required_args.iter().all(|arg| !arg.trim().is_empty()),
                "catalogue slug `{}` has an empty required-arg name",
                info.slug
            );
        }
    }

    #[test]
    fn json_text_block_passes_through_else_wrapped() {
        let json_result = ToolResult::success(r#"{"rows": 3}"#);
        assert_eq!(
            tool_result_to_value("csv_export", json_result).unwrap(),
            json!({ "rows": 3 })
        );

        let text_result = ToolResult::success("Exported 3 rows to exports/out.csv");
        assert_eq!(
            tool_result_to_value("csv_export", text_result).unwrap(),
            json!({ "text": "Exported 3 rows to exports/out.csv" })
        );
    }

    #[test]
    fn error_result_becomes_a_capability_error() {
        let err = tool_result_to_value("csv_export", ToolResult::error("nope")).unwrap_err();
        assert!(
            matches!(err, EngineError::Capability(ref m) if m.contains("nope")),
            "{err:?}"
        );
    }

    #[test]
    fn ungranted_and_unknown_slugs_are_rejected_fail_closed() {
        use tinyflows::caps::ToolInvoker;
        // No `code` grant → csv_export (a `code`-namespace tool) is denied even
        // though it is wired.
        let invoker = WorkflowToolInvoker {
            tools: HashMap::new(),
            grants: vec!["web.*".to_string()],
            wiring: WorkflowToolWiring::default(),
        };
        let denied = tokio_test_block_on(invoker.invoke("csv_export", json!({}), None));
        assert!(
            matches!(denied, Err(EngineError::Capability(ref m)) if m.contains("not granted")),
            "{denied:?}"
        );
        // A slug with no toolbelt namespace is rejected as unwired.
        let unwired = tokio_test_block_on(invoker.invoke("email.send", json!({}), None));
        assert!(
            matches!(unwired, Err(EngineError::Capability(ref m)) if m.contains("not a wired")),
            "{unwired:?}"
        );
    }

    #[test]
    fn the_search_namespace_requires_an_explicit_grant_not_a_wildcard() {
        use tinyflows::caps::ToolInvoker;
        // `*` covers ordinary namespaces but must NOT confer the priced `search`
        // family — the invoke-time gate mirrors construction (build.rs).
        let wildcard = WorkflowToolInvoker {
            tools: HashMap::new(),
            grants: vec!["*".to_string()],
            wiring: WorkflowToolWiring::default(),
        };
        let denied = tokio_test_block_on(wildcard.invoke("web_search", json!({}), None));
        assert!(
            matches!(denied, Err(EngineError::Capability(ref m)) if m.contains("not granted")),
            "{denied:?}"
        );
        // An explicit `search` grant passes the gate; the empty tool map then
        // fails the lookup with a different, later error.
        let granted = WorkflowToolInvoker {
            tools: HashMap::new(),
            grants: vec!["search".to_string()],
            wiring: WorkflowToolWiring::default(),
        };
        let looked_up = tokio_test_block_on(granted.invoke("web_search", json!({}), None));
        assert!(
            matches!(looked_up, Err(EngineError::Capability(ref m)) if m.contains("not available")),
            "{looked_up:?}"
        );
    }

    /// The replay arm answers a sentinel invocation on an invoker that grants
    /// **nothing**, and reaches no capability doing it (issue #846).
    ///
    /// Both halves matter and neither is provable without the other. Answering
    /// on a zero-grant invoker is what proves the arm sits ABOVE the fail-closed
    /// grant check — if it sat below, a continuation would have to be granted a
    /// namespace for a call it does not make. And the same invoker refusing a
    /// real slug in the same test is what proves the arm is a narrow sentinel
    /// rather than a hole: nothing else got easier to invoke.
    #[tokio::test]
    async fn the_replay_sentinel_is_answered_without_a_grant_and_reaches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let audit = tempfile::tempdir().unwrap();
        let security = Arc::new(toolbelt::exec_security(
            dir.path(),
            crate::harness::policy::PolicyMode::Supervised,
        ));
        let invoker = WorkflowToolInvoker::new(
            security,
            dir.path(),
            audit.path(),
            Vec::new(),
            // No grants at all: every real slug is refused fail-closed.
            Vec::new(),
            &CapabilityFilter::AllowAll,
            None,
            test_metering(),
            WorkflowToolWiring::default(),
        );

        let recorded = json!({ "status": 201, "id": "abc" });
        let encoded = serde_json::to_string(&recorded).unwrap();
        let replayed = invoker
            .invoke(
                crate::workflows::replay::REPLAY_SLUG,
                json!({ crate::workflows::replay::REPLAY_RESULT_KEY: encoded }),
                None,
            )
            .await
            .expect("the sentinel is answered from its own arguments");
        assert_eq!(replayed, recorded);

        // The control: the same invoker still refuses an ungranted real tool.
        let refused = invoker
            .invoke("shell", json!({ "command": "id" }), None)
            .await;
        assert!(
            matches!(refused, Err(EngineError::Capability(ref m)) if m.contains("not granted")),
            "{refused:?}"
        );
    }

    #[test]
    fn granted_search_refusals_name_provider_and_capability_failures() {
        let provider = WorkflowToolWiring {
            missing: [("search", MissingReason::SearchBackendNotConfigured)]
                .into_iter()
                .collect(),
            ..WorkflowToolWiring::default()
        };
        let provider_message = refusal_for("web_search", &["search".to_string()], &provider)
            .expect("missing provider refuses");
        assert!(provider_message.contains("no managed search backend"));
        assert!(provider_message.contains("ask the platform operator"));

        let tier = WorkflowToolWiring {
            missing: [("search", MissingReason::CapabilityTierFiltered)]
                .into_iter()
                .collect(),
            ..WorkflowToolWiring::default()
        };
        let tier_message = refusal_for("web_search", &["search".to_string()], &tier)
            .expect("tier filtering refuses");
        assert!(tier_message.contains("capability tier filtered"));
        assert!(tier_message.contains("raise the capability tier"));
    }

    #[test]
    fn construction_only_initializes_granted_tool_families() {
        let dir = tempfile::tempdir().unwrap();
        // A SEPARATE root from the workspace: the audit sink is host-owned and
        // must never live inside the directory the exec policy sandboxes to
        // (issue #775).
        let audit = tempfile::tempdir().unwrap();
        let security = Arc::new(toolbelt::exec_security(
            dir.path(),
            crate::harness::policy::PolicyMode::Supervised,
        ));

        let none = WorkflowToolInvoker::new(
            security.clone(),
            dir.path(),
            audit.path(),
            Vec::new(),
            Vec::new(),
            &CapabilityFilter::AllowAll,
            None,
            test_metering(),
            WorkflowToolWiring::default(),
        );
        assert!(none.tools.is_empty());

        let code = WorkflowToolInvoker::new(
            security,
            dir.path(),
            audit.path(),
            Vec::new(),
            vec!["code.*".to_string()],
            &CapabilityFilter::AllowAll,
            None,
            test_metering(),
            WorkflowToolWiring {
                wired_namespaces: ["code"].into_iter().collect(),
                ..WorkflowToolWiring::default()
            },
        );
        assert!(code.tools.contains_key("apply_patch"));
        assert!(code.tools.contains_key("csv_export"));
        assert!(!code.tools.contains_key("shell"));
        assert!(!code.tools.contains_key("web_fetch"));
    }

    #[test]
    fn search_wires_only_with_an_explicit_grant_and_a_backend() {
        let dir = tempfile::tempdir().unwrap();
        let audit = tempfile::tempdir().unwrap();
        let security = Arc::new(toolbelt::exec_security(
            dir.path(),
            crate::harness::policy::PolicyMode::Supervised,
        ));
        let backend = SearchBackend::new(
            "https://api.example.test".to_string(),
            crate::company::credentials::Credential::from_value("managed"),
            5,
        );

        // Explicit `search` grant + a backend → the metered `web_search` is wired.
        let wired = WorkflowToolInvoker::new(
            security.clone(),
            dir.path(),
            audit.path(),
            Vec::new(),
            vec!["search".to_string()],
            &CapabilityFilter::AllowAll,
            Some(&backend),
            test_metering(),
            WorkflowToolWiring {
                wired_namespaces: WORKFLOW_TOOL_NAMESPACES.into_iter().collect(),
                missing: BTreeMap::new(),
            },
        );
        assert!(wired.tools.contains_key("web_search"));

        // The catch-all `*` must NOT confer the priced search family.
        let wildcard = WorkflowToolInvoker::new(
            security.clone(),
            dir.path(),
            audit.path(),
            Vec::new(),
            vec!["*".to_string()],
            &CapabilityFilter::AllowAll,
            Some(&backend),
            test_metering(),
            WorkflowToolWiring {
                wired_namespaces: WORKFLOW_TOOL_NAMESPACES.into_iter().collect(),
                missing: BTreeMap::new(),
            },
        );
        assert!(!wildcard.tools.contains_key("web_search"));

        // Granted but uncredentialed wires nothing (fail-closed) rather than panicking.
        let uncredentialed = WorkflowToolInvoker::new(
            security,
            dir.path(),
            audit.path(),
            Vec::new(),
            vec!["search".to_string()],
            &CapabilityFilter::AllowAll,
            None,
            test_metering(),
            WorkflowToolWiring {
                wired_namespaces: WORKFLOW_TOOL_NAMESPACES.into_iter().collect(),
                missing: BTreeMap::new(),
            },
        );
        assert!(!uncredentialed.tools.contains_key("web_search"));
    }

    #[test]
    fn wiring_namespaces_match_constructed_tool_namespaces() {
        let dir = tempfile::tempdir().unwrap();
        let audit = tempfile::tempdir().unwrap();
        let security = Arc::new(toolbelt::exec_security(
            dir.path(),
            crate::harness::policy::PolicyMode::Supervised,
        ));
        let wiring = WorkflowToolWiring {
            wired_namespaces: WORKFLOW_TOOL_NAMESPACES.into_iter().collect(),
            missing: BTreeMap::new(),
        };
        let invoker = WorkflowToolInvoker::new(
            security,
            dir.path(),
            audit.path(),
            Vec::new(),
            vec!["*".to_string(), "search".to_string()],
            &CapabilityFilter::AllowAll,
            Some(&SearchBackend::new(
                "https://api.example.test".to_string(),
                crate::company::credentials::Credential::from_value("managed"),
                5,
            )),
            test_metering(),
            wiring.clone(),
        );
        let constructed: BTreeSet<&str> = invoker
            .tools
            .keys()
            .filter_map(|slug| toolbelt::namespace_of(slug))
            .collect();
        assert_eq!(constructed, wiring.wired_namespaces);
    }

    /// A throwaway [`SearchMetering`] for the construction tests — the tool is
    /// never executed here, so the company/agent/meter values are inert.
    fn test_metering() -> SearchMetering {
        SearchMetering {
            company: crate::ports::types::CompanyId::new("test"),
            agent: "workflow:test".to_string(),
            meter: None,
        }
    }

    /// Minimal blocking bridge so the fail-closed checks (which never touch the
    /// tool map) can be unit-tested without a full tokio runtime import churn.
    fn tokio_test_block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(fut)
    }
}
