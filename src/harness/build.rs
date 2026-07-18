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
use oh::context::prompt::SystemPromptBuilder;
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

/// The persona system prompt for a company agent.
///
/// Frames the agent as its manifest role at the company, in the first person.
/// This is what makes the agent answer *as* the CEO of Acme rather than falling
/// back to openhuman's own assistant identity — the harness passes it as the
/// archetype body with the default identity section omitted.
pub fn persona_prompt(company_name: &str, agent: &ManifestAgent) -> String {
    let mut prompt = format!(
        "You are the {role} at {company}. Speak in the first person as this role.",
        role = agent.role,
        company = company_name,
    );
    if let Some(description) = agent.description.as_deref() {
        let description = description.trim();
        if !description.is_empty() {
            prompt.push(' ');
            prompt.push_str(description);
        }
    }
    prompt
}

/// Build one openhuman [`Agent`] for `manifest_agent` within `company`.
pub fn build_agent(
    company: &CompanyId,
    company_name: &str,
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

    // Persona over openhuman's own identity: `omit_identity = true` drops the
    // "you are OpenHuman" preamble so the agent speaks as its company role.
    let persona = persona_prompt(company_name, manifest_agent);
    let prompt_builder = SystemPromptBuilder::for_subagent(
        persona, /* omit_identity */ true, /* omit_safety_preamble */ false,
        /* omit_skills_catalog */ true,
    );

    let model = deps
        .model_override
        .clone()
        .unwrap_or_else(|| model_for_tier(manifest_agent.tier.as_deref()));

    AgentBuilder::default()
        .provider_arc(deps.provider.clone())
        .memory(Arc::new(memory))
        .tools(tools)
        .tool_dispatcher(Box::new(XmlToolDispatcher))
        .tool_policy(Arc::new(policy))
        .prompt_builder(prompt_builder)
        .model_name(model)
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

    fn manifest_agent(role: &str, description: Option<&str>) -> ManifestAgent {
        ManifestAgent {
            id: "ceo".to_string(),
            role: role.to_string(),
            description: description.map(str::to_string),
            tier: None,
            tools: Vec::new(),
            budget_usd_daily: None,
        }
    }

    #[test]
    fn persona_frames_role_company_and_description() {
        let agent = manifest_agent("Chief Executive", Some("Sets direction."));
        let persona = persona_prompt("Acme", &agent);
        assert!(persona.contains("Chief Executive"), "{persona}");
        assert!(persona.contains("Acme"), "{persona}");
        assert!(persona.contains("first person"), "{persona}");
        assert!(persona.ends_with("Sets direction."), "{persona}");
    }

    #[test]
    fn persona_omits_absent_or_blank_description() {
        let persona = persona_prompt("Acme", &manifest_agent("Engineer", Some("   ")));
        assert!(persona.contains("Engineer"));
        assert!(!persona.contains("   Engineer"));
        // No trailing description clause.
        assert!(persona.trim_end().ends_with("role."), "{persona}");
    }
}
