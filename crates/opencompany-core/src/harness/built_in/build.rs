//! Manifest `[[agent]]` → the [`AgentBlueprint`] one company agent is
//! instantiated from on the process-wide OpenHuman runtime.
//!
//! [`build_agent_with_model`] turns one roster entry into everything a turn
//! needs: the system prompt (persona, bundle, team, briefs, skills catalogue,
//! sandbox brief), the assembled toolbelt, the inference model the turn runs
//! against, the [`ApprovalPolicy`] and the agent's workspace directory.
//! [`agent_spec_for`] then renders that into an [`AgentSpec`] for
//! [`Runtime::agent`](openhuman_embed::Runtime::agent) (plan hive-desks,
//! Phase 2).
//!
//! **Which tools reach the model.** OpenHuman's embed facade takes its tool set
//! from its own registry, scoped per agent by name (`ToolScopeSpec::Named`),
//! plus MCP servers; there is no seam for an in-process `Tool` a host built.
//! So of the belt assembled here, the OpenHuman-native tools (`shell`,
//! `file_read`, `web_fetch`, …) are named in the spec's tool scope and reach
//! the model directly, while this crate's own tools — ledger, tasks, pages,
//! workspace, composio, hosting, memory, speech, approval — are carried on
//! the blueprint **unattached**: they become the catalogue the per-agent MCP
//! server serves in Phase 3. Until then a turn has exactly the native subset.
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
//! Tool-call parsing is OpenHuman's: the embedded turn loop reads native
//! `tool_calls` off the wire and falls back to its own text-tag parser, so the
//! attribute-tolerant XML dispatcher this crate carried (issue #105) went with
//! `AgentBuilder`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use openhuman_core as oh;

use oh::security::SecurityPolicy;
#[cfg(feature = "mcp")]
use oh::tools::McpListToolsTool;
use oh::tools::{EditFileTool, FileReadTool, FileWriteTool, GlobTool, GrepTool, ListFilesTool};
use openhuman_embed::{Access, AgentDefinitionSpec, AgentSpec, ToolScopeSpec};
use tinytools::Tool;

use crate::company::Agent as ManifestAgent;
use crate::company::inference::store as inference_store;
use crate::harness::HarnessDeps;
use crate::harness::built_in::provider::HarnessModel;
use crate::harness::file_tool_outputs::WritePromotion;
#[cfg(feature = "mcp")]
use crate::harness::mcp::{
    OcMcpCallTool, OcMcpListServersTool, OcMcpRegistryInstalledListTool, OcMcpRegistryScopedTool,
    capability_brief, granted_policies, granted_secrets, registry_for_agent,
};
use crate::harness::orchestrator;
use crate::harness::policy::ApprovalPolicy;
use crate::harness::skills::EffectiveSkills;
use crate::harness::toolbelt;
use crate::hive::mcp_server::{McpAttach, attach_opencompany_mcp};
use crate::ports::skills_state::SkillState;
use crate::ports::types::CompanyId;
use crate::runtime::tools::{NAMESPACE_SEPARATORS, extends_on_boundary};

/// The per-tool-result byte budget every OpenCompany agent runs under.
///
/// The harness cuts **every** tool result to this many bytes on its way into
/// the model's context — `ToolOutputMiddleware`, fed from
/// `ContextManager::tool_result_budget_bytes`, the vendored default every
/// embedded turn runs under. It is the real ceiling on what a
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
const MENTION_BRIEF: &str = " Naming a teammate: write their name as ordinary text when you are \
referring to them: \"Quinn has the failing case\". An `@` in your reply renders a chip and \
nothing more: it notifies nobody and starts no work, so it cannot hand anything over. Reaching for \
`@` to make somebody pick something up does not make them pick it up. ";

/// Who reads what an agent writes, and what belongs in a tool call instead.
///
/// Every agent, pooled or seated: a reply, a desk post and a relayed answer
/// all land in front of a person. States the audience and the ordering, not a
/// length budget; OpenHuman's own style rules own tone.
pub(crate) const READER_BRIEF: &str = " A person reads what you post. Lead with the answer in \
plain words, usually a few short sentences; offer detail rather than dump it. Refer to teammates, \
desks and work by name. Ids, tool names, card, run and sequence numbers, and JSON belong in tool \
calls, never in what you write. ";

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
    policy: Arc<ApprovalPolicy>,
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
) -> crate::Result<AgentBlueprint> {
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

    // The company's own granted MCP servers, attached directly to the
    // `AgentSpec` in `agent_spec_for` (plan hive-desks Phase 2 follow-up) —
    // see `embed_servers_for_agent`'s doc comment for why this exists
    // alongside (not instead of) `registry_for_agent` below.
    #[cfg(feature = "mcp")]
    let mut company_mcp_servers: Vec<openhuman_embed::McpServer> = Vec::new();

    // Deliberate-memory tools, oc-authored over this company's own context
    // port — see `memory_tools`'s doc comment for why not the vendored ones.
    let mut tools: Vec<Box<dyn Tool>> = memory_tools(deps, company, &manifest_agent.id);
    // Belt tools this agent keeps but is not offered — see AgentBlueprint::unadvertised.
    let mut unadvertised: Vec<String> = Vec::new();
    // Approvals are an explicit agent action, not a policy side effect. Every
    // roster agent gets this intrinsic tool regardless of external grants.
    tools.push(Box::new(
        crate::harness::approval_tool::RequestApprovalTool::new(
            manifest_agent.id.clone(),
            deps.approval_requests.clone(),
        ),
    ));
    // Speaking — `post`, `broadcast`, `dm`, `complete_episode`, `read` — and
    // reading another thread are served to every agent by the `opencompany`
    // MCP server (plan hive-desks, Phases 3-4), not by tools on this belt:
    // `openhuman_embed::Agent` has no seam for an in-process host tool, and
    // a seat's one utterance per turn is attributed to its round by the
    // in-flight registry the server reads, which a belt tool cannot reach.
    // Installed-MCP-registry surface (`mcp_registry_list_tools` /
    // `mcp_registry_tool_call`) — distinct from the per-server `mcp:<name>`
    // bridge below: both tools address an install at call time by a `server_id`
    // argument rather than by the grant they were wired under. Both are
    // therefore wrapped in `OcMcpRegistryScopedTool`, which resolves that
    // argument against the agent's grants (`grants_cover_registry_server`)
    // before delegating, so a scoped `mcp_registry.<server_id>` grant reaches
    // one install and a bare `mcp_registry` grant reaches all of them. The same
    // decorator reads that install's stored tool policy, so a blocked tool is
    // refused at call time. Two hard gates before either tool is wired,
    // following the
    // `composio`/`media`/`search` precedent above:
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
                let config = std::sync::Arc::new(crate::harness::mcp::McpRuntime::config_for(
                    mcp_home.clone(),
                ));
                // Enumeration, so the two tools below have a `server_id` to
                // name. OpenHuman's own answer to this question carries the
                // dial string and the install's config blob, so this is our own
                // tool rather than a decorator over it.
                tools.push(Box::new(OcMcpRegistryInstalledListTool::new(
                    std::sync::Arc::new(crate::harness::mcp::McpRuntime::new(mcp_home)),
                    grants.to_vec(),
                )));
                tools.push(Box::new(OcMcpRegistryScopedTool::new(
                    Box::new(oh::mcp::registry::tools::McpRegistryListToolsTool::new(
                        config.clone(),
                    )),
                    grants.to_vec(),
                    company.clone(),
                    deps.secrets.clone(),
                )));
                tools.push(Box::new(OcMcpRegistryScopedTool::new(
                    Box::new(oh::mcp::registry::tools::McpRegistryToolCallTool::new(
                        config,
                    )),
                    grants.to_vec(),
                    company.clone(),
                    deps.secrets.clone(),
                )));
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
        let promotion = deps.workspace.as_ref().map(|store| {
            Arc::new(WritePromotion::new(
                store.clone(),
                company.clone(),
                manifest_agent.id.clone(),
                deps.pending_publishes.output_collector(),
                workspace.clone(),
            ))
        });
        tools.extend(file_tools(&workspace, promotion));
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
    let agent_label = manifest_agent
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(manifest_agent.role.trim())
        .to_string();
    tools.push(Box::new(
        crate::harness::built_in::blockers::EscalateToHumanTool::new(
            deps.approval_requests.clone(),
            manifest_agent.id.clone(),
            agent_label,
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
    persona.push_str(READER_BRIEF);

    // How this company talks, when it talks by calling a tool.
    //
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
            &manifest_agent.id,
            manifest_agent.skills.as_deref(),
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
        // Reaches the model natively: `agent_spec_for` attaches each of these
        // to the `AgentSpec` via `AgentSpec::mcp`, alongside the internal
        // `opencompany` server, so OpenHuman's own `mcp_call_tool` /
        // `mcp_list_servers` / `mcp_list_tools` — the only implementations of
        // those names that actually run for a company agent now — can reach
        // this company's own registered servers by name. See
        // `embed_servers_for_agent`'s doc comment for the full story.
        company_mcp_servers =
            crate::harness::mcp::embed_servers_for_agent(&deps.mcp_servers, grants);
        let mcp_security = Arc::new(SecurityPolicy::default());
        // The known-secret set for the scrubber: every credential the agent's
        // granted servers carry, so no configured token can leak into an
        // agent-visible MCP error (the error-hardening cell). Use the same
        // effective grants that selected `registry`, not the raw manifest
        // request: an empty request inherits the company belt and can therefore
        // reach servers even when `manifest_agent.tools` is empty.
        let secrets = granted_secrets(&deps.mcp_servers, grants);
        let mcp_policies = granted_policies(&deps.mcp_servers, grants);
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
            mcp_policies,
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
        // **Kept on the belt, withheld from the model.**
        //
        // `ask` replaced these for a teammate, and they were never honestly
        // available to one: a hive seat has them stripped per turn
        // (`EPISODE_WITHHELD_TOOLS`), because the queue they fill is drained
        // by a brain that does not run inside an episode — while the prompt
        // went on naming them. Observed live: a copywriter read "Never tell
        // anyone a teammate is out of reach — you can, with
        // `delegate_to_teammate`" on a turn where the tool was not on its
        // belt, and answered by broadcasting a hand-off that transferred
        // nothing to a teammate that never ran.
        //
        // The orchestrator and workflow nodes are untouched: they delegate by
        // design, and this is the member branch.
        unadvertised.push(crate::runtime::delegation_tools::DELEGATE_TO_DESK_TOOL.to_owned());
        unadvertised.push(crate::runtime::delegation_tools::DELEGATE_TO_TEAMMATE_TOOL.to_owned());
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

    let model = deps
        .model_override
        .clone()
        .unwrap_or_else(|| model_for_tier(manifest_agent.tier.as_deref()));

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
    let chat_model: Arc<dyn HarnessModel> = pinned_model
        .clone()
        .unwrap_or_else(|| deps.provider.clone());

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

    #[cfg(feature = "composio")]
    let definition_name = if composio_toolkits.is_some() {
        "integrations_agent".to_string()
    } else {
        manifest_agent.id.clone()
    };
    #[cfg(not(feature = "composio"))]
    let definition_name = manifest_agent.id.clone();

    super::tool_posture::declare();
    let native_tool_names = native_tool_names(&tools);
    Ok(AgentBlueprint {
        system_prompt: persona,
        tools,
        native_tool_names,
        unadvertised,
        #[cfg(feature = "mcp")]
        company_mcp_servers,
        chat_model,
        model,
        workspace,
        policy,
        definition_name,
    })
}

/// Everything [`build_agent_with_model`] assembles for one company agent,
/// before it is registered on the runtime.
///
/// Held apart from the [`AgentSpec`] because the spec is consumed by
/// [`Runtime::agent`](openhuman_embed::Runtime::agent) while the pool keeps
/// needing the rest: the model for metering and auxiliary passes, the belt
/// for step labels and — in Phase 3 — the MCP catalogue, the policy for the
/// approval decision that catalogue's handler makes.
pub struct AgentBlueprint {
    /// The full system prompt: persona, bundle, team, briefs, skills
    /// catalogue, sandbox brief, routed context.
    pub system_prompt: String,
    /// The assembled toolbelt. Only the OpenHuman-native subset (see
    /// [`native_tool_names`](Self::native_tool_names)) reaches the model in
    /// this phase; the rest is the Phase 3 MCP catalogue.
    pub tools: Vec<Box<dyn Tool>>,
    /// The belt's OpenHuman-native tool names — the spec's `ToolScopeSpec`.
    pub native_tool_names: Vec<String>,
    /// Belt tools this agent keeps but is **not** offered.
    ///
    /// `ToolScopeSpec::Named` is the advertisement, so a name left off it is
    /// dropped before the model sees it — the tool, its queue and its drain
    /// are untouched, and any caller that still reaches for one works as
    /// before.
    ///
    /// Carried per agent rather than as a constant because the answer is not
    /// the same for everyone: the orchestrator and a workflow agent node
    /// delegate as a matter of design, and a workflow run that could not hand
    /// work on would lose the notice that says so
    /// (`an_ungrounded_hand_off_surfaces_on_the_runs_own_notices`).
    pub unadvertised: Vec<String>,
    /// This agent's own granted MCP servers (issue: company servers were
    /// unreachable once the native-dispatch builder was removed — see
    /// `embed_servers_for_agent`'s doc comment), attached directly to the
    /// `AgentSpec` in [`agent_spec_for`] alongside the internal `opencompany`
    /// server.
    #[cfg(feature = "mcp")]
    pub company_mcp_servers: Vec<openhuman_embed::McpServer>,
    /// The inference model the turn runs against (the company default or
    /// this agent's own pin).
    pub chat_model: Arc<dyn HarnessModel>,
    /// The model name every request carries (`chat-v1`, an override, …).
    pub model: String,
    /// The agent's own workspace directory (file tools sandbox / turn cwd).
    pub workspace: PathBuf,
    /// The approval policy the (Phase 3) tool handler decides under.
    /// Shared, because both consumers need their own handle: the MCP server
    /// admits speech calls against it, and it rides the native belt as that
    /// belt's gate (`agent_spec_for`). A `Box` here would force one of them
    /// to go without.
    pub policy: Arc<ApprovalPolicy>,
    /// The definition name the agent runs under — its manifest id, or
    /// `integrations_agent` when Composio toolkits are wired.
    pub definition_name: String,
}

impl std::fmt::Debug for AgentBlueprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentBlueprint")
            .field("model", &self.model)
            .field("tools", &self.tools.len())
            .field("native_tool_names", &self.native_tool_names)
            .field("workspace", &self.workspace)
            .finish_non_exhaustive()
    }
}

impl AgentBlueprint {
    /// The assembled belt.
    pub fn tools(&self) -> &[Box<dyn Tool>] {
        &self.tools
    }

    /// The tool-iteration cap the spec is rendered with.
    pub fn max_tool_iterations(&self) -> usize {
        MAX_TOOL_ITERATIONS
    }

    /// The belt's tool names, in belt order — what the previous builder's
    /// `Agent::tools()` listed, kept for the tests that pin a grant to the
    /// tools it wires.
    pub fn tool_names(&self) -> Vec<String> {
        self.tools
            .iter()
            .map(|tool| tool.name().to_string())
            .collect()
    }
}

/// The OpenHuman-native tools a company agent's belt may name in its tool
/// scope: the ones OpenHuman's own registry builds, which this crate wires
/// per grant (`files`, `shell`, `code`, `web`) and, under `mcp`, the static
/// MCP bridge tools.
///
/// The names are OpenHuman's; a belt entry outside this set is one of this
/// crate's own tools and is never advertised to the runtime, which would not
/// find it.
pub const OPENHUMAN_NATIVE_TOOLS: &[&str] = &[
    "shell",
    "read_workspace_state",
    "file_read",
    "file_write",
    "edit",
    "list",
    "grep",
    "glob",
    "apply_patch",
    "git_operations",
    "csv_export",
    "web_fetch",
    "http_request",
    "curl",
    "image_info",
    "mcp_list_servers",
    "mcp_list_tools",
    "mcp_call_tool",
];

/// The native subset of a belt, in belt order.
pub fn native_tool_names(tools: &[Box<dyn Tool>]) -> Vec<String> {
    tools
        .iter()
        .map(|tool| tool.name())
        .filter(|name| OPENHUMAN_NATIVE_TOOLS.contains(name))
        .map(str::to_string)
        .collect()
}

/// One teammate as a custom agent definition, for whichever hosted authority
/// is about to resolve it.
///
/// Every turn OpenHuman runs is a *hosted root invocation*: before composing
/// a message it resolves the agent's id against the host catalogue and
/// refuses the turn when the id is not there. This entry is how a teammate
/// declares itself, and both of this crate's agent shapes need one -- the
/// pooled [`AgentSpec`] agent, which carries the entry on the config the
/// embedded runtime threads through, and the
/// [`episode_seat`](episode_seat) session host, which projects it to an
/// `AgentDefinition` the session carries itself. One constructor so the two
/// cannot describe the same teammate differently.
///
/// `tools` is the authority, not a hint: the resolved definition's tool list
/// becomes the turn's allow-list, intersected with the belt the agent was
/// actually built with. Naming nothing denies everything rather than
/// allowing everything -- the hosted allow-list is fail-closed -- so this
/// always takes the whole belt.
#[cfg(feature = "openhuman")]
fn registry_entry(
    runtime_id: &str,
    definition_name: &str,
    system_prompt: &str,
    tools: Vec<String>,
) -> oh::agent::registry::AgentRegistryEntry {
    oh::agent::registry::AgentRegistryEntry {
        id: runtime_id.to_string(),
        name: definition_name.to_string(),
        description: format!("OpenCompany agent {definition_name}"),
        source: oh::agent::registry::AgentRegistrySource::Custom,
        enabled: true,
        // The blueprint's own model is already on the session or the spec;
        // a pin here would be a second opinion about the same turn.
        model: None,
        system_prompt: Some(system_prompt.to_string()),
        tool_allowlist: tools,
        tool_denylist: Vec::new(),
        subagents: oh::agent::registry::types::AgentSubagentPolicy::default(),
        tags: Vec::new(),
        metadata: serde_json::Value::Null,
    }
}

/// Renders a blueprint into the [`AgentSpec`] the runtime instantiates.
///
/// * `runtime_id` is [`runtime_agent_id`](crate::session_key::runtime_agent_id);
/// * `provider` is the route the turn runs on — the loopback
///   [`model_bridge`](crate::harness::model_bridge) over the blueprint's
///   [`chat_model`](AgentBlueprint::chat_model);
/// * the tool scope is the belt's native subset (an empty belt is an empty
///   scope, not a wildcard: a tool-less teammate must stay tool-less);
/// * `Access::full()` because the approval decision is this crate's
///   ([`ApprovalPolicy`]), enforced where its own tools run (Phase 3), not
///   OpenHuman's process-wide gate;
/// * `max_iterations` is [`MAX_TOOL_ITERATIONS`], unchanged;
/// * `action_dir` is the agent workspace, so a relative path in a tool call
///   resolves where the file tools were sandboxed;
/// * the same definition is ALSO declared as a custom
///   [`AgentRegistryEntry`](oh::agent::registry::AgentRegistryEntry) in the
///   agent's own config. Since OpenHuman 33566d38 ("run sessions through
///   TinyAgents runtime") every chat turn is a *hosted* invocation that
///   resolves its agent by id through the host definition registry — the
///   process-global registry plus the config's custom entries — and the
///   session's own definition is not consulted for that lookup, so a spec
///   without the entry fails every turn with "agent definition … was not
///   found". The tinyhivemind reference host registers its seats the same
///   way (`examples/openhuman/src/main.rs::registry_entry`).
pub fn agent_spec_for(
    blueprint: &AgentBlueprint,
    runtime_id: &str,
    provider: openhuman_embed::Provider,
    mcp: Option<&McpAttach>,
    belt: Option<&Arc<Vec<Arc<dyn Tool>>>>,
    gate: Option<&Arc<crate::harness::policy::ApprovalPolicy>>,
    seating: Option<&crate::hive::seating::EpisodeBelts>,
) -> AgentSpec {
    // **This crate's own tools are native.**
    //
    // They used to reach the model only over the `opencompany` MCP server,
    // because an `AgentSpec` had no seam for a host's own `dyn Tool` and MCP
    // was the one road a spec offered. The model paid for that road: a
    // discovery call to learn the catalogue, and an `mcp_call_tool` envelope
    // whose inner `arguments` object carries no schema a provider can validate
    // or constrain decoding against. Observed live, a teammate asked to
    // convene a room spent three calls guessing argument names before it got
    // one right, then a fourth on `mcp_list_tools` to read the schema it
    // should have been handed.
    //
    // `AgentSpec::tools` takes them directly now: own schema on the wire, own
    // name, validated arguments. The belt is shared and this factory mints
    // owned handles onto it per turn (`hive::shared_tool`), so nothing is
    // rebuilt.
    //
    // The MCP attachment stays for what MCP is actually for — the speech tools
    // an episode seat answers with, and any server an operator connected to
    // this company. A company with neither carries no bridge tools at all.
    let mut tool_names = blueprint.native_tool_names.clone();
    let mut system_prompt = blueprint.system_prompt.clone();
    if let Some(belt) = belt {
        for tool in belt.iter() {
            let name = tool.name().to_string();
            if blueprint.unadvertised.contains(&name) {
                continue;
            }
            if !tool_names.contains(&name) {
                tool_names.push(name);
            }
        }
    }
    // **The scope has to allow what a seated turn may carry.**
    //
    // `ToolScopeSpec::Named` is fixed when the agent is registered; the
    // episode's belt arrives per turn. A name the scope does not list is
    // dropped before the model sees it, so a seat was offered its teammate's
    // belt and told to reach the room over MCP -- the envelope this work
    // exists to remove, still there because the scope had never heard of
    // `desk_complete_episode`.
    //
    // Listing them here costs nothing on an ordinary turn: the belt factory
    // decides whether the tools exist at all, and the episode's own admission
    // gates them when they do. The scope only stops being a reason they
    // cannot.
    for speech in crate::hive::tools::served_speech_tool_names()
        .into_iter()
        .chain([crate::hive::takeover::TAKE_OVER_TOOL])
    {
        let prefixed = format!("{}{speech}", crate::hive::host::TOOL_PREFIX);
        if !tool_names.contains(&prefixed) {
            tool_names.push(prefixed);
        }
    }
    if let Some(mcp) = mcp {
        for bridge in ["mcp_list_tools", "mcp_call_tool"] {
            if !tool_names.iter().any(|name| name == bridge) {
                tool_names.push(bridge.to_string());
            }
        }
        system_prompt.push_str(&opencompany_mcp_brief(&mcp.allow_tools));
    }
    let entry = registry_entry(
        runtime_id,
        &blueprint.definition_name,
        &system_prompt,
        tool_names.clone(),
    );
    let mut spec = AgentSpec::new(runtime_id)
        .definition(
            AgentDefinitionSpec::new()
                .system_prompt(system_prompt)
                .display_name(blueprint.definition_name.clone())
                .tools(ToolScopeSpec::Named(tool_names))
                .max_iterations(MAX_TOOL_ITERATIONS),
        )
        .provider(provider)
        .access(Access::full());
    if let Some(belt) = belt {
        let belt = Arc::clone(belt);
        // **The gate travels with the belt.**
        //
        // While these tools were served over MCP, every call went through
        // `McpAgent::serve_call`, which checked the company's `ApprovalPolicy`
        // before dispatching. A native tool runs inside OpenHuman's own loop
        // and never reaches that handler, so a belt handed over without its
        // gate is a belt with no approval on it at all — the manifest
        // `[policy]`, the per-agent budget and the HITL parks all silently
        // stop applying.
        let gate = gate.map(Arc::clone);
        let seating = seating.cloned().unwrap_or_default();
        // Withheld from the model but kept on the belt. `ToolScopeSpec::Named`
        // is not enough on its own: a per-turn belt carries its own `visible`
        // set, and `HostTurnTools::advertised` fills that with *every* name it
        // holds — so a name dropped from the registration scope is advertised
        // again the moment the factory runs. Observed live: a member's pooled
        // turn was still offered both delegate verbs.
        let unadvertised = blueprint.unadvertised.clone();
        spec = spec.tools(move |turn| {
            let mut tools = crate::hive::shared_tool::owned_belt(&belt);
            // **A seated turn carries the episode's tools too.**
            //
            // The belt is composed per turn and the turn says which
            // conversation it is for, so an episode that lent this teammate a
            // belt gets it back here -- on the turns it runs as a seat, and
            // on no others. That is what lets one teammate answer its
            // operator and sit in a room without being two agents.
            let seated = seating.lent_to(turn.session_id());
            let Some(loan) = seated else {
                let visible: std::collections::HashSet<String> = tools
                    .iter()
                    .map(|tool| tool.name().to_owned())
                    .filter(|name| !unadvertised.contains(name))
                    .collect();
                let belt = openhuman_embed::HostTurnTools {
                    tools,
                    visible,
                    policy: None,
                };
                return match &gate {
                    Some(gate) => belt.with_policy(
                        Arc::clone(gate) as Arc<dyn oh::agent::tool_policy::ToolPolicy>
                    ),
                    None => belt,
                };
            };
            // **What a seat still may not reach.**
            //
            // These queue work for the `HarnessBrain` to drain, and no brain
            // drains inside an episode. The persona has their prose cut to
            // match (`seat_persona`), so the seat is neither told about them
            // nor handed them -- which is the only honest pairing until the
            // queues drain on a seated turn. Then both go.
            tools.retain(|tool| {
                !crate::harness::built_in::EPISODE_WITHHELD_TOOLS.contains(&tool.name())
            });
            let episode = loan.source.belt();
            // **`broadcast` is withheld in an operator's direct line.**
            //
            // There is no room to broadcast to: the roster is bound so `ask`
            // has targets, not so a message can be addressed to it. See
            // `seating::broadcast_withheld_in` for what two live runs cost.
            let withheld = crate::hive::seating::broadcast_withheld_in(
                loan.dm,
                crate::hive::host::TOOL_PREFIX,
            );
            let kept = |name: &str| withheld.as_deref() != Some(name);
            let mut visible: std::collections::HashSet<String> =
                tools.iter().map(|tool| tool.name().to_owned()).collect();
            visible.extend(episode.names().iter().filter(|name| kept(name)).cloned());
            let mut episode_tools = episode.tools;
            episode_tools.retain(|tool| kept(tool.name()));
            tools.append(&mut episode_tools);
            // **A guest seat can claim the work instead of answering it.**
            //
            // `take_over` wraps the room's own `complete_episode`, so the
            // conversation that asked still concludes -- which is the only
            // thing that releases the asker -- and the operator is told in
            // this teammate's own line. `None` for every seat the episode
            // lent no takeover: a desk seat, and the teammate whose DM it is.
            if let Some(takeover) =
                crate::hive::takeover::tool_for(&loan, crate::hive::host::TOOL_PREFIX)
            {
                tracing::debug!(
                    tool = %takeover.name(),
                    "[hive] a guest seat was offered the takeover verb"
                );
                visible.insert(takeover.name().to_owned());
                tools.push(takeover);
            } else if loan.takeover.is_some() {
                // **A guest that got no verb, said out loud.**
                //
                // `tool_for` answers `None` two ways and only one is ordinary:
                // no takeover on the loan (a desk seat, or the teammate whose
                // DM it is). The other is a loan that HAS one whose
                // `complete_episode` could not be lifted off a spare belt --
                // a renamed tool, a changed prefix, an episode that served a
                // narrower set. That path withholds the verb silently, and the
                // seat then does what a live run showed it do: agree in words
                // to own the work, reach for `complete_episode`, and leave the
                // operator's own line empty.
                tracing::warn!(
                    prefix = crate::hive::host::TOOL_PREFIX,
                    "[hive] a guest seat was lent a takeover but got no verb: `complete_episode` was \
                     not on its belt to wrap"
                );
            }
            // **The gate is the episode's, over this company's.**
            //
            // `with_policy` *replaces* the session's policy rather than
            // sitting in front of it, so composition is ours to do:
            // `admit` answers for the episode's own names and defers every
            // other call to the company gate. Passing `None` would deny the
            // teammate every tool it otherwise has.
            let admit = loan.source.belt().admit(
                gate.as_ref()
                    .map(|gate| Arc::clone(gate) as Arc<dyn oh::agent::tool_policy::ToolPolicy>),
            );
            openhuman_embed::HostTurnTools {
                tools,
                visible,
                policy: Some(admit),
            }
        });
    }
    if let Some(mcp) = mcp {
        spec = attach_opencompany_mcp(spec, mcp);
    }
    // The company's own registered MCP servers — see `company_mcp_servers`'s
    // doc comment. `AgentSpec::mcp` is additive ("call repeatedly to add
    // several"), so each attaches beside the `opencompany` server above
    // without displacing it.
    #[cfg(feature = "mcp")]
    for server in blueprint.company_mcp_servers.clone() {
        spec = spec.mcp(server);
    }
    // The runtime refuses an agent whose action dir it cannot create. A
    // workspace root that cannot be provisioned is reported once per agent
    // by the pool (issue #551) and must not stop dispatch — the file tools
    // refuse relative paths there, which is the failure the operator sees —
    // so the agent falls back to the runtime's own per-agent action dir.
    if blueprint.workspace.is_dir() || std::fs::create_dir_all(&blueprint.workspace).is_ok() {
        spec = spec.action_dir(blueprint.workspace.clone());
    } else {
        tracing::warn!(
            workspace = %blueprint.workspace.display(),
            "[build] agent workspace is not creatable; the runtime's default action dir stands in"
        );
    }
    spec.config(move |config| {
        config
            .agent_registry
            .entries
            .retain(|existing| existing.id != entry.id);
        config.agent_registry.entries.push(entry);
        // Pin the pooled turn to provider-native tool calls.
        //
        // A pooled turn's dialect comes from `agent.tool_dispatcher`
        // (`resolve_dispatcher_kind`, reached via `agent_chat_for` ->
        // `from_config_with_definition`), and OpenHuman's schema default for
        // that field is `"python"` — which resolves to
        // `DispatcherKind::Code(CodeStyle::Python)` *before* the native-support
        // arm is consulted, so an explicit choice wins over a model that can do
        // native calling perfectly well. `CodeDialect::should_send_tool_specs()`
        // is `false`, so the model is handed the catalogue as prose in the
        // system prompt and asked to write Python, and never sees a structured
        // tool spec at all.
        //
        // That is the wrong protocol for this crate: every OpenCompany tool
        // reaches a pooled teammate over the `opencompany` MCP server as
        // `mcp_call_tool` with a JSON `arguments` object, which is a structured
        // call described in prose. Models answer it with whatever markup they
        // favour, and `native_salvage` exists to parse the result back out.
        //
        // `episode_seat` already pins `NativeDialect` explicitly, so the two
        // agent shapes disagreed about the tool protocol for no reason. Pin the
        // pooled path to the same one.
        config.agent.tool_dispatcher = "native".into();
    })
}

/// The prompt section that tells an agent where this crate's tools went: on
/// the `opencompany` MCP server, reached through `mcp_call_tool`. Without it
/// the model calls `publish_artifact` by its bare name — the tool the belt
/// brief describes — and OpenHuman answers that no such tool exists.
fn opencompany_mcp_brief(tools: &[String]) -> String {
    let mut brief = String::from(MCP_BRIEF_HEADING);
    brief.push_str(
        "\n\nEvery tool named below is served \
         by the MCP server `opencompany`. Call one with `mcp_call_tool` and the arguments \
         object the tool's schema describes — `{\"server\": \"opencompany\", \"tool\": \"<name>\", \
         \"arguments\": {...}}` — never by its bare name; `mcp_list_tools` on that server shows \
         each schema. A result whose text begins `refused:` or `awaiting approval:` is final \
         for this turn: do not retry it.\n\n",
    );
    brief.push_str(MCP_BRIEF_TOOLS_PREFIX);
    brief.push_str(&tools.join(", "));
    brief.push('\n');
    brief
}

/// `blueprint.system_prompt` rendered the way OpenHuman renders an agent's standing
/// prompt: the body, then the shared grounding contract and the writing-style
/// block read from `blueprint.workspace`.
///
/// A seat's every turn is seeded, and a seeded session is never cold, so the
/// runtime composes no prompt of its own for it. The text returned here is
/// the only system prompt such a turn carries.
///
/// # Errors
///
/// A prompt section failing to render.
#[cfg(feature = "openhuman")]
pub fn rendered_seat_persona(blueprint: &AgentBlueprint) -> crate::Result<String> {
    let tools = Vec::new();
    let visible = std::collections::HashSet::new();
    let context = oh::agent::prompts::PromptContext {
        workspace_dir: &blueprint.workspace,
        model_name: &blueprint.model,
        agent_id: &blueprint.definition_name,
        tools: &tools,
        workflows: &[],
        dispatcher_instructions: "",
        learned: oh::agent::prompts::LearnedContextData::default(),
        visible_tool_names: &visible,
        tool_call_format: oh::agent::prompts::ToolCallFormat::Native,
        connected_integrations: &[],
        connected_identities_md: String::new(),
        include_profile: false,
        include_memory_md: false,
        curated_snapshot: None,
        user_identity: None,
        personality_roster: Vec::new(),
        agents_md_global: None,
        agents_md_local: None,
    };
    let rendered =
        oh::agent::prompts::SystemPromptBuilder::from_final_body(blueprint.system_prompt.clone())
            .build(&context)
            .map_err(|error| crate::error::OpenCompanyError::Harness(error.to_string()))?;
    tracing::debug!(
        agent = %blueprint.definition_name,
        bytes = rendered.len(),
        "[harness] rendered seat persona"
    );
    Ok(rendered)
}

/// The catalogue brief again, on a turn's text, for a session whose pinned
/// system prompt may name an older one — the roster was rebuilt under it
/// (see `CompanyAgent::catalogue_brief_stale`). The same block
/// [`opencompany_mcp_brief`] wrote, so [`tools_named_in_mcp_brief`] reads it
/// back from either place, headed by one line saying which list stands.
#[must_use]
pub(crate) fn opencompany_mcp_rebrief(tools: &[String], turn_text: &str) -> String {
    let brief = opencompany_mcp_brief(tools);
    format!(
        "[Your company tools changed since this conversation began. The list below          replaces the `Company tools` section of your instructions.]{brief}
{turn_text}"
    )
}

/// The heading [`opencompany_mcp_brief`] opens with.
const MCP_BRIEF_HEADING: &str = "\n\n## Company tools (MCP server `opencompany`)";
/// The line of the brief that lists the served tools.
const MCP_BRIEF_TOOLS_PREFIX: &str = "Tools: ";

/// The tools a system prompt advertises on the `opencompany` MCP server —
/// what [`opencompany_mcp_brief`] wrote, read back. Empty when the prompt
/// carries no brief. A turn test that used to look for a tool in the wire's
/// `tools` array looks here for the half of the belt that moved to MCP.
#[must_use]
pub fn tools_named_in_mcp_brief(system_prompt: &str) -> Vec<String> {
    let Some(at) = system_prompt.find(MCP_BRIEF_HEADING.trim_start()) else {
        return Vec::new();
    };
    let rest = &system_prompt[at..];
    let Some(line) = rest
        .lines()
        .find_map(|line| line.strip_prefix(MCP_BRIEF_TOOLS_PREFIX))
    else {
        return Vec::new();
    };
    line.split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
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
    policy: Arc<ApprovalPolicy>,
    deps: &HarnessDeps,
    grants: &[String],
    skill_deltas: &[SkillState],
    routed_context: &[(String, String)],
    instructions: Option<&str>,
    is_orchestrator: bool,
) -> crate::Result<AgentBlueprint> {
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
    )
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
///
/// `promotion`, when wired, additionally copies what `file_write` and `edit`
/// wrote into the company workspace under `agents/<agent id>/`, so the reply
/// can address it. Only those two are wrapped — the four readers produce
/// nothing to address, and wrapping them would buy a tree read per read-only
/// call. `None` leaves the belt byte-for-byte what it was. See
/// [`WritePromotion`].
pub(crate) fn file_tools(
    workspace: &Path,
    promotion: Option<Arc<WritePromotion>>,
) -> Vec<Box<dyn Tool>> {
    let security = Arc::new(workspace_security(workspace));
    let tools: Vec<Box<dyn Tool>> = vec![
        Box::new(FileReadTool::new(security.clone())),
        Box::new(FileWriteTool::new(security.clone())),
        Box::new(EditFileTool::new(security.clone())),
        Box::new(ListFilesTool::new(security.clone())),
        Box::new(GrepTool::new(security.clone())),
        Box::new(GlobTool::new(security)),
    ];
    match promotion {
        Some(promotion) => promotion.wrap_writers(tools),
        None => tools,
    }
}

#[cfg(test)]
#[path = "build_tests.rs"]
mod tests;
