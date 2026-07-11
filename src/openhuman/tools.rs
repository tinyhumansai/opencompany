//! [`OpenHumanToolProvider`]: a [`ToolProvider`] backed by openhuman-core.
//!
//! The catalog is fetched over JSON-RPC and **filtered by the company's tool
//! grants** (`[tools].allow` intersected with per-agent `tools`, precomputed by
//! the builder). `invoke` re-checks the grant *before* issuing any RPC, so an
//! ungranted call is side-effect-free. When openhuman-core is unreachable the
//! provider degrades to a built-in `fallback` rather than failing.

use std::sync::Arc;

use async_trait::async_trait;

use crate::Result;
use crate::error::OpenCompanyError;
use crate::openhuman::rpc::{OpenHumanRpc, rpc_method};
use crate::ports::tools::ToolProvider;
use crate::ports::types::{CompanyId, ToolCall, ToolResult, ToolSpec};
use crate::runtime::tools::grant_matches;

/// A [`ToolProvider`] that delegates to openhuman-core over JSON-RPC.
pub struct OpenHumanToolProvider {
    rpc: Arc<dyn OpenHumanRpc>,
    grants: Vec<String>,
    fallback: Arc<dyn ToolProvider>,
}

impl OpenHumanToolProvider {
    /// Wires a provider over `rpc`, restricting the catalog/invocations to
    /// `grants` and degrading to `fallback` when openhuman-core fails.
    pub fn new(
        rpc: Arc<dyn OpenHumanRpc>,
        grants: Vec<String>,
        fallback: Arc<dyn ToolProvider>,
    ) -> Self {
        Self {
            rpc,
            grants,
            fallback,
        }
    }

    /// Whether `tool` is covered by any of the company's grant globs.
    fn is_granted(&self, tool: &str) -> bool {
        self.grants.iter().any(|grant| grant_matches(grant, tool))
    }
}

#[async_trait]
impl ToolProvider for OpenHumanToolProvider {
    async fn catalog(&self, company: &CompanyId) -> Result<Vec<ToolSpec>> {
        // On any RPC failure, degrade to the built-in catalog rather than error.
        let value = match self
            .rpc
            .call(&rpc_method("tools", "list"), serde_json::json!({}))
            .await
        {
            Ok(value) => value,
            Err(_) => return self.fallback.catalog(company).await,
        };
        let specs: Vec<ToolSpec> = serde_json::from_value(value)?;
        Ok(specs
            .into_iter()
            .filter(|spec| self.is_granted(&spec.name))
            .collect())
    }

    async fn invoke(&self, _company: &CompanyId, call: ToolCall) -> Result<ToolResult> {
        // Enforce the grant *before* any RPC/side effect.
        if !self.is_granted(&call.tool) {
            return Err(OpenCompanyError::ToolNotGranted(call.tool));
        }
        let params = serde_json::json!({ "tool": call.tool, "args": call.args });
        match self.rpc.call(&rpc_method("tools", "invoke"), params).await {
            // The wire result is a `ToolResult`; fall back to a well-formed
            // failure if openhuman returns a shape we cannot decode.
            Ok(value) => Ok(serde_json::from_value(value.clone()).unwrap_or(ToolResult {
                ok: false,
                output: value,
            })),
            // Granted but the RPC failed: report a failed-but-well-formed result
            // so a grant misconfiguration and a runtime failure stay distinct.
            Err(err) => Ok(ToolResult {
                ok: false,
                output: serde_json::json!({
                    "error": "openhuman rpc failed",
                    "tool": call.tool,
                    "detail": err.to_string(),
                }),
            }),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::openhuman::rpc::MockOpenHumanRpc;
    use crate::runtime::tools::StubToolProvider;

    fn company() -> CompanyId {
        CompanyId::new("acme")
    }

    fn spec(name: &str) -> serde_json::Value {
        serde_json::json!({ "name": name, "description": "", "input_schema": {} })
    }

    #[tokio::test]
    async fn catalog_filters_by_grants() {
        let rpc = Arc::new(MockOpenHumanRpc::new().with_result(
            "openhuman.tools_list",
            serde_json::json!([spec("email.send"), spec("payment.send")]),
        ));
        let provider = OpenHumanToolProvider::new(
            rpc,
            vec!["email.*".into()],
            Arc::new(StubToolProvider::new(vec!["email.*".into()])),
        );
        let catalog = provider.catalog(&company()).await.unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].name, "email.send");
    }

    #[tokio::test]
    async fn ungranted_invoke_rejected_before_rpc() {
        let rpc = Arc::new(MockOpenHumanRpc::new());
        let provider = OpenHumanToolProvider::new(
            rpc.clone(),
            vec!["email.*".into()],
            Arc::new(StubToolProvider::new(vec!["email.*".into()])),
        );
        let err = provider
            .invoke(
                &company(),
                ToolCall {
                    tool: "payment.send".into(),
                    args: serde_json::Value::Null,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, OpenCompanyError::ToolNotGranted(t) if t == "payment.send"));
        // Rejection must be side-effect-free: no RPC issued.
        assert_eq!(rpc.call_count(), 0);
    }

    #[tokio::test]
    async fn granted_invoke_decodes_tool_result() {
        let rpc = Arc::new(MockOpenHumanRpc::new().with_result(
            "openhuman.tools_invoke",
            serde_json::json!({ "ok": true, "output": { "id": "msg_1" } }),
        ));
        let provider = OpenHumanToolProvider::new(
            rpc.clone(),
            vec!["email.*".into()],
            Arc::new(StubToolProvider::new(vec!["email.*".into()])),
        );
        let result = provider
            .invoke(
                &company(),
                ToolCall {
                    tool: "email.send".into(),
                    args: serde_json::json!({ "to": "a@b.c" }),
                },
            )
            .await
            .unwrap();
        assert!(result.ok);
        assert_eq!(result.output["id"], "msg_1");
        assert_eq!(rpc.call_count(), 1);
    }

    #[tokio::test]
    async fn granted_invoke_rpc_failure_is_well_formed() {
        // No handler registered → the mock errors, standing in for an RPC failure.
        let rpc = Arc::new(MockOpenHumanRpc::new());
        let provider = OpenHumanToolProvider::new(
            rpc,
            vec!["email.*".into()],
            Arc::new(StubToolProvider::new(vec!["email.*".into()])),
        );
        let result = provider
            .invoke(
                &company(),
                ToolCall {
                    tool: "email.send".into(),
                    args: serde_json::Value::Null,
                },
            )
            .await
            .unwrap();
        assert!(!result.ok);
        assert_eq!(result.output["error"], "openhuman rpc failed");
    }

    #[tokio::test]
    async fn rpc_failure_falls_back_to_builtin_catalog() {
        // Unhealthy/erroring mock (no tools_list handler) → the stub's catalog.
        let rpc = Arc::new(MockOpenHumanRpc::new().unhealthy());
        let provider = OpenHumanToolProvider::new(
            rpc,
            vec!["email.*".into()],
            Arc::new(StubToolProvider::new(vec!["email.*".into()])),
        );
        let catalog = provider.catalog(&company()).await.unwrap();
        assert!(catalog.is_empty());
    }
}
