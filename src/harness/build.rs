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
//!     but empty in v1. Still deferred: browser automation (needs a backend)
//!     and Node/NPM exec (need a managed bootstrap).
//! * **Metered web search** (issue #238, [`search`](crate::harness::search)):
//!   `web_search` over the managed backend, the discovery half the `web` tools
//!   never had — they read a *known* URL and cannot find one. Two hard gates
//!   before it is wired: an **explicit** `search` grant (a bare `*` does not
//!   confer it, the media/composio precedent) and a managed platform
//!   credential; granted-but-uncredentialed wires nothing and warns. Every call
//!   is charged by the backend, so a per-company **daily call cap** is enforced
//!   before the request and exactly one priced `SearchCall` usage sample is
//!   recorded after it completes.
//! * **Company workspace** (issue #237, [`workspace_tools`](crate::harness::workspace_tools)):
//!   `workspace_list` / `workspace_read` over the operator's shared note tree,
//!   granted under the ordinary namespace rule (`*` confers them) and hit live
//!   per call so there is no snapshot to go stale. `workspace_write` is added
//!   only under an **explicit** `workspace` / `workspace.write` grant — a bare
//!   `*` does not confer it — and is guarded by a required compare-and-swap
//!   revision token. Unlike the file tools these are scoped by the store, not
//!   the filesystem: every call resolves through one company-scoped `tree()`
//!   read, so no host path is ever built from agent input.
//! * **Delegation is orchestrator-only.** `query_company` / `spawn_task` /
//!   `delegate_to_desk` (and the other orchestrator roster/workflow tools) are
//!   wired only when `is_orchestrator`; a **dispatched** desk/roster agent never
//!   gets them. That is the depth cap = 1 / "no re-delegation in v1" invariant
//!   (issue #178). The dispatched belt is thus a curated, metered derivative of
//!   an OpenHuman agent — the exec subset above plus intrinsic memory / file /
//!   MCP / skill tools, and nothing more. Both halves (the exact dispatched set,
//!   and the orchestrator-vs-dispatched delegation contrast) are pinned by the
//!   contract tests in this module's `tests` submodule.
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
            // The metering handle lets `composio_execute` record an `OauthCall`
            // usage sample per completed call, so the Usage view's
            // calls-by-provider chart reflects real connected-tool activity
            // (issue #152). A `None` meter simply leaves metering off.
            Some(config) => tools.extend(crate::harness::composio::composio_tools(
                config,
                crate::harness::composio::ComposioMetering {
                    company: company.clone(),
                    agent: manifest_agent.id.clone(),
                    meter: deps.meter.clone(),
                },
            )),
            None => tracing::warn!(
                company = %company,
                agent = %manifest_agent.id,
                "[build] agent explicitly grants `composio` but no per-tenant Composio token is configured; composio tools NOT wired (fail-closed)"
            ),
        }
    }

    // Metered web search (issue #238) — the discovery tool the `web` namespace
    // never had. `web_fetch` / `http_request` / `curl` read a URL the agent
    // already has; nothing could find one, while three shipped skills instruct
    // the agent to "search broadly" and cite sources. Two hard gates:
    //
    //  1. an **EXPLICIT** `search` grant (`grants_search_explicit`) — the
    //     catch-all `*` does NOT confer it, following `media` / `composio`,
    //     because each call is a priced request on the managed platform.
    //  2. a MANAGED backend credential on the deps (`deps.search`), resolved
    //     env-only by the runtime builder — never a tenant secret.
    //
    // Granted-but-uncredentialed wires nothing and warns (fail-closed), which
    // is the state the skills' degradation clause is written for.
    //
    // NOT feature-gated, unlike `media` and `composio`: it needs only the
    // always-compiled `openhuman_core` integrations client, and CI's gated lane
    // builds `--features openhuman,tinycortex`. Hiding a real-money tool behind
    // a feature no CI job compiles is how #288 / #281 / #297 each happened.
    if crate::company::grants_search_explicit(grants) {
        match &deps.search {
            Some(backend) => tools.extend(crate::harness::search::search_tools(
                backend,
                crate::harness::search::SearchMetering {
                    company: company.clone(),
                    agent: manifest_agent.id.clone(),
                    meter: deps.meter.clone(),
                },
            )),
            None => tracing::warn!(
                company = %company,
                agent = %manifest_agent.id,
                "[build] agent explicitly grants `search` but no managed search backend is configured; web_search NOT wired (fail-closed)"
            ),
        }
    }

    // Company workspace (issue #237) — live read (and optionally write) tools
    // over the operator-owned note tree, so an agent can ground an answer in
    // the company's own `Standards/` / `Playbooks/` instead of guessing. Two
    // independent gates, deliberately asymmetric:
    //
    //  1. READS follow the ordinary namespace rule, so a catch-all `*` confers
    //     them — the whole point of the issue is that shared guidance should be
    //     reachable by default.
    //  2. WRITES need an **EXPLICIT** `workspace` (or `workspace.write`) grant
    //     (`grants_workspace_write_explicit`); `*` does NOT confer them,
    //     mirroring the media/composio precedent, because a write mutates
    //     operator-owned guidance every other agent then trusts.
    //
    // Unwired-store is fail-closed: with no `deps.workspace` no tool is built
    // and the agent behaves exactly as it did before this cell. Create, rename
    // and delete stay operator-only — the write tool's required
    // `expected_updated_at` means only an existing note can be targeted.
    //
    // Not mapped in `toolbelt::namespace_of`, so these stay intrinsic to the
    // capability filter (the `file_tools` precedent): the reads are free and
    // correctness-critical, and shedding them under token-budget pressure
    // would make agents hallucinate company standards to save nothing.
    let workspace_writes = crate::company::grants_workspace_write_explicit(grants);
    let workspace_tools = match &deps.workspace {
        Some(store) if grants_cover(grants, "workspace") => {
            Some(crate::harness::workspace_tools::workspace_tools(
                store.clone(),
                company.clone(),
                workspace_writes,
            ))
        }
        _ => None,
    };
    let workspace_granted = workspace_tools.is_some();
    if let Some(workspace_tools) = workspace_tools {
        tools.extend(workspace_tools);
    }

    // Persona over openhuman's own identity: `omit_identity = true` drops the
    // "you are OpenHuman" preamble so the agent speaks as its company role.
    let mut persona = persona_prompt(company_name, manifest_agent);

    // A short, STATIC brief — never a tree snapshot. A snapshot baked into the
    // system prompt would be stale the moment the operator edits a note, which
    // is exactly what hitting the store per call avoids.
    if workspace_granted {
        persona.push_str(&crate::harness::workspace_tools::workspace_brief(
            workspace_writes,
        ));
    }

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
            &deps.skills_registry,
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

    // Tool-calling transport follows the provider's advertised capability. A
    // provider that advertises native tool calling (`profile().tool_calling`,
    // e.g. the managed hosted/tenant surface) gets openhuman's
    // [`NativeToolDispatcher`], so the harness sends structured `tools` and reads
    // `message.tool_calls` back — the reliable multi-step path. A provider that
    // does not (the offline `MockProvider`, a keyless local model with no profile)
    // keeps the prompt-guided [`AttrTolerantXmlDispatcher`] fallback. Without this
    // every turn is pinned to prompt-XML and a model that narrates prose instead
    // of the exact `<tool_call>` tag silently runs no tools (bug #1).
    use oh::agent::dispatcher::{NativeToolDispatcher, ToolDispatcher};
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

    AgentBuilder::default()
        // `HarnessModel` upcasts to the tinyagents `ChatModel<()>` the builder's
        // native injection seam takes (the old `Provider` adapter is gone).
        .chat_model(deps.provider.clone() as Arc<dyn tinyagents::harness::model::ChatModel<()>>)
        .memory(memory)
        .tools(tools)
        .tool_dispatcher(tool_dispatcher)
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

    // --- Dispatched-agent toolbelt contract (issue #188a) -------------------
    //
    // These tests PIN the tool surface a dispatched company agent receives by
    // building a real agent via `build_agent` and reading back its live
    // `tools()` list. They lock three things so a future change can neither
    // silently widen nor narrow the belt:
    //
    //   a. the EXACT set of tool names a dispatched desk agent gets (snapshot);
    //   b. delegation tools are ABSENT for a dispatched agent but PRESENT for
    //      the orchestrator (the depth-cap = 1 / "no re-delegation" invariant,
    //      issue #178 — the single most important thing to pin);
    //   c. none of the deferred families (browser / search / node / subagent-
    //      spawn / skill-exec / memory-tree / `forget`) appear in the belt.
    //
    // Compiled under the module's default `--features openhuman` config (the
    // whole `harness` module is `openhuman`-gated), so the pinned set is the
    // openhuman-only belt: the `media` (#109) and `composio` (#110) tool arms
    // are inert without their features and are never wired here. Their
    // namespace mapping is pinned separately by the `namespace_of` tests in
    // `toolbelt.rs`.

    use crate::company::Policy;
    use crate::harness::mcp_probe::McpFailureQueue;
    use crate::harness::orchestrator::{DelegationQueue, WorkflowRunnerHandle};
    use crate::harness::policy::ApprovalRequestQueue;
    use crate::harness::provider::MockProvider;
    use crate::ports::CompanyStore;
    use crate::ports::types::{
        ChunkAddr, ChunkHit, ChunkMeta, CompanyRecord, CompanySummary, ContextChunk, LedgerEntry,
    };

    /// A no-op context store — the belt tests never exercise memory, they only
    /// assert the wired tool surface.
    struct PinContext;
    #[async_trait::async_trait]
    impl crate::ports::ContextStore for PinContext {
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
        async fn search(&self, _: &CompanyId, _: &str, _: usize) -> crate::Result<Vec<ChunkHit>> {
            Ok(Vec::new())
        }
    }

    /// A no-op company store — `build_agent` only needs a handle; nothing here
    /// loads or persists.
    struct PinStore;
    #[async_trait::async_trait]
    impl CompanyStore for PinStore {
        async fn load(&self, _: &CompanyId) -> crate::Result<Option<CompanyRecord>> {
            Ok(None)
        }
        async fn save(&self, _: &CompanyRecord) -> crate::Result<()> {
            Ok(())
        }
        async fn list(&self) -> crate::Result<Vec<CompanySummary>> {
            Ok(Vec::new())
        }
        async fn append_ledger(&self, _: &CompanyId, _: LedgerEntry) -> crate::Result<()> {
            Ok(())
        }
    }

    /// Minimal `HarnessDeps` for building a single agent: offline mock provider,
    /// no-op stores, no meter/skills/mcp/media/composio, `AllowAll` capability
    /// filter (identity). Workspace lands under a caller-owned tempdir.
    fn pin_deps(workspace_root: std::path::PathBuf) -> HarnessDeps {
        HarnessDeps {
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            context: Arc::new(PinContext),
            store: Arc::new(PinStore),
            meter: None,
            workspace_root,
            model_override: None,
            tasks: None,
            artifacts: None,
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: DelegationQueue::default(),
            workflow_runner: WorkflowRunnerHandle::default(),
            mcp_failures: McpFailureQueue::default(),
            approval_requests: ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            steer: crate::company::steer::InflightRegistry::default(),
            delivery: None,
            // Fail-closed default: with no managed search backend wired, the
            // #238 tool is never built and the pinned belt below is the
            // pre-#238 belt exactly.
            search: None,
            // Fail-closed default: with no workspace store wired, the #237
            // tools are never built and the pinned belt below is the
            // pre-#237 belt exactly.
            workspace: None,
        }
    }

    /// Build one agent under `grants` and return its live tool names, sorted, so
    /// a snapshot compares byte-stably against a literal.
    fn built_tool_names(grants: &[&str], is_orchestrator: bool) -> Vec<String> {
        let dir = tempfile::tempdir().expect("tempdir");
        let deps = pin_deps(dir.path().to_path_buf());
        let manifest_agent = ManifestAgent {
            id: "desk".to_string(),
            role: "Desk Lead".to_string(),
            description: None,
            tier: None,
            tools: Vec::new(),
            budget_usd_daily: None,
        };
        let policy = ApprovalPolicy::new(&Policy::default(), None);
        let grants: Vec<String> = grants.iter().map(|g| g.to_string()).collect();
        let agent = build_agent(
            &CompanyId::new("acme"),
            "Acme",
            &manifest_agent,
            policy,
            &deps,
            &grants,
            &[],
            is_orchestrator,
        )
        .expect("agent builds");
        let mut names: Vec<String> = agent.tools().iter().map(|t| t.name().to_string()).collect();
        names.sort();
        names
    }

    /// Build one agent under `grants` with a MANAGED search backend wired, and
    /// return its live tool names. Mirrors [`built_tool_names`], differing only
    /// in `deps.search` — so the difference between the two is exactly "a
    /// credential exists", which is one of the three gate states pinned below.
    fn built_tool_names_with_search(grants: &[&str]) -> Vec<String> {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut deps = pin_deps(dir.path().to_path_buf());
        deps.search = Some(crate::harness::search::SearchBackend::new(
            "https://api.example.test".to_string(),
            crate::company::credentials::Credential::from_value("managed-platform-token"),
            crate::company::DEFAULT_SEARCH_DAILY_CALLS,
        ));
        let manifest_agent = ManifestAgent {
            id: "desk".to_string(),
            role: "Desk Lead".to_string(),
            description: None,
            tier: None,
            tools: Vec::new(),
            budget_usd_daily: None,
        };
        let policy = ApprovalPolicy::new(&Policy::default(), None);
        let grants: Vec<String> = grants.iter().map(|g| g.to_string()).collect();
        let agent = build_agent(
            &CompanyId::new("acme"),
            "Acme",
            &manifest_agent,
            policy,
            &deps,
            &grants,
            &[],
            false,
        )
        .expect("agent builds");
        let mut names: Vec<String> = agent.tools().iter().map(|t| t.name().to_string()).collect();
        names.sort();
        names
    }

    // --- Company-workspace wiring gates (issue #237) -----------------------

    /// Build one agent with a workspace store wired and return its tool names.
    /// Mirrors [`built_tool_names`], differing only in `deps.workspace`.
    fn built_tool_names_with_workspace(grants: &[&str]) -> Vec<String> {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut deps = pin_deps(dir.path().to_path_buf());
        deps.workspace = Some(Arc::new(crate::store::FsOps::new(dir.path())));
        let manifest_agent = ManifestAgent {
            id: "desk".to_string(),
            role: "Desk Lead".to_string(),
            description: None,
            tier: None,
            tools: Vec::new(),
            budget_usd_daily: None,
        };
        let policy = ApprovalPolicy::new(&Policy::default(), None);
        let grants: Vec<String> = grants.iter().map(|g| g.to_string()).collect();
        let agent = build_agent(
            &CompanyId::new("acme"),
            "Acme",
            &manifest_agent,
            policy,
            &deps,
            &grants,
            &[],
            false,
        )
        .expect("agent builds");
        let mut names: Vec<String> = agent.tools().iter().map(|t| t.name().to_string()).collect();
        names.sort();
        names
    }

    /// The three gate states of the metered `web_search` surface (issue #238),
    /// in one table.
    ///
    /// The load-bearing row is the first: a broad `*` grant does **not** wire
    /// `web_search` even with a credential present. Every call is a priced
    /// request on the managed platform, so — like `media` and `composio` — it
    /// must be opted into by name and can never ride in on the wildcard a
    /// company set for its file and shell tools.
    #[test]
    fn web_search_is_wired_only_by_explicit_grant_and_credential() {
        // `*` + credential → absent. The wildcard never confers spend.
        let wildcard = built_tool_names_with_search(&["*"]);
        assert!(
            !wildcard.contains(&"web_search".to_string()),
            "a bare `*` must NOT confer the metered search family: {wildcard:?}"
        );

        // explicit `search` + credential → present.
        let granted = built_tool_names_with_search(&["search"]);
        assert!(
            granted.contains(&"web_search".to_string()),
            "an explicit `search` grant with a credential must wire web_search: {granted:?}"
        );
        // The sub-grant form works the same way `media.*` / `composio.*` do.
        let sub_granted = built_tool_names_with_search(&["search.web"]);
        assert!(
            sub_granted.contains(&"web_search".to_string()),
            "{sub_granted:?}"
        );

        // explicit `search`, NO credential → absent, fail-closed.
        let uncredentialed = built_tool_names(&["search"], false);
        assert!(
            !uncredentialed.contains(&"web_search".to_string()),
            "a search grant with no managed credential must wire nothing: {uncredentialed:?}"
        );

        // An unrelated grant confers nothing even with a credential wired.
        let unrelated = built_tool_names_with_search(&["web.*"]);
        assert!(
            !unrelated.contains(&"web_search".to_string()),
            "an unrelated grant must not confer web_search: {unrelated:?}"
        );
    }

    /// Granting `search` must not quietly hand over anything *else*: the
    /// credentialed `["search"]` belt is the ungranted belt plus exactly one
    /// tool. A namespace that widens the belt beyond its own family is how a
    /// grant stops meaning what the operator read.
    #[test]
    fn the_search_grant_adds_exactly_one_tool() {
        let mut baseline = built_tool_names(&[], false);
        let granted = built_tool_names_with_search(&["search"]);
        baseline.push("web_search".to_string());
        baseline.sort();
        assert_eq!(granted, baseline, "the `search` grant widened the belt");
    }

    /// The four gate states of the workspace surface, in one table.
    ///
    /// The load-bearing row is the second: a broad `*` grant yields the READ
    /// tools but NOT `workspace_write`. Writes mutate operator-owned guidance
    /// every other agent then trusts, so — like `media` and `composio` — they
    /// must be opted into by name and can never ride in on a wildcard.
    #[test]
    fn workspace_tools_are_wired_by_grant_and_store_presence() {
        // No store wired → fail closed, nothing built, whatever the grant.
        let unwired = built_tool_names(&["workspace"], false);
        for tool in ["workspace_list", "workspace_read", "workspace_write"] {
            assert!(
                !unwired.contains(&tool.to_string()),
                "no store must mean no `{tool}`: {unwired:?}"
            );
        }

        // `*` → reads only. This is the whole asymmetry.
        let wildcard = built_tool_names_with_workspace(&["*"]);
        assert!(
            wildcard.contains(&"workspace_list".to_string()),
            "{wildcard:?}"
        );
        assert!(
            wildcard.contains(&"workspace_read".to_string()),
            "{wildcard:?}"
        );
        assert!(
            !wildcard.contains(&"workspace_write".to_string()),
            "a bare `*` must NOT confer workspace writes: {wildcard:?}"
        );

        // Explicit `workspace` → reads + write.
        let explicit = built_tool_names_with_workspace(&["workspace"]);
        assert!(
            explicit.contains(&"workspace_write".to_string()),
            "{explicit:?}"
        );

        // No workspace grant at all → nothing, even with a store wired.
        let ungranted = built_tool_names_with_workspace(&["web.*"]);
        for tool in ["workspace_list", "workspace_read", "workspace_write"] {
            assert!(
                !ungranted.contains(&tool.to_string()),
                "an unrelated grant must not confer `{tool}`: {ungranted:?}"
            );
        }
    }

    /// `workspace.read` is a genuinely read-only grant.
    ///
    /// A deliberate divergence from the `media` / `composio` helpers, which
    /// match any `<ns>.` prefix and would therefore let `workspace.read` confer
    /// writes — a footgun on a destructive surface.
    #[test]
    fn a_workspace_read_grant_does_not_confer_writes() {
        let read_grant = built_tool_names_with_workspace(&["workspace.read"]);
        assert!(
            read_grant.contains(&"workspace_read".to_string()),
            "{read_grant:?}"
        );
        assert!(
            !read_grant.contains(&"workspace_write".to_string()),
            "`workspace.read` must not confer writes: {read_grant:?}"
        );

        let write_grant = built_tool_names_with_workspace(&["workspace.write"]);
        assert!(
            write_grant.contains(&"workspace_write".to_string()),
            "{write_grant:?}"
        );
    }

    /// (a) The EXACT tool belt a dispatched desk agent receives with the broad
    /// `*` grant. Any tool added to or removed from a dispatched agent flips
    /// this snapshot and fails CI — the whole point of the pin. The set is the
    /// curated exec subset (shell / code / web) plus the intrinsic memory + file
    /// tools; it contains NO delegation tool and NO deferred family, and — the
    /// #238 addition — no `web_search`, because a bare `*` does not confer the
    /// `search` grant.
    ///
    /// **Feature-aware (issue #297).** The belt genuinely differs by feature
    /// set: `#[cfg(feature = "mcp")]` pushes two `mcp_registry_*` tools
    /// unconditionally in `build_agent`, so a flat literal was *wrong* under
    /// `--features openhuman,mcp,telegram` — the combination a full local build
    /// and the shipped tenant image both use, and which no CI lane ran. The pin
    /// was therefore failing unseen on `main`. Extending the array
    /// unconditionally would only move the failure onto plain
    /// `--features openhuman`, which CI *does* run, so the fix has to branch.
    /// Composing the expectation from the same `cfg` the wiring uses keeps the
    /// two from drifting again.
    #[test]
    fn dispatched_desk_agent_tool_belt_is_pinned() {
        let names = built_tool_names(&["*"], false);
        // `mut` is only used by the `mcp` arm below; without the feature the
        // literal is already the whole expectation.
        #[cfg_attr(not(feature = "mcp"), allow(unused_mut))]
        let mut expected = vec![
            "apply_patch",
            "csv_export",
            "curl",
            "edit",
            "file_read",
            "file_write",
            "git_operations",
            "glob",
            "grep",
            "http_request",
            "image_info",
            "list",
            "memory_recall",
            "memory_store",
            "read_workspace_state",
            "shell",
            "web_fetch",
        ];
        // Mirrors the unconditional `#[cfg(feature = "mcp")]` push in
        // `build_agent`. These two are intrinsic (unmapped by `namespace_of`),
        // so no grant gates them — enabling the feature is the whole condition.
        #[cfg(feature = "mcp")]
        {
            expected.push("mcp_registry_list_tools");
            expected.push("mcp_registry_tool_call");
            expected.sort();
        }
        assert_eq!(names, expected, "dispatched desk belt drifted: {names:?}");
    }

    /// (b) The depth-cap = 1 invariant (issue #178): a dispatched desk agent
    /// must NEVER receive the orchestrator's delegation tools, while the
    /// orchestrator agent MUST. Building both from the same grant and contrasting
    /// them is the registration check that a dispatched turn cannot re-delegate.
    #[test]
    fn dispatched_agent_has_no_delegation_tools_but_orchestrator_does() {
        let delegation = ["query_company", "spawn_task", "delegate_to_desk"];

        let dispatched = built_tool_names(&["*"], false);
        for tool in delegation {
            assert!(
                !dispatched.contains(&tool.to_string()),
                "dispatched desk agent must NOT receive delegation tool `{tool}`: {dispatched:?}"
            );
        }

        let orchestrator = built_tool_names(&["*"], true);
        for tool in delegation {
            assert!(
                orchestrator.contains(&tool.to_string()),
                "orchestrator agent MUST receive delegation tool `{tool}`: {orchestrator:?}"
            );
        }
    }

    /// (c) No deferred family leaks into a dispatched belt: raw browser
    /// automation, Node/NPM exec, OpenHuman sub-agent spawn tools (the
    /// `subagent` namespace is reserved but empty in v1), skill *execution*,
    /// the raw memory-tree tool surface, and `forget`. A negative assertion, so
    /// it stays honest even as OpenHuman renames tools upstream — the pin is
    /// "none of these shapes appear".
    ///
    /// **`web_search` was removed from this list by issue #238** — and only
    /// `web_search`. It was deferred for one *infrastructure* reason ("need
    /// engine keys") that the managed-credential pattern dissolved; the other
    /// families here were deferred for *safety* reasons that still hold. The
    /// remaining `search` / `google_search` entries stay pinned, so OpenHuman's
    /// broader search families cannot arrive on the back of this decision.
    ///
    /// That `web_search` is nonetheless absent from a `*`-granted belt is not
    /// this test's job any more — it is pinned deliberately by
    /// [`web_search_is_wired_only_by_explicit_grant_and_credential`] and by the
    /// exact snapshot in
    /// [`dispatched_desk_agent_tool_belt_is_pinned`], which would both fail if
    /// the wildcard ever started conferring spend.
    #[test]
    fn dispatched_belt_excludes_every_deferred_family() {
        let names = built_tool_names(&["*"], false);
        let forbidden = [
            // raw browser automation
            "browser",
            "browser_navigate",
            "browser_click",
            "browser_screenshot",
            // the search families still deferred (`web_search` is admitted
            // under an explicit `search` grant — issue #238)
            "search",
            "google_search",
            // Node / NPM exec
            "node",
            "npm",
            "run_node",
            "run_npm",
            // OpenHuman sub-agent spawn (subagent namespace reserved, empty v1)
            "spawn_subagent",
            "spawn_agent",
            "delegate_archivist",
            // skill execution
            "run_skill",
            "skill_run",
            "run_workflow",
            "await_workflow",
            // raw memory-tree tool surface
            "memory_tree",
            "memory_tree_search",
            "memory_tree_get",
            // destructive memory
            "forget",
            "memory_forget",
        ];
        for tool in forbidden {
            assert!(
                !names.contains(&tool.to_string()),
                "deferred tool `{tool}` leaked into the dispatched belt: {names:?}"
            );
        }
    }
}
