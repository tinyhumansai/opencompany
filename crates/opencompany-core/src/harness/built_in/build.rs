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
//!   The three **hand-off** tools, `spawn_task`, `delegate_to_desk` and
//!   `delegate_to_teammate`, are wired onto every other roster agent too,
//!   scoped by its manifest [`delegates_to`](crate::company::Agent::delegates_to):
//!   unrestricted when the list is empty, narrowed to the named desks when it
//!   is not. Every agent is also briefed on its team
//!   (`company::team_brief::team_section`) so it knows who those tools reach.
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

use openhuman_core as oh;

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
use crate::company::inference::store as inference_store;
use crate::error::OpenCompanyError;
use crate::harness::HarnessDeps;
use crate::harness::built_in::provider::HarnessModel;
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

/// What an `@` in an agent's own reply does — and does not — do.
///
/// Every agent gets this, because every agent can write one. `Mention::quiet`
/// is the contract it states in prose: "draw the chip, but do not notify and
/// do not route". Without it an agent reaches for `@name` to make somebody
/// pick something up, the chip renders, nothing happens, and the work is
/// silently dropped — the failure is invisible precisely because the message
/// LOOKS like a hand-off.
///
/// Deliberately does not name the hand-off tools: most agents do not have
/// them, and pointing an agent at a tool it was not granted is the "a tool
/// granted, unmentioned" problem pointed the other way. The agents that do
/// have them are told in [`orchestrator::orchestrator_brief`].
const MENTION_BRIEF: &str = " Naming a teammate: write their name or id as ordinary text when you are \
referring to them — \"qa_engineer has the failing case\". An `@` in your reply renders a chip and \
nothing more: it notifies nobody and starts no work, so it cannot hand anything over. Reaching for \
`@` to make somebody pick something up does not make them pick it up. ";

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
//
// Returns the [`Agent`] alongside the [`HarnessModel`] it was actually wired
// to — `deps.provider` for an unpinned agent, or the per-agent
// [`TenantProvider`](crate::harness::built_in::provider::TenantProvider)
// [`pinned`](HarnessModel::pinned) minted when the manifest names a
// `{provider, model}` pair (issue #2306 / Codex round 2, comment 4012457318).
// A pinned agent's own `TenantProvider` carries telemetry cells the shared
// `deps.provider` never sees, so metering a pinned turn from `deps.provider`
// silently books it to the company default. The caller that goes on to meter
// this agent's turns must keep this exact `Arc` — not re-resolve the pin —
// so it reads the same telemetry cells the turn actually wrote.
#[allow(clippy::too_many_arguments)]
pub fn build_agent_with_model(
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
    // The `## Your team` section for this agent, pre-rendered by the caller
    // with `company::team_brief::team_section` over the live record, or `""`
    // for a roster of one. A string rather than the record itself on the
    // `is_orchestrator` precedent: this function builds one agent from parts
    // the caller decided, and the roster is one of them.
    team_section: &str,
    // Whether this company's `[speech]` block turns talking into a tool call.
    //
    // A `bool` resolved by the caller rather than a `&CompanyManifest` read
    // here, on exactly the precedent `is_orchestrator` above sets: this
    // function builds one agent from parts its caller has already decided, and
    // handing it the whole manifest so it could re-derive one flag would give
    // it a second, drifting opinion about the company.
    speech_enabled: bool,
) -> crate::Result<(Agent, Arc<dyn HarnessModel>)> {
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
    // Issue #1890 F: reading another thread of the channel this turn is in.
    //
    // On **every** roster agent's belt, not just the orchestrator's — the agent
    // that needs it is the one answering in the channel, and gating it on
    // delegation grants would leave a desk lead able to see #1890 E's thread
    // index and unable to follow any of it.
    //
    // Intrinsic on the same terms as the approval tool above: it reads this
    // company's own journal, scoped at call time to the conversation the turn
    // is in, so there is no grant for it to be covered by.
    if let Some(events) = deps.events.clone() {
        tools.push(Box::new(crate::harness::thread_tools::ReadThreadTool::new(
            company.clone(),
            events,
            deps.store.clone(),
        )));
    }
    // Talking as a tool call (`[speech] enabled`). On unless the manifest opts
    // out, and on every roster agent's belt when enabled — speaking is not a
    // capability one teammate has and another does not, so there is no grant
    // for it to be scoped by, exactly as with the two intrinsic tools above.
    //
    // Needs the journal: these tools ARE the append, so without an `EventLog`
    // there is nothing for them to do and registering them would advertise a
    // voice the host cannot give. A company in that configuration keeps the
    // return-text path, which is the same fallback an un-called tool gets.
    // `speech_enabled` is the resolved default-on/opt-out value; whether the
    // tools actually got wired also needs a journal to append to (the comment
    // above). The persona brief below must agree with THIS — the AND, not the
    // flag alone — or a company with no `EventLog` gets a brief instructing it
    // to call tools that were never registered.
    let speech_wired = speech_enabled && deps.events.is_some();
    if speech_wired && let Some(events) = deps.events.clone() {
        tools.extend(crate::harness::speech_tools::speech_belt(
            crate::harness::speech_tools::SpeechContext::new(
                company.clone(),
                manifest_agent.id.clone(),
                events,
                deps.store.clone(),
            )
            .with_dispatch(deps.delegations.clone()),
        ));
    }
    // Installed-MCP-registry surface (`mcp_registry_list_tools` /
    // `mcp_registry_tool_call`) — distinct from the per-server `mcp:<name>`
    // bridge below, and reaching further: `mcp_registry_tool_call` invokes an
    // arbitrary tool on ANY server the company has installed and connected,
    // addressed at call time by a bare `server_id` argument, with none of the
    // bridge's per-server grant scoping. Two hard gates before either tool is
    // wired, following the `composio`/`media`/`search` precedent above:
    //
    //  1. an **EXPLICIT** `mcp_registry` grant
    //     (`grants_mcp_registry_explicit`) — the catch-all `*` does NOT confer
    //     it, for the same reason it does not confer `composio`: this reaches
    //     third-party servers and can mutate them, so a broadly-permissioned
    //     company must still opt in by name.
    //  2. a configured registry store (`deps.mcp_home`) — the store an install
    //     writes through. Granted-but-unconfigured wires nothing and warns
    //     (fail-closed), matching every other explicit namespace in this file.
    //
    // `mcp_registry_list_tools` (read-only schema discovery over the same
    // registry) rides the SAME grant as the mutating call tool rather than a
    // narrower one of its own — see `grants_mcp_registry_explicit`'s doc
    // comment for why: every other third-party-reaching family already bundles
    // its read-only discovery tools under the one grant that covers the
    // mutating ones, and OpenHuman's own tool description frames the two as a
    // single discover-then-call workflow.
    #[cfg(feature = "mcp")]
    if crate::company::grants_mcp_registry_explicit(grants) {
        match deps.mcp_home.clone() {
            Some(mcp_home) => {
                let config =
                    std::sync::Arc::new(crate::harness::mcp::McpRuntime::config_for(mcp_home));
                tools.push(Box::new(
                    oh::mcp::registry::tools::McpRegistryListToolsTool::new(config.clone()),
                ));
                tools.push(Box::new(
                    oh::mcp::registry::tools::McpRegistryToolCallTool::new(config),
                ));
            }
            None => tracing::warn!(
                company = %company,
                agent = %manifest_agent.id,
                "[build] agent explicitly grants `mcp_registry` but no MCP registry home is \
                 configured; mcp_registry tools NOT wired (fail-closed)"
            ),
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
    //     `composio/tinyhumans/key` override, else the company's own TinyHumans key, else
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
                // *resolver* outcome over three tiers (BYO `composio/tinyhumans/key`,
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
    //     from the company's copied TinyHumans key first and the deployment
    //     credential second.
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
                deps.pending_publishes.output_collector(),
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
    // The roster and desks, and who this agent may hand work to — rendered by
    // the caller from the live company record, which is the only place the
    // effective roster (overlay teammates and desks included) is known. Static
    // for the life of the belt: the belt is rebuilt when the roster changes.
    persona.push_str(team_section);

    // Every agent, granted tools or not: an `@` is something any of them can
    // write, and what it does is not guessable from the fact that it renders.
    persona.push_str(MENTION_BRIEF);

    // How this company talks, when it talks by calling a tool.
    //
    // Placed high, beside the mention brief, because it is a rule about every
    // reply rather than a note about one namespace — and because the failure it
    // prevents is silent: an agent that never learns about `desk_post` just
    // answers in text, the reply path journals it, and nothing reports that the
    // feature did nothing. Gated on the same condition that wired the tools
    // (`speech_wired`, not the bare `speech_enabled` flag — Codex/CodeRabbit),
    // so the brief can never describe a voice this agent was not given.
    if speech_wired {
        persona.push_str(&crate::harness::speech_tools::speech_brief());
    }

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

    // Name public-web fetch and discovery separately. A broad `web.*` grant
    // wires URL readers, while `web_search` also needs the explicit priced
    // `search` grant and a live provider credential. Research agents must know
    // which half they actually have or they fall back to rereading unrelated
    // workspace/ledger state until the loop guard stops them.
    let web_fetch_wired = wants_web && !toolbelt::namespace_denied(&deps.capabilities, "web");
    let web_search_wired = tools
        .iter()
        .any(|tool| tool.name() == crate::harness::search::WEB_SEARCH_TOOL)
        && !toolbelt::namespace_denied(&deps.capabilities, "search");
    persona.push_str(&toolbelt::web_brief(web_fetch_wired, web_search_wired));

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
    //
    // The native side of that same check was missed here: `tools` above is
    // still the pre-filter belt (`filter_by_capabilities` does not run until
    // below), so a tier denying e.g. `search` while admitting `composio`
    // rendered a brief claiming the built-in search tool as a Composio
    // fallback reason — a tool absent from this agent's actual final belt.
    // [`toolbelt::native_caps_for_composio_brief`] applies the same
    // `namespace_denied` check `filter_by_capabilities` is about to apply per
    // tool, so this stays in lockstep with the filter below without needing
    // the already-filtered belt in hand.
    #[cfg(feature = "composio")]
    if toolbelt::composio_capability_admits(composio_toolkits.is_some(), &deps.capabilities)
        && let Some(toolkits) = composio_toolkits.as_deref()
    {
        let native_caps = toolbelt::native_caps_for_composio_brief(&tools, &deps.capabilities);
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
            // Issue #1859: the board + run-history read surface `list_tasks` /
            // `read_task` / `read_run` need, and `query_company`'s `## Board`
            // section reads `tasks` too.
            deps.tasks.clone(),
            deps.workflow_runs.clone(),
            deps.artifacts.clone(),
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
    // Every OTHER roster agent gets the three hand-off tools — `spawn_task`,
    // `delegate_to_desk` and `delegate_to_teammate` — and the brief that goes
    // with them, whatever its manifest entry says. Issue #176 wired these only
    // onto a member that opted in with a `delegates_to` allowlist, so a desk
    // lead or a specialist with no such line had no way to reach the colleague
    // sitting beside it, and no way to track anything either; the runtime
    // covered the second gap by carding every message for it, which is how
    // the board filled with cards nobody asked for. Now the reach is decided
    // by the list (empty = everyone, see
    // `delegation_tools::reach_is_unrestricted`) and the tool is always there.
    //
    // `else`, not a second `if`: the orchestrator already has all three from
    // `orchestrator_tools` above, and wiring a second, scoped copy beside its
    // unrestricted one would put two tools with the same name on one belt.
    else {
        persona.push_str(&orchestrator::member_delegation_brief());
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
    );

    let model = deps
        .model_override
        .clone()
        .unwrap_or_else(|| model_for_tier(manifest_agent.tier.as_deref()));

    // Keys rework slice 3a (issue #2306): this agent's own `{provider, model}`
    // pair, when it has one. Only `built_in` lanes reach `build_agent`, and
    // `CompanyManifest::validate` refuses a pair on an `acp` agent, so no
    // harness-kind check is needed here. `deps.provider.pinned` returns
    // `None` for an implementation that cannot pin (test doubles) — falling
    // back to the un-pinned provider rather than failing the whole roster
    // build over a fixture that predates this field.
    let pin = match (
        manifest_agent.provider.as_deref(),
        manifest_agent.model.as_deref(),
    ) {
        (Some(provider), Some(model))
            if !provider.trim().is_empty() && !model.trim().is_empty() =>
        {
            Some(inference_store::ModelChoice {
                provider: provider.trim().to_string(),
                model: model.trim().to_string(),
            })
        }
        _ => None,
    };
    // `Some` only when `pin` names a pair AND `deps.provider` can actually
    // mint a sibling for it — never a synthetic pin that turns out to be
    // `deps.provider` itself. Auxiliary per-agent passes (issue #2306, X12;
    // round-2 review comment 4012457329) key their own default-first
    // fallback on this being a genuinely distinct provider — see
    // [`crate::harness::built_in::pass_model`].
    let pinned_model: Option<Arc<dyn HarnessModel>> = pin.as_ref().and_then(|choice| {
        deps.provider.pinned(
            &manifest_agent.id,
            manifest_agent
                .name
                .as_deref()
                .unwrap_or(manifest_agent.role.as_str()),
            choice,
        )
    });
    if pin.is_some() && pinned_model.is_none() {
        tracing::warn!(
            agent = %manifest_agent.id,
            "this provider cannot pin; the agent pair is ignored"
        );
    }
    // The primary chat model: the agent's own pin, fails closed on its own
    // terms (X8/F6) rather than falling back to `deps.provider` — the
    // `unwrap_or_else` below only covers the "this provider cannot pin"
    // case above, never a *working* pin that later fails at turn time.
    let chat_model: Arc<dyn HarnessModel> = pinned_model
        .clone()
        .unwrap_or_else(|| deps.provider.clone());

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
    let native_tools = chat_model
        .profile()
        .map(|profile| profile.tool_calling)
        .unwrap_or(false);
    let tool_dispatcher: Box<dyn ToolDispatcher> = if native_tools {
        Box::new(NativeToolDispatcher)
    } else {
        Box::new(AttrTolerantXmlDispatcher::default())
    };

    // OpenCompany has already constructed and narrowed the real tool vector.
    // Select a disclosure identity that preserves those host-owned tools:
    // workflow_builder owns both the workflow and Composio packs. Using only
    // integrations_agent hid create_workflow/run_workflow from our coordinator.
    #[cfg(feature = "composio")]
    let has_composio = composio_toolkits.is_some();
    #[cfg(not(feature = "composio"))]
    let has_composio = false;
    let agent_definition_name =
        host_toolpack_identity(manifest_agent.id.as_str(), is_orchestrator, has_composio);

    super::tool_posture::declare();
    let mut agent = AgentBuilder::default()
        // `HarnessModel` upcasts to the tinyinference `ChatModel<()>` the builder's
        // native injection seam takes (the old `Provider` adapter is gone).
        .chat_model(chat_model.clone() as Arc<dyn tinyinference::model::ChatModel<()>>)
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
        // Issue #6014: extract the answering content from an oversized tool
        // result instead of cutting it on a byte boundary.
        //
        // `ContextConfig` has carried the threshold this fires at
        // (`summarizer_payload_threshold_tokens`, 4000) since before this crate
        // existed, and `ToolOutputMiddleware` has consulted it on every tool
        // result — but the builder defaults the summarizer itself to `None`, so
        // the whole path was inert here and the byte cut was the only thing
        // bounding a large payload. That cut keeps whatever came first: a
        // thirty-issue listing reached the model as two issues, and the agent
        // reported two.
        //
        // The upstream implementation dispatches a sub-agent, which this crate
        // cannot use (see `toolbelt`'s v1 note on spawn tools under
        // multi-tenancy), so `PayloadExtractor` serves the same trait with one
        // bounded model call — built `from_deps` like every other one-shot pass
        // here, so it spends the company's own credential and meters against it.
        // `pinned_model.clone()` (issue #2306, X12; round-2 review comment
        // 4012457329) so this agent's own pair is reachable when the company
        // default cannot serve the call, instead of always failing extraction
        // for a company configured solely through agent pins.
        .payload_summarizer(std::sync::Arc::new(
            crate::harness::payload_extract::PayloadExtractor::from_deps(
                deps,
                company,
                pinned_model.clone(),
            ),
        ))
        .model_name(model)
        .workspace_dir(workspace)
        // One teammate, one *named* openhuman session.
        //
        // The builder defaults this pair to `("standalone", "internal")`, and
        // this crate never set it — so every agent of every company on the
        // process published `AgentTurnStarted`, `AgentTurnCompleted`,
        // `AgentError` and its prompt-enforcement context under one shared
        // session id. That was survivable only because one turn ran at a time.
        // openhuman's library host now runs many sessions over one core
        // concurrently, and an unlabelled event stream is exactly what stops
        // being readable when turns overlap.
        //
        // See [`session_key`](crate::harness::session_key) for the shape and
        // for why it must be a pure function of the two ids rather than
        // anything a roster rebuild disturbs.
        .event_context(
            crate::harness::session_key::openhuman_session_key(company, &manifest_agent.id),
            crate::harness::session_key::SESSION_CHANNEL,
        )
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
    Ok((agent, chat_model))
}

/// [`build_agent_with_model`], discarding the [`HarnessModel`] it resolved.
///
/// For every caller that only wants the [`Agent`] — every test in this crate
/// that builds one to exercise its tools/prompt/policy, plus any future
/// non-metering caller. The roster construction path that meters this agent's
/// turns must call [`build_agent_with_model`] directly and keep the model it
/// returns; see that function's doc comment for why the pinned `Arc` cannot be
/// re-resolved after the fact.
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
    speech_enabled: bool,
) -> crate::Result<Agent> {
    build_agent_with_model(
        company,
        company_name,
        manifest_agent,
        policy,
        deps,
        grants,
        skill_deltas,
        routed_context,
        instructions,
        is_orchestrator,
        // Test-only wrapper; the roster section is the caller's to render, and
        // every caller of this wrapper is exercising something else.
        "",
        speech_enabled,
    )
    .map(|(agent, _chat_model)| agent)
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
/// directory found `law_firm/page_builder/` next to a note tree whose
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
/// (`law_firm/page_builder/`), and an agent upgraded into the new
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
#[path = "build_tests.rs"]
mod tests;

/// Select a definition label whose pack disclosure preserves host-granted tools.
/// This also changes OpenHuman transcript labels; it does not change grants.
fn host_toolpack_identity(agent_id: &str, orchestrator: bool, composio: bool) -> &str {
    if orchestrator {
        "workflow_builder"
    } else if composio {
        "integrations_agent"
    } else {
        agent_id
    }
}

#[cfg(test)]
#[path = "host_toolpack_tests.rs"]
mod host_toolpack_tests;
