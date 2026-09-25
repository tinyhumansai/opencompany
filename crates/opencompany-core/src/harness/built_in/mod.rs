//! WS4 — openhuman embedded as a library (the harness).
//!
//! This module supersedes the out-of-process OpenHuman seam
//! (`src/openhuman/{launcher,rpc,tools,channel}.rs`, JSON-RPC behind
//! `openhuman-rpc`) with **direct library embedding** of `vendor/openhuman`:
//! one process-wide [`openhuman_embed::Runtime`]
//! ([`crate::harness::openhuman_runtime`]) and one
//! [`openhuman_embed::Agent`] per manifest `[[agent]]`, each instantiated from
//! an [`AgentSpec`](openhuman_embed::AgentSpec) that carries its system
//! prompt, tool scope, provider route, access tier and workspace (plan
//! hive-desks, Phase 2).
//!
//! Compiled only under `feature = "openhuman"`. The default build links none of
//! it and keeps its offline, echo-brained behaviour.
//!
//! ## Layout
//!
//! * [`build`] — manifest `[[agent]]` → [`AgentBlueprint`](build::AgentBlueprint)
//!   (prompt, belt, model, policy, workspace) → `AgentSpec`.
//! * [`provider`] — hosted Medulla [`Provider`] + a `MockProvider` for tests;
//!   served to the runtime over the loopback
//!   [`model_bridge`](crate::harness::model_bridge).
//! * [`progress_pump`] — the per-turn progress stream → console frames, run
//!   trace, steps and cost.
//! * [`policy`] — [`ApprovalPolicy`](policy::ApprovalPolicy): `[policy]` →
//!   the approval decision (enforced on this crate's own tools; Phase 3).
//! * [`cost`] — per-turn usage → ledger + usage meter.
//!
//! ## Flagged seams
//!
//! * **Group-chat / desk routing** is opencompany's job (openhuman is
//!   single-agent). The per-desk `OpenHumanHive` arrives in Phase 4; until
//!   then every desk message takes the single-responder path.
//! * **This crate's own tools** (ledger, tasks, pages, workspace, composio,
//!   hosting, memory, speech, approval) are assembled on the blueprint but
//!   **unattached**: the embedded runtime's tool set is its own plus MCP
//!   servers, so they become the per-agent MCP catalogue in Phase 3.
//!
//! Live turn cost is **wired**: [`CompanyAgent::run`] reads each attempt's
//! token/cost totals off the bridge's usage tap (what the provider reported),
//! falling back to the runtime's own `TurnCostUpdated` figure, and
//! [`HarnessPool::run`] records them through [`cost::record_turn_cost`].

pub mod approval_tool;
/// Issue #775: the fail-closed shell audit wrapper — one intent line appended
/// (and fsynced) *before* a command runs, refusing the command outright when
/// that append fails. Pairs with the host-owned, per-agent sink
/// [`toolbelt::shell_audit`] resolves. See [`audit`].
pub mod audit;
pub mod blockers;
pub mod brain;
pub mod build;
pub mod capability_budget;
#[cfg(feature = "chargebee")]
pub mod chargebee;
/// Guarding a chat-only (`suppress_tools`) turn's reply against the tool-call
/// markup its frozen-at-turn-1 system prompt can still provoke, since that
/// prompt still advertises tool briefs the turn's own tool schema no longer
/// carries (#2094). Applied after [`native_salvage`], which is deliberately
/// inert on this same turn shape — see [`chat_only_guard`]'s module docs for
/// why the two do not overlap.
mod chat_only_guard;
mod checkpoint;
pub mod composio;
/// Issue #410: how a Composio action catalogue is narrowed and rendered for an
/// agent, and why every cut it makes describes itself. Pure and un-gated (the
/// live tools are behind `composio`, which CI never *runs*) — see
/// [`composio_catalog`].
pub mod composio_catalog;
/// The BYOK half of the Composio surface: a company's **own** Composio account,
/// reached directly at `backend.composio.dev` instead of through the
/// OpenHuman-managed proxy. Mirrors OpenHuman's `backend` / `direct` split. See
/// [`composio_direct`].
#[cfg(feature = "composio")]
pub mod composio_direct;
/// End-to-end proof that #410's narrowable, self-describing Composio listing is
/// reachable from a real turn on two large toolkits — the harness, the grant
/// gate, the approval policy and the Composio client are all real; only the
/// model's choices and the Composio backend are scripted. Test-only.
#[cfg(all(test, feature = "composio"))]
mod composio_turn_tests;
/// Issue #416: the confined turn — an ephemeral agent with no tools, no company
/// memory and no delegation, for a question that is about one object rather than
/// about the company. See [`confine`].
pub mod confine;
pub mod cost;
/// The decorator that lands a native `file_write`/`edit` in the company
/// workspace so the reply can address it. See [`file_tool_outputs`].
pub mod file_tool_outputs;
/// Hosting (TinyHosts): the per-company connection and the agent tools over it.
/// The keys it reads live in `company::hosting`, which is compiled in every
/// build — the console's Hosting settings write them whether or not this
/// harness exists to use them.
pub mod hosting;
/// End-to-end proof of issue #988: a turn really does get
/// [`MAX_TOOL_ITERATIONS`](build::MAX_TOOL_ITERATIONS) tool rounds instead of the
/// vendored ten, and a budget-armed turn's in-turn
/// [`BudgetStopHook`](oh::agent::stop_hooks::BudgetStopHook) halts it when it
/// outruns its money — distinguishably from an iteration-cap pause. Test-only.
///
/// Declared here rather than at `crate::harness` because it reads
/// `CompanyAgent`'s private `agent` field (the vendored session) to ask
/// `last_turn_hit_cap` — a child of `built_in`, not of the re-exporting parent.
#[cfg(test)]
mod iteration_cap_turn_tests;
pub mod ledger_tools;
pub mod lifecycle;
pub mod mcp;
pub mod mcp_probe;
pub mod memory;
pub mod memory_loop;
pub mod memory_tools;
/// Recovering a tool call that a model on the **native** transport wrote into
/// its message body as prose instead of emitting it through the structured
/// channel. Validated against the tools the turn itself offered — the marker a
/// shared parser cannot use — and applied in the provider, which is the last
/// point on this turn path where a text-shaped call can still become a real
/// one. See [`native_salvage`].
pub mod native_salvage;
/// End-to-end proof that a tool call a model wrote as **text** is executed by a
/// real turn: the harness, the grant gates, the approval policy, the dispatch
/// and the meter are all real, and the recovered call's synthesized id is shown
/// to keep its cycle paired all the way back into the model's context. Only the
/// model's output and the search backend are scripted. Test-only.
#[cfg(test)]
mod native_salvage_turn_tests;
pub mod orchestrator;
/// Issue #6014: task-aware extraction of an oversized tool result — one
/// bounded model call that keeps what answers the turn, in place of a byte cut
/// that keeps whatever happened to come first. See [`payload_extract`].
pub mod payload_extract;
/// Chargebee billing tools (issue #788), wired per company from its own
/// SecretStore. Always compiled so the credential resolution and the fail-closed
/// decision are testable at default features; only the tools are gated.
/// PayPal wallet + transaction tools (issue #789), wired per company from its
/// own SecretStore. Always compiled so credential resolution and the
/// fail-closed decision are testable at default features.
#[cfg(feature = "paypal")]
pub mod paypal;
/// Issue #337: the planning station — one tool-less model call per card entering
/// `planning`, with the host gathering the evidence and verifying every
/// prerequisite the model claims. See [`planning`].
pub mod planning;
pub mod policy;
/// The per-turn progress pump: OpenHuman's progress stream → live console
/// frames, the run trace, and the event buffer steps and cost are read from.
pub mod progress_pump;
pub mod provider;
/// Issue #244: `publish_artifact` — the only way a workspace file becomes a
/// deliverable — plus the staging queue the brain drains, the bounded workspace
/// scan that detects unpublished work, and the follow-up nudge's prompt. See
/// [`publish`].
pub mod publish;
#[cfg(test)]
mod publish_turn_conversation_tests;
#[cfg(test)]
mod publish_turn_dispatch_tests;
/// End-to-end proof that #244's `publish_artifact` is reachable from a real
/// dispatch, that a re-run extends by identity, and — the part nothing shorter
/// than a real turn loop can show — that the follow-up nudge fires **once**,
/// records a decline, and can never fail the run it follows. Test-only.
#[cfg(test)]
mod publish_turn_helpers_tests;
#[cfg(test)]
mod publish_turn_link_tests;
pub mod run_origin;
pub mod run_trace;
pub mod run_turn;
pub mod search;
/// A company's **own** search provider (the BYO half of issue #238): Brave,
/// Exa, Querit or a self-hosted SearXNG, wired from that company's stored key
/// through OpenHuman's own search tools. Falls back to [`search`]'s metered
/// managed surface whenever nothing is configured.
pub mod search_byo;
/// End-to-end proof that the #238 `web_search` tool is reachable from a real
/// turn — the harness, the grant gates, the approval policy, the cap and the
/// meter are all real; only the model's choices and the search backend's
/// responses are scripted. Test-only.
#[cfg(test)]
mod search_turn_tests;
pub mod skills;
pub mod steer;
pub mod steps;
pub mod title;
pub mod tool_posture;
pub mod toolbelt;
pub mod triage;
pub mod turn_outputs;
/// Issue #661 (M7): `read_workflow` / `update_workflow` / `delete_workflow` —
/// the agent's way to fix or retire a workflow instead of only ever creating
/// another one beside it. Kept out of `orchestrator.rs` (already the largest
/// file in `src/harness/`) because the three share a handle, a guard and a set
/// of refusals with each other rather than with anything there. See
/// [`workflow_admin`].
pub mod workflow_admin;
/// Issue #339: the staging queue the orchestrator's `run_workflow` /
/// `create_workflow` tools push a workflow reference onto and the
/// [`HarnessBrain`] drains at the end of a dispatch, so a card that built or
/// Issue #580: the workflow builder pass — turns a `workflow`-deliverable card's
/// plan into a proposed graph that lands In Review for approval. Modeled on the
/// planning station (one card, one tool-less model call, one settled outcome),
/// but it mints an attempt row because building the workflow is the card's work.
/// See [`workflow_build`].
pub mod workflow_build;
/// ran a workflow can link to it. See [`workflow_refs`].
pub mod workflow_refs;
/// End-to-end proof that an agent granted `files` and **not** `shell` can write
/// a relative path on a company that has never run — the #409 provisioning gap,
/// which only exists before anything has created the agent's workspace. Covers
/// a manifest teammate and a runtime overlay teammate, and pins that a traversal
/// out of a provisioned sandbox is still refused. Test-only.
#[cfg(test)]
mod workspace_provision_turn_tests;
pub mod workspace_tools;
#[cfg(test)]
mod workspace_turn_basic_tests;
/// End-to-end proof that the #237 workspace tools are reachable from a real
/// turn, with only the model's choices stubbed. Test-only.
#[cfg(test)]
mod workspace_turn_helpers_tests;
#[cfg(test)]
mod workspace_turn_supervised_tests;

use crate::harness::run_trace::RunTraceSink;
pub use brain::HarnessBrain;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use openhuman_core as oh;
use std::path::Path;
use tokio::sync::{Mutex, RwLock};

use crate::harness::provider::HarnessModel;

use crate::company::Agent as ManifestAgent;
use crate::company::Policy;
use crate::company::mcp::McpServerDecl;
use crate::company::steer::{SteerAction, SteerControl};
use crate::error::OpenCompanyError;
use crate::harness::cost::{TurnUsage, record_turn_cost};
use crate::harness::mcp_probe::McpFailureQueue;
use crate::harness::orchestrator::DelegationQueue;
use crate::harness::policy::{ApprovalPolicy, ApprovalRequestQueue};
use crate::hive::mcp_server::McpHost;
use crate::ports::skills_state::{SkillState, SkillStateStore};
use crate::ports::types::{
    Actor, ActorKind, AgentOverride, BudgetOverride, CompanyId, CompanyRecord, EventSeq,
    OverlayAgent, OverlayDesk, OverlayDeskMember, PolicyOverride, TurnStep,
};
use crate::ports::{
    ArtifactStore, CompanyStore, ContextStore, EventLog, FactStore, SecretStore, TaskStore,
    UsageMeter,
};
use crate::runtime::builder::agent_scoped_grants;

/// Shared dependencies every harness-built agent draws on.
#[derive(Clone)]
pub struct HarnessDeps {
    /// The company's emergency-stop flag, consulted by every agent's
    /// [`ApprovalPolicy`](crate::harness::built_in::policy::ApprovalPolicy) so a
    /// harness tool dispatched under `full` autonomy — which reaches no other
    /// gate — still refuses a consequential call once the switch is pulled.
    ///
    /// `None` at every non-harness construction site and every test that has no
    /// company gate to ask, which keeps them admitting exactly as before.
    pub emergency_gate: Option<Arc<crate::policy::gate::ManifestApprovalGate>>,
    /// The inference model shared across a company's agents. A [`HarnessModel`]
    /// is a tinyinference [`ChatModel<()>`](tinyinference::model::ChatModel)
    /// plus the telemetry slug the cost hook reads live per turn; it is served
    /// to the embedded runtime over the loopback `model_bridge`.
    pub provider: Arc<dyn HarnessModel>,
    /// Stable provider slug attributed to usage samples (e.g. `subscription`).
    pub provider_slug: String,
    /// Which agents this pool builds, when it serves one named harness rather
    /// than the whole company.
    ///
    /// `None` — the whole roster, which is every pre-harness caller and the
    /// single-harness case.
    ///
    /// `Some(ids)` — only those agents. A company running two `built_in`
    /// harnesses gets one pool per harness, each holding its own
    /// [`provider`](Self::provider); without this filter every pool would build
    /// every agent, so a ten-agent roster on three harnesses would stand up
    /// thirty live agents to use ten.
    pub serves: Option<std::collections::HashSet<String>>,
    /// Context store backing every agent's [`OcMemory`](memory::OcMemory).
    pub context: Arc<dyn ContextStore>,

    /// Company store the cost hook appends ledger entries to.
    pub store: Arc<dyn CompanyStore>,
    /// Optional usage meter (WS5 seam); `None` skips usage sampling.
    pub meter: Option<Arc<dyn UsageMeter>>,
    /// Root under which per-agent workspace directories are created
    /// (`{root}/{company}/{agent}/workspace`).
    pub workspace_root: PathBuf,
    /// The company home's MCP store directory — `<home>/mcp`, the same one
    /// [`McpRuntime`](crate::harness::mcp::McpRuntime) is built over.
    ///
    /// Carried because OpenHuman's `mcp_registry_*` tools take a config now
    /// instead of reading a process global, and the toolbelt has to hand them
    /// the config that selects *this* company's store. `None` leaves those two
    /// tools off the belt, which is what a caller with no MCP home should get.
    pub mcp_home: Option<PathBuf>,
    /// Whether each private agent workspace is initialized as a Git repository
    /// and checkpointed after tool calls. Host-level `[workspace]` config owns
    /// this switch; false preserves the pre-checkpoint behavior exactly.
    pub workspace_git_enabled: bool,
    /// The **instance data root** the shell audit sink hangs off, resolved
    /// through [`DataLayout::agent_audit_dir`](crate::store::DataLayout::agent_audit_dir)
    /// to `companies/<slug>/audit/<agent>/` (issue #775).
    ///
    /// Carried as its own field rather than derived from
    /// [`workspace_root`](Self::workspace_root)`.parent()` on purpose. The two
    /// are siblings under one data root today, and an implicit
    /// `workspace_root/..` would make a security boundary depend on a directory
    /// relationship nobody declared — the ambient-context coupling this codebase
    /// keeps getting bitten by. The audit sink is where it is because a caller
    /// said so.
    ///
    /// It must never be inside `workspace_root`: the agent workspace is also the
    /// `workspace_only` `SecurityPolicy` root the file tools enforce, so a sink
    /// under it is a policy-*permitted* write target for the very agent it
    /// records.
    pub audit_root: PathBuf,
    /// Optional model/tier applied to every agent, overriding the per-agent
    /// `tier` → model mapping. Set from the resolved hosted-inference model so
    /// the whole roster addresses the configured workload (e.g. `chat-v1`).
    /// `None` keeps each agent's tier-derived default.
    pub model_override: Option<String>,
    /// The company's durable notification store, used by workflow tools to
    /// announce failures and other unhealthy outcomes.
    pub notifications: Option<Arc<dyn crate::ports::notifications::NotificationStore>>,
    /// The company's task board, so a [`TaskDispatched`] cycle can load the
    /// dispatched card and write its result back. `None` off the task path (the
    /// chat brain leaves the board untouched).
    ///
    /// [`TaskDispatched`]: crate::ports::types::CompanyEvent::TaskDispatched
    pub tasks: Option<Arc<dyn TaskStore>>,
    /// The company's artifact store, so a dispatched card's output is recorded
    /// as a versioned artifact (#187) instead of only as note text. `None`
    /// leaves the board's behaviour exactly as before — the note is still
    /// written either way, so an unwired artifact store loses nothing that
    /// existed previously.
    pub artifacts: Option<Arc<dyn ArtifactStore>>,
    /// The company's ledgers, so an agent can read what has already been
    /// decided, goaled or ruled out, record what it decides, and declare an axis
    /// nobody anticipated. `None` builds no ledger tools at all — which is right
    /// for a path with no company behind it, and is what every construction site
    /// that predates them does.
    pub ledgers: Option<Arc<dyn crate::ports::ledgers::LedgerStore>>,
    /// The company's ledgers as they stood when the agent was built, for the
    /// prompt catalogue.
    ///
    /// Resolved to **data** before deps construction because `build_agent` is
    /// synchronous, the same shape the MCP servers already take. A ledger
    /// declared mid-run is therefore reachable by every tool immediately (the
    /// `ledger` argument is checked against the live registry at call time) and
    /// appears in the *prompt* only from the next build — which is the honest
    /// limit: system prompts are assembled once, and nothing can retroactively
    /// edit one already in flight.
    pub ledger_registry: crate::ledger::Registry,
    /// The company's skill-delta store, so a built agent can see its effective
    /// skill set (company-dir skills ∪ operator deltas ∪ custom docs) as read
    /// tools + a prompt catalogue. `None` leaves the agent skill-less (the chat
    /// path off the skills seam builds no skill surface).
    ///
    /// See [`skills`](crate::harness::skills) — this is the read-only catalogue
    /// slice; skill *execution* is deferred.
    pub skills: Option<Arc<dyn SkillStateStore>>,
    /// The company's source directory (`companies/<name>`), whose `skills/`
    /// subtree supplies the committed skill bundles unioned into the effective
    /// set. `None` surfaces only the operator deltas.
    pub skills_source_dir: Option<PathBuf>,
    /// The repo-level shared skill library (`skills/*/SKILL.md`), the same set
    /// the console's registry tab browses. Used only to heal pre-fix registry
    /// installs, whose stored snapshot is a one-line stub — see
    /// [`EffectiveSkills::materialize`](crate::harness::skills::EffectiveSkills::materialize).
    /// Empty only when the host serves no shared skill library, where a stub
    /// simply stays as it is; a platform-provisioned runtime otherwise receives
    /// the library the application state loaded, same as the serve path.
    pub skills_registry: Arc<[crate::company::SkillDoc]>,
    /// The company's effective MCP servers (issue #50), resolved to **data**
    /// (manifest `[[mcp_server]]` ∪ the runtime index, with each server's
    /// outbound credential materialized to
    /// [`AuthMaterial`](crate::company::mcp::AuthMaterial)) before deps
    /// construction. `build_agent` is synchronous but the
    /// [`SecretStore`](crate::ports::SecretStore) is async, so the runtime
    /// builder resolves these ahead of time; each agent then filters the set by
    /// its `mcp:*` tool grants. Empty leaves the agent with no MCP bridge tools.
    pub mcp_servers: Vec<McpServerDecl>,
    /// Install-wide default MCP servers (issue #527), carried so the live
    /// re-resolution in [`Harness::resolve_effective_mcp`] merges the same three
    /// layers the boot-time resolution did. Without it a console edit would
    /// re-resolve to manifest ∪ runtime and silently drop every default.
    pub default_mcp_servers: Vec<crate::company::McpServer>,
    /// The company's durable [`FactStore`], surfaced to the orchestrator agent
    /// through the `query_company` read tool (issue #53). `None` leaves the
    /// orchestrator without the facts half of its insight surface (the chat path
    /// off the orchestrator seam wires nothing).
    pub facts: Option<Arc<dyn FactStore>>,
    /// The company's [`EventLog`], surfaced to the orchestrator agent through
    /// the `query_company` read tool for recent-activity context (issue #53).
    /// `None` leaves the orchestrator without the recent-events half.
    pub events: Option<Arc<dyn EventLog>>,
    /// The shared delegation queue the orchestrator's `spawn_task` /
    /// `delegate_to_desk` tools push onto and the [`HarnessBrain`] drains after
    /// an orchestrator turn (issue #53). A [`DelegationQueue`] is a cheap shared
    /// handle; cloning `HarnessDeps` shares one queue between the tools built
    /// into the agent and the brain that drains it. Default is an empty queue.
    pub delegations: DelegationQueue,
    /// The shared handle to the company's [`WorkflowRunner`](crate::ports::WorkflowRunner),
    /// so the orchestrator's `run_workflow` tool can reach the runner that is
    /// itself built *from* these deps (issue #67). The runtime builder threads an
    /// empty handle here, builds the [`HarnessWorkflowRunner`](crate::workflows::HarnessWorkflowRunner)
    /// from a deps clone, then fills the shared cell — so the orchestrator agent
    /// (built later from a clone of these deps) reaches it at turn time. The cell
    /// holds a [`Weak`](std::sync::Weak), so deps↔runner is not a strong cycle.
    /// Default (and any build with no runner) leaves it empty and the tool
    /// reports workflow execution is not wired.
    pub workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle,
    /// The shared MCP failure queue the `OcMcpCallTool` decorator pushes onto and
    /// the [`HarnessBrain`] drains after a turn (the error-hardening cell). Same
    /// cheap-shared-handle pattern as [`Self::delegations`]; every string it
    /// carries is scrubbed at the source. Default is an empty queue.
    pub mcp_failures: McpFailureQueue,
    /// The shared publish queue the `publish_artifact` tool stages onto and the
    /// [`HarnessBrain`] drains at the end of a dispatch (issue #244). Same
    /// cheap-shared-handle pattern as [`Self::mcp_failures`], and for the same
    /// structural reason: tools are built **once per agent** while the card
    /// varies **per dispatch**, so a tool cannot hold a task id or a store and
    /// has to hand its work to something that does.
    ///
    /// Default is an empty queue, which simply means nothing is ever published
    /// — every path degrades to "this task produced no artifact", which is a
    /// legitimate outcome rather than a failure.
    pub pending_publishes: crate::harness::publish::PendingPublishQueue,
    /// The shared queue the orchestrator's `run_workflow` / `create_workflow`
    /// tools stage a workflow reference onto and the [`HarnessBrain`] drains at
    /// the end of a dispatch (issue #339) — the workflow half of a card's
    /// output link.
    ///
    /// Same cheap-shared-handle pattern as [`Self::pending_publishes`], and for
    /// the same structural reason: the tools are built **once per agent** while
    /// the card varies **per dispatch**, so a tool cannot hold a task id and has
    /// to hand its work to something that does.
    ///
    /// Default is an empty queue, which simply means no card ever links to a
    /// workflow — the stamp falls back to the attempt's trace, which is a
    /// complete answer rather than a missing one.
    pub workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue,
    /// The bounded, in-process cache the orchestrator's `run_workflow` tool fills
    /// with each successful run's node output and the `read_run_output` companion
    /// reads back (issue #418) — so a preview the run summary clipped is
    /// reachable within the same turn.
    ///
    /// Same cheap-shared-handle pattern as [`Self::workflow_refs`]: the run tool
    /// that stores and the read tool that serves are built in one `build_agent`
    /// pass off the same deps clone, so they share one cache. Default is an empty
    /// cache; nothing durable rides on it (the console run drawer is the durable
    /// record), so a fresh process simply starts with nothing to read back.
    pub run_outputs: crate::harness::orchestrator::RunOutputCache,
    /// The DURABLE, console-facing per-node run output store (issue #596) —
    /// distinct from [`Self::run_outputs`] above, which is the in-process,
    /// evictable agent cache. The workflow runner persists each settled run's
    /// bounded node output here so a *past* run reopened from History shows what
    /// every node produced. `None` (the default build, and every unwired test)
    /// degrades the persist to a no-op, exactly like [`Self::events`].
    pub run_output_store: Option<Arc<dyn crate::ports::run_output::WorkflowRunOutputStore>>,
    /// Where a workflow `agent` node's turn is recorded as a first-class
    /// attempt.
    ///
    /// A node's turn has neither a card nor a conversation, so before this it
    /// minted no row at all and nothing could ask what its agent did. `None`
    /// (the default build, and every unwired test) leaves each node behaving
    /// exactly as it did then — the node still runs, it is simply not recorded.
    pub workflow_runs: Option<Arc<dyn crate::ports::RunStore>>,
    /// The unredacted companion of those attempts' steps — reasoning text and
    /// raw tool I/O. `None` keeps only the scrubbed skeleton.
    pub deep_trace: Option<Arc<dyn crate::ports::deep_trace::DeepTraceStore>>,
    /// Issue #274's per-workflow snapshot ring, so the orchestrator's
    /// `update_workflow` / `delete_workflow` tools (issue #661, M7) write
    /// through the same undo-and-cascade path the console's `PUT`/`DELETE`
    /// routes do — an agent edit is recoverable on exactly the terms an
    /// operator's is.
    ///
    /// `None` (the default build, and every unwired test) makes those two tools
    /// refuse rather than degrade, unlike [`Self::events`]. The asymmetry is the
    /// point: a missing journal loses an audit line, while a missing revision
    /// store loses the only copy of the graph being overwritten.
    pub workflow_revisions: Option<Arc<dyn crate::ports::WorkflowRevisionStore>>,
    /// The shared approval-request queue every agent's [`ApprovalPolicy`] pushes
    /// a `RequireApproval` decision onto and the [`HarnessBrain`] drains after a
    /// turn, parking each request through
    /// [`CycleHost::park_effect`](crate::ports::brain::CycleHost::park_effect)
    /// so it reaches the operator's Approvals page (issue #172). Same
    /// cheap-shared-handle pattern as [`Self::delegations`]; the default is an
    /// empty queue, which simply means nothing is ever parked.
    pub approval_requests: ApprovalRequestQueue,
    /// The runtime's shared park transaction, for a turn that parks outside a
    /// cycle. `None` where no runtime is wired (tests, examples).
    pub approval_parker: Option<crate::runtime::approval_park::ApprovalParker>,
    /// The company's [`SecretStore`], so [`HarnessPool::ensure`] can **re-resolve**
    /// the effective MCP server set on each call and rebuild the roster when a
    /// console add/remove/enable-toggle changes it — the MCP-freshness fix (a
    /// runtime-added server reaches the agent on its next turn, no restart).
    /// `None` (default/tests) keeps the boot-resolved [`Self::mcp_servers`]
    /// static, exactly as before.
    pub secrets: Option<Arc<dyn SecretStore>>,
    /// The per-company SSRF allowlist for the `web` toolbelt (Cell A), from the
    /// manifest `[tools].web_allowed_domains`. Empty (the default) is *open
    /// mode* — all public hosts allowed — while OpenHuman's upstream `url_guard`
    /// still rejects private/loopback/link-local/metadata IPs regardless. A
    /// non-empty list is strict (only those hosts + subdomains); `"*"` is an
    /// explicit allow-all-public wildcard. Threaded verbatim into
    /// [`toolbelt::web_tools`](crate::harness::toolbelt::web_tools).
    pub web_allowed_domains: Vec<String>,
    /// The capability-tier filter applied to each agent's assembled tool vector
    /// (Cell A seam). [`AllowAll`](crate::harness::toolbelt::CapabilityFilter::AllowAll)
    /// (the default) is identity. When [`Self::plan`] is set,
    /// [`HarnessPool::ensure`] overwrites this per turn with the tenant's
    /// resolved filter; when the plan is `None` this stays the no-plan
    /// fallback/test override.
    pub capabilities: toolbelt::CapabilityFilter,
    /// The company's source directory (`companies/<name>`), from which a
    /// workflow's `sub_workflow` nodes resolve a child by `workflow_id`
    /// (`workflows/<id>.toml`). Distinct from
    /// [`Self::skills_source_dir`](Self::skills_source_dir) so the two seams stay
    /// independent even though both currently derive from the same `seed_dir`.
    /// `None` (default/tests, and platform-provisioned tenants with nothing on
    /// disk) keeps the loud `UnwiredResolver`, so a reached `sub_workflow` node
    /// fails clearly instead of resolving nothing.
    pub workflow_source_dir: Option<PathBuf>,
    /// The tenant's capability tier plan (issue #108). `None` (the default)
    /// leaves gating **off** — byte-identical to Cell A, [`Self::capabilities`]
    /// is used verbatim. When set, [`HarnessPool::ensure`] resolves a per-tenant,
    /// per-period, fail-closed [`CapabilityFilter`](toolbelt::CapabilityFilter)
    /// from the [`UsageMeter`] before each turn and installs it on the roster it
    /// builds. Resolved from the manifest `[plan]` section by the runtime builder.
    pub plan: Option<capability_budget::CapabilityPlan>,
    /// The MANAGED media-generation backend (issue #109). `None` (the default at
    /// every construction site) fails closed — no image/video tools are wired.
    /// Only the production runtime builder sets it, from
    /// [`media_backend_from_env`](crate::harness::provider::media_backend_from_env)
    /// (env-only — never a tenant secret). When `Some` **and** a company
    /// explicitly grants `media`, [`build::build_agent`] wires the
    /// [`toolbelt::media_tools`]; a grant with no credential wires nothing and
    /// warns.
    pub media: Option<toolbelt::MediaBackend>,
    /// The per-tenant Composio configuration (issue #110). `None` (the default
    /// at every construction site) fails closed — no Composio tools are wired.
    /// [`HarnessPool::ensure`] re-resolves it each turn (folded into the roster
    /// fingerprint) so a console token set/rotate takes effect next turn with no
    /// restart. Only wired when a company **explicitly** grants `composio` **and**
    /// a credential can be obtained: the company's own token under
    /// [`composio::TINYHUMANS_KEY_KEY`](crate::harness::composio::TINYHUMANS_KEY_KEY) if it has one,
    /// else this instance's platform identity. With neither, no tools are wired —
    /// never a borrowed identity.
    pub composio: Option<composio::TenantComposio>,

    /// The per-company Chargebee connection (issue #788). `None` (the default at
    /// every construction site) fails closed — no billing tools are wired.
    /// Resolved from that company's own secret store, never from the
    /// environment: two companies on one host bill two different sites.
    /// `HarnessPool::ensure` re-resolves it each turn, so a key set or rotated in
    /// the console takes effect next turn with no restart.
    #[cfg(feature = "chargebee")]
    pub chargebee: Option<chargebee::TenantChargebee>,

    /// The per-company PayPal connection (issue #789). `None` fails closed —
    /// no wallet tools are wired. Resolved from that company's own secret store
    /// and re-resolved each turn, like `chargebee`.
    #[cfg(feature = "paypal")]
    pub paypal: Option<paypal::TenantPaypal>,

    /// The per-company hosting connection. `None` (the default at every
    /// construction site) fails closed — no hosting tools are wired. Resolved
    /// from that company's own secret store and re-resolved each turn, like
    /// `chargebee`: two companies on one host deploy to two different hosting
    /// accounts, and a deployment publishes files to the internet under the
    /// account's own name.
    pub hosting: Option<hosting::TenantHosting>,
    /// The MANAGED web-search backend (issue #238). `None` (the default at every
    /// construction site but the production runtime builder) **fails closed** —
    /// no `web_search` tool is wired and agents behave exactly as before.
    ///
    /// The runtime builder supplies the deployment fallback from
    /// [`search_backend_handle_from_env`](crate::harness::provider::search_backend_handle_from_env),
    /// and [`HarnessPool::ensure`] attaches the company's `search/managed/key`
    /// as the request-time first tier. Searches authenticated by that key bill
    /// the company's TinyHumans account; the deployment fallback bills the
    /// account of whoever runs this server. It also applies the company's
    /// `[tools].search_daily_calls` cap. When `Some` **and** a company
    /// **explicitly** grants `search` (never via `*`), [`build::build_agent`]
    /// wires [`search::search_tools`]; a grant with no credential wires nothing
    /// and warns, media's shape exactly.
    ///
    /// The handle carries the company's shared daily-call ledger, so cloning
    /// these deps across a roster gives every agent of the company one budget
    /// rather than one each.
    pub search: Option<search::SearchBackend>,
    /// The company's **own** search provider connection, when it configured one
    /// in the console (Brave / Exa / Querit / a self-hosted SearXNG).
    ///
    /// `None` — the default at every construction site — means "search through
    /// the managed surface above", which is the fallback OpenHuman's own
    /// registry takes for a BYO engine with no key. When `Some` **and** the
    /// company **explicitly** grants `search`, [`build::build_agent`] wires
    /// [`search_byo::byo_search_tools`] *instead of* the metered managed tool:
    /// two "search the web" tools on one belt is how a model comes to spend the
    /// platform's money by accident.
    ///
    /// Resolved from that company's own secret store and re-resolved each turn
    /// like `composio` / `hosting`, so a key set or rotated in the console takes
    /// effect on the next turn with no restart. Never from the environment: a
    /// BYO key is billed to the company that pasted it.
    pub tenant_search: Option<search_byo::TenantSearch>,
    /// Issue #111 — the shared registry of in-flight, steerable runs. The
    /// [`HarnessBrain`] registers a dispatched task / desk delegation here before
    /// running it (and installs the steer stop-hook over the slot's control), so
    /// an operator can pause / cancel / redirect it mid-flight. The **same**
    /// handle is threaded onto the [`CompanyRuntime`](crate::company::runtime::CompanyRuntime)
    /// so the operator steer routes reach it. A cheap shared handle (like
    /// [`delegations`](Self::delegations)); the default is an empty registry,
    /// which simply lists nothing and rejects every steer as `not in flight`.
    pub steer: crate::company::steer::InflightRegistry,
    /// Issue #383 — the shared set of cancellable workflow runs. The
    /// orchestrator's `run_workflow` tool mints its run context through this, so
    /// an agent-initiated run appears in the same map the console's cancel route
    /// reads and is stoppable like any other. The runtime builder threads in the
    /// same handle it puts on the [`CompanyRuntime`](crate::company::CompanyRuntime);
    /// the default is a private map nothing else can see, which simply means the
    /// tool's runs are not cancellable.
    pub run_supervisor: crate::runtime::RunSupervisor,
    /// Issue #170 — the ports an `output` node's `destination` needs to route a
    /// finished workflow's report to a person or a channel (mail handle, inbox,
    /// user directory, wired channels), bundled so this struct grows one field
    /// rather than four.
    ///
    /// Read post-engine by
    /// [`deliver_outputs`](crate::workflows::delivery::deliver_outputs) — never
    /// by the engine, which knows nothing about destinations. `None` (the
    /// default at every construction site but the production runtime builder)
    /// **fails closed and loud**: nothing is sent and the run result carries a
    /// `failed` row saying delivery is not wired, so an authored destination can
    /// never quietly do nothing.
    pub delivery: Option<crate::workflows::WorkflowDeliveryDeps>,
    /// Issue #237 — the company's shared workspace note tree, so agents can
    /// read (and, under an explicit `workspace` grant, revise) the operator's
    /// standards and playbooks instead of guessing at them.
    ///
    /// The same [`WorkspaceStore`](crate::ports::WorkspaceStore) handle the
    /// console's REST/GraphQL surface writes through, so an operator edit is
    /// visible to the next agent turn with no rebuild — the tools hold no
    /// snapshot and hit the store per call. `None` (the default at every
    /// construction site but the production runtime builder) **fails closed**:
    /// no workspace tools are wired and agents behave exactly as before.
    pub workspace: Option<Arc<dyn crate::ports::WorkspaceStore>>,
}

/// One live company agent: a handle on the process-wide OpenHuman runtime,
/// keyed by its manifest id.
///
/// Since plan hive-desks Phase 2 the agent is an [`openhuman_embed::Agent`]
/// — a cheap-clone handle whose `turn(&self)` takes a shared reference — so
/// the pool no longer holds a `Mutex<Agent>`. What serialises one agent's
/// turns is [`turn_lock`](Self::turn_lock): one agent runs at most one turn
/// at a time, and *different* agents run concurrently. A hive that binds this
/// agent into two desks clones the handle and queues on the same lock.
pub struct CompanyAgent {
    /// The manifest agent id.
    pub agent_id: String,
    /// The manifest agent's human-readable role.
    pub role: String,
    /// This teammate's manifest `budget_usd_daily` cap, carried onto the roster
    /// so the dispatch gate in [`HarnessPool::run_inner`] can read it without
    /// re-loading the manifest per turn (issue #304).
    ///
    /// `None` for an uncapped teammate — and for every overlay teammate, which
    /// carries no per-agent cap in v1.
    pub budget_usd_daily: Option<f64>,
    /// The stable per-`(company, agent)` OpenHuman session — `{company}:{agent_id}`,
    /// minted by [`openhuman_session_key`](crate::harness::session_key::openhuman_session_key)
    /// and passed as [`Turn::session`](openhuman_embed::Turn::session) on
    /// every conversational turn on every surface. OpenHuman owns the thread
    /// transcript behind it; nothing is seeded from this side.
    pub session_key: String,
    /// The id this agent is registered under on the runtime
    /// (`{company}--{agent_id}`, see
    /// [`runtime_agent_id`](crate::session_key::runtime_agent_id)), and the
    /// key everything the runtime writes for it — transcripts, skills root,
    /// action dir — lives under.
    pub runtime_id: String,
    /// The bearer the `opencompany` MCP server authenticates this agent's
    /// tool calls with (plan hive-desks Phase 3): minted at build, registered
    /// on the process-wide [`McpHost`] under [`runtime_id`](Self::runtime_id),
    /// and fixed on the agent's spec, so bearer → agent → in-flight turn is a
    /// total function for the life of a turn.
    pub mcp_bearer: String,
    /// The company this agent belongs to — the first half of every key the
    /// MCP host and the in-flight registry use.
    pub company: CompanyId,
    /// The runtime agent. Shared, not locked: see the type docs.
    agent: openhuman_embed::Agent,
    /// Serialises this agent's turns.
    /// Belts episodes have lent this teammate, by conversation. The same map
    /// the belt factory reads, so what a host lends here reaches the turn.
    seating: crate::hive::seating::EpisodeBelts,
    turn_lock: Arc<Mutex<()>>,
    /// The loopback route this agent's model is served on, and the usage tap
    /// its attempts are metered from. See [`model_bridge`](crate::harness::model_bridge).
    bridge: crate::harness::model_bridge::BridgeHandle,
    /// The curated step labels of this agent's tools, captured from the built
    /// tool set (see [`StepLabels`](steps::StepLabels) for why the turn loop
    /// cannot supply them).
    ///
    /// Resolved once per agent build rather than per turn: the tool set is fixed
    /// for the life of a pooled agent, and a rebuild — the only thing that can
    /// change which search belt is wired — mints a new `CompanyAgent` anyway.
    step_labels: steps::StepLabels,
    /// This crate's own tools on the belt — everything OpenHuman does not run
    /// natively — as the `opencompany` MCP server serves them to this agent.
    /// Shared with the host's [`McpAgent`] entry; kept here so a test can
    /// still ask which tools a grant wired.
    tools: Arc<Vec<Arc<dyn tinytools::Tool>>>,
    /// Every belt tool's name in belt order, native and served — what a grant
    /// wired, which is what the roster tests ask.
    belt_names: Vec<String>,
    /// The process-wide MCP host this agent is registered on, held so a
    /// dropped roster entry revokes its bearer and a turn can register itself
    /// in flight.
    mcp: Arc<McpHost>,
    /// The agent workspace: the file tools' sandbox and every turn's `cwd`.
    workspace: PathBuf,
    /// The [`HarnessModel`] this agent's turns actually run against — the same
    /// `Arc` [`build::build_agent_with_model`] resolved (the company default or
    /// this agent's own pin), served to the runtime over the bridge (issue
    /// #2306 / Codex round 2, comment 4012457318).
    ///
    /// `deps.provider` is the company **default**; an agent with its own
    /// `{provider, model}` pin runs against a distinct
    /// [`TenantProvider`](provider::TenantProvider) that carries its own
    /// telemetry cells. Held here so `meter_turn_costs` reads the SAME
    /// instance the turn ran through.
    chat_model: Arc<dyn HarnessModel>,
    /// The tools this agent's belt wired when it was built. Kept so a roster
    /// rebuild can tell whether the catalogue moved — see
    /// [`Self::catalogue_brief_stale`].
    served_catalogue: Vec<String>,
    /// Whether the session this agent resumes may still carry an OLDER brief
    /// than [`Self::served_catalogue`].
    ///
    /// The embedded runtime pins a session's system prompt at its first
    /// committed turn (`tinyagents_runtime::Session::apply_prefix` refuses a
    /// changed prefix after one), and every conversational turn resumes this
    /// agent's one stable [`session_key`](Self::session_key). So a roster
    /// rebuild that changes the served catalogue — a Composio token set, a
    /// tool grant, an MCP server added — reaches the MCP host (the rebuilt
    /// [`McpAgent`] serves the new set at once) but never the prompt the
    /// model reads the catalogue off. The previous builder rebuilt an
    /// in-memory session per roster, so the prompt was always current; here
    /// the fix is OpenHuman's own for a warm session (its
    /// `refresh_dynamic_announcements`): say it on the turn text. Set by
    /// [`HarnessPool::ensure`] when the rebuilt entry's catalogue differs
    /// from the retired one's (or the retired one was itself still pending),
    /// read by [`Self::run_with_steer`], which prepends the current brief to
    /// the next conversational turn and clears it once that turn commits.
    catalogue_brief_stale: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for CompanyAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompanyAgent")
            .field("agent_id", &self.agent_id)
            .field("runtime_id", &self.runtime_id)
            .field("tools", &self.tools.len())
            .finish_non_exhaustive()
    }
}

impl Drop for CompanyAgent {
    fn drop(&mut self) {
        // A retired roster entry's bearer stops working with it. Guarded by
        // the bearer so a rebuilt agent that took this runtime id — possible
        // only after this one released it — is never evicted by the old
        // one's drop.
        self.mcp
            .unregister_if_bearer(&self.runtime_id, &self.mcp_bearer);
    }
}

/// The embedded runtime's deterministic summary for a turn whose model
/// produced no result at all (`turn_checkpoint::build_deterministic_final_summary`).
/// This host reads it as the transient empty class — or, when the bridge saw
/// the provider fail behind it, as that failure.
const NO_RESULT_SENTINEL: &str = "I finished this turn but produced no result to report.";

/// The graceful reply returned when a turn yields the transient empty-response
/// class twice — so chat never shows a bare "Couldn't send" for a model hiccup.
const GRACEFUL_EMPTY_REPLY: &str = "Sorry — I hit a temporary model hiccup and couldn't produce a reply. Please resend your message.";

/// The operator-facing notice returned when the plan-level total token ceiling
/// (issue #188) is reached — a hard dispatch refusal, so no model call is made.
/// Surfaced as the turn's reply on every dispatch path (operator chat, task,
/// steered/background), since they all funnel through
/// [`HarnessPool::run_inner`](HarnessPool::run_inner).
const TOTAL_BUDGET_EXHAUSTED_NOTICE: &str =
    "Token budget for this period is exhausted — dispatch paused until the period resets.";

fn monthly_budget_exhausted_notice(cap_usd: f64) -> String {
    format!(
        "This company has reached its monthly spend cap of ${cap_usd:.2} — dispatch is paused until the month resets."
    )
}

fn unmeasurable_monthly_budget_notice(cap_usd: f64) -> String {
    format!(
        "This company's monthly spend cap of ${cap_usd:.2} cannot be checked because its financial ledger is unavailable — dispatch is paused until the ledger can be read."
    )
}

/// The operator-facing notice returned when one teammate has spent its manifest
/// `budget_usd_daily` (issue #304) — a hard dispatch refusal for that teammate
/// only, made before any model call.
///
/// Deliberately a *visible refusal* rather than a silent no-op, and deliberately
/// per-teammate: the rest of the company keeps running, and the operator is told
/// which desk stopped, what its cap is, and when it comes back. There is no
/// per-call unit to park at turn level — an inference turn is not a tool call —
/// so a notice is the honest answer, mirroring
/// [`TOTAL_BUDGET_EXHAUSTED_NOTICE`].
fn agent_budget_exhausted_notice(agent_id: &str, cap_usd: f64) -> String {
    format!(
        "{agent_id} has reached its daily spend cap of ${cap_usd:.2} — dispatch to this teammate \
         is paused until the cap resets at 00:00 UTC. Other teammates are unaffected."
    )
}

/// Why a spend gate could not read the spend it exists to bound.
///
/// The two cases are not the same fault and must not be reported as one. A
/// meter that errors is transient — the next read may succeed, and nothing
/// about the deployment is wrong. A host with no meter at all can never
/// enforce a declared cap: the cap and the deployment contradict each other
/// until an operator changes one of them, and no amount of retrying resolves
/// it. Both refuse the priced operation; they differ in what the operator is
/// told to do about it.
enum SpendReadFault {
    NoMeter,
    QueryFailed(OpenCompanyError),
}

/// Reads the usage samples a spend gate needs, resolving the two ways that
/// read comes back empty-handed into [`SpendReadFault`].
///
/// Every spend gate goes through this one seam, so "can this cap be enforced
/// right now" cannot answer differently for the total ceiling, a teammate's
/// daily cap, and the predicate the optional model calls consult.
async fn read_spend_for_gate(
    meter: Option<&dyn UsageMeter>,
    company: &CompanyId,
    since_millis: u64,
) -> std::result::Result<Vec<crate::ports::UsageSample>, SpendReadFault> {
    let Some(meter) = meter else {
        return Err(SpendReadFault::NoMeter);
    };
    meter
        .query(company, since_millis)
        .await
        .map_err(SpendReadFault::QueryFailed)
}

/// The shape every pre-dispatch spend refusal returns: the reply IS the
/// notice, and none of the in-turn signals apply because no turn ran — no
/// iteration cap was reached, no in-turn spend brake armed, and no provider
/// reported the account out of credits.
///
/// `abnormal_stop` IS set: this is a terminal, non-resumable stop with no
/// checkpoint to continue from, same as an ACP refusal/cancellation. Workflow
/// and card dispatch already fail the attempt on `abnormal_stop`; leaving it
/// `None` here let a pre-dispatch refusal settle those attempts `Succeeded`
/// and bind the refusal notice downstream as if it were the node's answer.
fn spend_gate_refusal(reply: String, cause: SpendGateCause) -> TurnOutcome {
    TurnOutcome {
        reply,
        steps: Vec::new(),
        hit_iteration_cap: false,
        abnormal_stop: Some(cause.abnormal_stop().to_string()),
        halted_for_spend: None,
        budget_paused: None,
    }
}

/// Why a pre-dispatch spend gate refused.
///
/// The two read differently to whoever is looking: an unreadable meter is a
/// host fault to go and fix, an exhausted cap is a healthy meter reporting a
/// real ceiling, and waiting for the reset or raising the cap is the move.
/// Reporting both as the former sends operators to troubleshoot a meter that
/// is working.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpendGateCause {
    Unmeasurable,
    Exhausted,
}

impl SpendGateCause {
    fn abnormal_stop(self) -> &'static str {
        match self {
            Self::Unmeasurable => {
                "[stopped: dispatch refused, spend could not be measured against a declared cap]"
            }
            Self::Exhausted => "[stopped: dispatch refused, a declared spend cap is exhausted]",
        }
    }
}

/// The operator-facing refusal when the company's declared token ceiling
/// cannot be measured.
///
/// Names the fault and the operator's move, because the alternative — running
/// the turn and warning into a log — leaves the console rendering a ceiling
/// that has quietly stopped applying, which is a worse state than a refusal
/// somebody can see and act on.
fn unmeasurable_ceiling_notice(fault: &SpendReadFault) -> String {
    match fault {
        SpendReadFault::NoMeter => "This company declares a token budget for the period, but \
             this host has no usage meter to measure spend against it — dispatch is paused \
             rather than run against a ceiling that cannot be enforced. Configure a usage \
             meter, or remove the budget."
            .to_string(),
        SpendReadFault::QueryFailed(_) => "Spend against this company's token budget could not \
             be read, so dispatch is paused rather than run against a ceiling that cannot be \
             checked. It resumes as soon as the usage meter reads again."
            .to_string(),
    }
}

/// The per-teammate twin of [`unmeasurable_ceiling_notice`]. Names the cap it
/// could not check and says the rest of the company is unaffected — this gate
/// is scoped to the desk that declared a bound.
fn unmeasurable_agent_budget_notice(
    agent_id: &str,
    cap_usd: f64,
    fault: &SpendReadFault,
) -> String {
    match fault {
        SpendReadFault::NoMeter => format!(
            "{agent_id} declares a daily spend cap of ${cap_usd:.2}, but this host has no usage \
             meter to measure spend against it — dispatch to this teammate is paused rather than \
             run against a cap that cannot be enforced. Configure a usage meter, or remove the \
             cap. Other teammates are unaffected."
        ),
        SpendReadFault::QueryFailed(_) => format!(
            "{agent_id}'s spend against its ${cap_usd:.2} daily cap could not be read, so \
             dispatch to this teammate is paused rather than run against a cap that cannot be \
             checked. It resumes as soon as the usage meter reads again. Other teammates are \
             unaffected."
        ),
    }
}

/// The coarse pre-task proximity warning threshold (issue #1846): 90% of the
/// applicable ceiling. Deliberately a fixed constant rather than a manifest
/// setting — an operator-configurable threshold needs a new `[plan]` field,
/// validation, and a console control, which is a bigger, separate piece of
/// work; this closes the "no pre-task proximity primitive exists" gap the
/// issue names with the coarsest version that still tells an operator "you're
/// about to lose dispatch" before it happens rather than only when it does.
const BUDGET_PROXIMITY_RATIO: f64 = 0.9;

/// Whether an integer token spend has crossed the coarse proximity threshold
/// against `cap`, without having reached it (the exhaustion check owns `>=`
/// separately — this and that are mutually exclusive by construction at both
/// call sites, which check `!total_exhausted`/`spent < cap` first).
fn is_approaching_budget_ceiling(spent: u64, cap: u64) -> bool {
    if cap == 0 {
        return false;
    }
    (spent as f64) >= (cap as f64) * BUDGET_PROXIMITY_RATIO
}

/// The USD-denominated twin of [`is_approaching_budget_ceiling`], for the
/// per-agent daily cap read (which is measured in dollars, not tokens).
fn is_approaching_budget_ceiling_f64(spent: f64, cap: f64) -> bool {
    if !(cap.is_finite() && cap > 0.0) {
        return false;
    }
    spent >= cap * BUDGET_PROXIMITY_RATIO
}

/// The company-wide proximity warning's operator-facing text. Deliberately
/// makes NO per-task cost claim ("this task will exceed your budget") — only
/// the coarse, honest "you are near your limit" the meter read actually
/// supports. Names no exact figures either: the threshold is an internal
/// constant, not something the operator configured and would expect echoed
/// back.
fn budget_proximity_message() -> String {
    "This company is nearing its token budget for the current period. Dispatch will pause \
     automatically once the ceiling is reached."
        .to_string()
}

/// The per-agent twin of [`budget_proximity_message`].
fn budget_proximity_message_usd(agent_id: &str) -> String {
    format!(
        "{agent_id} is nearing its daily spend cap. Dispatch to this teammate will pause \
         automatically once the cap is reached."
    )
}

/// The classification of a single `agent.turn` attempt, for the retry wrapper.
enum AttemptOutcome {
    /// A non-empty reply.
    Reply(String),
    /// The transient empty-response class (an empty/blank reply, or the model's
    /// "empty response" error) — retryable.
    Empty,
    /// The turn's own inference call failed on the account being out of
    /// budget/credits (issue #1846) — recognised via the same
    /// `is_budget_exhausted_message` wire-shape check the delegated sub-agent
    /// halt already keys on. **Not retryable** (retrying hits the identical
    /// wall) and **not a `Hard` error** — it ends the turn gracefully with the
    /// actionable summary as the reply, distinguishable from a real failure.
    BudgetPaused {
        /// The actionable, operator-facing halt copy.
        summary: String,
    },
    /// A hard error (auth/build/non-budget provider rejection/etc.) —
    /// propagated loudly, never swallowed.
    Hard(OpenCompanyError),
}

/// The result of a completed turn: the reply text plus the scrubbed
/// [`TurnStep`] timeline folded from the turn's progress stream.
///
/// The steps are per-bubble: the operator bubble carries the orchestrator's
/// steps, a delegated desk bubble carries that desk lead's steps. They ride the
/// wire on [`OutboundMessage::steps`](crate::ports::types::OutboundMessage) and
/// are **never** written to memory ([`HarnessPool::run`] persists
/// `outcome.reply` only).
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    /// The agent's reply text.
    pub reply: String,
    /// The scrubbed, folded processing steps (empty for a memory-served or
    /// tool-less turn — the zero-steps tell).
    pub steps: Vec<TurnStep>,
    /// Whether this turn **paused at its tool-iteration cap** rather than
    /// finishing what it set out to do (issue #926).
    ///
    /// A capped turn is not an error and never has been: openhuman stops the
    /// tool loop, makes one extra tools-disabled call asking the model for a
    /// resumable "Done so far / Next steps" checkpoint, and returns that as an
    /// ordinary `Ok(reply)`. So the reply reads like a finished answer, and
    /// nothing in the text, the steps or the error channel distinguishes "I
    /// answered you" from "I ran out of steps mid-task" — which is exactly what
    /// the operator could not tell.
    ///
    /// Read from openhuman's public
    /// `progress_pump::hit_iteration_cap` off the turn's progress stream while
    /// the agent lock is still held, the same under-lock idiom
    /// [`read_turn_usage`] uses. `false` on every path that returns an outcome
    /// **without** running a model turn (the two pre-turn budget refusals, the
    /// ACP fold) — a refusal is not a pause, and labelling one as a cap hit
    /// would tell the operator to reply "continue" to a turn that never ran.
    pub hit_iteration_cap: bool,
    /// A fixed, host-authored notice when this turn stopped for a reason that
    /// is neither a clean finish nor a resumable cap (PR #1880 review) — on
    /// the ACP fold, an agent-issued `refusal`, a `cancelled` turn, or a
    /// `stopReason` this fold does not recognise.
    ///
    /// `None` on every other path, including [`hit_iteration_cap`] pauses,
    /// which already have their own distinct, resumable-checkpoint signal and
    /// must keep it — this field is for the opposite case, where there is no
    /// checkpoint to resume. Before this field existed,
    /// [`HarnessAgentRunner`](crate::workflows::caps::HarnessAgentRunner)
    /// read only `hit_iteration_cap` to decide whether a workflow agent node
    /// finished; it stayed `false` for all three of these stops too, so a
    /// refused or cancelled turn settled the node `Succeeded` and reported
    /// `StopReason::Finished` — indistinguishable from the agent actually
    /// answering, and a declined or interrupted reply advanced the workflow
    /// graph as if it were the deliverable.
    ///
    /// Always a short, host-authored string, **never** the raw
    /// `stopReason`/error text an external agent sent (same reasoning as the
    /// `Other` arm of `stop_reason_note` in `harness::acp::run_turn`, which
    /// this field's message is drawn from): it ends up in
    /// `EngineError::Capability`, a developer/operator-facing message, and an
    /// unbounded wire string has no more business there than in a persisted
    /// [`TurnStep`].
    ///
    /// [`hit_iteration_cap`]: Self::hit_iteration_cap
    pub abnormal_stop: Option<String>,
    /// The in-turn **spend halt**, when one stopped this turn (issue #1032).
    ///
    /// `Some` exactly when the teammate declared a `budget_usd_daily`, the
    /// [`SpendStopHook`](crate::harness::spend::SpendStopHook) armed for it
    /// fired, and the turn therefore stopped short of the answer it was working
    /// towards. `None` on every other path, including every turn by a teammate
    /// who declared no budget — no hook is installed for them, so there is
    /// nothing that could have halted them.
    ///
    /// A **separate** field from [`hit_iteration_cap`](Self::hit_iteration_cap)
    /// rather than another reading of it, because the two are different
    /// outcomes needing different operator actions: a step pause is resumable
    /// with "continue", a spend halt means the work costs more than its budget
    /// allows and asking again just spends more. #988 pinned that they are
    /// distinguishable — a budget halt reads `last_turn_hit_cap() == false`,
    /// because the run paused *below* `max_tool_iterations` — which is why this
    /// could not be folded into the existing flag.
    ///
    /// Carries the figures rather than a bare `bool` so the notice can say what
    /// was spent against which cap, and names the teammate so a chain of turns
    /// cannot report a number the operator has no way to attribute.
    pub halted_for_spend: Option<SpendHalt>,
    /// The turn **paused for lack of inference budget/credits** (issue #1846),
    /// rather than dying with a generic error.
    ///
    /// `Some` exactly when [`classify_turn`](CompanyAgent) recognised the
    /// model provider's `Err` as the same budget-exhausted wire shape the
    /// delegated sub-agent path already halts gracefully on
    /// (`oh::api::classify::is_budget_exhausted_message`). `None` on
    /// every other path, including a turn that failed for an unrelated
    /// reason — those still propagate as `Err`, never as this field.
    ///
    /// A **third, distinct** terminal state alongside
    /// [`hit_iteration_cap`](Self::hit_iteration_cap) and
    /// [`halted_for_spend`](Self::halted_for_spend): an iteration-cap pause is
    /// resumable with "continue", an in-turn spend halt means the *company's*
    /// own declared cap was reached mid-turn, and a budget pause means the
    /// *account itself* is out of money — the operator's only lever is adding
    /// credits, not continuing or raising a cap. Conflating it with either
    /// would tell the operator the wrong next action.
    pub budget_paused: Option<BudgetPause>,
}

/// What one in-turn spend halt cost, and whose cap it was measured against
/// (issue #1032).
///
/// The figures are the ones this crate already owns: the cap is
/// [`CompanyAgent::turn_spend_cap_usd`], and the spend is the sum of the
/// [`TurnUsage::cost_usd`](crate::harness::cost::TurnUsage::cost_usd) totals the
/// turn already reports. Deliberately **not** parsed out of the vendored hook's
/// `reason` string, which is a developer-facing trace line whose shape is
/// upstream's to change.
///
/// `agent` is carried because one operator bubble can cover a responder turn, a
/// desk turn and a relay turn, each with its own cap. The iteration-cap notice
/// declines to name a number for exactly that reason; naming the teammate is
/// what makes a number attributable, and is why this one can be quoted where
/// that one could not.
#[derive(Debug, Clone, PartialEq)]
pub struct SpendHalt {
    /// The teammate whose cap was reached.
    pub agent: String,
    /// What that teammate's turn had spent when the brake fired, in USD.
    ///
    /// Can exceed [`cap_usd`](Self::cap_usd): the brake fires *between* tool
    /// iterations, so the call that crossed the line has already been paid for.
    pub spent_usd: f64,
    /// The cap it was measured against, in USD — the teammate's declared
    /// `budget_usd_daily`.
    pub cap_usd: f64,
}

/// A turn that ended because the account (or a BYO/custom provider's own
/// account) ran out of inference budget/credits — issue #1846.
///
/// Before this, the top-level orchestrator's own inference call had no
/// envelope marker to key on (that marker is written only for a *delegated*
/// sub-agent tool result — see `classify_turn`'s doc comment on
/// `CompanyAgent`), so a budget-exhausted 400 from the model provider fell
/// through `classify_turn` into the generic
/// `Hard(OpenCompanyError::Harness(_))` arm and the turn died with an opaque
/// HTTP error instead of a graceful, actionable pause. This is the top-level
/// analogue of the delegated path's halt
/// (`RepeatedToolFailureMiddleware::after_tool`,
/// `terminal_inference_halt_summary`) — same actionable copy, same "add
/// credits and try again" framing, but for a turn with no tool/sub-agent
/// envelope to key on.
///
/// **Not true resume** (issue #561: mid-turn checkpointing was declined). The
/// turn ends cleanly here; whatever it had already done (memory writes, tool
/// side effects) stays done. `HarnessPool` parks a durable re-issue marker
/// (see `crate::runtime::grants`) so the operator's "Add credits" action
/// re-dispatches the SAME original message from the top once the account is
/// topped up — a fresh turn, not a resumed one. A re-issue can therefore
/// repeat a non-idempotent side effect the first attempt already performed;
/// this crate cannot guarantee exactly-once here any more than a human
/// re-sending the same message could.
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetPause {
    /// The teammate whose turn paused.
    pub agent: String,
    /// The actionable, operator-facing halt copy — byte-identical in shape to
    /// [`agent_budget_exhausted_notice`]'s pre-dispatch refusal and to the
    /// vendored sub-agent halt's summary, so "out of budget" reads the same
    /// way everywhere a company hits it.
    pub summary: String,
}

impl CompanyAgent {
    /// Registers one blueprint on the runtime and wraps the result.
    ///
    /// The blueprint's model is served over the loopback bridge; the spec is
    /// [`build::agent_spec_for`] over it. A `DuplicateId` — the previous
    /// roster's clone of this id is still alive, which a rebuild racing an
    /// in-flight turn can produce, or two test pools naming one company at
    /// once — is retried under a numbered suffix rather than failed: the
    /// suffix changes only which transcripts directory the runtime writes,
    /// never the session key a turn resumes.
    ///
    /// The agent is also registered on the process-wide `opencompany` MCP
    /// host (plan hive-desks Phase 3) under the same runtime id, with a fresh
    /// bearer, its non-native belt as the catalogue and its
    /// [`ApprovalPolicy`] as the gate; when the host's listener is up, the
    /// spec carries the matching `McpServer`. `events` is the journal `read`
    /// is served from — `None` leaves that one tool refusing.
    pub(crate) fn register(
        runtime: &openhuman_embed::Runtime,
        company: &CompanyId,
        agent_id: &str,
        role: &str,
        budget_usd_daily: Option<f64>,
        blueprint: build::AgentBlueprint,
        events: Option<Arc<dyn EventLog>>,
    ) -> crate::Result<Self> {
        let bridge = crate::harness::model_bridge::register(
            blueprint.chat_model.clone() as Arc<dyn tinyinference::model::ChatModel<()>>,
            &blueprint.model,
        )?;
        let mcp = crate::hive::mcp_server::global();
        let mcp_bearer = crate::hive::mcp_server::McpAgent::mint_bearer();
        // The speech tools stay on the MCP server; this crate's own tools do
        // not, so they leave the served catalogue with them.
        let allow_tools: Vec<String> = crate::hive::tools::served_speech_tool_names()
            .iter()
            .map(|name| (*name).to_string())
            .collect();
        // Shared once, here, and handed to the spec as a factory that mints
        // owned handles per turn. OpenHuman's own tools are filtered out: it
        // runs those itself, and handing them back would register each twice.
        let mut blueprint = blueprint;
        let step_labels = steps::StepLabels::from_tools(&blueprint.tools);
        let belt_names: Vec<String> = blueprint
            .tools
            .iter()
            .map(|tool| tool.name().to_string())
            .collect();
        // **The catalogue a rebuild compares is the belt, not the MCP list.**
        //
        // It used to be `allow_tools`, which was every served tool back when
        // this crate's tools reached the model over the `opencompany` server.
        // They are native now, so `allow_tools` is a constant — the four
        // speech verbs — and a catalogue that cannot move can never be found
        // stale. A console grant that wires `workspace.write` changed the
        // agent's tools and owed the live session a brief, and nothing said
        // so.
        //
        // The belt is what actually changed, and it is what the pinned
        // definition scopes, so it is what a rebuild has to compare.
        let served_catalogue = belt_names.clone();
        let gate = Arc::clone(&blueprint.policy);
        let native_belt: Arc<Vec<Arc<dyn tinytools::Tool>>> =
            Arc::new(crate::hive::tools::share_belt(
                std::mem::take(&mut blueprint.tools)
                    .into_iter()
                    .filter(|tool| !build::OPENHUMAN_NATIVE_TOOLS.contains(&tool.name()))
                    .collect(),
            ));
        // Created before the agent, because the belt factory closes over it at
        // registration and an episode writes to it long afterwards.
        let seating = crate::hive::seating::EpisodeBelts::default();
        let base_id = crate::session_key::runtime_agent_id(company, agent_id);
        let mut runtime_id = base_id.clone();
        let mut attempt = 0u32;
        let agent = loop {
            let attach = mcp.endpoint_for(company, &runtime_id).map(|endpoint| {
                crate::hive::mcp_server::McpAttach {
                    endpoint,
                    bearer: mcp_bearer.clone(),
                    allow_tools: allow_tools.clone(),
                }
            });
            let spec = build::agent_spec_for(
                &blueprint,
                &runtime_id,
                bridge.provider(),
                attach.as_ref(),
                Some(&native_belt),
                Some(&gate),
                Some(&seating),
            );
            match runtime.agent(spec) {
                Ok(agent) => break agent,
                Err(openhuman_embed::AgentError::DuplicateId(_)) if attempt < 64 => {
                    attempt += 1;
                    let suffix = format!("-{attempt}");
                    let head: String = base_id
                        .chars()
                        .take(64usize.saturating_sub(suffix.len()))
                        .collect();
                    runtime_id = format!("{}{suffix}", head.trim_end_matches('-'));
                }
                Err(err) => {
                    return Err(OpenCompanyError::Harness(format!(
                        "register agent '{agent_id}' on the OpenHuman runtime: {err}"
                    )));
                }
            }
        };
        if attempt > 0 {
            tracing::debug!(
                company = %company,
                agent = agent_id,
                runtime_id = %runtime_id,
                "[harness] runtime id was taken; registered under a numbered suffix"
            );
        }
        let build::AgentBlueprint {
            workspace,
            chat_model,
            ..
        } = blueprint;
        // No `.tools(..)`: the belt is the agent's own now. The server still
        // serves the speech tools, and still holds the policy and workspace
        // those calls are admitted and sandboxed against.
        let mut entry = crate::hive::mcp_server::McpAgent::new(
            company.clone(),
            agent_id,
            runtime_id.clone(),
            mcp_bearer.clone(),
        )
        .policy(Arc::clone(&gate))
        .workspace(workspace.clone());
        if let Some(events) = events {
            entry = entry.events(events);
        }
        mcp.register(entry);
        Ok(Self {
            agent_id: agent_id.to_string(),
            role: role.to_string(),
            budget_usd_daily,
            session_key: crate::harness::session_key::openhuman_session_key(company, agent_id),
            runtime_id,
            mcp_bearer,
            company: company.clone(),
            agent,
            seating,
            turn_lock: Arc::new(Mutex::new(())),
            bridge,
            step_labels,
            tools: native_belt,
            belt_names,
            mcp,
            workspace,
            chat_model,
            served_catalogue,
            catalogue_brief_stale: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// The catalogue this agent's prompt brief names — see
    /// [`Self::catalogue_brief_stale`].
    #[must_use]
    pub(crate) fn served_catalogue(&self) -> &[String] {
        &self.served_catalogue
    }

    /// Whether the next conversational turn re-announces the catalogue — see
    /// [`Self::catalogue_brief_stale`].
    #[must_use]
    pub(crate) fn catalogue_brief_pending(&self) -> bool {
        self.catalogue_brief_stale
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Marks the resumed session's brief as possibly older than this entry's
    /// catalogue, given what the entry it replaces was briefed with and
    /// whether that one had itself still to announce. Carried forward rather
    /// than compared pairwise alone: a rebuild that changed the catalogue and
    /// was rebuilt again (to the same set) before any turn ran still leaves
    /// the session on the prefix the first roster committed.
    pub(crate) fn inherit_catalogue_brief(&self, previous_catalogue: &[String], pending: bool) {
        if pending || previous_catalogue != self.served_catalogue.as_slice() {
            self.catalogue_brief_stale
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }

    /// The conversation a turn on `chat_id` answers in, as the in-flight
    /// registry records it. `None` — a dispatched card, a workflow node — is
    /// a workflow surface keyed by the session the turn runs on.
    fn surface_for(
        chat_id: Option<&str>,
        thread_root: Option<EventSeq>,
        session_id: &str,
    ) -> tinyhivemind_embed::ConversationRef {
        use tinyhivemind_embed::ConversationKind;
        let (id, kind) = match chat_id {
            Some(chat) if tinyhivemind_core::chat::is_general_chat(Some(chat)) => {
                (chat.to_string(), ConversationKind::General)
            }
            Some(chat) if chat.starts_with("dm:") => (chat.to_string(), ConversationKind::Direct),
            Some(chat) => (chat.to_string(), ConversationKind::Desk),
            None => (session_id.to_string(), ConversationKind::Workflow),
        };
        tinyhivemind_embed::ConversationRef {
            id,
            kind,
            thread_root: thread_root.map(|root| tinyhivemind::Sequence(root.value())),
        }
    }

    /// The runtime agent handle, for a hive binding.
    pub fn runtime_agent(&self) -> &openhuman_embed::Agent {
        &self.agent
    }

    /// The lock one of this agent's turns must hold. Shared with every hive
    /// this agent is bound into, so two desks cannot run it at once.
    pub fn turn_lock(&self) -> Arc<Mutex<()>> {
        self.turn_lock.clone()
    }

    /// Where an episode lends this teammate its belt.
    #[must_use]
    pub fn seating(&self) -> &crate::hive::seating::EpisodeBelts {
        &self.seating
    }

    /// The names of every tool this agent's belt wires, in belt order — the
    /// OpenHuman-native ones the spec scopes and the ones the `opencompany`
    /// MCP server serves alike.
    pub fn tool_names(&self) -> Vec<String> {
        self.belt_names.clone()
    }

    /// This crate's own tools on the belt — the `opencompany` MCP catalogue
    /// the agent reaches through `mcp_call_tool`.
    pub fn tools(&self) -> &[Arc<dyn tinytools::Tool>] {
        &self.tools
    }

    /// The process-wide MCP host this agent is served on.
    pub fn mcp(&self) -> &Arc<McpHost> {
        &self.mcp
    }

    /// The agent workspace directory.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// The model this agent's turns run against.
    pub fn chat_model(&self) -> &Arc<dyn HarnessModel> {
        &self.chat_model
    }

    /// Runs one turn against this agent, returning its reply text and the
    /// per-attempt token/cost totals.
    ///
    /// **Empty-response hardening (the error-hardening cell)**: the hosted brain
    /// occasionally returns a transient empty completion, which openhuman
    /// surfaces as an error. Rather than letting the operator see a bare
    /// "Couldn't send", this wrapper retries **once**; if the second attempt is
    /// still empty it returns a graceful, scrubbed message instead of an `Err`.
    /// **Non-transient** errors (budget, auth, build) still propagate loudly — no
    /// blanket swallow. Every attempt's usage is returned so the cost hook meters
    /// what the model actually consumed (a burnt empty attempt still costs
    /// tokens).
    ///
    /// # Why the usage is beside the `Result`, not inside it
    ///
    /// A failing turn is not a free turn. A wall-clock ceiling fires *because*
    /// the agent did ten minutes of real work, and the tokens it read back are
    /// as owed as a success's. Returning `Result<(TurnOutcome, Vec<TurnUsage>)>`
    /// made "the turn failed" and "there is nothing to meter" the same value, so
    /// a `?` anywhere downstream silently dropped the spend from the attempt
    /// row, from the ledger and from the usage meter alike — the console then
    /// reported ten minutes of model work as `0 tok / $0.000`. The tuple is the
    /// fix that the compiler enforces: a caller must handle the usage before it
    /// can even look at the outcome.
    ///
    /// The usage is read from the bridge's tap — every model call the attempt
    /// made, as the provider reported it — with the turn's own
    /// `TurnCostUpdated` figure as the fallback when the tap saw nothing.
    pub async fn run(&self, message: &str) -> (crate::Result<TurnOutcome>, Vec<TurnUsage>) {
        self.run_with_steer(
            message,
            None,
            None,
            None,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
    }

    /// Runs one turn with an optional operator **steer** control installed
    /// (issue #111).
    ///
    /// When `steer` is `Some`, a [`SteerStopHook`](crate::harness::steer::SteerStopHook)
    /// over the shared control is installed around the turn via
    /// [`with_stop_hooks`](oh::agent::stop_hooks::with_stop_hooks). OpenHuman
    /// fires stop hooks **between** tool-loop iterations (never mid-tool-call),
    /// so an operator pause / cancel / redirect halts the turn gracefully at the
    /// next iteration boundary. The hooks are a task-local the embedded turn
    /// reads when it builds its session, and `Turn::send` awaits in this task,
    /// so they reach it.
    ///
    /// When a steer is pending after the first attempt yields the transient
    /// empty-response class, the one-shot retry is **skipped** — a cancel (or
    /// pause) issued before any text is produced must not silently restart the
    /// work. With no steer this is byte-identical to the pre-#111 `run`.
    ///
    /// When `run_sink` is `Some`, the progress pump also writes each step
    /// through to the [`RunStore`](crate::ports::RunStore) as it arrives, so a
    /// dispatched card's trace is durable *during* the run rather than only
    /// after it (issue #242).
    ///
    /// # Which session a turn resumes
    ///
    /// A conversational turn — one on a chat, addressed by `chat` — resumes
    /// this agent's one stable session ([`session_key`](Self::session_key)),
    /// whatever chat it is on: OpenHuman owns the thread, and which
    /// conversation a line belongs to is what the turn text says (the cue
    /// below; the attributed desk delta in Phase 4). An **isolated** turn — a
    /// dispatched card, a workflow node, a background task: anything that
    /// names no chat, or that brings its own context — runs on a fresh
    /// session of its own, so it neither drags the chat transcript in nor
    /// leaves its working notes there; that is what the previous builder's
    /// clear-and-suppress-autoload did.
    pub async fn run_with_steer(
        &self,
        message: &str,
        steer: Option<&SteerControl>,
        stream: Option<crate::turn_stream::TurnStreamCtx>,
        run_sink: Option<Arc<run_trace::RunTraceSink>>,
        // The conversation this turn belongs to (#1890), carried in its own
        // right rather than read off `stream`: a turn that has a chat to
        // resume on may still stream nowhere, and a workflow turn streams on
        // a route that is not a chat.
        chat: crate::runtime::delegation::ChatTarget<'_>,
    ) -> (crate::Result<TurnOutcome>, Vec<TurnUsage>) {
        let turn_chat_id: Option<String> = stream
            .as_ref()
            .and_then(|ctx| match &ctx.route {
                crate::turn_stream::LiveRoute::Chat { chat_id } => Some(chat_id.clone()),
                crate::turn_stream::LiveRoute::Workflow { .. } => None,
            })
            .or_else(|| chat.chat_id.map(str::to_string));
        // Isolated: a turn that names no conversation at all (a dispatched
        // card, a workflow node, a background task), or one that brings its
        // own context (`history_seed == false`). Everything else is a line
        // in this agent's one conversation session.
        let isolated = Self::isolated_session(turn_chat_id.as_deref(), chat.history_seed);
        let session_id = if isolated {
            format!("{}:run:{}", self.session_key, uuid::Uuid::new_v4().simple())
        } else {
            self.session_key.clone()
        };

        let pump = progress_pump::ProgressPump::start(self.step_labels.clone(), stream, run_sink);

        // Where this line was said, for a session that hears every chat. A
        // single line rather than the `agent_session` cue block it replaces:
        // the attributed delta that names the other speakers is Phase 4's.
        let cued: std::borrow::Cow<'_, str> = match turn_chat_id.as_deref() {
            Some(chat_id) if !isolated => std::borrow::Cow::Owned(match chat.thread_root {
                Some(root) => format!(
                    "[conversation: {chat_id}, thread {}]\n{message}",
                    root.value()
                ),
                None => format!("[conversation: {chat_id}]\n{message}"),
            }),
            _ => std::borrow::Cow::Borrowed(message),
        };
        // A resumed session whose pinned prompt may name an older catalogue
        // hears the current one on this turn — see `catalogue_brief_stale`.
        // An isolated turn runs cold on a fresh session and needs nothing.
        let rebrief = !isolated && self.catalogue_brief_pending();
        let cued: std::borrow::Cow<'_, str> = if rebrief {
            std::borrow::Cow::Owned(build::opencompany_mcp_rebrief(
                &self.served_catalogue,
                cued.as_ref(),
            ))
        } else {
            cued
        };
        let message: &str = cued.as_ref();

        let chat_only = crate::runtime::delegation::is_chat_only_turn();
        if chat_only {
            tracing::debug!(
                "[harness] chat-only turn — the embedded turn keeps its tool scope; \
                 the reply is guarded for tool markup (#2094)"
            );
        }

        let mut hooks: Vec<Arc<dyn oh::agent::stop_hooks::StopHook>> = Vec::new();
        if let Some(control) = steer {
            hooks.push(Arc::new(crate::harness::steer::SteerStopHook::new(
                control.clone(),
            )));
        }
        let mut spend_brake: Option<(f64, Arc<std::sync::atomic::AtomicBool>)> = None;
        if let Some(cap) = self.turn_spend_cap_usd() {
            let hook = crate::harness::spend::SpendStopHook::new(cap);
            spend_brake = Some((cap, hook.halted()));
            hooks.push(Arc::new(hook));
        }

        let budget_pause_summary: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

        // A hive seat turn (plan hive-desks, Phase 4): the driver's episode
        // coordinates ride on the in-flight registration below so the MCP
        // server attributes the seat's speech to its round, the timeout is
        // counted from the moment the lock is held, and the outbox is handed
        // back through the scope when the turn returns.
        let seat = crate::runtime::delegation::seat_turn();
        let _turn = self.turn_lock.lock().await;
        // An episode seat brackets its own turn, inside this same lock, from
        // `hive::host`: the session it runs is the episode's, not this
        // pool's, so the pool no longer has a bracket handed down to it.
        let deadline = seat
            .as_ref()
            .map(|seat| tokio::time::Instant::now() + seat.timeout);
        // Register the turn in flight so the `opencompany` MCP server can
        // attribute this agent's tool calls to it (plan hive-desks Phase 3),
        // and hand it the channel those calls come back on: the belt's tools
        // file into task-local queues (approval scope, publish and delegation
        // claims), so they must run on THIS task — `serve_jobs` below, joined
        // with the turn. The lock is held, so nothing else of this agent's
        // should be registered; a caller that registered first regardless
        // keeps its own entry and this turn only lends it the executor.
        let (job_tx, mut job_rx) = tokio::sync::mpsc::channel::<crate::hive::tools::ToolJob>(8);
        let in_flight = self.mcp.in_flight();
        let mut registration = crate::hive::tools::InFlight::new(
            self.company.clone(),
            self.runtime_id.clone(),
            self.agent_id.clone(),
            Self::surface_for(turn_chat_id.as_deref(), chat.thread_root, &session_id),
        )
        .with_executor(job_tx.clone());
        if let Some(seat) = seat.as_ref() {
            registration = registration.with_hive(seat.hive.clone());
        }
        let _in_flight = match in_flight.begin(registration) {
            Ok(ticket) => Some(ticket),
            Err(_) => {
                in_flight.with(&self.runtime_id, |turn| {
                    turn.executor = Some(job_tx.clone())
                });
                None
            }
        };
        drop(job_tx);
        let served = self.mcp.agent(&self.runtime_id);
        let serve_jobs = async {
            while let Some(job) = job_rx.recv().await {
                let turn = in_flight.snapshot(&self.runtime_id);
                let result = match &served {
                    Some(agent) => agent.serve_call(&job.tool, job.arguments, turn).await,
                    None => serde_json::json!({
                        "content": [{ "type": "text", "text": format!(
                            "refused: '{}' is not served for this agent", job.tool
                        ) }],
                        "isError": true,
                    }),
                };
                let _ = job.reply.send(result);
            }
            // The registry's sender outlives the turn, so this loop ends only
            // if the entry was dropped under us; never let it end the select.
            std::future::pending::<()>().await;
        };
        // Anything left on the taps belongs to no attempt of ours.
        let _ = self.bridge.take_usage();
        let _ = self.bridge.take_errors();
        // `cwd` only where the workspace exists: the runtime refuses a turn
        // rooted at an inaccessible path, and a broken workspace root is a
        // reported-once condition the turn survives (issue #551) — relative
        // file writes are what the operator loses, not the reply.
        let cwd = self.workspace.is_dir().then_some(self.workspace.as_path());
        let send = |sender: tokio::sync::mpsc::Sender<oh::agent::progress::AgentProgress>| {
            let mut turn = self
                .agent
                .turn(message)
                .session(session_id.clone())
                .on_progress(sender);
            if let Some(cwd) = cwd {
                turn = turn.cwd(cwd);
            }
            let fut = turn.send();
            let seat = seat.clone();
            async move {
                match deadline {
                    Some(deadline) => match tokio::time::timeout_at(deadline, fut).await {
                        Ok(outcome) => outcome,
                        Err(_) => {
                            let secs = seat
                                .as_ref()
                                .map(|seat| seat.timeout.as_secs())
                                .unwrap_or_default();
                            if let Some(seat) = seat.as_ref() {
                                seat.timed_out
                                    .store(true, std::sync::atomic::Ordering::Relaxed);
                            }
                            Err(openhuman_embed::CoreError::Rpc {
                                method: "agent.turn",
                                message: format!("the seat turn ran past its {secs}s timeout"),
                            })
                        }
                    },
                    None => fut.await,
                }
            }
        };

        let turn_body = oh::agent::stop_hooks::with_stop_hooks(
            hooks,
            Box::pin(async {
                let mut usages: Vec<TurnUsage> = Vec::new();
                let started = std::time::Instant::now();
                let first = send(pump.sender()).await.map(|outcome| outcome.reply);
                let first_elapsed = started.elapsed();
                usages.push(self.tapped_usage());
                let reply: crate::Result<String> = match self
                    .classify_turn(self.unmask(first), first_elapsed)
                {
                    AttemptOutcome::Reply(reply) => Ok(reply),
                    AttemptOutcome::Hard(err) => Err(err),
                    AttemptOutcome::BudgetPaused { summary } => {
                        let redacted = crate::harness::mcp_probe::redact(&summary, &[]);
                        if let Ok(mut slot) = budget_pause_summary.lock() {
                            *slot = Some(redacted.clone());
                        }
                        Ok(crate::harness::mcp_probe::scrub(&redacted, &[]))
                    }
                    AttemptOutcome::Empty => {
                        let spend_halted = spend_brake.as_ref().is_some_and(|(_, halted)| {
                            halted.load(std::sync::atomic::Ordering::SeqCst)
                        });
                        if steer.map(|c| c.requested()).unwrap_or(false) || spend_halted {
                            Ok(crate::harness::mcp_probe::scrub(GRACEFUL_EMPTY_REPLY, &[]))
                        } else {
                            let retry_started = std::time::Instant::now();
                            let second = send(pump.sender()).await.map(|outcome| outcome.reply);
                            let second_elapsed = retry_started.elapsed();
                            usages.push(self.tapped_usage());
                            match self.classify_turn(self.unmask(second), second_elapsed) {
                                AttemptOutcome::Reply(reply) => Ok(reply),
                                AttemptOutcome::Empty => {
                                    Ok(crate::harness::mcp_probe::scrub(GRACEFUL_EMPTY_REPLY, &[]))
                                }
                                AttemptOutcome::BudgetPaused { summary } => {
                                    let redacted = crate::harness::mcp_probe::redact(&summary, &[]);
                                    if let Ok(mut slot) = budget_pause_summary.lock() {
                                        *slot = Some(redacted.clone());
                                    }
                                    Ok(crate::harness::mcp_probe::scrub(&redacted, &[]))
                                }
                                AttemptOutcome::Hard(err) => Err(err),
                            }
                        }
                    }
                };
                (reply, usages)
            }),
        );
        let (reply, mut usages): (crate::Result<String>, Vec<TurnUsage>) = tokio::select! {
            biased;
            outcome = turn_body => outcome,
            () = serve_jobs => unreachable!("the tool-job loop never completes"),
        };
        // The session heard the current catalogue; the next rebuild decides
        // afresh. A failed turn commits nothing, so the brief stays owed.
        if rebrief && reply.is_ok() {
            self.catalogue_brief_stale
                .store(false, std::sync::atomic::Ordering::Release);
        }
        match _in_flight {
            // What the seat said, back to the caller, before the lock goes.
            Some(ticket) => {
                let finished = ticket.finish();
                if let Some(seat) = seat.as_ref()
                    && let Ok(mut outbox) = seat.outbox.lock()
                {
                    *outbox = finished.outbox;
                }
            }
            None => {
                in_flight.with(&self.runtime_id, |turn| turn.executor = None);
            }
        }
        drop(_turn);

        let events = pump.finish().await;
        // The bridge tap is authoritative for tokens and for a charged amount
        // the provider reported. A provider that reports tokens but no price
        // (the managed backend's billing meta is absent on a BYOK route, and
        // on every scripted double) leaves the cost at zero there, while the
        // runtime's own `TurnCostUpdated` carries its catalogue estimate — the
        // figure the in-turn spend brake fired on — so that estimate stands
        // in for the price, and for everything when the tap saw nothing.
        if usages
            .iter()
            .any(|usage| usage.is_zero() || usage.cost_usd == 0.0)
        {
            let segments = progress_pump::attempt_event_segments(&events, usages.len());
            // **The price when nothing marks where an attempt began.**
            //
            // `attempt_event_segments` splits on `AgentProgress::TurnStarted`,
            // which is declared and never emitted -- a real stream opens
            // `IterationStarted`. So every segment comes back empty and the
            // `else { continue }` below skipped silently: a provider that
            // reported tokens but no price (every scripted double, and a BYOK
            // route per the note above) kept `cost_usd` at zero, and the spend
            // a halt announced was zero with it.
            //
            // `TurnCostUpdated` is a cumulative rollup, so the stream's last
            // one prices the turn. It supplies the **price only**, and only to
            // an attempt that burned something: a turn that burned nothing must
            // not inherit a total, which is what `zero_usage_turn_writes_nothing`
            // and its two neighbours exist to hold.
            let rollup = progress_pump::last_observed_turn_cost(&events);
            for (usage, segment) in usages.iter_mut().zip(segments) {
                let Some(observed) = progress_pump::last_observed_turn_cost(segment) else {
                    if !usage.is_zero()
                        && usage.cost_usd == 0.0
                        && let Some(rollup) = rollup.as_ref()
                        && rollup.cost_usd > 0.0
                    {
                        usage.cost_usd = rollup.cost_usd;
                    }
                    continue;
                };
                if usage.is_zero() {
                    tracing::info!(
                        agent = %self.agent_id,
                        input_tokens = observed.input_tokens,
                        output_tokens = observed.output_tokens,
                        cost_usd = observed.cost_usd,
                        "[turn] an attempt published no totals; metering the spend observed on \
                         its own progress-stream segment"
                    );
                    *usage = observed;
                } else if usage.cost_usd == 0.0 && observed.cost_usd > 0.0 {
                    usage.cost_usd = observed.cost_usd;
                }
            }
        }
        let raw_iteration_cap = progress_pump::hit_iteration_cap(&events);
        let halted_for_spend = spend_brake.and_then(|(cap_usd, halted)| {
            halted
                .load(std::sync::atomic::Ordering::SeqCst)
                .then(|| SpendHalt {
                    agent: self.agent_id.clone(),
                    spent_usd: usages.iter().map(|usage| usage.cost_usd).sum(),
                    cap_usd,
                })
        });
        // #988: a spend halt reads `hit_iteration_cap == false`.
        //
        // `brain.rs` emits the step-pause notice and the spend notice from
        // separate `if`s, on the stated grounds that one operator message can
        // run several turns and both facts may be owed — but that the two
        // "cannot both come from ONE turn" *because* this invariant holds.
        // The predicate itself cannot see the halt: it reads only the progress
        // stream, and a hook-driven halt is not in it. While the stream never
        // reported a cap at all the invariant held for free; now that it does,
        // it has to be stated here, where both facts are in hand, rather than
        // re-checked at each notice site.
        //
        // The halt wins because it is the more specific account of why the
        // turn stopped, and the two notices are not interchangeable: a step
        // pause invites "continue", which on a spent budget would invite the
        // operator to burn a cap that has already run out.
        let hit_iteration_cap =
            progress_pump::reportable_iteration_cap(raw_iteration_cap, halted_for_spend.is_some());
        if hit_iteration_cap {
            tracing::info!(
                agent = %self.agent_id,
                "[turn] paused at the tool-iteration cap; the reply is a resumable checkpoint, not a finished answer"
            );
        }
        if raw_iteration_cap && !hit_iteration_cap {
            tracing::info!(
                agent = %self.agent_id,
                "[turn] the iteration cap was reached on a turn already halted for spend; \
                 reporting the halt, which is why it stopped"
            );
        }
        if let Some(halt) = &halted_for_spend {
            tracing::info!(
                agent = %self.agent_id,
                spent_usd = halt.spent_usd,
                cap_usd = halt.cap_usd,
                "[turn] halted at the in-turn spend cap; the reply stops short of the work it was doing"
            );
        }
        let budget_paused = budget_pause_summary
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
            .map(|summary| BudgetPause {
                agent: self.agent_id.clone(),
                summary,
            });
        if let Some(pause) = &budget_paused {
            tracing::info!(
                agent = %self.agent_id,
                "[turn] paused for lack of inference budget/credits: {}",
                pause.summary
            );
        }
        let steps = steps::fold_steps(events);

        let outcome = reply.map(|reply| TurnOutcome {
            reply: if chat_only {
                chat_only_guard::guard_suppressed_reply(reply)
            } else {
                reply
            },
            steps,
            hit_iteration_cap,
            abnormal_stop: None,
            halted_for_spend,
            budget_paused,
        });
        (outcome, usages)
    }

    /// Restores the provider's own words to a failed attempt.
    ///
    /// The embedded runtime reports a failed hosted invocation as a fixed,
    /// caller-safe sentence; the provider error that actually happened —
    /// which is what `classify_turn` reads for the budget-pause and
    /// wall-clock classes — was seen by the bridge. The newest bridge error
    /// of this attempt is appended to the error so the wire-shape checks see
    /// it, and the runtime's sentence stays in front for the operator.
    fn unmask(&self, result: Result<String, openhuman_embed::CoreError>) -> anyhow::Result<String> {
        match result {
            // The runtime's deterministic summary for a turn whose model
            // produced nothing. When the bridge saw the provider FAIL behind
            // it, the turn did not finish empty — it failed, and the failure
            // is the provider's own error (a budget wall, an outage), which
            // `classify_turn` needs verbatim. With no provider error behind
            // it, it is the transient empty class the one-shot retry covers.
            Ok(reply) if reply.trim() == NO_RESULT_SENTINEL => {
                let seen = self.bridge.take_errors();
                match seen.last() {
                    Some(provider) => Err(anyhow::anyhow!(
                        "turn produced no result: provider error: {provider}"
                    )),
                    None => Ok(String::new()),
                }
            }
            Ok(reply) => Ok(reply),
            Err(err) => {
                let seen = self.bridge.take_errors();
                match seen.last() {
                    Some(provider) => Err(anyhow::anyhow!("{err}: provider error: {provider}")),
                    None => Err(anyhow::Error::from(err)),
                }
            }
        }
    }

    /// Whether a turn runs on a session of its own rather than the agent's
    /// conversation session: it names no chat, or brings its own context.
    fn isolated_session(turn_chat_id: Option<&str>, history_seed: bool) -> bool {
        turn_chat_id.is_none() || !history_seed
    }

    /// Everything the bridge tapped since the previous attempt, summed.
    fn tapped_usage(&self) -> TurnUsage {
        self.bridge
            .take_usage()
            .into_iter()
            .fold(TurnUsage::default(), |acc, call| TurnUsage {
                input_tokens: acc.input_tokens + call.input_tokens,
                output_tokens: acc.output_tokens + call.output_tokens,
                cached_input_tokens: acc.cached_input_tokens + call.cached_input_tokens,
                cost_usd: acc.cost_usd + call.cost_usd,
            })
    }

    /// This turn's in-turn spend ceiling, in USD — the value that
    /// [`BudgetStopHook`](oh::agent::stop_hooks::BudgetStopHook) halts the turn
    /// at, armed only when the teammate declares a `budget_usd_daily` cap
    /// (issue #988). `None` means no hook is installed.
    ///
    /// This mirrors the vendored runtime's own posture. OpenCompany's plan-level
    /// token ceiling and a teammate's `budget_usd_daily` are **pre-dispatch** —
    /// they decide whether to start a turn and cannot see inside one — and
    /// openhuman itself constructs `BudgetStopHook` nowhere, applying only an
    /// opt-in token-based goal hook. So this crate, like upstream, arms the
    /// in-turn brake only for a teammate who has opted into a budget: a declared
    /// `budget_usd_daily` cap also bounds any single turn of that teammate's, so
    /// the worst-case overshoot is "one daily cap" rather than "one turn, of
    /// unknown size". A teammate with no declared budget gets no hook — the
    /// runtime never hard-stops a turn that isn't actively burning a live budget
    /// — and there is no blanket magic number no operator can see or change.
    ///
    /// A non-finite or non-positive manifest value is ignored (no hook armed)
    /// rather than forwarded: the vendored hook fails closed on a malformed cap
    /// and would halt every turn at iteration one. Such a teammate is already
    /// refused before dispatch (`spent >= cap` holds at zero spend), so this only
    /// guards the path where no meter was available to make that call.
    fn turn_spend_cap_usd(&self) -> Option<f64> {
        match self.budget_usd_daily {
            Some(daily) if daily.is_finite() && daily > 0.0 => Some(daily),
            _ => None,
        }
    }

    /// Classify one `agent.turn` result for the retry wrapper.
    ///
    /// `elapsed` is how long THIS attempt ran, and is used only by the
    /// wall-clock-ceiling arm (issue #1680) — see
    /// [`wall_clock_ceiling_message`].
    fn classify_turn(&self, result: anyhow::Result<String>, elapsed: Duration) -> AttemptOutcome {
        match result {
            Ok(reply) if reply.trim().is_empty() => AttemptOutcome::Empty,
            Ok(reply) => AttemptOutcome::Reply(reply),
            Err(err) if is_transient_empty_response(&err) => AttemptOutcome::Empty,
            // Issue #1680: still Hard — a ceiling hit is not retryable and the
            // one-shot retry must not double a ten-minute failure — but told in
            // terms the operator can act on rather than the harness's own.
            Err(err) if is_wall_clock_ceiling(&err) => {
                AttemptOutcome::Hard(OpenCompanyError::Harness(wall_clock_ceiling_message(
                    &self.agent_id,
                    elapsed,
                    &err,
                )))
            }
            // Issue #1846: the top-level orchestrator's own inference call
            // carries no delegated-tool envelope, so it cannot be recognised by
            // `RepeatedToolFailureMiddleware`'s envelope-gated check — only by
            // matching the SAME underlying wire shape directly against the
            // error chain. Checked AFTER the wall-clock-ceiling arm so a
            // ceiling hit (which can itself carry provider response text) is
            // never re-read as a budget pause; checked BEFORE the generic
            // `Hard` catch-all, which is exactly the asymmetry this issue
            // closes — every other `Err` still falls through unchanged.
            Err(err) if is_top_level_budget_exhausted(&err) => AttemptOutcome::BudgetPaused {
                summary: budget_paused_summary(&self.agent_id, &err),
            },
            Err(err) => AttemptOutcome::Hard(OpenCompanyError::Harness(format!(
                "turn for '{}': {err}",
                self.agent_id
            ))),
        }
    }
}

/// Whether a turn error is the top-level analogue of the delegated sub-agent
/// budget halt (issue #1846): the model provider's response body, still
/// present somewhere in the `anyhow` error chain (`turn` returns
/// `anyhow::Result`, so the typed error is erased the same way
/// [`is_transient_empty_response`] and [`is_wall_clock_ceiling`] already
/// account for), matches the single existing budget-exhausted wire-shape
/// classifier.
///
/// Deliberately reuses `oh::api::classify::is_budget_exhausted_message`
/// rather than forking a second copy of the phrase list — the whole point of
/// this fix is to close the asymmetry, not add a second place for the two to
/// drift apart. See `budget_wire_shapes_all_classify_as_budget_paused` for the
/// drift-coupling test that fails CI if the two ever disagree.
fn is_top_level_budget_exhausted(err: &anyhow::Error) -> bool {
    oh::api::classify::is_budget_exhausted_message(&format!("{err:#}"))
}

/// UTF-8-safe truncation to at most `max` chars, appending a truncation marker
/// when cut. Mirrors the vendored `truncate_for_halt` this notice's copy is
/// modelled on (`tinyagents`' `RepeatedToolFailureMiddleware`), so a very long
/// provider error body cannot blow out the reply the operator sees.
fn truncate_for_pause(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}\n… [truncated]")
}

/// The actionable, operator-facing copy for a top-level budget pause (issue
/// #1846) — deliberately the SAME framing the delegated sub-agent halt already
/// emits (`terminal_inference_halt_summary`'s `BudgetExhausted` arm in the
/// vendored `tinyagents` middleware): "add credits and try again", not the
/// harness's own error vocabulary. Pinned equal in shape by
/// `top_level_budget_pause_copy_matches_the_delegated_halt_copy`.
///
/// Framed as a **turn** running out rather than a **tool step** — the
/// top-level orchestrator call is not a tool call, so there is no `{tool}`
/// name to name, unlike the delegated halt's "the `{tool}` step failed".
fn budget_paused_summary(agent_id: &str, err: &anyhow::Error) -> String {
    format!(
        "Paused — {agent_id}'s turn ran out of inference budget/credits, so it stopped \
         instead of failing silently. Add credits to your account (or, when using a \
         custom/BYO provider, top up that provider's own account), then resend your message \
         to continue. Details:\n{}",
        truncate_for_pause(&format!("{err:#}"), 600),
    )
}

/// The [`HarnessModel`] a **per-agent auxiliary** model pass (today: payload
/// extraction; any future one built inside [`build::build_agent_with_model`])
/// should run against, given that agent's own `{provider, model}` pin when it
/// has one (keys rework, issue #2306, X12; round-2 review comment
/// 4012457329).
///
/// Order, never silently swapped:
///
/// 1. the company default (`deps.provider`) is tried first, on every call;
/// 2. `pin`, when given, is tried only once the default's own call fails —
///    so a company running solely on per-agent pins (no default configured
///    at all) still gets these passes, instead of losing them outright to a
///    default that was never going to answer;
/// 3. neither: the caller sees the default's own error (`DefaultFirstModel`
///    below), the same failure it would have surfaced before this existed —
///    a non-essential pass reads that as "skip", an essential one surfaces
///    it through `company::inference::copy` exactly as the primary turn
///    model does.
///
/// Deliberately the **opposite** priority from the primary chat model's own
/// pin resolution (decision X8/F6, [`HarnessModel::pinned`]): a pinned
/// agent's primary turn fails closed on its own pin and never silently
/// reroutes to the default, because the pin is that agent's explicit choice.
/// An auxiliary pass has no choice of its own to honour — it is company
/// bookkeeping riding along with whichever agent happened to trigger it — so
/// it prefers the shared default and reaches for an agent's pin only as a
/// last resort to keep running at all.
///
/// Returns `deps.provider` unchanged when `pin` is `None`: there is nothing
/// to fall back to, and this is exactly the pre-X12 behaviour every
/// company-wide pass (title, triage, planning, selector) still gets — none
/// of them has a single agent to pin against (each is built once per company
/// in [`brain`](crate::harness::built_in::brain), before any agent is
/// chosen), so `pass_model` is not wired into them; their own "unreachable ⇒
/// skip" contract already satisfies point 3 above with no agent pin to fall
/// back to.
pub(crate) fn pass_model(
    deps: &HarnessDeps,
    pin: Option<Arc<dyn HarnessModel>>,
) -> Arc<dyn HarnessModel> {
    match pin {
        Some(pinned) => Arc::new(DefaultFirstModel {
            default: deps.provider.clone(),
            pin: pinned,
            served_from_pin: std::sync::atomic::AtomicBool::new(false),
        }),
        None => deps.provider.clone(),
    }
}

/// [`pass_model`]'s combinator: tries `default` on every call, falls back to
/// `pin` only when `default`'s own call errors, and reports telemetry for
/// whichever one actually answered the most recent call.
///
/// `served_from_pin` is sequenced by nothing stronger than the two `invoke`
/// calls' own ordering — the same approximate-under-concurrency bound
/// [`HarnessModel::telemetry_model`] already documents for a single shared
/// provider serving overlapping turns.
struct DefaultFirstModel {
    default: Arc<dyn HarnessModel>,
    pin: Arc<dyn HarnessModel>,
    served_from_pin: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl tinyinference::model::ChatModel<()> for DefaultFirstModel {
    fn profile(&self) -> Option<&tinyinference::model::ModelProfile> {
        self.default.profile()
    }

    async fn invoke(
        &self,
        state: &(),
        request: tinyinference::model::ModelRequest,
    ) -> tinyinference::Result<tinyinference::model::ModelResponse> {
        match self.default.invoke(state, request.clone()).await {
            Ok(response) => {
                self.served_from_pin
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                Ok(response)
            }
            Err(default_err) => match self.pin.invoke(state, request).await {
                Ok(response) => {
                    self.served_from_pin
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    Ok(response)
                }
                // The default's error is the actionable one — company config
                // an operator can fix in Connections — where the pin's own
                // failure is either the same story or a second, narrower one
                // this pass never asked to surface on its own.
                Err(_pin_err) => Err(default_err),
            },
        }
    }
}

impl HarnessModel for DefaultFirstModel {
    fn telemetry_provider_id(&self) -> String {
        if self
            .served_from_pin
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            self.pin.telemetry_provider_id()
        } else {
            self.default.telemetry_provider_id()
        }
    }

    fn telemetry_model(&self) -> Option<crate::metering::ModelSlug> {
        if self
            .served_from_pin
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            self.pin.telemetry_model()
        } else {
            self.default.telemetry_model()
        }
    }
}

/// Writes every attempt's spend of a **finished** turn to the ledger and the
/// usage meter, whether that turn succeeded or failed.
///
/// The one place `record_turn_cost` is called from the pool, so the two turn
/// paths cannot disagree about when a turn is metered. Both call it *before*
/// they unwrap the turn's own result — see
/// [`turn_result_after_metering`] for why that ordering is the fix and not an
/// accident of layout.
pub(crate) async fn meter_turn_costs(
    turn_costs: &[TurnUsage],
    agent_id: &str,
    company: &CompanyId,
    deps: &HarnessDeps,
    chat_model: &dyn HarnessModel,
    run_id: Option<&str>,
) -> crate::Result<()> {
    // Attribute cost to the provider and model this turn actually resolved to.
    // `chat_model` is the SAME [`HarnessModel`] the agent's `Agent` was built
    // against — `deps.provider` for an unpinned agent, or that agent's own
    // pinned [`TenantProvider`](crate::harness::provider::TenantProvider) when
    // the manifest names a `{provider, model}` pair. Reading `deps.provider`
    // unconditionally here booked every pinned agent's turn to the company
    // default's telemetry instead of the provider that actually served it
    // (issue #2306 / Codex round 2, comment 4012457318). With a per-tenant
    // `TenantProvider` a console BYOK switch changes the slug between turns,
    // so both are still read live rather than trusting a static value baked
    // at build. The model is folded onto the closed vocabulary at the
    // provider so no operator-authored model name reaches the meter (issue
    // #1749).
    let provider_slug = chat_model.telemetry_provider_id();
    let model_slug = chat_model.telemetry_model();
    for turn_cost in turn_costs {
        record_turn_cost(
            turn_cost,
            agent_id,
            &provider_slug,
            model_slug,
            company,
            deps.store.as_ref(),
            deps.meter.as_deref(),
            run_id,
        )
        .await?;
    }
    Ok(())
}

/// Resolves a metered turn into the one error that should propagate.
///
/// A turn's own failure outranks a metering failure. The turn is the thing the
/// operator asked for and its error is the one that explains what they see; a
/// ledger write that also failed is a second, quieter problem, and letting it
/// replace the first would report "could not append to the ledger" for a run
/// that actually hit its wall-clock ceiling.
///
/// The reverse case is not symmetric: when the turn *succeeded*, a metering
/// failure is the only failure there is, and it still propagates — losing a
/// ledger entry silently is the class of bug this whole path exists to close.
fn turn_result_after_metering(
    outcome: crate::Result<TurnOutcome>,
    metered: crate::Result<()>,
    company: &CompanyId,
    agent_id: &str,
) -> crate::Result<TurnOutcome> {
    match outcome {
        Err(turn_error) => {
            if let Err(meter_error) = metered {
                tracing::warn!(
                    company = %company,
                    agent = %agent_id,
                    error = %meter_error,
                    "[cost] could not meter a failed turn's spend; reporting the turn's own error"
                );
            }
            Err(turn_error)
        }
        Ok(outcome) => metered.map(|()| outcome),
    }
}

/// Whether a turn error is the transient empty-response class openhuman raises
/// instead of a silent blank reply. Matched on the error chain's message
/// (`turn` returns `anyhow::Result`, so the typed `AgentError` is erased):
/// "The model returned an empty response…".
fn is_transient_empty_response(err: &anyhow::Error) -> bool {
    format!("{err:#}")
        .to_ascii_lowercase()
        .contains("empty response")
}

/// The two phrasings `TinyAgentsError::Timeout` uses when the run's wall-clock
/// budget expires around a call.
///
/// Copied from the vendored crate's own list — `web_errors::is_turn_timeout_error`
/// anchors on exactly these — rather than invented here, so a phrasing added
/// upstream is a diff against a known set instead of a silent miss.
const WALL_CLOCK_CEILING_LEAVES: [&str; 2] = [
    "exceeded its remaining wall-clock budget",
    "exceeded its wall-clock deadline",
];

/// Whether a turn error is the harness's per-turn wall-clock ceiling firing
/// (issue #1680).
///
/// Matched on the error chain's message for the same reason
/// [`is_transient_empty_response`] is: `turn` returns `anyhow::Result`, so the
/// typed error is erased by the time it reaches us. The leaf reads
/// "…exceeded its remaining wall-clock budget (56636 ms)", raised by
/// `with_call_budget` in the vendored tinyagents harness.
///
/// **Whole phrases, not the words `wall-clock budget`.** A provider's response
/// body reaches this chain verbatim — `provider.rs` raises
/// `InferenceError::Model(format!("hosted inference returned {status}: {text}"))`
/// — so a hosted or BYOK endpoint that says anything about a wall-clock budget
/// of its own would otherwise be reported as this ceiling, complete with a
/// measured duration of a second or two and an instruction to raise
/// `OPENHUMAN_AGENT_TURN_TIMEOUT_SECS`, which would fix nothing. Being wrong in
/// that direction is worse than the bare wrapper this replaces, because it
/// reads as a diagnosis.
fn is_wall_clock_ceiling(err: &anyhow::Error) -> bool {
    let chain = format!("{err:#}").to_ascii_lowercase();
    WALL_CLOCK_CEILING_LEAVES
        .iter()
        .any(|leaf| chain.contains(leaf))
}

/// A duration as an operator reads one: `9s`, `1m 30s`, `10m 01s`.
fn humanise_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        return format!("{secs}s");
    }
    format!("{}m {:02}s", secs / 60, secs % 60)
}

/// What an operator should be told when a turn hits the wall-clock ceiling
/// (issue #1680).
///
/// ## Why this message exists at all
///
/// The harness's own leaf reads "model call for run 'agent_turn' exceeded its
/// remaining wall-clock budget (56636 ms)", and every part of that is true and
/// almost every part of it misleads.
///
/// The ceiling is a **whole-turn** bound, armed as the harness policy's
/// `max_wall_clock_ms` and checked as `ceiling − Instant::elapsed()` since the
/// run began. Model time is therefore fully counted against it, as is tool
/// time, sub-agent time and retry backoff. But the number the harness prints is
/// the budget that **remained** when the offending call was issued, not that
/// call's duration and not the ceiling — so a turn that genuinely ran for the
/// full ten minutes reports a figure ten times smaller than the limit it hit,
/// and reads as though one slow model call were at fault.
///
/// That reading is what issue #1680 was filed on: a node that had spent about
/// nine minutes before its last model call even started was diagnosed as a 56
/// second budget being too tight. Nothing about the mechanism was wrong; the
/// only defect was that its report could not be read correctly.
///
/// ## Why the underlying error is kept
///
/// Appended verbatim rather than replaced. It is the only thing that names
/// which call was in flight when the ceiling fired, and a bug report that has
/// lost it is worse than a wordy one. The console strips known wrapper prefixes
/// (`run-error-message.ts`) and leaves this leaf intact.
///
/// Rendered `{err:#}` — the whole chain — rather than `{err}`, which is only
/// the outermost context. Today's leaf happens to arrive flattened
/// (`vendor/openhuman/.../agent/tinyagents/mod.rs` interpolates it into a
/// single `anyhow!`), but [`is_wall_clock_ceiling`] already searches `{err:#}`
/// precisely because a chained one is possible; the two halves must agree, and
/// only one of them is safe if it is. On a flat error the two render
/// identically, so this costs nothing to be right about.
///
/// ## Why the ceiling's value is not quoted
///
/// `DEFAULT_AGENT_TURN_TIMEOUT_SECS` is private to the vendored openhuman crate
/// and cannot be read from here. Restating `600` would be a copy that silently
/// goes stale on the next vendored bump — the elapsed time is measured, and the
/// knob's NAME is a fact independent of its value, so both can be stated
/// honestly while the number cannot.
///
/// ## Why it hedges about the figure
///
/// "**any** millisecond figure below", not "the figure below", because the two
/// spellings this classifies do not both carry one. `with_call_budget` raises
/// `… exceeded its remaining wall-clock budget (56636 ms)`; the run loop and the
/// tool loop raise `run \`agent_turn\` exceeded its wall-clock deadline`, which
/// has no number in it at all. Pointing at a figure that is not there would
/// reintroduce this issue's own defect one spelling over — an accurate sentence
/// that cannot be followed. One clause covers both; a branch on the spelling
/// would be two messages to keep true.
///
/// ## Why it is not longer than this
///
/// `RunHistoryPanel` renders a journaled run's error as the row's **headline**
/// sentence, so every extra clause is a wall of red text over the run the
/// operator is trying to read. Three facts earn their place — what the turn
/// spent, that the harness's number is a remainder, and which knob moves the
/// ceiling. The rest of the explanation belongs in
/// `docs/spec/runtime/harnesses.md`, not in every failed run.
fn wall_clock_ceiling_message(agent_id: &str, elapsed: Duration, err: &anyhow::Error) -> String {
    format!(
        "turn for '{agent_id}' hit the harness's per-turn wall-clock ceiling after {}. \
         The ceiling bounds the whole turn, model time included, so any millisecond \
         figure below is the budget that REMAINED when the last call started — not a \
         limit on that call. Give this step less to do, or raise the ceiling with \
         OPENHUMAN_AGENT_TURN_TIMEOUT_SECS. Underlying error: {err:#}",
        humanise_elapsed(elapsed)
    )
}

/// What a workspace-ensure attempt should say, given what the last attempt for
/// the same agent said (issue #449).
///
/// The attempt itself is per dispatch and stays that way — see
/// [`note_workspace_attempt`](HarnessPool::note_workspace_attempt) for why
/// memoising it is the wrong fix. Only the *reporting* is edge-triggered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkspaceReport {
    /// The first failure since this agent was last healthy: report it.
    Failed,
    /// Still failing, and already reported: say nothing.
    StillFailing,
    /// Working again after a reported failure: say so once, so a reader who saw
    /// the error learns it ended.
    Recovered,
    /// Working, and was already working: say nothing.
    StillHealthy,
}

impl WorkspaceReport {
    /// Whether this transition has anything to log at all.
    pub(crate) fn is_silent(self) -> bool {
        matches!(self, Self::StillFailing | Self::StillHealthy)
    }
}

/// Folds one attempt's outcome into the set of currently-failing keys and
/// returns what to report.
///
/// Pure but for the `failing` set it edits, so the whole state machine is
/// testable without a model, a roster or a filesystem. `failing` holds exactly
/// the keys whose last attempt failed **and** whose failure has been reported;
/// `failed` is this attempt's outcome.
fn workspace_report<K>(failing: &mut HashSet<K>, key: &K, failed: bool) -> WorkspaceReport
where
    K: std::hash::Hash + Eq + Clone,
{
    if failed {
        // `insert` returns false when the key was already there — i.e. the
        // previous attempt failed and was already reported.
        if failing.insert(key.clone()) {
            WorkspaceReport::Failed
        } else {
            WorkspaceReport::StillFailing
        }
    } else if failing.remove(key) {
        WorkspaceReport::Recovered
    } else {
        WorkspaceReport::StillHealthy
    }
}

/// A pool of live agents, one roster per company.
pub struct HarnessPool {
    agents: RwLock<HashMap<CompanyId, Vec<Arc<CompanyAgent>>>>,
    /// The process-wide `opencompany` MCP host every roster agent is served
    /// on (plan hive-desks Phase 3): its listener, its bearers, and the
    /// in-flight turn registry the hive driver attributes calls through.
    mcp: Arc<McpHost>,
    monthly_budgets: RwLock<HashMap<CompanyId, Option<f64>>>,
    /// Fingerprint of the effective MCP server set the cached roster was built
    /// from, keyed by company. Drives MCP-freshness: [`ensure`](Self::ensure)
    /// rebuilds the roster whenever the fingerprint changes.
    mcp_fingerprints: RwLock<HashMap<CompanyId, u64>>,
    /// Fingerprint of the overlay-agent set (issue #71 — Active Runtime
    /// Teammates) the cached roster was built from, keyed by company. Drives
    /// overlay-agent freshness: [`ensure`](Self::ensure) rebuilds the roster
    /// whenever an operator- or orchestrator-added teammate is added/removed,
    /// mirroring the MCP-freshness fingerprint above.
    overlay_fingerprints: RwLock<HashMap<CompanyId, u64>>,
    /// Fingerprint of the resolved [`CapabilityFilter`](toolbelt::CapabilityFilter)
    /// the cached roster was built from, keyed by company (issue #108). Drives
    /// capability-budget freshness: [`ensure`](Self::ensure) re-resolves the
    /// tenant's filter from the [`UsageMeter`] on every call and rebuilds the
    /// roster whenever the denied-namespace set changes — so a tier that crosses
    /// its token budget switches off on the company's **next** turn. With no
    /// plan ([`HarnessDeps::plan`] `None`) the filter is the static
    /// [`HarnessDeps::capabilities`], whose fingerprint never moves — no rebuild,
    /// byte-identical to Cell A.
    capability_fingerprints: RwLock<HashMap<CompanyId, u64>>,
    /// Fingerprint of the resolved per-tenant [`TenantComposio`](composio::TenantComposio)
    /// config the cached roster was built from, keyed by company (issue #110).
    /// Drives Composio-freshness: [`ensure`](Self::ensure) re-resolves the token
    /// (+ toolkit allowlist) from the [`SecretStore`] on every call and rebuilds
    /// the roster whenever it changes — so a console token set/rotate/clear takes
    /// effect on the company's **next** turn with no restart. With no secret
    /// store wired the config is the static [`HarnessDeps::composio`], whose
    /// fingerprint never moves.
    composio_fingerprints: RwLock<HashMap<CompanyId, u64>>,
    /// Fingerprint of the billing connections (Chargebee #788, PayPal #789) the
    /// cached roster was built from, keyed by company.
    ///
    /// Without this axis a credential saved from the console reaches nothing
    /// until a restart — the roster is cached, so `build_agent` is never called
    /// again to notice it. That was live for both integrations until the tools
    /// were observed missing from an agent whose settings page said "Connected".
    billing_fingerprints: RwLock<HashMap<CompanyId, u64>>,
    /// Stable company-only managed Search backends for deployments with no
    /// environment credential. Keeping the backend (and therefore its shared
    /// daily-call ledger) here prevents an unrelated roster rebuild from
    /// resetting that company's cap.
    managed_search_backends: RwLock<HashMap<CompanyId, search::SearchBackend>>,
    /// Fingerprint of the operator skill-delta set the cached roster was built
    /// from, keyed by company (issue #41). Drives skill-delta freshness:
    /// [`ensure`](Self::ensure) re-fetches the deltas from the
    /// [`SkillStateStore`](crate::ports::skills_state::SkillStateStore) on every
    /// call and rebuilds the roster whenever they change — so a skill
    /// authored / edited / enabled / disabled in the console Skills tab reaches
    /// the agent on the company's **next** turn with no restart. Without this
    /// axis the four fingerprints above are all stable on a skills-only change,
    /// the fast path returns early, and the new skill never surfaces until a
    /// process restart (the regression this fixes). With no skill store wired
    /// the delta set is always empty — stable fingerprint, no rebuild.
    skill_fingerprints: RwLock<HashMap<CompanyId, u64>>,
    /// Fingerprint of the operator budget-override set the cached roster was
    /// built from, keyed by company (issue #343). Drives budget freshness:
    /// [`ensure`](Self::ensure) re-resolves the overrides from
    /// [`HarnessDeps::store`] on every call and rebuilds the roster whenever a
    /// cap is set, changed, cleared or reset — so a budget edited on the console
    /// Team page reaches the dispatch gate and the per-agent
    /// [`ApprovalPolicy`](policy::ApprovalPolicy) on the company's **next** turn,
    /// with no restart and no redeploy. That is the entire point of #343: the
    /// cap is enforced from the roster, and without this axis every other
    /// fingerprint is stable on a budget-only change, so the fast path would
    /// reuse a roster still carrying the old cap until the process restarted.
    /// A company that never sets an override keeps an empty set and a stable
    /// fingerprint — no rebuild, byte-identical to the pre-#343 behaviour.
    budget_fingerprints: RwLock<HashMap<CompanyId, u64>>,
    /// Fingerprint of the operator per-agent persona-override set the cached
    /// roster was built from, keyed by company (issue #1530). Drives persona
    /// freshness: [`ensure`](Self::ensure) re-resolves the overrides from
    /// [`HarnessDeps::store`] on every call and rebuilds the roster whenever a
    /// persona is edited, cleared or reset — so an instructions edit on the
    /// console Team page reaches the agent's system prompt on the company's
    /// **next** turn, with no restart and no redeploy. Needed for the same reason
    /// as [`Self::budget_fingerprints`]: the persona is assembled once per roster,
    /// not once per call, so without this axis every other fingerprint is stable
    /// on a persona-only change and the fast path would keep serving the old
    /// instructions until the process restarted. A company that never edits a
    /// persona keeps an empty set and a stable fingerprint — no rebuild,
    /// byte-identical to the pre-#1530 behaviour.
    override_fingerprints: RwLock<HashMap<CompanyId, u64>>,
    /// Per-company fingerprint of the company's display name (PR #1875 review
    /// finding). `build_roster` embeds `manifest.company.name` into every
    /// agent's persona, and this is the axis that catches a `PATCH {scope}`
    /// rename the same way [`Self::override_fingerprints`] catches a
    /// per-agent persona edit — see [`company_name_fingerprint`]'s own doc
    /// comment for the full staleness story.
    company_name_fingerprints: RwLock<HashMap<CompanyId, u64>>,
    /// Per-company fingerprint of the operator `[policy]` override (issue #562),
    /// so a console tier change rebuilds the roster instead of waiting for a
    /// restart. Without this axis the override persists and is silently ignored:
    /// `ApprovalPolicy` is built once per roster, not once per call.
    policy_fingerprints: RwLock<HashMap<CompanyId, u64>>,
    /// The last cycle-start policy snapshot pinned to a company's roster via
    /// [`ensure_with_policy`](Self::ensure_with_policy), keyed by company.
    ///
    /// A cycle holds the runtime's serial lock, so its own `ensure_with_policy`
    /// install and the dispatch that follows cannot be interleaved by another
    /// cycle. The workflow runner is not a cycle caller: it drives turns from a
    /// spawned task with a plain live [`ensure`](Self::ensure), so without this
    /// pin it could adopt a mid-cycle console override a turn early — replacing
    /// the cycle's pinned roster with a looser one before `run_inner` clones its
    /// agent, and running one turn with the harness gate auto-approving what the
    /// native gate still parks (issue #1455). A live `ensure` therefore rebuilds
    /// the policy axis against the pin while one is active. The pin is released
    /// when the cycle ends ([`Self::end_cycle`]), so it covers exactly the
    /// cycle's own turns: a standalone workflow turn *between* cycles rebuilds
    /// against the live store overlay, not a snapshot that would otherwise stay
    /// stale until an unrelated cycle refreshed it.
    ///
    /// A `std::sync::Mutex` rather than a `tokio::sync::RwLock` like its
    /// neighbours: every critical section is a single lookup / insert / remove
    /// with no `await` in it, and the synchronous form is what lets a cycle's
    /// drop guard release the pin on cancellation or panic — an async lock
    /// could not be touched from `Drop` (issue #1455).
    pinned_policies: std::sync::Mutex<HashMap<CompanyId, Policy>>,
    /// Per-company fingerprint of the desk scoping a roster's grants resolve
    /// through — which desks exist, who sits on them, and each one's tool
    /// ceiling.
    ///
    /// Needed for the same reason as [`Self::budget_fingerprints`]: a tool belt
    /// is wired once per roster, not once per call, so without this axis a
    /// console desk-ceiling edit (or seating a teammate on a restricted desk)
    /// would leave every other fingerprint stable and the fast path would keep
    /// serving the old belt until the process restarted. A company whose desks
    /// declare no ceilings keeps a stable fingerprint and never rebuilds on this
    /// axis.
    desk_fingerprints: RwLock<HashMap<CompanyId, u64>>,
    /// Per-company fingerprint of the `[tools].allow` a roster's belts are wired
    /// from — the seed's grants **plus** the namespaces an operator granted from
    /// a connect surface (issue #1796).
    ///
    /// Needed for the same reason as [`Self::desk_fingerprints`], and it is what
    /// makes the one-click grant mean anything: a belt is wired once per roster,
    /// so without this axis an operator who connected Chargebee and granted it
    /// would watch the page flip to "Connected" while every teammate kept the
    /// belt built before the grant — until the process restarted. That is the
    /// same "Connected and reaching nobody" the grant was clicked to end.
    ///
    /// A company with no console grants keeps a stable fingerprint (it hashes
    /// the effective list, which is then just the seed's) and never rebuilds on
    /// this axis.
    grants_fingerprints: RwLock<HashMap<CompanyId, u64>>,
    /// Per-company fingerprint of the routed workspace documents — hashed over
    /// their **bodies**, not merely their names.
    ///
    /// A persona is assembled once per roster, so without this axis an operator
    /// editing a routed note would leave every other fingerprint stable and the
    /// fast path would keep serving a prompt quoting the old text until the
    /// process restarted. Hashing the names alone would have exactly that bug,
    /// since the routing table does not move when a document's contents do —
    /// which is the whole reason the routing layer is worth having.
    ///
    /// A company with no workspace store wired, or whose roles route nothing,
    /// keeps a stable fingerprint and never rebuilds on this axis.
    context_fingerprints: RwLock<HashMap<CompanyId, u64>>,
    /// The memory-engine selection the company's cached roster was built
    /// against, keyed by company (issue #1113).
    ///
    /// `Some(fp)` for a provider-backed engine — a fingerprint of its
    /// memory-family ports — and `None` for the base backend. Recorded by
    /// [`RuntimeBuilder::build`](crate::runtime::RuntimeBuilder::build) on every
    /// build that re-applies the engine selection so the next rebuild can tell
    /// a live engine swap from a no-op;
    /// [`ensure`](Self::ensure) does not know the selection, so the pool needs
    /// this bookkeeping of its own.
    ///
    /// Missing on the boot path and when no build has run yet, `None` is also
    /// the base backend's marker — `get(company).copied().flatten()` makes an
    /// absent row and a recorded `None` indistinguishable, which is correct: a
    /// roster built over the base backend must be dropped exactly when a swap
    /// binds a provider engine, and a build that re-applies the base backend
    /// must keep it.
    memory_engine: RwLock<HashMap<CompanyId, Option<u64>>>,
    /// The `(company, agent)` pairs whose last workspace-ensure failed and whose
    /// failure has already been reported (issue #449).
    ///
    /// Not a memo of the *attempt* — see
    /// [`note_workspace_attempt`](Self::note_workspace_attempt). Purely a record
    /// of what has already been said, so an unmountable volume produces one
    /// error line instead of one per turn forever.
    ///
    /// A `std::sync::Mutex` rather than a `tokio::sync::RwLock` like its
    /// neighbours: the critical section is a single hash lookup with no `await`
    /// in it, so the async lock would buy nothing and cost a scheduling point on
    /// the dispatch path.
    workspace_failures: std::sync::Mutex<HashSet<(CompanyId, String)>>,
}

impl Default for HarnessPool {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether a turn tees its progress onto the live [`turn_stream`](crate::turn_stream)
/// bus, and if so which chat thread its frames route to. `Off` for a turn with no
/// operator chat bubble (a dispatched task card or workflow agent node) — those
/// frames would misattribute to whatever thread most recently sent, so they
/// publish nothing (#125 review). `On { chat_id }` streams; `chat_id` is the
/// thread the durable reply is journaled under (`AgentReply.chat_id`), falling
/// back to the default desk when the caller addressed none.
#[derive(Clone, Copy)]
enum LiveStream<'a> {
    Off,
    On {
        /// Where this turn's transient frames are published — and **only**
        /// that, since issue #1890 I.
        ///
        /// The thread used to ride here too, on the argument that this variant
        /// "is already the turn's chat identity and not only its stream key".
        /// That conflation is what I removes: identity now travels on the
        /// `ChatTarget` the caller passes, so a turn can have a conversation
        /// and stream nothing — which an approval's re-issued call does, and
        /// which this enum could not express.
        ///
        /// The history seed (issue #1840) reads that same `ChatTarget`, for the
        /// same reason: whether a turn is seeded is a fact about the
        /// conversation it is in, not about whether anything is watching it.
        chat_id: Option<&'a str>,
    },
    /// A workflow agent node (issue #1702): it streams live like `On`, but its
    /// frames route by the workflow run + node rather than a chat thread — the
    /// node has no chat bubble, and the console's run-trace sheet keys the live
    /// timeline on the run. This is what makes a node's tool calls appear live
    /// without misattributing to whatever thread most recently sent (#125).
    Workflow {
        run_id: &'a str,
        node_id: &'a str,
    },
}

/// Per-company serialization of the roster's policy-axis decision through its
/// publish ([`HarnessPool::ensure_impl`]), mirroring
/// [`company_write_lock`](crate::ports::store::company_write_lock).
///
/// The window it closes (issue #1455): a plain `ensure` from the workflow
/// runner can read the pool's pin map as empty *before* a concurrent cycle's
/// `ensure_with_policy` installs its snapshot, then publish a roster rebuilt
/// from the looser live policy *after* the cycle's pinned roster — running one
/// turn with the harness gate auto-approving what the native gate still parks.
/// Holding this lock from the pin read through the roster publish makes the
/// two operations atomic per company, so a plain ensure either publishes
/// before the cycle pins or reads the pin afterwards and rebuilds against it.
/// Keyed globally rather than per pool because a company's cycle and its
/// workflow ensure can arrive on different lanes of the same router; the extra
/// serialization across lanes is harmless (ensures are idempotent warm-ups).
static POLICY_AXIS_LOCKS: std::sync::LazyLock<
    std::sync::Mutex<HashMap<CompanyId, Arc<tokio::sync::Mutex<()>>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Returns (or creates) the per-company policy-axis lock for `company`.
fn policy_ensure_lock(company: &CompanyId) -> Arc<tokio::sync::Mutex<()>> {
    let mut map = POLICY_AXIS_LOCKS.lock().expect("policy axis locks");
    map.entry(company.clone())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

static MONTHLY_SPEND_LOCKS: std::sync::LazyLock<
    std::sync::Mutex<HashMap<CompanyId, std::sync::Weak<tokio::sync::Mutex<()>>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

fn monthly_spend_lock(company: &CompanyId) -> Arc<tokio::sync::Mutex<()>> {
    let mut locks = MONTHLY_SPEND_LOCKS.lock().expect("monthly spend locks");
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(company).and_then(std::sync::Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(company.clone(), Arc::downgrade(&lock));
    lock
}

/// What the total-ceiling gate decided.
///
/// Not a `Result`: a refusal is an ordinary outcome of asking rather than an
/// error, and `TurnOutcome` is large enough that carrying it in an `Err` is a
/// lint in its own right.
enum CeilingGate {
    /// Dispatch may proceed. The reservation — present whenever there is a
    /// ceiling to reserve against — must be held for the turn.
    Admitted(Option<crate::metering::TokenReservation>),
    /// Dispatch is refused; this is the turn's outcome.
    Refused(TurnOutcome),
}

enum MonthlyBudgetGate {
    Admitted(Option<tokio::sync::OwnedMutexGuard<()>>),
    Refused(TurnOutcome),
}

impl HarnessPool {
    /// Builds an empty pool.
    pub fn new() -> Self {
        Self {
            agents: RwLock::new(HashMap::new()),
            mcp: crate::hive::mcp_server::global(),
            monthly_budgets: RwLock::new(HashMap::new()),
            mcp_fingerprints: RwLock::new(HashMap::new()),
            overlay_fingerprints: RwLock::new(HashMap::new()),
            capability_fingerprints: RwLock::new(HashMap::new()),
            composio_fingerprints: RwLock::new(HashMap::new()),
            billing_fingerprints: RwLock::new(HashMap::new()),
            managed_search_backends: RwLock::new(HashMap::new()),
            skill_fingerprints: RwLock::new(HashMap::new()),
            budget_fingerprints: RwLock::new(HashMap::new()),
            override_fingerprints: RwLock::new(HashMap::new()),
            company_name_fingerprints: RwLock::new(HashMap::new()),
            policy_fingerprints: RwLock::new(HashMap::new()),
            pinned_policies: std::sync::Mutex::new(HashMap::new()),
            desk_fingerprints: RwLock::new(HashMap::new()),
            grants_fingerprints: RwLock::new(HashMap::new()),
            context_fingerprints: RwLock::new(HashMap::new()),
            memory_engine: RwLock::new(HashMap::new()),
            workspace_failures: std::sync::Mutex::new(HashSet::new()),
        }
    }

    /// Records one workspace-ensure outcome for `(company, agent)` and returns
    /// what it should say.
    ///
    /// **The attempt stays per dispatch.** The obvious fix for a repeating log
    /// line — remember that this agent's workspace was already handled and stop
    /// trying — is the wrong one in both directions, and this is why the
    /// suppression is on the reporting rather than on the work:
    ///
    /// * Memoising **success** means a data dir wiped or restored *after* the
    ///   first successful turn is never noticed again, and every relative file
    ///   write is refused for the life of the process — the exact regression
    ///   issue #409 added the per-dispatch retry to prevent.
    /// * Memoising **failure** means a volume that mounts a second late never
    ///   recovers, because nothing ever tries again.
    ///
    /// Both trade a noisy log for a broken agent. The retry is cheap (two
    /// syscalls on the already-exists path, against a turn about to call a
    /// model) and it is what makes the condition self-healing, so it keeps
    /// running every time. What changes is that a persistent failure is stated
    /// once rather than once per turn.
    fn note_workspace_attempt(
        &self,
        company: &CompanyId,
        agent_id: &str,
        failed: bool,
    ) -> WorkspaceReport {
        let key = (company.clone(), agent_id.to_string());
        let mut failing = self
            .workspace_failures
            .lock()
            .expect("workspace-failure set poisoned");
        workspace_report(&mut failing, &key, failed)
    }

    /// Ensures a company's roster is built and cached.
    ///
    /// **MCP-freshness (the error-hardening cell)**: on every call, the effective
    /// MCP server set is re-resolved (from the [`SecretStore`] when
    /// [`HarnessDeps::secrets`] is wired, else the boot-resolved
    /// [`HarnessDeps::mcp_servers`]) and fingerprinted. The roster is rebuilt when
    /// it is absent **or** the fingerprint changed — so a console MCP
    /// add/remove/enable-toggle reaches the agent on its **next turn**, with no
    /// company restart (the "Parallel Search / BrowserBase" bug). When nothing
    /// changed, the cached roster is reused (the common fast path), exactly as
    /// before.
    ///
    /// **Overlay-agent freshness (issue #71)**: the live overlay-agent set is
    /// re-resolved and fingerprinted the same way, from [`HarnessDeps::store`]
    /// rather than the (possibly stale) `company` snapshot passed in — so a
    /// teammate added through the console `POST .../team` route or the
    /// orchestrator's `add_agent` tool becomes a real, addressable roster agent
    /// on the company's **next** `ensure` call, with no restart.
    ///
    /// **Skill-delta freshness (issue #41)**: the operator skill deltas are
    /// fetched from [`HarnessDeps::skills`] and fingerprinted **before** the
    /// fast-path staleness check (not after it, as they were — the regression),
    /// so a skill authored / edited / enabled / disabled in the console Skills
    /// tab rebuilds the roster and reaches the agent on its **next** turn, even
    /// when every other axis (MCP, overlay, capability, composio) is unchanged.
    /// With no skill store wired the delta set is empty and the fingerprint is
    /// stable — no rebuild, exactly as before.
    ///
    /// **Budget freshness (issue #343)**: the operator's per-teammate daily
    /// spend caps ride the same live [`HarnessDeps::store`] read as the overlay
    /// agents and are fingerprinted alongside them, so a cap set, raised,
    /// cleared or reset from the console Team page rebuilds the roster and is
    /// enforced on the company's **next** dispatch. Nothing downstream had to
    /// change for this: the L1 gate in [`Self::run`] reads
    /// [`CompanyAgent::budget_usd_daily`] and the policy arm reads the
    /// [`ApprovalPolicy`](policy::ApprovalPolicy) both roster-built here, so
    /// rebuilding the roster *is* the enforcement update. That is what makes
    /// "no restart, no redeploy" a property of the design rather than a claim.
    pub async fn ensure(&self, company: &CompanyRecord, deps: &HarnessDeps) -> crate::Result<()> {
        self.ensure_impl(company, deps, None).await
    }

    /// [`ensure`](Self::ensure) with the policy axis pinned to an explicit
    /// cycle-start snapshot instead of the live store overlay.
    ///
    /// The runtime's native gate is re-applied from the record loaded at the
    /// top of a cycle, and this is the same snapshot: a console policy override
    /// that lands mid-turn (after that load, before the harness's own refresh)
    /// must reach *neither* gate until the next cycle boundary. Letting the
    /// roster pick it up early would run one turn with the harness
    /// auto-approving what the native gate parks (issue #1455).
    pub async fn ensure_with_policy(
        &self,
        company: &CompanyRecord,
        deps: &HarnessDeps,
        policy: &Policy,
    ) -> crate::Result<()> {
        self.ensure_impl(company, deps, Some(policy)).await
    }

    /// Release a cycle's policy pin, restoring the live-store policy axis for
    /// plain [`ensure`](Self::ensure) calls.
    ///
    /// The pin exists to keep a cycle's in-flight roster on the snapshot the
    /// native gate was re-applied from (see [`Self::ensure_with_policy`]); once
    /// the cycle is over — success or error — nothing in flight needs the
    /// snapshot any more. Without this release a stale pin would survive until
    /// an unrelated cycle refreshed it, so a standalone workflow turn between
    /// cycles would keep rebuilding against the last cycle's tier even after
    /// the operator moved the store (issue #1455).
    pub async fn end_cycle(&self, company: &CompanyId) {
        self.pinned_policies.lock().unwrap().remove(company);
    }

    /// The synchronous half of [`end_cycle`](Self::end_cycle), for a cycle's
    /// drop guard.
    ///
    /// A cycle whose future is cancelled or unwinds through a panic after
    /// [`ensure_with_policy`](Self::ensure_with_policy) installed its pin never
    /// reaches the async `end_cycle` — the `await` that would have called it is
    /// exactly where the future is dropped. The pin would then outlive the
    /// cycle and keep a standalone workflow turn between cycles on a stale
    /// snapshot until an unrelated cycle replaced it. Releasing here is a
    /// synchronous map removal, so the guard can do it from `Drop` (issue
    /// #1455). Idempotent with `end_cycle`; callers may run either or both.
    pub fn release_policy_pin_sync(&self, company: &CompanyId) {
        self.pinned_policies.lock().unwrap().remove(company);
    }

    async fn ensure_impl(
        &self,
        company: &CompanyRecord,
        deps: &HarnessDeps,
        policy_snapshot: Option<&Policy>,
    ) -> crate::Result<()> {
        self.monthly_budgets
            .write()
            .await
            .insert(company.id.clone(), company.manifest.budget.monthly_usd);

        // Re-resolve + fingerprint the effective MCP set (cheap; no rebuild yet).
        let effective_mcp = self.resolve_effective_mcp(company, deps).await;
        let mcp_fp = mcp_fingerprint(&effective_mcp);

        // Re-resolve + fingerprint the live overlay-agent set the same way, and
        // the operator budget overrides riding the same store read (issue #343).
        let overlay = self.resolve_effective_overlay(company, deps).await;
        let overlay_fp =
            overlay_fingerprint(&overlay.agents, &overlay.agent_edits, &overlay.retired);
        let budget_fp = budget_fingerprint(&overlay.budgets);
        // Issue #1530: the persona overrides ride the same store read, and go
        // stale the same way a budget does — the persona is assembled once per
        // roster, so an edit unseen by any fingerprint would not reach the
        // system prompt until a restart.
        let override_fp = override_fingerprint(&overlay.agent_edits);
        // PR #1875 review finding: the company's display name rides the same
        // store read as the overlays above and goes stale the same way — see
        // `company_name_fingerprint`'s own doc comment.
        let company_name_fp = company_name_fingerprint(&overlay.company_name);
        // The policy axis. A cycle pins it to the snapshot the native gate was
        // re-applied from (so a mid-turn override reaches neither gate), and the
        // pin is released when the cycle ends. A plain `ensure` — the workflow
        // runner's cadence — reuses that pin while one is active, so a spawned
        // workflow turn cannot adopt a live override a turn early (issue #1455);
        // between cycles, no pin is active and the live overlay applies. Either
        // way the fingerprint covers the effective mode/list/cap values — not a
        // relative override — so a manifest `[policy]` edit moves the cache key
        // even when no override is stored (or a redundant one was carried and
        // cleared), and the roster cannot keep an `ApprovalPolicy` built under a
        // tier the native gate no longer enforces.
        //
        // Issue #1455: the pin decision and the roster publish below must be
        // mutually exclusive with every other ensure for this company. A plain
        // `ensure` that read no pin *before* the cycle installed one could
        // otherwise finish rebuilding the shared roster from the looser live
        // policy *after* the cycle's pinned ensure had published the strict
        // roster — leaving the harness gate auto-approving what the native gate
        // still parks. The per-company lock closes that window: either the plain
        // ensure publishes first and the cycle's strict roster supersedes it, or
        // it runs after the pin is installed and rebuilds against the pin.
        let _policy_axis_lock = policy_ensure_lock(&company.id);
        let _policy_axis_guard = _policy_axis_lock.lock().await;
        let (effective_snapshot, pin_to_store) = match policy_snapshot {
            Some(policy) => (Some(policy.clone()), Some(policy.clone())),
            None => {
                let pin = self
                    .pinned_policies
                    .lock()
                    .unwrap()
                    .get(&company.id)
                    .cloned();
                (pin, None)
            }
        };
        if let Some(pin) = pin_to_store {
            self.pinned_policies
                .lock()
                .unwrap()
                .insert(company.id.clone(), pin);
        }
        let policy_fp = match &effective_snapshot {
            Some(policy) => effective_policy_fingerprint(policy),
            None => {
                // No cycle has pinned this company yet (a fresh pool before the
                // first cycle turn, or a company the cycle has not reached). Build
                // against the live effective policy — the manifest `[policy]`
                // folded with the operator override from the store read above —
                // which is exactly what the overlay installed below reflects.
                let mut live_company = company.clone();
                live_company.overlay_policy = overlay.policy.clone();
                effective_policy_fingerprint(&live_company.effective_policy())
            }
        };
        // Desk scoping now decides capability (the middle level of the
        // three-level narrowing), so it joins the staleness check: without this
        // a console desk-ceiling edit — or seating a teammate on a restricted
        // desk — would not reach the roster until a restart.
        let desk_fp =
            desk_scope_fingerprint(&overlay.desks, &overlay.desk_members, &overlay.desk_tools);
        // Issue #1796: the company grant list itself joins the staleness check.
        // Hashed over the EFFECTIVE list — the record's `[tools].allow` folded
        // with the live override read above — rather than over the override
        // alone, so this axis also catches a seed `[tools]` edit that arrived
        // with no override at all, and does not move when a redundant override
        // is carried and later cleared. That is the shape `policy_fp` settled
        // on, for the same reason.
        //
        // One asymmetry with `policy_fp` worth naming, since it is not obvious
        // from the symmetry of the two lines. The policy axis reads BOTH halves
        // live: `overlay.policy` from the store read above, and the manifest's
        // `[policy]`, which no runtime write touches. This axis reads the
        // override live but takes the base from `company.manifest.tools.allow`,
        // and that field IS runtime-mutable now — the fold makes it so. A
        // `DELETE …/tools/grants` landing after the caller snapshotted `company`
        // therefore moves the override half but not the base, and the withdrawal
        // reaches the belt a cycle later than a grant would.
        //
        // Bounded and safe rather than clean: it delays a revocation by one
        // cycle, never a grant, and every axis here has some version of that
        // window. It is recorded because the obvious reading of these two lines
        // — "the grant axis works exactly like the policy axis" — is not quite
        // true, and the next person to touch either should know which half is
        // live.
        let grants_fp = tool_grants_fingerprint(&crate::ports::types::effective_tool_allow(
            &company.manifest.tools.allow,
            overlay.tool_grants.as_ref(),
        ));

        // Re-resolve + fingerprint the tenant's capability filter (issue #108):
        // a per-tenant, per-period, fail-closed budget read from the meter. With
        // no plan this is the static `deps.capabilities`, whose fingerprint is
        // stable — so a no-plan company never rebuilds on this axis.
        let capability_filter = self.resolve_capability_filter(company, deps).await;
        let capability_fp = capability_budget::filter_fingerprint(&capability_filter);

        // Re-resolve + fingerprint the per-tenant Composio config (issue #110):
        // the token (+ toolkit allowlist) read live from the secret store, so a
        // console token set/rotate/clear takes effect on the next turn. With no
        // secret store wired this is the static `deps.composio`, whose
        // fingerprint is stable — so that company never rebuilds on this axis.
        let composio_config = self.resolve_composio(company, deps).await;
        let composio_fp = composio::TenantComposio::fingerprint(&composio_config);

        // Re-resolve + fingerprint the billing connections (#788, #789) for the
        // same reason as Composio above: both are set from the console, so a
        // roster that never re-reads them leaves an agent without billing tools
        // on a company whose settings page reads "Connected".
        #[cfg(feature = "chargebee")]
        let chargebee_config = self.resolve_chargebee(company, deps).await;
        #[cfg(feature = "paypal")]
        let paypal_config = self.resolve_paypal(company, deps).await;
        // The hosting credential is set from the same settings surface and goes
        // stale the same way, so it rides the same axis.
        let hosting_config = self.resolve_hosting(company, deps).await;
        // The company's own search provider is set from that same settings
        // surface and goes stale the same way, so it rides the same axis: a key
        // pasted in the console must reach the next turn, not the next restart.
        // Gated on the same effective grant `grants_fp` above hashes over — the
        // live override folded onto `company`'s base — so a console grant this
        // pass has not hot-rebuilt into `company` still unlocks the backend.
        let tenant_search_config = self
            .resolve_tenant_search(company, deps, overlay.tool_grants.as_ref())
            .await;
        let managed_search_config = self
            .resolve_managed_search(company, deps, overlay.tool_grants.as_ref())
            .await;
        // A build without either feature has no billing axis to go stale on, so
        // the fingerprint is a constant and this company never rebuilds on it.
        let billing_fp = {
            use std::hash::Hasher;
            // Always written to: the hosting axis below is ungated.
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            #[cfg(feature = "chargebee")]
            hasher.write_u64(chargebee::TenantChargebee::fingerprint(&chargebee_config));
            #[cfg(feature = "paypal")]
            hasher.write_u64(paypal::TenantPaypal::fingerprint(&paypal_config));
            hasher.write_u64(hosting::TenantHosting::fingerprint(&hosting_config));
            hasher.write_u64(search_byo::TenantSearch::fingerprint(&tenant_search_config));
            // Presence is a separate bit: every `u32`, including `MAX`, is a
            // valid configured cap, so no cap value can safely stand in for
            // "no backend" without making the first managed key invisible to
            // this staleness fingerprint.
            hasher.write_u8(u8::from(managed_search_config.is_some()));
            if let Some(backend) = &managed_search_config {
                hasher.write_u32(backend.daily_call_cap);
            }
            hasher.finish()
        };

        // Re-fetch + fingerprint the operator skill deltas (issue #41) BEFORE the
        // fast-path check. A skills-only change leaves every other axis stable, so
        // unless skills participate in the staleness check the cached roster is
        // wrongly reused and a console-authored / edited / disabled skill never
        // surfaces until a restart (the regression). `build_roster`/`build_agent`
        // stay synchronous and fold these deltas into each agent's effective
        // skill set; the same Vec is reused for the rebuild below (no re-fetch).
        let mut skill_deltas = match &deps.skills {
            Some(store) => store.list(&company.id).await?,
            None => Vec::new(),
        };
        // `[globals].disable = ["skill:…"]` reaches the effective set as a
        // synthesized disabling delta rather than a second opt-out mechanism
        // inside `EffectiveSkills`: the manifest and the console are then saying
        // the same thing in the same vocabulary, and a disable always beats an
        // enable there, so the company's own declaration wins over a console
        // re-enable of a skill it opted out of.
        skill_deltas.extend(crate::company::skill_effective::globals_skill_disables(
            &company.manifest.globals.disable,
        ));
        let skill_deltas = skill_deltas;
        let skill_fp = skill_delta_fingerprint(&skill_deltas);

        // Resolve the routed workspace documents (context routing) before the
        // fast-path check, and fingerprint their *content*. Both halves matter:
        // resolving here is what lets the synchronous `build_agent` fold them
        // into a persona at all, and hashing the bodies rather than the file
        // names is what makes an operator's edit to a routed note rebuild the
        // roster. A name-only hash would leave an edited note invisible until a
        // restart — the same staleness bug `skill_fp` above exists to close.
        let routed_context = self
            .resolve_routed_context(company, deps, &overlay.agents)
            .await;
        let context_fp = routed_context_fingerprint(&routed_context);

        {
            let agents = self.agents.read().await;
            let mcp_fingerprints = self.mcp_fingerprints.read().await;
            let overlay_fingerprints = self.overlay_fingerprints.read().await;
            let capability_fingerprints = self.capability_fingerprints.read().await;
            let composio_fingerprints = self.composio_fingerprints.read().await;
            let billing_fingerprints = self.billing_fingerprints.read().await;
            let skill_fingerprints = self.skill_fingerprints.read().await;
            let budget_fingerprints = self.budget_fingerprints.read().await;
            let override_fingerprints = self.override_fingerprints.read().await;
            let company_name_fingerprints = self.company_name_fingerprints.read().await;
            let policy_fingerprints = self.policy_fingerprints.read().await;
            let desk_fingerprints = self.desk_fingerprints.read().await;
            let grants_fingerprints = self.grants_fingerprints.read().await;
            let context_fingerprints = self.context_fingerprints.read().await;
            if agents.contains_key(&company.id)
                && mcp_fingerprints.get(&company.id) == Some(&mcp_fp)
                && overlay_fingerprints.get(&company.id) == Some(&overlay_fp)
                && capability_fingerprints.get(&company.id) == Some(&capability_fp)
                && composio_fingerprints.get(&company.id) == Some(&composio_fp)
                && billing_fingerprints.get(&company.id) == Some(&billing_fp)
                && skill_fingerprints.get(&company.id) == Some(&skill_fp)
                && budget_fingerprints.get(&company.id) == Some(&budget_fp)
                && override_fingerprints.get(&company.id) == Some(&override_fp)
                && company_name_fingerprints.get(&company.id) == Some(&company_name_fp)
                && policy_fingerprints.get(&company.id) == Some(&policy_fp)
                && desk_fingerprints.get(&company.id) == Some(&desk_fp)
                && grants_fingerprints.get(&company.id) == Some(&grants_fp)
                && context_fingerprints.get(&company.id) == Some(&context_fp)
            {
                return Ok(());
            }
        }

        // Fold the freshly-resolved MCP set into the deps the roster is built
        // from, so a changed set actually reaches the rebuilt agents. The clone
        // shares every Arc / queue handle — only `mcp_servers` is overridden.
        let mut fresh_deps = deps.clone();
        fresh_deps.mcp_servers = effective_mcp;
        // Install the freshly-resolved capability filter on the deps the roster
        // is built from, the same pattern as `mcp_servers` — so a tenant that
        // crossed a tier budget gets a roster whose exec tools are actually
        // trimmed. With no plan this is just `deps.capabilities` unchanged.
        fresh_deps.capabilities = capability_filter;
        // Install the freshly-resolved Composio config the same way, so a token
        // set/rotate/clear reaches the rebuilt agents (issue #110).
        fresh_deps.composio = composio_config;
        #[cfg(feature = "chargebee")]
        {
            fresh_deps.chargebee = chargebee_config;
        }
        #[cfg(feature = "paypal")]
        {
            fresh_deps.paypal = paypal_config;
        }
        fresh_deps.hosting = hosting_config;
        // And the company's own search provider, so a key pasted (or cleared) in
        // the console decides what the rebuilt agents search through.
        fresh_deps.tenant_search = tenant_search_config;
        fresh_deps.search = managed_search_config;
        // Same treatment for the overlay-agent set: `company` may be a stale
        // boot-time snapshot (e.g. `HarnessBrain::record`), so the roster is
        // built from the live-resolved overlay set, not `company.overlay_agents`.
        let mut fresh_company = company.clone();
        fresh_company.overlay_agents = overlay.agents;
        // And the operator's edits of the manifest teammates, for exactly the
        // reason the budget overrides below are installed: `build_roster`
        // resolves every manifest row through `fresh_company.effective_agent`,
        // so the live edit set has to be the one it reads — otherwise a console
        // rename would reach the roster only after a restart.
        fresh_company.overlay_agent_edits = overlay.agent_edits;
        // And the tombstones, for the same reason: `build_roster` filters the
        // manifest roster through `fresh_company.effective_agents`, so the live
        // removal set has to be the one it reads.
        fresh_company.overlay_retired_agents = overlay.retired;
        // Same treatment for the budget overrides (issue #343): `build_roster`
        // resolves every agent's cap through `fresh_company.effective_budget`,
        // so installing the live set here is what carries a console budget edit
        // into the roster the very next turn runs on.
        fresh_company.overlay_budgets = overlay.budgets;
        // The desk axis gets the same treatment, and needs it for the same
        // reason: `build_roster` resolves every teammate's grants through
        // `fresh_company.agent_desk_tools`, so the live desk set, seating and
        // ceilings have to be the ones installed here.
        fresh_company.overlay_desks = overlay.desks;
        fresh_company.overlay_desk_members = overlay.desk_members;
        fresh_company.overlay_desk_tools = overlay.desk_tools;
        // Issue #1796: same treatment for the company grant list. `build_roster`
        // reads `[tools].allow` off the manifest rather than through an
        // accessor — some three dozen sites do — so the live effective list is
        // installed onto the manifest the roster is built from, which is the
        // one place that has to be right for a console grant to reach a belt.
        //
        // `company` may be a stale boot-time snapshot (`HarnessBrain::record`),
        // so this reads the freshly-loaded override rather than the snapshot's,
        // exactly as the overlay fields above do.
        fresh_company.overlay_tool_grants = overlay.tool_grants.clone();
        fresh_company.manifest.tools.allow = crate::ports::types::effective_tool_allow(
            &company.manifest.tools.allow,
            overlay.tool_grants.as_ref(),
        );
        // Issue #562: same treatment for the policy override — `build_roster`
        // resolves the tier through `fresh_company.effective_policy`, so installing
        // the live value here is what carries a console tier change into the roster
        // the next turn runs on.
        //
        // A cycle's `ensure_with_policy` installs the snapshot instead — and so
        // does a plain `ensure` while that snapshot is pinned — because the
        // override synthesized below reproduces exactly the policy the native
        // gate is evaluating this turn against, so the roster's ApprovalPolicy
        // and the gate cannot disagree about which tier is live.
        fresh_company.overlay_policy = match &effective_snapshot {
            Some(policy) => Some(policy_override_for(policy, &company.manifest.policy)),
            None => overlay.policy.clone(),
        };
        // PR #1875 review finding: `build_roster` reads the company name off
        // `fresh_company.manifest.company.name` directly (there is no overlay
        // field for it — the rename route writes straight into the manifest,
        // see `server::ops::company_profile`'s own doc comment for why). The
        // live-resolved name from the same store read above is installed here
        // for the same reason every overlay field above is: `company` may be
        // a stale boot-time snapshot, and without this a rebuild triggered by
        // some other axis would still hand every persona the stale name.
        fresh_company.manifest.company.name = overlay.company_name.clone();

        // Issue #551 note — this rebuild deliberately touches no workspace.
        //
        // It used to provision `Agents/<id>/` for the roster it was about to
        // build, because a teammate added at runtime (a manifest edit, the
        // console's `add_member`, the orchestrator's `add_agent`) all land here
        // as a moved overlay fingerprint and boot could not have known about
        // them. That justification is gone: a member folder is no longer a
        // function of the roster. `agents/` and `desks/` are laid down once at
        // boot ([`RuntimeBuilder::build`]) and depend on nothing a rebuild can
        // change, and `agents/<id>/` is minted by
        // [`ensure_agent_folder`](crate::company::workspace_scaffold::ensure_agent_folder)
        // at the moment that agent first produces something — which is also the
        // repair path if boot's create ever fail-softed, since the minter
        // creates the root it needs. A rebuild-time call would now be a tree
        // read that can only ever find its work already done.
        // One runtime per process; every company's agents are instantiated on
        // it (plan hive-desks, Phase 2). Booted from the environment `serve`
        // prepared — an ephemeral workspace in a test binary.
        let runtime = crate::harness::openhuman_runtime::global(
            crate::harness::openhuman_runtime::RuntimeBoot::from_env(),
        )
        .await?;
        // The `opencompany` MCP listener has to be up before a spec names its
        // endpoint (plan hive-desks Phase 3). Idempotent after the first bind.
        self.mcp.serve_loopback().await.map_err(|err| {
            OpenCompanyError::Harness(format!("bind the opencompany MCP listener: {err}"))
        })?;

        // Retire the previous roster before the new one registers: a runtime
        // agent id stays reserved while any clone of it lives, so the old
        // `CompanyAgent`s must be dropped first, and dropping one mid-turn
        // would pull the handle out from under that turn. Take each old
        // agent's turn lock (bounded — a wedged turn must not wedge every
        // rebuild after it), then drop. A turn still holding its lock past
        // the bound keeps its old handle alive; the new registration then
        // lands on a numbered suffix (`CompanyAgent::register`) and the old
        // one releases when the turn ends.
        let previous = self.agents.write().await.remove(&company.id);
        // What each retired entry's prompt brief named, and whether it still
        // owed the session that brief — see `CompanyAgent::catalogue_brief_stale`.
        // Read before the drop: the rebuilt entry inherits it below.
        let retired_briefs: HashMap<String, (Vec<String>, bool)> = previous
            .iter()
            .flatten()
            .map(|agent| {
                (
                    agent.agent_id.clone(),
                    (
                        agent.served_catalogue().to_vec(),
                        agent.catalogue_brief_pending(),
                    ),
                )
            })
            .collect();
        if let Some(previous) = previous {
            let quiesce = std::time::Duration::from_secs(30);
            for agent in &previous {
                let lock = agent.turn_lock();
                match tokio::time::timeout(quiesce, lock.lock()).await {
                    Ok(guard) => drop(guard),
                    Err(_) => tracing::warn!(
                        company = %company.id,
                        agent = %agent.agent_id,
                        "[harness] a turn is still running past the rebuild quiesce bound; \
                         the rebuilt agent registers beside it"
                    ),
                }
            }
            drop(previous);
        }

        let roster = build_roster(
            &runtime,
            &fresh_company,
            &fresh_deps,
            &skill_deltas,
            &routed_context,
        )?;

        // Keep the policy snapshot and the roster together for the entire turn.
        // `ensure_with_policy` pins the snapshot on the pool (above), so a
        // concurrent plain `ensure` — the workflow runner, a spawned task outside
        // the cycle serial lock — rebuilds the policy axis against that same pin
        // instead of a live override it could otherwise adopt a turn early. The
        // serial lock already serializes cycle callers; the pin is what keeps a
        // direct caller from regressing a pinned roster before `run_inner` clones
        // its agent.
        for agent in &roster {
            if let Some((catalogue, pending)) = retired_briefs.get(&agent.agent_id) {
                agent.inherit_catalogue_brief(catalogue, *pending);
            }
        }

        let mut agents = self.agents.write().await;
        agents.insert(company.id.clone(), roster);
        self.mcp_fingerprints
            .write()
            .await
            .insert(company.id.clone(), mcp_fp);
        self.overlay_fingerprints
            .write()
            .await
            .insert(company.id.clone(), overlay_fp);
        self.capability_fingerprints
            .write()
            .await
            .insert(company.id.clone(), capability_fp);
        self.composio_fingerprints
            .write()
            .await
            .insert(company.id.clone(), composio_fp);
        self.billing_fingerprints
            .write()
            .await
            .insert(company.id.clone(), billing_fp);
        self.skill_fingerprints
            .write()
            .await
            .insert(company.id.clone(), skill_fp);
        self.budget_fingerprints
            .write()
            .await
            .insert(company.id.clone(), budget_fp);
        self.override_fingerprints
            .write()
            .await
            .insert(company.id.clone(), override_fp);
        self.company_name_fingerprints
            .write()
            .await
            .insert(company.id.clone(), company_name_fp);
        self.policy_fingerprints
            .write()
            .await
            .insert(company.id.clone(), policy_fp);
        self.desk_fingerprints
            .write()
            .await
            .insert(company.id.clone(), desk_fp);
        self.grants_fingerprints
            .write()
            .await
            .insert(company.id.clone(), grants_fp);
        self.context_fingerprints
            .write()
            .await
            .insert(company.id.clone(), context_fp);
        Ok(())
    }

    /// The memory-engine selection the company's cached roster was built
    /// against, if any (issue #1113). `None` for the base backend.
    ///
    /// Recorded by [`RuntimeBuilder::build`](crate::runtime::RuntimeBuilder::build);
    /// an absent row is indistinguishable from a recorded base-backend `None`,
    /// which is correct (see the field doc).
    pub async fn memory_engine(&self, company: &CompanyId) -> Option<u64> {
        self.memory_engine
            .read()
            .await
            .get(company)
            .copied()
            .flatten()
    }

    /// Records the engine selection `engine` as the one the company's roster is
    /// now bound to, dropping the cached roster when it differs from what was
    /// recorded before (a live swap, issue #1113).
    ///
    /// Returns `true` when the selection is unchanged and the roster survived —
    /// the ordinary issue #290 rebuild fast path — and `false` when the roster
    /// was invalidated and the next [`ensure`](Self::ensure) will rebuild it
    /// over the replacement memory-family ports.
    ///
    /// The pool only ever compares selections recorded on a previous `build`;
    /// it cannot itself know whether an engine swap happened, because the new
    /// engine's ports arrive on the builder, not here. The builder is therefore
    /// the only caller: it records the selection on every build that
    /// re-applies the engine (`with_memory_overlay` / `with_memory_overlay_cleared`),
    /// boot included, so the first rebuild has a recorded selection to differ
    /// from. A rebuild about something else inherits the handover's ports
    /// unchanged (issue #290) and does not call this — its selection is the
    /// recorded one by construction.
    pub async fn rebind_memory_engine(&self, company: &CompanyId, engine: Option<u64>) -> bool {
        let recorded = self.memory_engine.write().await;
        if recorded.get(company).copied().flatten() == engine {
            return true;
        }
        drop(recorded);
        self.invalidate_roster(company).await;
        self.memory_engine
            .write()
            .await
            .insert(company.clone(), engine);
        false
    }

    /// Drops every cached artifact for one company, so the next `ensure`
    /// rebuilds its roster from scratch. The memory-engine bookkeeping is a
    /// cached artifact like any fingerprint — the caller re-records the new
    /// selection after invalidating.
    async fn invalidate_roster(&self, company: &CompanyId) {
        self.agents.write().await.remove(company);
        self.monthly_budgets.write().await.remove(company);
        self.mcp_fingerprints.write().await.remove(company);
        self.overlay_fingerprints.write().await.remove(company);
        self.capability_fingerprints.write().await.remove(company);
        self.composio_fingerprints.write().await.remove(company);
        self.billing_fingerprints.write().await.remove(company);
        self.skill_fingerprints.write().await.remove(company);
        self.budget_fingerprints.write().await.remove(company);
        self.override_fingerprints.write().await.remove(company);
        self.company_name_fingerprints.write().await.remove(company);
        self.policy_fingerprints.write().await.remove(company);
        self.desk_fingerprints.write().await.remove(company);
        self.grants_fingerprints.write().await.remove(company);
        self.context_fingerprints.write().await.remove(company);
        self.memory_engine.write().await.remove(company);
    }

    /// Re-resolves the company's capability filter (issue #108): with a plan
    /// wired ([`HarnessDeps::plan`]), a per-tenant, per-period, fail-closed
    /// budget read from the [`UsageMeter`] via
    /// [`capability_budget::resolve_filter`]; without one, the static
    /// [`HarnessDeps::capabilities`] verbatim (gating off). Never a boot
    /// snapshot — resolved on every `ensure` so a tier switches off the turn
    /// after its budget is crossed.
    async fn resolve_capability_filter(
        &self,
        company: &CompanyRecord,
        deps: &HarnessDeps,
    ) -> toolbelt::CapabilityFilter {
        match &deps.plan {
            Some(plan) => {
                capability_budget::resolve_filter(
                    plan,
                    deps.meter.as_deref(),
                    &company.id,
                    crate::ports::now_millis(),
                )
                .await
            }
            None => deps.capabilities.clone(),
        }
    }

    /// Re-resolves the company's per-tenant Composio config (issue #110) from the
    /// [`SecretStore`], so a console token set/rotate/clear takes effect on the
    /// next turn. Only companies that **explicitly** grant `composio` touch the
    /// secret store on this axis; others resolve to `None` (no tools). With no
    /// secret store wired this degrades to the static [`HarnessDeps::composio`].
    ///
    /// Resolution prefers the company's own stored token and falls back to this
    /// instance's platform identity; with neither it yields `None` (fail closed).
    /// Both the backend URL (from the tenant API base
    /// [`composio::TINYHUMANS_API_URL_ENV`], then the prod default — the
    /// explicit per-surface override was removed in phase 6a, issue #2306) and
    /// the platform identity are read process-globally here, so a live
    /// re-resolution keeps them even when nothing was stored at boot.
    ///
    /// Re-deriving the token source every turn costs nothing — building it reads
    /// no file — and the roster that keeps it holds one instance for its whole
    /// lifetime, so its rotation cache still works.
    async fn resolve_composio(
        &self,
        company: &CompanyRecord,
        deps: &HarnessDeps,
    ) -> Option<composio::TenantComposio> {
        if !crate::company::grants_composio_explicit(&company.manifest.tools.allow) {
            return None;
        }
        let toolkits = company.manifest.tools.composio.toolkits.clone();
        match &deps.secrets {
            Some(secrets) => {
                use crate::app::config::EnvSource;
                let env = crate::app::config::ProcessEnv;
                let api_url = env.get(composio::TINYHUMANS_API_URL_ENV);
                composio::TenantComposio::resolve(
                    &company.id,
                    secrets.as_ref(),
                    toolkits,
                    api_url,
                    crate::company::TinyhumansTokenSource::from_env(&env).map(std::sync::Arc::new),
                )
                .await
            }
            None => deps.composio.clone(),
        }
    }

    /// Re-reads the company's Chargebee connection from the secret store, so a
    /// key saved or rotated in Settings → Billing reaches the agent on its next
    /// turn rather than at the next restart (issue #788).
    ///
    /// Only companies that **explicitly** grant `chargebee` read at all. With no
    /// secret store wired this keeps the boot-resolved
    /// [`HarnessDeps::chargebee`] — which was itself resolved from *this*
    /// company's secret store by the runtime builder, so the fallback cannot
    /// reach another tenant's credential.
    ///
    /// A transient **read error** keeps that connection too, with a warning,
    /// rather than un-wiring the billing tools — the same direction
    /// [`Self::resolve_effective_mcp`]
    /// degrade in, and the safe one here for a specific reason: a stale
    /// Chargebee credential is refused by Chargebee, which the agent surfaces as
    /// a tool error it can report, whereas a tool that has vanished is invisible
    /// to the agent — it simply stops being able to invoice and says nothing.
    /// An absent credential still resolves to `None`; only the error case holds.
    #[cfg(feature = "chargebee")]
    async fn resolve_chargebee(
        &self,
        company: &CompanyRecord,
        deps: &HarnessDeps,
    ) -> Option<chargebee::TenantChargebee> {
        if !crate::company::grants_chargebee_explicit(&company.manifest.tools.allow) {
            return None;
        }
        let Some(secrets) = &deps.secrets else {
            return deps.chargebee.clone();
        };
        match chargebee::TenantChargebee::resolve(secrets, &company.id).await {
            Ok(resolved) => resolved,
            Err(err) => {
                tracing::warn!(
                    company = %company.id,
                    "[chargebee] could not read the billing credential; keeping the last known \
                     connection: {err}"
                );
                deps.chargebee.clone()
            }
        }
    }

    /// The hosting equivalent, for the same reasons.
    ///
    /// Only companies that **explicitly** grant `hosting` read at all: a
    /// deployment publishes a company's files to the public internet and can
    /// provision a database it is billed for, so the catch-all `*` does not
    /// confer it.
    ///
    /// A transient read error keeps the last known connection with a warning,
    /// like `chargebee` and for the same reason: a stale hosting key is refused
    /// by the provider, which the agent surfaces as a tool error it can report,
    /// whereas a tool that has vanished is invisible to the agent — it simply
    /// stops being able to deploy and says nothing.
    async fn resolve_hosting(
        &self,
        company: &CompanyRecord,
        deps: &HarnessDeps,
    ) -> Option<hosting::TenantHosting> {
        if !crate::company::grants_hosting_explicit(&company.manifest.tools.allow) {
            return None;
        }
        let Some(secrets) = &deps.secrets else {
            return deps.hosting.clone();
        };
        match hosting::TenantHosting::resolve(secrets, &company.id).await {
            Ok(resolved) => resolved,
            Err(err) => {
                tracing::warn!(
                    company = %company.id,
                    "[hosting] could not read the hosting credential; keeping the last known \
                     connection: {err}"
                );
                deps.hosting.clone()
            }
        }
    }

    /// The company's own search provider, for the same reasons as `hosting`.
    ///
    /// Only companies that **explicitly** grant `search` read at all — the same
    /// gate the metered managed tool passes, because this is the same namespace
    /// wearing a different credential. A company that never opted into web
    /// search does not get a store read per turn for a setting it cannot use.
    ///
    /// The grant check reads the effective allow-list — `company`'s base folded
    /// with `overlay_tool_grants` — not `company.manifest.tools.allow` alone.
    /// `company` can be a stale snapshot the live override has not yet been
    /// folded into; checking only its raw field would resolve no backend for a
    /// grant the roster's own effective-allow-list read already honours,
    /// leaving native evidence claim `search` while no tool gets wired.
    ///
    /// A transient read error keeps the last known connection with a warning,
    /// like `hosting`: degrading to `None` would silently move the company's
    /// searches back onto the platform's metered account — a bill moving between
    /// two parties because a store hiccuped.
    async fn resolve_tenant_search(
        &self,
        company: &CompanyRecord,
        deps: &HarnessDeps,
        overlay_tool_grants: Option<&crate::ports::types::ToolGrantsOverride>,
    ) -> Option<search_byo::TenantSearch> {
        let effective_allow = crate::ports::types::effective_tool_allow(
            &company.manifest.tools.allow,
            overlay_tool_grants,
        );
        if !crate::company::grants_search_explicit(&effective_allow) {
            return None;
        }
        let Some(secrets) = &deps.secrets else {
            return deps.tenant_search.clone();
        };
        match search_byo::TenantSearch::resolve(secrets, &company.id).await {
            Ok(resolved) => resolved,
            Err(err) => {
                tracing::warn!(
                    company = %company.id,
                    "[search] could not read the company's search provider; keeping the last known \
                     connection: {err}"
                );
                deps.tenant_search.clone()
            }
        }
    }

    /// Managed Search with the company's copied TinyHumans key as the first
    /// tier and the deployment credential as the fallback. The backend reads
    /// the company tier again for every request, so rotations need no roster
    /// rebuild; this resolver only ensures a backend exists when a previously
    /// uncredentialed deployment receives its first company key.
    async fn resolve_managed_search(
        &self,
        company: &CompanyRecord,
        deps: &HarnessDeps,
        overlay_tool_grants: Option<&crate::ports::types::ToolGrantsOverride>,
    ) -> Option<search::SearchBackend> {
        let effective_allow = crate::ports::types::effective_tool_allow(
            &company.manifest.tools.allow,
            overlay_tool_grants,
        );
        if !crate::company::grants_search_explicit(&effective_allow) {
            return None;
        }
        let Some(secrets) = &deps.secrets else {
            return deps
                .search
                .clone()
                .filter(|backend| backend.credential.configured())
                .map(|backend| {
                    backend.with_daily_call_cap(
                        company
                            .manifest
                            .tools
                            .search_daily_calls
                            .unwrap_or(crate::company::DEFAULT_SEARCH_DAILY_CALLS),
                    )
                });
        };
        let company_key = match crate::company::search::load_managed_key(
            &company.id,
            secrets.as_ref(),
        )
        .await
        {
            Ok(key) => key,
            Err(err) => {
                tracing::warn!(
                    company = %company.id,
                    "[search] could not read the managed company credential; keeping the last known configuration: {err}"
                );
                let cached = self
                    .managed_search_backends
                    .read()
                    .await
                    .get(&company.id)
                    .cloned();
                return cached.or_else(|| deps.search.clone()).map(|backend| {
                    backend
                        .with_daily_call_cap(
                            company
                                .manifest
                                .tools
                                .search_daily_calls
                                .unwrap_or(crate::company::DEFAULT_SEARCH_DAILY_CALLS),
                        )
                        .with_company_credential(company.id.clone(), secrets.clone())
                });
            }
        };
        // Drop the read guard before the vacant branch below takes the write
        // lock; retaining it across that branch deadlocks the first
        // company-only resolution.
        let cached_company_backend = if deps.search.is_none() && company_key.is_some() {
            let backends = self.managed_search_backends.read().await;
            backends.get(&company.id).cloned()
        } else {
            None
        };
        let backend = match (&deps.search, company_key) {
            (Some(backend), Some(_)) => backend.clone(),
            (Some(backend), None) if backend.credential.configured() => backend.clone(),
            (Some(_), None) => return None,
            (None, Some(_)) => match cached_company_backend {
                Some(backend) => backend,
                None => {
                    use crate::app::config::ProcessEnv;
                    let backend = search::SearchBackend::new(
                        provider::search_backend_url_from_env(&ProcessEnv),
                        crate::company::credentials::Credential::None,
                        crate::company::DEFAULT_SEARCH_DAILY_CALLS,
                    );
                    self.managed_search_backends
                        .write()
                        .await
                        .entry(company.id.clone())
                        .or_insert_with(|| backend.clone())
                        .clone()
                }
            },
            (None, None) => return None,
        };
        Some(
            backend
                .with_daily_call_cap(
                    company
                        .manifest
                        .tools
                        .search_daily_calls
                        .unwrap_or(crate::company::DEFAULT_SEARCH_DAILY_CALLS),
                )
                .with_company_credential(company.id.clone(), secrets.clone()),
        )
    }

    /// The PayPal equivalent (issue #789), for the same reasons.
    #[cfg(feature = "paypal")]
    async fn resolve_paypal(
        &self,
        company: &CompanyRecord,
        deps: &HarnessDeps,
    ) -> Option<paypal::TenantPaypal> {
        if !crate::company::grants_paypal_explicit(&company.manifest.tools.allow) {
            return None;
        }
        let Some(secrets) = &deps.secrets else {
            return deps.paypal.clone();
        };
        match paypal::TenantPaypal::resolve(secrets, &company.id).await {
            Ok(resolved) => resolved,
            Err(err) => {
                tracing::warn!(
                    company = %company.id,
                    "[paypal] could not read the billing credential; keeping the last known \
                     connection: {err}"
                );
                deps.paypal.clone()
            }
        }
    }

    /// Re-resolves the company's effective MCP server set: from the secret store
    /// when [`HarnessDeps::secrets`] is wired (picking up console changes), else
    /// the boot-resolved [`HarnessDeps::mcp_servers`] unchanged. A resolution
    /// error degrades to the boot-resolved set rather than dropping MCP tools.
    async fn resolve_effective_mcp(
        &self,
        company: &CompanyRecord,
        deps: &HarnessDeps,
    ) -> Vec<McpServerDecl> {
        match &deps.secrets {
            Some(secrets) => {
                let mut decls = crate::company::mcp::resolve_effective(
                    &company.id,
                    &deps.default_mcp_servers,
                    &company.manifest.mcp_servers,
                    secrets.as_ref(),
                )
                .await
                .unwrap_or_else(|_| deps.mcp_servers.clone());
                // Refresh any near-expiry console-OAuth credential before the
                // registry is built, so an agent never sends a stale bearer.
                refresh_oauth_decls(&company.id, &mut decls, secrets.as_ref()).await;
                decls
            }
            None => deps.mcp_servers.clone(),
        }
    }

    /// Re-resolves the company's live overlay-agent set (issue #71) **and** its
    /// operator budget overrides (issue #343): reloads the [`CompanyRecord`]
    /// from [`HarnessDeps::store`] so a teammate added through the console
    /// `POST .../team` route or the orchestrator's `add_agent` tool, and a cap
    /// written through `PUT .../team/{id}/budget`, both reach the roster on the
    /// company's next `ensure` call — the same live-re-resolution pattern as
    /// [`Self::resolve_effective_mcp`]. A missing record or a store error
    /// degrades to the `company` snapshot passed in (never worse than the
    /// pre-#71 always-static behaviour).
    ///
    /// The two collections share **one** store round-trip deliberately: they
    /// come off the same record, and splitting them would double the per-turn
    /// read for no gain.
    /// Resolves every roster member's routed workspace documents, keyed by agent
    /// id (`docs/spec/runtime/orchestration/context-routing.md`).
    ///
    /// Runs here, in the async caller, because `build_roster` is synchronous and
    /// the [`WorkspaceStore`](crate::ports::WorkspaceStore) is not — the same
    /// split as the skill deltas beside it.
    ///
    /// **Fails soft, per agent.** A store error yields no documents for that
    /// role rather than failing the rebuild: routing enriches a prompt, and a
    /// company whose workspace read hiccuped should answer from a thinner prompt
    /// rather than stop answering. An unwired store (`None`) resolves to an
    /// empty map, which is the pre-routing behaviour exactly.
    ///
    /// Overlay teammates are included: they are real roster agents that
    /// [`build_roster`] builds the same way, so leaving them out would give a
    /// console-added teammate a silently different prompt from a manifest one.
    async fn resolve_routed_context(
        &self,
        company: &CompanyRecord,
        deps: &HarnessDeps,
        overlay_agents: &[OverlayAgent],
    ) -> HashMap<String, Vec<(String, String)>> {
        let Some(workspace) = deps.workspace.as_ref() else {
            return HashMap::new();
        };

        // A manifest agent wins an id collision, exactly as `build_roster`
        // resolves one, so the overlay half skips any id already claimed.
        let manifest_ids: HashSet<&str> = company
            .manifest
            .agents
            .iter()
            .map(|a| a.id.as_str())
            .collect();
        let overlay_as_manifest: Vec<ManifestAgent> = overlay_agents
            .iter()
            .filter(|overlay| !manifest_ids.contains(overlay.id.as_str()))
            .map(overlay_agent_to_manifest)
            .collect();

        let mut routed = HashMap::new();
        for agent in company.manifest.agents.iter().chain(&overlay_as_manifest) {
            match crate::company::context_routing::resolve_routed_documents(
                workspace.as_ref(),
                &company.id,
                agent,
            )
            .await
            {
                // An agent that resolved nothing is left out of the map rather
                // than stored as an empty vec: `build_roster` reads an absent id
                // as "no routed documents", so the two are the same answer and
                // the map stays the size of what actually routed.
                Ok(documents) if documents.is_empty() => {}
                Ok(documents) => {
                    routed.insert(agent.id.clone(), documents);
                }
                Err(err) => tracing::warn!(
                    company = %company.id,
                    agent = %agent.id,
                    error = %err,
                    "[context] could not read this role's routed documents; its prompt \
                     goes out without them"
                ),
            }
        }
        routed
    }

    async fn resolve_effective_overlay(
        &self,
        company: &CompanyRecord,
        deps: &HarnessDeps,
    ) -> EffectiveOverlay {
        match deps.store.load(&company.id).await {
            Ok(Some(record)) => EffectiveOverlay {
                company_name: record.manifest.company.name.clone(),
                agents: record.overlay_agents,
                agent_edits: record.overlay_agent_edits,
                retired: record.overlay_retired_agents,
                budgets: record.overlay_budgets,
                policy: record.overlay_policy,
                desks: record.overlay_desks,
                desk_members: record.overlay_desk_members,
                tool_grants: record.overlay_tool_grants,
                desk_tools: record.overlay_desk_tools,
            },
            _ => EffectiveOverlay {
                company_name: company.manifest.company.name.clone(),
                agents: company.overlay_agents.clone(),
                agent_edits: company.overlay_agent_edits.clone(),
                retired: company.overlay_retired_agents.clone(),
                budgets: company.overlay_budgets.clone(),
                policy: company.overlay_policy.clone(),
                desks: company.overlay_desks.clone(),
                desk_members: company.overlay_desk_members.clone(),
                tool_grants: company.overlay_tool_grants.clone(),
                desk_tools: company.overlay_desk_tools.clone(),
            },
        }
    }

    /// The current MCP fingerprint for a company (test-only), so a freshness test
    /// can assert a rebuild happened without introspecting agent internals.
    #[cfg(test)]
    pub async fn mcp_fingerprint_of(&self, company: &CompanyId) -> Option<u64> {
        self.mcp_fingerprints.read().await.get(company).copied()
    }

    /// The current overlay-agent fingerprint for a company (test-only), mirroring
    /// [`Self::mcp_fingerprint_of`].
    #[cfg(test)]
    pub async fn overlay_fingerprint_of(&self, company: &CompanyId) -> Option<u64> {
        self.overlay_fingerprints.read().await.get(company).copied()
    }

    /// The current capability-filter fingerprint for a company (test-only), so a
    /// budget-freshness test can assert a rebuild happened (issue #108).
    #[cfg(test)]
    pub async fn capability_fingerprint_of(&self, company: &CompanyId) -> Option<u64> {
        self.capability_fingerprints
            .read()
            .await
            .get(company)
            .copied()
    }

    /// The current skill-delta fingerprint for a company (test-only), so a
    /// skill-freshness test can assert a rebuild happened (issue #41).
    #[cfg(test)]
    pub async fn skill_fingerprint_of(&self, company: &CompanyId) -> Option<u64> {
        self.skill_fingerprints.read().await.get(company).copied()
    }

    /// The current budget-override fingerprint for a company (test-only), so a
    /// budget-freshness test can assert the roster was actually rebuilt after a
    /// console cap change rather than inferring it from the refusal (issue
    /// #343). This is the observable that makes "no restart" testable.
    #[cfg(test)]
    pub async fn budget_fingerprint_of(&self, company: &CompanyId) -> Option<u64> {
        self.budget_fingerprints.read().await.get(company).copied()
    }

    /// The current company-name fingerprint for a company (test-only), so a
    /// rename-freshness test can assert the roster was actually rebuilt after a
    /// `PATCH {scope}` rename rather than inferring it (PR #1875 review
    /// finding). Mirrors [`Self::override_fingerprint_of`]'s role for the
    /// persona-override axis.
    #[cfg(test)]
    pub async fn company_name_fingerprint_of(&self, company: &CompanyId) -> Option<u64> {
        self.company_name_fingerprints
            .read()
            .await
            .get(company)
            .copied()
    }

    /// The current persona-override fingerprint for a company (test-only), so a
    /// persona-freshness test can assert the roster was actually rebuilt after a
    /// console instructions edit rather than inferring it (issue #1530). This is
    /// the observable that makes "no restart" testable.
    #[cfg(test)]
    pub async fn override_fingerprint_of(&self, company: &CompanyId) -> Option<u64> {
        self.override_fingerprints
            .read()
            .await
            .get(company)
            .copied()
    }

    /// The current grant fingerprint for a company (test-only), so a
    /// grant-freshness test can assert the roster was actually rebuilt after a
    /// console tool grant rather than inferring it (issue #1796). This is the
    /// observable that makes "no restart" testable.
    #[cfg(test)]
    pub async fn grants_fingerprint_of(&self, company: &CompanyId) -> Option<u64> {
        self.grants_fingerprints.read().await.get(company).copied()
    }

    /// The current policy fingerprint for a company (test-only), so a
    /// policy-freshness test can assert the roster was rebuilt against the
    /// cycle-start snapshot and not against a mid-turn store edit (issue #1455).
    #[cfg(test)]
    pub async fn policy_fingerprint_of(&self, company: &CompanyId) -> Option<u64> {
        self.policy_fingerprints.read().await.get(company).copied()
    }

    /// The current billing-connection fingerprint for a company (test-only), so
    /// a credential-freshness test can assert the roster was rebuilt after a key
    /// was saved or rotated in Settings → Billing rather than inferring it from
    /// the tool list (issues #788, #789).
    #[cfg(test)]
    pub async fn billing_fingerprint_of(&self, company: &CompanyId) -> Option<u64> {
        self.billing_fingerprints.read().await.get(company).copied()
    }

    /// The current desk-scope fingerprint for a company (test-only), so a
    /// desk-scoping test can assert the roster was actually rebuilt after a
    /// ceiling or seating change rather than inferring it from a refused call.
    #[cfg(test)]
    pub async fn desk_fingerprint_of(&self, company: &CompanyId) -> Option<u64> {
        self.desk_fingerprints.read().await.get(company).copied()
    }

    /// The current routed-context fingerprint for a company (test-only), so a
    /// routing test can assert that editing a routed workspace note actually
    /// rebuilt the roster rather than inferring it from a reply.
    #[cfg(test)]
    pub async fn context_fingerprint_of(&self, company: &CompanyId) -> Option<u64> {
        self.context_fingerprints.read().await.get(company).copied()
    }

    /// Routes a message to one agent and returns its reply, recording the turn's
    /// cost. `agent_id` must name a member of the company's roster.
    ///
    /// Desk routing (which agent answers a group chat) is the caller's job — v1
    /// is single-responder and the WS3 chat handler picks the addressed member.
    ///
    /// `chat` names the conversation this turn answers: the chat/desk id
    /// journaled as `AgentReply.chat_id`, and the thread within it (#1890). The
    /// id rides each live turn-stream frame so the console routes the in-flight
    /// tool timeline to the right thread; `None` falls back to the default
    /// desk, matching the durable reply. The root scopes the history seed and
    /// is never streamed.
    pub async fn run(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        deps: &HarnessDeps,
        chat: crate::runtime::delegation::ChatTarget<'_>,
    ) -> crate::Result<TurnOutcome> {
        self.run_inner(
            company,
            agent_id,
            message,
            deps,
            None,
            LiveStream::On {
                chat_id: chat.chat_id,
            },
            chat,
            None,
        )
        .await
    }

    /// Like [`run`](Self::run) but WITHOUT live turn streaming — for a turn that
    /// surfaces no operator chat bubble (a workflow agent node, which drops its
    /// steps). Its transient `tool_call`/`tool_result` frames would otherwise
    /// leak onto the console's live timeline and misattribute to whatever thread
    /// most recently sent, so this path publishes nothing (#125 review).
    pub async fn run_background(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        deps: &HarnessDeps,
        run_sink: Option<Arc<RunTraceSink>>,
    ) -> crate::Result<TurnOutcome> {
        // `LiveStream::Off` stays: the frames must not reach the console
        // timeline. The sink is a different channel — a durable per-attempt
        // trace keyed on its own run id, which cannot misattribute to a chat
        // thread because it names none.
        self.run_inner(
            company,
            agent_id,
            message,
            deps,
            None,
            LiveStream::Off,
            // A dispatched card's own turn answers no conversation: its steps
            // go to the card's note, and nothing binds to a thread.
            crate::runtime::delegation::ChatTarget::default(),
            run_sink,
        )
        .await
    }

    /// Like [`run_background`](Self::run_background) but tees the node's live
    /// tool-call frames onto the turn-stream bus (issue #1702).
    ///
    /// A workflow agent node still shows no operator chat bubble, so its frames
    /// cannot route by a chat thread — they carry the workflow `run_id`/`node_id`
    /// instead, and the console's run-trace sheet keys the in-flight tool
    /// timeline on the run. This is the only difference from `run_background`:
    /// the durable per-attempt trace (`run_sink`) is unchanged, and a
    /// tag/publish hiccup can never fail the turn (the collector's publish is
    /// best-effort and the frame carries only the already-scrubbed projection).
    #[allow(clippy::too_many_arguments)]
    pub async fn run_background_workflow(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        deps: &HarnessDeps,
        run_sink: Option<Arc<RunTraceSink>>,
        workflow_run_id: &str,
        node_id: &str,
    ) -> crate::Result<TurnOutcome> {
        self.run_inner(
            company,
            agent_id,
            message,
            deps,
            None,
            LiveStream::Workflow {
                run_id: workflow_run_id,
                node_id,
            },
            // Routed by run and node, not by a conversation — there is none to
            // bind history to (issue #1702).
            crate::runtime::delegation::ChatTarget::default(),
            run_sink,
        )
        .await
    }

    /// Routes a message to one agent with an operator **steer** control installed
    /// (issue #111), so a dispatched task / desk delegation can be paused,
    /// cancelled, or redirected mid-flight. Otherwise identical to
    /// [`run`](Self::run) — same retrieve→inject, cost accounting, and
    /// memory-writeback. The steer hook fires only between tool-loop iterations.
    /// `chat_id` routes the live turn-stream frames exactly as in [`run`](Self::run).
    ///
    /// `run_sink` is the dispatched attempt this turn belongs to, when it
    /// belongs to one (issue #242) — a desk turn a *dispatched card* handed its
    /// work to records into the card's run, while the same delegation reached
    /// from operator chat passes `None`.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_steered(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        deps: &HarnessDeps,
        control: &SteerControl,
        chat: crate::runtime::delegation::ChatTarget<'_>,
        run_sink: Option<Arc<run_trace::RunTraceSink>>,
    ) -> crate::Result<TurnOutcome> {
        self.run_inner(
            company,
            agent_id,
            message,
            deps,
            Some(control),
            LiveStream::On {
                chat_id: chat.chat_id,
            },
            chat,
            run_sink,
        )
        .await
    }

    /// Like [`run_steered`](Self::run_steered) but WITHOUT live turn streaming —
    /// for a dispatched task card, which discards its steps and shows no chat
    /// bubble. Its transient turn frames must not reach the live console
    /// timeline (they'd misattribute to a chat thread), so this path publishes
    /// nothing while still honouring the operator steer control (#125 review).
    ///
    /// # It still has a conversation, when its caller does (issue #1890 I)
    ///
    /// `chat` is separate from the (absent) stream, and that separation is the
    /// whole of what I fixes. An approval's re-issued call comes through here:
    /// it publishes no frames, but it *was* raised in a conversation, and the
    /// grant has recorded which one since #435. With identity read off the
    /// stream, that call was indistinguishable from a dispatched card's turn —
    /// so it ran against whatever history happened to be loaded and then
    /// answered into the thread it had never been bound to.
    ///
    /// A dispatched card's turn passes [`ChatTarget::default()`] and keeps
    /// exactly the behaviour it had: no binding, and — deliberately — no clear
    /// either, since one background task can span several turns that depend on
    /// what accumulated between them.
    // One over the limit, and the one that pushed it there is the whole point
    // of #1890 I: the conversation must be sayable independently of the stream.
    // Bundling the rest into a struct to get back under would hide six
    // parameters that every sibling entry point on this type spells out.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_steered_background(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        deps: &HarnessDeps,
        control: &SteerControl,
        chat: crate::runtime::delegation::ChatTarget<'_>,
        run_sink: Option<Arc<run_trace::RunTraceSink>>,
    ) -> crate::Result<TurnOutcome> {
        self.run_inner(
            company,
            agent_id,
            message,
            deps,
            Some(control),
            LiveStream::Off,
            chat,
            run_sink,
        )
        .await
    }

    /// Whether the plan-level total-token ceiling is already spent — the bare
    /// predicate behind [`total_ceiling_refusal`](Self::total_ceiling_refusal),
    /// for callers that make a model call the refusal shape does not describe.
    ///
    /// The responder-selection pass (issue #1835) is the first: it runs
    /// *before* a responder is chosen, so it has no agent to refuse as and no
    /// `TurnOutcome` to hand back — but it is a real model call, and a tenant
    /// past its ceiling must not keep paying for routing (codex on #1872).
    /// One predicate, so "is the ceiling spent" cannot answer differently for
    /// the gate and for the turn it gates.
    ///
    /// **Answers `false` only when no ceiling is declared** — no plan, or a
    /// plan with no `total_budget`. A company that declared no bound has
    /// nothing to enforce, so these optional model calls run freely.
    ///
    /// When a ceiling IS declared but the spend behind it cannot be read, this
    /// answers `true`, matching what `total_ceiling_refusal` does with the
    /// same cases: a declared cap is binding, and a priced call made against a
    /// bound nobody can measure is spending real money outside it. The callers
    /// this gates are all optional extras — a card title, a sufficiency judge,
    /// a responder-selection pass — each of which already has a deterministic
    /// answer to fall back on, so declining them costs a nicety, not the work.
    pub(crate) async fn total_ceiling_spent(company: &CompanyId, deps: &HarnessDeps) -> bool {
        let Some(plan) = deps.plan.as_ref() else {
            return false;
        };
        if plan.total_budget.is_none() {
            return false;
        }
        let since = plan.period.period_start_millis(crate::ports::now_millis());
        match read_spend_for_gate(deps.meter.as_deref(), company, since).await {
            Ok(samples) => plan.total_exhausted(capability_budget::tokens_in(&samples)),
            Err(_) => true,
        }
    }

    /// The plan-level total-token ceiling, as a refusal or nothing.
    ///
    /// Extracted from [`run_inner`](Self::run_inner) so the confined turn
    /// (issue #416) is gated by the *same* ceiling rather than a second copy of
    /// the rule: a turn that reaches nothing still spends model tokens, so a
    /// tenant past its cap must not be able to keep spending through the
    /// copilot.
    /// How much one dispatch promises against the total ceiling before it runs.
    ///
    /// Not an estimate of a turn: no per-turn ceiling exists to derive one from.
    /// It is the granularity at which the ceiling binds — overshoot is bounded
    /// by this rather than by however many turns happened to race.
    const DISPATCH_RESERVATION_TOKENS: u64 = 4_000;

    async fn total_ceiling_refusal(
        company: &CompanyId,
        agent_id: &str,
        deps: &HarnessDeps,
    ) -> CeilingGate {
        let Some(plan) = deps.plan.as_ref() else {
            return CeilingGate::Admitted(None);
        };
        if plan.total_budget.is_none() {
            return CeilingGate::Admitted(None);
        }
        let since = plan.period.period_start_millis(crate::ports::now_millis());
        let samples = match read_spend_for_gate(deps.meter.as_deref(), company, since).await {
            Ok(samples) => samples,
            Err(fault) => {
                match &fault {
                    SpendReadFault::NoMeter => tracing::error!(
                        company = %company,
                        agent = agent_id,
                        "[capability-budget] a total token ceiling is declared but this host has no usage meter; refusing dispatch (no model call) until a meter is configured or the ceiling is removed"
                    ),
                    SpendReadFault::QueryFailed(error) => tracing::error!(
                        company = %company,
                        agent = agent_id,
                        %error,
                        "[capability-budget] total-ceiling spend query failed; refusing dispatch (no model call) rather than spending against a ceiling that cannot be checked"
                    ),
                }
                return CeilingGate::Refused(spend_gate_refusal(
                    unmeasurable_ceiling_notice(&fault),
                    SpendGateCause::Unmeasurable,
                ));
            }
        };

        let spent = capability_budget::tokens_in(&samples);
        // Issue #1846: the coarse pre-task proximity warning, read BESIDE the
        // exhaustion check below — same query, same samples, no second meter
        // read. Published, never returned: a warning is non-blocking, so the
        // turn keeps dispatching normally whether or not a console is
        // listening.
        if let Some(cap) = plan.total_budget
            && !plan.total_exhausted(spent)
            && is_approaching_budget_ceiling(spent, cap)
        {
            tracing::info!(
                company = %company,
                spent,
                cap,
                "[capability-budget] approaching the total token ceiling; publishing a non-blocking proximity warning"
            );
            crate::turn_stream::publish(
                company,
                crate::turn_stream::BudgetProximityFrame {
                    kind: "budget_proximity",
                    agent_id: None,
                    message: budget_proximity_message(),
                    at_millis: crate::ports::now_millis(),
                },
            );
        }
        // The reservation, not the bare comparison, is what makes the ceiling
        // bind: the meter reports only finished work, so several turns reading
        // one `spent` would each find room and each dispatch. A promise
        // recorded here is visible to the next caller before it looks.
        match crate::metering::reserve(company, Self::DISPATCH_RESERVATION_TOKENS, spent, plan) {
            Some(reservation) => CeilingGate::Admitted(Some(reservation)),
            None => {
                tracing::info!(
                    company = %company,
                    agent = agent_id,
                    spent,
                    "[capability-budget] total token ceiling reached; refusing dispatch (no model call) until the period resets"
                );
                CeilingGate::Refused(spend_gate_refusal(
                    TOTAL_BUDGET_EXHAUSTED_NOTICE.to_string(),
                    SpendGateCause::Exhausted,
                ))
            }
        }
    }

    async fn monthly_budget_refusal(
        &self,
        company: &CompanyId,
        agent_id: &str,
        deps: &HarnessDeps,
    ) -> MonthlyBudgetGate {
        let Some(configured_cap) = self
            .monthly_budgets
            .read()
            .await
            .get(company)
            .copied()
            .flatten()
        else {
            return MonthlyBudgetGate::Admitted(None);
        };

        let guard = monthly_spend_lock(company).lock_owned().await;
        let record = match deps.store.load(company).await {
            Ok(Some(record)) => record,
            Ok(None) => {
                tracing::error!(
                    company = %company,
                    agent = agent_id,
                    cap = configured_cap,
                    "[company-budget] company record is unavailable; refusing inference dispatch"
                );
                return MonthlyBudgetGate::Refused(spend_gate_refusal(
                    unmeasurable_monthly_budget_notice(configured_cap),
                    SpendGateCause::Unmeasurable,
                ));
            }
            Err(error) => {
                tracing::error!(
                    company = %company,
                    agent = agent_id,
                    cap = configured_cap,
                    %error,
                    "[company-budget] ledger read failed; refusing inference dispatch"
                );
                return MonthlyBudgetGate::Refused(spend_gate_refusal(
                    unmeasurable_monthly_budget_notice(configured_cap),
                    SpendGateCause::Unmeasurable,
                ));
            }
        };

        let Some(cap) = record.manifest.budget.monthly_usd else {
            return MonthlyBudgetGate::Admitted(None);
        };
        let spent = crate::metering::finances_from(
            &record.ledger,
            &record.manifest.budget,
            None,
            crate::ports::now_millis(),
        )
        .spent_usd;
        if spent >= cap {
            tracing::info!(
                company = %company,
                agent = agent_id,
                spent,
                cap,
                "[company-budget] monthly spend cap reached; refusing inference dispatch"
            );
            return MonthlyBudgetGate::Refused(spend_gate_refusal(
                monthly_budget_exhausted_notice(cap),
                SpendGateCause::Exhausted,
            ));
        }

        MonthlyBudgetGate::Admitted(Some(guard))
    }

    /// Runs one **confined** turn (issue #416): an ephemeral agent with no
    /// tools, no company memory and no roster identity, for a question about one
    /// object rather than about the company.
    ///
    /// Deliberately not a variant of [`run_inner`](Self::run_inner), because the
    /// two differ in what they are allowed to touch rather than in a flag:
    ///
    /// * the agent is **built here and dropped after**, so it is never in the
    ///   pooled roster and cannot be addressed, dispatched or delegated to;
    /// * there is **no retrieve→inject** — the company's prior task outcomes are
    ///   not prepended to the message, so the model cannot answer from work it
    ///   was not asked about;
    /// * there is **no memory writeback** — the exchange leaves nothing for a
    ///   later company turn to retrieve, so a confined conversation cannot
    ///   become unconfined context tomorrow.
    ///
    /// What it does share: the plan-level token ceiling (spend is spend), live
    /// turn streaming onto the addressed thread, and cost recording, so a
    /// confined turn is billed and observable exactly like any other.
    pub async fn run_confined(
        &self,
        company: &CompanyId,
        company_name: &str,
        message: &str,
        deps: &HarnessDeps,
        chat_id: Option<&str>,
        confinement: &confine::Confinement,
    ) -> crate::Result<TurnOutcome> {
        let _ceiling =
            match Self::total_ceiling_refusal(company, confine::CONFINED_AGENT_ID, deps).await {
                CeilingGate::Admitted(reservation) => reservation,
                CeilingGate::Refused(refusal) => return Ok(refusal),
            };

        let _monthly_budget = match self
            .monthly_budget_refusal(company, confine::CONFINED_AGENT_ID, deps)
            .await
        {
            MonthlyBudgetGate::Admitted(guard) => guard,
            MonthlyBudgetGate::Refused(refusal) => return Ok(refusal),
        };

        let runtime = crate::harness::openhuman_runtime::global(
            crate::harness::openhuman_runtime::RuntimeBoot::from_env(),
        )
        .await?;
        self.mcp.serve_loopback().await.map_err(|err| {
            OpenCompanyError::Harness(format!("bind the opencompany MCP listener: {err}"))
        })?;
        let blueprint = confine::build_confined_agent(company, company_name, confinement, deps)?;
        // Registered under a per-turn id: the copilot is not on the roster,
        // and two confined turns of one company may overlap.
        let turn_id = format!(
            "{}-{}",
            confine::CONFINED_AGENT_ID,
            uuid::Uuid::new_v4().simple()
        );
        let agent = CompanyAgent::register(
            &runtime,
            company,
            &turn_id,
            "Workflow copilot",
            None,
            blueprint,
            deps.events.clone(),
        )?;

        let stream_ctx = Some(crate::turn_stream::TurnStreamCtx {
            company: company.clone(),
            agent_id: confine::CONFINED_AGENT_ID.to_string(),
            route: crate::turn_stream::LiveRoute::Chat {
                chat_id: chat_id
                    .map(str::to_string)
                    .unwrap_or_else(|| crate::server::ops::language::DEFAULT_DESK.to_string()),
            },
            // A copilot turn is addressed by `chat_id` alone — this entry point
            // takes no `ChatTarget` — so its frames key by thread, as every
            // frame did before `messageSeq` existed. A copilot thread runs one
            // turn at a time, so there is nothing here for the finer key to
            // separate.
            message_seq: None,
        });

        // The message goes to the model AS SENT. This is the retrieve→inject
        // step's absence, and it is the difference between "grounded in one
        // workflow" and "confined to one workflow". Empty seed for the same
        // reason (issue #1840): a confined turn is intentionally context-free, so
        // it carries none of the desk's recent history.
        let (outcome, turn_costs) = agent
            .run_with_steer(
                message,
                None,
                stream_ctx,
                None,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await;

        // Metered before the outcome is unwrapped: a copilot turn that failed
        // still consumed whatever it consumed before it failed.
        let metered = meter_turn_costs(
            &turn_costs,
            confine::CONFINED_AGENT_ID,
            company,
            deps,
            agent.chat_model.as_ref(),
            None,
        )
        .await;
        turn_result_after_metering(outcome, metered, company, confine::CONFINED_AGENT_ID)
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_inner(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        deps: &HarnessDeps,
        steer: Option<&SteerControl>,
        live: LiveStream<'_>,
        // Which conversation this turn belongs to, independent of whether it
        // streams (#1890 I). For a live chat turn it is the same pair `live`
        // carries; for an approval's re-issued call it is the conversation the
        // approval was raised in, with no stream at all.
        chat: crate::runtime::delegation::ChatTarget<'_>,
        run_sink: Option<Arc<run_trace::RunTraceSink>>,
    ) -> crate::Result<TurnOutcome> {
        let agent = {
            let guard = self.agents.read().await;
            let roster = guard
                .get(company)
                .ok_or_else(|| OpenCompanyError::CompanyNotFound(company.to_string()))?;
            roster
                .iter()
                .find(|a| a.agent_id == agent_id)
                .cloned()
                .ok_or_else(|| {
                    OpenCompanyError::InvalidRequest(format!(
                        "agent '{agent_id}' is not on company '{company}' roster"
                    ))
                })?
        };

        // Renew the agent's sandbox directory at the moment it acts (issue
        // #409). `build_agent` already created it, but a roster is built once
        // and then cached behind fingerprints — and handed *across* an in-place
        // rebuild — so a workspace that goes missing afterwards (a restored or
        // wiped data dir, an operator clearing the tree, a boot that raced a
        // not-yet-mounted volume) would otherwise stay missing for the life of
        // the process, and every relative file write would be refused as if it
        // had tried to escape the sandbox. Two syscalls on the already-exists
        // path, against a turn that is about to call a model — not worth
        // deferring off the runtime thread.
        //
        // Deliberately not fatal, for the same reason `build_agent`'s attempt is
        // not: an agent with no file grant runs a perfectly good turn without
        // this directory. The `error!` (not `warn!`) records the one condition
        // under which the misdirecting guard message can still be reached, so it
        // is greppable next to the refusal it explains. Both of those are the
        // right calls and issue #449 does not change either.
        //
        // What #449 changes is only how often it is *said*. A workspace root
        // that cannot be written — a volume that failed to mount, a path that
        // resolves onto a file — fails identically on every dispatch, so the
        // unconditional `error!` emitted one byte-identical line per turn,
        // forever, with nothing distinguishing the thousandth from the first.
        // The state is edge-triggered instead: the first failure reads exactly
        // as it did before, the repeats are silent, and a recovery gets one
        // `info!` so a reader who saw the error learns when it ended. The
        // attempt itself still runs every dispatch — see
        // `note_workspace_attempt` for why memoising it would be a regression.
        let attempt = build::ensure_agent_workspace(&deps.workspace_root, company, agent_id);
        let report = self.note_workspace_attempt(company, agent_id, attempt.is_err());
        if !report.is_silent() {
            let workspace = build::agent_workspace(&deps.workspace_root, company, agent_id);
            match attempt {
                Err(error) => tracing::error!(
                    company = %company,
                    agent = agent_id,
                    workspace = %workspace.display(),
                    %error,
                    "[harness] could not create the agent workspace before dispatch; relative file writes will be refused (the refusal will read as a workspace escape, but the cause is this missing directory)"
                ),
                Ok(_) => tracing::info!(
                    company = %company,
                    agent = agent_id,
                    workspace = %workspace.display(),
                    "[harness] agent workspace is available again; the earlier creation failure has cleared and relative file writes work"
                ),
            }
        }

        // Plan-level total-token ceiling (issue #188): a HARD dispatch refusal
        // that never reaches the model once the tenant's total period spend
        // crosses the cap. The per-namespace budget gate in `ensure` is *soft* —
        // it only trims which exec tools the roster carries; an exhausted
        // tenant's turn still runs on intrinsic tools and burns model tokens.
        // This closes that gap by refusing dispatch outright, before any model
        // call, on every path that funnels through `run_inner` (operator chat,
        // task, steered/background). We return early here — before retrieve→
        // inject and the memory writeback — so a refused turn costs nothing and
        // leaves no fabricated outcome in the memory store.
        //
        // One rule, applied at every spend gate: a declared cap is BINDING, and
        // a gate that cannot read the spend it bounds refuses the priced
        // operation rather than admitting it. Dispatch is unconditionally
        // priced — it is about to call a model — so admitting one against an
        // unreadable ceiling spends real money outside a bound nobody can
        // observe, and the console goes on rendering that ceiling as if it
        // still applied. A refusal an operator can see and act on is a better
        // state than a cap that silently stopped existing.
        let _ceiling = match Self::total_ceiling_refusal(company, agent_id, deps).await {
            CeilingGate::Admitted(reservation) => reservation,
            CeilingGate::Refused(refusal) => return Ok(refusal),
        };

        let _monthly_budget = match self.monthly_budget_refusal(company, agent_id, deps).await {
            MonthlyBudgetGate::Admitted(guard) => guard,
            MonthlyBudgetGate::Refused(refusal) => return Ok(refusal),
        };

        // Per-agent daily spend cap (issue #304): the same HARD, pre-model-call
        // refusal as the ceiling above, scoped to ONE teammate.
        //
        // This is the layer that matters most in practice. The manifest's
        // `budget_usd_daily` was validated, persisted and passed to
        // `ApprovalPolicy` — where it sat on a field with no reader. But the
        // dominant spend stream is not tool calls at all, it is inference, and
        // inference never reaches a `ToolPolicy`. Gating only priced tool calls
        // (the policy arm) would leave a capped teammate free to burn its budget
        // many times over on model turns alone, which is how the cap came to be
        // decorative in the first place.
        //
        // Refused BEFORE retrieve→inject and the memory writeback, exactly like
        // the total ceiling, so a refused turn costs nothing and leaves no
        // fabricated outcome in the store. The reply names the teammate, the cap
        // and the reset — never a bare failure.
        //
        // Scoped to the desk that declared a bound: an uncapped colleague is
        // never gated by a meter fault, so an unreadable meter costs the company
        // its capped teammates, not its cognition.
        if let Some(cap) = agent.budget_usd_daily {
            let since = crate::metering::utc_day_start_millis(crate::ports::now_millis());
            let samples = match read_spend_for_gate(deps.meter.as_deref(), company, since).await {
                Ok(samples) => samples,
                Err(fault) => {
                    match &fault {
                        SpendReadFault::NoMeter => tracing::error!(
                            company = %company,
                            agent = agent_id,
                            cap,
                            "[agent-budget] a daily spend cap is declared but this host has no usage meter; refusing dispatch to this teammate (no model call) until a meter is configured or the cap is removed"
                        ),
                        SpendReadFault::QueryFailed(error) => tracing::error!(
                            company = %company,
                            agent = agent_id,
                            cap,
                            %error,
                            "[agent-budget] daily-spend query failed; refusing dispatch to this teammate (no model call) rather than spending against a cap that cannot be checked"
                        ),
                    }
                    return Ok(spend_gate_refusal(
                        unmeasurable_agent_budget_notice(agent_id, cap, &fault),
                        SpendGateCause::Unmeasurable,
                    ));
                }
            };

            let spent = crate::metering::usd_spent_by_agent(&samples, agent_id);
            // Issue #1846: same coarse proximity warning as the total-ceiling
            // read above, reusing the SAME `samples` — no second query.
            // Non-blocking; only fires when this teammate is not already
            // refused below.
            if spent < cap && is_approaching_budget_ceiling_f64(spent, cap) {
                tracing::info!(
                    company = %company,
                    agent = agent_id,
                    spent,
                    cap,
                    "[agent-budget] approaching the daily spend cap; publishing a non-blocking proximity warning"
                );
                crate::turn_stream::publish(
                    company,
                    crate::turn_stream::BudgetProximityFrame {
                        kind: "budget_proximity",
                        agent_id: Some(agent_id.to_string()),
                        message: budget_proximity_message_usd(agent_id),
                        at_millis: crate::ports::now_millis(),
                    },
                );
            }
            if spent >= cap {
                tracing::info!(
                    company = %company,
                    agent = agent_id,
                    spent,
                    cap,
                    "[agent-budget] daily spend cap reached; refusing dispatch (no model call) until 00:00 UTC"
                );
                return Ok(spend_gate_refusal(
                    agent_budget_exhausted_notice(agent_id, cap),
                    SpendGateCause::Exhausted,
                ));
            }
        }

        // Retrieve→inject: pull the top-K prior task outcomes relevant to this
        // message and prepend them as context. On a cold store this yields no
        // hits and the message is passed through unchanged.
        //
        // Skipped entirely for a chat-only turn (issue #1725): a greeting /
        // "Just chatting" reply must not be grounded in prior task outcomes, and
        // pulling them is the exact context leak the fast path exists to stop.
        let augmented = if crate::runtime::delegation::is_chat_only_turn() {
            message.to_string()
        } else {
            // **Retrieved on the operator's own words, injected into the
            // composed message** (#1890 review). `message` may already carry
            // this turn's in-memory briefings — open work, the settled-work
            // digest, the thread index, attachment markers — and those are for
            // the model to read, not for the store to search on. Retrieving on
            // them made the query drift toward whatever the briefings happened
            // to name: the settled digest is a list of finished card titles, so
            // a conversation that had just closed some work recalled *that*
            // work rather than what the operator was asking about, and grew
            // more biased with every card that finished.
            //
            // `operator_words` is the existing seam for this — the same cut the
            // triage decision takes, and for the same reason its docs give: the
            // annotations are not something anybody typed.
            let hits = deps
                .context
                .search(
                    company,
                    crate::runtime::delegation::operator_words(message),
                    memory_loop::RETRIEVE_TOP_K,
                )
                .await?;
            memory_loop::inject(message, &hits)
        };

        // Run the turn and record its real cost. `CompanyAgent::run` reads each
        // attempt's token/cost totals from the bridge's usage tap
        // accessor and returns one entry per attempt (two when the empty-response
        // wrapper retried once). A zero-usage attempt (offline provider) writes
        // nothing, so the inert-metering contract holds.
        // Live tool-call streaming: for a turn that surfaces an operator chat
        // bubble (`live`), hand the runner the routing context so it tees each
        // progress event onto the company's transient turn-stream bus as it
        // happens (the console renders the timeline live). Background turns —
        // dispatched task cards and workflow agent nodes — pass `live = false`
        // and stream nothing, since they carry no chat thread to render onto and
        // their frames would otherwise misattribute to the active chat (#125
        // review). Either way the durable `TurnStep`s still fold from the same
        // buffered events at turn end.
        // The chat thread this turn answers, if any — captured before `live` is
        // consumed by the `stream_ctx` match below. Only a chat turn (`On`) seeds
        // recent history; a background task or workflow node carries no chat
        // thread to bind history to (issue #1840).
        let stream_ctx = match live {
            LiveStream::On { chat_id, .. } => Some(crate::turn_stream::TurnStreamCtx {
                company: company.clone(),
                agent_id: agent_id.to_string(),
                // The chat/desk thread this turn answers — the same id journaled
                // as `AgentReply.chat_id`, so the console keys the live timeline
                // on it and concurrent turns on different threads never
                // cross-attribute. Falls back to the default desk to match the
                // durable reply when the caller addressed no desk (e.g. an API
                // client that omits `chat`).
                route: crate::turn_stream::LiveRoute::Chat {
                    chat_id: chat_id
                        .map(str::to_string)
                        .unwrap_or_else(|| crate::server::ops::language::DEFAULT_DESK.to_string()),
                },
                // The operator message this turn answers, read off the
                // `ChatTarget` the caller already passes. Nothing new is
                // threaded through the runtime to get it here — and pointedly
                // NOT used to decide *whether* to stream, which is the
                // conflation #1890 I removed and the revert of aa2787e9a
                // re-established. `LiveStream` still decides that alone.
                message_seq: chat.message_seq.map(|seq| seq.value()),
            }),
            // A workflow agent node (issue #1702): streams live, but keyed on
            // the workflow run + node so its frames land on the console's
            // run-trace sheet rather than misattributing to a chat thread.
            LiveStream::Workflow { run_id, node_id } => Some(crate::turn_stream::TurnStreamCtx {
                company: company.clone(),
                agent_id: agent_id.to_string(),
                route: crate::turn_stream::LiveRoute::Workflow {
                    run_id: run_id.to_string(),
                    node_id: node_id.to_string(),
                },
                // A workflow node answers a graph, not a message.
                message_seq: None,
            }),
            LiveStream::Off => None,
        };
        // Issue #6014: what this turn is for, in scope for its whole duration, so
        // an oversized tool result can be extracted against the task instead of
        // cut on a byte boundary. `operator_words` for the reason its own docs
        // give — `message` here is the composed text and carries the cycle's
        // briefings, which are not what anybody asked for.
        // What a seat says through the `opencompany` MCP server's speech
        // tools reaches the driver through the seat scope
        // (`delegation::seat_turn`), never through the reply text: on a hive
        // seat turn the reply is the seat's own thinking, and on every other
        // turn there is no speech tool to call.
        let (outcome, turn_costs) = deps
            .approval_requests
            .turn_scoped(agent.run_with_steer(
                &augmented,
                steer,
                stream_ctx,
                run_sink.clone(),
                // The caller's own, not read off `live` (#1890 I). A turn can
                // have a conversation and stream nothing.
                chat,
            ))
            .await;
        // Issue B-120: bank what the turn spent BEFORE its result is unwrapped.
        //
        // Both consumers of `turn_costs` used to sit below a `?` on this very
        // await, so a turn that ended in a hard error — a wall-clock ceiling
        // above all, which fires precisely *because* the agent worked for ten
        // minutes — reached neither of them. The attempt row settled with a
        // default `TokenUsage`, the ledger got no `inference.spend` entry, and
        // the meter got no `UsageSample`: the console reported the most
        // expensive runs a founder owns as free, and the company-wide total
        // agreed with it, because the spend had never been recorded anywhere.
        //
        // First consumer: the attempt row's own total (issue #242). Per turn,
        // not once at the end, so a redirect re-run and a delegate's turn both
        // count — an attempt's cost is what the attempt spent. This is a second
        // *reader* of `turn_costs`, not a second writer: the ledger and the
        // usage meter below stay the only places money is recorded.
        if let Some(sink) = run_sink.as_ref() {
            for turn_cost in &turn_costs {
                sink.add_usage(turn_cost);
            }
        }
        // Second consumer: the ledger and the usage meter. Issue #242 also
        // attributes the sample to the attempt this turn ran under, so "what did
        // this run cost?" is answerable from the meter as well as from the row.
        let metered = meter_turn_costs(
            &turn_costs,
            agent_id,
            company,
            deps,
            agent.chat_model.as_ref(),
            run_sink.as_ref().map(|s| s.run_id()),
        )
        .await;
        // Issue #1846, Codex review (PR #2053): the budget-pause park/retire
        // side effects below read the turn's OWN outcome, and must run before
        // `turn_result_after_metering`'s `?` — a ledger write that fails is a
        // problem with the METER, not with what this turn actually did, and
        // must not also swallow a genuine pause marker (the operator's only
        // "add credits and resend" path) or a genuine retirement of a stale
        // one (leaving a stale CTA that could later re-dispatch a
        // potentially non-idempotent request a second time). `meter_turn_costs`
        // itself still runs first and unconditionally, exactly as
        // `meter_turn_costs`'s own doc requires — this only reorders reading
        // `outcome` for these two side effects ahead of the point that
        // `outcome` might get replaced by a metering error.
        //
        // Issue #1846: park a durable re-issue marker the moment a pause is
        // seen, mirroring the grant-reissue precedent (`crate::runtime::grants`)
        // — mint on the event that needs a later redemption, not on whatever
        // happens to read the outcome next. `message` (not `augmented`) is
        // parked: the operator's own words are what gets re-sent, and
        // retrieve→inject re-runs fresh against whatever memory looks like at
        // redeem time rather than replaying a stale injection.
        if let Ok(turn_outcome) = &outcome {
            if let Some(pause) = &turn_outcome.budget_paused {
                let chat_id = match live {
                    LiveStream::On { chat_id, .. } => chat_id.map(str::to_string),
                    LiveStream::Workflow { .. } | LiveStream::Off => None,
                };
                // Issue #1846 review (Codex #3869193112): whether an operator
                // was ever addressing this turn AT ALL, not just whether they
                // named a specific desk — see `BudgetPauseMarker::background`'s
                // doc for why this is a different question from `chat_id`
                // above, which is `None` for BOTH an unaddressed interactive
                // message and a background turn alike.
                let is_background = matches!(live, LiveStream::Workflow { .. } | LiveStream::Off);
                // Issue #1846 review (Codex #3865812419/#3865812423/#3865812432):
                // the ambient parent/deliverable/mentions the cycle was started
                // with, so a redeem replays the operator's ORIGINAL
                // thread/intent/audience instead of the empty defaults
                // `redeem_budget_pause` used to fall back to.
                let redeem_context = crate::runtime::grants::current_redeem_context();
                // Issue #1846 review (Codex #3866418891): `message` here is
                // whatever this turn actually ran with — for an operator-message
                // turn that is `composed`, already carrying `with_attachment_refs`
                // markers baked into the text, which would double up with
                // `redeem_context.attachments` below once `redeem_budget_pause`
                // recomposes them fresh. The ambient context's own RAW text (set
                // once, from the ORIGINAL `OperatorMessage`, before any composing
                // happened) is preferred whenever one is in scope; falling back to
                // the local `message` only for a cycle with no `OperatorMessage`
                // at all (a workflow node's own background turn), which has no
                // raw/composed split — and no attachments — to begin with.
                let park_message = redeem_context.text.clone().unwrap_or_else(|| {
                    // Issue #1890 E: the operator's own words, which is what this
                    // fallback has always claimed to hold. `message` here is the
                    // composed turn text, so it carries whatever the cycle appended
                    // — the open-work briefing, the settled-work one, the thread
                    // index — and parking that bakes a machine briefing into the
                    // request a redeem re-sends. It was already reachable through
                    // the #176 briefing whenever the agent had open cards; the
                    // thread index made it reachable on an ordinary channel, which
                    // is how `redeem_replays_the_markers_attachments` caught it.
                    crate::runtime::delegation::operator_words(message).to_string()
                });
                let pauses = crate::runtime::grants::budget_pauses_for(company);
                let marker = if is_background {
                    pauses.park_background(
                        pause.agent.clone(),
                        chat_id,
                        park_message,
                        pause.summary.clone(),
                        crate::ports::now_millis(),
                        redeem_context,
                    )
                } else {
                    pauses.park(
                        pause.agent.clone(),
                        chat_id,
                        park_message,
                        pause.summary.clone(),
                        crate::ports::now_millis(),
                        redeem_context,
                    )
                };
                tracing::info!(
                    company = %company,
                    agent = %pause.agent,
                    marker_id = %marker.id,
                    "[budget-pause] parked a re-issue marker; the operator can redeem it once credits are added"
                );
            } else if let Some(stale) = {
                // Issue #1846 review (Codex #3869792503, tightened by
                // #3869968949): match on the SAME saved-request CONTEXT
                // `park_message`/`park`/`park_background` above parks a marker
                // under — text, chat thread, parent, deliverable, mentions AND
                // attachments — not an unconditional `redeem` and not text
                // alone. An unrelated turn for this agent (an automatic
                // background task, a second chat message about something else
                // entirely, or even a coincidentally-identical-text request in a
                // DIFFERENT thread) succeeding first must not silently drop the
                // marker for a DIFFERENT, still-unretried original request. A
                // resend, by construction, runs with the SAME context the
                // marker parked; an unrelated success does not.
                let candidate_chat_id = match live {
                    LiveStream::On { chat_id, .. } => chat_id.map(str::to_string),
                    LiveStream::Workflow { .. } | LiveStream::Off => None,
                };
                let candidate_redeem = crate::runtime::grants::current_redeem_context();
                let candidate_message = candidate_redeem.text.clone().unwrap_or_else(|| {
                    // Stripped on exactly the terms the park above is, or the
                    // retire-match would compare a briefing-laden candidate against
                    // a clean parked marker and never retire it (#1890 E).
                    crate::runtime::delegation::operator_words(message).to_string()
                });
                crate::runtime::grants::budget_pauses_for(company).retire_if_message_matches(
                    agent_id,
                    &candidate_message,
                    candidate_chat_id.as_deref(),
                    &candidate_redeem,
                )
            } {
                // Issue #1846 review (Codex #3868962381): this turn just
                // completed WITHOUT pausing, which is proof the account that
                // blocked the LAST turn now has budget again — whether the
                // operator got there by clicking "Add credits & resend" (which
                // already took the marker itself, so this finds nothing) or, as
                // the notice's own copy also invites, by manually adding credits
                // and resending the message from the composer, bypassing the
                // CTA/redeem route entirely. Only the second path used to leave
                // the marker parked: nothing but a click on THIS specific CTA
                // ever consumed it, so a manual resend left a stale marker and
                // its stale CTA sitting on the old notice indefinitely. Clicking
                // it later would silently re-dispatch the OLD message a second
                // time — a duplicate, and for a non-idempotent request, a
                // duplicate side effect the operator never asked for.
                //
                // `retire_if_message_matches`, not a peek-then-drop: single
                // atomic check-and-take, same as every other consumer of this
                // set, so a concurrent CTA click racing this retire cannot
                // double-consume the same marker.
                tracing::info!(
                    company = %company,
                    agent = %agent_id,
                    marker_id = %stale.id,
                    "[budget-pause] retired a stale re-issue marker; this agent's turn succeeded \
                     without it, so the pause it named is already resolved"
                );
            }
        }
        let outcome = turn_result_after_metering(outcome, metered, company, agent_id)?;
        // Store: persist the outcome (original task + reply) so it compounds
        // into later turns. Without this the harness never writes memory back.
        // SECURITY: the reply **text only** — the scrubbed `outcome.steps` never
        // enter the memory store, so a step detail can never be retrieved and
        // re-injected into a later turn.
        //
        // TAINT (issue #1113): deliberately `deps.context` (Internal), not
        // the runtime's inbound port. Harness turns are operator-triggered —
        // `OperatorMessage` is operator speech, the same authorship precedent
        // that stamps operator facts Internal — while channel/webhook content
        // enters through the cycle path, which routes its puts through the
        // inbound port (`CycleHostImpl::external_trigger`). If a harness turn
        // ever takes a webhook trigger, that turn must route its store half
        // through the runtime's inbound port — the cycle path shows the shape.
        // Issue #1846: a budget-paused turn's `reply` is the actionable "add
        // credits" halt copy, not an answer the teammate produced — the
        // pre-flight refusals above (`total_ceiling_refusal`, the per-agent
        // cap) already skip this writeback entirely by returning early, before
        // ever reaching it. This turn does not return early (the model call
        // was actually attempted and failed), so it has to be excluded here
        // instead. Writing it back would recall "you are out of credits" as
        // prior context in the NEXT turn, and — worse — as something this
        // teammate is on record having said.
        if !matches!(
            steer.and_then(SteerControl::pending),
            Some(SteerAction::Cancel)
        ) && outcome.budget_paused.is_none()
        {
            deps.context
                .put(
                    company,
                    memory_loop::outcome_chunk(agent_id, message, &outcome.reply),
                )
                .await?;
        }

        Ok(outcome)
    }

    /// The `opencompany` MCP host the pool's agents are served on.
    pub fn mcp(&self) -> &Arc<McpHost> {
        &self.mcp
    }

    /// The turns in flight across every agent the pool serves — the hive
    /// driver registers a seat's turn here before `agent.turn(..)` and takes
    /// its outbox back after.
    pub fn in_flight(&self) -> &Arc<crate::hive::tools::InFlightRegistry> {
        self.mcp.in_flight()
    }

    /// The loopback address the MCP listener is bound to, once
    /// [`ensure`](Self::ensure) has run.
    pub fn mcp_addr(&self) -> Option<std::net::SocketAddr> {
        self.mcp.addr()
    }

    /// The live agent `agent_id` of `company`, if the roster holds one.
    pub async fn agent(&self, company: &CompanyId, agent_id: &str) -> Option<Arc<CompanyAgent>> {
        self.agents
            .read()
            .await
            .get(company)?
            .iter()
            .find(|agent| agent.agent_id == agent_id)
            .cloned()
    }

    /// Number of companies currently resident in the pool (test/observability).
    pub async fn resident_companies(&self) -> usize {
        self.agents.read().await.len()
    }

    /// The agent ids this pool currently holds for `company`, in roster order.
    ///
    /// Observability for the per-harness split: a pool serving one named harness
    /// should hold only that harness's agents, and this is how that is checked
    /// without reaching into the lock.
    pub async fn agent_ids(&self, company: &CompanyId) -> Vec<String> {
        self.agents
            .read()
            .await
            .get(company)
            .map(|agents| agents.iter().map(|a| a.agent_id.clone()).collect())
            .unwrap_or_default()
    }
}

/// A stable fingerprint of an effective MCP server set, used to detect a console
/// change (add / remove / enable-toggle / token rotation) between
/// [`HarnessPool::ensure`] calls. Hashes only non-secret configuration plus the
/// credential substrings — the resulting `u64` is non-reversible and never
/// surfaces anywhere, so it is not a credential leak, and hashing the credential
/// substrings means a rotate-token also invalidates the cached roster.
fn mcp_fingerprint(decls: &[McpServerDecl]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    decls.len().hash(&mut hasher);
    for decl in decls {
        decl.name.hash(&mut hasher);
        decl.endpoint.hash(&mut hasher);
        decl.enabled.hash(&mut hasher);
        decl.description.hash(&mut hasher);
        decl.allowed_tools.hash(&mut hasher);
        decl.disallowed_tools.hash(&mut hasher);
        decl.timeout_secs.hash(&mut hasher);
        auth_kind(&decl.auth).hash(&mut hasher);
        for secret in decl.auth.secret_values() {
            secret.hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// A small discriminant for an [`AuthMaterial`] variant, for the fingerprint.
fn auth_kind(material: &crate::company::mcp::AuthMaterial) -> u8 {
    use crate::company::mcp::AuthMaterial::*;
    match material {
        None => 0,
        Bearer(_) => 1,
        Header { .. } => 2,
        QueryParam { .. } => 3,
        OAuth { .. } => 4,
    }
}

/// Refreshes any near-expiry console-OAuth credential in `decls` before the
/// registry is built, re-persisting the rotated token **write-only** so agents
/// never send an expired bearer. Per-tenant analogue of OpenHuman's
/// `mcp_registry::oauth::refresh_if_expired`. A refresh failure is non-fatal —
/// the old token is kept and the next `401` re-prompts sign-in.
#[cfg(feature = "mcp")]
async fn refresh_oauth_decls(
    company: &CompanyId,
    decls: &mut [McpServerDecl],
    secrets: &dyn SecretStore,
) {
    use crate::company::mcp_oauth;

    for decl in decls.iter_mut() {
        if !mcp_oauth::needs_refresh(&decl.auth, 60) {
            continue;
        }
        let Some(new_material) = mcp_oauth::refresh(&decl.auth).await else {
            continue;
        };
        match crate::company::mcp::store_auth(company, &decl.name, &new_material, secrets).await {
            Ok(()) => decl.auth = new_material,
            Err(err) => log::warn!(
                "[mcp-oauth] failed to persist refreshed token for `{}`: {}",
                decl.name,
                err.code()
            ),
        }
    }
}

/// Without the `mcp` feature there is no OAuth credential to refresh, so this is
/// a no-op (keeps `resolve_effective_mcp` uniform across the two builds).
#[cfg(not(feature = "mcp"))]
async fn refresh_oauth_decls(
    _company: &CompanyId,
    _decls: &mut [McpServerDecl],
    _secrets: &dyn SecretStore,
) {
}

/// Whether an override row is purely a face — `avatar` set and nothing the
/// harness reads set. Such a row must not move a fingerprint: the fingerprints
/// hash what a teammate *is* (name, role, description, toolbelt, persona,
/// model, harness), and an avatar is none of those. A row that changed only the
/// face would otherwise count itself and its `agent_id` into the hash, rebuild
/// the roster, and drop every live agent session for a cosmetic change — issue
/// #1676's review note.
///
/// An explicit `Some(vec![])` tool list, `Some("")` instructions and `Some("")`
/// model/harness (the stored form of "cleared") stay real overrides ("the
/// company's standard grant" / "cleared" / "the blueprint's model and harness"),
/// so only the all-`None` row is filtered, not the emptied one.
fn is_avatar_only(edit: &crate::ports::types::AgentOverride) -> bool {
    edit.name.is_none()
        && edit.role.is_none()
        && edit.description.is_none()
        && edit.tools.is_none()
        && edit.instructions.is_none()
        && edit.model.is_none()
        && edit.harness.is_none()
        && edit.provider.is_none()
}

/// A stable fingerprint of the roster overlay — the operator-added teammates
/// (issue #71) **and** the operator's edits of the manifest-declared ones —
/// used to detect a teammate add/remove/edit between [`HarnessPool::ensure`]
/// calls. Mirrors [`mcp_fingerprint`]'s shape; no secrets are involved here so
/// there is nothing to scrub — both are display data.
///
/// The edits share this axis rather than taking one of their own because they
/// answer the same question it does: *who is on this roster, and as what*. They
/// have to move something — a persona and a tool belt are assembled once per
/// roster, so a console rename that moved no fingerprint would persist, read
/// back correctly on the Team page, and be invisible to every turn the teammate
/// took until the process restarted.
///
/// Avatar-only rows ([`is_avatar_only`]) are excluded: the face a teammate
/// wears is display data resolved at render time, never part of the persona the
/// harness builds, so a choice of face must not discard live agent sessions.
fn overlay_fingerprint(
    agents: &[OverlayAgent],
    edits: &[crate::ports::types::AgentOverride],
    retired: &[String],
) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    // A removal has to move this too, and it is the sharpest case of all: a
    // retired teammate that stayed in a cached roster would still take turns
    // and still receive delegations after the console said it was gone.
    // Order-stable for the same reason the edits are — `retire_agent` appends
    // and never re-appends an id it already holds.
    retired.len().hash(&mut hasher);
    for id in retired {
        id.hash(&mut hasher);
    }
    // Hashed in stored order, which `upsert_agent_override` keeps stable: it
    // replaces an existing entry in place and only ever appends a new one, so a
    // repeated edit of one teammate does not permute the list and drop every
    // live session for a change that touched nobody else. Avatar-only rows are
    // skipped (`is_avatar_only`): they carry no persona, so they must not move
    // the fingerprint.
    let persona_edits: Vec<_> = edits.iter().filter(|edit| !is_avatar_only(edit)).collect();
    persona_edits.len().hash(&mut hasher);
    for edit in persona_edits {
        edit.agent_id.hash(&mut hasher);
        edit.name.hash(&mut hasher);
        edit.role.hash(&mut hasher);
        edit.description.hash(&mut hasher);
        edit.tools.hash(&mut hasher);
        // A routing override changes the harness binding the roster must build,
        // so it has to move this fingerprint too — otherwise re-binding one
        // teammate to another model/harness would persist and be silently
        // ignored until the next process restart (issue #1676 review note).
        // Hashed as `Option`s, so the stored `Some("")` "cleared" form stays
        // distinct from `None` ("never edited"), the same discriminant the
        // resolver's reset-to-blueprint contract depends on.
        edit.model.hash(&mut hasher);
        edit.harness.hash(&mut hasher);
        // The provider half of the pair (keys rework slice 3a) moves the same
        // roster binding `model`/`harness` do, so it has to move this
        // fingerprint for the same reason.
        edit.provider.hash(&mut hasher);
    }
    agents.len().hash(&mut hasher);
    for agent in agents {
        agent.id.hash(&mut hasher);
        agent.name.hash(&mut hasher);
        agent.role.hash(&mut hasher);
        agent.description.hash(&mut hasher);
        // Issue #661 / L5: a grant edit changes the roster the harness must
        // build, so it has to move this fingerprint — otherwise a re-grant would
        // persist and be silently ignored until the next process restart, the
        // same staleness the tier/skill fingerprints exist to prevent. Hashed in
        // order (an operator's own list), length folded in first via the slice
        // length above so `["a","b"]` cannot collide with `["ab"]`.
        agent.tools.hash(&mut hasher);
        // The overlay's own routing binding (`overlay_agent_to_manifest` carries
        // both straight through), so a model/harness change on an overlay
        // teammate invalidates the cached roster exactly as an edit of a
        // manifest one does.
        agent.model.hash(&mut hasher);
        agent.harness.hash(&mut hasher);
        agent.provider.hash(&mut hasher);
    }
    hasher.finish()
}

/// A stable hash of the effective `[policy]`, so a console tier change or a
/// manifest `[policy]` edit rebuilds the roster on the company's next `ensure`
/// (issue #562).
///
/// # Why this axis has to exist at all
///
/// `ApprovalPolicy` is constructed in [`build_roster`], **once per roster
/// build** — not once per call. The roster is cached and rebuilt only when one
/// of the fingerprints in the staleness check moves. So without this function a
/// policy change would be written, persisted, and then **silently ignored
/// until the process restarted**: the write route would return `204`, the
/// console would show the new tier, and every agent would keep running the old
/// one. That is the same failure the skill-delta fingerprint above exists to
/// prevent, and it is invisible from the outside.
///
/// # Why it hashes the effective values, not a stored override
///
/// The input is the effective policy — the manifest `[policy]` block as
/// reconciled with any operator override — because the roster is built from
/// that effective view (`CompanyRecord::effective_policy`). A relative-override
/// fingerprint is the empty value whenever effective == manifest, which is
/// exactly the case after a manifest `[policy]` edit that stores no override:
/// the write is persisted, the native gate is re-applied, and yet the cache key
/// never moves — so the next `ensure` reuses the roster (with its old
/// `ApprovalPolicy`) and harness tool calls keep running under the pre-edit
/// tier while the native gate already enforces the new one. Hashing the
/// effective values closes that gap.
///
/// # What is hashed, and what deliberately is not
///
/// - `mode` and `always_approve` are hashed — they are what the gate reads.
/// - `always_approve` is hashed **in order**, unlike the budget set. The order
///   is the operator's own list as they wrote it, not an accumulation of
///   independent rows, so a reorder is a real edit rather than a spurious
///   difference. Its length is folded in first so `["a","b"]` cannot collide
///   with `["ab"]`.
/// - The `Some`/`None` distinction of `auto_approve_under_usd` is hashed, so a
///   cap change moves the key whether it flips between numbers or to/from
///   `None` (the strictest setting).
/// - **The TTL is deliberately NOT hashed**, for the same reason it was excluded
///   from the old override fingerprint: it is enforced by the live gate, not the
///   roster snapshot, so a deadline-only change must not trigger a roster
///   rebuild.
/// - **Attribution is structurally absent from `Policy`**, so re-saving the same
///   tier can never rebuild the roster the way hashing an override's `set_by`
///   would.
fn effective_policy_fingerprint(policy: &Policy) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    policy.mode.hash(&mut hasher);
    policy.always_approve.len().hash(&mut hasher);
    for kind in &policy.always_approve {
        kind.hash(&mut hasher);
    }
    match policy.auto_approve_under_usd {
        Some(amount) => {
            1u8.hash(&mut hasher);
            amount.to_bits().hash(&mut hasher);
        }
        None => 0u8.hash(&mut hasher),
    }
    hasher.finish()
}

/// Synthesizes the override that makes `manifest ⊕ result == policy`.
///
/// `ensure_with_policy` pins the roster's policy axis to the cycle-start
/// snapshot the native gate was re-applied from, and `build_roster` only knows
/// how to read a policy through `CompanyRecord::effective_policy` — so this is
/// the inverse of that merge: for each field, override it iff the snapshot
/// differs from the manifest. The attribution fields are transient (never
/// persisted, and neither `effective_policy_fingerprint` nor `build_roster`
/// reads them), so a synthetic system actor is honest about what they are.
fn policy_override_for(policy: &Policy, manifest: &Policy) -> PolicyOverride {
    PolicyOverride {
        mode: (policy.mode != manifest.mode).then(|| policy.mode.clone()),
        always_approve: (policy.always_approve != manifest.always_approve)
            .then(|| policy.always_approve.clone()),
        auto_approve_under_usd: (policy.auto_approve_under_usd != manifest.auto_approve_under_usd)
            .then_some(policy.auto_approve_under_usd),
        // The TTL is a bare `Option` whose `None` falls through the merge, so
        // the override reproduces a differing value by naming it directly and
        // reproduces an equal one by saying nothing. (Inert on the roster —
        // `ApprovalPolicy` carries no TTL — and absent from the fingerprint,
        // so this arm only keeps the synthesis honest.)
        approval_ttl_hours: if policy.approval_ttl_hours != manifest.approval_ttl_hours {
            policy.approval_ttl_hours
        } else {
            None
        },
        set_by: Actor {
            kind: ActorKind::System,
            id: "harness".to_string(),
        },
        at_millis: 0,
    }
}

/// The live overlay state one roster rebuild is resolved against.
///
/// A struct rather than the tuple this used to be: it grew past the point where
/// positional returns stay readable, and — more to the point — the desk fields
/// were added because desks now decide *capability*, so a caller silently
/// binding `desk_tools` to the `desks` position would hand every teammate the
/// wrong tool belt with nothing to catch it.
pub(crate) struct EffectiveOverlay {
    pub agents: Vec<OverlayAgent>,
    /// The operator's edits of the manifest-declared teammates.
    pub agent_edits: Vec<crate::ports::types::AgentOverride>,
    /// The ids of manifest teammates the operator has removed.
    pub retired: Vec<String>,
    pub budgets: Vec<BudgetOverride>,
    pub policy: Option<PolicyOverride>,
    pub desks: Vec<OverlayDesk>,
    pub desk_members: Vec<OverlayDeskMember>,
    /// The namespaces an operator granted from a connect surface (issue #1796).
    pub tool_grants: Option<crate::ports::types::ToolGrantsOverride>,
    pub desk_tools: std::collections::BTreeMap<String, Vec<String>>,
    /// The company's current display name (issue #1875 review finding),
    /// read live from [`HarnessDeps::store`] the same way every other field
    /// on this struct is — `company: &CompanyRecord` may be a stale
    /// boot-time snapshot, and `PATCH {scope}` (`server::ops::company_profile`)
    /// writes a rename straight into `manifest.company.name` with no overlay
    /// field to fingerprint separately.
    pub company_name: String,
}

/// Fingerprints the routed workspace documents a roster's personas are built
/// from — **over their bodies**, not their names.
///
/// Hashing the content is the whole point. The routing table is manifest data
/// and does not move when an operator edits a note, so a name-only hash would
/// leave the edit invisible: the persona is assembled once per roster, and the
/// fast path would keep serving a prompt quoting the old text until the process
/// restarted. That is precisely the staleness the routing layer exists to avoid.
///
/// Sorted by agent id before hashing, for the reason [`budget_fingerprint`]
/// documents — a `HashMap` has no order, and an order-sensitive hash would drop
/// every live agent session on a rebuild that changed nothing.
fn routed_context_fingerprint(routed: &HashMap<String, Vec<(String, String)>>) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut ordered: Vec<(&String, &Vec<(String, String)>)> = routed.iter().collect();
    ordered.sort_by(|a, b| a.0.cmp(b.0));

    let mut hasher = DefaultHasher::new();
    ordered.len().hash(&mut hasher);
    for (agent_id, documents) in ordered {
        agent_id.hash(&mut hasher);
        documents.len().hash(&mut hasher);
        for (path, body) in documents {
            path.hash(&mut hasher);
            body.hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// Fingerprints the desk scoping a roster's grants are resolved through: which
/// desks exist, who sits on them, and what each one's tool ceiling is.
///
/// All three axes are hashed together because all three feed one answer — an
/// agent's effective grant. Seating a teammate on a restricted desk narrows its
/// belt just as surely as editing that desk's ceiling does, so a fingerprint
/// over the ceilings alone would leave a membership change invisible until the
/// next restart, which is the staleness bug this whole fingerprint set exists to
/// prevent.
///
/// Sorted before hashing, for the reason [`budget_fingerprint`] documents: the
/// write routes push and retain rather than maintain an order, and an
/// order-sensitive hash would drop every live agent session on a save that
/// changed nothing an agent can observe. (`desk_tools` is a `BTreeMap` and so is
/// already ordered by construction.)
fn desk_scope_fingerprint(
    desks: &[OverlayDesk],
    members: &[OverlayDeskMember],
    tools: &std::collections::BTreeMap<String, Vec<String>>,
) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();

    let mut desk_ids: Vec<&str> = desks.iter().map(|desk| desk.id.as_str()).collect();
    desk_ids.sort_unstable();
    desk_ids.hash(&mut hasher);

    let mut seats: Vec<(&str, &str)> = members
        .iter()
        .map(|seat| (seat.desk_id.as_str(), seat.agent_id.as_str()))
        .collect();
    seats.sort_unstable();
    seats.hash(&mut hasher);

    tools.len().hash(&mut hasher);
    for (desk_id, ceiling) in tools {
        desk_id.hash(&mut hasher);
        ceiling.hash(&mut hasher);
    }

    hasher.finish()
}

/// A stable fingerprint of the `[tools].allow` a roster's belts are wired from
/// (issue #1796).
///
/// Order-**sensitive**, unlike its neighbours: `[tools].allow` is an ordered
/// list an operator authored, `effective_tool_allow` appends to it
/// deterministically, and two grant lists that differ only in order are two
/// different manifests. Sorting here would hide a reordering that
/// `allow_covers` can genuinely read differently.
fn tool_grants_fingerprint(allow: &[String]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    allow.hash(&mut hasher);
    hasher.finish()
}

/// A stable fingerprint of a company's operator budget-override set (issue
/// #343), used to detect a cap set / changed / cleared / reset between
/// [`HarnessPool::ensure`] calls. Mirrors [`overlay_fingerprint`]'s shape; a
/// [`BudgetOverride`] holds no secret.
///
/// Two details carry weight:
///
/// - The set is **sorted by `agent_id`** first, because the write routes push
///   and retain rather than maintain an order, and an order-sensitive hash would
///   rebuild the roster (dropping live agent sessions) on a save that changed
///   nothing an agent can observe.
/// - The cap is hashed as an `Option` **discriminant plus `f64::to_bits`**, not
///   through `PartialEq`. `f64` is not `Hash`, and going through bits is also
///   what keeps `Some(0.0)` distinct from `None` in the hash — the very
///   distinction the issue insists must not collapse. `to_bits` additionally
///   makes the hash total over values `PartialEq` would call incomparable.
fn budget_fingerprint(overrides: &[BudgetOverride]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut ordered: Vec<&BudgetOverride> = overrides.iter().collect();
    ordered.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));

    let mut hasher = DefaultHasher::new();
    ordered.len().hash(&mut hasher);
    for entry in ordered {
        entry.agent_id.hash(&mut hasher);
        match entry.budget_usd_daily {
            Some(cap) => {
                1u8.hash(&mut hasher);
                cap.to_bits().hash(&mut hasher);
            }
            None => 0u8.hash(&mut hasher),
        }
    }
    // Attribution is deliberately NOT hashed: who set the cap and when changes
    // nothing an agent can act on, and folding it in would rebuild the roster
    // (discarding live sessions) every time the same value was re-saved.
    hasher.finish()
}

/// A stable fingerprint of a company's operator persona-override set (issue
/// #1530), used to detect a persona set / changed / cleared / reset between
/// [`HarnessPool::ensure`] calls. Mirrors [`budget_fingerprint`]'s shape; an
/// [`AgentOverride`] holds no secret.
///
/// **Sorted by `agent_id`** first, for the reason [`budget_fingerprint`]
/// documents: the write routes push and retain rather than maintain an order, so
/// an order-sensitive hash would rebuild the roster (dropping live agent
/// sessions) on a save that changed nothing an agent can observe. The
/// instructions text is hashed as an `Option` discriminant plus its bytes, so a
/// stored `Some("")` stays distinct from `None` — the same distinction the
/// resolver's reset-to-blueprint contract depends on.
///
/// Avatar-only rows ([`is_avatar_only`]) are excluded here exactly as in
/// [`overlay_fingerprint`]: a face is resolved at render time, never part of the
/// persona the harness builds, so choosing or clearing one must not rebuild the
/// roster.
fn override_fingerprint(overrides: &[AgentOverride]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut ordered: Vec<&AgentOverride> = overrides
        .iter()
        .filter(|entry| !is_avatar_only(entry))
        .collect();
    ordered.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));

    let mut hasher = DefaultHasher::new();
    ordered.len().hash(&mut hasher);
    for entry in ordered {
        entry.agent_id.hash(&mut hasher);
        match &entry.instructions {
            Some(text) => {
                1u8.hash(&mut hasher);
                text.hash(&mut hasher);
            }
            None => 0u8.hash(&mut hasher),
        }
        // A routing override changes the harness binding the roster must build,
        // so it has to move this fingerprint too — a model/harness change on a
        // teammate who already has a persona override would otherwise be ignored
        // until the next process restart (issue #1676 review note). Hashed as
        // `Option`s so the stored `Some("")` "cleared" form stays distinct from
        // `None` ("never edited").
        entry.model.hash(&mut hasher);
        entry.harness.hash(&mut hasher);
        entry.provider.hash(&mut hasher);
    }
    hasher.finish()
}

/// A stable fingerprint of a company's display name (PR #1875 review finding).
///
/// `build_roster` embeds `manifest.company.name` into every agent's persona
/// (`build::build_agent`'s `company_name` argument), and that embedding is
/// assembled once per cached roster, not once per turn — exactly the same
/// staleness shape [`override_fingerprint`] exists to close for a per-agent
/// persona edit. Without this axis, none of the other fingerprints move on a
/// `PATCH {scope}` rename (`server::ops::company_profile::patch_company`), so
/// the fast path in [`HarnessPool::ensure_impl`] keeps serving every agent's
/// old-company-name persona until an unrelated axis happens to change or the
/// process restarts.
fn company_name_fingerprint(name: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    hasher.finish()
}

/// A stable fingerprint of a company's operator skill-delta set (issue #41),
/// used to detect a skill authored / edited / enabled / disabled between
/// [`HarnessPool::ensure`] calls. Mirrors [`mcp_fingerprint`]'s shape.
///
/// The deltas are **sorted by `slug`** before hashing because
/// [`SkillStateStore::list`](crate::ports::skills_state::SkillStateStore::list)
/// gives no ordering contract — an order-sensitive hash would thrash the roster
/// (and drop live agent conversation state) whenever the store returned the
/// same skills in a different row order. The full `custom_doc` body is hashed so
/// an *edited* skill (same slug, new content) also triggers a rebuild. No
/// secrets are involved — a skill delta is operator-authored content.
fn skill_delta_fingerprint(deltas: &[SkillState]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut ordered: Vec<&SkillState> = deltas.iter().collect();
    ordered.sort_by(|a, b| a.slug.cmp(&b.slug));

    let mut hasher = DefaultHasher::new();
    ordered.len().hash(&mut hasher);
    for delta in ordered {
        delta.slug.hash(&mut hasher);
        delta.enabled.hash(&mut hasher);
        delta.source.hash(&mut hasher);
        delta.custom_doc.hash(&mut hasher);
    }
    hasher.finish()
}

/// Fingerprint of a company's bound repositories (issue #245).
///
/// Over `(key, token_fingerprint, branches)`, sorted by key, because those are
/// exactly the three things a rebuild has to notice:
///
/// Build every roster agent for a company: every manifest `[[agent]]`, plus
/// every operator- or orchestrator-added [`OverlayAgent`] (issue #71 — Active
/// Runtime Teammates) that does not collide with a manifest agent id.
///
/// Overlay teammates were presentation-only before this cell (listed in the
/// console Team tab but never addressable); this promotes each one into a real
/// [`CompanyAgent`] with the same shape [`build::build_agent`] gives a manifest
/// agent — a standard (company-wide) tool grant, no cognition tier (the
/// default `chat-v1` model), and never the orchestrator. A manifest agent
/// always wins an id collision: the version-controlled roster is authoritative,
/// and [`orchestrator::orchestrator_id`] only ever looks at `manifest.agents`,
/// so an overlay teammate can never become the orchestrator.
///
/// `skill_deltas` are the company's operator skill overrides (fetched once by
/// the async caller); every agent folds them into its effective skill set.
///
/// `routed_context` maps an agent id to the workspace documents routed into its
/// system prompt, resolved by the async caller for the same reason
/// `skill_deltas` is — this function is synchronous and the `WorkspaceStore` is
/// not. An agent absent from the map gets no routed documents, which is the
/// correct reading for a company with no workspace store wired: fail closed to
/// the pre-routing prompt rather than to a half-populated one.
/// Whether `deps` builds `agent_id`.
///
/// `serves: None` is the whole roster — every pre-harness caller, and the
/// single-harness case that is still the overwhelming majority.
fn serves(deps: &HarnessDeps, agent_id: &str) -> bool {
    match deps.serves.as_ref() {
        None => true,
        Some(ids) => ids.contains(agent_id),
    }
}

/// The tool grants one teammate is scoped to: company-wide, narrowed by the
/// desks it sits on, then by its own declaration.
///
/// Split out of [`build_roster`] so a seat of a running episode resolves the
/// same grants the roster agent of the same name does.
pub(crate) fn grants_for_policy(
    company: &CompanyRecord,
    allow: &[String],
    manifest_agent: &ManifestAgent,
) -> Vec<String> {
    let desk_tools = company.agent_desk_tools(&manifest_agent.id);
    let desk_allows: Vec<&[String]> = desk_tools.iter().map(Vec::as_slice).collect();
    agent_scoped_grants(allow, &desk_allows, manifest_agent.tools.as_deref())
}

/// The MCP `(server, tool)` pairs a teammate's gate lets run without parking.
///
/// Resolved through each server's stored tool policy, so an operator's
/// refusal or approval requirement wins over the manifest declaration. Every
/// teammate policy takes its read set from here, whether it serves the chat
/// roster or an episode seat.
pub(crate) fn agent_mcp_reads(deps: &HarnessDeps) -> crate::policy::McpReadSet {
    crate::company::mcp_policy::mcp_allow_set(&deps.mcp_servers)
}

/// The approval policy one teammate is built with.
///
/// Extracted from [`build_roster`] so an episode seat is gated exactly as
/// the roster agent of the same name is: the same budget, emergency gate,
/// workspace, meter, MCP read set and Composio deflection.
pub(crate) fn agent_policy_for(
    company: &CompanyRecord,
    deps: &HarnessDeps,
    manifest_agent: &ManifestAgent,
    policy: &Policy,
    effective_budget: Option<f64>,
    #[cfg_attr(not(feature = "composio"), allow(unused_variables))] grants: &[String],
) -> ApprovalPolicy {
    let mut agent_policy = ApprovalPolicy::new(policy, effective_budget)
        .with_policy_hitl_disabled()
        .with_requests(deps.approval_requests.clone())
        // Issue #243: stamp who the parked effect belongs to, so approving it
        // can hand the grant back to this agent rather than to nobody.
        .with_agent(manifest_agent.id.clone())
        .with_mcp_reads(agent_mcp_reads(deps));
    if let Some(gate) = deps.emergency_gate.as_ref() {
        agent_policy = agent_policy.with_emergency_gate(gate.clone());
    }
    if let Some(workspace) = deps.workspace.as_ref() {
        agent_policy = agent_policy.with_workspace(workspace.clone(), company.id.clone());
    }
    // Issue #304: give the policy something to measure `budget_usd_daily`
    // against. Only wired when the host has a meter — without one the cap
    // arm stays inert and warns once, rather than parking every priced call
    // on a host that can never answer the question.
    if let Some(meter) = deps.meter.as_ref() {
        agent_policy = agent_policy.with_spend(meter.clone(), company.id.clone());
    }
    agent_policy
}

/// The approval policy an episode seat is built with: the roster agent's
/// policy, resolved from the company's policy and budget in force.
#[cfg(feature = "openhuman")]
pub(crate) fn seat_policy(
    company: &CompanyRecord,
    deps: &HarnessDeps,
    manifest_agent: &ManifestAgent,
    grants: &[String],
) -> ApprovalPolicy {
    agent_policy_for(
        company,
        deps,
        manifest_agent,
        &company.effective_policy(),
        company.effective_budget(&manifest_agent.id),
        grants,
    )
}

/// The standing prompt one teammate carries as a seat of a running episode.
///
/// The persona [`build_roster`] would build for that teammate, less the
/// hand-off tools and their briefs, rendered with OpenHuman's grounding and
/// writing-style blocks so a seeded seat turn reads the prompt a cold turn
/// would have composed.
///
/// # Errors
///
/// [`OpenCompanyError::Config`] when the company seats no teammate by that
/// name, or whatever stops the session being built.
#[cfg(feature = "openhuman")]
pub(crate) fn seat_persona(
    company: &CompanyRecord,
    deps: &HarnessDeps,
    seat: &str,
) -> crate::Result<String> {
    let live_roster = company.effective_agents();
    let manifest_agent = live_roster
        .iter()
        .find(|agent| agent.id == seat)
        .ok_or_else(|| {
            crate::error::OpenCompanyError::Config(format!(
                "hive episode: `{seat}` is seated at the desk but not on the roster"
            ))
        })?;
    let grants = grants_for_policy(company, &company.manifest.tools.allow, manifest_agent);
    let policy = seat_policy(company, deps, manifest_agent, &grants);
    let instructions = company.effective_instructions(&manifest_agent.id);
    let blueprint = build::build_agent_with_model(
        &company.id,
        &company.manifest.company.name,
        manifest_agent,
        Arc::new(policy),
        deps,
        &grants,
        // An episode seat carries no skill deltas and no routed context: it
        // is built for one episode and torn down with it.
        &[],
        &[],
        instructions.as_deref(),
        orchestrator::orchestrator_id(&live_roster).as_deref() == Some(manifest_agent.id.as_str()),
        &crate::company::team_brief::seat_team_section(company, &manifest_agent.id),
    )?;
    // **The hand-off tools come off an episode seat's belt.**
    //
    // `spawn_task`, `delegate_to_desk` and `delegate_to_teammate` are wired
    // onto every roster agent (`build.rs`), and each queues work the
    // [`HarnessBrain`] drains. Inside an episode nothing drains that queue,
    // so the orchestrator refuses the call in the model's own turn rather
    // than parking it forever (`drain_unwired`).
    //
    // The refusal is handled; what it invites is not. A seat that reaches for
    // one concludes delegation is impossible here and reports that to the
    // operator -- "board actions are unavailable, so delegation is blocked",
    // asking them to go and fix a board that was never the problem -- while
    // the hive's own `ask` sat on the same belt the whole time. Offering a
    // tool that cannot work in this context is worse than withholding it: it
    // does not just fail, it argues the seat out of the tool that would have
    // worked.
    //
    // Removed from the belt AND from the provider-visible names, because
    // `episode_seat` builds the allowlist from these and a name the model can
    // see is a name it will reach for.
    let mut blueprint = blueprint;
    blueprint
        .tools
        .retain(|tool| !EPISODE_WITHHELD_TOOLS.contains(&tool.name()));
    blueprint
        .native_tool_names
        .retain(|name| !EPISODE_WITHHELD_TOOLS.contains(&name.as_str()));

    // **And the briefs that describe them.**
    //
    // A seat is built by the same builder as an ordinary roster agent, so it
    // inherits the orchestrator runtime's prose wholesale: how to hand work
    // on, and how the board tracks it. Inside an episode there is no drain
    // and no board, and `tinyhivemind` is the thing running the room -- a
    // seat reaches a teammate with `ask`, which the episode's own belt
    // serves.
    //
    // Taking the tools without the prose is the worst of both: the persona
    // spends a paragraph on `delegate_to_teammate`, the belt does not have
    // it, and a seat that goes looking concludes the capability was
    // withdrawn. On a live run one did exactly that and told the operator to
    // go and make "the board" available -- reporting, accurately, an
    // affordance its prompt had promised and its belt could not honour.
    //
    // Removed by exact match on what was appended, so a brief that is
    // reworded upstream is either removed whole or left whole, never
    // half-cut.
    for brief in [
        orchestrator::orchestrator_brief(),
        orchestrator::member_delegation_brief(),
    ] {
        if let Some(at) = blueprint.system_prompt.find(&brief) {
            blueprint
                .system_prompt
                .replace_range(at..at + brief.len(), "");
        }
    }

    // The belt reaches the pooled agent per turn through `EpisodeBelts`; the
    // standing prompt cannot, because a seeded turn is not cold and composes
    // none. The host puts this at the head of the seed instead -- see
    // `EpisodeHost::persona` -- so it has to be the rendered prompt, not the
    // bare body.
    build::rendered_seat_persona(&blueprint)
}

/// The roster tools an episode seat is **not** built with.
///
/// Every one of these queues work for the [`HarnessBrain`] to drain, and no
/// brain drains inside an episode. See `build_episode_seat` for why they are
/// withheld rather than left to refuse.
pub(crate) const EPISODE_WITHHELD_TOOLS: [&str; 3] =
    ["spawn_task", "delegate_to_desk", "delegate_to_teammate"];

pub(crate) fn build_roster(
    runtime: &openhuman_embed::Runtime,
    company: &CompanyRecord,
    deps: &HarnessDeps,
    skill_deltas: &[SkillState],
    routed_context: &HashMap<String, Vec<(String, String)>>,
) -> crate::Result<Vec<Arc<CompanyAgent>>> {
    // Issue #562: the policy in force, not the one the manifest shipped with —
    // the same relationship `effective_budget` (issue #343) has to the manifest
    // cap, and resolved through the same record so the console and this gate
    // cannot disagree about which tier is live.
    //
    // Owned rather than borrowed because the effective value is a field-wise
    // merge of the override and the manifest, so there may be nothing to borrow.
    let effective = company.effective_policy();
    let policy: &Policy = &effective;
    let company_name = &company.manifest.company.name;
    let allow = &company.manifest.tools.allow;
    // The orchestrator agent (tier `orchestrator`, else the first agent) receives
    // the delegating-orchestrator persona + tools (issue #53).
    // Resolved over the roster as it effectively stands, not the blueprint's:
    // a company whose first declared agent has since been removed still has an
    // orchestrator, and it is the next one — not a teammate that is not built.
    let live_roster = company.effective_agents();
    let orchestrator = orchestrator::orchestrator_id(&live_roster);

    let mut roster =
        Vec::with_capacity(company.manifest.agents.len() + company.overlay_agents.len());

    // The roster as it effectively stands: `company.toml` says who this company
    // was launched with, the operator's stored edits say who each teammate is
    // now, and its tombstones say who is no longer here. The harness builds the
    // second — a teammate an operator removed is not built at all, which is what
    // makes it undispatchable rather than merely hidden from the Team page.
    for manifest_agent in &live_roster {
        // When these deps serve one named harness, build only the agents bound
        // to it — every other agent is another pool's, holding another
        // provider. Skipping here rather than filtering afterwards is what keeps
        // the unbuilt agents from ever standing up a model client.
        if !serves(deps, &manifest_agent.id) {
            continue;
        }
        // Issue #343: the cap in force, not the one the manifest shipped with.
        // `effective_budget` is an operator override when one is stored and the
        // manifest value otherwise, so a console cap change reaches BOTH readers
        // built below — the `ApprovalPolicy` arm and `CompanyAgent`'s copy that
        // the L1 dispatch gate reads — from this one call.
        let effective_budget = company.effective_budget(&manifest_agent.id);
        // Issue #1530: the persona in force, not the one the manifest shipped
        // with. `effective_instructions` is an operator override when one is
        // stored and the manifest `prompt` otherwise, so a console persona edit
        // reaches the system prompt this agent is built with — and it wins over
        // the blueprint without cloning the borrowed `&ManifestAgent`.
        let effective_instructions = company.effective_instructions(&manifest_agent.id);
        let grants = grants_for_policy(company, allow, manifest_agent);
        // `mut` for the Composio arm below, which is the only thing that
        // reassigns it -- and is feature-gated, so a build without that
        // feature would see the binding as needlessly mutable. Same
        // `cfg_attr` the `grants` parameter above carries, for the same
        // reason: one feature owns the mutation and every other build must
        // compile clean under `-D warnings`.
        #[cfg_attr(not(feature = "composio"), allow(unused_mut))]
        let mut agent_policy = agent_policy_for(
            company,
            deps,
            manifest_agent,
            policy,
            effective_budget,
            &grants,
        );
        let is_orchestrator = orchestrator.as_deref() == Some(manifest_agent.id.as_str());
        // Three-level narrowing: company → the desks this teammate sits on →
        // the teammate itself. `agent_desk_tools` resolves through the record's
        // *effective* desk membership, so a console-seated member is scoped by
        // its desk exactly as a manifest one is.
        let desk_tools = company.agent_desk_tools(&manifest_agent.id);
        let desk_allows: Vec<&[String]> = desk_tools.iter().map(Vec::as_slice).collect();
        let grants = agent_scoped_grants(allow, &desk_allows, manifest_agent.tools.as_deref());
        // Issue #1759 (S2): when this agent has Composio wired (an explicit
        // company/agent grant AND a resolved credential with a non-empty toolkit
        // allowlist), install those connected toolkits on its policy so a raw
        // `http_request`/`curl`/`web_fetch` aimed at one of their API hosts is
        // deflected to the Composio route — the same condition S1's
        // `composio_brief` is wired under, one door down (enforcement, not just
        // instruction).
        //
        // `toolbelt::composio_capability_admits` (PR #1780 review) keeps this
        // in lockstep with `build_agent`'s brief gate: `deps.capabilities` is
        // the per-turn tier, and when it has denied `composio` (budget
        // exhausted, or a fail-closed metering error) `filter_by_capabilities`
        // strips every `composio_*` tool from this agent's belt. Installing the
        // deflection anyway would deny the raw web call AND point the agent at
        // a tool it no longer has — a dead end, not defense-in-depth.
        #[cfg(feature = "composio")]
        if crate::company::grants_composio_explicit(&grants)
            && let Some(config) = deps.composio.as_ref()
            && toolbelt::composio_capability_admits(!config.toolkits.is_empty(), &deps.capabilities)
        {
            agent_policy = agent_policy.with_connected_composio_toolkits(config.toolkits.clone());
        }
        let blueprint = build::build_agent_with_model(
            &company.id,
            company_name,
            manifest_agent,
            Arc::new(agent_policy),
            deps,
            &grants,
            skill_deltas,
            routed_context
                .get(&manifest_agent.id)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            effective_instructions.as_deref(),
            is_orchestrator,
            &crate::company::team_brief::team_section(company, &manifest_agent.id),
        )?;
        roster.push(Arc::new(CompanyAgent::register(
            runtime,
            &company.id,
            &manifest_agent.id,
            &manifest_agent.role,
            effective_budget,
            blueprint,
            deps.events.clone(),
        )?));
    }

    // Issue #71 — Active Runtime Teammates (minimal slice): promote every
    // operator/orchestrator-added overlay teammate into a real roster agent
    // too, skipping any id already claimed by a manifest agent.
    let manifest_ids: HashSet<&str> = company
        .manifest
        .agents
        .iter()
        .map(|a| a.id.as_str())
        .collect();
    for overlay in &company.overlay_agents {
        if manifest_ids.contains(overlay.id.as_str()) {
            continue;
        }
        if !serves(deps, &overlay.id) {
            continue;
        }
        let manifest_agent = overlay_agent_to_manifest(overlay);
        // Issue #343: an overlay teammate has no manifest row to carry a cap, so
        // before the override existed it was unconditionally uncapped — the "v1
        // limitation" this lifts. `effective_budget` gives it a stored cap when
        // an operator set one, and `None` (as before) when nobody has.
        let effective_budget = company.effective_budget(&manifest_agent.id);
        // Issue #1530: the same persona resolution as the manifest loop. A bare
        // overlay teammate has no manifest `prompt`, so this is `None` unless an
        // operator set an override for it — and an override wins uniformly, the
        // one reason `overlay_agent_to_manifest` can keep `prompt: None`.
        let effective_instructions = company.effective_instructions(&manifest_agent.id);
        // An overlay teammate is scoped by its desks the same as a manifest one:
        // it can be seated on a desk, and a desk ceiling that applied to only
        // half its members would not be a ceiling.
        let desk_tools = company.agent_desk_tools(&manifest_agent.id);
        let desk_allows: Vec<&[String]> = desk_tools.iter().map(Vec::as_slice).collect();
        let grants = agent_scoped_grants(allow, &desk_allows, manifest_agent.tools.as_deref());
        #[cfg_attr(not(feature = "composio"), allow(unused_mut))]
        let mut agent_policy = agent_policy_for(
            company,
            deps,
            &manifest_agent,
            policy,
            effective_budget,
            &grants,
        );
        // Issue #1759 (S2): same Composio deflection wiring as the manifest loop
        // — an overlay teammate that holds the Composio grant is guarded on the
        // same terms, including the `composio_capability_admits` check (PR
        // #1780 review; see the manifest loop above for why).
        #[cfg(feature = "composio")]
        if crate::company::grants_composio_explicit(&grants)
            && let Some(config) = deps.composio.as_ref()
            && toolbelt::composio_capability_admits(!config.toolkits.is_empty(), &deps.capabilities)
        {
            agent_policy = agent_policy.with_connected_composio_toolkits(config.toolkits.clone());
        }
        let blueprint = build::build_agent_with_model(
            &company.id,
            company_name,
            &manifest_agent,
            Arc::new(agent_policy),
            deps,
            &grants,
            skill_deltas,
            routed_context
                .get(&manifest_agent.id)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            effective_instructions.as_deref(),
            /* is_orchestrator */ false,
            &crate::company::team_brief::team_section(company, &manifest_agent.id),
        )?;
        roster.push(Arc::new(CompanyAgent::register(
            runtime,
            &company.id,
            &manifest_agent.id,
            &manifest_agent.role,
            effective_budget,
            blueprint,
            deps.events.clone(),
        )?));
    }

    Ok(roster)
}

/// Converts an operator-added [`OverlayAgent`] into the manifest agent shape
/// [`build::build_agent`] consumes: an empty `tools` list (so
/// [`agent_effective_grants`] falls back to the full company `[tools].allow`
/// — the "standard tool grant"), no cognition tier (→ the default `chat-v1`
/// model), and no manifest budget cap — an overlay teammate has no manifest row
/// at all, so its cap (if any) comes from the record's budget overrides via
/// [`CompanyRecord::effective_budget`], resolved by the caller. The overlay's
/// `name` is carried across (issue #1105): it is what
/// [`crate::metering::roster_display_names`] labels this teammate with
/// everywhere in the console, so
/// [`persona_prompt`](crate::company::prompt::persona_prompt) needs it to frame the
/// agent as the person the operator is addressing. Dropping it here — as this
/// did until #1105 — left the model knowing only its role, so it denied being
/// the name on its own DM header.
fn overlay_agent_to_manifest(overlay: &OverlayAgent) -> ManifestAgent {
    ManifestAgent {
        // Carried straight through, exactly like `model` below — an overlay
        // teammate's own `{provider, model}` pair (keys rework slice 3a) is
        // resolved the same way a manifest agent's is, once it reaches
        // `TenantProvider::resolve`.
        provider: overlay.provider.clone(),
        global: false,
        id: overlay.id.clone(),
        role: overlay.role.clone(),
        name: Some(overlay.name.clone()),
        description: overlay.description.clone(),
        tier: None,
        // Carried straight through — issue #1245's harness-picker follow-up
        // gave overlay teammates the same `harness` binding a manifest agent
        // has. `None` still means "the default harness", exactly as before
        // this field existed.
        harness: overlay.harness.clone(),
        // Issue #661 / L5: carry the overlay's own per-teammate grant. An empty
        // list here is unchanged behaviour — `agent_effective_grants` reads it as
        // the standard company-wide grant, exactly as the hardcoded empty did.
        // A non-empty list is intersected with `[tools].allow` by that same
        // function below (narrow-only, never a widen).
        tools: overlay.tools.clone(),
        // An overlay teammate declares no delegation allowlist, and an empty
        // list is unrestricted (`delegation_tools::reach_is_unrestricted`): it
        // carries the hand-off tools like every roster agent and may reach
        // anyone. Narrowing an overlay needs a console write surface.
        delegates_to: Vec::new(),
        context: None,
        budget_usd_daily: None,
        // Issue #1530: still `None`, and deliberately so. An overlay teammate's
        // persona is carried by the record's per-agent override (resolved through
        // `effective_instructions` and threaded into `build_agent` by the caller),
        // not by this synthetic `ManifestAgent` — the same shape the budget cap
        // takes, where the override rather than a manifest field is the source.
        prompt: None,
        prompt_files: Vec::new(),
        prompt_files_resolved: Vec::new(),
        classes: Vec::new(),
        ledgers: None,
        can_declare_ledgers: true,
        // Issue #1245's per-agent follow-up: carried straight through, exactly
        // like `tools`/`description` above. Meaningful only when the default
        // harness this teammate lands on (see `harness: None` above) turns out
        // to be an `acp` one — a `built_in` engine simply has no lever that
        // reads it, the same as it ignores `AcpHarness::model`.
        model: overlay.model.clone(),
    }
}

/// A minimal [`HarnessDeps`] for tests that only care about **workflow-tool
/// wiring**: which namespaces a `tool_call` can reach, and why the others cannot.
///
/// Only the inputs [`workflow_tool_wiring`](crate::workflows::caps) actually
/// reads are parameters — the meter and plan (which resolve the capability
/// filter per company and spend) and the static filter itself. `search` is
/// pinned to `None`, because a deployment with no managed search backend is the
/// shape issue #874 is about. Everything else is the cheapest inert default, so
/// a test asserting on wiring does not have to name thirty fields that cannot
/// affect the answer.
///
/// Shared rather than copied: the same fixture backs the runtime-level wiring
/// tests and the `tool-slugs` route test, so both ask about one deployment shape.
#[cfg(test)]
pub(crate) fn workflow_wiring_deps(
    runtime: &crate::CompanyRuntime,
    meter: Option<Arc<dyn UsageMeter>>,
    capabilities: toolbelt::CapabilityFilter,
    plan: Option<capability_budget::CapabilityPlan>,
) -> HarnessDeps {
    HarnessDeps {
        emergency_gate: None,
        provider: Arc::new(provider::MockProvider::default()),
        provider_slug: "mock".to_string(),
        serves: None,
        context: runtime.context.clone(),
        store: runtime.store.clone(),
        notifications: Some(runtime.notifications().clone()),
        ledgers: None,
        ledger_registry: Default::default(),
        meter,
        workspace_root: std::env::temp_dir(),
        mcp_home: None,
        workspace_git_enabled: false,
        audit_root: std::env::temp_dir(),
        model_override: None,
        tasks: None,
        artifacts: None,
        skills: None,
        skills_source_dir: None,
        skills_registry: Arc::from([]),
        mcp_servers: Vec::new(),
        default_mcp_servers: Vec::new(),
        facts: None,
        events: None,
        delegations: orchestrator::DelegationQueue::default(),
        workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
        mcp_failures: mcp_probe::McpFailureQueue::default(),
        pending_publishes: publish::PendingPublishQueue::default(),
        workflow_refs: workflow_refs::WorkflowRefQueue::default(),
        run_outputs: orchestrator::RunOutputCache::default(),
        run_output_store: None,
        workflow_runs: None,
        deep_trace: None,
        workflow_revisions: None,
        approval_requests: policy::ApprovalRequestQueue::default(),
        approval_parker: None,
        secrets: None,
        web_allowed_domains: Vec::new(),
        capabilities,
        workflow_source_dir: None,
        plan,
        media: None,
        composio: None,
        #[cfg(feature = "chargebee")]
        chargebee: None,
        #[cfg(feature = "paypal")]
        paypal: None,
        hosting: None,
        // The staging shape in issue #874: `searchCredentialConfigured: false`.
        search: None,
        tenant_search: None,
        steer: crate::company::steer::InflightRegistry::default(),
        run_supervisor: crate::runtime::RunSupervisor::default(),
        delivery: None,
        workspace: None,
    }
}

#[cfg(test)]
#[path = "built_in_catalogue_brief_tests.rs"]
mod built_in_catalogue_brief_tests;
/// Issue #1840: chat-turn history seeding, first half.
/// `routed_context` fingerprint/resolution coverage.
#[cfg(test)]
#[path = "built_in_routed_context_tests.rs"]
mod built_in_routed_context_tests;
/// Shared fixtures for this module's own inline tests (part 1 of 2). See
/// `built_in_test_fixtures_2` for the rest.
#[cfg(test)]
#[path = "built_in_test_fixtures.rs"]
mod built_in_test_fixtures;
/// Shared fixtures for this module's own inline tests (part 2 of 2).
#[cfg(test)]
#[path = "built_in_test_fixtures_2.rs"]
mod built_in_test_fixtures_2;
/// This module's own inline tests, split into 10 files in original order
/// (see `built_in_tests_part01.rs` for why there is no topical grouping).
#[cfg(test)]
#[path = "built_in_tests_part01.rs"]
mod built_in_tests_part01;
#[cfg(test)]
#[path = "built_in_tests_part02.rs"]
mod built_in_tests_part02;
#[cfg(test)]
#[path = "built_in_tests_part03.rs"]
mod built_in_tests_part03;
#[cfg(test)]
#[path = "built_in_tests_part04.rs"]
mod built_in_tests_part04;
#[cfg(test)]
#[path = "built_in_tests_part05.rs"]
mod built_in_tests_part05;
#[cfg(test)]
#[path = "built_in_tests_part06.rs"]
mod built_in_tests_part06;
#[cfg(test)]
#[path = "built_in_tests_part07.rs"]
mod built_in_tests_part07;
#[cfg(test)]
#[path = "built_in_tests_part08.rs"]
mod built_in_tests_part08;
#[cfg(test)]
#[path = "built_in_tests_part09.rs"]
mod built_in_tests_part09;
#[cfg(test)]
#[path = "built_in_tests_part10.rs"]
mod built_in_tests_part10;
#[cfg(all(test, feature = "openhuman"))]
#[path = "mcp_reads_tests.rs"]
mod mcp_reads_tests;
