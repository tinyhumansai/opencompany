//! The [`ToolProvider`] port: tool catalog and invocation, scoped per company.

use async_trait::async_trait;

use crate::Result;
use crate::ports::types::{CompanyId, ToolCall, ToolResult, ToolSpec};

/// Tool catalog + invocation. Backed by OpenHuman JSON-RPC by default, with
/// TinyAgents built-ins as fallback.
///
/// Grants come from the manifest (`[tools].allow`, per-agent `tools`);
/// `invoke` MUST reject calls outside the grant before any side effect.
#[async_trait]
pub trait ToolProvider: Send + Sync {
    /// Lists the tools granted to a company.
    async fn catalog(&self, company: &CompanyId) -> Result<Vec<ToolSpec>>;
    /// Invokes a tool on behalf of a company.
    async fn invoke(&self, company: &CompanyId, call: ToolCall) -> Result<ToolResult>;
}
