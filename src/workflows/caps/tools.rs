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
//! Every invocation is **fail-closed**: the slug's grant namespace (via
//! [`toolbelt::namespace_of`]) must be covered by the company's `[tools].allow`
//! globs — reusing the exact grant-intersection rule an agent's exec tools use
//! ([`crate::harness::build::grants_cover`]) — before the tool is even looked up.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tinyflows::caps::ToolInvoker;
use tinyflows::error::{EngineError, Result as TfResult};

use oh::security::SecurityPolicy;
use oh::tools::{Tool, ToolResult};
use openhuman_core::openhuman as oh;

use crate::harness::toolbelt::{self, CapabilityFilter};

/// A [`ToolInvoker`] over the Cell A toolbelt, scoped to a per-company workflow
/// workspace and gated by the company's `[tools].allow` grants.
pub struct WorkflowToolInvoker {
    /// The wired toolbelt tools, indexed by runtime `name()` (== the node slug).
    tools: HashMap<String, Arc<dyn Tool>>,
    /// The company's `[tools].allow` grant globs — the fail-closed gate.
    grants: Vec<String>,
}

impl WorkflowToolInvoker {
    /// Builds the invoker: assemble the Cell A toolbelt under `security`
    /// (sandboxed to `workspace`), run it through the capability `filter`, and
    /// index the survivors by name. `grants` is the company's `[tools].allow`.
    pub fn new(
        security: Arc<SecurityPolicy>,
        workspace: &Path,
        web_allowed_domains: Vec<String>,
        grants: Vec<String>,
        filter: &CapabilityFilter,
    ) -> Self {
        // Mirror `build_agent`: do not initialize a tool family (or its audit
        // state) unless the company's grants can invoke that namespace.
        let mut tools: Vec<Box<dyn Tool>> = Vec::new();
        if crate::harness::build::grants_cover(&grants, "shell") {
            tools.extend(toolbelt::shell_tools(
                security.clone(),
                toolbelt::native_runtime(),
                toolbelt::workspace_audit(workspace),
                workspace,
            ));
        }
        if crate::harness::build::grants_cover(&grants, "code") {
            tools.extend(toolbelt::code_tools(security.clone(), workspace));
        }
        if crate::harness::build::grants_cover(&grants, "web") {
            tools.extend(toolbelt::web_tools(
                security,
                web_allowed_domains,
                workspace,
            ));
        }
        // Apply the capability-tier filter (identity in production) just as the
        // agent builder does, so the workflow surface never exceeds the agent one.
        let tools = toolbelt::filter_by_capabilities(tools, filter);

        let tools = tools
            .into_iter()
            .map(|tool| (tool.name().to_string(), Arc::<dyn Tool>::from(tool)))
            .collect();

        Self { tools, grants }
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
        // FAIL-CLOSED grant check FIRST, before any lookup or execution.
        match toolbelt::namespace_of(slug) {
            Some(namespace) if crate::harness::build::grants_cover(&self.grants, namespace) => {}
            Some(namespace) => {
                return Err(EngineError::Capability(format!(
                    "tool_call '{slug}' (namespace '{namespace}') is not granted by this company's \
                     [tools].allow"
                )));
            }
            None => {
                return Err(EngineError::Capability(format!(
                    "tool_call '{slug}' is not a wired workflow tool"
                )));
            }
        }

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
    fn construction_only_initializes_granted_tool_families() {
        let dir = tempfile::tempdir().unwrap();
        let security = Arc::new(toolbelt::exec_security(
            dir.path(),
            crate::harness::policy::PolicyMode::Supervised,
        ));

        let none = WorkflowToolInvoker::new(
            security.clone(),
            dir.path(),
            Vec::new(),
            Vec::new(),
            &CapabilityFilter::AllowAll,
        );
        assert!(none.tools.is_empty());

        let code = WorkflowToolInvoker::new(
            security,
            dir.path(),
            Vec::new(),
            vec!["code.*".to_string()],
            &CapabilityFilter::AllowAll,
        );
        assert!(code.tools.contains_key("apply_patch"));
        assert!(code.tools.contains_key("csv_export"));
        assert!(!code.tools.contains_key("shell"));
        assert!(!code.tools.contains_key("web_fetch"));
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
