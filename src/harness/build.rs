//! Manifest `[[agent]]` → openhuman [`AgentBuilder`] wiring.
//!
//! [`build_agent`] turns one roster entry into a ready-to-run openhuman
//! [`Agent`], injecting the harness's provider, the [`OcMemory`] adapter, the
//! [`ApprovalPolicy`] tool policy, and a workspace directory.
//!
//! * **Tools**: every agent gets the intrinsic [`memory_tools`] (`memory_store`
//!   + `memory_recall`) over its own company memory. External/integration tools
//!   gated on the manifest `tools ∩ agent.tools` allow-list (`web.*`/`docs.*`
//!   etc.) still need the grant→tool bridge + a security/runtime sandbox — a
//!   tracked follow-up; the builder accepts the vector so they extend it here.
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
use oh::memory::tools::{MemoryRecallTool, MemoryStoreTool};
use oh::memory::traits::Memory;
use oh::security::SecurityPolicy;
use oh::tools::Tool;

use crate::company::Agent as ManifestAgent;
use crate::error::OpenCompanyError;
use crate::harness::HarnessDeps;
use crate::harness::memory::OcMemory;
use crate::harness::policy::ApprovalPolicy;
use crate::harness::skills::EffectiveSkills;
use crate::ports::skills_state::SkillState;
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
///
/// `skill_deltas` are the company's operator skill overrides. When the harness
/// is wired to a skills source (a [`SkillStateStore`](crate::ports::SkillStateStore)
/// and/or a source directory), the agent's effective skill set is materialized
/// and surfaced as three read tools plus a persona-prompt catalogue.
pub fn build_agent(
    company: &CompanyId,
    company_name: &str,
    manifest_agent: &ManifestAgent,
    policy: ApprovalPolicy,
    deps: &HarnessDeps,
    skill_deltas: &[SkillState],
) -> crate::Result<Agent> {
    let memory: Arc<dyn Memory> = Arc::new(OcMemory::new(
        company.clone(),
        manifest_agent.id.clone(),
        deps.context.clone(),
    ));

    let workspace = deps
        .workspace_root
        .join(company.as_ref())
        .join(&manifest_agent.id)
        .join("workspace");

    // Intrinsic memory tools: every agent can deliberately store and recall over
    // its own company memory, complementing the automatic retrieve→inject→store
    // loop. They are tenant-isolated (an agent's memory is its company's
    // `ContextStore`) and granted to every agent — unlike the manifest
    // `[tools]` allow-list, which scopes external/integration tools (the
    // grant→tool bridge for `web.*`/`docs.*` etc. is a follow-up seam).
    let mut tools: Vec<Box<dyn Tool>> = memory_tools(memory.clone());

    // Persona over openhuman's own identity: `omit_identity = true` drops the
    // "you are OpenHuman" preamble so the agent speaks as its company role.
    let mut persona = persona_prompt(company_name, manifest_agent);

    // Skill read surface (read-only catalogue slice). Only materializes when the
    // harness is wired to a skills source; otherwise the agent stays skill-less
    // and the default path is untouched. The catalogue is folded into the
    // persona body because `omit_skills_catalog` is inert upstream.
    if deps.skills_source_dir.is_some() || !skill_deltas.is_empty() {
        let skill_ws = deps
            .workspace_root
            .join(company.as_ref())
            .join(&manifest_agent.id)
            .join("skill-catalog");
        let effective = EffectiveSkills::materialize(
            skill_ws,
            deps.skills_source_dir.as_deref(),
            skill_deltas,
        )?;
        if !effective.is_empty() {
            tools.extend(effective.read_tools());
            persona.push_str(&effective.catalogue());
        }
    }

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
        .memory(memory)
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

/// The always-on memory tools every embedded agent receives: `memory_store` and
/// `memory_recall` over the agent's own [`OcMemory`]. Backed by the same
/// `ContextStore` the automatic loop and `OPENCOMPANY_MEMORY` overlay use, so
/// deliberate and automatic memory share one store.
///
/// `MemoryForgetTool` is deliberately excluded — [`OcMemory`]'s append-only
/// `ContextStore` cannot delete, so a forget tool would silently no-op.
fn memory_tools(memory: Arc<dyn Memory>) -> Vec<Box<dyn Tool>> {
    let security = Arc::new(SecurityPolicy::default());
    vec![
        Box::new(MemoryStoreTool::new(memory.clone(), security)),
        Box::new(MemoryRecallTool::new(memory)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_tools_expose_store_and_recall() {
        use crate::ports::ContextStore;
        use crate::ports::types::{ChunkAddr, ChunkHit, ChunkMeta, CompanyId, ContextChunk};

        // The memory handle is never exercised here — we only assert the tool
        // surface — so a no-op context suffices.
        struct NoopContext;
        #[async_trait::async_trait]
        impl ContextStore for NoopContext {
            async fn put(&self, _: &CompanyId, _: ContextChunk) -> crate::Result<ChunkAddr> {
                Ok(ChunkAddr::new("x"))
            }
            async fn list(&self, _: &CompanyId, _: &str) -> crate::Result<Vec<ChunkMeta>> {
                Ok(Vec::new())
            }
            async fn peek(
                &self,
                _: &CompanyId,
                _: &ChunkAddr,
                _: Option<std::ops::Range<usize>>,
            ) -> crate::Result<String> {
                Ok(String::new())
            }
            async fn search(
                &self,
                _: &CompanyId,
                _: &str,
                _: usize,
            ) -> crate::Result<Vec<ChunkHit>> {
                Ok(Vec::new())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(OcMemory::new(
            CompanyId::new("acme"),
            "ceo",
            Arc::new(NoopContext),
        ));
        let tools = memory_tools(memory);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"memory_store"), "got {names:?}");
        assert!(names.contains(&"memory_recall"), "got {names:?}");
    }

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
