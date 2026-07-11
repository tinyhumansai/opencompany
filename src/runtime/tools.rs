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
/// granted when its name matches a glob exactly, via a trailing `*` prefix
/// match (`email.*`), or the catch-all `*`.
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

/// Matches a single grant glob against a tool name.
fn grant_matches(grant: &str, tool: &str) -> bool {
    if grant == "*" {
        return true;
    }
    if let Some(prefix) = grant.strip_suffix('*') {
        return tool.starts_with(prefix);
    }
    grant == tool
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
}
