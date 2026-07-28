//! Manifest `[[agent]]` → openhuman [`AgentBuilder`] wiring.
//!
//! [`build_agent`] turns one roster entry into a ready-to-run openhuman
//! [`Agent`], injecting the harness's provider, the [`OcMemory`] adapter, the
//! [`ApprovalPolicy`] tool policy, and a workspace directory.
//!
//! * **Tools**: every agent gets the intrinsic [`memory_tools`] (`memory_store`
//!   + `memory_recall`) over its own company memory. **File tools** (read,
//!     write, edit, list, grep, glob) are granted per-agent when the effective
//!     `tools ∩ agent.tools` grants cover the `files`/`docs` namespace, and are
//!     sandboxed to the agent's own workspace via a `workspace_only`
//!     [`SecurityPolicy`] ([`file_tools`]). **Exec-grade tools** (Cell A) now
//!     slot in beside them via [`toolbelt`](crate::harness::toolbelt): `shell`
//!     (shell + `read_workspace_state`) and `code` (`apply_patch`,
//!     `git_operations`, `csv_export`) behind a strict [`toolbelt::exec_security`]
//!     policy + native runtime + per-workspace audit; `web` (`web_fetch`,
//!     `http_request`, `curl`, `image_info`) behind the same policy plus a
//!     per-company SSRF domain allowlist. The `subagent` namespace is reserved
//!     but empty in v1. Still deferred: browser automation (needs a backend),
//!     search (needs engine keys), and Node/NPM exec (need a managed bootstrap).
//! * **Workflows/skills** start empty. Parsing enabled `SKILL.md` bodies via
//!   `openhuman::skills::ops_parse` depends on WS1's skill parsing; the seam is
//!   the `.workflows(...)` setter.
//!
//! The tool dispatcher is the attribute-tolerant
//! [`AttrTolerantXmlDispatcher`](crate::harness::tool_dispatcher::AttrTolerantXmlDispatcher),
//! a thin wrapper over OpenHuman's text-based `XmlToolDispatcher` that first
//! strips attributes off `tool_call`-family open tags (issue #105) so the
//! vendored bare-literal parser matches them. It needs no global tool registry —
//! the harness stays self-contained.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use openhuman_core::openhuman as oh;

use oh::agent::{Agent, AgentBuilder};
use oh::context::prompt::SystemPromptBuilder;
use oh::memory::tools::{MemoryRecallTool, MemoryStoreTool};
use oh::memory::traits::Memory;
use oh::security::SecurityPolicy;
#[cfg(feature = "mcp")]
use oh::tools::McpListToolsTool;
use oh::tools::{
    EditFileTool, FileReadTool, FileWriteTool, GlobTool, GrepTool, ListFilesTool, Tool,
};

use crate::company::Agent as ManifestAgent;
use crate::error::OpenCompanyError;
use crate::harness::HarnessDeps;
#[cfg(feature = "mcp")]
use crate::harness::mcp::{
    OcMcpCallTool, OcMcpListServersTool, capability_brief, granted_secrets, registry_for_agent,
};
use crate::harness::memory::OcMemory;
use crate::harness::orchestrator;
use crate::harness::policy::ApprovalPolicy;
use crate::harness::skills::EffectiveSkills;
use crate::harness::tool_dispatcher::AttrTolerantXmlDispatcher;
use crate::harness::toolbelt;
use crate::ports::skills_state::SkillState;
use crate::ports::types::CompanyId;

/// Map a manifest cognition-tier hint to a hosted model/tier name.
///
/// The manifest tier "never selects a model" (that is the TinyHumans backend's
/// job); this only picks the abstract hosted workload string the provider
/// resolves. Unknown / absent tiers fall back to the conversational `chat-v1`.
pub fn model_for_tier(tier: Option<&str>) -> String {
    match tier.map(|t| t.trim().to_ascii_lowercase()).as_deref() {
        // The orchestrator answers from whole-company context and drives tool
        // use (query/delegate), so it maps to the capable agentic workload.
        Some("orchestrator") => "agentic-v1",
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
///
/// `is_orchestrator` marks the company's orchestrator agent (issue #53): it
/// additionally receives the delegating-orchestrator persona brief and the
/// `query_company` / `spawn_task` / `delegate_to_desk` tools.
// Each parameter is a distinct, load-bearing dependency of agent construction;
// bundling them into a struct would only relocate the surface. (Pre-existing —
// surfaced only under the full `openhuman,mcp,telegram` clippy combo, which CI
// does not build; see the OpenCompany full-feature CI-gap note.)
#[allow(clippy::too_many_arguments)]
pub fn build_agent(
    company: &CompanyId,
    company_name: &str,
    manifest_agent: &ManifestAgent,
    policy: ApprovalPolicy,
    deps: &HarnessDeps,
    grants: &[String],
    skill_deltas: &[SkillState],
    is_orchestrator: bool,
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
    // `ContextStore`) and granted to every agent — unlike the external tools
    // below, which are scoped by the manifest `[tools]` allow-list.
    let mut tools: Vec<Box<dyn Tool>> = memory_tools(memory.clone());
    #[cfg(feature = "mcp")]
    {
        // These config-free tools read OpenHuman's live process registry, so
        // installs and lifecycle changes are visible without rebuilding agents.
        tools.push(Box::new(oh::mcp_registry::tools::McpRegistryListToolsTool));
        tools.push(Box::new(oh::mcp_registry::tools::McpRegistryToolCallTool));
    }

    // Granted file tools, sandboxed to this agent's own workspace directory. An
    // agent gets them only when its effective grants cover the `files`/`docs`
    // namespace (`docs.*`, `files.*`, or `*`). The security policy is
    // `workspace_only`, so a granted agent can read and write within its
    // workspace and nowhere else on the host.
    if grants_cover(grants, "files") || grants_cover(grants, "docs") {
        tools.extend(file_tools(&workspace));
    }

    // Exec-grade coding + web tools (Cell A), each behind its own grant
    // namespace and sandboxed to this agent's workspace by ONE strict
    // `exec_security` policy shared across the shell/code/web tool constructors.
    // Unlike the MCP bridge (which hands OpenHuman a permissive Supervised
    // policy), shell/code/web receive the strict policy directly — the company's
    // own `ApprovalPolicy` (`tool_policy` below) stays the authoritative
    // per-call park/deny gate on top of it. The autonomy tier is mapped 1:1 from
    // the manifest `[policy].mode`.
    let wants_shell = grants_cover(grants, "shell");
    let wants_code = grants_cover(grants, "code");
    let wants_web = grants_cover(grants, "web");
    if wants_shell || wants_code || wants_web {
        let exec_security = Arc::new(toolbelt::exec_security(&workspace, policy.mode()));
        // `shell` and `code` are separate grant namespaces and are wired from
        // separate tool vectors — a company granting only one MUST NOT receive
        // the other's tools (the production `CapabilityFilter` is identity and
        // does not re-trim namespaces after construction). Only the `shell`
        // tools need a host runtime + per-workspace audit logger (tenant-
        // isolated), so those handles are built only under `wants_shell`.
        if wants_shell {
            let runtime = toolbelt::native_runtime();
            // Fail closed: `workspace_audit` returns `None` if the per-workspace
            // audit logger cannot be initialized, and `shell_tools` then withholds
            // the shell namespace entirely rather than register an unaudited
            // `ShellTool`. A granted agent silently loses shell here — the
            // error-level log in `workspace_audit` surfaces why.
            let audit = toolbelt::workspace_audit(&workspace);
            tools.extend(toolbelt::shell_tools(
                exec_security.clone(),
                runtime,
                audit,
                &workspace,
            ));
        }
        if wants_code {
            tools.extend(toolbelt::code_tools(exec_security.clone(), &workspace));
        }
        // Web tools reuse OpenHuman's upstream SSRF `url_guard` internally; the
        // per-company allowlist comes from the manifest `[tools].web_allowed_domains`
        // (empty = allow-public with private/metadata IPs always rejected).
        if wants_web {
            tools.extend(toolbelt::web_tools(
                exec_security,
                deps.web_allowed_domains.clone(),
                &workspace,
            ));
        }
    }
    // The `subagent` namespace is reserved but intentionally wires no tools in
    // v1 — OpenHuman's spawn tools use a process-global registry + budget bypass
    // unsafe under multi-tenancy. Reserved so a grant can land without effect.
    if grants_cover(grants, "subagent") {
        tools.extend(toolbelt::subagent_tools());
    }

    // Media generation (issue #109) — image/video tools that spend REAL MONEY
    // (the backend charges on submit). Two hard gates before any tool is wired:
    //
    //  1. an **EXPLICIT** `media` grant (`grants_media_explicit`) — unlike every
    //     other namespace, the catch-all `*` does NOT grant media, so a company
    //     never accidentally hands its agents a paid generator via a broad
    //     wildcard; it must opt in by name.
    //  2. a MANAGED backend credential on the deps (`deps.media`), resolved
    //     env-only by the runtime builder — never a tenant secret.
    //
    // Granted-but-uncredentialed wires nothing and warns (fail-closed). The
    // generate tools additionally park for operator approval via the
    // `ApprovalPolicy`. Gated on the `media` feature; the default/`openhuman`
    // build never compiles this.
    #[cfg(feature = "media")]
    if crate::company::grants_media_explicit(grants) {
        match &deps.media {
            Some(backend) => tools.extend(toolbelt::media_tools(backend, &workspace)),
            None => tracing::warn!(
                company = %company,
                agent = %manifest_agent.id,
                "[build] agent explicitly grants `media` but no managed media backend is configured; media tools NOT wired (fail-closed)"
            ),
        }
    }

    // Per-tenant Composio (issue #110) — Gmail / Slack / GitHub over the
    // company's OAuth token. Two hard gates before any tool is wired:
    //
    //  1. an **EXPLICIT** `composio` grant (`grants_composio_explicit`) — like
    //     `media`, the catch-all `*` does NOT grant it, so a broadly-permissioned
    //     company never accidentally hands its agents a live account-reaching
    //     surface; it must opt in by name.
    //  2. a resolved per-tenant token on the deps (`deps.composio`), read from the
    //     company secret store by `HarnessPool::ensure` — never an env/platform
    //     key. The backend derives the Composio entity from THIS token, so it is
    //     the entire tenant-isolation lever.
    //
    // Granted-but-tokenless wires nothing and warns (fail-closed). The
    // `authorize` / `execute` tools additionally park for operator approval via
    // the `ApprovalPolicy`. Gated on the `composio` feature; the default/
    // `openhuman` build never compiles this.
    #[cfg(feature = "composio")]
    if crate::company::grants_composio_explicit(grants) {
        match &deps.composio {
            Some(config) => tools.extend(crate::harness::composio::composio_tools(config)),
            None => tracing::warn!(
                company = %company,
                agent = %manifest_agent.id,
                "[build] agent explicitly grants `composio` but no per-tenant Composio token is configured; composio tools NOT wired (fail-closed)"
            ),
        }
    }

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

    // MCP bridge (issue #50): if this agent is granted any enabled MCP server
    // (via its `mcp:*` tool grants), give it the three bridge tools over a
    // registry scoped to just those servers. The registry reuses OpenHuman's
    // HTTP transport + injection-safety filter. The credential-redacting
    // `OcMcpListServersTool` replaces upstream's list-servers tool (which would
    // serialize bearer tokens into agent-visible output). `mcp_call_tool` takes
    // a permissive OpenHuman `SecurityPolicy` (Supervised — allows `Act`);
    // OpenCompany's own `ApprovalPolicy` tool policy below stays the real
    // per-call gate.
    #[cfg(feature = "mcp")]
    if let Some(registry) = registry_for_agent(&deps.mcp_servers, grants) {
        let mcp_security = Arc::new(SecurityPolicy::default());
        // The known-secret set for the scrubber: every credential the agent's
        // granted servers carry, so no configured token can leak into an
        // agent-visible MCP error (the error-hardening cell).
        let secrets = granted_secrets(&deps.mcp_servers, manifest_agent);
        tools.push(Box::new(OcMcpListServersTool::new(registry.clone())));
        tools.push(Box::new(McpListToolsTool::new(registry.clone())));
        // `OcMcpCallTool` replaces upstream's `McpCallTool`: same name/schema,
        // but it classifies + scrubs failures, rewrites the agent-facing text,
        // and records each failure on the shared queue the brain drains.
        tools.push(Box::new(OcMcpCallTool::new(
            registry,
            mcp_security,
            secrets,
            deps.mcp_failures.clone(),
        )));
        // Stale-memory mitigation: direct the agent to answer capability
        // questions from a live `mcp_list_servers` call, never from memory.
        persona.push_str(&capability_brief());
    }

    // Orchestrator seam (issues #53 + #67 + #71): the company's orchestrator agent
    // additionally gets the delegating-orchestrator persona + tools. `query_company`
    // reads the company's facts + recent events; `spawn_task` / `delegate_to_desk`
    // push onto the shared delegation queue the brain drains after the turn;
    // `run_workflow` executes one of the company's saved workflows by id through
    // the shared runner handle (so a task waiting on a workflow can be run to
    // completion); `add_agent` lets the orchestrator bring on a new teammate
    // mid-chat. Additive beside the MCP block above.
    if is_orchestrator {
        persona.push_str(&orchestrator::orchestrator_brief());
        tools.extend(orchestrator::orchestrator_tools(
            company.clone(),
            deps.facts.clone(),
            deps.events.clone(),
            &deps.delegations,
            // The company source dir (`companies/<name>`) also houses `workflows/`,
            // which the `run_workflow` tool loads graphs from.
            deps.skills_source_dir.clone(),
            deps.workflow_runner.clone(),
            // The company store, for the `add_agent` tool to persist overlay
            // teammates through the same path the console `POST .../team` uses.
            deps.store.clone(),
        ));
    }

    let prompt_builder = SystemPromptBuilder::for_subagent(
        persona, /* omit_identity */ true, /* omit_safety_preamble */ false,
        /* omit_skills_catalog */ true,
    );

    let model = deps
        .model_override
        .clone()
        .unwrap_or_else(|| model_for_tier(manifest_agent.tier.as_deref()));

    // Capability-tier seam (Cell A): one filtering pass over the fully assembled
    // tool vector, just before it is handed to the builder. Today `AllowAll` is
    // the only production variant (identity); a future capability-tier cell only
    // swaps how `deps.capabilities` is constructed. Intrinsic tools
    // (memory/MCP/orchestrator/file/skill) have no mapped namespace and are
    // always kept.
    let tools = toolbelt::filter_by_capabilities(tools, &deps.capabilities);

    AgentBuilder::default()
        // `HarnessModel` upcasts to the tinyagents `ChatModel<()>` the builder's
        // native injection seam takes (the old `Provider` adapter is gone).
        .chat_model(deps.provider.clone() as Arc<dyn tinyagents::harness::model::ChatModel<()>>)
        .memory(memory)
        .tools(tools)
        .tool_dispatcher(Box::new(AttrTolerantXmlDispatcher::default()))
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

/// Whether an agent's effective `grants` cover a tool `namespace`.
///
/// Matches the bare namespace (`docs`), any glob under it (`docs.*`,
/// `docs.read`), or the catch-all `*`. Shared with the workflow toolbelt
/// ([`crate::workflows::caps`]) so a workflow `tool_call` is gated by the same
/// namespace-grant rule an agent's exec tools are.
pub(crate) fn grants_cover(grants: &[String], namespace: &str) -> bool {
    grants.iter().any(|grant| {
        grant == "*" || grant == namespace || grant.starts_with(&format!("{namespace}."))
    })
}

/// A [`SecurityPolicy`] that sandboxes an agent's file tools to `workspace` and
/// nowhere else: `workspace_only` with both the workspace and the tool action
/// root pinned to the agent's own directory.
fn workspace_security(workspace: &Path) -> SecurityPolicy {
    let dir: PathBuf = workspace.to_path_buf();
    SecurityPolicy {
        workspace_dir: dir.clone(),
        action_dir: dir,
        workspace_only: true,
        ..SecurityPolicy::default()
    }
}

/// The file tools granted under the `files`/`docs` namespace, each sandboxed to
/// the agent's `workspace` by a shared [`workspace_security`] policy: read,
/// write, edit, list, grep, and glob within the workspace only.
fn file_tools(workspace: &Path) -> Vec<Box<dyn Tool>> {
    let security = Arc::new(workspace_security(workspace));
    vec![
        Box::new(FileReadTool::new(security.clone())),
        Box::new(FileWriteTool::new(security.clone())),
        Box::new(EditFileTool::new(security.clone())),
        Box::new(ListFilesTool::new(security.clone())),
        Box::new(GrepTool::new(security.clone())),
        Box::new(GlobTool::new(security)),
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
    fn grants_cover_matches_namespace_glob_and_star() {
        assert!(grants_cover(&["docs.*".into()], "docs"));
        assert!(grants_cover(&["docs".into()], "docs"));
        assert!(grants_cover(&["docs.read".into()], "docs"));
        assert!(grants_cover(&["*".into()], "docs"));
        assert!(!grants_cover(&["web.*".into()], "docs"));
        assert!(!grants_cover(&[], "docs"));
        // A prefix must end on a namespace boundary, not a substring.
        assert!(!grants_cover(&["documentation.*".into()], "docs"));
    }

    #[test]
    fn file_tools_are_sandboxed_to_the_workspace() {
        let ws = Path::new("/tmp/agent-ws");
        let policy = workspace_security(ws);
        assert!(policy.workspace_only, "file tools must be workspace-only");
        assert_eq!(policy.workspace_dir, ws);
        assert_eq!(policy.action_dir, ws);

        let tools = file_tools(ws);
        assert_eq!(tools.len(), 6, "read/write/edit/list/grep/glob");
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"file_read"), "got {names:?}");
        assert!(names.contains(&"file_write"), "got {names:?}");
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
