//! Manifest `[[agent]]` → openhuman [`AgentBuilder`] wiring.
//!
//! [`build_agent`] turns one roster entry into a ready-to-run openhuman
//! [`Agent`], injecting the harness's provider, the [`OcMemory`] adapter, the
//! [`ApprovalPolicy`] tool policy, and a workspace directory. It is deliberately
//! minimal for v1:
//!
//! * **Tools** start empty. The manifest `tools ∩ agent.tools` intersection and
//!   the port-backed tool surface are ported from the legacy
//!   `src/openhuman/tools.rs` in a follow-up (WS4 subtask, tracked seam) — the
//!   builder accepts the vector so wiring them in is a one-line change.
//! * **Workflows/skills** start empty. Parsing enabled `SKILL.md` bodies via
//!   `openhuman::skills::ops_parse` depends on WS1's skill parsing; the seam is
//!   the `.workflows(...)` setter.
//!
//! The tool dispatcher is the text-based [`XmlToolDispatcher`], which needs no
//! global tool registry — the harness stays self-contained.

use std::sync::Arc;

use openhuman_core::openhuman as oh;

use oh::agent::dispatcher::XmlToolDispatcher;
use oh::agent::{Agent, AgentBuilder};
use oh::tools::Tool;

use crate::company::Agent as ManifestAgent;
use crate::error::OpenCompanyError;
use crate::harness::HarnessDeps;
use crate::harness::memory::OcMemory;
use crate::harness::policy::ApprovalPolicy;
use crate::ports::types::CompanyId;

/// Map a manifest cognition-tier hint to a hosted model/tier name.
///
/// The manifest tier "never selects a model" (that is the TinyHumans backend's
/// job); this only picks the abstract hosted workload string the provider
/// resolves. Unknown / absent tiers fall back to the conversational `chat-v1`.
pub fn model_for_tier(tier: Option<&str>) -> String {
    match tier.map(|t| t.trim().to_ascii_lowercase()).as_deref() {
        Some("reasoning") => "reasoning-v1",
        Some("agentic") => "agentic-v1",
        Some("vision") => "vision-v1",
        _ => "chat-v1",
    }
    .to_string()
}

/// Build one openhuman [`Agent`] for `manifest_agent` within `company`.
pub fn build_agent(
    company: &CompanyId,
    manifest_agent: &ManifestAgent,
    policy: ApprovalPolicy,
    deps: &HarnessDeps,
) -> crate::Result<Agent> {
    let memory = OcMemory::new(
        company.clone(),
        manifest_agent.id.clone(),
        deps.context.clone(),
    );

    // v1: no tools yet (see module docs — WS4 tool-surface seam). The manifest
    // `tools ∩ agent.tools` intersection lands here.
    let tools: Vec<Box<dyn Tool>> = Vec::new();

    let workspace = deps
        .workspace_root
        .join(company.as_ref())
        .join(&manifest_agent.id)
        .join("workspace");

    AgentBuilder::default()
        .provider_arc(deps.provider.clone())
        .memory(Arc::new(memory))
        .tools(tools)
        .tool_dispatcher(Box::new(XmlToolDispatcher))
        .tool_policy(Arc::new(policy))
        .model_name(model_for_tier(manifest_agent.tier.as_deref()))
        .workspace_dir(workspace)
        .agent_definition_name(manifest_agent.id.clone())
        .auto_save(false)
        .build()
        .map_err(|e| OpenCompanyError::Harness(format!("build agent '{}': {e}", manifest_agent.id)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_for_tier_maps_hints_and_defaults() {
        assert_eq!(model_for_tier(Some("reasoning")), "reasoning-v1");
        assert_eq!(model_for_tier(Some("AGENTIC")), "agentic-v1");
        assert_eq!(model_for_tier(None), "chat-v1");
        assert_eq!(model_for_tier(Some("mystery")), "chat-v1");
    }
}
