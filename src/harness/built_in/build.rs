//! Manifest `[[agent]]` → openhuman [`AgentBuilder`] wiring.
//!
//! [`build_agent`] turns one roster entry into a ready-to-run openhuman
//! [`Agent`], injecting the harness's provider, the [`OcMemory`] adapter, the
//! [`ApprovalPolicy`] tool policy, and a workspace directory.
//!
//! * **Tools**: [`memory_tools`] (`memory_store` + `memory_recall`) is called
//!   but currently returns nothing — see its doc comment for why openhuman's
//!   API no longer gives embedders a way to point either tool at a company's
//!   own memory. **File tools** (read, write, edit, list, grep, glob) are
//!   granted per-agent when the effective `tools ∩ agent.tools` grants cover
//!   the `files`/`docs` namespace, and are sandboxed to the agent's own
//!   workspace via a `workspace_only` [`SecurityPolicy`] ([`file_tools`]).
//!   **Exec-grade tools** (Cell A) now slot in beside them via
//!   [`toolbelt`](crate::harness::toolbelt): `shell` (shell +
//!   `read_workspace_state`) and `code` (`apply_patch`, `git_operations`,
//!   `csv_export`) behind a strict [`toolbelt::exec_security`] policy + native
//!   runtime + per-workspace audit; `web` (`web_fetch`, `http_request`,
//!   `curl`, `image_info`) behind the same policy plus a per-company SSRF
//!   domain allowlist. The `subagent` namespace is reserved but empty in v1.
//!   Still deferred: browser automation (needs a backend) and Node/NPM exec
//!   (need a managed bootstrap).
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
//!   `workspace_list` / `workspace_search` / `workspace_read` over the
//!   operator's shared note tree, granted under the ordinary namespace rule
//!   (`*` confers all three) and hit live per call so there is no snapshot to
//!   go stale. `workspace_search` (issue #607) rides this READ grant and not
//!   the metered `search` one: it reads exactly what `workspace_read` already
//!   grants, so requiring a billed backend credential for it would price the
//!   cheap discovery path above the list-then-read crawl it replaces.
//!   `workspace_create` / `workspace_write` and, since issue #671,
//!   `workspace_rename` / `workspace_delete` are added only under an
//!   **explicit** `workspace` / `workspace.write` grant — a bare `*` does not
//!   confer them. `workspace_write` and `workspace_delete` are each guarded by
//!   a required compare-and-swap revision token, and the lifecycle pair reaches
//!   only `agents/<agent id>/`. Unlike the file tools these are scoped by the
//!   store, not the filesystem: every call resolves through one company-scoped
//!   `tree()` read, so no host path is ever built from agent input.
//! * **Delegation authority is orchestrator-only; delegation itself is
//!   opt-in per member.** `query_company`, `run_workflow`, `create_workflow`,
//!   `add_agent`, `assign_task` and `review_task` are wired only when
//!   `is_orchestrator` — they are the company's *authority* (who owns a card,
//!   what passes review, who is on the roster) and no desk agent gets them.
//!
//!   The two **hand-off** tools, `spawn_task` and `delegate_to_desk`, are also
//!   wired onto a desk agent whose manifest entry names a
//!   [`delegates_to`](crate::company::Agent::delegates_to) allowlist (issue
//!   #176), narrowed to those desks. A member that names none — every agent of
//!   every manifest written before this — carries no delegation tool at all,
//!   which is #178's original depth cap = 1 invariant, now the default rather
//!   than the only possibility.
//!
//!   Recursion is bounded **dynamically**, not by which tools were wired: belts
//!   are cached per roster and rebuilt rarely, so the tool cannot be withheld
//!   from the one turn that happens to be running too deep. `[tools]
//!   .max_delegation_depth` is enforced at the tool boundary by
//!   [`DelegationQueue::push_within_cap`](crate::harness::orchestrator::DelegationQueue::push_within_cap)
//!   against the live scope chain, and a hand-off that would loop or leave the
//!   member's allowlist is refused there too.
//!
//!   The dispatched belt is otherwise a curated, metered derivative of an
//!   OpenHuman agent — the exec subset above plus intrinsic memory / file / MCP
//!   / skill tools, and nothing more. All three halves (the exact dispatched
//!   set with and without an allowlist, and the orchestrator-vs-member
//!   authority contrast) are pinned by the contract tests in this module's
//!   `tests` submodule.
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

use oh::agent::prompts::SystemPromptBuilder;
use oh::agent::{Agent, AgentBuilder};
use oh::memory::Memory;
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
use crate::runtime::tools::{NAMESPACE_SEPARATORS, extends_on_boundary};

/// The per-tool-result byte budget every OpenCompany agent runs under.
///
/// The harness cuts **every** tool result to this many bytes on its way into
/// the model's context — `ToolOutputMiddleware`, fed from
/// `ContextManager::tool_result_budget_bytes`, which [`build_agent`] threads in
/// below via [`AgentBuilder::context_config`]. It is the real ceiling on what a
/// model ever sees from a tool, and it is *smaller* than the caps individual
/// tools tend to write for themselves.
///
/// It is stated here rather than inherited on purpose (issue #417). Before this
/// constant existed `build_agent` never called `context_config`, so the builder
/// fell back to `ContextConfig::default()` and the 16 KiB became OpenCompany's
/// effective bound by accident — invisible from this crate, and out of step
/// with a tool that had capped itself at 64 KiB. `workspace_read` consequently
/// told the model "nothing was dropped, you may write the complete body back"
/// about a note the model had only seen the first 16 KiB of, and the resulting
/// `workspace_write` silently destroyed the rest.
///
/// So any tool whose result carries a *contract* — "this is the whole thing",
/// "act on the trailer below" — must size itself against this number, not
/// against a cap of its own choosing. [`workspace_tools`] derives its read and
/// write caps from it directly.
///
/// The value still tracks the vendored default: keeping the number identical
/// makes adopting it a no-op today, so the behaviour change belongs to the
/// consumers that now respect it, not to this line.
///
/// [`workspace_tools`]: crate::harness::workspace_tools
pub(crate) const TOOL_RESULT_BUDGET_BYTES: usize =
    oh::agent::context::DEFAULT_TOOL_RESULT_BUDGET_BYTES;

/// How many tool-calling iterations one OpenCompany turn may spend (issue #988).
///
/// Stated, not inherited. Omitting the [`Agent::set_max_tool_iterations`] call
/// leaves every company agent on `AgentConfig::default()`'s **10**, which is a
/// summariser's budget: a product manager asked for a feature spec reads the
/// standards, reads the release checklist, reads the nearest prior spec, drafts,
/// and publishes — and spends the ten before delivering anything. The turn then
/// pauses at the cap and the operator gets a checkpoint digest instead of the
/// work (issue #926).
///
/// Twenty-five is ~2.5x headroom over that observed multi-read/draft/publish
/// shape, without the 5x of jumping to openhuman's `Extended` 50. Cost grows
/// **faster** than the multiplier, because every iteration re-sends a transcript
/// that is longer than the last one's — so the number is deliberately the
/// smallest one that covers the shape rather than the largest one that is safe.
/// Revisit it from cap-rate instrumentation across templates, not from one
/// incident.
///
/// Applied **globally**, in [`build_agent`], rather than per template: it is the
/// only shape that reaches all 22 shipped templates without editing each one.
///
/// The lever is `set_max_tool_iterations` and nothing else. openhuman's
/// `IterationPolicy::Extended` and the `AgentDefinition` `max_iterations` field
/// are read only by `build_session_agent_inner`, a construction path this crate
/// never takes — setting either here would compile, read as a fix, and change
/// nothing.
///
/// An in-turn spend brake ([`BudgetStopHook`](oh::agent::stop_hooks::BudgetStopHook))
/// is installed on the turn itself, but only for a teammate who declares a
/// `budget_usd_daily` cap — see [`CompanyAgent::turn_spend_cap_usd`]
/// (crate::harness::CompanyAgent::turn_spend_cap_usd).
pub const MAX_TOOL_ITERATIONS: usize = 25;

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
        // A code-writing tier (the global `page_builder` agent): a capable,
        // tool-using model, not the conversational default. `frontend` is a
        // manifest-tier value (see `company::types::TIERS`), so it must be
        // mapped here rather than relying on the `chat-v1` fallback.
        Some("frontend") => "agentic-v1",
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
///
/// Delegates to [`crate::company::prompt::persona_prompt`], which is compiled in
/// every build. Kept as a re-export rather than inlined at the call sites so the
/// harness's existing callers and tests keep one name for "the persona", and so
/// the composition rules (including the operator's inline `prompt`) are exercised
/// by the default-build test suite rather than only where this module links.
pub fn persona_prompt(
    company_name: &str,
    agent: &ManifestAgent,
    instructions: Option<&str>,
) -> String {
    crate::company::prompt::persona_prompt(company_name, agent, instructions)
}

/// The `(files, shell, code)` flags [`toolbelt::sandbox_brief`] renders from,
/// each true only when the namespace both (a) was wired from the agent's
/// GRANT (`wants_files`/`shell_wired`/`wants_code`) and (b) is not denied by
/// the per-turn capability tier in `capabilities`.
///
/// Pulled out of [`build_agent`] as a pure function so the capability-denial
/// case — the brief must not describe `shell`/`code` on a turn where
/// `filter_by_capabilities` is about to strip them — is unit-testable without
/// standing up a full agent build.
fn sandbox_brief_flags(
    wants_files: bool,
    shell_wired: bool,
    wants_code: bool,
    capabilities: &toolbelt::CapabilityFilter,
) -> (bool, bool, bool) {
    let shell = shell_wired && !toolbelt::namespace_denied(capabilities, "shell");
    let code = wants_code && !toolbelt::namespace_denied(capabilities, "code");
    (wants_files, shell, code)
}

/// Build one openhuman [`Agent`] for `manifest_agent` within `company`.
///
/// `skill_deltas` are the company's operator skill overrides. When the harness
/// is wired to a skills source (a [`SkillStateStore`](crate::ports::SkillStateStore)
/// and/or a source directory), the agent's effective skill set is materialized
/// and surfaced as three read tools plus a persona-prompt catalogue.
///
/// `routed_context` are this agent's workspace documents, already selected by
/// [`context_routing`](crate::company::context_routing) and read out of the
/// store by the async caller. Passed in rather than fetched here for the same
/// reason `skill_deltas` is: this function is synchronous and runs on every
/// roster rebuild, while the `WorkspaceStore` is async.
///
/// `instructions` are this agent's **effective** persona text (issue #1530),
/// resolved by the caller through
/// [`CompanyRecord::effective_instructions`](crate::ports::types::CompanyRecord::effective_instructions)
/// — an operator override when one is set, else the manifest `prompt`, else
/// `None`. Passed in rather than read off `manifest_agent.prompt` so an overlay
/// teammate (which has no manifest `prompt`) and a console-edited manifest agent
/// are framed through the one injection point.
///
/// `is_orchestrator` marks the company's orchestrator agent (issue #53): it
/// additionally receives the delegating-orchestrator persona brief and the
/// `query_company` / `spawn_task` / `delegate_to_desk` tools.
// Each parameter is a distinct, load-bearing dependency of agent construction;
// bundling them into a struct would only relocate the surface. (Pre-existing —
// surfaced only under the full `openhuman,mcp` clippy combo, which CI
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
    routed_context: &[(String, String)],
    instructions: Option<&str>,
    is_orchestrator: bool,
) -> crate::Result<Agent> {
    let memory: Arc<dyn Memory> = Arc::new(OcMemory::new(
        company.clone(),
        manifest_agent.id.clone(),
        deps.context.clone(),
    ));

    // Create the sandbox now, before any tool — or any `SecurityPolicy` — is
    // bound to it. See [`ensure_agent_workspace`] for why an absent directory
    // breaks relative writes outright, and why creating it late is not the same
    // as creating it here.
    //
    // Best-effort: a failure here is logged, not fatal. The tools then behave
    // exactly as they did before, the dispatch path gets a second attempt just
    // before the agent acts, and the agent is still perfectly able to run a turn
    // that touches no files. The message names the real condition — a directory
    // that could not be created — rather than leaving the operator with the
    // guard's traversal wording as the only clue.
    let workspace = match ensure_agent_workspace(&deps.workspace_root, company, &manifest_agent.id)
    {
        Ok(workspace) => workspace,
        Err(err) => {
            let workspace = agent_workspace(&deps.workspace_root, company, &manifest_agent.id);
            tracing::warn!(
                company = %company,
                agent = %manifest_agent.id,
                workspace = %workspace.display(),
                error = %err,
                "[build] could not create the agent workspace; file tools will refuse relative paths"
            );
            workspace
        }
    };

    // Deliberate-memory tools, oc-authored over this company's own context
    // port — see `memory_tools`'s doc comment for why not the vendored ones.
    let mut tools: Vec<Box<dyn Tool>> = memory_tools(deps, company, &manifest_agent.id);
    // Approvals are an explicit agent action, not a policy side effect. Every
    // roster agent gets this intrinsic tool regardless of external grants.
    tools.push(Box::new(
        crate::harness::approval_tool::RequestApprovalTool::new(
            manifest_agent.id.clone(),
            deps.approval_requests.clone(),
        ),
    ));
    #[cfg(feature = "mcp")]
    {
        // These read the installed-server registry, so installs and lifecycle
        // changes are visible without rebuilding agents. They take the config
        // that selects the store now rather than reading a process global —
        // hand them this company's own, the one REST writes through.
        if let Some(mcp_home) = deps.mcp_home.clone() {
            let config = std::sync::Arc::new(crate::harness::mcp::McpRuntime::config_for(mcp_home));
            tools.push(Box::new(
                oh::mcp::registry::tools::McpRegistryListToolsTool::new(config.clone()),
            ));
            tools.push(Box::new(
                oh::mcp::registry::tools::McpRegistryToolCallTool::new(config),
            ));
        }
    }

    // Granted file tools, sandboxed to this agent's own workspace directory. An
    // agent gets them only when its effective grants cover the `files`/`docs`
    // namespace (`docs.*`, `files.*`, or `*`). The security policy is
    // `workspace_only`, so a granted agent can read and write within its
    // workspace and nowhere else on the host.
    //
    // Issue #1192: a *caller* of the shared predicate, not a second spelling of
    // it. The console's capability panel has to answer the same question — is
    // publishing on for this company — and the way that panel comes to report a
    // capability the toolbelt does not wire is a second derivation drifting from
    // this one (issue #886's whole subject). This gate is also what decides
    // whether the file belt itself is offered, two lines down, so a predicate
    // that is *nearly* identical would silently grant or revoke file tools as
    // well as publishing.
    let wants_files = crate::company::grants_files_or_docs(grants);
    if wants_files {
        tools.extend(file_tools(&workspace));
    }

    // `publish_artifact` (issue #244) — the only way a file the agent wrote
    // becomes a deliverable. Two gates, and it is wired only when both hold:
    //
    //  1. the **same** `files`/`docs` grant the file tools ride on. An agent
    //     that cannot write a file has nothing to publish, and offering it the
    //     tool would only buy a confusing refusal mid-turn.
    //  2. a configured **artifact store** (`deps.artifacts`). This is the
    //     fail-closed half, following the `media` precedent: a tool that stages
    //     into a queue nothing will ever drain looks like it worked, tells the
    //     agent its deliverable is safe, and drops it. Better to not offer it
    //     and say why in the log.
    //
    // Unlike `media` the grant is the ordinary namespace rule (a bare `*`
    // confers it): publishing spends nothing and reaches nothing outside the
    // company's own board.
    // Issue #1861: every agent can ask. Unconditional and ungated — a question
    // is not a capability, it is the alternative to guessing, and an agent
    // narrow enough to have no other tools is the one most likely to need it.
    //
    // Safe to wire everywhere because both drains exist: a chat or task turn
    // parks what this stages through `park_approval_requests`, a workflow agent
    // node through `park_gated_calls`. There is no belt on which the question
    // would stage into a queue nothing empties — the `media` failure mode the
    // publish gate below guards against.
    tools.push(Box::new(
        crate::harness::built_in::blockers::EscalateToHumanTool::new(
            deps.approval_requests.clone(),
            manifest_agent.id.clone(),
        ),
    ));

    let publishing = wants_files && deps.artifacts.is_some();
    if publishing {
        tools.push(Box::new(crate::harness::publish::PublishArtifactTool::new(
            workspace.clone(),
            manifest_agent.id.clone(),
            deps.pending_publishes.clone(),
        )));
    } else if wants_files {
        tracing::warn!(
            company = %company,
            agent = %manifest_agent.id,
            "[build] agent is granted file tools but no artifact store is configured; \
             `publish_artifact` NOT wired (fail-closed) — this agent's files cannot become \
             deliverables"
        );
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
    // The GRANT says shell was asked for; this says it was actually wired.
    // `shell_tools` withholds the whole namespace when the audit logger cannot
    // be initialized (below), and the sandbox brief must describe the belt the
    // agent holds rather than the one it requested — otherwise the one company
    // whose audit sink is unwritable is also the one whose agents are told to
    // run commands with a tool that is not there.
    let mut shell_wired = false;
    if wants_shell || wants_code || wants_web {
        let exec_security = Arc::new(toolbelt::exec_security(&workspace, policy.toolbelt_mode()));
        // `shell` and `code` are separate grant namespaces and are wired from
        // separate tool vectors — a company granting only one MUST NOT receive
        // the other's tools (the production `CapabilityFilter` is identity and
        // does not re-trim namespaces after construction). Only the `shell`
        // tools need a host runtime + per-workspace audit logger (tenant-
        // isolated), so those handles are built only under `wants_shell`.
        if wants_shell {
            let runtime = toolbelt::native_runtime();
            // Fail closed: `shell_audit` returns `None` if the per-agent audit
            // logger cannot be initialized, and `shell_tools` then withholds
            // the shell namespace entirely rather than register an unaudited
            // `ShellTool`. A granted agent silently loses shell here — the
            // error-level log in `shell_audit` surfaces why.
            //
            // The sink is HOST-owned and lives outside the workspace (issue
            // #775): `companies/<slug>/audit/<agent>/`, resolved from the
            // explicitly-threaded `audit_root` rather than from the workspace's
            // parent. Inside the workspace it was a policy-permitted write
            // target for the agent's own file tools.
            let audit = toolbelt::shell_audit(&agent_audit_dir(
                &deps.audit_root,
                company,
                &manifest_agent.id,
            ));
            let shell = toolbelt::shell_tools(exec_security.clone(), runtime, audit, &workspace);
            shell_wired = !shell.is_empty();
            tools.extend(shell);
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
    //  2. a resolved credential on the deps (`deps.composio`), produced by
    //     `HarnessPool::ensure` through `composio::resolve_credential` — the BYO
    //     `composio/token` override, else the company's own TinyHumans key, else
    //     this instance's platform identity (issue #586). The backend derives the
    //     Composio entity from whichever tier answered, so this resolution is the
    //     entire tenant-isolation lever. It is NOT "a stored token": on a hosted
    //     tenant nobody pastes one and the platform identity is what wires the
    //     tools (issue #886).
    //
    // Granted-but-credential-less wires nothing and warns (fail-closed). The
    // `authorize` / `execute` tools additionally park for operator approval via
    // the `ApprovalPolicy`. Gated on the `composio` feature; the default/
    // `openhuman` build never compiles this.
    // Issue #1759: the connected toolkits to name in the capability-grounding +
    // Composio-routing brief, captured HERE — where the tools are actually wired
    // — and rendered into the persona further down. `Some` only when the tools
    // land on the belt (grant + resolved credential), so the brief can never
    // advertise a Composio surface this agent does not hold.
    #[cfg(feature = "composio")]
    let mut composio_toolkits: Option<Vec<String>> = None;
    #[cfg(feature = "composio")]
    if crate::company::grants_composio_explicit(grants) {
        match &deps.composio {
            // The metering handle lets `composio_execute` record an `OauthCall`
            // usage sample per completed call, so the Usage view's
            // calls-by-provider chart reflects real connected-tool activity
            // (issue #152). A `None` meter simply leaves metering off.
            Some(config) => {
                composio_toolkits = Some(config.toolkits.clone());
                tools.extend(crate::harness::composio::composio_tools(
                    config,
                    crate::harness::composio::ComposioMetering {
                        company: company.clone(),
                        agent: manifest_agent.id.clone(),
                        meter: deps.meter.clone(),
                    },
                ));
            }
            None => tracing::warn!(
                company = %company,
                agent = %manifest_agent.id,
                // Issue #886: the gate is `deps.composio.is_none()`, which is a
                // *resolver* outcome over three tiers (BYO `composio/token`,
                // the company's TinyHumans key, this instance's platform
                // identity) — not "no token is stored". Naming the stored token
                // sent operators to paste one they did not need.
                "[build] agent explicitly grants `composio` but no Composio credential could be resolved for this company; composio tools NOT wired (fail-closed)"
            ),
        }
    }

    // Issue #788: Chargebee billing. Same fail-closed shape as `composio`
    // above, and for a sharper reason — these tools send invoices to a real
    // business's real customers. Two conditions, both required:
    //
    //  1. an **EXPLICIT** `chargebee` grant. The catch-all `*` does NOT confer
    //     it, following the media/composio/search precedent.
    //  2. a resolved per-company connection on the deps (`deps.chargebee`),
    //     read from THAT company's secret store by the runtime builder.
    //
    // A grant with no credential wires nothing and warns: an agent told it can
    // bill, that silently cannot, is better than one billing through somebody
    // else's Chargebee site.
    #[cfg(feature = "chargebee")]
    if crate::company::grants_chargebee_explicit(grants) {
        match &deps.chargebee {
            Some(config) => {
                tools.extend(crate::harness::chargebee::chargebee_tools(config));
            }
            None => tracing::warn!(
                company = %company,
                agent = %manifest_agent.id,
                "[build] agent explicitly grants `chargebee` but no per-company Chargebee \
                 credentials are configured; billing tools NOT wired (fail-closed)"
            ),
        }
    }

    // Issue #789: PayPal wallet reads. Same fail-closed shape as `chargebee`
    // above — an explicit `paypal` grant AND a resolved per-company credential.
    // Both tools are read-only, so nothing here can move money; the grant is
    // still opt-in by name because a wallet balance is a business's private
    // figure, not something a `*` wildcard should hand out.
    #[cfg(feature = "paypal")]
    if crate::company::grants_paypal_explicit(grants) {
        match &deps.paypal {
            Some(config) => tools.extend(crate::harness::paypal::paypal_tools(config)),
            None => tracing::warn!(
                company = %company,
                agent = %manifest_agent.id,
                "[build] agent explicitly grants `paypal` but no per-company PayPal credentials \
                 are configured; wallet tools NOT wired (fail-closed)"
            ),
        }
    }

    // Hosting: put this agent's workspace on a real hosting provider. Same
    // fail-closed shape as `chargebee` and `paypal` above, and for both of their
    // reasons at once — a deployment publishes the company's files to the public
    // internet under its own account, and provisioning a database is a bill it
    // pays. Two conditions, both required:
    //
    //  1. an **EXPLICIT** `hosting` grant. The catch-all `*` does NOT confer it.
    //  2. a resolved per-company connection on the deps (`deps.hosting`), read
    //     from THAT company's secret store by the runtime builder — never from
    //     an environment variable, which under multi-tenancy could only ever be
    //     somebody else's account.
    //
    // The tools are pinned to `workspace`, the same sandbox directory the file
    // tools get, so an agent can only ever deploy what it can already read.
    #[cfg(feature = "openhuman")]
    if crate::company::grants_hosting_explicit(grants) {
        match &deps.hosting {
            Some(config) => {
                tools.extend(crate::harness::hosting::hosting_tools(
                    config,
                    workspace.clone(),
                ));
            }
            None => tracing::warn!(
                company = %company,
                agent = %manifest_agent.id,
                "[build] agent explicitly grants `hosting` but no per-company hosting \
                 credential is configured; hosting tools NOT wired (fail-closed)"
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
    // builds `--features openhuman,tinymemory`. Hiding a real-money tool behind
    // a feature no CI job compiles is how #288 / #281 / #297 each happened.
    //
    // A company that configured its **own** provider in the console
    // (`deps.tenant_search`) gets that provider's family from OpenHuman's search
    // domain INSTEAD of the managed tool — never as well as. The two would
    // otherwise sit on one belt under one name, and the model would pick
    // whichever the prompt happened to mention, which for a company that pasted
    // a key means quietly spending the platform's money instead of its own. A
    // BYO belt carries no daily cap and no usage sample either: the calls are
    // billed by Brave or Exa to the company's own account, and metering a bill
    // this host does not pay would be a number nobody can reconcile.
    if crate::company::grants_search_explicit(grants) {
        match (&deps.tenant_search, &deps.search) {
            (Some(tenant), _) => {
                let byo = crate::harness::search_byo::byo_search_tools(tenant);
                tracing::debug!(
                    company = %company,
                    agent = %manifest_agent.id,
                    provider = %tenant.provider(),
                    tools = byo.len(),
                    "[build] wiring the company's own search provider in place of managed search"
                );
                tools.extend(byo);
            }
            (None, Some(backend)) => tools.extend(crate::harness::search::search_tools(
                backend,
                crate::harness::search::SearchMetering {
                    company: company.clone(),
                    agent: manifest_agent.id.clone(),
                    meter: deps.meter.clone(),
                },
            )),
            (None, None) => tracing::warn!(
                company = %company,
                agent = %manifest_agent.id,
                "[build] agent explicitly grants `search` but neither a company search provider nor a managed search backend is configured; web_search NOT wired (fail-closed)"
            ),
        }
    }

    // Company workspace (issues #237, #551) — live read (and optionally
    // create/write) tools over the shared note tree, so an agent can ground an
    // answer in the company's own `standards/` / `playbooks/` instead of
    // guessing, and can put what it produces somewhere the operator and its
    // teammates will actually find it. Two independent gates, deliberately
    // asymmetric:
    //
    //  1. READS follow the ordinary namespace rule, so a catch-all `*` confers
    //     them — the whole point of #237 is that shared guidance should be
    //     reachable by default.
    //  2. CREATE + WRITE + RENAME + DELETE need an **EXPLICIT** `workspace`
    //     (or `workspace.write`) grant (`grants_workspace_write_explicit`); `*`
    //     does NOT confer them, mirroring the media/composio precedent, because
    //     they mutate a tree every other agent then trusts. All four ride the
    //     one flag: overwriting an existing standard is strictly more
    //     destructive than adding a note beside it, and strictly more
    //     destructive than removing or moving something inside the agent's own
    //     folder — so a grant that permits the first has already permitted the
    //     rest, and issue #671 deliberately added no fifth grant name.
    //
    // Unwired-store is fail-closed: with no `deps.workspace` no tool is built
    // and the agent behaves exactly as it did before this cell.
    //
    // Note what does and does not contain a write here, now that create
    // (#551) and the lifecycle pair (#671) are agent-reachable. It is NOT
    // intra-company isolation — an agent may create and overwrite anywhere in
    // its company's tree, by design. What holds is: company tenancy (the store
    // is pinned to one `CompanyId` at build time, and every tool resolves
    // inside a single company-scoped tree read); the explicit grant above; the
    // required `expected_updated_at` CAS token on `workspace_write` and
    // `workspace_delete`; policy parking, since all four mutations are
    // `Reach::Consequence` and never grantable standing
    // (`policy::consequence`); and authorship, since every node records who
    // created it and who last wrote it (#326).
    //
    // Rename and delete additionally reach only `agents/<agent id>/`. Read that
    // as a division of labour, not as a security boundary: the same grant
    // already confers unconfined overwrite, which is the broader power. See the
    // `workspace_tools::lifecycle` module docs.
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
                // Issue #552: so an overwrite of a *published* note is recorded
                // on that deliverable's artifact chain rather than diverging
                // from it. Read-only for the write tool's mirroring — no tool
                // here opens or deletes an artifact.
                deps.artifacts.clone(),
                company.clone(),
                manifest_agent.id.clone(),
                workspace_writes,
                manifest_agent.write_scope(),
            ))
        }
        _ => None,
    };
    let workspace_granted = workspace_tools.is_some();
    if let Some(workspace_tools) = workspace_tools {
        tools.extend(workspace_tools);
    }

    // Agent-authored internal dashboard pages (`pages/<slug>/` in the same
    // workspace store). Unlike workspace reads vs. writes above, there is no
    // two-tier gate here: per the design, `pages` rides the default `"*"`
    // grant whole, so a single `grants_cover` check on `pages` is enough —
    // whoever gets any pages tool gets create/read/write/delete together.
    // Unwired-store is fail-closed, same as the workspace block: with no
    // `deps.workspace` no tool is built and the agent is unaffected.
    if let Some(store) = &deps.workspace
        && grants_cover(grants, "pages")
    {
        tools.extend(crate::harness::pages_tools::pages_tools(
            store.clone(),
            company.clone(),
            manifest_agent.id.clone(),
        ));
    }

    // The company's own record. Ungated, and deliberately: an agent that can
    // read the task board but not what the company already decided is exactly
    // the split these ledgers exist to close, and every *write* here is
    // `PermissionLevel::Write`, so a supervised policy parks it like any other
    // consequence. The one thing no grant can confer is deletion — there is no
    // delete tool at all. See `ledger_tools`.
    let ledger_granted = deps.ledgers.is_some();
    if let Some(store) = &deps.ledgers {
        tools.extend(crate::harness::ledger_tools::ledger_tools(
            crate::company::ledgers::Ledgers::new(company.clone(), store.clone())
                .with_tasks_opt(deps.tasks.clone())
                .with_workspace_opt(deps.workspace.clone()),
            manifest_agent.id.clone(),
            manifest_agent.ledgers.clone(),
            manifest_agent.can_declare_ledgers,
        ));
    }

    // Persona over openhuman's own identity: `omit_identity = true` drops the
    // "you are OpenHuman" preamble so the agent speaks as its company role.
    // Includes the effective persona instructions (issue #1530) — an operator
    // override when one is set, else the manifest `prompt` — resolved by the
    // caller and appended to the generated framing.
    let mut persona = persona_prompt(company_name, manifest_agent, instructions);

    // The agent's checked-in briefing documents, placed here — before every
    // tool brief and before the routed workspace documents — because they are
    // the most static material in the prompt after the persona itself. The
    // prompt prefix is what a provider cache reuses across turns, so ordering
    // static-before-volatile is what keeps an operator editing a workspace note
    // from invalidating the briefing behind it.
    persona.push_str(&crate::company::prompt::bundle_section(manifest_agent));

    // A short, STATIC brief — never a tree snapshot. A snapshot baked into the
    // system prompt would be stale the moment the operator edits a note, which
    // is exactly what hitting the store per call avoids.
    if workspace_granted {
        persona.push_str(&crate::harness::workspace_tools::workspace_brief(
            workspace_writes,
        ));
    }

    // The catalogue, not a pointer to one. A tool granted, unmentioned and
    // never called is the observed failure, so every ledger is named with what
    // it holds — see `ledger_brief`.
    if ledger_granted {
        persona.push_str(&crate::harness::ledger_tools::ledger_brief(
            &deps.ledger_registry,
        ));
    }

    // The agent's own working directory, and the tools that reach it. Placed
    // BEFORE the publish brief because that brief's first sentence ("the files
    // you write live in your sandbox") presumes a sandbox the agent has by then
    // been told about — and because publishing is gated on an artifact store,
    // so a company without one used to get no mention of the sandbox at all
    // while still holding every file tool.
    //
    // Each flag is the same one that wired the tools a few hundred lines up, so
    // the brief cannot describe a namespace this agent was not granted. `shell`
    // in particular was wired since Cell A and named in no brief anywhere: an
    // agent asked to run something recorded a task about running it.
    //
    // `shell_wired`/`wants_code` only reflect the GRANT, but `deps.capabilities`
    // (the per-turn capability tier resolved by `capability_budget::resolve_filter`
    // — live at `HarnessPool::ensure`, not a hypothetical future cell) is applied
    // to the tool vector later by `filter_by_capabilities`, below. Without this
    // check the brief would describe `shell`/`code` on a turn where the capability
    // tier denied them (a fail-closed metering error, or an exhausted budget),
    // telling the agent to call a tool `filter_by_capabilities` already removed.
    let (sandbox_files, sandbox_shell, sandbox_code) =
        sandbox_brief_flags(wants_files, shell_wired, wants_code, &deps.capabilities);
    persona.push_str(&toolbelt::sandbox_brief(
        sandbox_files,
        sandbox_shell,
        sandbox_code,
    ));

    // Issue #244: what a deliverable is, and how to hand one over. Only when
    // the tool was actually wired above — describing a tool the agent does not
    // have is how you get a turn spent calling something that does not exist.
    if publishing {
        persona.push_str(&crate::harness::publish::publish_brief());
    }

    // Issue #1759: ground the agent in its connected-integration surface and
    // route provider actions through it. Appended ONLY when the Composio tools
    // were actually wired above (`composio_toolkits` is `Some`), so — like every
    // other tool brief here — it never describes a surface this agent does not
    // hold. The brief itself is a pure renderer in `composio_catalog` (not behind
    // the `composio` feature) so CI's `openhuman` test lane exercises it; this
    // call site is feature-gated because the tools it describes are.
    //
    // Same `deps.capabilities` check as the `shell`/`code` sandbox brief above
    // (PR #1780 review): `composio_toolkits` reflects only the GRANT, not the
    // per-turn capability tier. When a `free`/`starter`/`pro` plan's Composio
    // budget is exhausted, `filter_by_capabilities` strips every
    // `composio_*` tool from the belt below — without this check the brief
    // would still tell the agent to call one.
    #[cfg(feature = "composio")]
    if toolbelt::composio_capability_admits(composio_toolkits.is_some(), &deps.capabilities)
        && let Some(toolkits) = composio_toolkits.as_deref()
    {
        let native_caps: Vec<&str> = toolbelt::native_capabilities_on_belt(&tools)
            .into_iter()
            .collect();
        persona.push_str(&crate::harness::composio_catalog::composio_brief(
            toolkits,
            &native_caps,
        ));
    }

    // Skill read surface (read-only catalogue slice). Only materializes when the
    // harness is wired to a skills source; otherwise the agent stays skill-less
    // and the default path is untouched. The catalogue is folded into the
    // persona body because `omit_skills_catalog` is inert upstream.
    // The global baseline installs skills in every company, including one with
    // no source dir and no deltas — a platform-provisioned tenant is exactly
    // that — so its presence arms this branch too. Without that clause the
    // baseline would reach every company except the hosted ones.
    if deps.skills_source_dir.is_some()
        || !skill_deltas.is_empty()
        || !crate::globals::skills().is_empty()
    {
        // Named through the same helper as the sandbox beside it, so the two
        // siblings cannot end up under different spellings of one agent.
        let skill_ws = deps
            .workspace_root
            .join(sandbox_segment(company.as_ref()))
            .join(sandbox_segment(&manifest_agent.id))
            .join("skill-catalog");
        // Best-effort, not fatal. This ran only for a company with a skills
        // source or an operator delta until the global baseline made it run for
        // every company — including one whose workspace root is unusable, where
        // failing here would turn "this agent has no skill catalogue" into "this
        // company cannot build an agent at all". The agent still answers; it
        // just answers without the catalogue, and the reason is logged.
        match EffectiveSkills::materialize(
            skill_ws,
            deps.skills_source_dir.as_deref(),
            &deps.skills_registry,
            skill_deltas,
        ) {
            Ok(effective) => {
                if !effective.is_empty() {
                    tools.extend(effective.read_tools());
                    persona.push_str(&effective.catalogue());
                }
            }
            Err(err) => tracing::warn!(
                company = %company.as_ref(),
                agent = %manifest_agent.id,
                error = %err,
                "skill catalogue unavailable for this agent",
            ),
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
        // agent-visible MCP error (the error-hardening cell). Use the same
        // effective grants that selected `registry`, not the raw manifest
        // request: an empty request inherits the company belt and can therefore
        // reach servers even when `manifest_agent.tools` is empty.
        let secrets = granted_secrets(&deps.mcp_servers, grants);
        tools.push(Box::new(OcMcpListServersTool::new(registry.clone())));
        tools.push(Box::new(McpListToolsTool::new(registry.clone())));
        // `OcMcpCallTool` replaces upstream's `McpCallTool`: same name/schema,
        // but it classifies + scrubs failures, rewrites the agent-facing text,
        // and records each failure on the shared queue the brain drains.
        // The metering handle lets `mcp_call_tool` record an `OauthCall` usage
        // sample per completed call, so a company routing its real work through
        // MCP stops reading as zero in the Usage view's calls-by-provider chart
        // and `connections` KPI (issue #698). A `None` meter leaves metering
        // off, exactly as on the Composio path.
        tools.push(Box::new(OcMcpCallTool::new(
            registry,
            mcp_security,
            secrets,
            deps.mcp_failures.clone(),
            crate::harness::mcp::McpMetering {
                company: company.clone(),
                agent: manifest_agent.id.clone(),
                meter: deps.meter.clone(),
            },
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
            // Issue #383: the same supervisor the console's cancel route reads,
            // so a run this agent starts is stoppable by an operator too.
            deps.run_supervisor.clone(),
            // The company store, for the `add_agent` tool to persist overlay
            // teammates through the same path the console `POST .../team` uses.
            deps.store.clone(),
            // Issue #661 (M7): the same revision store the console's workflow
            // PUT/DELETE routes write through, so an agent edit is undoable and
            // an agent delete cascades the history on identical terms.
            deps.workflow_revisions.clone(),
            // Issue #339: the shared queue `run_workflow` / `create_workflow`
            // stage onto, so a dispatched card can link to the workflow its
            // attempt built or ran. Orchestrator-only, like the tools.
            deps.workflow_refs.clone(),
            // Issue #418: the shared run-output cache `run_workflow` fills and
            // the `read_run_output` companion reads back, so a clipped preview
            // is reachable within the turn. Orchestrator-only, like the tools.
            deps.run_outputs.clone(),
            // Issue #619: who is minting, and how wide they are. `add_agent`
            // bounds the teammate it mints by this agent's own scope — #661
            // clamped to the *company* grant, which still lets a narrowly
            // scoped agent mint a teammate holding everything the company
            // holds — and names this agent in the mint log.
            manifest_agent.id.clone(),
            manifest_agent.tools.clone(),
            grants.to_vec(),
            deps.notifications.clone(),
        ));
    }
    // Recursive desk delegation (issue #176): a NON-orchestrator agent whose
    // manifest entry names a `delegates_to` allowlist gets exactly the two
    // hand-off tools — `spawn_task` and a `delegate_to_desk` narrowed to that
    // allowlist — and nothing else from the orchestrator's set. It is what lets
    // a desk lead pull in a specialist for one slice instead of handing the
    // whole thing back to the CEO.
    //
    // `else if` rather than a second `if`: the orchestrator already has both
    // tools from `orchestrator_tools` above, and wiring a second, narrowed
    // `delegate_to_desk` beside its unrestricted one would put two tools with
    // the same name on one belt.
    //
    // An empty allowlist wires nothing, which is the pre-#176 belt exactly — so
    // this whole block is inert for every manifest that has not opted in.
    else if !manifest_agent.delegates_to.is_empty() {
        persona.push_str(&orchestrator::member_delegation_brief(
            &manifest_agent.delegates_to,
        ));
        tools.extend(orchestrator::member_delegation_tools(
            &deps.delegations,
            company.clone(),
            deps.store.clone(),
            orchestrator::MemberScope {
                member: manifest_agent.id.clone(),
                delegates_to: manifest_agent.delegates_to.clone(),
            },
        ));
    }

    // The routed workspace documents go LAST, after every tool brief. They are
    // the most volatile thing in the prompt — an operator editing a note between
    // two turns moves them — and the prompt prefix is what a provider cache
    // reuses across turns, so putting them anywhere earlier would invalidate
    // every brief behind them on an edit that changed none of those briefs.
    persona.push_str(&crate::company::prompt::context_section(routed_context));

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
    let tools = if deps.workspace_git_enabled {
        match crate::harness::built_in::checkpoint::WorkspaceCheckpointer::initialize_off_worker(
            &workspace,
        ) {
            Ok(checkpointer) => crate::harness::built_in::checkpoint::CheckpointingTool::wrap_all(
                tools,
                checkpointer,
            ),
            Err(error) => {
                tracing::warn!(
                    company = %company,
                    agent = %manifest_agent.id,
                    workspace = %workspace.display(),
                    %error,
                    "[workspace-checkpoint] could not initialize Git; continuing without checkpoints"
                );
                tools
            }
        }
    } else {
        tools
    };

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

    // OpenHuman's tool-pack table withholds `composio_*` schemas unless the
    // session identifies as its integrations specialist. OpenCompany already
    // narrows this agent's actual belt by the explicit company and agent grants
    // above; once that grants Composio, use the supported specialist identity
    // so the model can call the real tools rather than being offered an absent
    // pack proxy.
    #[cfg(feature = "composio")]
    let agent_definition_name = if composio_toolkits.is_some() {
        "integrations_agent"
    } else {
        manifest_agent.id.as_str()
    };
    #[cfg(not(feature = "composio"))]
    let agent_definition_name = manifest_agent.id.as_str();

    let mut agent = AgentBuilder::default()
        // `HarnessModel` upcasts to the tinyagents `ChatModel<()>` the builder's
        // native injection seam takes (the old `Provider` adapter is gone).
        .chat_model(deps.provider.clone() as Arc<dyn tinyagents::harness::model::ChatModel<()>>)
        .memory(memory)
        .tools(tools)
        .tool_dispatcher(tool_dispatcher)
        .tool_policy(Arc::new(policy))
        .prompt_builder(prompt_builder)
        // Stated, not inherited (issue #417). Omitting this call leaves the
        // builder on `ContextConfig::default()`, which lands on the same number
        // — so this is behaviour-identical today. What changes is that the
        // number is now *chosen here*, where tools that must size their results
        // against it can read it as [`TOOL_RESULT_BUDGET_BYTES`], instead of
        // being a vendored default no OpenCompany source mentioned.
        .context_config(oh::config::ContextConfig {
            tool_result_budget_bytes: TOOL_RESULT_BUDGET_BYTES,
            ..Default::default()
        })
        .model_name(model)
        .workspace_dir(workspace)
        .agent_definition_name(agent_definition_name)
        .auto_save(false)
        .build()
        .map_err(|e| {
            OpenCompanyError::Harness(format!("build agent '{}': {e}", manifest_agent.id))
        })?;

    // Stated, not inherited (issue #988). The builder has no setter for this, so
    // it is applied post-construction — the same seam openhuman's own task
    // dispatcher and this crate's workflow copilot use. See
    // [`MAX_TOOL_ITERATIONS`] for why 25, and why this is the only lever that
    // works on this construction path.
    agent.set_max_tool_iterations(MAX_TOOL_ITERATIONS);
    Ok(agent)
}

/// The intrinsic deliberate-memory tools (`memory_store` / `memory_recall` /
/// `memory_forget`) — **oc-authored**, over the company's own `ContextStore`
/// (issue #1113 / G11).
///
/// # Why not the vendored upstream tools (history that must not be re-learned)
///
/// Through openhuman's earlier API, `MemoryStoreTool::new` /
/// `MemoryRecallTool::new` took the `Arc<dyn Memory>` directly, so each
/// company's own `ContextStore` was exactly what the two tools read and wrote.
/// The version this crate now vendors changed both constructors to
/// `fn new(security: Arc<SecurityPolicy>)` / `fn new()` — no memory parameter
/// — and moved resolution *inside* `execute()` to
/// `active_memory_guard()`, which reaches an ambient `CoreContext`
/// this crate's session/turn machinery never scopes (`rg CoreContext::scope`
/// under `agent/harness/session` finds nothing), or — with no context bound —
/// falls back to **one process-global workspace** resolved from
/// `Config::load_or_init()`. Either path is disconnected from `.memory(memory)`
/// on the session builder: `Tool::execute(&self, args)` takes no session or
/// memory parameter at all, so there is no route left by which a per-company
/// `Arc<dyn Memory>` could reach these two tools.
///
/// Wiring the upstream tools would mean every company's "deliberate" memory
/// read and write lands in one shared, unconfigured store instead of that
/// company's own `ContextStore` — silently wrong at best, a cross-company
/// memory leak at worst under this crate's multi-tenant-in-one-process model.
/// So the tools here are oc-authored instead (`super::memory_tools`), the
/// `workspace_tools` shape: company and agent captured at build time, the
/// port a field, nothing ambient for `execute()` to reach. Forget became
/// possible when `ContextStore` grew `delete` (every backend implements it);
/// it is scoped to the agent's own `agent-memory/<id>/` rows.
fn memory_tools(deps: &HarnessDeps, company: &CompanyId, agent_id: &str) -> Vec<Box<dyn Tool>> {
    super::memory_tools::memory_tools(deps.context.clone(), company.clone(), agent_id.to_string())
}

/// Whether an agent's effective `grants` cover a tool `namespace`.
///
/// Matches the bare namespace (`docs`), any glob under it (`docs.*`,
/// `docs.read`), or the catch-all `*`. Shared with the workflow toolbelt
/// ([`crate::workflows::caps`]) so a workflow `tool_call` is gated by the same
/// namespace-grant rule an agent's exec tools are.
///
/// A thin caller of [`extends_on_boundary`] rather than its own prefix test.
/// This matcher always required the prefix to stop on a separator (so
/// `documentation.*` is not a grant on `docs`) while the per-tool matcher did
/// not, and the two drifted apart unnoticed. The rule now exists once in the
/// crate, next to [`grant_matches`](crate::runtime::tools), and cannot fork
/// again (issue #461).
pub(crate) fn grants_cover(grants: &[String], namespace: &str) -> bool {
    grants
        .iter()
        .any(|grant| grant == "*" || extends_on_boundary(grant, namespace, NAMESPACE_SEPARATORS))
}

/// One agent's sandbox directory: `{root}/{company}/{agent}/workspace`.
///
/// A named function rather than a repeated `join` chain because three callers
/// now need to agree on it exactly: [`build_agent`], which sandboxes the file
/// tools to it; the brain's #244 unpublished-file scan, which snapshots it; and
/// [`ensure_agent_workspace`], which creates it. A second transcription of the
/// layout would make the scan silently look at the wrong directory — reporting
/// nothing, forever, with no error anywhere.
///
/// Naming only — this never touches the disk. Anything that needs the directory
/// to *exist* goes through [`ensure_agent_workspace`].
pub fn agent_workspace(root: &Path, company: &CompanyId, agent_id: &str) -> PathBuf {
    root.join(sandbox_segment(company.as_ref()))
        .join(sandbox_segment(agent_id))
        .join("workspace")
}

/// One directory name in the sandbox tree, under the workspace naming rule.
///
/// The sandbox is the other half of "the agent's workspace", and it carried the
/// company id and the roster id verbatim — so a company browsing its own data
/// directory found `agentic_law_firm/page_builder/` next to a note tree whose
/// every name is lowercase and dashed. One rule for both
/// ([`crate::company::workspace_names`]) is the point.
///
/// Two ids cannot collide into one sandbox by being normalized. Roster ids are
/// snake_case (`company::manifest::is_snake_case`), so `-` never occurs in one
/// and the mapping is injective over that alphabet. A company id is not
/// validated that tightly, but it already shares a slug with its bundle
/// directory (`store::paths`), so two ids that normalize alike were sharing
/// their company data long before they shared a sandbox.
fn sandbox_segment(raw: &str) -> String {
    crate::company::workspace_names::kebab_name_or(raw, raw)
}

/// One agent's shell audit sink directory, resolved from the instance data root:
/// `{audit_root}/companies/{company}/audit/{agent}` (issue #775).
///
/// A thin adapter over
/// [`DataLayout::agent_audit_dir`](crate::store::DataLayout::agent_audit_dir) so
/// the harness names the layout through the layout type instead of transcribing
/// the path — the same reason [`agent_workspace`] exists.
///
/// `audit_root` is [`HarnessDeps::audit_root`](crate::harness::HarnessDeps),
/// **not** the workspace root: the sink must not land inside the agent workspace,
/// which is also the `workspace_only` policy root the file tools sandbox to.
///
/// Naming only — this never touches the disk.
/// [`toolbelt::shell_audit`](crate::harness::toolbelt::shell_audit) creates it.
pub fn agent_audit_dir(audit_root: &Path, company: &CompanyId, agent_id: &str) -> PathBuf {
    crate::store::DataLayout::new(audit_root).agent_audit_dir(company.as_ref(), agent_id)
}

/// Create one agent's sandbox directory, returning the path
/// [`agent_workspace`] names. Idempotent.
///
/// The single **creation** site for the agent-workspace layout, and not a
/// convenience: the file tools do not work without the directory. OpenHuman's
/// `validate_parent_path` resolves a relative write against `action_dir`, then
/// walks up to the deepest *existing* ancestor to canonicalize it. With the
/// workspace absent that walk climbs straight past it — through `{agent}/` and
/// `{company}/` to the workspace root — and the ancestor it lands on is,
/// correctly, outside the agent's own sandbox. The write is then refused as
/// *"Resolved parent path escapes workspace"* for a path that is plainly inside
/// it (issue #409).
///
/// Nothing else mints this directory. `<home>/harness` is deliberately absent
/// from [`DataLayout::ensure`](crate::store::DataLayout::ensure), which
/// pre-creates only the instance-shared trees; per-company and per-agent trees
/// are minted on demand by whoever owns them, the same rule `companies/`
/// follows. The near miss is
/// [`EffectiveSkills::materialize`](crate::harness::skills::EffectiveSkills),
/// which creates `{agent}/skill-catalog/` — a *sibling* of `workspace`, so it
/// makes the walk stop one level higher and refuse just the same.
///
/// Called from two places, because one is not enough:
///
///  * [`build_agent`], before any [`SecurityPolicy`] is constructed over the
///    path. Ordering matters beyond the guard: the per-workspace audit logger
///    keys its process-global registry on the *canonicalized* workspace path and
///    falls back to the raw one when the directory is missing, so a workspace
///    created late can be registered twice under one physical directory.
///  * the dispatch path ([`HarnessPool::run`](crate::harness::HarnessPool::run)
///    and friends), because a roster is built once and then cached behind
///    fingerprints and handed across an in-place rebuild — so a workspace that
///    disappears after the roster was built (a restored/wiped data dir, an
///    operator clearing the tree, a boot that raced a not-yet-mounted volume)
///    would otherwise stay missing for the life of the process.
pub fn ensure_agent_workspace(
    root: &Path,
    company: &CompanyId,
    agent_id: &str,
) -> std::io::Result<PathBuf> {
    let workspace = agent_workspace(root, company, agent_id);
    adopt_legacy_sandbox(root, company, agent_id, &workspace);
    std::fs::create_dir_all(&workspace)?;
    Ok(workspace)
}

/// Move a pre-lowercase-dashed sandbox onto its canonical path, once.
///
/// The tree used to be named by the company and roster ids verbatim
/// (`agentic_law_firm/page_builder/`), and an agent upgraded into the new
/// naming would otherwise start in an empty directory with its half-finished
/// work still on disk under the old name — present, unreachable, and reported
/// by nothing.
///
/// This is a *rename*, unlike the workspace tree, where the equivalent
/// migration is refused and offered as an operator action instead. The two are
/// different things: this directory is the agent's private scratch, addressed
/// only by [`agent_workspace`] within this process, with no ids, no links and
/// no console pointing into it. Nothing outside can notice the move.
///
/// Best-effort and silent-on-conflict by construction: it acts only when the
/// canonical path does not exist and the legacy one is a directory, so it
/// cannot overwrite a live sandbox, and a failed rename simply leaves the
/// agent with a fresh empty one rather than failing the turn.
fn adopt_legacy_sandbox(root: &Path, company: &CompanyId, agent_id: &str, canonical: &Path) {
    let legacy = root.join(company.as_ref()).join(agent_id).join("workspace");
    if legacy == canonical || canonical.exists() || !legacy.is_dir() {
        return;
    }
    let Some(parent) = canonical.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    match std::fs::rename(&legacy, canonical) {
        Ok(()) => tracing::info!(
            company = %company,
            agent = %agent_id,
            from = %legacy.display(),
            to = %canonical.display(),
            "[harness] moved the agent sandbox onto its lowercase-dashed path"
        ),
        Err(error) => tracing::warn!(
            company = %company,
            agent = %agent_id,
            %error,
            from = %legacy.display(),
            "[harness] could not move the legacy agent sandbox; starting from an empty one"
        ),
    }
}

/// A [`SecurityPolicy`] that sandboxes an agent's file tools to `workspace` and
/// nowhere else: `workspace_only` with both the workspace and the tool action
/// root pinned to the agent's own directory.
pub(crate) fn workspace_security(workspace: &Path) -> SecurityPolicy {
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
pub(crate) fn file_tools(workspace: &Path) -> Vec<Box<dyn Tool>> {
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

    #[tokio::test]
    async fn memory_tools_are_wired_to_the_company_context_store() {
        // The flip of the old withholding lock. The doc comment on
        // `memory_tools` demanded that whatever un-withholds these must first
        // confirm each company's own `ContextStore` genuinely backs them — so
        // that is exactly what this asserts: a store through the tool lands in
        // THIS company's context rows, under the agent's own label prefix,
        // reachable by the same port the memory_loop and the Brain view read.
        use crate::ports::ContextStore;
        use crate::ports::types::CompanyId;
        use std::sync::Arc;

        let dir = tempfile::tempdir().expect("tempdir");
        let context: Arc<dyn ContextStore> =
            Arc::new(crate::store::FsContextStore::new(dir.path().to_path_buf()));
        let company = CompanyId::new("acme");
        let tools = super::super::memory_tools::memory_tools(
            context.clone(),
            company.clone(),
            "ceo".to_string(),
        );
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(names, ["memory_store", "memory_recall", "memory_forget"]);

        let store = &tools[0];
        let reply = store
            .execute(serde_json::json!({"title": "Pin", "body": "the fact"}))
            .await
            .expect("execute");
        assert!(!reply.is_error, "{reply:?}");
        let rows = context.list(&company, "agent-memory/ceo/").await.unwrap();
        assert_eq!(
            rows.len(),
            1,
            "the tool write must land on the company port"
        );
        assert_eq!(rows[0].label, "agent-memory/ceo/pin");
    }

    /// The grant alone is not enough: a wired `shell`/`code` namespace the
    /// capability tier denies must not be described in the sandbox brief,
    /// because `filter_by_capabilities` is about to strip the matching tools
    /// from the vector handed to the builder. This is the fix for the P1
    /// codex found on PR #1670 — before it, `sandbox_brief_flags` did not
    /// exist and the brief was built from the grant flags alone.
    #[test]
    fn sandbox_brief_flags_withhold_a_capability_denied_namespace() {
        use std::collections::HashSet;

        let deny_shell = toolbelt::CapabilityFilter::DenyNamespaces(HashSet::from(["shell"]));
        assert_eq!(
            sandbox_brief_flags(true, true, true, &deny_shell),
            (true, false, true),
            "a denied `shell` must not be reported even though it was wired"
        );

        let deny_code = toolbelt::CapabilityFilter::DenyNamespaces(HashSet::from(["code"]));
        assert_eq!(
            sandbox_brief_flags(true, true, true, &deny_code),
            (true, true, false),
            "a denied `code` must not be reported even though it was granted"
        );

        let deny_both =
            toolbelt::CapabilityFilter::DenyNamespaces(HashSet::from(["shell", "code"]));
        assert_eq!(
            sandbox_brief_flags(true, true, true, &deny_both),
            (true, false, false)
        );
    }

    /// The identity filter changes nothing — the flags are exactly the wired
    /// grant flags, files included (files are never a gateable namespace).
    #[test]
    fn sandbox_brief_flags_pass_through_under_allow_all() {
        assert_eq!(
            sandbox_brief_flags(true, true, true, &toolbelt::CapabilityFilter::AllowAll),
            (true, true, true)
        );
        assert_eq!(
            sandbox_brief_flags(false, false, false, &toolbelt::CapabilityFilter::AllowAll),
            (false, false, false)
        );
    }

    /// An ungranted/unwired namespace stays absent regardless of the capability
    /// filter — denial can only ever narrow, never widen, what the grant wired.
    #[test]
    fn sandbox_brief_flags_never_add_a_namespace_the_grant_did_not_wire() {
        use std::collections::HashSet;

        let allow_all = toolbelt::CapabilityFilter::AllowAll;
        assert_eq!(
            sandbox_brief_flags(false, false, false, &allow_all),
            (false, false, false)
        );

        // Denying a namespace that was never wired is a no-op on that flag.
        let deny_shell = toolbelt::CapabilityFilter::DenyNamespaces(HashSet::from(["shell"]));
        assert_eq!(
            sandbox_brief_flags(false, false, false, &deny_shell),
            (false, false, false)
        );
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

    // --- Agent-workspace provisioning (issue #409) --------------------------

    #[test]
    fn ensure_agent_workspace_mints_the_whole_chain_and_is_idempotent() {
        let root = tempfile::tempdir().expect("tempdir");
        let company = CompanyId::new("acme");

        // Nothing under the root exists yet — not the company segment, not the
        // agent segment. This is a company that has never run.
        let named = agent_workspace(root.path(), &company, "ceo");
        assert!(!named.exists(), "precondition: nothing minted yet");

        let made = ensure_agent_workspace(root.path(), &company, "ceo").expect("first ensure");
        assert_eq!(made, named, "creation and naming must agree exactly");
        assert!(made.is_dir());

        // Idempotent: a second call on an existing tree is a success, not an
        // `AlreadyExists` error — the dispatch path calls this on every turn.
        let again = ensure_agent_workspace(root.path(), &company, "ceo").expect("second ensure");
        assert_eq!(again, made);
        assert!(again.is_dir());
    }

    /// The sandbox is named under the workspace naming rule, so a snake_case
    /// roster id and an underscored company id land on dashed directories —
    /// the same convention the note tree beside them is kept in.
    #[test]
    fn the_sandbox_path_is_lowercase_and_dashed() {
        let root = tempfile::tempdir().expect("tempdir");
        let named = agent_workspace(
            root.path(),
            &CompanyId::new("Agentic_Law Firm"),
            "page_builder",
        );

        assert_eq!(
            named,
            root.path()
                .join("agentic-law-firm")
                .join("page-builder")
                .join("workspace")
        );
    }

    /// An agent upgraded into the new naming keeps the work it had in flight.
    ///
    /// The sandbox is private scratch that nothing outside this process
    /// addresses, so moving it is invisible — while leaving it behind would
    /// strand a half-finished file on disk, present and unreachable, with
    /// nothing reporting it.
    #[test]
    fn a_pre_rule_sandbox_is_moved_onto_the_canonical_path() {
        let root = tempfile::tempdir().expect("tempdir");
        let company = CompanyId::new("acme");

        let legacy = root
            .path()
            .join("acme")
            .join("page_builder")
            .join("workspace");
        std::fs::create_dir_all(&legacy).expect("legacy sandbox");
        std::fs::write(legacy.join("draft.md"), "half-finished").expect("in-flight work");

        let made = ensure_agent_workspace(root.path(), &company, "page_builder").expect("ensure");

        assert_eq!(made, agent_workspace(root.path(), &company, "page_builder"));
        assert_eq!(
            std::fs::read_to_string(made.join("draft.md")).expect("the work came with it"),
            "half-finished"
        );
        assert!(!legacy.exists(), "the legacy path is not left as a twin");
    }

    /// A sandbox that already exists at the canonical path is never overwritten
    /// by a stale legacy one — the move is a one-time adoption, not a sync.
    #[test]
    fn a_live_sandbox_is_never_replaced_by_a_legacy_one() {
        let root = tempfile::tempdir().expect("tempdir");
        let company = CompanyId::new("acme");

        let canonical =
            ensure_agent_workspace(root.path(), &company, "page_builder").expect("ensure");
        std::fs::write(canonical.join("current.md"), "live").expect("live work");
        let legacy = root
            .path()
            .join("acme")
            .join("page_builder")
            .join("workspace");
        std::fs::create_dir_all(&legacy).expect("legacy sandbox");
        std::fs::write(legacy.join("stale.md"), "stale").expect("stale work");

        let again = ensure_agent_workspace(root.path(), &company, "page_builder").expect("ensure");

        assert_eq!(again, canonical);
        assert_eq!(
            std::fs::read_to_string(canonical.join("current.md")).expect("still there"),
            "live"
        );
        assert!(!canonical.join("stale.md").exists());
    }

    /// The bug, pinned. With the workspace absent, `validate_parent_path` walks
    /// up past it to an ancestor that really *is* outside the sandbox and
    /// refuses a plainly-inside relative path — the refusal an agent granted
    /// `files` but not `shell` used to hit on every write.
    #[tokio::test]
    async fn a_missing_workspace_makes_a_plain_relative_write_look_like_an_escape() {
        let root = tempfile::tempdir().expect("tempdir");
        let workspace = agent_workspace(root.path(), &CompanyId::new("acme"), "ceo");
        assert!(!workspace.exists(), "precondition: never provisioned");

        let policy = workspace_security(&workspace);
        let err = policy
            .validate_parent_path("notes.md")
            .await
            .expect_err("a missing workspace refuses the write");
        assert!(
            err.contains("escapes workspace"),
            "the guard blames traversal for a missing directory: {err}"
        );
    }

    /// The fix. The same policy over an *ensured* workspace resolves the same
    /// relative path, inside the sandbox.
    #[tokio::test]
    async fn an_ensured_workspace_resolves_a_relative_write_inside_the_sandbox() {
        let root = tempfile::tempdir().expect("tempdir");
        let workspace =
            ensure_agent_workspace(root.path(), &CompanyId::new("acme"), "ceo").expect("ensure");

        let policy = workspace_security(&workspace);
        let resolved = policy
            .validate_parent_path("notes.md")
            .await
            .expect("an existing workspace accepts a relative write");

        let canonical = workspace.canonicalize().expect("canonicalize");
        assert!(
            resolved.starts_with(&canonical),
            "{} is not inside {}",
            resolved.display(),
            canonical.display()
        );
        assert_eq!(
            resolved.file_name().and_then(|n| n.to_str()),
            Some("notes.md")
        );

        // A nested path whose parent does not exist yet still resolves — the
        // guard only ever needed *some* existing ancestor inside the sandbox.
        let nested = policy
            .validate_parent_path("reports/q3/summary.md")
            .await
            .expect("a not-yet-created subdirectory still resolves");
        assert!(nested.starts_with(&canonical));
    }

    /// Provisioning does not loosen the guard: a genuine escape is still
    /// refused — and it comes back **word for word** the same as the
    /// missing-workspace refusal above. That is why #409 was filed rather than
    /// closed by the one-line create.
    ///
    /// Reaching the resolved-parent arm with a real escape takes some care,
    /// which is itself part of the finding. A symlink whose *immediate* parent
    /// resolves (`escape/loot.txt`) is caught earlier, by the string-level
    /// symlink check, with a different and perfectly clear message. The arm
    /// under test is reached only when no existing ancestor can be canonicalized
    /// up front: a symlink out of the sandbox plus a not-yet-created
    /// subdirectory under it. So in a `workspace_only` agent sandbox this
    /// wording fires for exactly two conditions — a hostile symlink and a
    /// workspace that was never created — and says "escapes workspace" for both.
    ///
    /// Pinned here so a wording change upstream surfaces in this repo instead of
    /// drifting silently.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_real_escape_and_a_missing_workspace_are_refused_in_identical_words() {
        let root = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        let workspace =
            ensure_agent_workspace(root.path(), &CompanyId::new("acme"), "ceo").expect("ensure");
        std::os::unix::fs::symlink(outside.path(), workspace.join("escape")).expect("symlink");

        let policy = workspace_security(&workspace);

        // The easy half: the immediate parent resolves, so the string-level
        // symlink check refuses it first — clearly, and distinguishably.
        let shallow = policy
            .validate_parent_path("escape/loot.txt")
            .await
            .expect_err("a symlink out of the sandbox is refused");
        assert!(
            shallow.contains("Path not allowed by security policy"),
            "expected the string-level refusal: {shallow}"
        );

        // The arm this issue is about: nothing up front can be canonicalized,
        // so the ancestor walk runs and lands outside the sandbox.
        let deep = policy
            .validate_parent_path("escape/nested/loot.txt")
            .await
            .expect_err("a real escape must still be refused");
        assert!(
            deep.contains("Resolved parent path escapes workspace"),
            "expected the resolved-parent refusal: {deep}"
        );

        // The same arm, reached instead by a workspace nobody ever created.
        let absent = agent_workspace(root.path(), &CompanyId::new("acme"), "nobody");
        let missing = workspace_security(&absent)
            .validate_parent_path("notes.md")
            .await
            .expect_err("a missing workspace refuses too");
        assert!(
            missing.contains("Resolved parent path escapes workspace"),
            "{missing}"
        );

        // Verbatim identical up to the path each names. A reader given either
        // one goes looking for a traversal attempt; only one of them is.
        let strip = |m: &str| {
            m.split_once("escapes workspace: ")
                .map(|(head, _)| head.to_string())
                .unwrap_or_default()
        };
        assert_eq!(
            strip(&deep),
            strip(&missing),
            "an attack and an unprovisioned directory should not read alike"
        );
    }

    /// A `..` traversal is refused earlier, by the string-level check, and
    /// *does* read differently — so the ambiguity above is specifically about
    /// the resolved-parent arm, not about every refusal.
    #[tokio::test]
    async fn a_dot_dot_traversal_is_refused_with_a_distinguishable_message() {
        let root = tempfile::tempdir().expect("tempdir");
        let workspace =
            ensure_agent_workspace(root.path(), &CompanyId::new("acme"), "ceo").expect("ensure");

        let err = workspace_security(&workspace)
            .validate_parent_path("../../loot.txt")
            .await
            .expect_err("a traversal is refused");
        assert!(
            err.contains("Path not allowed by security policy"),
            "expected the string-level refusal: {err}"
        );
        assert!(
            !err.contains("escapes workspace"),
            "this arm is already distinguishable: {err}"
        );
    }

    #[test]
    fn model_for_tier_maps_hints_and_defaults() {
        assert_eq!(model_for_tier(Some("reasoning")), "reasoning-v1");
        assert_eq!(model_for_tier(Some("AGENTIC")), "agentic-v1");
        assert_eq!(model_for_tier(Some("frontend")), "agentic-v1");
        assert_eq!(model_for_tier(None), "chat-v1");
        assert_eq!(model_for_tier(Some("mystery")), "chat-v1");
    }

    fn manifest_agent(role: &str, description: Option<&str>) -> ManifestAgent {
        ManifestAgent {
            global: false,
            id: "ceo".to_string(),
            role: role.to_string(),
            name: None,
            description: description.map(str::to_string),
            tier: None,
            harness: None,
            tools: None,
            delegates_to: Vec::new(),
            context: None,
            budget_usd_daily: None,
            prompt: None,
            prompt_files: Vec::new(),
            prompt_files_resolved: Vec::new(),
            classes: Vec::new(),
            ledgers: None,
            can_declare_ledgers: true,
            model: None,
        }
    }

    #[test]
    fn persona_frames_role_company_and_description() {
        let agent = manifest_agent("Chief Executive", Some("Sets direction."));
        let persona = persona_prompt("Acme", &agent, None);
        assert!(persona.contains("Chief Executive"), "{persona}");
        assert!(persona.contains("Acme"), "{persona}");
        assert!(persona.contains("first person"), "{persona}");
        assert!(persona.ends_with("Sets direction."), "{persona}");
    }

    #[test]
    fn persona_omits_absent_or_blank_description() {
        let persona = persona_prompt("Acme", &manifest_agent("Engineer", Some("   ")), None);
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
        async fn delete(
            &self,
            _: &CompanyId,
            _: &crate::ports::types::ChunkAddr,
        ) -> crate::Result<bool> {
            Ok(false)
        }
        async fn delete_label(
            &self,
            _: &CompanyId,
            _: &crate::ports::types::ChunkAddr,
            _: &str,
        ) -> crate::Result<bool> {
            Ok(false)
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
    fn pin_deps(root: std::path::PathBuf) -> HarnessDeps {
        // Two DISTINCT roots under one caller-owned tempdir, mirroring
        // production (`<home>/harness` beside `<home>/companies`). Reusing one
        // root here would let a test pass while the audit sink sat inside the
        // workspace tree — the exact defect issue #775 fixed.
        let workspace_root = root.join("harness");
        // Production sets this to `<home>/mcp` (see `runtime::builder`); mirror
        // it so the pinned belt reflects a real company rather than the
        // degraded no-MCP-home shape.
        let mcp_home = Some(root.join("mcp"));
        let audit_root = root;
        HarnessDeps {
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            serves: None,
            context: Arc::new(PinContext),
            store: Arc::new(PinStore),
            meter: None,
            workspace_root,
            mcp_home,
            workspace_git_enabled: false,
            audit_root,
            model_override: None,
            tasks: None,
            artifacts: None,
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            default_mcp_servers: Vec::new(),
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: DelegationQueue::default(),
            workflow_runner: WorkflowRunnerHandle::default(),
            mcp_failures: McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_revisions: None,
            approval_requests: ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            #[cfg(feature = "chargebee")]
            chargebee: None,
            #[cfg(feature = "paypal")]
            paypal: None,
            hosting: None,
            steer: crate::company::steer::InflightRegistry::default(),
            run_supervisor: crate::runtime::RunSupervisor::default(),
            delivery: None,
            // Fail-closed default: with no managed search backend wired, the
            // #238 tool is never built and the pinned belt below is the
            // pre-#238 belt exactly.
            search: None,
            tenant_search: None,
            // Fail-closed default: with no workspace store wired, the #237
            // tools are never built and the pinned belt below is the
            // pre-#237 belt exactly.
            workspace: None,
            workflow_runs: None,
            deep_trace: None,
        }
    }

    /// Build one agent under `grants` and return its live tool names, sorted, so
    /// a snapshot compares byte-stably against a literal.
    fn built_tool_names(grants: &[&str], is_orchestrator: bool) -> Vec<String> {
        built_tool_names_delegating(grants, is_orchestrator, &[])
    }

    /// [`built_tool_names`] with a `delegates_to` allowlist on the agent (issue
    /// #176) — the only difference between a member that may re-delegate and one
    /// that may not.
    fn built_tool_names_delegating(
        grants: &[&str],
        is_orchestrator: bool,
        delegates_to: &[&str],
    ) -> Vec<String> {
        let dir = tempfile::tempdir().expect("tempdir");
        let deps = pin_deps(dir.path().to_path_buf());
        let manifest_agent = ManifestAgent {
            global: false,
            id: "desk".to_string(),
            role: "Desk Lead".to_string(),
            name: None,
            description: None,
            tier: None,
            harness: None,
            tools: None,
            delegates_to: delegates_to.iter().map(|d| d.to_string()).collect(),
            context: None,
            budget_usd_daily: None,
            prompt: None,
            prompt_files: Vec::new(),
            prompt_files_resolved: Vec::new(),
            classes: Vec::new(),
            ledgers: None,
            can_declare_ledgers: true,
            model: None,
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
            &[],
            None,
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
            global: false,
            id: "desk".to_string(),
            role: "Desk Lead".to_string(),
            name: None,
            description: None,
            tier: None,
            harness: None,
            tools: None,
            delegates_to: Vec::new(),
            context: None,
            budget_usd_daily: None,
            prompt: None,
            prompt_files: Vec::new(),
            prompt_files_resolved: Vec::new(),
            classes: Vec::new(),
            ledgers: None,
            can_declare_ledgers: true,
            model: None,
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
            &[],
            None,
            false,
        )
        .expect("agent builds");
        let mut names: Vec<String> = agent.tools().iter().map(|t| t.name().to_string()).collect();
        names.sort();
        names
    }

    /// The native capabilities `native_capabilities_on_belt` reads off the SAME
    /// agent [`built_tool_names_with_search`] builds — proving the brief's native
    /// set is derived from tools that were actually wired, not from the grants.
    fn built_native_caps_with_search(grants: &[&str]) -> Vec<String> {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut deps = pin_deps(dir.path().to_path_buf());
        deps.search = Some(crate::harness::search::SearchBackend::new(
            "https://api.example.test".to_string(),
            crate::company::credentials::Credential::from_value("managed-platform-token"),
            crate::company::DEFAULT_SEARCH_DAILY_CALLS,
        ));
        let manifest_agent = ManifestAgent {
            global: false,
            id: "desk".to_string(),
            role: "Desk Lead".to_string(),
            name: None,
            description: None,
            tier: None,
            harness: None,
            tools: None,
            delegates_to: Vec::new(),
            context: None,
            budget_usd_daily: None,
            prompt: None,
            prompt_files: Vec::new(),
            prompt_files_resolved: Vec::new(),
            classes: Vec::new(),
            ledgers: None,
            can_declare_ledgers: true,
            model: None,
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
            &[],
            None,
            false,
        )
        .expect("agent builds");
        toolbelt::native_capabilities_on_belt(agent.tools())
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    /// The brief's native set is read off the wired belt: an explicit `search`
    /// grant with a credential wires `web_search`, so `search` shows up in the
    /// belt's native capabilities — and a bare `*` (which never wires the metered
    /// tool) does not.
    #[test]
    fn native_capabilities_on_belt_track_the_wired_search_tool() {
        let granted = built_native_caps_with_search(&["search"]);
        assert!(
            granted.contains(&"search".to_string()),
            "an explicit search grant wires web_search, so `search` is native on the belt: {granted:?}"
        );
        let wildcard = built_native_caps_with_search(&["*"]);
        assert!(
            !wildcard.contains(&"search".to_string()),
            "a bare `*` wires no metered search tool, so `search` is not native on the belt: {wildcard:?}"
        );
    }

    /// Build one agent under `grants` with BOTH a managed search backend and a
    /// company's own `provider` connection wired, and return its live tool
    /// names. The two together is the interesting case: it is what a company
    /// that pasted a key into the console actually has, and what decides which
    /// of the two surfaces the model is offered.
    fn built_tool_names_with_byo_search(grants: &[&str], provider: &str) -> Vec<String> {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut deps = pin_deps(dir.path().to_path_buf());
        deps.search = Some(crate::harness::search::SearchBackend::new(
            "https://api.example.test".to_string(),
            crate::company::credentials::Credential::from_value("managed-platform-token"),
            crate::company::DEFAULT_SEARCH_DAILY_CALLS,
        ));
        deps.tenant_search = Some(crate::harness::search_byo::TenantSearch::for_test(
            provider,
            Some("tenant-key"),
            Some("https://searx.example"),
        ));
        let manifest_agent = ManifestAgent {
            global: false,
            id: "desk".to_string(),
            role: "Desk Lead".to_string(),
            name: None,
            description: None,
            tier: None,
            harness: None,
            model: None,
            tools: None,
            delegates_to: Vec::new(),
            context: None,
            budget_usd_daily: None,
            prompt: None,
            prompt_files: Vec::new(),
            prompt_files_resolved: Vec::new(),
            classes: Vec::new(),
            ledgers: None,
            can_declare_ledgers: true,
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
            &[],
            None,
            false,
        )
        .expect("agent builds");
        let mut names: Vec<String> = agent.tools().iter().map(|t| t.name().to_string()).collect();
        names.sort();
        names
    }

    /// A company's own provider REPLACES the managed surface rather than
    /// joining it, and still answers to the one name the skills know.
    ///
    /// Both halves matter. Two "search the web" tools on one belt would let the
    /// model spend the platform's metered budget for a company that pasted its
    /// own key — the exact bill-swap the BYO surface exists to prevent. And a
    /// belt where the canonical name changed with the provider would break the
    /// shipped research skills, which name `web_search` in their instructions.
    #[test]
    fn a_company_provider_replaces_the_managed_search_tool_under_the_same_name() {
        let byo = built_tool_names_with_byo_search(&["search"], "brave");

        assert!(
            byo.contains(&"web_search".to_string()),
            "the canonical name must survive the provider switch: {byo:?}"
        );
        assert!(
            byo.contains(&"brave_news_search".to_string()),
            "the provider's own extras must be wired too: {byo:?}"
        );
        // Exactly one tool answers to the canonical name.
        assert_eq!(
            byo.iter().filter(|name| *name == "web_search").count(),
            1,
            "two search tools under one name: {byo:?}"
        );
        // And the managed family's siblings are absent — nothing on this belt
        // reaches the platform's metered backend.
        assert!(
            !byo.contains(&"exa_search".to_string()),
            "a Brave company must not carry Exa tools: {byo:?}"
        );
    }

    /// The BYO surface rides the SAME explicit grant as the metered one. A
    /// company key does not turn `search` into a wildcard-conferred namespace:
    /// the queries still leave the building, and which index reads them is a
    /// decision the manifest makes by name.
    #[test]
    fn a_wildcard_grant_confers_no_search_tools_even_with_a_company_provider() {
        let wildcard = built_tool_names_with_byo_search(&["*"], "exa");
        assert!(
            !wildcard.contains(&"web_search".to_string()),
            "{wildcard:?}"
        );
        assert!(
            !wildcard.contains(&"exa_get_contents".to_string()),
            "{wildcard:?}"
        );
    }

    // --- Company-workspace wiring gates (issue #237) -----------------------

    /// Build one agent with a workspace store wired and return its tool names.
    /// Mirrors [`built_tool_names`], differing only in `deps.workspace`.
    fn built_tool_names_with_workspace(grants: &[&str]) -> Vec<String> {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut deps = pin_deps(dir.path().to_path_buf());
        deps.workspace = Some(Arc::new(crate::store::FsOps::new(dir.path())));
        let manifest_agent = ManifestAgent {
            global: false,
            id: "desk".to_string(),
            role: "Desk Lead".to_string(),
            name: None,
            description: None,
            tier: None,
            harness: None,
            tools: None,
            delegates_to: Vec::new(),
            context: None,
            budget_usd_daily: None,
            prompt: None,
            prompt_files: Vec::new(),
            prompt_files_resolved: Vec::new(),
            classes: Vec::new(),
            ledgers: None,
            can_declare_ledgers: true,
            model: None,
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
            &[],
            None,
            false,
        )
        .expect("agent builds");
        let mut names: Vec<String> = agent.tools().iter().map(|t| t.name().to_string()).collect();
        names.sort();
        names
    }

    /// Build one agent with an artifact store wired, so the #244 publish gate
    /// can be exercised in both its states.
    fn built_tool_names_with_artifacts(grants: &[&str]) -> Vec<String> {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut deps = pin_deps(dir.path().to_path_buf());
        deps.artifacts = Some(Arc::new(crate::store::FsOps::new(dir.path())));
        let manifest_agent = ManifestAgent {
            global: false,
            id: "desk".to_string(),
            role: "Desk Lead".to_string(),
            name: None,
            description: None,
            tier: None,
            harness: None,
            tools: None,
            delegates_to: Vec::new(),
            context: None,
            budget_usd_daily: None,
            prompt: None,
            prompt_files: Vec::new(),
            prompt_files_resolved: Vec::new(),
            classes: Vec::new(),
            ledgers: None,
            can_declare_ledgers: true,
            model: None,
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
            &[],
            None,
            false,
        )
        .expect("agent builds");
        let mut names: Vec<String> = agent.tools().iter().map(|t| t.name().to_string()).collect();
        names.sort();
        names
    }

    /// Issue #244's two gates, in one table.
    ///
    /// The fail-closed row is the load-bearing one: an agent granted file tools
    /// with **no artifact store** must not be offered `publish_artifact`. The
    /// tool stages into a queue; with nothing to drain it, a call would report
    /// success, tell the agent its deliverable was safe, and drop it. Not
    /// offering it is the only honest option.
    #[test]
    fn publish_artifact_needs_both_a_file_grant_and_a_store() {
        let tool = crate::harness::publish::PUBLISH_ARTIFACT_TOOL.to_string();

        // `files` (and its aliases and the wildcard) + a store → present.
        // Publishing spends nothing and reaches nothing outside the company, so
        // unlike `media`/`search` it rides the ordinary namespace rule.
        for grant in ["files", "docs", "files.write", "*"] {
            let names = built_tool_names_with_artifacts(&[grant]);
            assert!(
                names.contains(&tool),
                "`{grant}` + a store must wire publish_artifact: {names:?}"
            );
        }

        // No file grant → absent. An agent that cannot write a file has nothing
        // to publish.
        let unfiled = built_tool_names_with_artifacts(&["web"]);
        assert!(
            !unfiled.contains(&tool),
            "an agent with no file tools must not be offered publish_artifact: {unfiled:?}"
        );

        // File grant, NO store → absent, fail-closed.
        let storeless = built_tool_names(&["files"], false);
        assert!(
            !storeless.contains(&tool),
            "without an artifact store the tool would stage into a void: {storeless:?}"
        );
        // …and the rest of the file belt is untouched, so the gate withholds one
        // tool rather than breaking the agent.
        assert!(
            storeless.contains(&"file_write".to_string()),
            "{storeless:?}"
        );
    }

    /// **Issue #1192, the standard issue #886 stated.** The verdict the console
    /// renders must equal what the toolbelt actually wires — asserted by running
    /// both over the same grant matrix, not by reading the two implementations
    /// and agreeing they look alike.
    ///
    /// The console panel calls
    /// [`grants_files_or_docs`](crate::company::grants_files_or_docs); this gate
    /// calls it too, so today the equality is true by construction. That is the
    /// point of pinning it: the day somebody re-inlines a `starts_with` on
    /// either side — or "tidies" the predicate into the `_explicit` family,
    /// where `*` confers nothing — this fails instead of a panel quietly
    /// reporting a capability no agent has, which is the failure #886 was filed
    /// about and the failure #886 said a test like this one prevents.
    ///
    /// An artifact store is wired throughout, so the store gate is held constant
    /// and the grant is the only variable — which is exactly the axis the
    /// console field answers on. (The store half is not a console field at all:
    /// production always configures one, so a `artifactStoreConfigured` flag
    /// would serialize a hardcoded `true`.)
    #[test]
    fn the_capability_verdict_matches_what_the_toolbelt_wires() {
        let tool = crate::harness::publish::PUBLISH_ARTIFACT_TOOL.to_string();
        for grant in [
            "*",
            "files",
            "docs",
            "files.write",
            "docs.read",
            "web",
            "shell",
            "documentation",
            "docsy",
            "filesystem",
            "composio",
            "repo",
        ] {
            let verdict = crate::company::grants_files_or_docs(&[grant.to_string()]);
            let wired = built_tool_names_with_artifacts(&[grant]).contains(&tool);
            assert_eq!(
                verdict, wired,
                "`{grant}`: the console would report publishing={verdict} while the toolbelt \
                 wires={wired}"
            );
        }
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
        for tool in [
            "workspace_list",
            "workspace_read",
            "workspace_search",
            "workspace_write",
            "workspace_rename",
            "workspace_delete",
        ] {
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
        // Issue #607: search is a read and rides the read side of the gate. It
        // reads exactly what `workspace_read` already grants, and it is the
        // cheap path — behind the write grant it would be missing from every
        // default agent, leaving them on the list-then-read crawl.
        assert!(
            wildcard.contains(&"workspace_search".to_string()),
            "a bare `*` must confer workspace search: {wildcard:?}"
        );
        for tool in ["workspace_write", "workspace_rename", "workspace_delete"] {
            assert!(
                !wildcard.contains(&tool.to_string()),
                "a bare `*` must NOT confer `{tool}`: {wildcard:?}"
            );
        }

        // Explicit `workspace` → reads + all four mutations. The lifecycle pair
        // (issue #671) rides this grant rather than a new one: it reaches only
        // the agent's own folder, which is narrower than the unconfined
        // overwrite the same grant already confers.
        let explicit = built_tool_names_with_workspace(&["workspace"]);
        for tool in [
            "workspace_create",
            "workspace_write",
            "workspace_rename",
            "workspace_delete",
        ] {
            assert!(explicit.contains(&tool.to_string()), "{tool}: {explicit:?}");
        }

        // No workspace grant at all → nothing, even with a store wired.
        let ungranted = built_tool_names_with_workspace(&["web.*"]);
        for tool in [
            "workspace_list",
            "workspace_read",
            "workspace_search",
            "workspace_write",
            "workspace_rename",
            "workspace_delete",
        ] {
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
        for tool in ["workspace_write", "workspace_rename", "workspace_delete"] {
            assert!(
                !read_grant.contains(&tool.to_string()),
                "`workspace.read` must not confer `{tool}`: {read_grant:?}"
            );
        }

        let write_grant = built_tool_names_with_workspace(&["workspace.write"]);
        for tool in ["workspace_write", "workspace_rename", "workspace_delete"] {
            assert!(
                write_grant.contains(&tool.to_string()),
                "{tool}: {write_grant:?}"
            );
        }
    }

    /// `workspace_search` rides the `workspace` READ grant and NEVER the
    /// metered `search` grant (issue #607).
    ///
    /// The names invite the wrong wiring, and the wrong wiring would defeat the
    /// issue: `search` is the paid external-credential grant that carries
    /// `web_search`, and putting workspace search behind it would mean an agent
    /// needs a billed backend credential to read its own company's notes — the
    /// crawl stays, and now it stays for a reason nobody would guess from the
    /// tool's description. Search reads exactly what `workspace_read` already
    /// grants, so it costs the operator no additional decision.
    #[test]
    fn workspace_search_rides_the_workspace_grant_and_not_the_metered_search_grant() {
        // The `search` grant alone confers `web_search` — and nothing from the
        // workspace family, which is not granted here at all.
        let metered = built_tool_names_with_search(&["search"]);
        assert!(metered.contains(&"web_search".to_string()), "{metered:?}");
        assert!(
            !metered.contains(&"workspace_search".to_string()),
            "the metered `search` grant must not confer workspace search: {metered:?}"
        );

        // …and the workspace read grant confers workspace search without
        // conferring the billed one.
        let workspace = built_tool_names_with_workspace(&["workspace.read"]);
        assert!(
            workspace.contains(&"workspace_search".to_string()),
            "`workspace.read` must confer workspace search: {workspace:?}"
        );
        assert!(
            !workspace.contains(&"web_search".to_string()),
            "reading company notes must not require a billed search credential: {workspace:?}"
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
    /// `--features openhuman,mcp` — the combination a full local build
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
            // The deliberate-memory trio (issue #1113): intrinsic, on every
            // belt — company-scoped by construction, see memory_tools.rs.
            "memory_forget",
            "memory_recall",
            "memory_store",
            "read_workspace_state",
            "request_approval",
            "shell",
            "web_fetch",
            // Issue #1861: intrinsic, on every belt and gated by nothing. The
            // ability to ask a person a question is not a capability an agent
            // can be too narrow to hold — a narrow agent is the one most likely
            // to hit something only the operator can answer, and its
            // alternatives are guessing or going quiet.
            "escalate_to_human",
        ];
        // The global baseline installs skills in every company (issue: global
        // agents/skills/workflows), so the three skill read tools are on every
        // belt now — including a company with no skills source of its own.
        expected.extend(["describe_skill", "list_skills", "read_skill_resource"]);
        expected.sort();
        // Mirrors the `#[cfg(feature = "mcp")]` push in `build_agent`. These two
        // are intrinsic (unmapped by `namespace_of`), so no grant gates them:
        // the feature plus a configured `HarnessDeps::mcp_home` is the whole
        // condition. The home is what selects the company's own registry store,
        // and a tool built without it would read a different one.
        #[cfg(feature = "mcp")]
        {
            expected.push("mcp_registry_list_tools");
            expected.push("mcp_registry_tool_call");
            expected.sort();
        }
        assert_eq!(names, expected, "dispatched desk belt drifted: {names:?}");
    }

    #[test]
    fn request_approval_is_intrinsic_and_needs_no_manifest_grant() {
        let names = built_tool_names(&[], false);
        assert!(names.contains(&"request_approval".to_string()), "{names:?}");
    }

    /// Issue #988: the tool-iteration ceiling is **stated** on every agent this
    /// crate builds, not inherited by omission.
    ///
    /// The distinction is the whole bug. `build_agent` never called
    /// `set_max_tool_iterations`, so every company agent silently ran on
    /// `AgentConfig::default()`'s ten — a number no OpenCompany source
    /// mentioned, and one that a teammate doing real multi-step work spends
    /// before it delivers anything (#926). Deleting the call again would put the
    /// build back on the vendored default and fail here.
    ///
    /// The second assertion is what keeps this honest across a vendored bump: it
    /// checks the stated number is genuinely *higher* than what omission would
    /// have given, rather than comparing a constant to itself.
    #[test]
    fn every_built_agent_states_a_raised_tool_iteration_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let deps = pin_deps(dir.path().to_path_buf());
        let manifest_agent = ManifestAgent {
            global: false,
            id: "ceo".to_string(),
            role: "Chief Executive".to_string(),
            name: None,
            description: None,
            tier: None,
            tools: None,
            delegates_to: Vec::new(),
            context: None,
            harness: None,
            budget_usd_daily: None,
            prompt: None,
            prompt_files: Vec::new(),
            prompt_files_resolved: Vec::new(),
            classes: Vec::new(),
            ledgers: None,
            can_declare_ledgers: true,
            model: None,
        };
        let agent = build_agent(
            &CompanyId::new("acme"),
            "Acme",
            &manifest_agent,
            ApprovalPolicy::new(&Policy::default(), None),
            &deps,
            &[],
            &[],
            &[],
            None,
            false,
        )
        .expect("agent builds");

        assert_eq!(
            agent.agent_config().max_tool_iterations,
            MAX_TOOL_ITERATIONS,
            "the built agent is not running on the cap this crate states"
        );

        let inherited = oh::config::AgentConfig::default().max_tool_iterations;
        assert!(
            MAX_TOOL_ITERATIONS > inherited,
            "stating {MAX_TOOL_ITERATIONS} is only a fix while it exceeds the vendored \
             default of {inherited}"
        );
    }

    /// (b) The **default** depth cap (issues #178, #176): a dispatched desk
    /// agent that named no `delegates_to` must NEVER receive a delegation tool,
    /// while the orchestrator agent MUST. Building both from the same grant and
    /// contrasting them is the registration check that an ordinary dispatched
    /// turn cannot re-delegate.
    ///
    /// #176 made this the default rather than the only possibility — the
    /// opt-in case is pinned by
    /// [`a_member_with_delegates_to_gets_exactly_the_two_hand_off_tools`], and
    /// the belt above is unchanged for every agent that does not opt in.
    #[test]
    fn dispatched_agent_has_no_delegation_tools_but_orchestrator_does() {
        let delegation = [
            "query_company",
            "spawn_task",
            "delegate_to_desk",
            // Issue #884: the new hand-off is opt-in on exactly the same terms —
            // an ordinary dispatched agent must not silently gain the ability to
            // run somebody else's turn.
            "delegate_to_teammate",
        ];

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

    /// (b2) Issue #176: a member the manifest opted in with `delegates_to` gets
    /// **exactly the hand-off tools** more than it had — `spawn_task`,
    /// `delegate_to_desk`, and (issue #884) `delegate_to_teammate` — and not one
    /// tool of the orchestrator's authority.
    ///
    /// Expressed as a delta against the un-opted-in belt rather than as a second
    /// flat literal, so the feature-aware snapshot above stays the single place
    /// the dispatched belt is written down. What this pins is the thing #176
    /// could get wrong: reaching for `orchestrator_tools` and handing a desk
    /// lead `add_agent`, `assign_task` and `review_task` along with the hand-off
    /// it actually needs.
    #[test]
    fn a_member_with_delegates_to_gets_exactly_the_two_hand_off_tools() {
        let plain = built_tool_names(&["*"], false);
        let delegating = built_tool_names_delegating(&["*"], false, &["research"]);

        let added: Vec<&String> = delegating.iter().filter(|t| !plain.contains(t)).collect();
        assert_eq!(
            added,
            vec!["delegate_to_desk", "delegate_to_teammate", "spawn_task"],
            "a delegating member's belt must differ from the plain one by exactly the \
             hand-off tools: {delegating:?}"
        );
        assert!(
            plain.iter().all(|t| delegating.contains(t)),
            "opting in must ADD tools, never remove any: {delegating:?}"
        );
        for authority in [
            "query_company",
            "assign_task",
            "review_task",
            "add_agent",
            "run_workflow",
            "create_workflow",
            "read_run_output",
        ] {
            assert!(
                !delegating.contains(&authority.to_string()),
                "a desk member must NOT receive orchestrator authority `{authority}`: \
                 {delegating:?}"
            );
        }
    }

    /// (b3) Issue #176: the wiring is inert for an agent that named no
    /// allowlist, and the orchestrator's own belt is untouched by the feature.
    ///
    /// The empty-allowlist half is what makes #176 a no-op for every manifest
    /// written before it; the orchestrator half is what proves the `else if`
    /// really is exclusive, since a second narrowed `delegate_to_desk` beside
    /// the orchestrator's unrestricted one would put two tools of the same name
    /// on one belt.
    #[test]
    fn an_empty_allowlist_wires_nothing_and_the_orchestrator_belt_is_unchanged() {
        assert_eq!(
            built_tool_names_delegating(&["*"], false, &[]),
            built_tool_names(&["*"], false),
            "an empty `delegates_to` must produce the pre-#176 belt byte-for-byte"
        );

        let orchestrator = built_tool_names(&["*"], true);
        assert_eq!(
            built_tool_names_delegating(&["*"], true, &["research"]),
            orchestrator,
            "an orchestrator's belt must not change when it also names `delegates_to`"
        );
        assert_eq!(
            orchestrator
                .iter()
                .filter(|t| *t == "delegate_to_desk")
                .count(),
            1,
            "exactly one `delegate_to_desk` may be wired: {orchestrator:?}"
        );
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
            // destructive memory: upstream's raw `forget` stays out; the
            // scoped oc-authored `memory_forget` is a real belt tool now.
            "forget",
        ];
        for tool in forbidden {
            assert!(
                !names.contains(&tool.to_string()),
                "deferred tool `{tool}` leaked into the dispatched belt: {names:?}"
            );
        }
    }

    /// [`pin_deps`] with automatic Git checkpoints enabled — the one switch
    /// `HarnessDeps::workspace_git_enabled` flips inside `build_agent`.
    fn enabled_git_deps(root: std::path::PathBuf) -> HarnessDeps {
        let mut deps = pin_deps(root);
        deps.workspace_git_enabled = true;
        deps
    }

    /// `git log` inside the workspace, resolving through the checkpointer's
    /// `.git` pointer, exactly as the existing checkpoint tests do.
    fn git_log(workspace: &std::path::Path) -> String {
        String::from_utf8(
            std::process::Command::new("git")
                .args(["log", "--format=%s"])
                .current_dir(workspace)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
    }

    /// The enabled Git path, end to end through `build_agent`: the `docs.*`
    /// grant wires the sandboxed `file_write`, `workspace_git_enabled: true`
    /// decorates every tool with the checkpointer, and a tool call that writes
    /// the workspace yields the baseline commit plus a post-call checkpoint.
    #[tokio::test]
    async fn workspace_git_enabled_checkpoints_a_tool_write() {
        use crate::company::Policy;
        use serde_json::json;

        let dir = tempfile::tempdir().expect("tempdir");
        let deps = enabled_git_deps(dir.path().to_path_buf());
        let company = CompanyId::new("acme");
        let manifest_agent = ManifestAgent {
            global: false,
            id: "desk".to_string(),
            role: "Desk Lead".to_string(),
            name: None,
            description: None,
            tier: None,
            harness: None,
            tools: None,
            delegates_to: Vec::new(),
            context: None,
            budget_usd_daily: None,
            prompt: None,
            prompt_files: Vec::new(),
            prompt_files_resolved: Vec::new(),
            classes: Vec::new(),
            ledgers: None,
            can_declare_ledgers: true,
            model: None,
        };
        // `full` so the sandboxed write executes without a supervised prompt.
        let policy = ApprovalPolicy::new(
            &Policy {
                mode: "full".to_string(),
                ..Policy::default()
            },
            None,
        );
        let grants = vec!["docs.*".to_string()];
        let agent = build_agent(
            &company,
            "Acme",
            &manifest_agent,
            policy,
            &deps,
            &grants,
            &[],
            &[],
            None,
            false,
        )
        .expect("agent builds");

        let workspace = agent_workspace(&deps.workspace_root, &company, "desk");
        let write = agent
            .tools()
            .iter()
            .find(|tool| tool.name() == "file_write")
            .expect("docs.* wires file_write");
        let result = write
            .execute(json!({"path": "answer.txt", "content": "42"}))
            .await
            .expect("file_write runs");
        assert!(!result.is_error, "unexpected failure: {result:?}");
        assert_eq!(
            std::fs::read_to_string(workspace.join("answer.txt")).unwrap(),
            "42"
        );

        let history = git_log(&workspace);
        assert!(
            history.contains("checkpoint: initialize workspace"),
            "{history}"
        );
        assert!(
            history.contains("checkpoint: after file_write"),
            "{history}"
        );
    }
}
