//! The create-time copilot's builder agent (issue #840, PR-2).
//!
//! PR-1 wired the effective-tool set; PR-2 turns the copilot into a real
//! tool-using [`Agent`](oh::agent::Agent) built fresh per request. It reuses the
//! roster's inference engine (`deps.provider`, an `Arc<dyn HarnessModel>` that
//! upcasts to the tinyagents `ChatModel<()>` the builder's native-injection seam
//! takes), the same seam [`build_agent`](crate::harness::build::build_agent)
//! uses for a company teammate — so the copilot's spend is captured as
//! backend-charged USD on the agent's own per-turn usage, not the token-only
//! total a bare tinyflows/tinyagents runner would report.
//!
//! It carries exactly the three OC-native tools in [`super::tools`] and an
//! **ephemeral** memory (nothing it does is worth persisting into the company's
//! durable memory — a create-time draft is reviewed and pressed Create, or
//! discarded). Its system prompt is the ported DSL persona
//! ([`copilot_persona`]); its tool-calling transport follows the provider's
//! advertised capability, native when supported — REQUIRED, or the model narrates
//! prose and the tools never fire.

use std::sync::Arc;

use async_trait::async_trait;
use openhuman_core::openhuman as oh;

use oh::agent::dispatcher::{NativeToolDispatcher, ToolDispatcher};
use oh::agent::prompts::SystemPromptBuilder;
use oh::agent::{Agent, AgentBuilder};
use oh::memory::{Memory, MemoryCategory, MemoryEntry, NamespaceSummary, RecallOpts};
use oh::tools::traits::Tool;

use crate::error::OpenCompanyError;
use crate::harness::HarnessDeps;
use crate::harness::build::model_for_tier;
use crate::harness::tool_dispatcher::AttrTolerantXmlDispatcher;

use super::tools::{
    AcceptedCell, CheckWorkflowTool, CopilotContext, DiagCell, ListEffectiveToolsTool,
    ProposeWorkflowTool,
};
use super::{DESCRIPTION_NODE_KINDS, graph_contract};

/// The ported DSL half of OpenHuman's builder prompt (issue #840): the workflow
/// model, the `=`/jq + envelope rules, minimal-graph guidance, honest
/// check-result interpretation, and OC's SAFETY paragraph — with OpenHuman's
/// save/create/run/OAuth/inference-readiness/ask-and-STOP machinery cut.
const COPILOT_PERSONA: &str = include_str!("copilot_prompt.md");

/// The copilot's full system prompt: the ported persona, then the shared
/// [`graph_contract`] for `DESCRIPTION_NODE_KINDS` — the SINGLE source of the
/// node-kind vocabulary + JSON schema both the prompt and the propose tool's
/// refusal read, so the two cannot drift (the drift is pinned by a test).
pub(super) fn copilot_persona() -> String {
    format!(
        "{COPILOT_PERSONA}\n\n{}",
        graph_contract(DESCRIPTION_NODE_KINDS)
    )
}

/// Builds the create-time copilot agent over the harness deps (issue #840).
///
/// Mirrors [`build_agent`](crate::harness::build::build_agent)'s native-vs-XML
/// dispatcher choice: a provider that advertises native tool calling
/// (`profile().tool_calling` — the managed hosted surface) gets openhuman's
/// [`NativeToolDispatcher`] so the turn sends structured `tools` and reads
/// `tool_calls` back; anything else keeps the prompt-guided XML fallback. Native
/// is REQUIRED for the copilot to work — without it the model narrates a proposal
/// as prose and never calls [`ProposeWorkflowTool`], so no proposal is ever
/// accepted.
///
/// The tool-iteration cap is set AFTER construction (per the setter's contract)
/// to a small budget: list → check → propose, with room for one correction
/// round.
pub(super) fn build_copilot_agent(
    deps: &HarnessDeps,
    ctx: Arc<CopilotContext>,
    accepted: AcceptedCell,
    diag: DiagCell,
) -> crate::Result<Agent> {
    let memory: Arc<dyn Memory> = Arc::new(EphemeralMemory);

    let tools: Vec<Box<dyn Tool>> = vec![
        Box::new(ListEffectiveToolsTool::new(ctx.clone())),
        Box::new(CheckWorkflowTool::new(ctx.clone(), diag.clone())),
        Box::new(ProposeWorkflowTool::new(ctx, accepted, diag)),
    ];

    let native_tools = deps
        .provider
        .profile()
        .map(|profile| profile.tool_calling)
        .unwrap_or(false);
    let tool_dispatcher: Box<dyn ToolDispatcher> = if native_tools {
        Box::new(NativeToolDispatcher)
    } else {
        Box::new(AttrTolerantXmlDispatcher::default())
    };

    let model_name = deps
        .model_override
        .clone()
        .unwrap_or_else(|| model_for_tier(None));

    // Scope the agent's harness bookkeeping (session/transcript/store subdirs) to
    // a stable subdir of the company's own workspace root, NOT the process CWD.
    // Without a `workspace_dir` the builder defaults to `"."`, which — in a tenant
    // container — would litter the read-mostly working directory with `sessions/`
    // / `session_raw/` / `tinyagents_store/`. `auto_save(false)` already turns off
    // transcript persistence; this makes the location tenant-scoped either way.
    // Best-effort create: a failure just leaves the harness to no-op its writes.
    let workspace = deps.workspace_root.join("workflow-copilot");
    let _ = std::fs::create_dir_all(&workspace);

    let mut agent = AgentBuilder::default()
        .chat_model(deps.provider.clone() as Arc<dyn tinyagents::harness::model::ChatModel<()>>)
        .memory(memory)
        .tools(tools)
        .tool_dispatcher(tool_dispatcher)
        .prompt_builder(SystemPromptBuilder::from_final_body(copilot_persona()))
        .model_name(model_name)
        .workspace_dir(workspace)
        .agent_definition_name("workflow:copilot")
        .auto_save(false)
        .build()
        .map_err(|e| OpenCompanyError::Harness(format!("build copilot agent: {e}")))?;

    // list → check → propose, plus a correction round: a small, hard cap so a
    // model that loops on its own tools stops rather than spending the budget.
    agent.set_max_tool_iterations(7);
    Ok(agent)
}

/// A no-op [`Memory`] for the copilot's one-shot turn (issue #840). A create-time
/// draft is reviewed and created — or discarded — so nothing it "remembers" is
/// worth writing into the company's durable memory; every method is inert. Mirrors
/// the surface [`OcMemory`](crate::harness::memory::OcMemory) implements, without
/// the store behind it.
struct EphemeralMemory;

#[async_trait]
impl Memory for EphemeralMemory {
    fn name(&self) -> &str {
        "workflow-copilot-ephemeral"
    }

    async fn store(
        &self,
        _namespace: &str,
        _key: &str,
        _content: &str,
        _category: MemoryCategory,
        _session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn recall(
        &self,
        _query: &str,
        _limit: usize,
        _opts: RecallOpts<'_>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        Ok(Vec::new())
    }

    async fn get(&self, _namespace: &str, _key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        Ok(None)
    }

    async fn list(
        &self,
        _namespace: Option<&str>,
        _category: Option<&MemoryCategory>,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        Ok(Vec::new())
    }

    async fn forget(&self, _namespace: &str, _key: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn namespace_summaries(&self) -> anyhow::Result<Vec<NamespaceSummary>> {
        Ok(Vec::new())
    }

    async fn count(&self) -> anyhow::Result<usize> {
        Ok(0)
    }

    async fn health_check(&self) -> bool {
        true
    }
}
