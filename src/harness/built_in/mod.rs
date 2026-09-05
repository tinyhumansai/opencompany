//! WS4 — openhuman embedded as a library (the harness).
//!
//! This module supersedes the out-of-process OpenHuman seam
//! (`src/openhuman/{launcher,rpc,tools,channel}.rs`, JSON-RPC behind
//! `openhuman-rpc`) with **direct library embedding** of `vendor/openhuman`
//! (`openhuman_core`): one openhuman [`Agent`](oh::agent::Agent) per manifest
//! `[[agent]]`, wired with memory, an inference provider, an approval policy,
//! and a workspace through [`AgentBuilder`](oh::agent::AgentBuilder).
//!
//! Compiled only under `feature = "openhuman"`. The default build links none of
//! it and keeps its offline, echo-brained behaviour.
//!
//! ## Layout
//!
//! * [`build`] — manifest `[[agent]]` → `AgentBuilder`.
//! * [`provider`] — hosted Medulla [`Provider`] + a `MockProvider` for tests.
//! * [`memory`] — [`OcMemory`](memory::OcMemory): openhuman `Memory` over the
//!   opencompany [`ContextStore`](crate::ports::ContextStore).
//! * [`policy`] — [`ApprovalPolicy`](policy::ApprovalPolicy): `[policy]` →
//!   openhuman `ToolPolicy`.
//! * [`cost`] — [`TurnCost`](oh::agent::cost::TurnCost) → ledger + usage meter.
//!
//! ## Flagged seams
//!
//! * **Group-chat / desk routing** is opencompany's job (openhuman is
//!   single-agent). v1 is single-responder; the full ops `chat` handler that
//!   resolves a desk's members and journals the `AgentReply` is WS3.
//!
//! Live turn cost is **wired**: [`CompanyAgent::run`] reads the completed turn's
//! token/cost totals from openhuman's public
//! [`Agent::last_turn_usage`](oh::agent::Agent::last_turn_usage) accessor and
//! [`HarnessPool::run`] records them through [`cost::record_turn_cost`]. Usage
//! only reaches the ledger/meter when the provider reports it — the
//! [`HostedProvider`](provider::HostedProvider) parses it off the wire; the
//! offline [`MockProvider`](provider::MockProvider) does not, so test turns stay
//! inert.

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
pub mod chat_seed;
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
mod composio_turn_test;
/// Issue #416: the confined turn — an ephemeral agent with no tools, no company
/// memory and no delegation, for a question that is about one object rather than
/// about the company. See [`confine`].
pub mod confine;
pub mod cost;
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
mod iteration_cap_turn_test;
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
mod native_salvage_turn_test;
pub mod orchestrator;
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
pub mod provider;
/// Issue #244: `publish_artifact` — the only way a workspace file becomes a
/// deliverable — plus the staging queue the brain drains, the bounded workspace
/// scan that detects unpublished work, and the follow-up nudge's prompt. See
/// [`publish`].
pub mod publish;
/// End-to-end proof that #244's `publish_artifact` is reachable from a real
/// dispatch, that a re-run extends by identity, and — the part nothing shorter
/// than a real turn loop can show — that the follow-up nudge fires **once**,
/// records a decline, and can never fail the run it follows. Test-only.
#[cfg(test)]
mod publish_turn_test;
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
mod search_turn_test;
/// The per-message responder selection for `auto` channels (issue #1835): the
/// tool-less model call that picks which member of a leadless channel answers
/// an unmentioned message, falling back to the channel's first roster member
/// wherever it cannot run.
pub mod selector;
pub mod skills;
pub mod steer;
pub mod steps;
pub mod title;
pub mod tool_dispatcher;
pub mod toolbelt;
pub mod triage;
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
mod workspace_provision_turn_test;
pub mod workspace_tools;
/// End-to-end proof that the #237 workspace tools are reachable from a real
/// turn, with only the model's choices stubbed. Test-only.
#[cfg(test)]
mod workspace_turn_test;

use crate::harness::run_trace::RunTraceSink;
pub use brain::HarnessBrain;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use openhuman_core::openhuman as oh;
use tokio::sync::{Mutex, RwLock};

use oh::agent::Agent;

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
    /// The inference model shared across a company's agents. A [`HarnessModel`]
    /// is a tinyinference [`ChatModel<()>`](tinyinference::model::ChatModel)
    /// plus the telemetry slug the cost hook reads live per turn; it upcasts to
    /// `Arc<dyn ChatModel<()>>` at the openhuman `AgentBuilder::chat_model` seam.
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
    /// [`composio::TOKEN_KEY`](crate::harness::composio::TOKEN_KEY) if it has one,
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
    /// Set by the runtime builder from
    /// [`search_backend_from_env`](crate::harness::provider::search_backend_from_env)
    /// (env-only — never a tenant secret) with the company's
    /// `[tools].search_daily_calls` cap applied. When `Some` **and** a company
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

/// One live openhuman agent, keyed by its manifest id.
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
    /// The embedded openhuman session. A [`Mutex`] because a `turn` takes
    /// `&mut self` and one agent must serialise its own turns.
    agent: Mutex<Agent>,
    /// The curated step labels of this agent's tools, captured from the built
    /// tool set (see [`StepLabels`](steps::StepLabels) for why the turn loop
    /// cannot supply them).
    ///
    /// Resolved once per agent build rather than per turn: the tool set is fixed
    /// for the life of a pooled agent, and a rebuild — the only thing that can
    /// change which search belt is wired — mints a new `CompanyAgent` anyway.
    step_labels: steps::StepLabels,
    /// The chat/desk thread this pooled agent's in-memory history is currently
    /// bound to (issue #1725).
    ///
    /// One `Agent` instance is reused for every chat of a `(company, agent_id)`
    /// pair, so its `history` would otherwise carry one thread's transcript into
    /// the next — the operator opens a new chat, types "hi", and the agent
    /// replies against the prior task's transcript and goal. Before each turn
    /// the pool compares the incoming `chat_id` to this value; on a switch it
    /// clears the history and re-seeds it from the incoming thread's durable
    /// transcript, so a thread only ever sees its own conversation. Guarded by
    /// the same `agent` critical section (turns are already serialised), so the
    /// pair cannot be read torn. `None` until the first bound turn.
    ///
    /// **The channel is not the finest conversation there is** (#1890). The
    /// binding is `(chat id, thread root)`, because two threads of one channel
    /// are two conversations: keyed on the channel alone, moving between them
    /// was not a switch, so the clear-and-re-seed never ran and one thread
    /// answered with the other's turns still loaded. A `None` root is the
    /// channel-level conversation and needs no special case — it is simply the
    /// thread every unparented line hangs in, which is every line in a company
    /// that has never threaded.
    bound_chat: Mutex<Option<(String, Option<EventSeq>)>>,
}

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
    /// [`Agent::last_turn_hit_cap`](oh::agent::Agent::last_turn_hit_cap) while
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
    /// (`oh::inference::provider::is_budget_exhausted_message`). `None` on
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
    /// The usage is read from each just-completed turn via openhuman's public
    /// [`Agent::last_turn_usage`](oh::agent::Agent::last_turn_usage) accessor
    /// while the agent lock is still held. An offline provider that reports no
    /// usage yields a zero [`TurnUsage`], which the cost hook treats as inert.
    ///
    /// **Activity-trace**: this is the one site holding `&mut Agent`, so it is
    /// where the turn's [`AgentProgress`](oh::agent::progress::AgentProgress)
    /// stream is captured. A per-turn `mpsc` channel is attached via
    /// [`Agent::set_on_progress`](oh::agent::Agent::set_on_progress); an
    /// always-draining collector task buffers every event so the turn loop never
    /// blocks on a full channel; and after the turn (both attempts share the one
    /// channel) the sink is detached, the collector joined, and the events folded
    /// into the scrubbed [`TurnOutcome::steps`] by
    /// [`steps::fold_steps`](crate::harness::steps::fold_steps). The sink is
    /// per-turn *local* — deliberately not a [`HarnessDeps`] field — so parallel
    /// turns never collide.
    pub async fn run(&self, message: &str) -> (crate::Result<TurnOutcome>, Vec<TurnUsage>) {
        self.run_with_steer(
            message,
            None,
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
    /// next iteration boundary. The control is `Box::pin`ned at the task-local
    /// scope boundary to avoid the nested-scope stack-overflow trap.
    ///
    /// When a steer is pending after the first attempt yields the transient
    /// empty-response class, the one-shot retry is **skipped** — a cancel (or
    /// pause) issued before any text is produced must not silently restart the
    /// work. With no steer this is byte-identical to the pre-#111 `run`.
    ///
    /// When `run_sink` is `Some`, the same collector also writes each step
    /// through to the [`RunStore`](crate::ports::RunStore) as it arrives, so a
    /// dispatched card's trace is durable *during* the run rather than only
    /// after it (issue #242). The await lives in the collector task, never in
    /// the model loop, so a slow store slows only trace persistence. `None`
    /// (chat turns, workflow nodes, every test) is byte-identical to the prior
    /// buffer-only behaviour.
    pub async fn run_with_steer(
        &self,
        message: &str,
        steer: Option<&SteerControl>,
        stream: Option<crate::turn_stream::TurnStreamCtx>,
        run_sink: Option<Arc<run_trace::RunTraceSink>>,
        chat_seed: Option<chat_seed::ChatSeedRequest>,
        // The conversation this turn belongs to (#1890), carried in its own
        // right rather than read off `chat_seed` or off `stream`.
        //
        // **Neither half may be inferred.** The root cannot come from
        // `chat_seed`: that is `None` whenever no `EventLog` is wired, so
        // reading it there made two threads of one channel compare equal on
        // such a host — no clear, no re-seed, and the leak this epic exists to
        // close, reopened in exactly the configuration that cannot re-seed its
        // way out of it (coderabbit review on #1896). And the channel cannot
        // come from `stream`, which is what #1890 I fixes: a turn that has a
        // conversation but publishes no live frames — an approval's re-issued
        // call — was indistinguishable from a turn that has none, so it ran
        // against whatever history was last loaded and then answered into a
        // thread it had never been bound to.
        //
        // One [`ChatTarget`] rather than two loose `Option`s, for the reason
        // that type documents: a mis-paired channel and root compiles and then
        // answers into the wrong conversation.
        chat: crate::runtime::delegation::ChatTarget<'_>,
    ) -> (crate::Result<TurnOutcome>, Vec<TurnUsage>) {
        // Per-turn progress sink + an always-draining collector, so a burst of
        // events never blocks the turn loop on a full channel.
        //
        // When `stream` is `Some`, the collector *tees* each event live onto the
        // transient [`turn_stream`](crate::turn_stream) bus as it arrives —
        // mirroring OpenHuman's `spawn_progress_bridge` — so the console renders
        // the tool timeline while the turn is still running. The same events are
        // still buffered and folded into the durable `TurnStep`s below, so the
        // live view and the final reply timeline are byte-identical. With `None`
        // (background turns, non-`openhuman` build) this is exactly the prior
        // buffer-only behaviour.
        // The chat/desk thread this turn answers, captured before `stream` is
        // moved into the collector task below — used for per-conversation history
        // isolation (issue #1725). `None` for a background turn that streams
        // nothing (a dispatched task card carries no operator chat to bind to),
        // and also `None` for a workflow agent node (issue #1702) — it routes by
        // run/node, not a chat thread, so there is nothing to bind history to.
        // Issue #1890 I: the live route when there is one, the caller's `chat`
        // otherwise.
        //
        // Streaming is about where transient frames are published; identity is
        // about which conversation's history this turn may see. Deriving the
        // second from the first made every *unstreamed* turn identity-less —
        // which is the bug, since an approval's re-issued call streams nothing
        // and still belongs to the conversation it was raised in.
        //
        // **The stream still wins when present**, and that is not laziness: the
        // turn-stream route has already folded an unaddressed message onto
        // `DEFAULT_DESK` (`mod.rs`'s chat route), so reading `chat.chat_id`
        // there would hand back `None` and unbind a turn that today binds to
        // General — the exact behaviour
        // `an_unaddressed_message_still_binds_to_its_thread` exists to pin, and
        // which #1896's review already established is correct rather than a
        // gap. So this is a strict extension: nothing that streams changes.
        //
        // Residue, stated rather than discovered: an approval raised in an
        // *unaddressed* message still has `origin_thread: None`, so its
        // re-issued call binds to nothing exactly as before. Closing that means
        // teaching `ChatTarget` to tell "unaddressed" from "no conversation at
        // all", which is a wider change than this one.
        let turn_chat_id: Option<String> = stream
            .as_ref()
            .and_then(|ctx| match &ctx.route {
                crate::turn_stream::LiveRoute::Chat { chat_id } => Some(chat_id.clone()),
                crate::turn_stream::LiveRoute::Workflow { .. } => None,
            })
            .or_else(|| chat.chat_id.map(str::to_string));
        let thread_root = chat.thread_root;
        // The company this turn's chat seed (if any) projects from — same
        // "captured before `stream` moves" reasoning as `turn_chat_id` above.
        // Only meaningful alongside `turn_chat_id`, so `None` for exactly the
        // same turns.
        let turn_company: Option<CompanyId> = stream.as_ref().map(|ctx| ctx.company.clone());
        let (tx, mut rx) = tokio::sync::mpsc::channel::<oh::agent::progress::AgentProgress>(1024);
        // This agent's curated step labels, restored onto each tool-call start as
        // it arrives. The turn loop labels a tool row from its *name* alone and
        // never asks the tool what it calls itself, so a branded belt would
        // otherwise render as the generic humanized name on every surface below
        // (see `steps::StepLabels`). Applied here — once, at the single point the
        // turn's events enter OpenCompany — so the live stream, the durable run
        // trace, and the folded timeline cannot disagree about a step's name.
        let step_labels = self.step_labels.clone();
        let collector = tokio::spawn(async move {
            let mut events = Vec::new();
            let mut seq: u64 = 0;
            // Mirrors `fold_steps`' thinking-run coalescing so the live timeline
            // emits the same "Thinking" rows the final folded one does.
            let mut thinking_open = false;
            while let Some(event) = rx.recv().await {
                let event = step_labels.apply(event);
                if let Some(ctx) = &stream
                    && let Some(frame) = steps::stream_event_from(&event, seq, &mut thinking_open)
                {
                    let frame = frame.with_agent(ctx.agent_id.clone());
                    // Route by the turn's surface: a chat turn stamps `chatId`;
                    // a workflow agent node stamps `workflowRunId`/`nodeId`
                    // instead, since it has no chat thread and the console's
                    // run-trace sheet keys the live timeline on the run (#1702).
                    let frame = match &ctx.route {
                        crate::turn_stream::LiveRoute::Chat { chat_id } => {
                            frame.with_chat(chat_id.clone())
                        }
                        crate::turn_stream::LiveRoute::Workflow { run_id, node_id } => {
                            frame.with_workflow(run_id.clone(), node_id.clone())
                        }
                    };
                    // Which *query* inside that thread, so a console holding two
                    // in-flight turns on one thread keeps their rows apart.
                    // Absent on a turn answering no journaled message, where the
                    // console falls back to keying by thread alone.
                    let frame = frame.with_message_seq(ctx.message_seq);
                    crate::turn_stream::publish(&ctx.company, frame);
                    seq += 1;
                }
                // Durable half (#242): persist the step before moving on, so a
                // process killed mid-run keeps every step written so far.
                if let Some(sink) = &run_sink {
                    sink.record(&event).await;
                }
                events.push(event);
            }
            // A turn that *ends* mid-thought has no closing `TextDelta` or tool
            // call, so the reasoning tail below the interim flush threshold
            // would otherwise sit in the trace unpersisted — exactly the
            // failed/interrupted turns worth diagnosing. Flush on drain.
            if let Some(sink) = &run_sink {
                sink.flush().await;
            }
            events
        });

        let mut agent = self.agent.lock().await;
        agent.set_on_progress(Some(tx));

        // Per-turn overrides for this turn (issue #1725), built once and applied
        // in a single `set_next_turn_overrides` call so the chat-binding and the
        // chat-only reductions cannot clobber each other.
        let mut overrides = oh::agent::harness::session::TurnOverrides::default();

        // Per-conversation history isolation. One `Agent` is reused for every
        // chat of this `(company, agent_id)` pair, so its in-memory `history`
        // would otherwise replay a prior thread's transcript into an unrelated
        // one — the operator opens a new chat, types "hi", and the reply is
        // grounded in the previous task. Bind the agent to the incoming chat
        // thread: on a switch, clear the history and re-seed from THAT thread's
        // own durable transcript. Runs inside the `agent` critical section,
        // which already serialises this agent's turns.
        if let Some(incoming) = turn_chat_id.as_deref() {
            let incoming_root = thread_root;
            let mut bound = self.bound_chat.lock().await;
            let switched = bound.as_ref().map(|(chat, root)| (chat.as_str(), *root))
                != Some((incoming, incoming_root));
            if switched {
                tracing::debug!(
                    from = bound.as_ref().map(|(chat, _)| chat.as_str()).unwrap_or("<none>"),
                    from_thread = ?bound.as_ref().and_then(|(_, root)| *root),
                    to = incoming,
                    to_thread = ?incoming_root,
                    "[harness] chat switched — resetting agent history and re-binding to the incoming thread"
                );
                agent.clear_history();
                // Prefer OpenCompany's own EventLog-derived seed (issue #1840).
                // OpenHuman never writes a file transcript for an OC `chat_id`, so
                // `seed_resume_from_thread_transcript` always misses and the reply
                // starts blind (the #1725/#1730 regression). Project it HERE, now
                // that `switched` is confirmed true — not by the caller for every
                // turn — because the projection walks the company journal and is
                // costly on the filesystem backend (`chat_seed::build_chat_seed`'s
                // docs); building it unconditionally meant every ordinary
                // same-desk reply paid for a journal scan its `switched == false`
                // branch below would just throw away (codex review finding). This
                // still runs inside the same `bound_chat`-locked section as the
                // switch decision, so it is exactly as atomic as the eager build
                // was — no turn can observe a `switched` verdict this projection
                // doesn't match.
                let seed = match (&chat_seed, turn_company.as_ref()) {
                    // `self.agent_id` is the viewer the seed is attributed
                    // against (issue #1956): this agent's own prior replies stay
                    // assistant turns, and every teammate's — plus the runtime's
                    // own notices — arrive as labelled user turns instead of
                    // collapsing into its first person.
                    (Some(request), Some(company)) => {
                        request.build(company, incoming, &self.agent_id).await
                    }
                    _ => Vec::new(),
                };
                tracing::debug!(
                    chat = incoming,
                    seeded = seed.len(),
                    "[harness] built recent-chat seed for the incoming desk"
                );
                // Fall back to the transcript lookup only when the seed is empty
                // (a background/workflow turn, no `chat_seed` request, or a desk
                // with no recent history).
                let seeded = if seed.is_empty() {
                    agent.seed_resume_from_thread_transcript(incoming)
                } else {
                    // `message` is the augmented turn text; `seed_resume_from_messages`
                    // drops a trailing user line matching it. `ChatSeedRequest::build`
                    // (above) already stripped the raw duplicate against
                    // `raw_message` (see `chat_seed::strip_current_message`), so
                    // this is a defensive no-op on the happy path and correct if
                    // augmentation was off.
                    match agent.seed_resume_from_messages(seed, message) {
                        Ok(()) => true,
                        Err(error) => {
                            tracing::warn!(
                                chat = incoming,
                                %error,
                                "[harness] chat-seed resume failed; turn starts without recent history"
                            );
                            false
                        }
                    }
                };
                // On a switch the agent-latest transcript is the WRONG thread, so
                // never let the turn's fallback auto-resume run: our explicit
                // correct-thread seed (or a transcript hit) has already set
                // `cached_transcript_messages`; a miss must start fresh, NOT reload
                // the previous chat's transcript and re-leak it (the exact
                // screenshot bug). Keep this true regardless of which seed path ran.
                overrides.suppress_transcript_autoload = true;
                tracing::debug!(
                    chat = incoming,
                    seeded,
                    "[harness] thread-transcript re-seed result"
                );
                *bound = Some((incoming.to_string(), incoming_root));
            }
        } else {
            // Unthreaded turn (a dispatched background task or a workflow
            // agent node): it still runs against this agent's shared,
            // in-memory `history` — the same field a chat turn reads and
            // extends — but carries no chat thread to bind that history to.
            // Left alone, `bound_chat` keeps pointing at whichever chat was
            // bound before this turn ran, so if the operator's next message
            // lands on that same thread, `switched` above reads `false` and
            // skips the clear-and-reseed entirely, silently grounding the
            // reply in whatever this background turn just appended (the
            // cross-context leak review found). Invalidate the binding so
            // the next chat-routed turn is *always* treated as a switch,
            // regardless of which thread it lands on.
            //
            // Deliberately does NOT clear `history` here: a single
            // background task can span several unthreaded turns in a row
            // (e.g. a steered continuation), and those legitimately depend
            // on the history accumulated between them. The clear already
            // happens on the switch branch above, the next time a chat turn
            // actually claims the binding.
            let mut bound = self.bound_chat.lock().await;
            if bound.is_some() {
                tracing::debug!(
                    "[harness] unthreaded turn — invalidating chat binding so the next chat turn rebinds"
                );
                *bound = None;
            }
        }

        // Reduced-scope chat turn. When the delegation runner marked this turn
        // chat-only (an explicit "Just chatting" or a high-confidence greeting —
        // see `delegation::with_chat_only_hint`), run it as a cheap conversational
        // reply: no tools to loop on, no pre-turn memory retrieval, and no prior
        // task's thread goal re-injected.
        if crate::runtime::delegation::is_chat_only_turn() {
            tracing::debug!(
                "[harness] chat-only turn — tool-less, memory-less, goal-less (fast path, #1725)"
            );
            overrides.suppress_active_goal = true;
            overrides.suppress_tools = true;
            overrides.suppress_memory_agent = true;
        }

        // One-shot: openhuman resets it after this turn, so the next real turn
        // gets its full agentic scope and normal transcript resume back.
        if overrides != oh::agent::harness::session::TurnOverrides::default() {
            agent.set_next_turn_overrides(overrides);
        }

        // Two hooks, both fired by openhuman between tool-loop iterations:
        //
        // * the **steer** hook, only when an operator control is provided (#111);
        // * the **budget** hook, only when this teammate declares a
        //   `budget_usd_daily` cap (#988) — the in-turn spend brake. A teammate
        //   with no declared budget gets no hook, which matches the vendored
        //   runtime's own posture: openhuman constructs `BudgetStopHook` nowhere
        //   and explicitly "never hard-stops a user-present turn that isn't
        //   actively burning a live budget". A turn that never outruns a real
        //   budget has nothing to protect it from, and a blanket magic number no
        //   operator can see or change would be worse than none.
        //
        // A budget halt and an iteration-cap pause are **different outcomes**, not
        // two spellings of one: openhuman reports the cap through
        // `Agent::last_turn_hit_cap`, which stays `false` for a hook-driven stop
        // (the run paused below `max_tool_iterations`, so its cap predicate does
        // not hold). Part 1 of #926 makes the cap pause operator-visible; it must
        // not inherit budget halts.
        let mut hooks: Vec<Arc<dyn oh::agent::stop_hooks::StopHook>> = Vec::new();
        if let Some(control) = steer {
            hooks.push(Arc::new(crate::harness::steer::SteerStopHook::new(
                control.clone(),
            )));
        }
        // Issue #1032: the budget hook is *wrapped* rather than pushed bare, so
        // the halt survives the boundary. Upstream's `StopDecision::Stop` is
        // consumed inside openhuman's tool loop, which returns the run's text as
        // an ordinary `Ok(reply)`; `with_stop_hooks` hands back only the
        // future's value; and `last_turn_hit_cap()` is `false` here by design.
        // Without the wrapper there is nothing left to read, and a turn stopped
        // for spend is indistinguishable from one that finished.
        //
        // The predicate itself stays upstream's — the wrapper only observes it.
        let mut spend_brake: Option<(f64, Arc<std::sync::atomic::AtomicBool>)> = None;
        if let Some(cap) = self.turn_spend_cap_usd() {
            let hook = crate::harness::spend::SpendStopHook::new(cap);
            // Taken before the hook is boxed into the task-local list; once it
            // is an `Arc<dyn StopHook>` the concrete type is unreachable.
            spend_brake = Some((cap, hook.halted()));
            hooks.push(Arc::new(hook));
        }

        // Issue #1846: set from inside the async block below (on either
        // attempt) when `classify_turn` recognises the top-level budget-paused
        // wire shape, and read back out after `with_stop_hooks` returns — the
        // same "flag on a `Mutex`, set inside the turn body, read after the
        // `.await`" idiom `spend_brake`'s `AtomicBool` uses just above,
        // because the async block borrows rather than moves (it is `async {}`,
        // not `async move {}`) so a plain local outlives it.
        let budget_pause_summary: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

        // `Box::pin` at the task-local scope boundary (the nested-scope
        // stack-overflow trap). The turn body owns the retry classification and
        // reports every attempt's usage.
        let (reply, mut usages): (crate::Result<String>, Vec<TurnUsage>) =
            oh::agent::stop_hooks::with_stop_hooks(
                hooks,
                Box::pin(async {
                    let mut usages: Vec<TurnUsage> = Vec::new();
                    // CodeRabbit review (PR #2053): `agent` is the ONE `Agent`
                    // this pool reuses for every chat of this `(company,
                    // agent_id)` pair (see `CompanyAgent::agent`'s doc), and
                    // openhuman's `last_turn_usage_totals` is set only when a
                    // turn finalizes normally — an attempt that ends in
                    // `EmptyProviderResponse` returns before that write, so
                    // `read_turn_usage` reads back whatever the PREVIOUS
                    // finalized turn left there, not this attempt's (zero) own.
                    // Left unguarded, that stale figure would ride home in
                    // `usages` as if this attempt had spent it — for the
                    // one-shot retry that is the previous ATTEMPT's total
                    // double-counted; across two separate calls to this method
                    // on the same reused agent, it is an unrelated PAST TURN's
                    // total billed a second time onto a turn that made no
                    // metered call at all.
                    //
                    // Codex review (PR #2053): an earlier version of this fix
                    // compared each read against the value seen before the
                    // attempt and treated an unchanged read as zero — which
                    // wrongly zeroed a genuinely NEW finalized total on the
                    // rare turn whose real spend happened to numerically equal
                    // the immediately preceding one. `agent.turn()`'s own
                    // `Result` already says, unambiguously, whether THIS
                    // attempt finalized: `Ok` only ever returns non-empty text
                    // (a blank `Ok` is retried inside openhuman's own loop
                    // before it can reach here — see the `Empty` arm below),
                    // and finalizing `last_turn_usage_totals` is part of what
                    // makes a turn return `Ok` at all. So trust `read_turn_usage`
                    // outright on `Ok`, regardless of its value, and never trust
                    // it on `Err` (no comparison needed there either — an
                    // `Err` never finalizes, so any read after one is
                    // necessarily either `None`'s zero or a stale carry-over,
                    // and either way is not this attempt's own). The fix does
                    // not reset the field itself — openhuman does not expose a
                    // way to from here (`take_last_turn_usage_totals` is
                    // `pub(crate)` to that crate) — it reads the outcome
                    // instead. See `last_observed_turn_cost` just below for the
                    // fallback that still recovers a genuinely spent-and-failed
                    // attempt's tokens from its own progress-stream segment.
                    //
                    // Issue #1680: timed PER ATTEMPT, not across the retry. Each
                    // `agent.turn` opens a fresh harness run with a fresh
                    // wall-clock budget, so a duration spanning both attempts
                    // would be compared against a ceiling neither of them saw.
                    // This is the only per-turn duration measured anywhere —
                    // `WorkflowRunNodeRow::elapsed_ms` is per NODE, and a node
                    // is not a turn.
                    let started = std::time::Instant::now();
                    let first = agent.turn(message).await;
                    let first_elapsed = started.elapsed();
                    let first_finalized = first.is_ok();
                    usages.push(if first_finalized {
                        read_turn_usage(&agent)
                    } else {
                        TurnUsage::default()
                    });
                    let reply: crate::Result<String> = match self
                        .classify_turn(first, first_elapsed)
                    {
                        AttemptOutcome::Reply(reply) => Ok(reply),
                        AttemptOutcome::Hard(err) => Err(err),
                        // Issue #1846: terminal, like `Hard`, but graceful —
                        // ends the turn with the actionable copy as an `Ok`
                        // reply rather than propagating an `Err`, and is never
                        // retried (retrying hits the identical wall). Recorded
                        // into `budget_pause_summary` so the caller can build
                        // `TurnOutcome::budget_paused` once this future
                        // resolves — the same reason `spend_brake` exists.
                        //
                        // Issue #1846 review (Codex #3869193105): `summary`
                        // embeds the provider's raw error chain
                        // (`budget_paused_summary`'s `{err:#}`), which a
                        // BYO/custom provider can return with a
                        // credential-bearing URL or an echoed secret baked in.
                        // Redact HERE, once, before the value is stored
                        // anywhere — not just on the copy returned as the
                        // authored reply — because `budget_pause_summary`'s
                        // slot is what `TurnOutcome::budget_paused.summary`
                        // carries onward into `BudgetPause`/`BudgetPauseMarker`,
                        // which is durable and operator-visible (the parked
                        // marker, the chat notice text via
                        // `budget_pause_notice`, and a dispatched card's
                        // settle note all read it straight through). A raw
                        // copy in the mutex slot would have leaked the secret
                        // to every one of those sinks even though the reply
                        // itself was clean.
                        //
                        // `redact`, not the full `scrub` pipeline: `scrub`
                        // additionally hard-truncates to
                        // `mcp_probe::SCRUB_MAX_BYTES` (300 bytes), which is
                        // the right ceiling for a transient reply bubble but
                        // is shorter than `budget_paused_summary`'s own
                        // `truncate_for_pause` cap (600 chars) already applied
                        // to the error detail — stacking `scrub`'s cap on top
                        // would silently chop the persisted marker/notice text
                        // well short of the length `budget_paused_summary`
                        // deliberately allows. `redact` does the same
                        // secret-substring and URL-query stripping without the
                        // second, shorter truncation.
                        AttemptOutcome::BudgetPaused { summary } => {
                            let redacted = crate::harness::mcp_probe::redact(&summary, &[]);
                            if let Ok(mut slot) = budget_pause_summary.lock() {
                                *slot = Some(redacted.clone());
                            }
                            // The reply keeps the FULL `scrub` pipeline
                            // (redact + the shorter 300-byte cap), unchanged
                            // from before this fix — a chat bubble was always
                            // meant to be terse.
                            Ok(crate::harness::mcp_probe::scrub(&redacted, &[]))
                        }
                        AttemptOutcome::Empty => {
                            // Retry-guard edge: skip the one-shot retry when an
                            // operator steer already pends, so a cancel/pause
                            // before any text can't restart the work.
                            //
                            // Issue #1032 adds the second guard, on the same
                            // reasoning: the work was stopped on purpose, and an
                            // empty reply is not licence to restart it. The
                            // retry is a fresh `agent.turn`, so openhuman builds
                            // it a fresh `TurnCost` — the brake's accumulator
                            // starts back at zero, and a teammate that had just
                            // exhausted its cap could spend up to a whole cap
                            // again before the hook fired a second time. The
                            // brake is armed per turn, so nothing else here
                            // would stop it.
                            //
                            // **Defence in depth, not a fix to an observed bug,
                            // and the difference is recorded so nobody re-derives
                            // it.** The `Empty` arm appears to be unreachable
                            // after a halt: a halt implies at least one completed
                            // tool iteration, and openhuman answers the post-halt
                            // wrap-up with its own synthesised "here's what I did
                            // this turn" summary — which it substitutes even when
                            // the wrap-up call returns blank text OR no choices
                            // at all. Both were scripted against the real turn
                            // loop and neither reached this arm, so there is no
                            // test here that would fail without this guard, and
                            // one was deliberately not left behind pretending
                            // otherwise. What the guard buys is that the
                            // invariant stops depending on that substitution
                            // staying true across a vendored bump.
                            //
                            // `halted_for_spend` below still reports the halt
                            // either way, so the operator gets the notice that
                            // explains a stub reply rather than silence.
                            let spend_halted = spend_brake.as_ref().is_some_and(|(_, halted)| {
                                halted.load(std::sync::atomic::Ordering::SeqCst)
                            });
                            if steer.map(|c| c.requested()).unwrap_or(false) || spend_halted {
                                Ok(crate::harness::mcp_probe::scrub(GRACEFUL_EMPTY_REPLY, &[]))
                            } else {
                                // Issue #1725 review: `set_next_turn_overrides`
                                // is one-shot — openhuman consumes it at the
                                // top of the NEXT `Agent::turn` call and resets
                                // to the default (`turn/core.rs`'s
                                // `std::mem::take`). Applied only once, above,
                                // a chat-only turn's suppression would cover
                                // just the first attempt: were this retry ever
                                // reached with the override already spent, it
                                // would run with the agent's full,
                                // un-suppressed scope — regaining the whole
                                // tool belt, memory agent and active goal the
                                // fast path exists to withhold. Reapply the
                                // SAME overrides so every attempt in a
                                // chat-only turn stays reduced, not just the
                                // first.
                                //
                                // Defence in depth, like the guard above it:
                                // an immediately-blank completion with no tool
                                // call is retried INSIDE openhuman's own tool
                                // loop under the SAME per-turn overrides
                                // (verified by instrumenting a scripted blank
                                // response — `first` came back
                                // `Ok("...")` directly, never reaching this
                                // arm at all), so this specific line is not
                                // known to fire from any script this suite can
                                // build. What it buys is that IF this arm ever
                                // is reached — a terminal `EmptyProviderResponse`
                                // openhuman raises after exhausting its own
                                // internal budget — the retry does not silently
                                // regress to full scope.
                                if overrides
                                    != oh::agent::harness::session::TurnOverrides::default()
                                {
                                    agent.set_next_turn_overrides(overrides);
                                }
                                let retry_started = std::time::Instant::now();
                                let second = agent.turn(message).await;
                                let second_elapsed = retry_started.elapsed();
                                // Same outcome-trusts-the-read rule as the first
                                // attempt above: only `Ok` means openhuman
                                // actually finalized a fresh total for THIS
                                // attempt, so only `Ok` earns trusting
                                // `read_turn_usage` — regardless of what value
                                // it reads back.
                                let second_finalized = second.is_ok();
                                usages.push(if second_finalized {
                                    read_turn_usage(&agent)
                                } else {
                                    TurnUsage::default()
                                });
                                match self.classify_turn(second, second_elapsed) {
                                    AttemptOutcome::Reply(reply) => Ok(reply),
                                    AttemptOutcome::Empty => Ok(crate::harness::mcp_probe::scrub(
                                        GRACEFUL_EMPTY_REPLY,
                                        &[],
                                    )),
                                    // Issue #1846: same terminal-not-retryable
                                    // handling as the first attempt's arm above
                                    // — this IS the retry, so there is no
                                    // further attempt to skip.
                                    //
                                    // Issue #1846 review (Codex #3869193105):
                                    // redacted before it reaches the mutex
                                    // slot, same as the first attempt's arm —
                                    // see that arm's doc comment for why this
                                    // is `redact`, not the shorter-truncating
                                    // `scrub`.
                                    AttemptOutcome::BudgetPaused { summary } => {
                                        let redacted =
                                            crate::harness::mcp_probe::redact(&summary, &[]);
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
            )
            .await;

        // Detach the sink (drops the only remaining `Sender`, closing the
        // channel), release the agent lock, then drain + fold. A `Hard` error
        // still runs this cleanup before propagating, so the collector never
        // leaks.
        agent.set_on_progress(None);
        // Issue #926: read the cap flag while the lock is still held, the same
        // under-lock idiom `read_turn_usage` uses above. Not draining, so the
        // retry path's second attempt simply overwrites the first's value —
        // which is right: the outcome describes the attempt that produced the
        // reply being returned.
        let hit_iteration_cap = agent.last_turn_hit_cap();
        drop(agent);
        let events = collector.await.unwrap_or_default();
        // A hard-failed ATTEMPT's spend, recovered from the progress stream —
        // per attempt, not only when every attempt reported nothing.
        //
        // `read_turn_usage` above reads openhuman's `last_turn_usage_totals`,
        // and `run_single` sets that only AFTER its own `let outcome = outcome?`
        // — so an attempt that ended in an error publishes nothing at all, and
        // `read_turn_usage` pushed a zero for it. That is precisely backwards
        // for the attempts worth accounting for: a wall-clock ceiling fires
        // *because* the agent did ten minutes of real work, and the run a
        // founder most needs the cost of was the one reported as free.
        //
        // The live tally openhuman publishes as it goes — `TurnCostUpdated`,
        // cumulative across ONE `agent.turn`, emitted after each provider
        // response that carried a usage block — survives the error, because
        // those frames were already sent down this shared channel before the
        // attempt failed.
        //
        // Codex review (PR #2053): the original gate only fired when EVERY
        // attempt was zero, which recovers at most one attempt — a metered
        // first attempt that empties, followed by a retry that succeeds and
        // publishes its OWN authoritative (small) total, left `usages` as
        // `[zero, retry_total]`. That is not all-zero, so the first attempt's
        // already-published spend was silently dropped rather than merely
        // under-reported. Segmenting `events` on `TurnStarted` — emitted
        // exactly once at the top of each `agent.turn()` call
        // (`core_turn.rs`), never for a delegated sub-agent's turn, which
        // uses `SubagentIterationStarted`/`SubagentToolCallStarted` instead —
        // gives each attempt its own contiguous slice of the stream, so each
        // zeroed attempt recovers its OWN tally independently.
        //
        // **A lower bound, stated rather than discovered.** `TurnCostUpdated`
        // is suppressed for child scopes (openhuman's `observability`: a
        // sub-agent's spend reaches the parent's `last_turn_usage_totals`
        // instead), so an attempt that had delegated under-reports the
        // delegates. Understating an attempt is a far smaller wrong than
        // reporting it as free, and this seam cannot see more than the stream
        // carries.
        if usages.iter().any(TurnUsage::is_zero) {
            let segments = attempt_event_segments(&events, usages.len());
            for (usage, segment) in usages.iter_mut().zip(segments) {
                if !usage.is_zero() {
                    continue;
                }
                if let Some(observed) = last_observed_turn_cost(segment) {
                    tracing::info!(
                        agent = %self.agent_id,
                        input_tokens = observed.input_tokens,
                        output_tokens = observed.output_tokens,
                        cost_usd = observed.cost_usd,
                        "[turn] an attempt published no totals; metering the spend observed on \
                         its own progress-stream segment"
                    );
                    *usage = observed;
                }
            }
        }
        // The cap openhuman was actually enforcing, for the trace only. Taken
        // from the last `IterationStarted` rather than from config, so the log
        // reports the number the turn ran under instead of the one this crate
        // believes it configured. Deliberately NOT plumbed into the operator
        // notice: one notice can cover a responder turn, a desk turn and a
        // relay turn, and naming one of their caps would be a number the
        // operator cannot map back to anything.
        let iteration_cap = events.iter().rev().find_map(|event| match event {
            oh::agent::progress::AgentProgress::IterationStarted { max_iterations, .. } => {
                Some(*max_iterations)
            }
            _ => None,
        });
        if hit_iteration_cap {
            tracing::info!(
                agent = %self.agent_id,
                iteration_cap,
                "[turn] paused at the tool-iteration cap; the reply is a resumable checkpoint, not a finished answer"
            );
        }
        // Issue #1032: read the spend brake the same way. Not under the agent
        // lock — the flag lives on the hook, not on the vendored session, and
        // the hook has already finished running by the time `with_stop_hooks`
        // returns.
        //
        // The spend is summed over every attempt's usage rather than read from
        // the hook, so the figure covers the retry path's second attempt too:
        // both were paid for, and reporting only one would understate what the
        // turn actually cost.
        let halted_for_spend = spend_brake.and_then(|(cap_usd, halted)| {
            halted
                .load(std::sync::atomic::Ordering::SeqCst)
                .then(|| SpendHalt {
                    agent: self.agent_id.clone(),
                    spent_usd: usages.iter().map(|usage| usage.cost_usd).sum(),
                    cap_usd,
                })
        });
        if let Some(halt) = &halted_for_spend {
            tracing::info!(
                agent = %self.agent_id,
                spent_usd = halt.spent_usd,
                cap_usd = halt.cap_usd,
                "[turn] halted at the in-turn spend cap; the reply stops short of the work it was doing"
            );
        }
        // Issue #1846: read the same way `halted_for_spend` is — the async
        // block above only borrowed this local, so the borrow has ended by the
        // time `with_stop_hooks` returned it.
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

        // The usage is returned BESIDE the result, never inside it (issue
        // B-120). `reply?` here would have discarded `usages` on every hard
        // failure — a wall-clock ceiling, a provider fault, an auth error —
        // and those attempts had already burned every token they read back.
        let outcome = reply.map(|reply| TurnOutcome {
            reply,
            steps,
            hit_iteration_cap,
            // This is the built_in harness, not the ACP fold — the only
            // path that produces an abnormal stop (PR #1880 review).
            abnormal_stop: None,
            halted_for_spend,
            budget_paused,
        });
        (outcome, usages)
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
/// Deliberately reuses `oh::inference::provider::is_budget_exhausted_message`
/// rather than forking a second copy of the phrase list — the whole point of
/// this fix is to close the asymmetry, not add a second place for the two to
/// drift apart. See `budget_wire_shapes_all_classify_as_budget_paused` for the
/// drift-coupling test that fails CI if the two ever disagree.
fn is_top_level_budget_exhausted(err: &anyhow::Error) -> bool {
    oh::inference::provider::is_budget_exhausted_message(&format!("{err:#}"))
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

/// Reads the just-completed turn's usage (zero when the provider reported none).
fn read_turn_usage(agent: &Agent) -> TurnUsage {
    agent
        .last_turn_usage()
        .map(|u| TurnUsage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cached_input_tokens: u.cached_input_tokens,
            cost_usd: u.cost_usd,
        })
        .unwrap_or_default()
}

/// The last cumulative cost tally openhuman published on a turn's progress
/// stream, or `None` when the turn made no metered model call.
///
/// [`TurnCostUpdated`](oh::agent::progress::AgentProgress::TurnCostUpdated) is
/// cumulative across one `agent.turn`, so the **last** frame is the whole
/// attempt's spend and earlier ones must never be summed with it.
///
/// This is the only figure a hard-failed attempt leaves behind — see the call
/// site in [`CompanyAgent::run_with_steer`] for why `read_turn_usage` reads back
/// nothing for one.
fn last_observed_turn_cost(events: &[oh::agent::progress::AgentProgress]) -> Option<TurnUsage> {
    events.iter().rev().find_map(|event| match event {
        oh::agent::progress::AgentProgress::TurnCostUpdated {
            input_tokens,
            output_tokens,
            cached_input_tokens,
            total_usd,
            ..
        } => Some(TurnUsage {
            input_tokens: *input_tokens,
            output_tokens: *output_tokens,
            cached_input_tokens: *cached_input_tokens,
            cost_usd: *total_usd,
        }),
        _ => None,
    })
}

/// Splits a turn's flat progress-event stream into one contiguous slice per
/// attempt, so [`last_observed_turn_cost`] can read a zeroed attempt's own
/// tally back without crediting it with a DIFFERENT attempt's spend (Codex
/// review, PR #2053).
///
/// [`AgentProgress::TurnStarted`](oh::agent::progress::AgentProgress::TurnStarted)
/// is emitted exactly once at the very top of every `agent.turn()` call
/// (`core_turn.rs`, "about to enter the iteration loop") and never for a
/// delegated sub-agent's turn — those use `SubagentIterationStarted`/
/// `SubagentToolCallStarted` instead — so each attempt owns exactly one
/// contiguous run of events starting at its own `TurnStarted` and ending
/// where the next attempt's begins, or at the stream's end for the last.
///
/// `attempts` is `usages.len()` — the number of `agent.turn()` calls the
/// wrapper actually made (one, or two across the one-shot retry). Always
/// returns exactly that many slices; an attempt whose `TurnStarted` never
/// reached this stream (openhuman's collector drops nothing observed in
/// practice, but the channel is not literally unbounded) gets an empty one,
/// which is the same "nothing to recover" outcome as before this fix.
fn attempt_event_segments(
    events: &[oh::agent::progress::AgentProgress],
    attempts: usize,
) -> Vec<&[oh::agent::progress::AgentProgress]> {
    let starts: Vec<usize> = events
        .iter()
        .enumerate()
        .filter_map(|(i, event)| {
            matches!(event, oh::agent::progress::AgentProgress::TurnStarted).then_some(i)
        })
        .collect();
    (0..attempts)
        .map(|i| match starts.get(i) {
            Some(&start) => {
                let end = starts.get(i + 1).copied().unwrap_or(events.len());
                &events[start..end]
            }
            None => &events[0..0],
        })
        .collect()
}

/// Writes every attempt's spend of a **finished** turn to the ledger and the
/// usage meter, whether that turn succeeded or failed.
///
/// The one place `record_turn_cost` is called from the pool, so the two turn
/// paths cannot disagree about when a turn is metered. Both call it *before*
/// they unwrap the turn's own result — see
/// [`turn_result_after_metering`] for why that ordering is the fix and not an
/// accident of layout.
async fn meter_turn_costs(
    turn_costs: &[TurnUsage],
    agent_id: &str,
    company: &CompanyId,
    deps: &HarnessDeps,
    run_id: Option<&str>,
) -> crate::Result<()> {
    // Attribute cost to the provider and model this turn actually resolved to.
    // With a per-tenant [`TenantProvider`](crate::harness::provider::TenantProvider)
    // a console BYOK switch changes the slug between turns, so read both live
    // rather than trusting the static `deps.provider_slug` baked at build. The
    // model is folded onto the closed vocabulary at the provider so no
    // operator-authored model name reaches the meter (issue #1749).
    let provider_slug = deps.provider.telemetry_provider_id();
    let model_slug = deps.provider.telemetry_model();
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

impl HarnessPool {
    /// Builds an empty pool.
    pub fn new() -> Self {
        Self {
            agents: RwLock::new(HashMap::new()),
            mcp_fingerprints: RwLock::new(HashMap::new()),
            overlay_fingerprints: RwLock::new(HashMap::new()),
            capability_fingerprints: RwLock::new(HashMap::new()),
            composio_fingerprints: RwLock::new(HashMap::new()),
            billing_fingerprints: RwLock::new(HashMap::new()),
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
        skill_deltas.extend(globals_skill_disables(&company.manifest.globals.disable));
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
        let roster = build_roster(&fresh_company, &fresh_deps, &skill_deltas, &routed_context)?;

        // Keep the policy snapshot and the roster together for the entire turn.
        // `ensure_with_policy` pins the snapshot on the pool (above), so a
        // concurrent plain `ensure` — the workflow runner, a spawned task outside
        // the cycle serial lock — rebuilds the policy axis against that same pin
        // instead of a live override it could otherwise adopt a turn early. The
        // serial lock already serializes cycle callers; the pin is what keeps a
        // direct caller from regressing a pinned roster before `run_inner` clones
        // its agent.
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
    /// Both the backend URL (from [`composio::COMPOSIO_BACKEND_URL_ENV`], then the
    /// tenant API base [`composio::TINYHUMANS_API_URL_ENV`], then the prod
    /// default) and the platform identity are read process-globally here, so a
    /// live re-resolution keeps them even when nothing was stored at boot.
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
                let url = env.get(composio::COMPOSIO_BACKEND_URL_ENV);
                let api_url = env.get(composio::TINYHUMANS_API_URL_ENV);
                composio::TenantComposio::resolve(
                    &company.id,
                    secrets.as_ref(),
                    toolkits,
                    url,
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
    /// **Answers `false` wherever the ceiling cannot be evaluated** — no plan,
    /// no total budget, no meter, or a failed spend query — which is exactly
    /// what `total_ceiling_refusal` does with the same cases: it declines to
    /// hard-refuse and defers to the per-namespace fail-closed roster. A gate
    /// that instead blocked on an unreadable meter would take routing down on
    /// a metering hiccup.
    pub(crate) async fn total_ceiling_spent(company: &CompanyId, deps: &HarnessDeps) -> bool {
        let Some(plan) = deps.plan.as_ref() else {
            return false;
        };
        if plan.total_budget.is_none() {
            return false;
        }
        let Some(meter) = deps.meter.as_deref() else {
            return false;
        };
        let since = plan.period.period_start_millis(crate::ports::now_millis());
        match meter.query(company, since).await {
            Ok(samples) => plan.total_exhausted(capability_budget::tokens_in(&samples)),
            Err(_) => false,
        }
    }

    /// The plan-level total-token ceiling, as a refusal or nothing.
    ///
    /// Extracted from [`run_inner`](Self::run_inner) so the confined turn
    /// (issue #416) is gated by the *same* ceiling rather than a second copy of
    /// the rule: a turn that reaches nothing still spends model tokens, so a
    /// tenant past its cap must not be able to keep spending through the
    /// copilot.
    async fn total_ceiling_refusal(
        company: &CompanyId,
        agent_id: &str,
        deps: &HarnessDeps,
    ) -> Option<TurnOutcome> {
        let plan = deps.plan.as_ref()?;
        plan.total_budget?;
        match deps.meter.as_deref() {
            Some(meter) => {
                let since = plan.period.period_start_millis(crate::ports::now_millis());
                match meter.query(company, since).await {
                    Ok(samples) => {
                        let spent = capability_budget::tokens_in(&samples);
                        // Issue #1846: the coarse pre-task proximity warning,
                        // read BESIDE the exhaustion check above — same query,
                        // same samples, no second meter read. Fail-open by
                        // construction: this whole arm only runs when the read
                        // already succeeded, and it makes no per-task cost
                        // claim, only "you are near the period ceiling".
                        // Published, never returned — a warning is
                        // non-blocking, so the turn keeps dispatching normally
                        // whether or not a console happens to be listening.
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
                        if plan.total_exhausted(spent) {
                            tracing::info!(
                                company = %company,
                                agent = agent_id,
                                spent,
                                "[capability-budget] total token ceiling reached; refusing dispatch (no model call) until the period resets"
                            );
                            return Some(TurnOutcome {
                                reply: TOTAL_BUDGET_EXHAUSTED_NOTICE.to_string(),
                                steps: Vec::new(),
                                // No model call ran, so no cap was reached
                                // (issue #926). A refusal is not a pause.
                                hit_iteration_cap: false,
                                // This pre-turn refusal is its own, older
                                // signal (the reply text itself names the
                                // cap) — not the PR #1880 `abnormal_stop`,
                                // which is scoped to the ACP fold's
                                // refusal/cancelled/unrecognized stops.
                                abnormal_stop: None,
                                // And no in-turn hook fired, because no turn
                                // ran (issue #1032). The reply already IS the
                                // budget notice; labelling this as a halt too
                                // would tell the operator the same thing twice.
                                halted_for_spend: None,
                                // Issue #1846: this is OpenCompany's own
                                // plan-level token ceiling refusing dispatch —
                                // a company policy, not the provider account
                                // being out of money. No model call ran, so
                                // `classify_turn` never saw a wire error to
                                // classify.
                                budget_paused: None,
                            });
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            company = %company,
                            %error,
                            "[capability-budget] total-ceiling spend query failed; not hard-refusing — deferring to the per-namespace fail-closed roster"
                        );
                    }
                }
            }
            None => {
                tracing::warn!(
                    company = %company,
                    "[capability-budget] no usage meter; cannot enforce the total token ceiling — deferring to the per-namespace fail-closed roster"
                );
            }
        }
        None
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
        if let Some(refusal) =
            Self::total_ceiling_refusal(company, confine::CONFINED_AGENT_ID, deps).await
        {
            return Ok(refusal);
        }

        let confined = confine::build_confined_agent(company, company_name, confinement, deps)?;
        let agent = CompanyAgent {
            agent_id: confine::CONFINED_AGENT_ID.to_string(),
            role: "Workflow copilot".to_string(),
            // A confined turn carries no manifest teammate, so there is no
            // per-agent daily cap to read; the company-wide ceiling above is the
            // one that applies to it.
            budget_usd_daily: None,
            step_labels: steps::StepLabels::from_tools(confined.tools()),
            agent: Mutex::new(confined),
            bound_chat: Mutex::new(None),
        };

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
                None,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await;

        // Metered before the outcome is unwrapped: a copilot turn that failed
        // still consumed whatever it consumed before it failed.
        let metered =
            meter_turn_costs(&turn_costs, confine::CONFINED_AGENT_ID, company, deps, None).await;
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
        // Fail-closed tradeoff (issue #188): the hard refusal fires ONLY when
        // spend is actually readable. With no meter, or a meter whose query
        // errors, we do NOT brick the tenant on a transient read failure — we
        // fall through to run the turn, which the per-namespace fail-closed path
        // in `resolve_filter`/`ensure` has already stripped of every exec tool.
        // A `warn!` records the deferral. Refusing every turn on a flaky meter
        // read would be a strictly worse failure mode than letting an
        // intrinsic-tools-only turn through.
        if let Some(refusal) = Self::total_ceiling_refusal(company, agent_id, deps).await {
            return Ok(refusal);
        }

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
        // FAIL-OPEN, mirroring #188's documented tradeoff: with no meter, or a
        // meter whose query errors, we warn and run the turn. Bricking a
        // company's cognition on a flaky read would be a strictly worse failure
        // mode than one day of overspend, and there is no operator recourse at
        // turn level (unlike the policy arm, whose park a human can approve —
        // which is why THAT layer fails closed and this one does not).
        if let Some(cap) = agent.budget_usd_daily {
            match deps.meter.as_deref() {
                Some(meter) => {
                    let since = crate::metering::utc_day_start_millis(crate::ports::now_millis());
                    match meter.query(company, since).await {
                        Ok(samples) => {
                            let spent = crate::metering::usd_spent_by_agent(&samples, agent_id);
                            // Issue #1846: same coarse proximity warning as the
                            // total-ceiling read above, beside this per-agent
                            // read, reusing the SAME `samples` — no second
                            // query. Non-blocking; only fires when this
                            // teammate is not already refused below.
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
                                return Ok(TurnOutcome {
                                    reply: agent_budget_exhausted_notice(agent_id, cap),
                                    steps: Vec::new(),
                                    // No model call ran, so no cap was reached
                                    // (issue #926). A refusal is not a pause.
                                    hit_iteration_cap: false,
                                    // Same reasoning as the total-ceiling
                                    // refusal above: this is its own signal,
                                    // not the PR #1880 ACP-only field.
                                    abnormal_stop: None,
                                    // Same teammate cap, refused BEFORE the
                                    // turn (issue #1032). The in-turn brake
                                    // never armed, and the reply above already
                                    // names the cap it refused against.
                                    halted_for_spend: None,
                                    // Issue #1846: same reasoning as the total
                                    // ceiling refusal above — this is the
                                    // teammate's manifest cap, not the provider
                                    // account being out of credits, and no
                                    // model call ran to classify.
                                    budget_paused: None,
                                });
                            }
                        }
                        Err(error) => {
                            tracing::warn!(
                                company = %company,
                                agent = agent_id,
                                %error,
                                "[agent-budget] daily-spend query failed; running the turn rather than bricking this teammate"
                            );
                        }
                    }
                }
                None => {
                    tracing::warn!(
                        company = %company,
                        agent = agent_id,
                        "[agent-budget] no usage meter; the per-agent daily spend cap cannot be enforced on this host"
                    );
                }
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
        // attempt's token/cost totals from openhuman's public `last_turn_usage()`
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
        let seed_chat: Option<Option<&str>> = match &live {
            LiveStream::On { chat_id, .. } => Some(*chat_id),
            _ => None,
        };
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
        // Recent-chat history seed (issue #1840): give a chat reply this desk's
        // own recent turns so it isn't assembled blind on every switch. Only
        // ever wanted for a chat turn with the company journal wired — never
        // built here, though: `run_with_steer` projects it itself, and only
        // once its `bound_chat`-locked switch check confirms this turn is
        // actually a switch (a same-desk reply right after another one is not,
        // and building it unconditionally on every chat turn made every
        // ordinary reply pay for a journal scan whose result would just be
        // thrown away — codex review finding). This is just the (cheap — two
        // `Arc` clones, no I/O) request the projection needs when the switch
        // check does land on `true`. The current operator message is ALREADY
        // journaled at this point (the server appends it before dispatch), so
        // it is the newest owning event the projector sees; `raw_message` is
        // what `chat_seed::strip_current_message` matches to strip it —
        // `run_single` re-appends the current message itself, so seeding it
        // too would duplicate it on the wire.
        let chat_seed_request = match (seed_chat, deps.events.as_ref()) {
            (Some(_), Some(events)) => Some(chat_seed::ChatSeedRequest {
                raw_message: message.to_string(),
                events: events.clone(),
                store: deps.store.clone(),
                thread_root: chat.thread_root,
                current_message_seq: chat.message_seq,
            }),
            _ => None,
        };
        // Issue #1890 F: the conversation this turn answers, ambient for the
        // duration of it, so `read_thread` can scope itself to the channel the
        // turn is actually in. Set here rather than on the tool because a belt
        // is built once per agent while a conversation changes every message.
        //
        // From the caller's `chat` since #1890 I, which is what the note F
        // shipped with said would happen when the two met: identity no longer
        // rides on the stream, so an approval's re-issued call — unstreamed,
        // but raised in a conversation — can read that conversation's threads
        // like any other turn.
        // Route first, caller second — the same order `turn_chat_id` resolves
        // in one frame down, and for the same reason: the live route has
        // already folded an unaddressed message onto `DEFAULT_DESK`, so reading
        // `chat.chat_id` alone yields `None` there, which `read_thread` treats
        // as a refusal. A turn on the General desk could then not read its own
        // channel's threads (coderabbit on #1972).
        let turn_chat = stream_ctx
            .as_ref()
            .and_then(|ctx| match &ctx.route {
                crate::turn_stream::LiveRoute::Chat { chat_id } => Some(chat_id.clone()),
                crate::turn_stream::LiveRoute::Workflow { .. } => None,
            })
            .or_else(|| chat.chat_id.map(str::to_string));
        let (outcome, turn_costs) = crate::runtime::delegation::with_turn_conversation(
            turn_chat,
            deps.approval_requests.turn_scoped(agent.run_with_steer(
                &augmented,
                steer,
                stream_ctx,
                run_sink.clone(),
                chat_seed_request,
                // The caller's own, not read off `live` (#1890 I). A turn can
                // have a conversation and stream nothing.
                chat,
            )),
        )
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
/// The disabling [`SkillState`] deltas a company's `[globals].disable` implies.
///
/// One per `skill:<slug>` entry, and nothing else: an entry naming another kind
/// is that kind's business, and manifest validation has already refused an entry
/// naming nothing at all.
pub(crate) fn globals_skill_disables(disable: &[String]) -> Vec<SkillState> {
    disable
        .iter()
        .filter_map(|entry| entry.strip_prefix("skill:"))
        .map(|slug| SkillState {
            slug: slug.to_string(),
            enabled: false,
            // The shared library is where these skills are authored, so that is
            // what they are a delta over. The value is inert here in any case:
            // this delta is synthesized per rebuild, never stored, and only its
            // `enabled = false` is read.
            source: crate::ports::SkillSource::Registry,
            custom_doc: None,
        })
        .collect()
}

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

pub(crate) fn build_roster(
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

    // Issue #1124: the company's per-server read-only MCP declaration, resolved
    // once and installed on every agent's policy so a server-declared read-only
    // bridge call does not park under `auto`. Built from the same effective MCP
    // servers the harness wires tools from, so the gate and the toolbelt cannot
    // disagree about which server declared what.
    let mcp_reads = crate::company::mcp::mcp_read_set(&deps.mcp_servers);

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
        let mut agent_policy = ApprovalPolicy::new(policy, effective_budget)
            .with_policy_hitl_disabled()
            .with_requests(deps.approval_requests.clone())
            // Issue #243: stamp who the parked effect belongs to, so approving it
            // can hand the grant back to this agent rather than to nobody.
            .with_agent(manifest_agent.id.clone())
            // Issue #1124: the per-server read-only MCP declaration, so a
            // server-declared read-only bridge call does not park under `auto`.
            .with_mcp_reads(mcp_reads.clone());
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
        let agent = build::build_agent(
            &company.id,
            company_name,
            manifest_agent,
            agent_policy,
            deps,
            &grants,
            skill_deltas,
            routed_context
                .get(&manifest_agent.id)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            effective_instructions.as_deref(),
            is_orchestrator,
        )?;
        roster.push(Arc::new(CompanyAgent {
            agent_id: manifest_agent.id.clone(),
            role: manifest_agent.role.clone(),
            budget_usd_daily: effective_budget,
            step_labels: steps::StepLabels::from_tools(agent.tools()),
            agent: Mutex::new(agent),
            bound_chat: Mutex::new(None),
        }));
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
        let mut agent_policy = ApprovalPolicy::new(policy, effective_budget)
            .with_policy_hitl_disabled()
            .with_requests(deps.approval_requests.clone())
            // An overlay teammate is a real roster agent and re-dispatches the
            // same way a manifest one does (issue #243).
            .with_agent(manifest_agent.id.clone())
            // Issue #1124: the same per-server read-only MCP declaration the
            // manifest agents get — an overlay teammate calls the same servers.
            .with_mcp_reads(mcp_reads.clone());
        if let Some(workspace) = deps.workspace.as_ref() {
            agent_policy = agent_policy.with_workspace(workspace.clone(), company.id.clone());
        }
        if let Some(meter) = deps.meter.as_ref() {
            agent_policy = agent_policy.with_spend(meter.clone(), company.id.clone());
        }
        // An overlay teammate is scoped by its desks the same as a manifest one:
        // it can be seated on a desk, and a desk ceiling that applied to only
        // half its members would not be a ceiling.
        let desk_tools = company.agent_desk_tools(&manifest_agent.id);
        let desk_allows: Vec<&[String]> = desk_tools.iter().map(Vec::as_slice).collect();
        let grants = agent_scoped_grants(allow, &desk_allows, manifest_agent.tools.as_deref());
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
        let agent = build::build_agent(
            &company.id,
            company_name,
            &manifest_agent,
            agent_policy,
            deps,
            &grants,
            skill_deltas,
            routed_context
                .get(&manifest_agent.id)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            effective_instructions.as_deref(),
            /* is_orchestrator */ false,
        )?;
        roster.push(Arc::new(CompanyAgent {
            agent_id: manifest_agent.id.clone(),
            role: manifest_agent.role.clone(),
            budget_usd_daily: effective_budget,
            step_labels: steps::StepLabels::from_tools(agent.tools()),
            agent: Mutex::new(agent),
            bound_chat: Mutex::new(None),
        }));
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
        // Issue #176: an overlay teammate declares no delegation allowlist in
        // this slice, so it carries today's behaviour — no hand-off tools wired.
        // Opting overlays in needs a console write surface; see the follow-up.
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
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    use async_trait::async_trait;
    use tinyinference::model::{ChatModel, ModelRequest, ModelResponse};

    use crate::company::CompanyManifest;
    use crate::harness::provider::MockProvider;
    use crate::ports::UsageSample;
    use crate::ports::types::{
        ChunkAddr, ChunkHit, ChunkMeta, CompanySummary, ContextChunk, LedgerEntry,
    };
    // The two-level resolver. Test-only now: the roster build goes through
    // `agent_scoped_grants`, and these tests assert the desk-less case still
    // resolves identically to what shipped before desks could scope tools.
    use crate::runtime::builder::agent_effective_grants;

    fn fp_entry_full(
        mode: Option<&str>,
        always: Option<Vec<&str>>,
        cap: Option<Option<f64>>,
        ttl: Option<u64>,
    ) -> PolicyOverride {
        use crate::ports::types::{Actor, ActorKind};
        PolicyOverride {
            mode: mode.map(str::to_string),
            always_approve: always.map(|v| v.into_iter().map(str::to_string).collect()),
            auto_approve_under_usd: cap,
            approval_ttl_hours: ttl,
            set_by: Actor {
                kind: ActorKind::User,
                id: "user-1".to_string(),
            },
            at_millis: 1_700_000_000_000,
        }
    }

    /// An effective `[policy]` block for fingerprint tests — what the roster is
    /// actually built from (`CompanyRecord::effective_policy`).
    fn fp_policy(mode: &str, always: &[&str], cap: Option<f64>, ttl: Option<u64>) -> Policy {
        Policy {
            mode: mode.to_string(),
            always_approve: always.iter().map(|k| (*k).to_string()).collect(),
            auto_approve_under_usd: cap,
            approval_ttl_hours: ttl,
        }
    }

    /// The fingerprint moves when the tier moves (issue #562).
    ///
    /// This is the assertion that keeps the feature from being a no-op.
    /// `ApprovalPolicy` is built once per roster build, and `ensure` reuses the
    /// cached roster unless a fingerprint changed — so if this returned a
    /// constant, a console tier change would persist, return `204`, render as
    /// applied, and be **silently ignored until the process restarted**. Every
    /// other test in this change would still pass.
    #[test]
    fn the_policy_fingerprint_moves_when_the_tier_does() {
        let supervised = effective_policy_fingerprint(&fp_policy("supervised", &[], None, None));
        let full = effective_policy_fingerprint(&fp_policy("full", &[], None, None));

        assert_ne!(
            supervised, full,
            "a tier change must move the fingerprint or the roster is never rebuilt"
        );
    }

    /// An always-ask edit moves it too, including clearing the list.
    ///
    /// `always_approve` wins over every tier including `full`, so an edit that
    /// did not rebuild would leave the gate enforcing a list the operator had
    /// already changed — the failure mode is stricter *or* looser than what the
    /// console shows, depending on the edit.
    #[test]
    fn the_policy_fingerprint_moves_when_the_always_ask_list_does() {
        let empty = effective_policy_fingerprint(&fp_policy("auto", &[], None, None));
        let one = effective_policy_fingerprint(&fp_policy("auto", &["payment.send"], None, None));
        let two = effective_policy_fingerprint(&fp_policy(
            "auto",
            &["payment.send", "filing.submit"],
            None,
            None,
        ));

        assert_ne!(empty, one, "adding an entry must move the fingerprint");
        assert_ne!(one, two, "a second entry must move it again");

        // Order is part of the value: the list is the operator's own, not an
        // accumulation of independent rows, so a reorder is a real edit.
        let reordered = effective_policy_fingerprint(&fp_policy(
            "auto",
            &["filing.submit", "payment.send"],
            None,
            None,
        ));
        assert_ne!(two, reordered);

        // Length is folded in, so concatenation cannot collide.
        let split = effective_policy_fingerprint(&fp_policy("auto", &["a", "b"], None, None));
        let joined = effective_policy_fingerprint(&fp_policy("auto", &["ab"], None, None));
        assert_ne!(split, joined);
    }

    /// A deadline-only change has no roster fingerprint.
    ///
    /// The TTL is enforced by the live gate, not the roster snapshot
    /// (`ApprovalPolicy` carries no TTL), so a deadline-only edit must not
    /// discard live agent sessions for a rebuild that could not apply it.
    #[test]
    fn a_deadline_only_change_has_no_roster_fingerprint() {
        let no_deadline = effective_policy_fingerprint(&fp_policy("auto", &[], None, None));
        let deadline = effective_policy_fingerprint(&fp_policy("auto", &[], None, Some(72)));
        assert_eq!(no_deadline, deadline);
    }

    /// A spend-cap edit moves it too — the third axis a console save can touch
    /// without touching the tier or the list.
    ///
    /// `ApprovalPolicy` is built once per roster, so a cap-only edit that left
    /// the fingerprint stable would keep the harness gate enforcing the old
    /// threshold until restart. `auto_approve_under_usd`'s `Some`/`None` are
    /// both states: an explicit no-cap (`None`) and a finite cap must each be
    /// distinct. The deadline is deliberately NOT in the fingerprint — the
    /// roster snapshot carries no TTL, and the deadline lives in the live gate,
    /// so a deadline-only edit must not discard agent sessions for a rebuild
    /// that could not apply it.
    #[test]
    fn the_policy_fingerprint_moves_when_the_cap_does() {
        let base = effective_policy_fingerprint(&fp_policy("auto", &[], None, None));
        let finite = effective_policy_fingerprint(&fp_policy("auto", &[], Some(25.0), None));
        let tighter = effective_policy_fingerprint(&fp_policy("auto", &[], Some(10.0), None));
        let deadline = effective_policy_fingerprint(&fp_policy("auto", &[], None, Some(72)));

        assert_ne!(base, finite, "a finite cap must rebuild");
        assert_ne!(finite, tighter, "a different cap value must rebuild");
        assert_eq!(
            base, deadline,
            "a deadline-only edit must NOT rebuild: the roster snapshot carries no TTL, \
             so a rebuild could not apply it and would only discard live agent sessions"
        );
        // Re-setting the same cap is a no-op, like re-setting the same tier.
        assert_eq!(
            finite,
            effective_policy_fingerprint(&fp_policy("auto", &[], Some(25.0), None))
        );
    }

    /// Issue #661 / L5: an overlay teammate's own `tools` grant flows into the
    /// manifest shape `build_agent` consumes, and is INTERSECTED with the company
    /// allow-list — narrow-only, never a widen. An empty grant is the standard
    /// company-wide grant, exactly as the pre-L5 hardcoded empty was.
    #[test]
    fn overlay_agent_to_manifest_carries_the_tool_grant() {
        let allow = vec!["docs.*".to_string(), "web".to_string()];

        // A scoped overlay teammate: the grant is carried, then narrowed to what
        // the company already allows. `payment.send` is NOT in `allow`, so the
        // overlay cannot escalate to it — the security invariant.
        let scoped = OverlayAgent {
            id: "scoped".into(),
            name: "Scoped".into(),
            role: "Researcher".into(),
            description: None,
            tools: Some(vec!["docs.*".into(), "payment.send".into()]),
            model: None,
            harness: None,
        };
        let manifest = overlay_agent_to_manifest(&scoped);
        assert_eq!(
            manifest.tools,
            Some(vec!["docs.*".to_string(), "payment.send".to_string()]),
            "the overlay's own grant must reach the manifest shape"
        );
        assert_eq!(
            agent_effective_grants(&allow, manifest.tools.as_deref()),
            vec!["docs.*".to_string()],
            "narrow-only: the un-allowed `payment.send` is intersected out"
        );

        // An absent (`None`) overlay grant is the standard company-wide grant.
        // Since #1804 this is `None`, NOT an empty list (which is a deny-all).
        let standard = OverlayAgent {
            id: "std".into(),
            name: "Std".into(),
            role: "Generalist".into(),
            description: None,
            tools: None,
            model: None,
            harness: None,
        };
        let manifest = overlay_agent_to_manifest(&standard);
        assert!(manifest.tools.is_none());
        assert_eq!(
            agent_effective_grants(&allow, manifest.tools.as_deref()),
            allow,
            "an empty grant falls back to the full company allow-list"
        );
    }

    /// Issue #1105: the overlay's display name is the only place the operator's
    /// chosen name exists, and the console shows it on the DM header, subtitle
    /// and composer. Dropping it here left the persona framed from the role
    /// alone, so the teammate denied being the person on its own header.
    #[test]
    fn overlay_agent_to_manifest_carries_the_display_name() {
        let overlay = OverlayAgent {
            id: "alex".into(),
            name: "Alex".into(),
            role: "Content Writer".into(),
            description: None,
            tools: None,
            model: None,
            harness: None,
        };

        let manifest = overlay_agent_to_manifest(&overlay);
        assert_eq!(manifest.name.as_deref(), Some("Alex"));
        // And it reaches the one place it has to: the persona the model reads.
        let persona = crate::company::prompt::persona_prompt("Acme", &manifest, None);
        assert!(
            persona.contains("You are Alex, the Content Writer at Acme"),
            "{persona}"
        );
    }

    /// Issue #661 / L5: a grant edit changes the roster the harness must build, so
    /// it has to move the overlay fingerprint — otherwise a re-grant would
    /// persist, render as applied, and be silently ignored until the process
    /// restarted, the same staleness the tier/skill fingerprints guard against.
    #[test]
    fn overlay_fingerprint_moves_on_a_tools_only_edit() {
        let one = |tools: Option<Vec<String>>| {
            vec![OverlayAgent {
                id: "a".into(),
                name: "A".into(),
                role: "r".into(),
                description: None,
                tools,
                model: None,
                harness: None,
            }]
        };
        // `None` = standard grant, `Some(list)` = narrowed (issue #1804).
        let standard = one(None);
        let scoped = one(Some(vec!["docs.*".into()]));
        let scoped_more = one(Some(vec!["docs.*".into(), "email".into()]));

        assert_ne!(
            overlay_fingerprint(&standard, &[], &[]),
            overlay_fingerprint(&scoped, &[], &[]),
            "adding a grant must move the fingerprint or the re-grant is ignored until restart"
        );
        assert_ne!(
            overlay_fingerprint(&scoped, &[], &[]),
            overlay_fingerprint(&scoped_more, &[], &[]),
            "widening the grant list must move it too"
        );
        // Identical grants → identical fingerprint (no spurious rebuild).
        assert_eq!(
            overlay_fingerprint(&scoped, &[], &[]),
            overlay_fingerprint(&one(Some(vec!["docs.*".into()])), &[], &[])
        );
    }

    /// An overlay teammate's routing binding has to move the same axis: a
    /// model/harness change is not a persona edit, but the roster the harness
    /// builds consumes it (`overlay_agent_to_manifest` carries both straight
    /// through), so a re-bind that moved nothing would be ignored until the
    /// process restarted (issue #1676 review note).
    #[test]
    fn overlay_fingerprint_moves_on_a_model_or_harness_change() {
        let one = |model: Option<&str>, harness: Option<&str>| {
            vec![OverlayAgent {
                id: "a".into(),
                name: "A".into(),
                role: "r".into(),
                description: None,
                tools: None,
                model: model.map(str::to_string),
                harness: harness.map(str::to_string),
            }]
        };
        let none = one(None, None);
        let model = one(Some("chat-v2"), None);
        let model_again = one(Some("chat-v2"), None);
        let harness = one(None, Some("acp"));
        let cleared = one(Some(""), None);

        assert_ne!(
            overlay_fingerprint(&none, &[], &[]),
            overlay_fingerprint(&model, &[], &[]),
            "binding an overlay to a model must move the fingerprint or the re-bind is ignored until restart"
        );
        assert_ne!(
            overlay_fingerprint(&model, &[], &[]),
            overlay_fingerprint(&harness, &[], &[]),
            "binding an overlay to a harness must move the fingerprint too"
        );
        // The stored `Some("")` "cleared" form is a distinct routing state from
        // `None` ("never edited"), the same discriminant the resolver uses.
        assert_ne!(
            overlay_fingerprint(&none, &[], &[]),
            overlay_fingerprint(&cleared, &[], &[]),
            "an explicit clear must not hash like an untouched overlay"
        );
        // The same binding twice → the same fingerprint (no spurious rebuild).
        assert_eq!(
            overlay_fingerprint(&model, &[], &[]),
            overlay_fingerprint(&model_again, &[], &[])
        );
    }

    /// An edit of a **manifest** teammate has to move the same axis, and for the
    /// same reason: a persona is assembled once per roster, so a rename that
    /// moved nothing would read back correctly on the Team page and be invisible
    /// to every turn the teammate took until the process restarted.
    #[test]
    fn overlay_fingerprint_moves_on_an_edit_of_a_manifest_teammate() {
        let edit = |role: &str| {
            vec![crate::ports::types::AgentOverride {
                agent_id: "ceo".into(),
                role: Some(role.to_string()),
                ..Default::default()
            }]
        };
        let none: Vec<crate::ports::types::AgentOverride> = Vec::new();

        assert_ne!(
            overlay_fingerprint(&[], &none, &[]),
            overlay_fingerprint(&[], &edit("Chief Vibes"), &[]),
            "a console rename must move the fingerprint or it is ignored until restart"
        );
        assert_ne!(
            overlay_fingerprint(&[], &edit("Chief Vibes"), &[]),
            overlay_fingerprint(&[], &edit("Chief Executive"), &[]),
            "and re-editing it must move it again"
        );
        // The same edit twice → the same fingerprint, so a save that changed
        // nothing does not drop every live session.
        assert_eq!(
            overlay_fingerprint(&[], &edit("Chief Vibes"), &[]),
            overlay_fingerprint(&[], &edit("Chief Vibes"), &[])
        );
    }

    /// A **routing** edit of a manifest teammate — a model or harness re-bind —
    /// has to move the same axis for the same reason: the roster the harness
    /// builds reads the override's routing fields, so a re-bind that moved
    /// nothing would be silently ignored until the process restarted (issue
    /// #1676 review note). `Some("")` (the stored "cleared" form) is a distinct
    /// routing state from `None` ("never edited"), mirroring the resolver's
    /// reset-to-blueprint contract.
    #[test]
    fn overlay_fingerprint_moves_on_a_model_or_harness_edit_of_a_manifest_teammate() {
        use crate::ports::types::AgentOverride;
        let edit = |model: Option<&str>, harness: Option<&str>| {
            vec![AgentOverride {
                agent_id: "ceo".into(),
                model: model.map(str::to_string),
                harness: harness.map(str::to_string),
                ..Default::default()
            }]
        };
        let none: Vec<AgentOverride> = Vec::new();

        assert_ne!(
            overlay_fingerprint(&[], &none, &[]),
            overlay_fingerprint(&[], &edit(Some("chat-v2"), None), &[]),
            "a model re-bind must move the fingerprint or it is ignored until restart"
        );
        assert_ne!(
            overlay_fingerprint(&[], &edit(Some("chat-v2"), None), &[]),
            overlay_fingerprint(&[], &edit(None, Some("acp")), &[]),
            "a harness re-bind must move it too"
        );
        assert_ne!(
            overlay_fingerprint(&[], &none, &[]),
            overlay_fingerprint(&[], &edit(Some(""), None), &[]),
            "an explicit model clear must not hash like an untouched teammate"
        );
        // The same edit twice → the same fingerprint (no spurious rebuild).
        assert_eq!(
            overlay_fingerprint(&[], &edit(Some("chat-v2"), None), &[]),
            overlay_fingerprint(&[], &edit(Some("chat-v2"), None), &[])
        );
    }

    /// Choosing or clearing a face writes an `AgentOverride` row whose only set
    /// field is `avatar` (a teammate with no other override). The fingerprints
    /// hash what a teammate *is*, never its face, so such a row must not move
    /// either fingerprint — otherwise a purely cosmetic change would rebuild the
    /// roster and drop every live agent session (issue #1676 review note).
    #[test]
    fn overlay_fingerprint_ignores_an_avatar_only_edit() {
        use crate::ports::types::AgentOverride;
        let avatar_only = |avatar: &str| {
            vec![AgentOverride {
                agent_id: "ceo".into(),
                avatar: Some(avatar.to_string()),
                ..Default::default()
            }]
        };
        let none: Vec<AgentOverride> = Vec::new();

        // Choosing a face for a teammate with no other override writes a row
        // whose only set field is `avatar`. That is not a persona change — the
        // harness reads nothing from the face — so it must not move the
        // fingerprint.
        assert_eq!(
            overlay_fingerprint(&[], &none, &[]),
            overlay_fingerprint(&[], &avatar_only("tiny:robot"), &[]),
            "an avatar-only row must not move the fingerprint"
        );
        // Clearing the face drops the row entirely (`clear_agent_avatar` →
        // `retain_nonempty_agent_edits`), which must not move it either.
        assert_eq!(
            overlay_fingerprint(&[], &avatar_only("tiny:robot"), &[]),
            overlay_fingerprint(&[], &none, &[]),
            "clearing an avatar-only row must not move the fingerprint"
        );
        // The filter is narrow: a real persona edit still moves the
        // fingerprint, even when the same teammate also carries a face.
        let edited = || {
            vec![AgentOverride {
                agent_id: "ceo".into(),
                role: Some("Chief".into()),
                avatar: Some("tiny:robot".into()),
                ..Default::default()
            }]
        };
        assert_ne!(
            overlay_fingerprint(&[], &none, &[]),
            overlay_fingerprint(&[], &edited(), &[]),
            "a real persona edit must still move the fingerprint"
        );
        // The filter is narrow the other way too: a row that changed only the
        // routing — `model` or `harness` with nothing else set — is not a face
        // change. The harness reads those fields when it binds a teammate, so
        // such a row must move the fingerprint or the old binding survives
        // until restart (codex review note).
        let routing = || {
            vec![AgentOverride {
                agent_id: "ceo".into(),
                model: Some("claude-opus-4-5".into()),
                ..Default::default()
            }]
        };
        assert_ne!(
            overlay_fingerprint(&[], &none, &[]),
            overlay_fingerprint(&[], &routing(), &[]),
            "a routing-only edit must still move the fingerprint"
        );
        let harness_routing = || {
            vec![AgentOverride {
                agent_id: "ceo".into(),
                harness: Some("external".into()),
                ..Default::default()
            }]
        };
        assert_ne!(
            overlay_fingerprint(&[], &none, &[]),
            overlay_fingerprint(&[], &harness_routing(), &[]),
            "a harness-only edit must still move the fingerprint"
        );
    }

    /// A removal has to move the same axis: a retired teammate left in a cached
    /// roster would keep taking turns and keep receiving delegations after the
    /// console said it was gone — the sharpest form of the staleness this axis
    /// exists to prevent.
    #[test]
    fn overlay_fingerprint_moves_when_a_teammate_is_retired() {
        assert_ne!(
            overlay_fingerprint(&[], &[], &[]),
            overlay_fingerprint(&[], &[], &["ceo".to_string()]),
            "removing a teammate must move the fingerprint or it keeps running until restart"
        );
        assert_ne!(
            overlay_fingerprint(&[], &[], &["ceo".to_string()]),
            overlay_fingerprint(&[], &[], &["ceo".to_string(), "engineer".to_string()]),
            "and removing a second one must move it again"
        );
        // Re-recording the same removal changes nothing, which is what
        // `retire_agent`'s idempotence buys: no rebuild, no dropped sessions.
        assert_eq!(
            overlay_fingerprint(&[], &[], &["ceo".to_string()]),
            overlay_fingerprint(&[], &[], &["ceo".to_string()])
        );
    }

    /// Re-setting the same tier does not rebuild the roster.
    ///
    /// Attribution is structurally absent from `Policy` — a re-save of the same
    /// tier writes the same effective values, so the fingerprint cannot move
    /// and live agent sessions are not dropped for a change no agent can
    /// observe (the same reason `budget_fingerprint` omits attribution). The
    /// inverted guard lives in `a_manifest_policy_edit_rebuilds_the_roster_with_no_override`
    /// below: a manifest `[policy]` edit with no override in between MUST move
    /// the key.
    #[test]
    fn re_setting_the_same_tier_does_not_rebuild_the_roster() {
        assert_eq!(
            effective_policy_fingerprint(&fp_policy("auto", &["payment.send"], Some(25.0), None)),
            effective_policy_fingerprint(&fp_policy("auto", &["payment.send"], Some(25.0), None)),
            "re-setting the same tier must not move the fingerprint"
        );
    }

    /// In-memory `ContextStore` so `OcMemory` has somewhere to land.
    #[derive(Default)]
    struct MockContext {
        chunks: StdMutex<Vec<(ChunkAddr, ContextChunk)>>,
        // Monotonic, NOT chunks.len(): a delete shrinks the vec, and a
        // len-derived addr would then collide with a surviving chunk's.
        next_addr: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl ContextStore for MockContext {
        async fn put(&self, _id: &CompanyId, chunk: ContextChunk) -> crate::Result<ChunkAddr> {
            let n = self
                .next_addr
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut guard = self.chunks.lock().unwrap();
            let addr = ChunkAddr::new(format!("addr-{n}"));
            guard.push((addr.clone(), chunk));
            Ok(addr)
        }
        async fn list(&self, _id: &CompanyId, prefix: &str) -> crate::Result<Vec<ChunkMeta>> {
            let guard = self.chunks.lock().unwrap();
            Ok(guard
                .iter()
                .filter(|(_, c)| c.label.starts_with(prefix))
                .map(|(addr, c)| ChunkMeta {
                    addr: addr.clone(),
                    label: c.label.clone(),
                    len: c.body.len(),
                    // The mock does not model store time; these tests exercise
                    // the harness, not the Brain's freshness stat.
                    stored_at_millis: 0,
                })
                .collect())
        }
        async fn peek(
            &self,
            _id: &CompanyId,
            addr: &ChunkAddr,
            _range: Option<std::ops::Range<usize>>,
        ) -> crate::Result<String> {
            let guard = self.chunks.lock().unwrap();
            Ok(guard
                .iter()
                .find(|(a, _)| a == addr)
                .map(|(_, c)| c.body.clone())
                .unwrap_or_default())
        }
        async fn delete(&self, _id: &CompanyId, addr: &ChunkAddr) -> crate::Result<bool> {
            let mut guard = self.chunks.lock().unwrap();
            let before = guard.len();
            guard.retain(|(a, _)| a != addr);
            Ok(guard.len() < before)
        }
        async fn delete_label(
            &self,
            _id: &CompanyId,
            addr: &ChunkAddr,
            label: &str,
        ) -> crate::Result<bool> {
            let mut guard = self.chunks.lock().unwrap();
            let before = guard.len();
            guard.retain(|(a, c)| !(a == addr && c.label == label));
            Ok(guard.len() < before)
        }
        async fn search(
            &self,
            _id: &CompanyId,
            query: &str,
            limit: usize,
        ) -> crate::Result<Vec<ChunkHit>> {
            let guard = self.chunks.lock().unwrap();
            Ok(guard
                .iter()
                .filter(|(_, c)| c.body.contains(query))
                .take(limit)
                .map(|(addr, c)| ChunkHit {
                    addr: addr.clone(),
                    snippet: c.body.clone(),
                    score: 1.0,
                })
                .collect())
        }
    }

    /// The mock's addresses are monotonic, not len-derived: a delete must not
    /// make the next put reuse a surviving chunk's address (len-derived bug:
    /// delete `addr-0` of `[addr-0, addr-1]`, and the next put minted
    /// `addr-1` again — a later delete of `addr-1` then removed both rows).
    #[tokio::test]
    async fn mock_context_addresses_survive_deletion_without_reuse() {
        let ctx = MockContext::default();
        let company = CompanyId::new("acme");
        let chunk = |label: &str| ContextChunk {
            label: label.into(),
            body: label.into(),
        };
        let first = ctx.put(&company, chunk("l/0")).await.unwrap();
        let second = ctx.put(&company, chunk("l/1")).await.unwrap();
        assert!(
            ctx.delete(&company, &first).await.unwrap(),
            "first delete removes the row"
        );
        assert!(
            !ctx.delete(&company, &first).await.unwrap(),
            "repeat delete of the same addr finds nothing"
        );
        let third = ctx.put(&company, chunk("l/2")).await.unwrap();
        assert_ne!(
            third.as_ref() as &str,
            second.as_ref(),
            "a post-delete put must not reuse a surviving address"
        );
        assert!(ctx.delete(&company, &second).await.unwrap());
        let left = ctx.list(&company, "l/").await.unwrap();
        assert_eq!(left.len(), 1, "only the newest row remains: {left:?}");
    }

    /// `CompanyStore` that records what the cost hook appends.
    #[derive(Default)]
    struct RecordingStore {
        ledger: StdMutex<Vec<LedgerEntry>>,
    }

    #[async_trait]
    impl CompanyStore for RecordingStore {
        async fn load(&self, _id: &CompanyId) -> crate::Result<Option<CompanyRecord>> {
            Ok(None)
        }
        async fn save(&self, _record: &CompanyRecord) -> crate::Result<()> {
            Ok(())
        }
        async fn list(&self) -> crate::Result<Vec<CompanySummary>> {
            Ok(Vec::new())
        }
        async fn append_ledger(&self, _id: &CompanyId, entry: LedgerEntry) -> crate::Result<()> {
            self.ledger.lock().unwrap().push(entry);
            Ok(())
        }
    }

    /// `CompanyStore` whose `append_ledger` always fails — the "ledger write
    /// that also failed" `turn_result_after_metering`'s own doc names, and
    /// the fixture `a_metering_failure_does_not_swallow_a_budget_pause_marker`
    /// needs to force `meter_turn_costs` into its `Err` arm on a turn that
    /// otherwise succeeded (Codex review, PR #2053).
    #[derive(Default)]
    struct FailingLedgerStore;

    #[async_trait]
    impl CompanyStore for FailingLedgerStore {
        async fn load(&self, _id: &CompanyId) -> crate::Result<Option<CompanyRecord>> {
            Ok(None)
        }
        async fn save(&self, _record: &CompanyRecord) -> crate::Result<()> {
            Ok(())
        }
        async fn list(&self) -> crate::Result<Vec<CompanySummary>> {
            Ok(Vec::new())
        }
        async fn append_ledger(&self, _id: &CompanyId, _entry: LedgerEntry) -> crate::Result<()> {
            Err(OpenCompanyError::Harness(
                "scripted ledger outage".to_string(),
            ))
        }
    }

    /// Records usage samples so a zero-usage turn can be asserted inert.
    #[derive(Default)]
    struct RecordingMeter {
        samples: StdMutex<Vec<UsageSample>>,
    }

    #[async_trait]
    impl UsageMeter for RecordingMeter {
        async fn record(&self, _company: &CompanyId, sample: &UsageSample) -> crate::Result<()> {
            self.samples.lock().unwrap().push(sample.clone());
            Ok(())
        }
        /// Honours `since_millis`, per the port contract ("every sample at or
        /// after `since_millis`"). The per-agent daily cap (issue #304) is a
        /// windowed read, so a double that returned everything regardless would
        /// make the day-rollover test pass against any boundary the code
        /// computed — including none at all.
        async fn query(&self, _company: &CompanyId, since: u64) -> crate::Result<Vec<UsageSample>> {
            Ok(self
                .samples
                .lock()
                .unwrap()
                .iter()
                .filter(|sample| sample.at_millis >= since)
                .cloned()
                .collect())
        }
    }

    /// A meter whose reads always fail — for the dispatch gate's fail-open pin.
    struct FailingMeter;

    #[async_trait]
    impl UsageMeter for FailingMeter {
        async fn record(&self, _company: &CompanyId, _sample: &UsageSample) -> crate::Result<()> {
            Ok(())
        }
        async fn query(
            &self,
            _company: &CompanyId,
            _since: u64,
        ) -> crate::Result<Vec<UsageSample>> {
            Err(OpenCompanyError::Store("meter unavailable".into()))
        }
    }

    fn manifest() -> CompanyManifest {
        toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "ceo"
role = "Chief Executive"
description = "Sets direction."

[[agent]]
id = "engineer"
role = "Engineer"
description = "Builds the product."
"#,
        )
        .expect("valid manifest")
    }

    fn record() -> CompanyRecord {
        CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: CompanyId::new("acme"),
            manifest: manifest(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            setup: None,
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            overlay_policy: None,
            overlay_tool_grants: None,
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
            name_confirmed: false,
            activation_completed_at: None,
            created_at_millis: None,
        }
    }

    struct Fixture {
        deps: HarnessDeps,
        store: Arc<RecordingStore>,
        meter: Arc<RecordingMeter>,
        _dir: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(RecordingStore::default());
        let meter = Arc::new(RecordingMeter::default());
        Fixture {
            deps: HarnessDeps {
                notifications: None,
                ledgers: None,
                ledger_registry: Default::default(),
                provider: Arc::new(MockProvider::new("mock: ")),
                provider_slug: "mock".to_string(),
                serves: None,
                context: Arc::new(MockContext::default()),
                store: store.clone(),
                meter: Some(meter.clone()),
                workspace_root: dir.path().to_path_buf(),
                mcp_home: None,
                workspace_git_enabled: false,
                audit_root: dir.path().to_path_buf(),
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
                workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
                mcp_failures: McpFailureQueue::default(),
                pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
                workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
                run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
                run_output_store: None,
                workflow_runs: None,
                deep_trace: None,
                workflow_revisions: None,
                approval_requests: ApprovalRequestQueue::default(),
                secrets: None,
                web_allowed_domains: Vec::new(),
                capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
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
                search: None,
                tenant_search: None,
                workspace: None,
            },
            store,
            meter,
            _dir: dir,
        }
    }

    #[tokio::test]
    async fn roster_builds_every_manifest_agent() {
        let fx = fixture();
        let roster =
            build_roster(&record(), &fx.deps, &[], &HashMap::new()).expect("roster builds");
        let ids: Vec<_> = roster.iter().map(|a| a.agent_id.as_str()).collect();
        assert_eq!(ids, vec!["ceo", "engineer"]);
        assert_eq!(roster[0].role, "Chief Executive");
    }

    /// Context routing: the resolution that feeds a persona, and the fingerprint
    /// that decides whether an edit reaches the next turn.
    mod routed_context {
        use super::*;
        use crate::ports::workspace::{NodeKind, WorkspaceNode, WorkspaceOrigin};

        fn docs(entries: &[(&str, &[(&str, &str)])]) -> HashMap<String, Vec<(String, String)>> {
            entries
                .iter()
                .map(|(agent, documents)| {
                    (
                        (*agent).to_string(),
                        documents
                            .iter()
                            .map(|(p, b)| ((*p).to_string(), (*b).to_string()))
                            .collect(),
                    )
                })
                .collect()
        }

        /// The property the whole axis exists for. The routing table is manifest
        /// data and does not move when an operator edits a note, so a
        /// name-only hash would leave the edit invisible until a restart.
        #[test]
        fn the_fingerprint_moves_when_a_documents_body_changes() {
            let before = routed_context_fingerprint(&docs(&[("ceo", &[("brief.md", "old")])]));
            let after = routed_context_fingerprint(&docs(&[("ceo", &[("brief.md", "new")])]));
            assert_ne!(
                before, after,
                "an edited routed note must rebuild the roster"
            );
        }

        /// A `HashMap` has no order, so an order-sensitive hash would drop every
        /// live agent session on a rebuild that changed nothing.
        #[test]
        fn the_fingerprint_is_stable_across_map_iteration_order() {
            let one = docs(&[
                ("ceo", &[("brief.md", "b")]),
                ("engineer", &[("claims.md", "c")]),
            ]);
            let two = docs(&[
                ("engineer", &[("claims.md", "c")]),
                ("ceo", &[("brief.md", "b")]),
            ]);
            assert_eq!(
                routed_context_fingerprint(&one),
                routed_context_fingerprint(&two)
            );
        }

        /// Renaming a document is a real change even when its text is identical:
        /// the persona quotes the path as the section heading.
        #[test]
        fn the_fingerprint_moves_when_a_document_is_renamed() {
            let before = routed_context_fingerprint(&docs(&[("ceo", &[("brief.md", "same")])]));
            let after = routed_context_fingerprint(&docs(&[("ceo", &[("GOAL.md", "same")])]));
            assert_ne!(before, after);
        }

        /// A company with no workspace store keeps a stable fingerprint and never
        /// rebuilds on this axis — the pre-routing behaviour exactly.
        #[tokio::test]
        async fn no_workspace_store_resolves_to_nothing() {
            let fx = fixture();
            assert!(fx.deps.workspace.is_none(), "fixture has no store wired");

            let pool = HarnessPool::new();
            let routed = pool.resolve_routed_context(&record(), &fx.deps, &[]).await;
            assert!(routed.is_empty(), "{routed:?}");
            assert_eq!(
                routed_context_fingerprint(&routed),
                routed_context_fingerprint(&HashMap::new()),
                "a company that routes nothing must not rebuild on this axis"
            );
        }

        /// The real path: a routed document that exists in the tree is read and
        /// keyed to the agent whose manifest asked for it.
        #[tokio::test]
        async fn a_routed_document_is_resolved_per_agent() {
            let dir = tempfile::tempdir().expect("tempdir");
            let ws: Arc<dyn crate::ports::WorkspaceStore> =
                Arc::new(crate::store::FsOps::new(dir.path()));
            let company = CompanyId::new("acme");
            ws.create(
                &company,
                &WorkspaceNode {
                    id: "n-brief".to_string(),
                    name: "brief.md".to_string(),
                    kind: NodeKind::File,
                    parent_id: None,
                    updated_at_millis: 1,
                    created_by: WorkspaceOrigin::Operator,
                    updated_by: WorkspaceOrigin::Operator,
                    mime: None,
                    size: None,
                    sha256: None,
                    adopted: false,
                },
                Some("What the company established."),
            )
            .await
            .expect("create");

            let mut fx = fixture();
            fx.deps.workspace = Some(ws);

            let pool = HarnessPool::new();
            let routed = pool.resolve_routed_context(&record(), &fx.deps, &[]).await;

            // Both fixture agents default to the `reasoning` row, which routes
            // BRIEF — so both resolve it, and neither invents the notes that do
            // not exist in the tree.
            for agent in ["ceo", "engineer"] {
                let documents = routed
                    .get(agent)
                    .unwrap_or_else(|| panic!("no routed documents for {agent}: {routed:?}"));
                assert_eq!(
                    documents,
                    &vec![(
                        "brief.md".to_string(),
                        "What the company established.".to_string()
                    )],
                    "{agent}"
                );
            }
        }
    }

    /// The roster builds end-to-end with the skill read surface wired: the
    /// effective set materializes, the read tools build, and the catalogue folds
    /// into the persona — all without error — and the scratch tree lands under
    /// the agent's workspace root.
    #[tokio::test]
    async fn roster_builds_with_skill_surface_wired() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = tempfile::tempdir().expect("source");
        let skill_dir = source.path().join("skills").join("web-research");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: Web Research\ndescription: Answer a question\n---\n\n# Web Research\n",
        )
        .unwrap();

        let deps = HarnessDeps {
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            serves: None,
            context: Arc::new(MockContext::default()),
            store: Arc::new(RecordingStore::default()),
            meter: None,
            workspace_root: dir.path().to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: dir.path().to_path_buf(),
            model_override: None,
            tasks: None,
            artifacts: None,
            skills: None,
            skills_source_dir: Some(source.path().to_path_buf()),
            skills_registry: std::sync::Arc::from([]),
            default_mcp_servers: Vec::new(),
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: DelegationQueue::default(),
            workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_runs: None,
            deep_trace: None,
            workflow_revisions: None,
            approval_requests: ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
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
            search: None,
            tenant_search: None,
            workspace: None,
        };

        let roster = build_roster(&record(), &deps, &[], &HashMap::new())
            .expect("roster builds with skills");
        assert_eq!(roster.len(), 2);
        // The scratch skill tree was materialized for the first roster agent.
        assert!(
            dir.path()
                .join("acme")
                .join("ceo")
                .join("skill-catalog")
                .join("skills")
                .join("web-research")
                .join("SKILL.md")
                .is_file(),
            "the effective skill bundle should be materialized under the agent workspace"
        );
    }

    /// Issue #71 — an operator/orchestrator-added overlay teammate is promoted
    /// into a real, addressable roster agent (not just a console row).
    #[tokio::test]
    async fn overlay_agent_is_built_as_a_real_roster_agent() {
        let fx = fixture();
        let mut rec = record();
        rec.overlay_agents.push(OverlayAgent {
            id: "growth".into(),
            name: "Jamie".into(),
            role: "Growth Lead".into(),
            description: Some("Owns acquisition experiments.".into()),
            tools: None,
            model: None,
            harness: None,
        });

        let roster = build_roster(&rec, &fx.deps, &[], &HashMap::new()).expect("roster builds");
        let ids: Vec<_> = roster.iter().map(|a| a.agent_id.as_str()).collect();
        assert_eq!(ids, vec!["ceo", "engineer", "growth"], "got {ids:?}");
        let overlay_agent = roster
            .iter()
            .find(|a| a.agent_id == "growth")
            .expect("overlay teammate present in roster");
        assert_eq!(overlay_agent.role, "Growth Lead");
    }

    /// The roster is built from the teammate an operator has since edited, not
    /// from the row `company.toml` declared. Without this the console would save
    /// a rename that nothing running ever heard about — the edit would be
    /// visible on the Team page and absent from every turn the agent took.
    #[tokio::test]
    async fn a_console_edit_of_a_manifest_teammate_reaches_the_built_roster() {
        let fx = fixture();
        let mut rec = record();
        rec.upsert_agent_override(crate::ports::types::AgentOverride {
            agent_id: "ceo".into(),
            role: Some("Chief Vibes".into()),
            ..Default::default()
        });

        let roster = build_roster(&rec, &fx.deps, &[], &HashMap::new()).expect("roster builds");
        let ceo = roster
            .iter()
            .find(|a| a.agent_id == "ceo")
            .expect("the ceo is still on the roster");
        assert_eq!(ceo.role, "Chief Vibes");
    }

    /// A teammate the operator removed is not built at all — which is what makes
    /// the delete real rather than cosmetic: an agent left in the roster keeps
    /// taking turns and keeps receiving delegations however the Team page reads.
    /// And when the removed teammate was the orchestrator, the role moves to the
    /// next one rather than to somebody who is no longer here.
    #[tokio::test]
    async fn a_retired_manifest_teammate_is_not_built() {
        let fx = fixture();
        let mut rec = record();
        rec.retire_agent("ceo");

        let roster = build_roster(&rec, &fx.deps, &[], &HashMap::new()).expect("roster builds");
        let ids: Vec<_> = roster.iter().map(|a| a.agent_id.as_str()).collect();
        assert_eq!(ids, vec!["engineer"], "got {ids:?}");
        assert_eq!(
            orchestrator::orchestrator_id(&rec.effective_agents()).as_deref(),
            Some("engineer"),
            "the orchestrator must be somebody who is actually on the roster"
        );
    }

    /// A manifest agent always wins an id collision with an overlay teammate —
    /// the version-controlled roster is authoritative.
    #[tokio::test]
    async fn overlay_agent_id_colliding_with_manifest_agent_is_skipped() {
        let fx = fixture();
        let mut rec = record();
        rec.overlay_agents.push(OverlayAgent {
            id: "ceo".into(),
            name: "Impostor".into(),
            role: "Shadow CEO".into(),
            description: None,
            tools: None,
            model: None,
            harness: None,
        });

        let roster = build_roster(&rec, &fx.deps, &[], &HashMap::new()).expect("roster builds");
        let ids: Vec<_> = roster.iter().map(|a| a.agent_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["ceo", "engineer"],
            "the manifest agent wins the id collision, not a duplicate"
        );
        assert_eq!(
            roster[0].role, "Chief Executive",
            "the manifest role survives, not the overlay's"
        );
    }

    /// Issue #686, end to end: the orchestrator adds a teammate whose display
    /// name slugs onto a **manifest** agent's id, and the teammate still shows
    /// up in the built roster.
    ///
    /// This is the failure the suffix exists to prevent, and it only became
    /// reachable when ids started coming from names. `add_agent`'s duplicate
    /// guard compares overlay *names*, so "Engineer" sails past it; an
    /// unsuffixed `engineer` would then be skipped by
    /// [`build_roster`](super::build_roster) as already claimed by the manifest
    /// — saved to the record, never materialised, no error anywhere.
    #[tokio::test]
    async fn a_tool_added_teammate_colliding_with_a_manifest_id_still_joins_the_roster() {
        use openhuman_core::openhuman::tools::Tool;

        use crate::harness::orchestrator::unscoped_add_agent;

        /// A `CompanyStore` that actually holds the record, unlike
        /// `RecordingStore` — `add_agent` has to load what it saves.
        struct SeededStore(StdMutex<CompanyRecord>);

        #[async_trait]
        impl CompanyStore for SeededStore {
            async fn load(&self, _id: &CompanyId) -> crate::Result<Option<CompanyRecord>> {
                Ok(Some(self.0.lock().unwrap().clone()))
            }
            async fn save(&self, record: &CompanyRecord) -> crate::Result<()> {
                *self.0.lock().unwrap() = record.clone();
                Ok(())
            }
            async fn list(&self) -> crate::Result<Vec<CompanySummary>> {
                Ok(Vec::new())
            }
            async fn append_ledger(
                &self,
                _id: &CompanyId,
                _entry: LedgerEntry,
            ) -> crate::Result<()> {
                Ok(())
            }
        }

        let fx = fixture();
        let company = CompanyId::new("acme");
        let store = Arc::new(SeededStore(StdMutex::new(record())));
        let tool = unscoped_add_agent(company.clone(), store.clone());

        let result = tool
            .execute(serde_json::json!({ "name": "Engineer", "role": "Platform" }))
            .await
            .expect("execute");
        assert!(
            !result.is_error,
            "the name guard compares overlay names only"
        );
        assert!(
            result.text().contains("engineer_2"),
            "the orchestrator has to learn the id it can address: {}",
            result.text()
        );

        let saved = store.load(&company).await.unwrap().expect("record");
        assert_eq!(saved.overlay_agents[0].id, "engineer_2");

        let roster = build_roster(&saved, &fx.deps, &[], &HashMap::new()).expect("roster builds");
        let ids: Vec<_> = roster.iter().map(|a| a.agent_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["ceo", "engineer", "engineer_2"],
            "a suffixed id materialises; an unsuffixed one would vanish here"
        );
    }

    /// Issue #551: a roster rebuild writes nothing to the workspace.
    ///
    /// This used to be the feature's second provisioning seam — a teammate
    /// added at runtime (a manifest edit, the console's `add_member`, the
    /// orchestrator's `add_agent`) reaches the harness as a moved overlay
    /// fingerprint, and the folder was minted here. A member folder is no
    /// longer a function of the roster, so joining one is no longer an event
    /// the tree records: the folder appears when the teammate first produces
    /// something, and the two system roots come from boot.
    ///
    /// Pinned as a test because a rebuild that quietly resumed writing would
    /// re-fill the tree with empty folders for teammates who have done nothing
    /// — exactly the noise this change removed.
    #[tokio::test]
    async fn a_roster_rebuild_writes_nothing_to_the_workspace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws: Arc<dyn crate::ports::WorkspaceStore> =
            Arc::new(crate::store::FsOps::new(dir.path()));
        let mut fx = fixture();
        fx.deps.workspace = Some(ws.clone());

        let mut rec = record();
        let pool = HarnessPool::new();
        pool.ensure(&rec, &fx.deps).await.expect("first ensure");
        assert!(
            ws.is_empty(&rec.id).await.expect("is_empty"),
            "the roster build touched the workspace"
        );

        // The runtime-added teammate. The overlay fingerprint moves, so this
        // `ensure` takes the rebuild path rather than the cached fast path.
        rec.overlay_agents.push(OverlayAgent {
            id: "designer".into(),
            name: "Dana".into(),
            role: "Designer".into(),
            description: None,
            tools: None,
            model: None,
            harness: None,
        });
        pool.ensure(&rec, &fx.deps).await.expect("second ensure");

        assert!(
            ws.is_empty(&rec.id).await.expect("is_empty"),
            "the rebuild minted a folder for a teammate that has produced nothing"
        );

        // …and the folder the teammate *does* get is the one it earns by
        // producing something, minted through the lazy seam instead.
        let minted = crate::company::workspace_scaffold::ensure_agent_folder(
            ws.as_ref(),
            &rec.id,
            "designer",
        )
        .await
        .expect("mint");
        let tree = ws.tree(&rec.id).await.expect("tree");
        let mut names: Vec<&str> = tree.iter().map(|n| n.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["agents", "designer"]);
        assert_eq!(
            tree.iter().find(|n| n.id == minted).unwrap().created_by,
            crate::ports::WorkspaceOrigin::Agent {
                id: "designer".to_string()
            },
        );
    }

    #[tokio::test]
    async fn run_executes_a_turn_on_the_openhuman_runtime() {
        let fx = fixture();
        let pool = HarnessPool::new();
        let rec = record();
        pool.ensure(&rec, &fx.deps).await.expect("ensure");

        let reply = pool
            .run(
                &rec.id,
                "ceo",
                "hello-marker",
                &fx.deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect("turn runs")
            .reply;

        assert!(
            reply.contains("hello-marker"),
            "reply should echo the prompt through the agent: {reply:?}"
        );
    }

    #[tokio::test]
    async fn run_stores_outcomes_and_injects_them_into_later_turns() {
        let fx = fixture();
        let pool = HarnessPool::new();
        let rec = record();
        pool.ensure(&rec, &fx.deps).await.expect("ensure");

        // Cold store: nothing to inject on the first turn.
        let first = pool
            .run(
                &rec.id,
                "ceo",
                "alpha task",
                &fx.deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect("first turn")
            .reply;
        assert!(
            !first.contains("Relevant prior work"),
            "a cold turn injects nothing: {first:?}"
        );

        // The outcome was written back under the task-outcome prefix.
        let stored = fx
            .deps
            .context
            .list(&rec.id, memory_loop::OUTCOME_LABEL_PREFIX)
            .await
            .unwrap();
        assert_eq!(stored.len(), 1, "the first turn stores its outcome");

        // Second turn: the prior outcome (its body contains "alpha") is
        // retrieved and injected, so the agent sees the preamble.
        let second = pool
            .run(
                &rec.id,
                "ceo",
                "alpha",
                &fx.deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect("second turn")
            .reply;
        assert!(
            second.contains("Relevant prior work"),
            "the second turn injects the retrieved outcome: {second:?}"
        );

        let stored = fx
            .deps
            .context
            .list(&rec.id, memory_loop::OUTCOME_LABEL_PREFIX)
            .await
            .unwrap();
        assert_eq!(stored.len(), 2, "the second turn stores its outcome too");
    }

    /// Recall is driven by **what the operator typed**, not by the briefings
    /// this turn folded onto it.
    ///
    /// The composed message can carry the open-work briefing, the settled-work
    /// digest, the thread index or attachment markers. Those are for the model
    /// to read; searching on them makes the query something nobody asked. Under
    /// this store's substring matching that costs the recall outright — any
    /// briefing at all and nothing matches — and under a vector store it drifts
    /// instead, toward whatever the briefing happens to name. The settled digest
    /// is a list of finished card titles, so a conversation that had just closed
    /// some work pulled *that* work in, and the bias grew with every card that
    /// finished.
    ///
    /// Found by the `orchestration-simulation` E2E, which went red the moment
    /// two cards settled in the conversation it drives (#1890 review).
    #[tokio::test]
    async fn recall_searches_the_operators_words_not_the_briefings() {
        let fx = fixture();
        let pool = HarnessPool::new();
        let rec = record();
        pool.ensure(&rec, &fx.deps).await.expect("ensure");

        // Turn one stores an outcome whose body carries "alpha".
        pool.run(
            &rec.id,
            "ceo",
            "alpha task",
            &fx.deps,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect("first turn");

        // Turn two asks the same thing, with a briefing folded on — the shape
        // every turn takes once a card has settled in the conversation.
        let briefed = format!(
            "alpha{} has finished — this is where each card landed:\n- something else\n]",
            crate::runtime::cycle::SETTLED_WORK_ANNOTATION
        );
        let second = pool
            .run(
                &rec.id,
                "ceo",
                &briefed,
                &fx.deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect("second turn")
            .reply;

        assert!(
            second.contains("Relevant prior work"),
            "the operator asked about alpha, so alpha is recalled — the briefing \
             appended after their words must not change what is searched for: {second:?}"
        );
    }

    /// A pool serving one named harness builds only the agents bound to it.
    ///
    /// This is what makes one-pool-per-harness affordable: without the filter a
    /// ten-agent roster on three harnesses would stand up thirty live agents,
    /// each holding a model client, to use ten.
    #[tokio::test]
    async fn a_scoped_pool_builds_only_the_agents_it_serves() {
        let fx = fixture();
        let rec = record();
        assert!(
            rec.manifest.agents.len() >= 2,
            "the fixture must have someone to leave out"
        );

        // Unfiltered: the whole roster, exactly as before this field existed.
        let pool = HarnessPool::new();
        pool.ensure(&rec, &fx.deps).await.expect("ensure");
        let all = pool.agent_ids(&rec.id).await;
        assert_eq!(all.len(), rec.manifest.agents.len());

        // Scoped to one agent: only that one is built.
        let mut scoped = fixture();
        scoped.deps.serves = Some(HashSet::from(["ceo".to_string()]));
        let pool = HarnessPool::new();
        pool.ensure(&rec, &scoped.deps).await.expect("ensure");
        assert_eq!(pool.agent_ids(&rec.id).await, vec!["ceo".to_string()]);
    }

    #[tokio::test]
    async fn ensure_is_idempotent() {
        let fx = fixture();
        let pool = HarnessPool::new();
        let rec = record();
        pool.ensure(&rec, &fx.deps).await.expect("first ensure");
        pool.ensure(&rec, &fx.deps).await.expect("second ensure");
        assert_eq!(pool.resident_companies().await, 1);
    }

    /// Issue #1113: a live memory-engine swap must not leave the cached roster
    /// reading/writing the deselected engine until a restart.
    ///
    /// The pool cannot see a swap itself — the replacement ports arrive on the
    /// builder — so [`RuntimeBuilder::build`] calls
    /// [`rebind_memory_engine`](Self::rebind_memory_engine) on every build. This
    /// test drives that contract directly: an unchanged selection keeps the
    /// roster (the ordinary issue #290 fast path), a changed one drops it, and
    /// the next [`ensure`](Self::ensure) folds the replacement context store
    /// into the rebuilt roster's agents — which a turn then demonstrably reads.
    #[tokio::test]
    async fn a_swapped_memory_engine_drops_the_roster_and_reads_the_replacement_store() {
        let fx = fixture();
        let pool = HarnessPool::new();
        let rec = record();

        // Boot: the company is on the base backend (`None`), and the roster is
        // built over the boot-time context store.
        assert!(
            pool.rebind_memory_engine(&rec.id, None).await,
            "first record has nothing to differ from"
        );
        pool.ensure(&rec, &fx.deps).await.expect("boot ensure");
        assert_eq!(
            pool.resident_companies().await,
            1,
            "roster resident after boot"
        );
        assert_eq!(
            pool.memory_engine(&rec.id).await,
            None,
            "selection recorded as the base backend"
        );

        // A rebuild that re-applies the same engine selection is a no-op (the
        // issue #290 fast path): the roster survives, conversation intact.
        assert!(
            pool.rebind_memory_engine(&rec.id, None).await,
            "unchanged selection keeps the roster"
        );
        assert_eq!(
            pool.resident_companies().await,
            1,
            "roster survives a no-op rebuild"
        );

        // A live swap to a provider engine: the selection changes, so the cached
        // roster must drop for the next `ensure` to rebuild over the replacement
        // ports — otherwise the agents keep reading the deselected engine.
        assert!(
            !pool.rebind_memory_engine(&rec.id, Some(0x1113_0001)).await,
            "changed selection drops the roster"
        );
        assert_eq!(
            pool.resident_companies().await,
            0,
            "roster invalidated on swap"
        );

        // Seed the replacement context store and ensure over it: the rebuilt
        // agent must read the new engine, not the deselected one.
        let replacement = Arc::new(MockContext::default());
        let query = "who approved the overtime";
        replacement
            .put(
                &rec.id,
                ContextChunk {
                    label: "prior/outcome".into(),
                    body: format!("REPLACEMENT-ENGINE: {query} on Tuesday"),
                },
            )
            .await
            .expect("seed the replacement store");
        let mut swapped = fixture();
        swapped.deps.context = replacement.clone();
        pool.ensure(&rec, &swapped.deps)
            .await
            .expect("ensure after swap");
        let reply = pool
            .run(
                &rec.id,
                "ceo",
                query,
                &swapped.deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect("turn after swap")
            .reply;
        assert!(
            reply.contains("REPLACEMENT-ENGINE"),
            "the rebuilt roster reads the replacement store, not the deselected one; got: {reply}"
        );
        assert_eq!(
            pool.memory_engine(&rec.id).await,
            Some(0x1113_0001),
            "selection recorded as the provider engine"
        );

        // And the reverse swap (back to the base backend — the
        // `with_memory_overlay_cleared` path): the provider-built roster must
        // drop the same way, and the next ensure must read the boot store again.
        assert!(
            !pool.rebind_memory_engine(&rec.id, None).await,
            "reverse swap also drops the roster"
        );
        assert_eq!(
            pool.resident_companies().await,
            0,
            "provider-built roster invalidated on the way back"
        );
        pool.ensure(&rec, &fx.deps)
            .await
            .expect("ensure after reverse swap");
        let reply = pool
            .run(
                &rec.id,
                "ceo",
                query,
                &fx.deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect("turn after reverse swap")
            .reply;
        assert!(
            !reply.contains("REPLACEMENT-ENGINE"),
            "the rebuilt roster reads the boot store again, not the deselected provider; got: {reply}"
        );
        // Sanity: the boot store itself has no seed for this query, so the
        // absence above is meaningful rather than a hit-free query.
        assert!(
            !reply.contains("SECRET-PAYROLL-REVIEW"),
            "sanity: the boot store holds no marker for this query"
        );
    }

    #[tokio::test]
    async fn turns_are_serialised_and_history_survives() {
        let fx = fixture();
        let pool = HarnessPool::new();
        let rec = record();
        pool.ensure(&rec, &fx.deps).await.expect("ensure");

        pool.run(
            &rec.id,
            "ceo",
            "first",
            &fx.deps,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect("first turn");
        let second = pool
            .run(
                &rec.id,
                "ceo",
                "second",
                &fx.deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect("second turn")
            .reply;

        assert!(second.contains("second"));
    }

    /// Issue #416 — a confined turn reaches the company's memory neither on the
    /// way in nor on the way out.
    ///
    /// The control half is what makes this a test rather than an assertion of
    /// absence: the SAME message on the ordinary roster path pulls the seeded
    /// chunk into the prompt (the mock provider echoes what it was sent, so the
    /// injection is visible in the reply), and writes the turn back. The
    /// confined path does neither, from the same store, in the same test.
    #[tokio::test]
    async fn a_confined_turn_neither_reads_nor_writes_company_memory() {
        let context = Arc::new(MockContext::default());
        let mut fx = fixture();
        fx.deps.context = context.clone();
        let pool = HarnessPool::new();
        let rec = record();
        pool.ensure(&rec, &fx.deps).await.expect("ensure");

        // A prior outcome sitting in the company's memory. The mock store
        // matches a chunk whose BODY contains the query, and retrieve→inject
        // queries with the whole message — so a body built around the message is
        // what a hit looks like here.
        let question = "why did it fail";
        context
            .put(
                &rec.id,
                ContextChunk {
                    label: "prior/outcome".into(),
                    body: format!("SECRET-PAYROLL-REVIEW: {question} on Monday"),
                },
            )
            .await
            .expect("seed the company's memory");
        let seeded = context.chunks.lock().unwrap().len();

        // Control: the ordinary path injects the hit and writes the turn back.
        let ordinary = pool
            .run(
                &rec.id,
                "ceo",
                question,
                &fx.deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect("the ordinary turn runs")
            .reply;
        assert!(
            ordinary.contains("SECRET-PAYROLL-REVIEW"),
            "the retrieve→inject step must be live for this test to mean anything: {ordinary}"
        );
        assert!(
            context.chunks.lock().unwrap().len() > seeded,
            "the ordinary path writes its outcome back to company memory"
        );

        let before_confined = context.chunks.lock().unwrap().len();
        let confined = pool
            .run_confined(
                &rec.id,
                "Acme",
                question,
                &fx.deps,
                Some("workflow-copilot:weekly_report"),
                &confine::Confinement::workflow("weekly_report"),
            )
            .await
            .expect("the confined turn runs")
            .reply;

        assert!(
            confined.contains(question),
            "the confined turn still answers the question it was asked: {confined}"
        );
        assert!(
            !confined.contains("SECRET-PAYROLL-REVIEW"),
            "a confined turn must not be handed company memory: {confined}"
        );
        assert_eq!(
            context.chunks.lock().unwrap().len(),
            before_confined,
            "a confined turn must leave nothing behind for a later turn to retrieve"
        );
    }

    /// The confined agent is not on the roster, so nothing can address it: a
    /// dispatch, a desk hand-off or a `chat` naming it is an unknown agent, the
    /// same as any other name that is not a teammate.
    #[tokio::test]
    async fn the_confined_agent_is_not_addressable() {
        let fx = fixture();
        let pool = HarnessPool::new();
        let rec = record();
        pool.ensure(&rec, &fx.deps).await.expect("ensure");

        let err = pool
            .run(
                &rec.id,
                confine::CONFINED_AGENT_ID,
                "hi",
                &fx.deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect_err("the confined agent is not a roster agent");
        assert!(
            matches!(err, OpenCompanyError::InvalidRequest(_)),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn unknown_agent_is_invalid_request() {
        let fx = fixture();
        let pool = HarnessPool::new();
        let rec = record();
        pool.ensure(&rec, &fx.deps).await.expect("ensure");

        let err = pool
            .run(
                &rec.id,
                "nobody",
                "hi",
                &fx.deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect_err("unknown agent rejected");
        assert!(
            matches!(err, OpenCompanyError::InvalidRequest(_)),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn unknown_company_is_not_found() {
        let fx = fixture();
        let pool = HarnessPool::new();
        let err = pool
            .run(
                &CompanyId::new("ghost"),
                "ceo",
                "hi",
                &fx.deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect_err("unknown company rejected");
        assert!(
            matches!(err, OpenCompanyError::CompanyNotFound(_)),
            "{err:?}"
        );
    }

    // --- Workspace-ensure log edge-triggering (issue #449) -------------------

    /// The whole transition table, exhaustively: a broken volume must produce
    /// one error line and then nothing, and a recovery must be announced once.
    #[test]
    fn workspace_report_is_edge_triggered() {
        let mut failing: HashSet<&str> = HashSet::new();

        // First failure speaks.
        assert_eq!(
            workspace_report(&mut failing, &"a", true),
            WorkspaceReport::Failed
        );
        // Every repeat is silent — this is the flood #449 is about.
        for _ in 0..100 {
            assert_eq!(
                workspace_report(&mut failing, &"a", true),
                WorkspaceReport::StillFailing
            );
        }
        // Recovery speaks exactly once.
        assert_eq!(
            workspace_report(&mut failing, &"a", false),
            WorkspaceReport::Recovered
        );
        assert_eq!(
            workspace_report(&mut failing, &"a", false),
            WorkspaceReport::StillHealthy
        );
        // A healthy agent that was never failing says nothing on its first
        // attempt either — a working workspace has never been worth a line.
        assert_eq!(
            workspace_report(&mut failing, &"never-failed", false),
            WorkspaceReport::StillHealthy
        );
        // And it can fail again later: the edge re-arms.
        assert_eq!(
            workspace_report(&mut failing, &"a", true),
            WorkspaceReport::Failed
        );

        assert!(
            WorkspaceReport::StillFailing.is_silent() && WorkspaceReport::StillHealthy.is_silent(),
            "only the repeats are silent"
        );
        assert!(
            !WorkspaceReport::Failed.is_silent() && !WorkspaceReport::Recovered.is_silent(),
            "both edges must be reported"
        );
    }

    /// Two agents interleaved: one failing, one healthy. Each key's edge is its
    /// own — a second agent's failure must not be swallowed by the first's, and
    /// a second agent's recovery must not clear the first's failure.
    #[test]
    fn workspace_report_tracks_each_key_separately() {
        let mut failing: HashSet<&str> = HashSet::new();

        assert_eq!(
            workspace_report(&mut failing, &"ceo", true),
            WorkspaceReport::Failed
        );
        // A different agent failing is its own first failure, not a repeat.
        assert_eq!(
            workspace_report(&mut failing, &"engineer", true),
            WorkspaceReport::Failed
        );
        assert_eq!(
            workspace_report(&mut failing, &"ceo", true),
            WorkspaceReport::StillFailing
        );
        // One recovers; the other stays failing and stays silent.
        assert_eq!(
            workspace_report(&mut failing, &"engineer", false),
            WorkspaceReport::Recovered
        );
        assert_eq!(
            workspace_report(&mut failing, &"ceo", true),
            WorkspaceReport::StillFailing
        );
        assert_eq!(
            workspace_report(&mut failing, &"ceo", false),
            WorkspaceReport::Recovered
        );
        assert!(failing.is_empty(), "a recovered key leaves no residue");
    }

    /// The real dispatch path against a workspace root that cannot hold a
    /// directory, driven through [`HarnessPool::run`] rather than the helper.
    ///
    /// The root is pointed at a **file**, which makes `create_dir_all` fail
    /// deterministically on every platform (`ENOTDIR` / its Windows equivalent)
    /// without needing permission bits a CI root user would ignore.
    ///
    /// Asserts the reporting state, not the log text: this test binary already
    /// installs a global `tracing` subscriber elsewhere
    /// (`runtime::workflow_scheduler`) and asserts it wins that race, so a
    /// second global capture here would make whichever test lost panic. The
    /// state is what decides whether a line is emitted, so pinning it pins the
    /// line count — three dispatches, one report.
    #[tokio::test]
    async fn a_broken_workspace_root_reports_once_across_repeated_dispatches() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A regular file where the workspace tree is expected.
        let not_a_dir = dir.path().join("workspace-root");
        std::fs::write(&not_a_dir, b"this is a file, not a directory").unwrap();

        let mut fx = fixture();
        fx.deps.workspace_root = not_a_dir.clone();
        let pool = HarnessPool::new();
        let rec = record();
        pool.ensure(&rec, &fx.deps).await.expect("ensure");

        // Sanity: the condition really is a hard, repeatable failure.
        assert!(
            build::ensure_agent_workspace(&not_a_dir, &rec.id, "ceo").is_err(),
            "the test root must actually be unusable, or this proves nothing"
        );

        for turn in 0..3 {
            pool.run(
                &rec.id,
                "ceo",
                "hi",
                &fx.deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .unwrap_or_else(|e| panic!("turn {turn} still runs without a workspace: {e:?}"));
        }

        // The turns ran — a missing workspace is not fatal, which #449 does not
        // change — and the failure is recorded exactly once.
        let failing = pool.workspace_failures.lock().unwrap();
        assert_eq!(
            failing.len(),
            1,
            "one failing agent, tracked once, however many turns it takes"
        );
        assert!(failing.contains(&(rec.id.clone(), "ceo".to_string())));
        drop(failing);

        // The next dispatch after the first is silent: only turn 1 spoke.
        assert_eq!(
            pool.note_workspace_attempt(&rec.id, "ceo", true),
            WorkspaceReport::StillFailing,
            "dispatches after the first must not re-emit the error"
        );
        // And when the volume comes back, one line says so.
        assert_eq!(
            pool.note_workspace_attempt(&rec.id, "ceo", false),
            WorkspaceReport::Recovered
        );
    }

    /// Pins the documented inert-metering contract: until the provider reports
    /// usage, a turn writes neither a ledger entry nor a usage sample.
    #[tokio::test]
    async fn zero_usage_turn_writes_nothing() {
        let fx = fixture();
        let pool = HarnessPool::new();
        let rec = record();
        pool.ensure(&rec, &fx.deps).await.expect("ensure");
        pool.run(
            &rec.id,
            "ceo",
            "hi",
            &fx.deps,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect("turn");

        assert!(fx.store.ledger.lock().unwrap().is_empty());
        assert!(fx.meter.samples.lock().unwrap().is_empty());
    }

    /// B-120, the half that made a founder's console disagree with their bill:
    /// a turn that ends in an error is still written to the ledger and the usage
    /// meter.
    ///
    /// Both writes used to sit below a `?` on the turn — so a wall-clock ceiling
    /// or a provider fault produced no `inference.spend` entry and no
    /// `UsageSample` at all. The spend was not merely mis-displayed on the run:
    /// it was never recorded anywhere, which is why the company-wide Observatory
    /// total agreed that ten minutes of model work had been free.
    #[tokio::test]
    async fn a_failed_turn_is_still_written_to_the_ledger_and_the_meter() {
        let mut fx = fixture();
        fx.deps.provider = Arc::new(
            ScriptedProvider::new(vec![Ok(String::new())])
                .reporting_usage(tinyinference::Usage {
                    input_tokens: 1_200,
                    output_tokens: 340,
                    total_tokens: 1_540,
                    ..Default::default()
                })
                .failing_when_exhausted(),
        );
        let pool = HarnessPool::new();
        let rec = record();
        pool.ensure(&rec, &fx.deps).await.expect("ensure");

        let outcome = pool
            .run(
                &rec.id,
                "ceo",
                "do ten minutes of work",
                &fx.deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await;

        assert!(outcome.is_err(), "the scripted provider stays down");
        let samples = fx.meter.samples.lock().unwrap().clone();
        assert_eq!(
            samples.len(),
            1,
            "the failed turn's tokens must reach the usage meter: {samples:?}"
        );
        assert_eq!(samples[0].input_tokens, 1_200);
        assert_eq!(samples[0].output_tokens, 340);
        assert_eq!(
            fx.store.ledger.lock().unwrap().len(),
            1,
            "and its spend must reach the ledger, or the console and the bill disagree"
        );
    }

    // --- Empty-response turn wrapper ----------------------------------------

    /// A model that plays back a scripted sequence of outcomes, one per
    /// [`invoke`](ChatModel::invoke) call, so the empty-response retry wrapper can
    /// be driven deterministically. `Ok("")` is the transient empty class (the
    /// harness turn raises the empty-response error on a blank assistant reply);
    /// `Err(_)` is a hard error.
    struct ScriptedProvider {
        script: StdMutex<std::collections::VecDeque<Result<String, String>>>,
        calls: std::sync::atomic::AtomicUsize,
        /// Usage stamped on every scripted `Ok` response, when the case needs a
        /// provider that reports any (`None` — the default — mirrors
        /// `MockProvider`, whose replies carry none at all). Only a response
        /// carrying usage makes openhuman publish the live
        /// `TurnCostUpdated` tally the metering path depends on.
        usage: Option<tinyinference::Usage>,
        /// What an exhausted script answers: the default `"exhausted"` reply,
        /// or a permanent error.
        ///
        /// A case that needs the turn to *fail* has to script a provider that
        /// stays failed, because openhuman retries a provider error inside its
        /// own loop — a finite run of `Err` entries is simply consumed and the
        /// turn then succeeds on the fallback reply, which is how this
        /// scripting seam quietly turned a failure case into a passing one.
        fail_when_exhausted: bool,
    }

    impl ScriptedProvider {
        fn new(outcomes: Vec<Result<String, String>>) -> Self {
            Self {
                script: StdMutex::new(outcomes.into_iter().collect()),
                calls: std::sync::atomic::AtomicUsize::new(0),
                usage: None,
                fail_when_exhausted: false,
            }
        }

        /// Report `usage` on every scripted `Ok` response.
        fn reporting_usage(mut self, usage: tinyinference::Usage) -> Self {
            self.usage = Some(usage);
            self
        }

        /// Fail every call past the end of the script, permanently.
        fn failing_when_exhausted(mut self) -> Self {
            self.fail_when_exhausted = true;
            self
        }
    }

    #[async_trait]
    impl ChatModel<()> for ScriptedProvider {
        async fn invoke(
            &self,
            _state: &(),
            _request: ModelRequest,
        ) -> tinyinference::Result<ModelResponse> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let with_usage = |reply: &str| {
                let mut response = ModelResponse::assistant(reply);
                response.usage = self.usage;
                response
            };
            match self.script.lock().unwrap().pop_front() {
                Some(Ok(reply)) => Ok(with_usage(&reply)),
                Some(Err(err)) => Err(tinyinference::Error::Model(err)),
                None if self.fail_when_exhausted => Err(tinyinference::Error::Model(
                    "scripted provider is permanently down".to_string(),
                )),
                None => Ok(with_usage("exhausted")),
            }
        }
    }

    impl HarnessModel for ScriptedProvider {
        fn telemetry_provider_id(&self) -> String {
            "scripted".to_string()
        }
    }

    /// Build a single [`CompanyAgent`] over a scripted provider so the wrapper can
    /// be exercised directly (its retry logic is the unit under test).
    fn scripted_agent(outcomes: Vec<Result<String, String>>) -> (Arc<CompanyAgent>, HarnessDeps) {
        scripted_agent_over(ScriptedProvider::new(outcomes))
    }

    /// As [`scripted_agent`], over an already-configured provider — the seam a
    /// case that needs the provider to *report usage* builds through.
    fn scripted_agent_over(provider: ScriptedProvider) -> (Arc<CompanyAgent>, HarnessDeps) {
        let dir = tempfile::tempdir().expect("tempdir");
        let deps = HarnessDeps {
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            provider: Arc::new(provider),
            provider_slug: "scripted".to_string(),
            serves: None,
            context: Arc::new(MockContext::default()),
            store: Arc::new(RecordingStore::default()),
            meter: None,
            workspace_root: dir.path().to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: dir.path().to_path_buf(),
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
            workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_runs: None,
            deep_trace: None,
            workflow_revisions: None,
            approval_requests: ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
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
            search: None,
            tenant_search: None,
            workspace: None,
        };
        let roster = build_roster(&record(), &deps, &[], &HashMap::new()).expect("roster");
        // Keep the tempdir alive for the agent's workspace by leaking it into the
        // test's lifetime — the process ends the test anyway.
        std::mem::forget(dir);
        (roster.into_iter().next().expect("one agent"), deps)
    }

    /// Empty first, real reply on retry → the wrapper returns the recovered reply
    /// and reports two attempts' usage (so both burnt attempts can be metered).
    #[tokio::test]
    async fn turn_wrapper_retries_empty_then_recovers() {
        let (agent, _deps) = scripted_agent(vec![Ok(String::new()), Ok("recovered".into())]);
        let (outcome, usages) = agent.run("hi").await;
        let outcome = outcome.expect("wrapper recovers");
        assert!(
            outcome.reply.contains("recovered"),
            "got {:?}",
            outcome.reply
        );
        assert_eq!(usages.len(), 2, "both attempts' usage is returned");
    }

    /// B-120: a turn that ends in a **hard error** still reports what it spent.
    ///
    /// The failure this pins is the one a founder saw as `0 tok / $0.000` on a
    /// ten-minute run: openhuman sets `last_turn_usage_totals` only after its
    /// own `?`, so `read_turn_usage` reads back nothing for an attempt that
    /// errored, and the usage then rode home on an `Ok` the caller never got.
    ///
    /// The script burns a real, usage-reporting model call and then fails:
    /// attempt 1 answers with usage but no text (openhuman raises
    /// `EmptyProviderResponse`, so it publishes no totals), and the one-shot
    /// retry hits a hard provider error. Both attempts therefore report zero of
    /// their own, and the only surviving figure is the live `TurnCostUpdated`
    /// tally — which is exactly what has to reach the caller *beside* the
    /// `Err`, because that is what the attempt row, the ledger and the usage
    /// meter are all built from.
    #[tokio::test]
    async fn a_hard_failed_turn_still_reports_the_tokens_it_burned() {
        let (agent, _deps) = scripted_agent_over(
            ScriptedProvider::new(vec![Ok(String::new())])
                .reporting_usage(tinyinference::Usage {
                    input_tokens: 1_200,
                    output_tokens: 340,
                    total_tokens: 1_540,
                    ..Default::default()
                })
                .failing_when_exhausted(),
        );

        let (outcome, usages) = agent.run("do ten minutes of work").await;

        assert!(
            outcome.is_err(),
            "the provider is permanently down past the first call; the turn must fail: {:?}",
            outcome.as_ref().map(|o| o.reply.clone())
        );
        let tokens: u64 = usages
            .iter()
            .map(|u| u.input_tokens + u.output_tokens)
            .sum();
        assert_eq!(
            tokens, 1_540,
            "a failed turn must carry home the tokens its own model call burned, \
             not report itself as free: {usages:?}"
        );
    }

    /// CodeRabbit review (PR #2053): a turn that fails WITHOUT spending
    /// anything must not inherit an earlier, unrelated turn's totals off the
    /// **reused** `Agent` — the same "0 tok / $0.000" bug B-120 fixes, in the
    /// opposite direction: a turn that spent nothing must not be billed for
    /// what a PAST turn on this same agent already spent and was already
    /// billed for.
    ///
    /// `CompanyAgent` reuses one `Agent` for every chat of a `(company,
    /// agent_id)` pair, and openhuman finalizes `last_turn_usage_totals` only
    /// on a turn that completes normally — an attempt that ends in
    /// `EmptyProviderResponse` never touches it, so a naive read after such an
    /// attempt reads back whatever the LAST *successful* turn on this agent
    /// left there, not this attempt's own (zero) spend. Left unguarded, that
    /// stale figure — already billed once when the first turn settled — would
    /// be billed a second time on a completely different, later turn that
    /// made no metered call at all.
    ///
    /// Turn 1 succeeds in one attempt and spends 1,540 tokens for real — the
    /// exact figure `last_turn_usage_totals` is left holding. Turn 2, on the
    /// SAME agent, scripts an immediate blank (`EmptyProviderResponse`) and
    /// then a hard failure on the one-shot retry once the script is exhausted
    /// — the identical shape `a_hard_failed_turn_still_reports_the_tokens_it_burned`
    /// already pins, just as the SECOND top-level call on this agent rather
    /// than the first. Because this provider carries usage on every scripted
    /// reply, turn 2's own first attempt genuinely burns another 1,540 tokens
    /// before dying, which the live `TurnCostUpdated` tally still recovers
    /// (`last_observed_turn_cost`) — so the correct total is 1,540 exactly,
    /// not 3,080 (turn 1's stale total, read back and double-counted across
    /// turn 2's own two attempts on top of what turn 2 itself burned).
    #[tokio::test]
    async fn a_turn_that_burns_nothing_does_not_inherit_a_past_turns_stale_total() {
        let (agent, _deps) = scripted_agent_over(
            ScriptedProvider::new(vec![
                Ok("turn one finished cleanly".to_string()),
                Ok(String::new()),
            ])
            .reporting_usage(tinyinference::Usage {
                input_tokens: 1_200,
                output_tokens: 340,
                total_tokens: 1_540,
                ..Default::default()
            })
            .failing_when_exhausted(),
        );

        // Turn one: a real, one-attempt success. Leaves
        // `last_turn_usage_totals` holding 1,540 tokens.
        let (outcome, usages) = agent.run("turn one").await;
        outcome.expect("turn one is a clean, successful reply");
        assert_eq!(
            usages
                .iter()
                .map(|u| u.input_tokens + u.output_tokens)
                .sum::<u64>(),
            1_540,
            "turn one's own real spend"
        );

        // Turn two, same agent: attempt 1 consumes the scripted blank
        // (EmptyProviderResponse), the retry then finds the script exhausted
        // and hits the permanent failure — both attempts error, neither
        // finalizes `last_turn_usage_totals`, and without the fix both reads
        // would instead return turn one's already-billed 1,540 a second AND
        // third time.
        let (second_outcome, second_usages) = agent.run("turn two").await;
        assert!(
            second_outcome.is_err(),
            "turn two's provider is permanently down past its first call: {:?}",
            second_outcome.as_ref().map(|o| o.reply.clone())
        );
        let second_tokens: u64 = second_usages
            .iter()
            .map(|u| u.input_tokens + u.output_tokens)
            .sum();
        assert_eq!(
            second_tokens, 1_540,
            "turn two must report its OWN spend — one metered call, recovered via the live \
             progress-stream tally since its own attempt also errors before finalizing totals \
             — never turn one's already-billed total read back a second and third time: \
             {second_usages:?}"
        );
    }

    /// Codex review (PR #2053): the original recovery gate only fired when
    /// EVERY attempt in `usages` reported zero, so it could recover at most
    /// one attempt's spend. A metered first attempt that empties, followed by
    /// a retry that succeeds and publishes its OWN authoritative total, left
    /// `usages` as `[zero, retry_total]` — not all-zero — so the first
    /// attempt's already-published `TurnCostUpdated` spend was silently
    /// dropped rather than merely under-reported.
    ///
    /// Both scripted replies carry the SAME usage (1,000 tokens each, via the
    /// one shared `.reporting_usage(...)` every `ScriptedProvider` reply
    /// gets), so the only way the total comes out to 2,000 rather than 1,000
    /// is if the first attempt's spend — recovered from its OWN segment of
    /// the progress stream, per `attempt_event_segments` — survives instead
    /// of being discarded the moment the retry's real total makes `usages`
    /// not-all-zero.
    #[tokio::test]
    async fn a_metered_empty_attempt_is_still_recovered_when_the_retry_succeeds() {
        let (agent, _deps) = scripted_agent_over(
            ScriptedProvider::new(vec![Ok(String::new()), Ok("recovered".to_string())])
                .reporting_usage(tinyinference::Usage {
                    input_tokens: 800,
                    output_tokens: 200,
                    total_tokens: 1_000,
                    ..Default::default()
                }),
        );

        let (outcome, usages) = agent.run("hi").await;
        let outcome = outcome.expect("the retry recovers a real reply");
        assert!(
            outcome.reply.contains("recovered"),
            "got {:?}",
            outcome.reply
        );
        assert_eq!(usages.len(), 2, "both attempts' usage is returned");

        let tokens: u64 = usages
            .iter()
            .map(|u| u.input_tokens + u.output_tokens)
            .sum();
        assert_eq!(
            tokens, 2_000,
            "both attempts genuinely burned 1,000 tokens each — the first attempt's spend must \
             not be dropped just because the retry went on to publish its own (also real) \
             total: {usages:?}"
        );
    }

    /// Codex review (PR #2053): an earlier version of the reused-agent fix
    /// above compared each `read_turn_usage` against the value seen before
    /// that attempt, and zeroed a read that came back unchanged — which is
    /// wrong for a genuinely NEW finalized total that happens to numerically
    /// equal the immediately preceding one. Two separate, single-attempt,
    /// fully successful calls on the SAME agent, both scripted with the exact
    /// same usage, must each report their own real spend in full — neither
    /// one is a retry, neither one errors, and a coincidental value match is
    /// not evidence that the second call spent nothing.
    #[tokio::test]
    async fn a_second_successful_turn_is_trusted_even_when_its_total_matches_the_first() {
        let (agent, _deps) = scripted_agent_over(
            ScriptedProvider::new(vec![Ok("turn one".to_string()), Ok("turn two".to_string())])
                .reporting_usage(tinyinference::Usage {
                    input_tokens: 500,
                    output_tokens: 100,
                    total_tokens: 600,
                    ..Default::default()
                }),
        );

        let (first_outcome, first_usages) = agent.run("turn one").await;
        first_outcome.expect("turn one succeeds in a single attempt");
        let first_tokens: u64 = first_usages
            .iter()
            .map(|u| u.input_tokens + u.output_tokens)
            .sum();
        assert_eq!(
            first_tokens, 600,
            "turn one's own real spend: {first_usages:?}"
        );

        let (second_outcome, second_usages) = agent.run("turn two").await;
        second_outcome.expect("turn two also succeeds in a single attempt");
        let second_tokens: u64 = second_usages
            .iter()
            .map(|u| u.input_tokens + u.output_tokens)
            .sum();
        assert_eq!(
            second_tokens, 600,
            "turn two's finalized total happens to equal turn one's — that coincidence must \
             not zero it out: {second_usages:?}"
        );
    }

    /// The tally is **cumulative**, so the last frame is the whole attempt's
    /// spend — summing the frames would multiply it, and taking the first would
    /// report only the opening call of a fifty-step run.
    #[test]
    fn the_observed_turn_cost_is_the_last_tally_not_the_first_or_the_sum() {
        let frame = |iteration: u32, input: u64, output: u64, usd: f64| {
            oh::agent::progress::AgentProgress::TurnCostUpdated {
                model: "scripted".to_string(),
                iteration,
                input_tokens: input,
                output_tokens: output,
                cached_input_tokens: 0,
                total_usd: usd,
            }
        };
        let events = vec![
            frame(1, 100, 10, 0.001),
            oh::agent::progress::AgentProgress::TextDelta {
                delta: "thinking".to_string(),
                iteration: 2,
            },
            frame(2, 900, 250, 0.019),
        ];

        let observed = last_observed_turn_cost(&events).expect("a tally was published");

        assert_eq!(observed.input_tokens, 900);
        assert_eq!(observed.output_tokens, 250);
        assert!((observed.cost_usd - 0.019).abs() < f64::EPSILON);
        assert_eq!(
            last_observed_turn_cost(&[]),
            None,
            "a turn that made no metered model call has no tally to report"
        );
    }

    /// Issue #111 retry-guard edge: when a steer already pends and the first
    /// attempt is the transient empty class, the one-shot retry is SKIPPED — so a
    /// cancel/pause issued before any text can't silently restart the work. The
    /// steered-empty turn therefore makes EXACTLY ONE attempt.
    #[tokio::test]
    async fn steered_empty_turn_makes_exactly_one_attempt() {
        // Attempt 1 is empty; a normal `run` would retry and consume the second
        // script entry. With a steer pending, the retry must not fire.
        let (agent, _deps) = scripted_agent(vec![Ok(String::new()), Ok("second".into())]);
        let control = SteerControl::new();
        control.request(SteerAction::Cancel);
        let (_outcome, usages) = agent
            .run_with_steer(
                "hi",
                Some(&control),
                None,
                None,
                None,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await;
        let _outcome = _outcome.expect("runs");
        assert_eq!(
            usages.len(),
            1,
            "a steered empty turn does NOT retry — exactly one attempt"
        );
    }

    // Note: the *installation* of the steer stop-hook can't be observed from the
    // provider — the tinyagents adapter snapshots the hooks at turn entry and the
    // provider call may run on a spawned task where the task-local isn't
    // inherited. The steer mechanism is instead proven end-to-end by the
    // retry-guard edge above and the `run_task` disposition matrix in
    // `harness::brain::tests` (cancel / pause / redirect all take effect).

    /// Empty twice → a graceful, non-error reply (chat never shows "Couldn't
    /// send" for a transient hiccup), still two attempts.
    #[tokio::test]
    async fn turn_wrapper_empty_twice_is_graceful() {
        let (agent, _deps) = scripted_agent(vec![Ok(String::new()), Ok(String::new())]);
        let (outcome, usages) = agent.run("hi").await;
        let outcome = outcome.expect("graceful, not an Err");
        assert!(
            outcome
                .reply
                .to_lowercase()
                .contains("temporary model hiccup"),
            "got {:?}",
            outcome.reply
        );
        assert_eq!(usages.len(), 2);
    }

    /// The Empty-vs-Hard split: only the transient empty-response class is
    /// retried/softened; every other error is `Hard` and propagates loudly (no
    /// blanket swallow). Driven at the classifier so it's deterministic — the
    /// live agent internally retries provider errors, which would make a scripted
    /// "hard error" non-deterministic.
    #[test]
    fn transient_empty_response_is_recognised_but_hard_errors_are_not() {
        let empty = anyhow::anyhow!("The model returned an empty response. Please try again.");
        assert!(
            is_transient_empty_response(&empty),
            "empty-response is transient"
        );

        let hard = anyhow::anyhow!("daily budget exceeded for agent 'ceo'");
        assert!(
            !is_transient_empty_response(&hard),
            "a budget error is NOT the transient empty class — it must propagate"
        );
    }

    // --- the per-turn wall-clock ceiling (issue #1680) -----------------------

    /// The leaf as the vendored harness actually writes it, taken verbatim from
    /// the failing run in issue #1680.
    fn ceiling_error() -> anyhow::Error {
        anyhow::anyhow!(
            "tinyagents harness run failed; model error; run timed out; model call for run \
             'agent_turn' exceeded its remaining wall-clock budget (56636 ms)"
        )
    }

    /// The same failure as [`ceiling_error`], but arriving as an `anyhow`
    /// context chain instead of one flattened string. Nothing guarantees the
    /// vendored crate keeps flattening it, and `is_wall_clock_ceiling` already
    /// assumes it might not.
    fn chained_ceiling_error() -> anyhow::Error {
        use anyhow::Context as _;
        Err::<(), _>(anyhow::anyhow!(
            "model call for run 'agent_turn' exceeded its remaining wall-clock budget (56636 ms)"
        ))
        .context("run timed out")
        .context("model error")
        .context("tinyagents harness run failed")
        .unwrap_err()
    }

    /// A chain must not lose its leaf. `{err}` renders only the outermost
    /// context — `tinyagents harness run failed` — which drops both the
    /// remaining-budget figure and the call that was in flight, the two things
    /// the message promises to keep. `{err:#}` renders the whole chain.
    #[test]
    fn a_chained_ceiling_error_keeps_its_leaf() {
        let err = chained_ceiling_error();
        assert_eq!(
            format!("{err}"),
            "tinyagents harness run failed",
            "the premise: the outermost context alone says nothing useful"
        );
        assert!(
            is_wall_clock_ceiling(&err),
            "a chained ceiling hit is still a ceiling hit"
        );

        let msg =
            wall_clock_ceiling_message("product_manager", Duration::from_millis(601_000), &err);
        assert!(
            msg.contains("56636 ms"),
            "the remaining-budget figure survives the chain: {msg}"
        );
        assert!(
            msg.contains("model call for run 'agent_turn'"),
            "and so does the call that was in flight: {msg}"
        );
    }

    #[test]
    fn wall_clock_ceiling_is_recognised_and_other_timeouts_are_not() {
        assert!(
            is_wall_clock_ceiling(&ceiling_error()),
            "the harness's own ceiling leaf must be recognised"
        );
        assert!(
            !is_wall_clock_ceiling(&anyhow::anyhow!(
                "request timed out after 30s connecting to the provider"
            )),
            "an ordinary provider timeout is NOT the turn ceiling — it keeps its own text"
        );
        assert!(
            !is_wall_clock_ceiling(&anyhow::anyhow!("daily budget exceeded for agent 'ceo'")),
            "a SPEND budget is not a wall-clock budget"
        );
    }

    /// A provider's response body reaches this chain verbatim — `provider.rs`
    /// raises `InferenceError::Model("hosted inference returned {status}: {text}")`
    /// — so an endpoint with a wall-clock budget of its own must not be read as
    /// OpenHuman's per-turn ceiling. That misdiagnosis is worse than the plain
    /// wrapper: it would report the second the request took as if it were a
    /// ten-minute turn, and tell the operator to raise a ceiling that was never
    /// reached.
    #[test]
    fn a_provider_body_that_mentions_a_wall_clock_budget_is_not_the_ceiling() {
        for body in [
            "hosted inference returned 429: {\"error\":\"wall-clock budget for this key is exhausted\"}",
            "hosted inference returned 400: your per-request wall-clock budget must be positive",
        ] {
            assert!(
                !is_wall_clock_ceiling(&anyhow::anyhow!("{body}")),
                "a provider body is not the turn ceiling: {body}"
            );
        }
        // Both phrasings the vendored harness actually raises still match,
        // including the deadline spelling the old three-word search covered
        // only by accident.
        for leaf in [
            "model call for run 'agent_turn' exceeded its remaining wall-clock budget (56636 ms)",
            "tool call for run 'agent_turn' exceeded its wall-clock deadline",
        ] {
            assert!(
                is_wall_clock_ceiling(&anyhow::anyhow!("{leaf}")),
                "the harness's own leaf must still be recognised: {leaf}"
            );
        }
    }

    #[test]
    fn elapsed_reads_as_an_operator_reads_a_clock() {
        assert_eq!(humanise_elapsed(Duration::from_millis(9_400)), "9s");
        assert_eq!(humanise_elapsed(Duration::from_secs(90)), "1m 30s");
        assert_eq!(humanise_elapsed(Duration::from_millis(601_000)), "10m 01s");
    }

    /// The whole point of #1680: the operator is told what the turn actually
    /// spent, and told that the harness's own number is the remainder rather
    /// than a limit. The old text said neither.
    #[test]
    fn ceiling_message_reports_elapsed_and_reframes_the_harness_number() {
        let err = ceiling_error();
        let msg =
            wall_clock_ceiling_message("product_manager", Duration::from_millis(601_000), &err);

        assert!(msg.contains("product_manager"), "names the agent: {msg}");
        assert!(
            msg.contains("10m 01s"),
            "states what the turn actually spent: {msg}"
        );
        assert!(
            msg.contains("REMAINED"),
            "says the harness's figure is the remainder, not a limit: {msg}"
        );
        assert!(
            msg.contains("OPENHUMAN_AGENT_TURN_TIMEOUT_SECS"),
            "names the knob that moves the ceiling: {msg}"
        );
        assert!(
            msg.contains("56636 ms"),
            "keeps the underlying diagnostic verbatim: {msg}"
        );
        // The ceiling's own value is private to the vendored crate, so it is
        // deliberately not restated — a stale copy would be worse than none.
        assert!(
            !msg.contains("600"),
            "must not hardcode a ceiling it cannot read: {msg}"
        );
    }

    /// At the classifier, where the retry wrapper actually reads it: a ceiling
    /// hit stays HARD (a ten-minute failure must not be retried into twenty),
    /// and carries the honest message rather than the bare `turn for 'x': …`.
    /// The other spelling the harness raises carries **no** figure — `run
    /// `agent_turn` exceeded its wall-clock deadline` — so the message must not
    /// point the operator at one. Being accurate but unfollowable is the exact
    /// defect #1680 was filed on; reintroducing it one spelling over would be a
    /// poor way to close it.
    #[test]
    fn the_figure_less_spelling_is_not_promised_a_figure() {
        let err = anyhow::anyhow!(
            "tinyagents harness run failed: run timed out: run `agent_turn` exceeded its \
             wall-clock deadline"
        );
        assert!(
            is_wall_clock_ceiling(&err),
            "it is still the ceiling, and still classified here"
        );

        let msg = wall_clock_ceiling_message("ceo", Duration::from_secs(600), &err);
        assert!(
            !msg.contains("the figure below"),
            "there is no figure below to point at: {msg}"
        );
        assert!(
            msg.contains("10m 00s"),
            "the measured elapsed still leads: {msg}"
        );
        assert!(
            msg.contains("OPENHUMAN_AGENT_TURN_TIMEOUT_SECS"),
            "and the knob is still named: {msg}"
        );
        assert!(
            msg.contains("exceeded its wall-clock deadline"),
            "the leaf survives verbatim: {msg}"
        );
    }

    #[tokio::test]
    async fn classify_turn_reframes_a_ceiling_hit_and_keeps_it_hard() {
        let (agent, _deps) = scripted_agent(vec![]);

        let outcome = agent.classify_turn(Err(ceiling_error()), Duration::from_millis(601_000));
        let AttemptOutcome::Hard(err) = outcome else {
            panic!("a ceiling hit is not retryable and must classify Hard");
        };
        let text = err.to_string();
        assert!(
            text.contains("per-turn wall-clock ceiling after 10m 01s"),
            "got {text}"
        );

        // Every other hard error keeps the plain wrapper, unchanged.
        let other = agent.classify_turn(
            Err(anyhow::anyhow!("provider refused the request")),
            Duration::from_secs(3),
        );
        let AttemptOutcome::Hard(err) = other else {
            panic!("an unrelated failure is still Hard");
        };
        assert!(
            err.to_string().contains("provider refused the request"),
            "an unrelated failure keeps its own text: {err}"
        );
        assert!(
            !err.to_string().contains("wall-clock"),
            "and gains no ceiling prose: {err}"
        );
    }

    // --- top-level budget pause (issue #1846) --------------------------------

    /// Drift-coupling: `is_top_level_budget_exhausted` must be a thin wrapper
    /// over `oh::inference::provider::is_budget_exhausted_message`, never a
    /// second, independently-maintained phrase list. Computes both sides for
    /// a spread of real and synthetic bodies and asserts they never disagree,
    /// so an edit that "helps" by hardcoding a phrase here fails CI instead of
    /// silently drifting from the shared source (the deferred-classifier-arm
    /// trap this repo has been bitten by before).
    #[test]
    fn top_level_budget_classifier_never_drifts_from_the_shared_source() {
        for body in [
            "hosted inference returned 400: insufficient budget for this account",
            "hosted inference returned 402: budget exceeded for this key",
            "anthropic API error (400 Bad Request): {\"error\":{\"code\":\"invalid_request_error\",\
             \"message\":\"Your credit balance is too low to access the Anthropic API. Please go \
             to Plans & Billing to upgrade or purchase credits.\",\"type\":\"invalid_request_error\"}}",
            "hosted inference returned 402: {\"success\": false, \"error\": \"You have no \
             remaining credits to use the LLM apis.\"}",
            "hosted inference returned 429: quota exceeded — add credits to continue",
            "hosted inference returned 500: internal server error",
            "provider refused the request",
            "request timed out after 30s connecting to the provider",
            "",
        ] {
            let err = anyhow::anyhow!("{body}");
            assert_eq!(
                is_top_level_budget_exhausted(&err),
                oh::inference::provider::is_budget_exhausted_message(&format!("{err:#}")),
                "top-level classifier drifted from the shared source for: {body}"
            );
        }
    }

    /// The headline unit test: every known budget-exhausted wire shape
    /// classifies `BudgetPaused`, not `Hard` — proving the asymmetry this
    /// issue closes at the one place it was introduced. A non-budget `Err`
    /// keeps classifying `Hard`, unchanged.
    #[tokio::test]
    async fn classify_turn_recognises_every_known_budget_wire_shape_as_paused_not_hard() {
        let (agent, _deps) = scripted_agent(vec![]);

        let wire_shapes = [
            (
                "managed backend 400 (USER_INSUFFICIENT_CREDITS-style)",
                "hosted inference returned 400: {\"error\":\"USER_INSUFFICIENT_CREDITS: \
                 insufficient budget for this account\"}",
            ),
            (
                "Anthropic BYO 400",
                "anthropic API error (400 Bad Request): {\"error\":{\"code\":\"invalid_request_error\",\
                 \"message\":\"Your credit balance is too low to access the Anthropic API. Please \
                 go to Plans & Billing to upgrade or purchase credits.\",\"type\":\"invalid_request_error\"}}",
            ),
            (
                "abacus/OpenRouter-style no-remaining-credits 402",
                "hosted inference returned 402: {\"success\": false, \"error\": \"You have no \
                 remaining credits to use the LLM apis.\"}",
            ),
            (
                "quota exceeded",
                "hosted inference returned 429: quota exceeded — add credits to continue",
            ),
        ];

        for (label, body) in wire_shapes {
            let outcome =
                agent.classify_turn(Err(anyhow::anyhow!("{body}")), Duration::from_secs(1));
            let AttemptOutcome::BudgetPaused { summary } = outcome else {
                panic!("{label}: must classify BudgetPaused for wire body: {body}");
            };
            assert!(summary.starts_with("Paused —"), "{label}: {summary}");
            assert!(
                summary.to_ascii_lowercase().contains("add credits"),
                "{label}: must carry the actionable ask: {summary}"
            );
        }

        // Non-budget Err → still Hard, byte-for-byte the pre-#1846 behaviour.
        let other = agent.classify_turn(
            Err(anyhow::anyhow!(
                "hosted inference returned 500: internal server error"
            )),
            Duration::from_secs(1),
        );
        let AttemptOutcome::Hard(err) = other else {
            panic!("a non-budget failure must still classify Hard");
        };
        assert!(
            err.to_string().contains("internal server error"),
            "an unrelated failure keeps its own text: {err}"
        );
    }

    /// The halt copy shares the delegated sub-agent halt's ACTIONABLE framing
    /// — "add credits" / "top up that provider's own account" / an explicit
    /// next step for the operator — never the harness's own error vocabulary.
    /// Byte-identity with the vendored `terminal_inference_halt_summary` is
    /// not achievable (that function is private to `tinyagents` and phrased
    /// per-tool, "the `{tool}` step failed", which has no analogue at the top
    /// level), so this asserts the shared phrases survive instead of a
    /// whole-string match. The top-level copy's own next step is "resend your
    /// message" rather than the delegated halt's "try again" — a turn, unlike
    /// a tool call, has no retry to invite; both say the SAME thing in the
    /// vocabulary that fits their own layer.
    #[test]
    fn budget_paused_copy_shares_the_add_credits_framing_with_the_delegated_halt() {
        let err = anyhow::anyhow!("hosted inference returned 400: insufficient budget");
        let summary = budget_paused_summary("ceo", &err);
        let lower = summary.to_ascii_lowercase();
        for phrase in [
            "add credits",
            "top up that provider's own account",
            "resend your message",
        ] {
            assert!(
                lower.contains(phrase),
                "missing shared framing {phrase:?}: {summary}"
            );
        }
        assert!(summary.contains("ceo"), "names the teammate: {summary}");
    }

    /// The coarse proximity threshold (issue #1846): fires at/above 90% of the
    /// cap, never below it, and never on a non-positive/non-finite cap — the
    /// guard that keeps a company with no ceiling configured (`cap == 0` is
    /// unreachable in practice, but the function must not divide by it) from
    /// ever firing.
    #[test]
    fn budget_proximity_threshold_fires_at_ninety_percent_and_not_below() {
        assert!(!is_approaching_budget_ceiling(89, 100), "89% is not near");
        assert!(is_approaching_budget_ceiling(90, 100), "90% is near");
        assert!(
            is_approaching_budget_ceiling(100, 100),
            "100% is near (though callers gate this out via total_exhausted first)"
        );
        assert!(
            !is_approaching_budget_ceiling(50, 0),
            "a zero cap must never divide-by-zero into true"
        );

        assert!(
            !is_approaching_budget_ceiling_f64(4.49, 5.0),
            "89.8% rounds down to not-near"
        );
        assert!(is_approaching_budget_ceiling_f64(4.5, 5.0), "90% is near");
        assert!(
            !is_approaching_budget_ceiling_f64(4.5, f64::NAN),
            "a non-finite cap must never read as near"
        );
        assert!(
            !is_approaching_budget_ceiling_f64(4.5, 0.0),
            "a non-positive cap must never read as near"
        );
    }

    /// **The regression.** The bug this issue closes: a top-level orchestrator
    /// turn whose own inference call fails with a budget-exhausted body — no
    /// delegated-tool envelope marker anywhere in the chain, because nothing
    /// was delegated — must terminate `Ok(TurnOutcome { budget_paused: Some(_), .. })`
    /// and park a re-issue marker, NOT propagate `Err(OpenCompanyError::Harness(_))`.
    ///
    /// Before this issue's fix, `classify_turn` had no arm between the
    /// wall-clock-ceiling check and the generic `Hard` catch-all, so this
    /// exact scenario fell through to `Hard(OpenCompanyError::Harness(format!("turn
    /// for '{agent}': {err}")))` and `HarnessPool::run` returned that `Err` to
    /// every caller — the silent mid-task break the issue is named for. This
    /// test's premise is provable by inspection: `classify_turn`'s match arms
    /// are ordered ceiling → budget → generic, and removing the budget arm
    /// (i.e. reverting this diff) makes this body fall through to the generic
    /// arm, which this test would then catch as an `Err` instead of the
    /// expected `Ok`. The `ScriptedProvider` returns the error DIRECTLY from
    /// `ChatModel::invoke` (no tool call, no delegation, no envelope) — the
    /// "simple non-delegating task" the issue specifies.
    #[tokio::test]
    async fn a_top_level_budget_exhaustion_pauses_gracefully_and_parks_a_reissue_marker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let company = CompanyId::new("acme-budget-regress");
        let mut rec = record();
        rec.id = company.clone();
        let deps = HarnessDeps {
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            // Scripted 10 deep, not once: whether the vendored harness retries
            // a model error internally before ever handing `agent.turn()`'s
            // caller an `Err` is not this crate's contract to assume — the
            // real-world case this mirrors (an exhausted account) fails the
            // SAME way on every retry regardless, so scripting depth instead
            // of count-exactness is both safer and truer to the scenario.
            provider: Arc::new(ScriptedProvider::new(vec![
                Err(
                    "USER_INSUFFICIENT_CREDITS: insufficient budget for this account — add \
                     credits to continue"
                        .to_string(),
                );
                10
            ])),
            provider_slug: "scripted".to_string(),
            serves: None,
            context: Arc::new(MockContext::default()),
            store: Arc::new(RecordingStore::default()),
            // No meter: the pre-flight budget gates (total ceiling, per-agent
            // cap) fail OPEN with none configured, so this dispatch reaches
            // the model call rather than being refused pre-flight — the
            // scenario under test is the model call itself failing, not a
            // pre-flight refusal.
            meter: None,
            workspace_root: dir.path().to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: dir.path().to_path_buf(),
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
            workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_runs: None,
            deep_trace: None,
            workflow_revisions: None,
            approval_requests: ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
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
            search: None,
            tenant_search: None,
            workspace: None,
        };

        let pool = HarnessPool::new();
        pool.ensure(&rec, &deps).await.expect("pool ensures");

        let outcome = pool
            .run(
                &company,
                "ceo",
                "Please summarize today's standup notes.",
                &deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect(
                "a budget pause is a graceful stop, not an error — it must return Ok, which is \
                 the whole regression this test proves",
            );

        let pause = outcome.budget_paused.as_ref().expect(
            "the top-level inference call failed on a budget-exhausted body; the turn must \
             report the pause",
        );
        assert_eq!(pause.agent, "ceo");
        assert!(
            pause.summary.contains("add credits"),
            "the actionable ask survives to the outcome: {}",
            pause.summary
        );

        // And a durable re-issue marker is parked, keyed on the agent, naming
        // the ORIGINAL message — the grant-reissue precedent this issue reuses.
        let marker = crate::runtime::grants::budget_pauses_for(&company)
            .peek("ceo")
            .expect("a re-issue marker must be parked for the paused agent");
        assert_eq!(marker.agent, "ceo");
        assert_eq!(marker.message, "Please summarize today's standup notes.");
        assert_eq!(marker.summary, pause.summary);
    }

    /// Codex review (PR #2053) — **the regression.** A ledger write is a
    /// separate concern from what the turn itself did, and a failure in it
    /// must not also swallow the OTHER outcome-side-effect this same code
    /// block performs: retiring a stale re-issue marker once an agent's turn
    /// succeeds again, proving the account that blocked it now has budget.
    /// Before this fix, `turn_result_after_metering`'s `?` ran BEFORE this
    /// retire logic, so a ledger write that failed for an UNRELATED reason
    /// left the stale marker — and its stale "Add credits & resend" CTA —
    /// parked indefinitely, able to later re-dispatch the OLD message a
    /// second time.
    ///
    /// Same fixture as `a_successful_turn_retires_a_stale_reissue_marker_for_the_same_agent`
    /// — a stale marker parked directly, then one ordinary successful `run`
    /// for the same agent in the same thread — except this provider's reply
    /// carries real usage, so `turn_costs` is nonzero and `meter_turn_costs`
    /// actually attempts (and, against `FailingLedgerStore`, fails) a ledger
    /// write. Reverting the reordering in `run_inner` makes the final `peek`
    /// below find the marker still parked instead of `None`.
    #[tokio::test]
    async fn a_metering_failure_does_not_swallow_a_stale_marker_retirement() {
        let dir = tempfile::tempdir().expect("tempdir");
        let company = CompanyId::new("acme-budget-meter-fail-retire-regress");
        let mut rec = record();
        rec.id = company.clone();
        let deps = HarnessDeps {
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            // A single, ordinary, non-blank reply — the same shape
            // `a_successful_turn_retires_a_stale_reissue_marker_for_the_same_agent`
            // scripts, just with usage attached so this turn's spend is
            // nonzero and `meter_turn_costs` has something to write.
            provider: Arc::new(
                ScriptedProvider::new(vec![Ok("Here's today's standup summary.".to_string()); 4])
                    .reporting_usage(tinyinference::Usage {
                        input_tokens: 800,
                        output_tokens: 200,
                        total_tokens: 1_000,
                        ..Default::default()
                    }),
            ),
            provider_slug: "scripted".to_string(),
            serves: None,
            context: Arc::new(MockContext::default()),
            store: Arc::new(FailingLedgerStore),
            meter: None,
            workspace_root: dir.path().to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: dir.path().to_path_buf(),
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
            workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_runs: None,
            deep_trace: None,
            workflow_revisions: None,
            approval_requests: ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
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
            search: None,
            tenant_search: None,
            workspace: None,
        };

        let pool = HarnessPool::new();
        pool.ensure(&rec, &deps).await.expect("pool ensures");

        // Park the stale marker directly — standing in for an earlier turn
        // that genuinely paused, exactly as the sibling retire test does.
        crate::runtime::grants::budget_pauses_for(&company).park(
            "ceo",
            Some("general".to_string()),
            "Please summarize today's standup notes.",
            "Paused — ceo's turn ran out of inference budget/credits.",
            crate::ports::now_millis(),
            crate::runtime::grants::RedeemContext::default(),
        );
        assert!(
            crate::runtime::grants::budget_pauses_for(&company)
                .peek("ceo")
                .is_some(),
            "the stale marker must be parked before the run this test exercises"
        );

        let result = pool
            .run(
                &company,
                "ceo",
                "Please summarize today's standup notes.",
                &deps,
                crate::runtime::delegation::ChatTarget::channel(Some("general")),
            )
            .await;

        assert!(
            result.is_err(),
            "the turn itself succeeded, so the ledger failure is the only failure there is, \
             and it still propagates — turn_result_after_metering's own documented contract: \
             {result:?}"
        );

        // The retirement must have happened regardless — read off the turn's
        // OWN outcome, before the metering error ever had a chance to short
        // circuit it.
        assert!(
            crate::runtime::grants::budget_pauses_for(&company)
                .peek("ceo")
                .is_none(),
            "the stale marker must be retired even though the ledger write for THIS turn \
             failed — the ledger is a separate concern from what the turn itself did, and \
             leaving it parked would let its stale CTA re-dispatch the old message again"
        );
    }

    /// Issue #1846 review (Codex #3869193105) — **the regression.** A
    /// BYO/custom-provider budget error can carry a credential-bearing URL
    /// (the account's own endpoint, with an API key riding in the query
    /// string) baked into the provider's response body, which becomes the
    /// raw `anyhow` error chain `budget_paused_summary` formats into
    /// `summary`.
    ///
    /// Before this fix, only the copy returned as the turn's authored REPLY
    /// was scrubbed (`Ok(mcp_probe::scrub(&summary, &[]))`); the copy stored
    /// into the `budget_pause_summary` mutex slot — which becomes
    /// `TurnOutcome::budget_paused.summary`, and from there the durable
    /// `BudgetPauseMarker.summary` AND the chat notice text
    /// `budget_pause_notice` renders from it — was the RAW, unscrubbed
    /// `summary`. A secret that never should have left the reply bubble was
    /// therefore persisted on the marker and shown in the chat notice, both
    /// operator-visible and durable, independent of whatever the reply itself
    /// said.
    ///
    /// Same fixture and scenario as the test above; the only difference is
    /// what the scripted provider's error body contains. Proof this pins the
    /// actual fix and not a coincidence: reverting the `scrub` call in
    /// either `BudgetPaused` arm of `classify_turn`'s caller makes this
    /// assertion fail while leaving the sibling test above green.
    #[tokio::test]
    async fn a_budget_pause_summary_is_scrubbed_before_it_is_persisted_anywhere() {
        let dir = tempfile::tempdir().expect("tempdir");
        let company = CompanyId::new("acme-budget-scrub-regress");
        let mut rec = record();
        rec.id = company.clone();
        let deps = HarnessDeps {
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            provider: Arc::new(ScriptedProvider::new(vec![
                Err(
                    "insufficient budget: BYO provider request to \
                     https://api.byo-provider.example/v1/chat?api_key=sk-live-topsecret123 \
                     failed with 400"
                        .to_string(),
                );
                10
            ])),
            provider_slug: "scripted".to_string(),
            serves: None,
            context: Arc::new(MockContext::default()),
            store: Arc::new(RecordingStore::default()),
            meter: None,
            workspace_root: dir.path().to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: dir.path().to_path_buf(),
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
            workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_runs: None,
            deep_trace: None,
            workflow_revisions: None,
            approval_requests: ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
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
            search: None,
            tenant_search: None,
            workspace: None,
        };

        let pool = HarnessPool::new();
        pool.ensure(&rec, &deps).await.expect("pool ensures");

        let outcome = pool
            .run(
                &company,
                "ceo",
                "Please summarize today's standup notes.",
                &deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect("a budget pause is a graceful stop, not an error");

        let pause = outcome
            .budget_paused
            .as_ref()
            .expect("the scripted body matches the budget-exhausted classifier");

        assert!(
            !pause.summary.contains("api_key=sk-live-topsecret123"),
            "the credential must not survive into TurnOutcome::budget_paused.summary: {}",
            pause.summary
        );
        assert!(
            !outcome.reply.contains("api_key=sk-live-topsecret123"),
            "the credential must not survive into the authored reply either: {}",
            outcome.reply
        );

        // The durable marker — what the chat notice (`budget_pause_notice`)
        // and the console's `GET …/budget-pause` both read — carries the SAME
        // scrubbed summary, not a second, unscrubbed copy of the raw error.
        let marker = crate::runtime::grants::budget_pauses_for(&company)
            .peek("ceo")
            .expect("a re-issue marker must be parked for the paused agent");
        assert!(
            !marker.summary.contains("api_key=sk-live-topsecret123"),
            "the persisted marker must not carry the credential either: {}",
            marker.summary
        );
        assert_eq!(marker.summary, pause.summary);
    }

    /// Issue #1846 review (Codex #3868962381) — **the regression.** The
    /// budget-pause notice's own copy gives the operator TWO ways to recover:
    /// click "Add credits & resend" (the CTA, which redeems the marker), or
    /// add credits and resend the message themselves from the composer. Only
    /// the first path used to retire the parked marker — `redeem`/
    /// `redeem_matching` are the sole consumers of `BudgetPauseSet`'s entries.
    /// A manual resend that succeeds bypasses both entirely, so the marker
    /// (and the stale "Add credits & resend" CTA on the old notice) stayed
    /// parked indefinitely. Clicking that stale CTA later would silently
    /// re-dispatch the OLD message a second time.
    ///
    /// Proof this pins the fix and not a coincidence: the marker parked
    /// (directly, bypassing the turn machinery entirely so this test does not
    /// depend on how many attempts the vendored harness's own internal retry
    /// consumes before a budget-exhausted body reaches `classify_turn` — see
    /// the sibling regression tests' "scripted 10 deep" comments for why that
    /// count is not this crate's contract to assume) must be gone after ONE
    /// ordinary successful `pool.run` call for the SAME agent — reverting the
    /// retire branch in `run_inner` (this file) makes the final `peek` below
    /// find the marker still parked instead of `None`.
    #[tokio::test]
    async fn a_successful_turn_retires_a_stale_reissue_marker_for_the_same_agent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let company = CompanyId::new("acme-budget-retire-regress");
        let mut rec = record();
        rec.id = company.clone();
        let deps = HarnessDeps {
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            // Always succeeds — the "operator manually added credits and
            // resent" half of the scenario, NOT the redeem route, which is
            // the whole point: nothing here ever calls `redeem`/
            // `redeem_matching`.
            provider: Arc::new(ScriptedProvider::new(vec![
                Ok(
                    "Here's today's standup summary.".to_string()
                );
                4
            ])),
            provider_slug: "scripted".to_string(),
            serves: None,
            context: Arc::new(MockContext::default()),
            store: Arc::new(RecordingStore::default()),
            meter: None,
            workspace_root: dir.path().to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: dir.path().to_path_buf(),
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
            workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_runs: None,
            deep_trace: None,
            workflow_revisions: None,
            approval_requests: ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
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
            search: None,
            tenant_search: None,
            workspace: None,
        };

        let pool = HarnessPool::new();
        pool.ensure(&rec, &deps).await.expect("pool ensures");

        // Park the stale marker directly — standing in for an earlier turn
        // that genuinely paused. `redeem`/`redeem_matching` are the only
        // consumers this fix's `else` branch is NOT one of, so parking it
        // this way exercises the exact same retire path a real pause would.
        crate::runtime::grants::budget_pauses_for(&company).park(
            "ceo",
            Some("general".to_string()),
            "Please summarize today's standup notes.",
            "Paused — ceo's turn ran out of inference budget/credits.",
            crate::ports::now_millis(),
            crate::runtime::grants::RedeemContext::default(),
        );
        assert!(
            crate::runtime::grants::budget_pauses_for(&company)
                .peek("ceo")
                .is_some(),
            "the stale marker must be parked before the run this test exercises"
        );

        // The "manually add credits and resend" half of the scenario: an
        // ordinary successful `run`, NOT the redeem route — nothing here
        // ever calls `redeem`/`redeem_matching` on the marker parked above.
        // Same thread ("general") the marker itself parked with (issue
        // #1846 review, Codex #3869968949): a genuine resend runs in the
        // SAME conversation, and the widened context match this test is
        // pinned against would otherwise (correctly) treat a different
        // thread as a different request.
        let outcome = pool
            .run(
                &company,
                "ceo",
                "Please summarize today's standup notes.",
                &deps,
                crate::runtime::delegation::ChatTarget::channel(Some("general")),
            )
            .await
            .expect("this run succeeds against the scripted reply");
        assert!(
            outcome.budget_paused.is_none(),
            "this attempt must NOT pause — this scenario is about a resend that succeeds"
        );

        assert!(
            crate::runtime::grants::budget_pauses_for(&company)
                .peek("ceo")
                .is_none(),
            "the stale marker parked above must be retired once this agent has a successful \
             turn again, even though nothing ever redeemed it"
        );
    }

    /// Issue #1846 review (Codex #3869792503) — **the regression.** The
    /// sibling test above proves a genuine RESEND retires its own marker;
    /// this proves an UNRELATED success for the same agent does not retire
    /// somebody else's still-unretried marker.
    ///
    /// An agent has at most one parked marker (`BudgetPauseSet` overwrites by
    /// agent id), so two DIFFERENT requests cannot both be "the" pause at
    /// once — but a marker parked for request A can still be live when an
    /// entirely separate request B for the same agent (an automatic
    /// background task, a second chat message) happens to succeed. Before
    /// this fix, that success unconditionally retired A's marker too — the
    /// operator's original ask (A) was never reissued, yet its CTA would
    /// report "nothing to resend" as though it had been.
    ///
    /// Proof this pins the fix and not a coincidence: reverting
    /// `retire_if_message_matches`'s match guard back to an unconditional
    /// take (this file's `run_inner`, or `BudgetPauseSet::retire_if_message_matches`
    /// itself) makes the final `peek` below find `None` instead of the still-
    /// parked marker for request A.
    #[tokio::test]
    async fn an_unrelated_success_does_not_retire_a_different_requests_marker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let company = CompanyId::new("acme-budget-retire-mismatch-regress");
        let mut rec = record();
        rec.id = company.clone();
        let deps = HarnessDeps {
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            // Succeeds against a request B's text — deliberately DIFFERENT
            // from request A's, parked below.
            provider: Arc::new(ScriptedProvider::new(vec![
                Ok(
                    "Filed under Q3 planning.".to_string()
                );
                4
            ])),
            provider_slug: "scripted".to_string(),
            serves: None,
            context: Arc::new(MockContext::default()),
            store: Arc::new(RecordingStore::default()),
            meter: None,
            workspace_root: dir.path().to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: dir.path().to_path_buf(),
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
            workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_runs: None,
            deep_trace: None,
            workflow_revisions: None,
            approval_requests: ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
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
            search: None,
            tenant_search: None,
            workspace: None,
        };

        let pool = HarnessPool::new();
        pool.ensure(&rec, &deps).await.expect("pool ensures");

        // Request A: paused, still unretried — parked directly, standing in
        // for a genuine earlier pause.
        crate::runtime::grants::budget_pauses_for(&company).park(
            "ceo",
            Some("general".to_string()),
            "Please summarize today's standup notes.",
            "Paused — ceo's turn ran out of inference budget/credits.",
            crate::ports::now_millis(),
            crate::runtime::grants::RedeemContext::default(),
        );

        // Request B: a completely different ask for the SAME agent, which
        // succeeds — an automatic background task landing before the
        // operator ever gets to A's CTA, per the finding's own example.
        let outcome = pool
            .run(
                &company,
                "ceo",
                "File this under Q3 planning.",
                &deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect("request B succeeds against the scripted reply");
        assert!(outcome.budget_paused.is_none(), "request B must not pause");

        let marker = crate::runtime::grants::budget_pauses_for(&company)
            .peek("ceo")
            .expect(
                "request A's marker must survive an unrelated request B succeeding — A was \
                 never reissued",
            );
        assert_eq!(
            marker.message, "Please summarize today's standup notes.",
            "the marker still parked must be request A's, untouched by B's success"
        );
    }

    /// Issue #1846 review (Codex #3869968949) — **the regression.** The
    /// sibling test above proves a DIFFERENT-text unrelated success does not
    /// retire the marker; this proves IDENTICAL text in a DIFFERENT thread
    /// does not either — the finding's own example ("review this", posted in
    /// two different threads).
    ///
    /// Same fixture and scenario, but request B repeats request A's EXACT
    /// text — in a different chat thread. Before the widened match (message
    /// text alone), this would have retired A's marker: `marker.message ==
    /// candidate_message` was already true for identical text regardless of
    /// which thread either ran in.
    #[tokio::test]
    async fn identical_text_in_a_different_thread_does_not_retire_the_original_marker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let company = CompanyId::new("acme-budget-retire-same-text-diff-thread");
        let mut rec = record();
        rec.id = company.clone();
        let deps = HarnessDeps {
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            provider: Arc::new(ScriptedProvider::new(vec![
                Ok("Here it is.".to_string());
                4
            ])),
            provider_slug: "scripted".to_string(),
            serves: None,
            context: Arc::new(MockContext::default()),
            store: Arc::new(RecordingStore::default()),
            meter: None,
            workspace_root: dir.path().to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: dir.path().to_path_buf(),
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
            workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_runs: None,
            deep_trace: None,
            workflow_revisions: None,
            approval_requests: ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
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
            search: None,
            tenant_search: None,
            workspace: None,
        };

        let pool = HarnessPool::new();
        pool.ensure(&rec, &deps).await.expect("pool ensures");

        // Request A: paused in "general", still unretried.
        crate::runtime::grants::budget_pauses_for(&company).park(
            "ceo",
            Some("general".to_string()),
            "review this",
            "Paused — ceo's turn ran out of inference budget/credits.",
            crate::ports::now_millis(),
            crate::runtime::grants::RedeemContext::default(),
        );

        // Request B: the EXACT same text, but in "sales" — a different
        // conversation entirely — which succeeds.
        let outcome = pool
            .run(
                &company,
                "ceo",
                "review this",
                &deps,
                crate::runtime::delegation::ChatTarget::channel(Some("sales")),
            )
            .await
            .expect("request B succeeds against the scripted reply");
        assert!(outcome.budget_paused.is_none(), "request B must not pause");

        let marker = crate::runtime::grants::budget_pauses_for(&company)
            .peek("ceo")
            .expect(
                "request A's marker must survive B's success — same text, but a DIFFERENT \
                 thread, is not the same request",
            );
        assert_eq!(marker.chat_id.as_deref(), Some("general"));
    }

    /// This file's own default `park()` call site — the top-level turn, not
    /// a delegated re-park — stamps the marker with the ambient
    /// `RedeemContext` a cycle sets around it (issue #1846 review, Codex
    /// #3865812419/#3865812423/#3865812432). Same fixture as the test
    /// above, wrapped in `with_redeem_context` the way
    /// `CycleRunner::run_bracketed` does in production, with a non-default
    /// parent/deliverable/mentions to prove they land on the marker instead
    /// of being silently dropped the way the pre-fix `redeem_budget_pause`
    /// dropped them on the OTHER side of a redeem.
    #[tokio::test]
    async fn a_top_level_budget_pause_parks_the_ambient_redeem_context() {
        use crate::ports::types::{Attachment, EventSeq, Mention, MentionTarget, MessageIntent};

        let dir = tempfile::tempdir().expect("tempdir");
        let company = CompanyId::new("acme-budget-redeem-context");
        let mut rec = record();
        rec.id = company.clone();
        let deps = HarnessDeps {
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            provider: Arc::new(ScriptedProvider::new(vec![
                Err(
                    "USER_INSUFFICIENT_CREDITS: insufficient budget for this account — add \
                     credits to continue"
                        .to_string(),
                );
                10
            ])),
            provider_slug: "scripted".to_string(),
            serves: None,
            context: Arc::new(MockContext::default()),
            store: Arc::new(RecordingStore::default()),
            meter: None,
            workspace_root: dir.path().to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: dir.path().to_path_buf(),
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
            workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_runs: None,
            deep_trace: None,
            workflow_revisions: None,
            approval_requests: ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
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
            search: None,
            tenant_search: None,
            workspace: None,
        };

        let pool = HarnessPool::new();
        pool.ensure(&rec, &deps).await.expect("pool ensures");

        // Issue #1846 review (Codex #3866418891): `text`/`attachments` are the
        // same "raw operator message" pair `park_message` prefers over this
        // turn's own COMPOSED `message` — assert they reach the marker
        // through this top-level call site too, not just the direct
        // `budget_pauses_for(...).park(...)` unit test in `budget_pause.rs`.
        let redeem = crate::runtime::grants::RedeemContext {
            parent: Some(EventSeq::new(42)),
            deliverable: Some(MessageIntent::Workflow),
            mentions: vec![Mention {
                target: MentionTarget::Agent {
                    id: "researcher".to_string(),
                },
                text: "@researcher".to_string(),
                offset: 0,
                quiet: false,
            }],
            text: Some("@researcher please summarize the attached standup notes.".to_string()),
            attachments: vec![Attachment {
                node_id: "node-top-level-1".to_string(),
                name: "standup-notes.txt".to_string(),
                mime: "text/plain".to_string(),
                size: 512,
                extracted_text: Some("stand-up highlights".to_string()),
            }],
        };

        crate::runtime::grants::with_redeem_context(redeem.clone(), async {
            pool.run(
                &company,
                "ceo",
                "@researcher please summarize today's standup notes.",
                &deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect("a budget pause is a graceful stop, not an error")
        })
        .await;

        let marker = crate::runtime::grants::budget_pauses_for(&company)
            .peek("ceo")
            .expect("a re-issue marker must be parked for the paused agent");
        assert_eq!(
            marker.parent, redeem.parent,
            "the marker must carry the ambient cycle's thread parent"
        );
        assert_eq!(
            marker.deliverable, redeem.deliverable,
            "the marker must carry the ambient cycle's deliverable choice"
        );
        assert_eq!(
            marker.mentions, redeem.mentions,
            "the marker must carry the ambient cycle's resolved mentions"
        );
        assert_eq!(
            marker.message,
            redeem.text.clone().unwrap(),
            "the marker must carry the ambient context's RAW text, not this turn's own \
             composed message"
        );
        assert_eq!(
            marker.attachments, redeem.attachments,
            "the marker must carry the ambient context's structured attachments"
        );
    }

    /// No-regression on the delegated path: a turn that finishes normally
    /// (the `ScriptedProvider` returns `Ok`, never an `Err`) reports no
    /// budget pause and parks no marker — the negative control that stops a
    /// hardcoded `Some`/an always-park bug passing every test above.
    #[tokio::test]
    async fn a_turn_that_finishes_normally_reports_no_budget_pause_and_parks_no_marker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let company = CompanyId::new("acme-budget-noregress");
        let mut rec = record();
        rec.id = company.clone();
        let deps = HarnessDeps {
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            provider: Arc::new(ScriptedProvider::new(vec![Ok(
                "Standup notes: all green.".to_string()
            )])),
            provider_slug: "scripted".to_string(),
            serves: None,
            context: Arc::new(MockContext::default()),
            store: Arc::new(RecordingStore::default()),
            meter: None,
            workspace_root: dir.path().to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: dir.path().to_path_buf(),
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
            workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_runs: None,
            deep_trace: None,
            workflow_revisions: None,
            approval_requests: ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
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
            search: None,
            tenant_search: None,
            workspace: None,
        };

        let pool = HarnessPool::new();
        pool.ensure(&rec, &deps).await.expect("pool ensures");

        let outcome = pool
            .run(
                &company,
                "ceo",
                "How did standup go?",
                &deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect("a normal turn returns Ok");

        assert!(
            outcome.budget_paused.is_none(),
            "a turn that never hit an Err must report no budget pause"
        );
        assert!(
            crate::runtime::grants::budget_pauses_for(&company)
                .peek("ceo")
                .is_none(),
            "and must park no re-issue marker"
        );
    }

    // --- MCP-freshness ------------------------------------------------------

    /// In-memory secret store so `ensure` can re-resolve the runtime MCP index.
    #[derive(Default)]
    struct MemSecrets {
        map: StdMutex<std::collections::HashMap<String, String>>,
    }

    #[async_trait]
    impl SecretStore for MemSecrets {
        async fn get(
            &self,
            _c: &CompanyId,
            key: &str,
        ) -> crate::Result<Option<crate::ports::types::SecretValue>> {
            Ok(self
                .map
                .lock()
                .unwrap()
                .get(key)
                .map(|v| crate::ports::types::SecretValue(v.clone())))
        }
        async fn set(
            &self,
            _c: &CompanyId,
            key: &str,
            value: crate::ports::types::SecretValue,
        ) -> crate::Result<()> {
            self.map.lock().unwrap().insert(key.to_string(), value.0);
            Ok(())
        }
    }

    /// A console-added MCP server reaches the agent on the NEXT `ensure`, with no
    /// restart — the roster rebuilds because the effective set, re-resolved from
    /// the LIVE secret store (not the boot snapshot), changed its fingerprint.
    /// This is the Parallel-Search / BrowserBase freshness bug proven end-to-end,
    /// and the CI guard for issue #566: the effective-MCP fingerprint is a *term*
    /// of [`HarnessPool::ensure`]'s staleness check. Both directions are pinned —
    /// an unchanged set holds the fingerprint (no needless rebuild), an MCP-only
    /// change moves it (rebuilt in place, without a restart). A refactor that
    /// drops the term makes the post-change `ensure` early-return without storing
    /// the new fingerprint: the value stops moving across the mutation and the
    /// `assert_ne!` fails, rather than the restart requirement quietly returning.
    #[tokio::test]
    async fn ensure_rebuilds_when_a_runtime_mcp_server_is_added() {
        let secrets: Arc<dyn SecretStore> = Arc::new(MemSecrets::default());
        let dir = tempfile::tempdir().unwrap();
        let deps = HarnessDeps {
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            serves: None,
            context: Arc::new(MockContext::default()),
            store: Arc::new(RecordingStore::default()),
            meter: None,
            workspace_root: dir.path().to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: dir.path().to_path_buf(),
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
            workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_runs: None,
            deep_trace: None,
            workflow_revisions: None,
            approval_requests: ApprovalRequestQueue::default(),
            secrets: Some(secrets.clone()),
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
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
            search: None,
            tenant_search: None,
            workspace: None,
        };
        let pool = HarnessPool::new();
        let rec = record();

        pool.ensure(&rec, &deps).await.expect("first ensure");
        let before = pool
            .mcp_fingerprint_of(&rec.id)
            .await
            .expect("fingerprinted");

        // Stability direction: with no axis changed, a redundant `ensure` is a
        // no-op — the gate reuses the cached roster and the fingerprint holds, so
        // the change-direction assertion below can't pass by coincidence.
        pool.ensure(&rec, &deps).await.expect("redundant ensure");
        assert_eq!(
            pool.mcp_fingerprint_of(&rec.id).await,
            Some(before),
            "an unchanged MCP set must not move the fingerprint"
        );

        // Console-add a runtime MCP server directly into the live secret store.
        crate::company::mcp::save_runtime_index(
            &rec.id,
            secrets.as_ref(),
            &[crate::company::McpServer {
                name: "browserbase".into(),
                endpoint: "https://api.browserbase.com/mcp".into(),
                description: None,
                command: None,
                allowed_tools: Vec::new(),
                disallowed_tools: Vec::new(),
                read_only_tools: Vec::new(),
                timeout_secs: 30,
                enabled: true,
                auth_secret: None,
            }],
        )
        .await
        .unwrap();

        // Change direction: the next ensure re-resolves from the live store →
        // fingerprint changes → roster rebuilt, so the new server reaches the
        // agent without a restart.
        pool.ensure(&rec, &deps).await.expect("post-add ensure");
        let after = pool
            .mcp_fingerprint_of(&rec.id)
            .await
            .expect("fingerprinted");
        assert_ne!(
            before, after,
            "an MCP-only change must move the staleness fingerprint (issue #566)"
        );
        assert_eq!(
            pool.resident_companies().await,
            1,
            "same company, rebuilt in place — not a new residency"
        );

        // Stability after the change too: a further ensure with no new change is
        // a no-op and the fingerprint holds at its post-change value.
        pool.ensure(&rec, &deps).await.expect("final no-op ensure");
        assert_eq!(pool.mcp_fingerprint_of(&rec.id).await, Some(after));
    }

    // --- Bound-repository freshness (issue #245) ----------------------------

    // --- Billing-credential freshness (issues #788, #789) -------------------

    /// Saving or rotating a key in Settings → Billing must reach the agent on
    /// its next turn.
    ///
    /// The fingerprint is the observable that makes "no restart" testable: a
    /// credential that fails to move it leaves the roster cached, and the agent
    /// keeps authenticating with the old key — or holds no billing tools at all
    /// — until the process restarts. That failure is invisible from the tool
    /// list alone, which is why this asserts the fingerprint directly.
    #[tokio::test]
    #[cfg(feature = "chargebee")]
    async fn ensure_rebuilds_when_a_chargebee_credential_is_saved_or_rotated() {
        use crate::chargebee::types::{API_KEY_SECRET, SITE_SECRET};

        let secrets: Arc<dyn SecretStore> = Arc::new(MemSecrets::default());
        let dir = tempfile::tempdir().unwrap();
        let mut deps = deps_with_plan(dir.path(), Arc::new(MockContext::default()), None, None);
        deps.secrets = Some(secrets.clone());

        // The explicit grant is what opens this axis. A `*` wildcard does not
        // confer it — see the module docs.
        let mut rec = record();
        rec.manifest.tools.allow = vec!["chargebee".to_string()];

        let write = |key: &'static str, value: &'static str| {
            let secrets = secrets.clone();
            async move {
                secrets
                    .set(
                        &CompanyId::new("acme"),
                        key,
                        crate::ports::types::SecretValue(value.to_string()),
                    )
                    .await
                    .expect("write secret");
            }
        };

        let pool = HarnessPool::new();
        pool.ensure(&rec, &deps).await.expect("first ensure");
        let unset = pool
            .billing_fingerprint_of(&rec.id)
            .await
            .expect("fingerprint");

        // Stability first, so every change assertion below cannot pass by
        // coincidence.
        pool.ensure(&rec, &deps).await.expect("redundant ensure");
        assert_eq!(
            pool.billing_fingerprint_of(&rec.id).await,
            Some(unset),
            "an unchanged credential must not move the fingerprint"
        );

        // Half a credential is not a connection, so it must not move either —
        // the pair is meaningless apart.
        write(SITE_SECRET, "acme-test").await;
        pool.ensure(&rec, &deps).await.expect("half ensure");
        assert_eq!(
            pool.billing_fingerprint_of(&rec.id).await,
            Some(unset),
            "a site with no key is still no connection"
        );

        // Connect.
        write(API_KEY_SECRET, "cb_first").await;
        pool.ensure(&rec, &deps).await.expect("post-connect ensure");
        let connected = pool
            .billing_fingerprint_of(&rec.id)
            .await
            .expect("fingerprint");
        assert_ne!(unset, connected, "saving a credential must rebuild");

        // Rotate: same site, new key. This is the one a fingerprint over the
        // site alone would miss, leaving the agent on the revoked key.
        write(API_KEY_SECRET, "cb_rotated").await;
        pool.ensure(&rec, &deps).await.expect("post-rotate ensure");
        let rotated = pool
            .billing_fingerprint_of(&rec.id)
            .await
            .expect("fingerprint");
        assert_ne!(
            connected, rotated,
            "a rotation must rebuild even though the site is identical"
        );

        // Disconnect.
        write(API_KEY_SECRET, "").await;
        pool.ensure(&rec, &deps).await.expect("post-clear ensure");
        assert_eq!(
            pool.billing_fingerprint_of(&rec.id).await,
            Some(unset),
            "clearing the key must land back on the unconnected fingerprint"
        );
        assert_eq!(
            pool.resident_companies().await,
            1,
            "same company, rebuilt in place — not a new residency"
        );
    }

    /// A company that does not explicitly grant `chargebee` never reads the
    /// billing secrets, so this axis is inert for it — and a credential sitting
    /// in its store confers nothing. Fail closed, as the module docs promise.
    #[tokio::test]
    #[cfg(feature = "chargebee")]
    async fn a_company_without_the_chargebee_grant_never_moves_on_this_axis() {
        use crate::chargebee::types::{API_KEY_SECRET, SITE_SECRET};

        let secrets: Arc<dyn SecretStore> = Arc::new(MemSecrets::default());
        let dir = tempfile::tempdir().unwrap();
        let mut deps = deps_with_plan(dir.path(), Arc::new(MockContext::default()), None, None);
        deps.secrets = Some(secrets.clone());

        // A wildcard, deliberately: it must NOT confer billing.
        let mut rec = record();
        rec.manifest.tools.allow = vec!["*".to_string()];

        let pool = HarnessPool::new();
        pool.ensure(&rec, &deps).await.expect("first ensure");
        let before = pool
            .billing_fingerprint_of(&rec.id)
            .await
            .expect("fingerprint");

        for (key, value) in [(SITE_SECRET, "acme-test"), (API_KEY_SECRET, "cb_key")] {
            secrets
                .set(
                    &CompanyId::new("acme"),
                    key,
                    crate::ports::types::SecretValue(value.to_string()),
                )
                .await
                .expect("write secret");
        }

        pool.ensure(&rec, &deps).await.expect("post-write ensure");
        assert_eq!(
            pool.billing_fingerprint_of(&rec.id).await,
            Some(before),
            "an ungranted company must not read the billing secrets, let alone rebuild on them"
        );
    }

    // --- Skill-delta freshness (issue #41) ----------------------------------

    /// An in-memory `SkillStateStore` whose delta set a test can mutate between
    /// two `ensure` calls — the same way the console Skills tab authors, edits,
    /// enables, or disables a skill — so the freshness gate can be observed
    /// reacting with no restart.
    #[derive(Default)]
    struct MemSkills {
        deltas: StdMutex<Vec<SkillState>>,
    }

    #[async_trait]
    impl SkillStateStore for MemSkills {
        async fn list(&self, _company: &CompanyId) -> crate::Result<Vec<SkillState>> {
            Ok(self.deltas.lock().unwrap().clone())
        }
        async fn set(&self, _company: &CompanyId, state: &SkillState) -> crate::Result<()> {
            let mut deltas = self.deltas.lock().unwrap();
            match deltas.iter_mut().find(|s| s.slug == state.slug) {
                Some(slot) => *slot = state.clone(),
                None => deltas.push(state.clone()),
            }
            Ok(())
        }
        async fn remove(&self, _company: &CompanyId, slug: &str) -> crate::Result<bool> {
            let mut deltas = self.deltas.lock().unwrap();
            let before = deltas.len();
            deltas.retain(|s| s.slug != slug);
            Ok(deltas.len() != before)
        }
    }

    /// A valid custom-skill delta (its `custom_doc` parses, so `materialize`
    /// writes it to the scratch tree).
    fn custom_skill(slug: &str, enabled: bool, body: &str) -> SkillState {
        SkillState {
            slug: slug.to_string(),
            enabled,
            source: crate::ports::skills_state::SkillSource::Custom,
            custom_doc: Some(body.to_string()),
        }
    }

    const STANDUP_MD: &str =
        "---\nname: Standup Digest\ndescription: Summarize the standup\n---\n\n# Standup Digest\n";

    /// The scratch path a materialized skill lands at for the first roster agent
    /// (`ceo`) under a company's workspace root.
    fn skill_scratch(ws: &std::path::Path, slug: &str) -> std::path::PathBuf {
        ws.join("acme")
            .join("ceo")
            .join("skill-catalog")
            .join("skills")
            .join(slug)
            .join("SKILL.md")
    }

    /// The regression: a skill authored in the console after the first roster
    /// build reaches the agent on the NEXT `ensure` — the fingerprint changes,
    /// the roster rebuilds in place, and the skill's `SKILL.md` materializes —
    /// even though MCP / overlay / capability / composio are all unchanged.
    #[tokio::test]
    async fn ensure_rebuilds_when_a_custom_skill_is_authored() {
        let skills = Arc::new(MemSkills::default());
        let mut fx = fixture();
        fx.deps.skills = Some(skills.clone());
        let ws = fx._dir.path().to_path_buf();
        let pool = HarnessPool::new();
        let rec = record();

        pool.ensure(&rec, &fx.deps).await.expect("first ensure");
        let before = pool
            .skill_fingerprint_of(&rec.id)
            .await
            .expect("fingerprinted");
        assert!(
            !skill_scratch(&ws, "standup-digest").exists(),
            "no skill authored yet"
        );

        // Author a custom skill in the "console" (the live store) — no restart.
        skills
            .set(&rec.id, &custom_skill("standup-digest", true, STANDUP_MD))
            .await
            .unwrap();

        pool.ensure(&rec, &fx.deps).await.expect("second ensure");
        let after = pool
            .skill_fingerprint_of(&rec.id)
            .await
            .expect("fingerprinted");
        assert_ne!(
            before, after,
            "authoring a skill must change the fingerprint"
        );
        assert_eq!(
            pool.resident_companies().await,
            1,
            "same company, rebuilt in place"
        );
        assert!(
            skill_scratch(&ws, "standup-digest").is_file(),
            "the authored skill must surface to the agent with no restart"
        );

        // A third ensure with no change is a no-op (fingerprint stable).
        pool.ensure(&rec, &fx.deps).await.expect("third ensure");
        assert_eq!(pool.skill_fingerprint_of(&rec.id).await, Some(after));
    }

    /// An unchanged delta set across two `ensure` calls keeps the fingerprint
    /// stable and reuses the cached roster (the common fast path).
    #[tokio::test]
    async fn ensure_skill_fast_path_is_stable() {
        let skills = Arc::new(MemSkills::default());
        let rec = record();
        skills
            .set(&rec.id, &custom_skill("standup-digest", true, STANDUP_MD))
            .await
            .unwrap();
        let mut fx = fixture();
        fx.deps.skills = Some(skills.clone());
        let pool = HarnessPool::new();

        pool.ensure(&rec, &fx.deps).await.expect("first ensure");
        let first = pool.skill_fingerprint_of(&rec.id).await.unwrap();
        pool.ensure(&rec, &fx.deps).await.expect("second ensure");
        let second = pool.skill_fingerprint_of(&rec.id).await.unwrap();
        assert_eq!(
            first, second,
            "unchanged deltas keep the fingerprint stable"
        );
        assert_eq!(
            pool.resident_companies().await,
            1,
            "roster reused, not grown"
        );
    }

    /// Disabling a skill in the console drops it from the rebuilt scratch tree
    /// on the next `ensure` (fingerprint moves, `SKILL.md` gone).
    #[tokio::test]
    async fn ensure_rebuilds_when_a_skill_is_disabled() {
        let skills = Arc::new(MemSkills::default());
        let rec = record();
        skills
            .set(&rec.id, &custom_skill("standup-digest", true, STANDUP_MD))
            .await
            .unwrap();
        let mut fx = fixture();
        fx.deps.skills = Some(skills.clone());
        let ws = fx._dir.path().to_path_buf();
        let pool = HarnessPool::new();

        pool.ensure(&rec, &fx.deps).await.expect("first ensure");
        let enabled_fp = pool.skill_fingerprint_of(&rec.id).await.unwrap();
        let path = skill_scratch(&ws, "standup-digest");
        assert!(path.is_file(), "an enabled skill materializes");

        // Disable it in the console.
        skills
            .set(&rec.id, &custom_skill("standup-digest", false, STANDUP_MD))
            .await
            .unwrap();
        pool.ensure(&rec, &fx.deps).await.expect("second ensure");
        let disabled_fp = pool.skill_fingerprint_of(&rec.id).await.unwrap();
        assert_ne!(enabled_fp, disabled_fp, "disabling changes the fingerprint");
        assert!(
            !path.exists(),
            "a disabled skill is dropped from the rebuilt scratch tree"
        );
        assert_eq!(pool.resident_companies().await, 1, "rebuilt in place");
    }

    /// The fingerprint is order-agnostic (the store gives no ordering contract)
    /// but content-sensitive (an edited `custom_doc` must trigger a rebuild).
    #[test]
    fn skill_delta_fingerprint_is_order_agnostic_but_content_sensitive() {
        let a = custom_skill("alpha", true, "---\nname: A\ndescription: a\n---\n");
        let b = custom_skill("beta", true, "---\nname: B\ndescription: b\n---\n");
        assert_eq!(
            skill_delta_fingerprint(&[a.clone(), b.clone()]),
            skill_delta_fingerprint(&[b, a.clone()]),
            "row order must not change the fingerprint"
        );

        let a_edited = custom_skill("alpha", true, "---\nname: A\ndescription: EDITED\n---\n");
        assert_ne!(
            skill_delta_fingerprint(&[a]),
            skill_delta_fingerprint(&[a_edited]),
            "an edited custom_doc must change the fingerprint"
        );
    }

    // --- Overlay-agent freshness (issue #71) --------------------------------

    /// A `CompanyStore` backed by a live, mutable record — so a test can mutate
    /// it between two `ensure` calls the same way the console `POST .../team`
    /// route or the orchestrator's `add_agent` tool would, and observe the
    /// freshness gate react.
    #[derive(Default)]
    struct LiveStore {
        record: StdMutex<Option<CompanyRecord>>,
    }

    #[async_trait]
    impl CompanyStore for LiveStore {
        async fn load(&self, _id: &CompanyId) -> crate::Result<Option<CompanyRecord>> {
            Ok(self.record.lock().unwrap().clone())
        }
        async fn save(&self, record: &CompanyRecord) -> crate::Result<()> {
            *self.record.lock().unwrap() = Some(record.clone());
            Ok(())
        }
        async fn list(&self) -> crate::Result<Vec<CompanySummary>> {
            Ok(Vec::new())
        }
        async fn append_ledger(&self, _id: &CompanyId, _entry: LedgerEntry) -> crate::Result<()> {
            Ok(())
        }
    }

    /// An overlay teammate added through the live company store (the same path
    /// the console `POST .../team` route and the orchestrator's `add_agent` tool
    /// both write through) reaches the roster on the company's NEXT `ensure` —
    /// no restart — mirroring `ensure_rebuilds_when_a_runtime_mcp_server_is_added`.
    #[tokio::test]
    async fn ensure_rebuilds_when_an_overlay_agent_is_added() {
        let live_store = Arc::new(LiveStore::default());
        let rec = record();
        live_store.save(&rec).await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let deps = HarnessDeps {
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            serves: None,
            context: Arc::new(MockContext::default()),
            store: live_store.clone(),
            meter: None,
            workspace_root: dir.path().to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: dir.path().to_path_buf(),
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
            workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_runs: None,
            deep_trace: None,
            workflow_revisions: None,
            approval_requests: ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
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
            search: None,
            tenant_search: None,
            workspace: None,
        };
        let pool = HarnessPool::new();

        pool.ensure(&rec, &deps).await.expect("first ensure");
        let before = pool
            .overlay_fingerprint_of(&rec.id)
            .await
            .expect("fingerprinted");
        assert_eq!(pool.resident_companies().await, 1);
        // The roster is not addressable under "growth" yet.
        assert!(
            pool.run(
                &rec.id,
                "growth",
                "hi",
                &deps,
                crate::runtime::delegation::ChatTarget::default()
            )
            .await
            .is_err(),
            "the overlay teammate must not exist before it is added"
        );

        // Add a teammate directly through the live store — the same write path
        // `AddAgentTool` and the console `POST .../team` route both use.
        let mut updated = rec.clone();
        updated.overlay_agents.push(OverlayAgent {
            id: "growth".into(),
            name: "Jamie".into(),
            role: "Growth Lead".into(),
            description: None,
            tools: None,
            model: None,
            harness: None,
        });
        live_store.save(&updated).await.unwrap();

        // Next ensure re-resolves the live store → fingerprint changes → roster
        // rebuilt, so the new teammate reaches the company without a restart.
        pool.ensure(&rec, &deps).await.expect("second ensure");
        let after = pool
            .overlay_fingerprint_of(&rec.id)
            .await
            .expect("fingerprinted");
        assert_ne!(
            before, after,
            "adding a teammate must change the overlay fingerprint"
        );
        assert_eq!(
            pool.resident_companies().await,
            1,
            "same company, rebuilt in place"
        );

        let reply = pool
            .run(
                &rec.id,
                "growth",
                "hello-marker",
                &deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect("the new teammate is addressable on the very next turn")
            .reply;
        assert!(reply.contains("hello-marker"), "got {reply:?}");

        // A third ensure with no further change is a no-op (fingerprint stable).
        pool.ensure(&rec, &deps).await.expect("third ensure");
        assert_eq!(pool.overlay_fingerprint_of(&rec.id).await, Some(after));
    }

    /// Issue #1455: the roster's approval policy is pinned to the cycle-start
    /// snapshot the native gate was re-applied from, so a console override that
    /// lands mid-turn (after the runtime's store load, before the harness's own
    /// refresh) cannot reach the harness gate a turn early. The override is not
    /// lost — it moves the fingerprint on the NEXT cycle, the same boundary the
    /// native gate moves on.
    #[tokio::test]
    async fn a_cycle_policy_snapshot_wins_over_a_mid_turn_store_edit() {
        let live_store = Arc::new(LiveStore::default());
        let mut rec = record();
        rec.overlay_policy = Some(fp_entry_full(Some("supervised"), None, None, None));
        live_store.save(&rec).await.unwrap();

        let mut fx = fixture();
        fx.deps.store = live_store.clone();
        let pool = HarnessPool::new();

        // The snapshot the runtime loads at the top of the cycle and re-applies
        // to the native gate.
        let snapshot = rec.effective_policy();

        // First cycle: the roster builds against the snapshot.
        pool.ensure_with_policy(&rec, &fx.deps, &snapshot)
            .await
            .expect("first ensure");
        let pinned = pool
            .policy_fingerprint_of(&rec.id)
            .await
            .expect("fingerprinted");

        // A redundant ensure with the same snapshot is a no-op — the stability
        // direction the mid-turn assertion below is read against.
        pool.ensure_with_policy(&rec, &fx.deps, &snapshot)
            .await
            .expect("redundant ensure");
        assert_eq!(pool.policy_fingerprint_of(&rec.id).await, Some(pinned));

        // Mid-window PUT: the store now holds a `full` override. The brain's
        // refresh picks this record up, but the cycle still carries the old
        // snapshot — so the roster must not rebuild against `full` a turn early.
        let mut edited = rec.clone();
        edited.overlay_policy = Some(fp_entry_full(Some("full"), None, None, None));
        live_store.save(&edited).await.unwrap();

        pool.ensure_with_policy(&rec, &fx.deps, &snapshot)
            .await
            .expect("mid-turn ensure");
        assert_eq!(
            pool.policy_fingerprint_of(&rec.id).await,
            Some(pinned),
            "a mid-turn store edit must not reach the roster while the cycle still \
             carries the old snapshot"
        );

        // The NEXT cycle captures the new policy: the fingerprint moves, the
        // roster rebuilds — deferred to the same boundary the native gate moves
        // on, so the change is applied, just not a turn early.
        let next = edited.effective_policy();
        pool.ensure_with_policy(&rec, &fx.deps, &next)
            .await
            .expect("next cycle");
        assert_ne!(
            pool.policy_fingerprint_of(&rec.id).await,
            Some(pinned),
            "the new policy must reach the roster on the next cycle"
        );
    }

    /// The codex P1 regression (commit 11a1f12ed): a manifest `[policy]` edit
    /// with no stored override must still move the roster's policy fingerprint.
    ///
    /// `ensure_with_policy` previously fingerprinted the *synthesized relative
    /// override* against the manifest. When effective == manifest — the
    /// no-override case, or a redundant override a rebuild carried and then
    /// cleared — that synthesis is all-`None`, the empty fingerprint, so the
    /// cache key never moved and the next `ensure` reused the cached roster
    /// (with its old `ApprovalPolicy`) while the native gate already enforced
    /// the new tier. Fingerprinting the effective policy values closes it.
    #[tokio::test]
    async fn a_manifest_policy_edit_rebuilds_the_roster_with_no_override() {
        let live_store = Arc::new(LiveStore::default());
        let rec = record();
        live_store.save(&rec).await.unwrap();

        let mut fx = fixture();
        fx.deps.store = live_store.clone();
        let pool = HarnessPool::new();

        // First cycle: manifest `[policy] mode = "full"`, no override. The
        // snapshot equals the manifest's own policy.
        let snapshot = rec.effective_policy();
        pool.ensure_with_policy(&rec, &fx.deps, &snapshot)
            .await
            .expect("first ensure");
        let pinned = pool
            .policy_fingerprint_of(&rec.id)
            .await
            .expect("fingerprinted");

        // A redundant ensure with the same snapshot is a no-op — the stability
        // direction the manifest edit below is read against.
        pool.ensure_with_policy(&rec, &fx.deps, &snapshot)
            .await
            .expect("redundant ensure");
        assert_eq!(pool.policy_fingerprint_of(&rec.id).await, Some(pinned));

        // Version control edits the manifest tier to `readonly`. No override is
        // stored, so the effective policy IS the manifest itself.
        let mut edited = rec.clone();
        edited.manifest.policy.mode = "readonly".to_string();
        live_store.save(&edited).await.unwrap();

        // The next cycle captures the new effective policy: the fingerprint
        // must move, or the cached roster's `ApprovalPolicy` (still `full`)
        // keeps governing harness tool calls while the native gate already
        // enforces `readonly`.
        let next = edited.effective_policy();
        pool.ensure_with_policy(&edited, &fx.deps, &next)
            .await
            .expect("next cycle");
        assert_ne!(
            pool.policy_fingerprint_of(&rec.id).await,
            Some(pinned),
            "a manifest [policy] edit must move the fingerprint even with no override"
        );
    }

    /// The workflow-runner regression (issue #1455): a plain `ensure` while a
    /// cycle snapshot is pinned cannot adopt a mid-turn console override a turn
    /// early.
    ///
    /// A cycle pins the roster to the policy snapshot the native gate was
    /// re-applied from. The workflow runner drives turns from a spawned task
    /// outside the cycle serial lock and calls plain `ensure`, so without the
    /// pool remembering the pin it would re-resolve the live store, see the
    /// mid-window `full` override, and rebuild the roster against it — running
    /// one turn with the harness gate auto-approving what the native gate still
    /// parks. The pin is what keeps the plain ensure on the cycle's cadence.
    #[tokio::test]
    async fn a_live_ensure_cannot_clobber_a_pinned_cycle_snapshot() {
        let live_store = Arc::new(LiveStore::default());
        let mut rec = record();
        rec.overlay_policy = Some(fp_entry_full(Some("supervised"), None, None, None));
        live_store.save(&rec).await.unwrap();

        let mut fx = fixture();
        fx.deps.store = live_store.clone();
        let pool = HarnessPool::new();

        let snapshot = rec.effective_policy();

        // Cycle 1: the roster pins the strict snapshot.
        pool.ensure_with_policy(&rec, &fx.deps, &snapshot)
            .await
            .expect("cycle ensure");
        let pinned = pool
            .policy_fingerprint_of(&rec.id)
            .await
            .expect("fingerprinted");

        // Mid-window PUT: the store now holds a `full` override, still unseen
        // by the cycle's snapshot.
        let mut edited = rec.clone();
        edited.overlay_policy = Some(fp_entry_full(Some("full"), None, None, None));
        live_store.save(&edited).await.unwrap();

        // The workflow runner's plain ensure fires while the pin is active. It
        // must rebuild against the pin, not the live `full` override.
        pool.ensure(&rec, &fx.deps).await.expect("workflow ensure");
        assert_eq!(
            pool.policy_fingerprint_of(&rec.id).await,
            Some(pinned),
            "a plain ensure must not adopt a mid-cycle override a turn early"
        );

        // The NEXT cycle captures the new policy: the fingerprint moves, the
        // roster rebuilds — the change lands, just at the native gate's own
        // boundary.
        let next = edited.effective_policy();
        pool.ensure_with_policy(&rec, &fx.deps, &next)
            .await
            .expect("next cycle");
        assert_ne!(
            pool.policy_fingerprint_of(&rec.id).await,
            Some(pinned),
            "the new policy must reach the roster on the next cycle"
        );

        // Cycle 2 ends: the pin is released, so a standalone workflow turn
        // between cycles rebuilds against the live override the store already
        // holds instead of a snapshot that would otherwise outlive its cycle.
        pool.end_cycle(&rec.id).await;
        pool.ensure(&rec, &fx.deps)
            .await
            .expect("post-cycle ensure");
        assert_eq!(
            pool.policy_fingerprint_of(&rec.id).await,
            Some(effective_policy_fingerprint(&edited.effective_policy())),
            "after end_cycle a plain ensure must adopt the live override"
        );
    }

    /// The drop-guard half of the release (issue #1455): a cycle cancelled or
    /// unwound through a panic after installing its pin cannot await
    /// `end_cycle`, but the synchronous `release_policy_pin_sync` — what the
    /// guard calls from `Drop` — must clear the pin just the same, so a
    /// standalone workflow turn between cycles rebuilds against the live
    /// override rather than a snapshot the abandoned cycle left behind.
    #[tokio::test]
    async fn a_sync_pin_release_restores_the_live_policy_axis() {
        let live_store = Arc::new(LiveStore::default());
        let mut rec = record();
        rec.overlay_policy = Some(fp_entry_full(Some("supervised"), None, None, None));
        live_store.save(&rec).await.unwrap();

        let mut fx = fixture();
        fx.deps.store = live_store.clone();
        let pool = HarnessPool::new();

        let snapshot = rec.effective_policy();

        // The cycle pins the strict snapshot, exactly as it does on entry.
        pool.ensure_with_policy(&rec, &fx.deps, &snapshot)
            .await
            .expect("cycle ensure");
        let pinned = pool
            .policy_fingerprint_of(&rec.id)
            .await
            .expect("fingerprinted");

        // Mid-window PUT, then the cycle future is dropped without end_cycle:
        // the release must come from the drop guard's sync path.
        let mut edited = rec.clone();
        edited.overlay_policy = Some(fp_entry_full(Some("full"), None, None, None));
        live_store.save(&edited).await.unwrap();
        pool.release_policy_pin_sync(&rec.id);

        pool.ensure(&rec, &fx.deps).await.expect("post-drop ensure");
        assert_eq!(
            pool.policy_fingerprint_of(&rec.id).await,
            Some(effective_policy_fingerprint(&edited.effective_policy())),
            "after a sync release a plain ensure must adopt the live override"
        );
        assert_ne!(
            pool.policy_fingerprint_of(&rec.id).await,
            Some(pinned),
            "the abandoned cycle's snapshot must not outlive its release"
        );
    }

    // --- Capability-budget freshness (issue #108) ---------------------------

    /// A manifest that grants every tool namespace, so the roster actually builds
    /// the exec tools the capability filter then trims. (The default `manifest()`
    /// grants nothing, so no exec tools would be present to gate.)
    fn granting_manifest() -> CompanyManifest {
        toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[tools]
allow = ["shell", "code", "web", "files"]

[[agent]]
id = "ceo"
role = "Chief Executive"
description = "Sets direction."
"#,
        )
        .expect("valid manifest")
    }

    fn granting_record() -> CompanyRecord {
        CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: CompanyId::new("acme"),
            manifest: granting_manifest(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            setup: None,
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            overlay_policy: None,
            overlay_tool_grants: None,
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
            name_confirmed: false,
            activation_completed_at: None,
            created_at_millis: None,
        }
    }

    /// The `ceo` roster agent's live tool names (test introspection via the
    /// public `Agent::tools()` accessor).
    async fn ceo_tool_names(pool: &HarnessPool, id: &CompanyId) -> Vec<String> {
        let guard = pool.agents.read().await;
        let roster = guard.get(id).expect("roster present");
        let ceo = roster
            .iter()
            .find(|a| a.agent_id == "ceo")
            .expect("ceo present");
        let agent = ceo.agent.lock().await;
        agent.tools().iter().map(|t| t.name().to_string()).collect()
    }

    /// End-to-end capability gating: a plan budgeting `shell` at 100 tokens grants
    /// the shell tools while spend is under budget; once a recorded turn pushes
    /// period spend past the threshold, the very next `ensure` rebuilds the roster
    /// with the shell namespace dropped — while intrinsic tools (memory) and the
    /// ungated `files` namespace survive. Mirrors the MCP-freshness test shape.
    #[tokio::test]
    async fn ensure_gates_shell_tools_once_the_token_budget_is_crossed() {
        let dir = tempfile::tempdir().unwrap();
        let meter = Arc::new(RecordingMeter::default());
        let plan = crate::harness::capability_budget::CapabilityPlan {
            period: crate::harness::capability_budget::BudgetPeriod::Daily,
            budgets: std::collections::BTreeMap::from([("shell".to_string(), 100u64)]),
            total_budget: None,
        };
        let deps = HarnessDeps {
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            serves: None,
            context: Arc::new(MockContext::default()),
            store: Arc::new(RecordingStore::default()),
            meter: Some(meter.clone()),
            workspace_root: dir.path().to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: dir.path().to_path_buf(),
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
            workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_runs: None,
            deep_trace: None,
            workflow_revisions: None,
            approval_requests: ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: Some(plan),
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
            search: None,
            tenant_search: None,
            workspace: None,
        };
        let pool = HarnessPool::new();
        let rec = granting_record();

        // First ensure: 0 spend < 100 → shell granted.
        pool.ensure(&rec, &deps).await.expect("first ensure");
        let before_fp = pool
            .capability_fingerprint_of(&rec.id)
            .await
            .expect("fingerprinted");
        let before = ceo_tool_names(&pool, &rec.id).await;
        assert!(before.contains(&"shell".to_string()), "got {before:?}");
        assert!(
            before.contains(&"read_workspace_state".to_string()),
            "got {before:?}"
        );
        // `memory_store`/`memory_recall` are currently withheld altogether
        // (see `harness::build::memory_tools`'s doc comment) — openhuman
        // removed the constructor seam that let either tool act on a
        // company's own `ContextStore` rather than one shared,
        // unconfigured store. `file_read` is this test's example of an
        // intrinsic, ungated tool instead.
        assert!(
            before.contains(&"file_read".to_string()),
            "ungated files namespace must be present: {before:?}"
        );

        // Record a turn that burns 150 inference tokens — past the 100 budget.
        meter
            .record(
                &rec.id,
                &UsageSample {
                    at_millis: crate::ports::now_millis(),
                    agent: "ceo".into(),
                    provider: "managed".into(),
                    input_tokens: 100,
                    output_tokens: 50,
                    cached_input_tokens: 0,
                    cost_usd: 0.0,
                    kind: crate::ports::SampleKind::Inference,
                    run_id: None,
                    model: None,
                },
            )
            .await
            .unwrap();

        // Second ensure: 150 >= 100 → shell exhausted → roster rebuilt without it.
        pool.ensure(&rec, &deps).await.expect("second ensure");
        let after_fp = pool
            .capability_fingerprint_of(&rec.id)
            .await
            .expect("fingerprinted");
        assert_ne!(
            before_fp, after_fp,
            "crossing the budget must change the capability fingerprint"
        );
        assert_eq!(pool.resident_companies().await, 1, "rebuilt in place");

        let after = ceo_tool_names(&pool, &rec.id).await;
        assert!(
            !after.contains(&"shell".to_string()),
            "shell must be gated off once exhausted: {after:?}"
        );
        assert!(
            !after.contains(&"read_workspace_state".to_string()),
            "the whole shell namespace drops: {after:?}"
        );
        assert!(
            after.contains(&"file_read".to_string()),
            "ungated files namespace survives gating: {after:?}"
        );

        // Third ensure with no new spend → no rebuild (fingerprint stable).
        pool.ensure(&rec, &deps).await.expect("third ensure");
        assert_eq!(
            pool.capability_fingerprint_of(&rec.id).await,
            Some(after_fp)
        );
    }

    /// With no plan wired, the capability fingerprint is stable across ensures —
    /// gating stays off, byte-identical to Cell A (no rebuild on this axis).
    #[tokio::test]
    async fn ensure_without_a_plan_never_gates() {
        let fx = fixture();
        let pool = HarnessPool::new();
        let rec = record();
        pool.ensure(&rec, &fx.deps).await.expect("first ensure");
        let fp = pool
            .capability_fingerprint_of(&rec.id)
            .await
            .expect("fingerprinted");
        pool.ensure(&rec, &fx.deps).await.expect("second ensure");
        assert_eq!(
            pool.capability_fingerprint_of(&rec.id).await,
            Some(fp),
            "no plan → stable fingerprint → no capability-driven rebuild"
        );
    }

    /// Builds a `HarnessDeps` carrying the given plan + meter, for the total-
    /// ceiling dispatch tests (issue #188). Everything else is the inert fixture
    /// wiring (mock provider/context, recording store).
    fn deps_with_plan(
        dir: &std::path::Path,
        context: Arc<MockContext>,
        meter: Option<Arc<dyn UsageMeter>>,
        plan: Option<crate::harness::capability_budget::CapabilityPlan>,
    ) -> HarnessDeps {
        HarnessDeps {
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            serves: None,
            context,
            store: Arc::new(RecordingStore::default()),
            meter,
            workspace_root: dir.to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: dir.to_path_buf(),
            model_override: None,
            tasks: None,
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            default_mcp_servers: Vec::new(),
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: DelegationQueue::default(),
            workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_runs: None,
            deep_trace: None,
            workflow_revisions: None,
            approval_requests: ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan,
            media: None,
            composio: None,
            #[cfg(feature = "chargebee")]
            chargebee: None,
            #[cfg(feature = "paypal")]
            paypal: None,
            hosting: None,
            artifacts: None,
            steer: crate::company::steer::InflightRegistry::default(),
            run_supervisor: crate::runtime::RunSupervisor::default(),
            delivery: None,
            search: None,
            tenant_search: None,
            workspace: None,
        }
    }

    /// The hard total-token ceiling (issue #188): once the tenant's total period
    /// spend crosses the plan's `total_budget`, the very next dispatch is refused
    /// **before any model call** — the reply is the fixed operator notice, the
    /// prompt is never echoed (proving the model was not run), and no fabricated
    /// outcome lands in memory. A turn under the ceiling still runs normally.
    #[tokio::test]
    async fn run_refuses_dispatch_once_the_total_ceiling_is_crossed() {
        let dir = tempfile::tempdir().unwrap();
        let context = Arc::new(MockContext::default());
        let meter = Arc::new(RecordingMeter::default());
        let plan = crate::harness::capability_budget::CapabilityPlan {
            period: crate::harness::capability_budget::BudgetPeriod::Daily,
            budgets: std::collections::BTreeMap::new(),
            total_budget: Some(100),
        };
        let deps = deps_with_plan(
            dir.path(),
            context.clone(),
            Some(meter.clone() as Arc<dyn UsageMeter>),
            Some(plan),
        );
        let pool = HarnessPool::new();
        let rec = record();
        pool.ensure(&rec, &deps).await.expect("ensure");

        // Under the ceiling (0 spend < 100): the turn runs and echoes the prompt.
        let ok = pool
            .run(
                &rec.id,
                "ceo",
                "hello-marker",
                &deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect("under-ceiling turn runs")
            .reply;
        assert!(
            ok.contains("hello-marker"),
            "under the ceiling the model runs: {ok:?}"
        );

        // Push total period spend to 150 — past the 100-token ceiling.
        meter
            .record(
                &rec.id,
                &UsageSample {
                    at_millis: crate::ports::now_millis(),
                    agent: "ceo".into(),
                    provider: "managed".into(),
                    input_tokens: 100,
                    output_tokens: 50,
                    cached_input_tokens: 0,
                    cost_usd: 0.0,
                    kind: crate::ports::SampleKind::Inference,
                    run_id: None,
                    model: None,
                },
            )
            .await
            .unwrap();

        let before = context
            .list(&rec.id, memory_loop::OUTCOME_LABEL_PREFIX)
            .await
            .unwrap()
            .len();

        // Over the ceiling: dispatch is refused with a benign notice — NOT an Err.
        let refused = pool
            .run(
                &rec.id,
                "ceo",
                "should-not-echo",
                &deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect("a refusal is a benign outcome, not a hard error")
            .reply;
        assert_eq!(
            refused, TOTAL_BUDGET_EXHAUSTED_NOTICE,
            "the refusal returns the fixed operator notice"
        );
        assert!(
            !refused.contains("should-not-echo"),
            "the model was never called, so the prompt is not echoed: {refused:?}"
        );

        // A refused turn writes no outcome back to memory.
        let after = context
            .list(&rec.id, memory_loop::OUTCOME_LABEL_PREFIX)
            .await
            .unwrap()
            .len();
        assert_eq!(before, after, "a refused turn stores nothing in memory");
    }

    /// Issue #416, the reason [`HarnessPool::total_ceiling_refusal`] was
    /// extracted rather than copied: a confined turn reaches nothing, but it
    /// still spends model tokens, so the tenant's ceiling refuses it exactly as
    /// it refuses a roster dispatch. Without this test the gate could be dropped
    /// from `run_confined` and every other test would stay green — the copilot
    /// would simply keep spending past the cap.
    #[tokio::test]
    async fn a_confined_turn_is_refused_once_the_total_ceiling_is_crossed() {
        let dir = tempfile::tempdir().unwrap();
        let context = Arc::new(MockContext::default());
        let meter = Arc::new(RecordingMeter::default());
        let plan = crate::harness::capability_budget::CapabilityPlan {
            period: crate::harness::capability_budget::BudgetPeriod::Daily,
            budgets: std::collections::BTreeMap::new(),
            total_budget: Some(100),
        };
        let deps = deps_with_plan(
            dir.path(),
            context.clone(),
            Some(meter.clone() as Arc<dyn UsageMeter>),
            Some(plan),
        );
        let pool = HarnessPool::new();
        let rec = record();
        pool.ensure(&rec, &deps).await.expect("ensure");

        let confinement = confine::Confinement::workflow("weekly_report");
        let thread = Some("workflow-copilot:weekly_report");

        // Under the ceiling the copilot answers, so the refusal below is the
        // ceiling talking and not the confined path failing to run at all.
        let ok = pool
            .run_confined(&rec.id, "Acme", "hello-marker", &deps, thread, &confinement)
            .await
            .expect("under-ceiling confined turn runs")
            .reply;
        assert!(
            ok.contains("hello-marker"),
            "under the ceiling the model runs: {ok:?}"
        );

        // Push total period spend past the 100-token ceiling.
        meter
            .record(
                &rec.id,
                &UsageSample {
                    at_millis: crate::ports::now_millis(),
                    agent: "ceo".into(),
                    provider: "managed".into(),
                    input_tokens: 100,
                    output_tokens: 50,
                    cached_input_tokens: 0,
                    cost_usd: 0.0,
                    kind: crate::ports::SampleKind::Inference,
                    run_id: None,
                    model: None,
                },
            )
            .await
            .unwrap();

        let refused = pool
            .run_confined(
                &rec.id,
                "Acme",
                "should-not-echo",
                &deps,
                thread,
                &confinement,
            )
            .await
            .expect("a refusal is a benign outcome, not a hard error")
            .reply;
        assert_eq!(
            refused, TOTAL_BUDGET_EXHAUSTED_NOTICE,
            "the copilot must not keep spending past the tenant ceiling"
        );
        assert!(
            !refused.contains("should-not-echo"),
            "the model was never called, so the prompt is not echoed: {refused:?}"
        );
    }

    /// Fail-closed tradeoff (issue #188): with a total ceiling configured but no
    /// meter to read spend from, the hard refusal does NOT fire — a transient
    /// unreadable-spend condition must not brick every turn. The turn runs (the
    /// per-namespace fail-closed roster already handles exec-tool stripping).
    #[tokio::test]
    async fn run_does_not_refuse_when_spend_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let context = Arc::new(MockContext::default());
        // A zero ceiling would refuse from the first token IF spend were readable;
        // with no meter wired the gate must defer, not brick.
        let plan = crate::harness::capability_budget::CapabilityPlan {
            period: crate::harness::capability_budget::BudgetPeriod::Daily,
            budgets: std::collections::BTreeMap::new(),
            total_budget: Some(0),
        };
        let deps = deps_with_plan(dir.path(), context.clone(), None, Some(plan));
        let pool = HarnessPool::new();
        let rec = record();
        pool.ensure(&rec, &deps).await.expect("ensure");

        let reply = pool
            .run(
                &rec.id,
                "ceo",
                "hello-marker",
                &deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect("no meter must not brick the turn")
            .reply;
        assert!(
            reply.contains("hello-marker"),
            "an unreadable ceiling defers to running the turn: {reply:?}"
        );
        assert_ne!(
            reply, TOTAL_BUDGET_EXHAUSTED_NOTICE,
            "the hard refusal must not fire without a spend read"
        );
    }

    // --- The per-agent daily spend cap at dispatch (issue #304) --------------

    /// A company whose `ceo` carries a $5/day cap and whose `engineer` carries
    /// none — the pair that proves the gate is per-teammate, not per-company.
    fn capped_record() -> CompanyRecord {
        let manifest: CompanyManifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "ceo"
role = "Chief Executive"
description = "Sets direction."
budget_usd_daily = 5.0

[[agent]]
id = "engineer"
role = "Engineer"
description = "Builds the product."
"#,
        )
        .expect("valid manifest");
        CompanyRecord {
            manifest,
            ..record()
        }
    }

    /// A `$usd` inference sample for `agent`, stamped at `at_millis`.
    fn spend_sample(agent: &str, usd: f64, at_millis: u64) -> UsageSample {
        UsageSample {
            at_millis,
            agent: agent.into(),
            provider: "managed".into(),
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 0,
            cost_usd: usd,
            kind: crate::ports::SampleKind::Inference,
            run_id: None,
            model: None,
        }
    }

    /// The heart of #304 at the layer that carries the money: once a teammate
    /// has spent its manifest `budget_usd_daily`, its next dispatch is refused
    /// **before any model call** — while its uncapped colleague keeps working.
    ///
    /// This is the layer that matters, because the dominant spend stream is
    /// inference and inference never reaches a `ToolPolicy`. Gating only priced
    /// tool calls would leave a capped teammate free to burn its budget many
    /// times over on model turns alone.
    #[tokio::test]
    async fn run_refuses_dispatch_for_a_teammate_over_its_daily_cap() {
        let dir = tempfile::tempdir().unwrap();
        let context = Arc::new(MockContext::default());
        let meter = Arc::new(RecordingMeter::default());
        let rec = capped_record();

        // The CEO has spent its whole $5 today. The engineer has spent nothing.
        meter
            .record(
                &rec.id,
                &spend_sample("ceo", 5.00, crate::ports::now_millis()),
            )
            .await
            .unwrap();

        let deps = deps_with_plan(
            dir.path(),
            context.clone(),
            Some(meter.clone() as Arc<dyn UsageMeter>),
            None,
        );
        let pool = HarnessPool::new();
        pool.ensure(&rec, &deps).await.expect("ensure");

        let samples_before = meter.samples.lock().unwrap().len();
        let memory_before = context
            .list(&rec.id, memory_loop::OUTCOME_LABEL_PREFIX)
            .await
            .unwrap()
            .len();

        let refused = pool
            .run(
                &rec.id,
                "ceo",
                "should-not-echo",
                &deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect("a refusal is a benign outcome, not a hard error")
            .reply;
        assert_eq!(
            refused,
            agent_budget_exhausted_notice("ceo", 5.0),
            "the refusal names the teammate, its cap and the reset"
        );
        assert!(
            !refused.contains("should-not-echo"),
            "the model was never called, so the prompt is not echoed: {refused:?}"
        );
        assert_eq!(
            meter.samples.lock().unwrap().len(),
            samples_before,
            "a pre-model-call refusal meters nothing"
        );
        assert_eq!(
            context
                .list(&rec.id, memory_loop::OUTCOME_LABEL_PREFIX)
                .await
                .unwrap()
                .len(),
            memory_before,
            "a refused turn stores no fabricated outcome"
        );

        // The cap is per-teammate: the uncapped engineer is untouched, and the
        // CEO's spend does not count against it.
        let ok = pool
            .run(
                &rec.id,
                "engineer",
                "hello-marker",
                &deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect("an uncapped teammate keeps working")
            .reply;
        assert!(
            ok.contains("hello-marker"),
            "one teammate's exhausted budget must not stop the company: {ok:?}"
        );
    }

    // --- Console tool grants, live (issue #1796) -----------------------------

    /// **The no-restart proof for the one-click grant.** A namespace granted
    /// through the company store — the exact path `PUT …/tools/grants` writes
    /// through — moves the grant fingerprint, so `ensure` rebuilds the roster in
    /// place and the belt the next turn runs with actually has the tools.
    ///
    /// Without this axis every other fingerprint stays stable across a grant,
    /// the fast path returns the cached roster, and the operator watches the
    /// connect page flip to "Connected" while no teammate receives anything
    /// until the process restarts — which is the same "Connected and reaching
    /// nobody" the grant was clicked to end, with a delay attached.
    ///
    /// One pool throughout, never reconstructed (`resident_companies()` stays
    /// 1), so nothing here can be smuggling in a restart.
    #[tokio::test]
    async fn a_tool_grant_written_through_the_store_rebuilds_the_roster_in_place() {
        use crate::ports::types::{Actor, ActorKind, ToolGrantsOverride};

        let dir = tempfile::tempdir().unwrap();
        let context = Arc::new(MockContext::default());
        // A catch-all company: `*` covers shell/code/web and confers none of the
        // five namespaces this route deals in, which is the manifest shape the
        // issue was reported against.
        let mut rec = capped_record();
        rec.manifest.tools.allow = vec!["*".to_string()];

        let live_store = Arc::new(LiveStore::default());
        live_store.save(&rec).await.unwrap();
        let mut deps = deps_with_plan(dir.path(), context.clone(), None, None);
        deps.store = live_store.clone();

        let pool = HarnessPool::new();
        pool.ensure(&rec, &deps).await.expect("ensure before");
        let before = pool
            .grants_fingerprint_of(&rec.id)
            .await
            .expect("fingerprinted");

        // An admin grants `chargebee` from the connect page.
        let mut granted = rec.clone();
        granted.overlay_tool_grants = Some(ToolGrantsOverride {
            added: vec!["chargebee".to_string()],
            set_by: Actor {
                kind: ActorKind::User,
                id: "user-admin".to_string(),
            },
            at_millis: crate::ports::now_millis(),
        });
        granted.manifest.tools.allow = granted.effective_tool_allow();
        live_store.save(&granted).await.unwrap();

        // Deliberately re-`ensure` with the STALE record the caller is holding.
        // A boot-time snapshot is what `HarnessBrain::record` hands in, so the
        // grant must be picked up from the live store read rather than from the
        // record passed in — otherwise this works only for callers that happen
        // to have reloaded.
        pool.ensure(&rec, &deps).await.expect("ensure after");
        let after = pool
            .grants_fingerprint_of(&rec.id)
            .await
            .expect("fingerprinted");
        assert_ne!(
            before, after,
            "granting a namespace must move the grant fingerprint, or the cached \
             roster is reused and no teammate ever receives the tools"
        );
        assert_eq!(
            pool.resident_companies().await,
            1,
            "the same company, rebuilt in place — not a new process"
        );

        // Withdrawing it returns the fingerprint to where it started: the axis
        // tracks the effective list, so a revoked grant is as visible as a
        // granted one.
        pool.ensure(&rec, &deps).await.expect("ensure idempotent");
        assert_eq!(
            pool.grants_fingerprint_of(&rec.id).await,
            Some(after),
            "an unchanged grant set must not churn the roster"
        );
        live_store.save(&rec).await.unwrap();
        pool.ensure(&rec, &deps).await.expect("ensure cleared");
        assert_eq!(
            pool.grants_fingerprint_of(&rec.id).await,
            Some(before),
            "withdrawing the grant must move the fingerprint back"
        );
    }

    /// **The search-backend counterpart of the proof above.** `grants_fp`
    /// (and the roster's own effective-allow-list read) already tolerate a
    /// stale `company` snapshot, because both fold the live override onto
    /// `company`'s base. `resolve_tenant_search` must do the same: a company
    /// snapshot that predates a console `search` grant must still resolve the
    /// backend once the live override is passed in, or the roster ends up
    /// crediting a capability no tool was ever wired for.
    #[tokio::test]
    async fn resolve_tenant_search_honours_a_console_grant_a_stale_company_misses() {
        use crate::ports::types::{Actor, ActorKind, ToolGrantsOverride};

        let dir = tempfile::tempdir().unwrap();
        let context = Arc::new(MockContext::default());

        // The stale snapshot: no explicit `search` grant in its own
        // `[tools].allow`, exactly what a caller holding a boot-time
        // `CompanyRecord` still has after an admin grants `search` from the
        // console without a hot rebuild. `*` covers files/shell/code/web but
        // deliberately not `search` — the same base the grant-fingerprint
        // test above uses.
        let mut rec = record();
        rec.manifest.tools.allow = vec!["*".to_string()];
        assert!(
            !crate::company::grants_search_explicit(&rec.manifest.tools.allow),
            "the fixture must start without an explicit search grant"
        );

        let mut deps = deps_with_plan(dir.path(), context.clone(), None, None);
        // No secret store wired: the fallback path returns the last known
        // connection, standing in for a company whose provider is already on
        // file.
        deps.secrets = None;
        deps.tenant_search = Some(search_byo::TenantSearch::for_test(
            "brave",
            Some("test-key"),
            None,
        ));

        let overlay_tool_grants = ToolGrantsOverride {
            added: vec!["search".to_string()],
            set_by: Actor {
                kind: ActorKind::User,
                id: "user-admin".to_string(),
            },
            at_millis: crate::ports::now_millis(),
        };

        let pool = HarnessPool::new();
        let resolved = pool
            .resolve_tenant_search(&rec, &deps, Some(&overlay_tool_grants))
            .await;
        assert!(
            resolved.is_some(),
            "a console grant the live overlay carries must resolve the search \
             backend even when the `company` snapshot passed in predates it"
        );
    }

    // --- Console budget overrides, live (issue #343) -------------------------

    /// **The no-restart proof.** A daily cap written through the company store —
    /// the exact path `PUT …/team/{id}/budget` writes through — is enforced on
    /// the company's **next dispatch**, in one process, with no restart and no
    /// redeploy.
    ///
    /// This is the whole of #343 at the layer that decides whether a teammate
    /// works. Before it, `budget_usd_daily` was readable only from the manifest,
    /// which is a boot snapshot baked into the tenant image — so an operator
    /// whose teammate had stopped had no remedy short of us shipping a new
    /// image. The four phases walk exactly that operator's day:
    ///
    ///   A. the CEO has spent its manifest $5 and is refused (issue #304, and
    ///      the state that motivates the issue);
    ///   B. an admin **raises** the cap to $50 — the stopped teammate works
    ///      again on its very next turn. This is the acceptance criterion;
    ///   C. the admin sets the cap to **$0** — a real cap of nothing, refused
    ///      from the first cent;
    ///   D. the admin **clears** the cap — an explicitly-uncapped override that
    ///      beats the manifest's $5 even with $5 already spent, so the teammate
    ///      works again.
    ///
    /// C and D are the same route with different bodies and they must not
    /// resolve alike: C refuses, D runs. That is "clearing is distinct from
    /// zeroing" asserted on live behaviour rather than on a type.
    ///
    /// Throughout, the pool holds **one** resident company and is never
    /// reconstructed — `resident_companies()` stays 1 and the same `pool` binding
    /// serves every phase — so the only mechanism that can be carrying these
    /// changes is the budget fingerprint flipping and `ensure` rebuilding the
    /// roster in place. Each phase asserts that fingerprint actually moved.
    #[tokio::test]
    async fn a_budget_written_through_the_store_is_enforced_on_the_next_dispatch() {
        use crate::ports::types::{Actor, ActorKind, BudgetOverride};

        let dir = tempfile::tempdir().unwrap();
        let context = Arc::new(MockContext::default());
        let meter = Arc::new(RecordingMeter::default());
        let rec = capped_record();

        // A live store, so `ensure` re-resolves the overrides the way it does in
        // production. `deps_with_plan`'s default store is inert.
        let live_store = Arc::new(LiveStore::default());
        live_store.save(&rec).await.unwrap();

        // The CEO has already spent its manifest $5 today.
        meter
            .record(
                &rec.id,
                &spend_sample("ceo", 5.00, crate::ports::now_millis()),
            )
            .await
            .unwrap();

        let mut deps = deps_with_plan(
            dir.path(),
            context.clone(),
            Some(meter.clone() as Arc<dyn UsageMeter>),
            None,
        );
        deps.store = live_store.clone();

        // ONE pool for the whole test. Nothing below reconstructs it, so nothing
        // below can be smuggling in a restart.
        let pool = HarnessPool::new();

        /// Writes an override through the store exactly as the console route
        /// does, and returns the record for the next `ensure`.
        fn with_override(base: &CompanyRecord, cap: Option<f64>) -> CompanyRecord {
            let mut next = base.clone();
            next.overlay_budgets = vec![BudgetOverride {
                agent_id: "ceo".to_string(),
                budget_usd_daily: cap,
                set_by: Actor {
                    kind: ActorKind::User,
                    id: "user-admin".to_string(),
                },
                at_millis: crate::ports::now_millis(),
            }];
            next
        }

        // --- A. The manifest cap is spent: the teammate is stopped. ----------
        pool.ensure(&rec, &deps).await.expect("ensure A");
        let fp_manifest = pool
            .budget_fingerprint_of(&rec.id)
            .await
            .expect("fingerprinted");
        let refused = pool
            .run(
                &rec.id,
                "ceo",
                "should-not-echo",
                &deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect("a refusal is a benign outcome")
            .reply;
        assert_eq!(
            refused,
            agent_budget_exhausted_notice("ceo", 5.0),
            "phase A: the manifest's $5 cap is spent, so dispatch is refused"
        );

        // --- B. An admin raises the cap. The teammate works again. -----------
        live_store
            .save(&with_override(&rec, Some(50.0)))
            .await
            .unwrap();
        pool.ensure(&rec, &deps).await.expect("ensure B");
        let fp_raised = pool
            .budget_fingerprint_of(&rec.id)
            .await
            .expect("fingerprinted");
        assert_ne!(
            fp_manifest, fp_raised,
            "phase B: setting a cap must move the budget fingerprint, or the \
             cached roster is reused and the change never reaches the gate"
        );
        assert_eq!(
            pool.resident_companies().await,
            1,
            "phase B: the same company, rebuilt in place — not a new process"
        );
        let unblocked = pool
            .run(
                &rec.id,
                "ceo",
                "hello-marker",
                &deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect("the raised cap unblocks the teammate")
            .reply;
        assert!(
            unblocked.contains("hello-marker"),
            "phase B: raising the cap from the console must unblock the stopped \
             teammate on its very next dispatch, with no restart: {unblocked:?}"
        );

        // --- C. The admin sets the cap to zero. Zero is a real cap. ----------
        live_store
            .save(&with_override(&rec, Some(0.0)))
            .await
            .unwrap();
        pool.ensure(&rec, &deps).await.expect("ensure C");
        let fp_zero = pool
            .budget_fingerprint_of(&rec.id)
            .await
            .expect("fingerprinted");
        assert_ne!(fp_raised, fp_zero, "phase C: lowering a cap is a change");
        let zeroed = pool
            .run(
                &rec.id,
                "ceo",
                "should-not-echo",
                &deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect("a refusal is a benign outcome")
            .reply;
        assert_eq!(
            zeroed,
            agent_budget_exhausted_notice("ceo", 0.0),
            "phase C: a $0 cap refuses from the first cent"
        );

        // --- D. The admin clears the cap. Cleared is not zero. ---------------
        live_store.save(&with_override(&rec, None)).await.unwrap();
        pool.ensure(&rec, &deps).await.expect("ensure D");
        let fp_cleared = pool
            .budget_fingerprint_of(&rec.id)
            .await
            .expect("fingerprinted");
        assert_ne!(
            fp_zero, fp_cleared,
            "phase D: 'no cap' and 'a cap of $0' must not hash alike — if they \
             did, clearing a cap would silently leave the teammate at zero"
        );
        let cleared = pool
            .run(
                &rec.id,
                "ceo",
                "hello-marker",
                &deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect("an explicitly-uncapped teammate runs")
            .reply;
        assert!(
            cleared.contains("hello-marker"),
            "phase D: an explicitly-uncapped override beats the manifest's $5 \
             even with $5 already spent today: {cleared:?}"
        );

        // Nothing above restarted anything.
        assert_eq!(
            pool.resident_companies().await,
            1,
            "one company, rebuilt in place across all four phases"
        );

        // A further `ensure` with no change is a no-op: the axis is not thrashing
        // the roster (and dropping live agent sessions) on every turn.
        pool.ensure(&rec, &deps).await.expect("ensure idempotent");
        assert_eq!(pool.budget_fingerprint_of(&rec.id).await, Some(fp_cleared));
    }

    /// `override_fingerprint` detects a persona edit, keeps `Some("")` distinct
    /// from `None` (the reset-to-blueprint distinction), and does not depend on
    /// the stored order — the `HashMap` the overrides come from has none, so an
    /// order-sensitive hash would rebuild the roster (dropping live sessions) on
    /// a save that changed nothing (issue #1530).
    #[test]
    fn override_fingerprint_detects_edits_and_ignores_order() {
        use crate::ports::types::AgentOverride;
        let entry = |id: &str, text: Option<&str>| AgentOverride {
            agent_id: id.to_string(),
            instructions: text.map(str::to_string),
            ..Default::default()
        };

        assert_ne!(
            override_fingerprint(&[]),
            override_fingerprint(&[entry("ceo", Some("x"))]),
            "adding an override must move the fingerprint"
        );
        assert_ne!(
            override_fingerprint(&[entry("ceo", Some("a"))]),
            override_fingerprint(&[entry("ceo", Some("b"))]),
            "editing the text must move the fingerprint"
        );
        assert_ne!(
            override_fingerprint(&[entry("ceo", Some(""))]),
            override_fingerprint(&[entry("ceo", None)]),
            "an empty-string override and a cleared one must not hash alike"
        );
        let ab = [entry("ceo", Some("a")), entry("eng", Some("b"))];
        let ba = [entry("eng", Some("b")), entry("ceo", Some("a"))];
        assert_eq!(
            override_fingerprint(&ab),
            override_fingerprint(&ba),
            "the fingerprint must not depend on the stored order"
        );
    }

    /// A routing edit — a model or harness re-bind — has to move the persona
    /// fingerprint too: the roster the harness builds reads those fields, so a
    /// re-bind that moved nothing would be silently ignored until the process
    /// restarted (issue #1676 review note). The `Some("")` "cleared" form stays
    /// distinct from `None` ("never edited"), the same discriminant the
    /// reset-to-blueprint contract depends on.
    #[test]
    fn override_fingerprint_moves_on_a_model_or_harness_change() {
        use crate::ports::types::AgentOverride;
        let entry = |model: Option<&str>, harness: Option<&str>| AgentOverride {
            agent_id: "ceo".into(),
            model: model.map(str::to_string),
            harness: harness.map(str::to_string),
            ..Default::default()
        };

        assert_ne!(
            override_fingerprint(&[]),
            override_fingerprint(&[entry(Some("chat-v2"), None)]),
            "a model override must move the fingerprint or the re-bind is ignored until restart"
        );
        assert_ne!(
            override_fingerprint(&[entry(Some("chat-v2"), None)]),
            override_fingerprint(&[entry(None, Some("acp"))]),
            "a harness override must move the fingerprint too"
        );
        assert_ne!(
            override_fingerprint(&[]),
            override_fingerprint(&[entry(Some(""), None)]),
            "an explicit model clear must not hash like an untouched teammate"
        );
        // The same override twice → the same fingerprint (no spurious rebuild).
        assert_eq!(
            override_fingerprint(&[entry(Some("chat-v2"), None)]),
            override_fingerprint(&[entry(Some("chat-v2"), None)])
        );
    }

    /// The persona-override fingerprint is filtered the same way as the overlay
    /// one: a row carrying only a face has no persona text to hash, so choosing
    /// or clearing an avatar for a teammate with no other override must not
    /// rebuild the roster (issue #1676 review note).
    #[test]
    fn override_fingerprint_ignores_an_avatar_only_row() {
        use crate::ports::types::AgentOverride;
        let avatar_only = AgentOverride {
            agent_id: "ceo".into(),
            avatar: Some("tiny:robot".into()),
            ..Default::default()
        };
        let persona = AgentOverride {
            agent_id: "ceo".into(),
            instructions: Some("speak plainly".into()),
            avatar: Some("tiny:robot".into()),
            ..Default::default()
        };

        // A row carrying only a face hashes like no row at all.
        assert_eq!(
            override_fingerprint(&[]),
            override_fingerprint(std::slice::from_ref(&avatar_only)),
            "an avatar-only row must not move the fingerprint"
        );
        // A persona edit still moves it, with or without a face riding along.
        assert_ne!(
            override_fingerprint(&[]),
            override_fingerprint(std::slice::from_ref(&persona)),
            "a persona edit must still move the fingerprint"
        );
        // Two rows differing only in their face hash alike — no spurious rebuild
        // when an operator changes one teammate's avatar.
        assert_eq!(
            override_fingerprint(std::slice::from_ref(&persona)),
            override_fingerprint(std::slice::from_ref(&AgentOverride {
                avatar: Some("tiny:fox".into()),
                ..persona
            })),
            "the face must not be part of the fingerprint"
        );
    }

    /// A persona override written through the store reaches the roster on the
    /// next dispatch — the cache-invalidation the whole feature turns on (#1530).
    /// The pool is never reconstructed (`resident_companies()` stays 1), so the
    /// only thing carrying the edit is `override_fingerprint` flipping and
    /// `ensure` rebuilding the roster in place. Reset-to-blueprint returns the
    /// fingerprint to its pre-edit value, proving the clear is a real change the
    /// roster picks up too.
    #[tokio::test]
    async fn a_persona_override_written_through_the_store_rebuilds_the_roster() {
        use crate::ports::types::AgentOverride;

        let dir = tempfile::tempdir().unwrap();
        let context = Arc::new(MockContext::default());
        let rec = capped_record();

        // A live store, so `ensure` re-resolves the overrides as production does.
        let live_store = Arc::new(LiveStore::default());
        live_store.save(&rec).await.unwrap();

        let mut deps = deps_with_plan(dir.path(), context.clone(), None, None);
        deps.store = live_store.clone();

        // ONE pool for the whole test — nothing below reconstructs it, so nothing
        // below can smuggle in a restart.
        let pool = HarnessPool::new();

        fn with_instructions(base: &CompanyRecord, text: Option<&str>) -> CompanyRecord {
            let mut next = base.clone();
            next.overlay_agent_edits = match text {
                Some(text) => vec![AgentOverride {
                    agent_id: "ceo".to_string(),
                    instructions: Some(text.to_string()),
                    ..Default::default()
                }],
                None => Vec::new(),
            };
            next
        }

        // A. Blueprint — the CEO runs on its manifest persona, no override.
        pool.ensure(&rec, &deps).await.expect("ensure A");
        let fp_blueprint = pool
            .override_fingerprint_of(&rec.id)
            .await
            .expect("fingerprinted");

        // B. An operator edits the CEO's persona from the console.
        live_store
            .save(&with_instructions(&rec, Some("Answer only in haiku.")))
            .await
            .unwrap();
        pool.ensure(&rec, &deps).await.expect("ensure B");
        let fp_edited = pool
            .override_fingerprint_of(&rec.id)
            .await
            .expect("fingerprinted");
        assert_ne!(
            fp_blueprint, fp_edited,
            "editing a persona must move the override fingerprint, or the cached \
             roster is reused and the edit never reaches the next turn"
        );
        assert_eq!(
            pool.resident_companies().await,
            1,
            "the same company, rebuilt in place — not a new process"
        );

        // C. Reset-to-blueprint clears the override; the persona returns to seed.
        live_store
            .save(&with_instructions(&rec, None))
            .await
            .unwrap();
        pool.ensure(&rec, &deps).await.expect("ensure C");
        let fp_reset = pool
            .override_fingerprint_of(&rec.id)
            .await
            .expect("fingerprinted");
        assert_ne!(fp_edited, fp_reset, "clearing the override is a change");
        assert_eq!(
            fp_reset, fp_blueprint,
            "reset-to-blueprint must return to the pre-edit fingerprint"
        );

        // An unchanged `ensure` is a no-op: the axis is not thrashing the roster.
        pool.ensure(&rec, &deps).await.expect("ensure idempotent");
        assert_eq!(pool.override_fingerprint_of(&rec.id).await, Some(fp_reset));
    }

    /// A `PATCH {scope}` rename reaches the roster on the next dispatch (PR
    /// #1875 review finding), mirroring
    /// `a_persona_override_written_through_the_store_rebuilds_the_roster`
    /// immediately above for the company-name axis. Before this fix, no
    /// fingerprint moved on a rename, so the cached roster — and every
    /// agent's persona, which embeds `manifest.company.name` — kept
    /// answering to the old name until an unrelated axis happened to change
    /// or the process restarted.
    #[tokio::test]
    async fn a_company_rename_written_through_the_store_rebuilds_the_roster() {
        let dir = tempfile::tempdir().unwrap();
        let context = Arc::new(MockContext::default());
        let rec = capped_record();
        assert_eq!(rec.manifest.company.name, "Acme");

        // A live store, so `ensure` re-resolves the name as production does —
        // exactly the pattern the persona-override test above uses.
        let live_store = Arc::new(LiveStore::default());
        live_store.save(&rec).await.unwrap();

        let mut deps = deps_with_plan(dir.path(), context.clone(), None, None);
        deps.store = live_store.clone();

        // ONE pool for the whole test — nothing below reconstructs it, so
        // nothing below can smuggle in a restart.
        let pool = HarnessPool::new();

        // A. Blueprint name.
        pool.ensure(&rec, &deps).await.expect("ensure A");
        let fp_before = pool
            .company_name_fingerprint_of(&rec.id)
            .await
            .expect("fingerprinted");

        // B. `PATCH {scope}` renames the company (`server::ops::company_profile`
        // writes straight into `manifest.company.name` and saves).
        let mut renamed = rec.clone();
        renamed.manifest.company.name = "New Name Inc.".to_string();
        live_store.save(&renamed).await.unwrap();
        pool.ensure(&rec, &deps).await.expect("ensure B");
        let fp_after = pool
            .company_name_fingerprint_of(&rec.id)
            .await
            .expect("fingerprinted");

        assert_ne!(
            fp_before, fp_after,
            "renaming the company must move the company-name fingerprint, or the \
             cached roster is reused and every agent's persona keeps the old name \
             until an unrelated axis changes or the process restarts"
        );
        assert_eq!(
            pool.resident_companies().await,
            1,
            "the same company, rebuilt in place — not a new process"
        );

        // An unchanged `ensure` is a no-op: the axis is not thrashing the roster.
        pool.ensure(&rec, &deps).await.expect("ensure idempotent");
        assert_eq!(
            pool.company_name_fingerprint_of(&rec.id).await,
            Some(fp_after)
        );
    }

    /// An **overlay** teammate — one added from the console, with no manifest
    /// row — can be capped through the same override, and is refused when it has
    /// spent it. Before #343 an overlay teammate was unconditionally uncapped
    /// ("overlay teammates are uncapped in v1"), so this is a capability that did
    /// not exist rather than a behaviour that changed.
    #[tokio::test]
    async fn an_overlay_teammate_can_be_capped_from_the_console() {
        use crate::ports::types::{Actor, ActorKind, BudgetOverride};

        let dir = tempfile::tempdir().unwrap();
        let context = Arc::new(MockContext::default());
        let meter = Arc::new(RecordingMeter::default());

        let mut rec = record();
        rec.overlay_agents.push(OverlayAgent {
            id: "growth".into(),
            name: "Jamie".into(),
            role: "Growth Lead".into(),
            description: None,
            tools: None,
            model: None,
            harness: None,
        });
        let live_store = Arc::new(LiveStore::default());
        live_store.save(&rec).await.unwrap();

        let mut deps = deps_with_plan(
            dir.path(),
            context.clone(),
            Some(meter.clone() as Arc<dyn UsageMeter>),
            None,
        );
        deps.store = live_store.clone();
        let pool = HarnessPool::new();

        // Uncapped to begin with: it answers.
        pool.ensure(&rec, &deps).await.expect("ensure");
        let reply = pool
            .run(
                &rec.id,
                "growth",
                "hello-marker",
                &deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect("an uncapped overlay teammate answers")
            .reply;
        assert!(reply.contains("hello-marker"), "got {reply:?}");

        // The operator caps it at $1 and it has already spent $2.
        meter
            .record(
                &rec.id,
                &spend_sample("growth", 2.00, crate::ports::now_millis()),
            )
            .await
            .unwrap();
        let mut capped = rec.clone();
        capped.overlay_budgets = vec![BudgetOverride {
            agent_id: "growth".to_string(),
            budget_usd_daily: Some(1.0),
            set_by: Actor {
                kind: ActorKind::User,
                id: "user-admin".to_string(),
            },
            at_millis: crate::ports::now_millis(),
        }];
        live_store.save(&capped).await.unwrap();

        pool.ensure(&rec, &deps).await.expect("ensure again");
        let refused = pool
            .run(
                &rec.id,
                "growth",
                "should-not-echo",
                &deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect("a refusal is a benign outcome")
            .reply;
        assert_eq!(
            refused,
            agent_budget_exhausted_notice("growth", 1.0),
            "a console-added teammate is capped by the same gate as a manifest one"
        );
    }

    /// Fail-open pin, mirroring #188's documented tradeoff exactly: with a cap
    /// set but spend unreadable, the turn RUNS.
    ///
    /// A `$0` cap would refuse from the first cent if spend were readable, so a
    /// meter that errors is the only reason this turn can proceed. Bricking a
    /// teammate's cognition on a flaky read is a strictly worse failure mode
    /// than one day of overspend — and unlike the policy arm's park, a turn-level
    /// refusal offers the operator nothing to approve.
    #[tokio::test]
    async fn run_does_not_refuse_a_capped_teammate_when_spend_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let context = Arc::new(MockContext::default());
        let manifest: CompanyManifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "ceo"
role = "Chief Executive"
description = "Sets direction."
budget_usd_daily = 0.0
"#,
        )
        .expect("valid manifest");
        let rec = CompanyRecord {
            manifest,
            ..record()
        };

        let deps = deps_with_plan(
            dir.path(),
            context.clone(),
            Some(Arc::new(FailingMeter) as Arc<dyn UsageMeter>),
            None,
        );
        let pool = HarnessPool::new();
        pool.ensure(&rec, &deps).await.expect("ensure");

        let reply = pool
            .run(
                &rec.id,
                "ceo",
                "hello-marker",
                &deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect("an unreadable budget must not brick the teammate")
            .reply;
        assert!(
            reply.contains("hello-marker"),
            "an unreadable cap defers to running the turn: {reply:?}"
        );

        // ...and with no meter at all, the same deferral.
        let no_meter = deps_with_plan(dir.path(), context.clone(), None, None);
        let pool = HarnessPool::new();
        pool.ensure(&rec, &no_meter).await.expect("ensure");
        let reply = pool
            .run(
                &rec.id,
                "ceo",
                "hello-marker",
                &no_meter,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect("no meter must not brick the teammate")
            .reply;
        assert!(reply.contains("hello-marker"), "no meter defers: {reply:?}");
    }

    /// The cap is the UTC calendar day: yesterday's $9 does not refuse today's
    /// first turn. Depends on `RecordingMeter` honouring `since_millis`.
    #[tokio::test]
    async fn a_yesterday_stamped_spend_does_not_refuse_todays_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let context = Arc::new(MockContext::default());
        let meter = Arc::new(RecordingMeter::default());
        let rec = capped_record();

        let yesterday =
            crate::metering::utc_day_start_millis(crate::ports::now_millis()).saturating_sub(1);
        meter
            .record(&rec.id, &spend_sample("ceo", 9.00, yesterday))
            .await
            .unwrap();

        let deps = deps_with_plan(
            dir.path(),
            context.clone(),
            Some(meter.clone() as Arc<dyn UsageMeter>),
            None,
        );
        let pool = HarnessPool::new();
        pool.ensure(&rec, &deps).await.expect("ensure");

        let reply = pool
            .run(
                &rec.id,
                "ceo",
                "hello-marker",
                &deps,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect("a new day admits the turn")
            .reply;
        assert!(
            reply.contains("hello-marker"),
            "the cap resets at 00:00Z; yesterday's spend is spent: {reply:?}"
        );
    }

    // -----------------------------------------------------------------------
    // The approval gate's coverage over the live toolbelt (issue #443)
    // -----------------------------------------------------------------------

    /// Build one agent and return the tools it actually received.
    ///
    /// A local mirror of `build`'s own `built_tool_names` — that one is private
    /// to its test module, and this file owns `deps_with_plan`, which is the
    /// expensive half.
    fn belt(grants: &[&str], is_orchestrator: bool, wire_everything: bool) -> Vec<String> {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut deps = deps_with_plan(dir.path(), Arc::new(MockContext::default()), None, None);
        if wire_everything {
            // The three tool families gated on a wired dependency rather than
            // on a cargo feature. Without these the belt is missing exactly the
            // tools most likely to be misclassified — the workspace writes and
            // the priced search.
            deps.workspace = Some(Arc::new(crate::store::FsOps::new(dir.path())));
            deps.artifacts = Some(Arc::new(crate::store::FsOps::new(dir.path())));
            deps.search = Some(crate::harness::search::SearchBackend::new(
                "https://api.example.test".to_string(),
                crate::company::credentials::Credential::from_value("managed-platform-token"),
                crate::company::DEFAULT_SEARCH_DAILY_CALLS,
            ));
            // A registered MCP server is what puts `mcp_list_servers`,
            // `mcp_list_tools` and `mcp_call_tool` on the belt — the three
            // tools issue #443 is about. Without one the coverage check would
            // pass while never having looked at them.
            // A skills source dir is what puts `list_skills`, `describe_skill`
            // and `read_skill_resource` on the belt (named for skills since
            // issue #845; upstream calls them `*_workflow*`). Leaving it `None`
            // is how those three stayed invisible to this check while
            // `describe_workflow` parked in production.
            let company_src = dir.path().join("company-src");
            std::fs::create_dir_all(company_src.join("skills").join("brief")).expect("skill dir");
            std::fs::write(
                company_src.join("skills").join("brief").join("SKILL.md"),
                "---\nname: brief\ndescription: Write a brief\n---\n\nWrite one.\n",
            )
            .expect("skill file");
            deps.skills_source_dir = Some(company_src);
            deps.mcp_servers = vec![McpServerDecl {
                name: "notes".to_string(),
                endpoint: "https://mcp.example.test".to_string(),
                description: None,
                allowed_tools: Vec::new(),
                disallowed_tools: Vec::new(),
                read_only_tools: Vec::new(),
                timeout_secs: 30,
                enabled: true,
                source: crate::company::mcp::McpSource::Runtime,
                auth: crate::company::mcp::AuthMaterial::None,
            }];
        }
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
        let agent = build::build_agent(
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
        agent.tools().iter().map(|t| t.name().to_string()).collect()
    }

    /// **The mechanism issue #443 asks for.** Every tool this crate can put in
    /// front of an agent must be classified in
    /// [`crate::policy::consequence`], or this fails.
    ///
    /// Three families had needed the same carve-out before it, each added after
    /// somebody hit it, and what the gate did with the ones nobody hit was
    /// silent: the tool simply started asking for permission, and whoever
    /// noticed was an operator wondering why a read needed approving. That is
    /// how `mcp_list_servers` — which the agent persona *instructs* every agent
    /// to call — came to cost an approval, and how `file_read`, `glob` and
    /// `grep` came to park with nobody reporting it.
    ///
    /// A tool declaring its own consequence to the gate at call time would be
    /// better, and is not reachable: openhuman's `ToolPolicy` surface hands the
    /// bridge a name and arguments, never the tool. So the declaration is
    /// checked against the live belt here instead — the issue's own stated
    /// fallback, "exhaustive by construction rather than by memory".
    ///
    /// Feature-aware by construction: it enumerates whatever this build wires,
    /// so a family behind a cargo feature is covered by the lane that enables
    /// it rather than by a `cfg` branch that has to be kept in step.
    #[test]
    fn every_registered_tool_is_declared() {
        let declared: std::collections::BTreeSet<&str> =
            crate::policy::consequence::declared_tools().collect();
        let mut live: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for (grants, orchestrator, everything) in [
            (&["*"][..], false, false),
            (&["*"][..], true, false),
            (&["*"][..], false, true),
            (&["*"][..], true, true),
            // `*` deliberately does not reach MCP (`grants_cover_server` treats
            // it as an explicit opt-in), so a family grant is what wires the
            // three bridge tools here — the same `mcp:*` a company would use.
            (
                &["workspace", "search", "media", "composio", "mcp:*"][..],
                false,
                true,
            ),
        ] {
            live.extend(belt(grants, orchestrator, everything));
        }
        // A vacuity guard with teeth. `!live.is_empty()` would not notice the
        // belt quietly narrowing to the tools nobody was worried about, and
        // three of the four names below are the ones the issues are about.
        for expected in [
            "shell",
            "workspace_write",
            "file_read",
            "describe_skill",
            #[cfg(feature = "mcp")]
            "mcp_list_servers",
            #[cfg(feature = "mcp")]
            "mcp_call_tool",
        ] {
            assert!(
                live.contains(expected),
                "the belt builder stopped wiring `{expected}`, so this check has \
                 narrowed without anyone deciding to narrow it: {live:?}"
            );
        }
        let undeclared: Vec<&String> = live
            .iter()
            .filter(|name| !declared.contains(name.as_str()))
            .collect();
        assert!(
            undeclared.is_empty(),
            "these tools are wired onto a live agent but nobody has said what they can \
             reach, so the gate is guessing from their names and they cannot be granted \
             standing: {undeclared:?}. Add them to `crate::policy::consequence::DECLARED`."
        );
    }

    /// The one-directional cross-check on the declaration.
    ///
    /// A tool's own `permission_level()` is NOT trustworthy as the authority —
    /// it defaults to `ReadOnly`, and upstream tools that plainly mutate
    /// (`git_operations`, `memory_store`) never override it, so believing a
    /// `ReadOnly` claim would wave a write straight through the gate. But the
    /// claims in the *other* direction are deliberate: nothing declares itself
    /// `Execute` or `Dangerous` by accident. So those are checked, and a
    /// `ReadOnly` claim is ignored.
    #[test]
    fn nothing_that_declares_itself_executable_is_internal_or_grantable() {
        use oh::tools::traits::PermissionLevel;
        let dir = tempfile::tempdir().expect("tempdir");
        let deps = deps_with_plan(dir.path(), Arc::new(MockContext::default()), None, None);
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
        let agent = build::build_agent(
            &CompanyId::new("acme"),
            "Acme",
            &manifest_agent,
            ApprovalPolicy::new(&Policy::default(), None),
            &deps,
            &["*".to_string()],
            &[],
            &[],
            None,
            true,
        )
        .expect("agent builds");
        let args = serde_json::json!({});
        let mut checked = 0;
        for tool in agent.tools() {
            if !matches!(
                tool.permission_level(),
                PermissionLevel::Execute | PermissionLevel::Dangerous
            ) {
                continue;
            }
            checked += 1;
            let verdict = crate::policy::consequence_of(tool.name(), &args);
            assert!(
                verdict.reach.denied_under_readonly(),
                "`{}` declares itself executable but a read-only desk would allow it",
                tool.name()
            );
            assert!(
                !verdict.standing.is_grantable(),
                "`{}` declares itself executable and must not be grantable",
                tool.name()
            );
        }
        assert!(checked > 0, "no executable tool was on the belt to check");
    }

    // --- Per-company billing resolution (issues #788, #789) -----------------
    //
    // `resolve_chargebee` / `resolve_paypal` are what actually decide whether a
    // company's agents get billing tools on a given turn — `HarnessPool::ensure`
    // re-resolves them every turn, and `RuntimeBuilder::build` runs the same
    // three-way decision once at boot. All three branches are silent when they
    // go wrong: a dropped grant check wires tools the manifest never allowed, and
    // a read error collapsed into "no credential" disconnects a working
    // integration on one transient store hiccup.

    /// A secret store that reads back what was seeded, or fails every read.
    #[cfg(any(feature = "chargebee", feature = "paypal"))]
    #[derive(Default)]
    struct BillingSecrets {
        map: StdMutex<std::collections::HashMap<String, String>>,
        fail: bool,
    }

    #[cfg(any(feature = "chargebee", feature = "paypal"))]
    #[async_trait]
    impl SecretStore for BillingSecrets {
        async fn get(
            &self,
            _c: &CompanyId,
            key: &str,
        ) -> crate::Result<Option<crate::ports::types::SecretValue>> {
            if self.fail {
                return Err(crate::error::OpenCompanyError::Store(
                    "the secret store is unreachable".into(),
                ));
            }
            Ok(self
                .map
                .lock()
                .unwrap()
                .get(key)
                .map(|v| crate::ports::types::SecretValue(v.clone())))
        }
        async fn set(
            &self,
            _c: &CompanyId,
            key: &str,
            value: crate::ports::types::SecretValue,
        ) -> crate::Result<()> {
            self.map.lock().unwrap().insert(key.to_string(), value.0);
            Ok(())
        }
    }

    /// A company whose manifest allows exactly `grants`.
    #[cfg(any(feature = "chargebee", feature = "paypal"))]
    fn record_granting(grants: &[&str]) -> CompanyRecord {
        let mut rec = record();
        rec.manifest.tools.allow = grants.iter().map(|g| g.to_string()).collect();
        rec
    }

    /// The inert fixture deps, with a secret store and a "last known" connection.
    #[cfg(any(feature = "chargebee", feature = "paypal"))]
    fn billing_deps(dir: &std::path::Path, secrets: Arc<dyn SecretStore>) -> HarnessDeps {
        let mut deps = deps_with_plan(dir, Arc::new(MockContext::default()), None, None);
        deps.secrets = Some(secrets);
        deps
    }

    #[cfg(feature = "chargebee")]
    #[tokio::test]
    async fn chargebee_resolves_only_for_a_company_that_grants_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let secrets = Arc::new(BillingSecrets::default());
        secrets
            .set(
                &CompanyId::new("acme"),
                crate::chargebee::types::SITE_SECRET,
                crate::ports::types::SecretValue("acme-test".into()),
            )
            .await
            .expect("seed");
        secrets
            .set(
                &CompanyId::new("acme"),
                crate::chargebee::types::API_KEY_SECRET,
                crate::ports::types::SecretValue("cb_key".into()),
            )
            .await
            .expect("seed");
        let deps = billing_deps(dir.path(), secrets);
        let pool = HarnessPool::new();

        // Granted and configured: the credential resolves.
        let granted = pool
            .resolve_chargebee(&record_granting(&["chargebee"]), &deps)
            .await
            .expect("a granted, configured company resolves");
        assert_eq!(granted.site(), "acme-test");

        // Same credentials, no grant. The store is untouched — the gate is the
        // manifest, so a company that never opted in gets no tools however well
        // configured the host happens to be.
        assert!(
            pool.resolve_chargebee(&record_granting(&[]), &deps)
                .await
                .is_none(),
            "an ungranted company must resolve nothing"
        );

        // And a wildcard is not a grant: these tools send invoices to real
        // people, so they are opted into by name rather than riding in on the
        // `*` somebody set for file and shell tools.
        assert!(
            pool.resolve_chargebee(&record_granting(&["*"]), &deps)
                .await
                .is_none(),
            "a catch-all grant must not confer chargebee"
        );
    }

    #[cfg(feature = "chargebee")]
    #[tokio::test]
    async fn a_chargebee_store_hiccup_keeps_the_last_known_connection() {
        // The distinction this pins: absence wires no tools, but a READ FAILURE
        // keeps whatever was already resolved. Collapsing the two would drop a
        // working company's billing tools mid-conversation on one bad read, and
        // silently — the agent would simply stop being able to invoice.
        let dir = tempfile::tempdir().expect("tempdir");
        let secrets = Arc::new(BillingSecrets {
            fail: true,
            ..Default::default()
        });
        let mut deps = billing_deps(dir.path(), secrets);
        let last_known = crate::harness::chargebee::TenantChargebee::resolve(
            &(Arc::new(BillingSecrets {
                map: StdMutex::new(
                    [
                        (
                            crate::chargebee::types::SITE_SECRET.to_string(),
                            "acme-test".to_string(),
                        ),
                        (
                            crate::chargebee::types::API_KEY_SECRET.to_string(),
                            "cb_key".to_string(),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                ),
                fail: false,
            }) as Arc<dyn SecretStore>),
            &CompanyId::new("acme"),
        )
        .await
        .expect("the seeded store reads")
        .expect("both halves present");
        deps.chargebee = Some(last_known);

        let kept = pool_resolve_chargebee(&deps).await;
        assert_eq!(
            kept.map(|c| c.site().to_string()).as_deref(),
            Some("acme-test"),
            "a transient read failure must not disconnect a working integration"
        );
    }

    #[cfg(feature = "chargebee")]
    async fn pool_resolve_chargebee(
        deps: &HarnessDeps,
    ) -> Option<crate::harness::chargebee::TenantChargebee> {
        HarnessPool::new()
            .resolve_chargebee(&record_granting(&["chargebee"]), deps)
            .await
    }

    #[cfg(feature = "paypal")]
    #[tokio::test]
    async fn paypal_resolves_only_for_a_company_that_grants_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let secrets = Arc::new(BillingSecrets::default());
        for (key, value) in [
            (crate::company::paypal::CLIENT_ID_SECRET, "AY_id"),
            (crate::company::paypal::CLIENT_SECRET_SECRET, "EL_secret"),
        ] {
            secrets
                .set(
                    &CompanyId::new("acme"),
                    key,
                    crate::ports::types::SecretValue(value.into()),
                )
                .await
                .expect("seed");
        }
        let deps = billing_deps(dir.path(), secrets);
        let pool = HarnessPool::new();

        assert!(
            pool.resolve_paypal(&record_granting(&["paypal"]), &deps)
                .await
                .is_some(),
            "a granted, configured company resolves"
        );
        assert!(
            pool.resolve_paypal(&record_granting(&[]), &deps)
                .await
                .is_none(),
            "an ungranted company must resolve nothing"
        );
        assert!(
            pool.resolve_paypal(&record_granting(&["*"]), &deps)
                .await
                .is_none(),
            "a catch-all grant must not confer paypal"
        );
    }

    #[cfg(feature = "paypal")]
    #[tokio::test]
    async fn a_paypal_grant_with_no_credential_wires_nothing_rather_than_failing() {
        // Fail closed: a manifest that grants `paypal` on a host where nobody
        // has saved a credential must wire no tools, not tools that fail on
        // first use — an agent that HAS a wallet tool tells the operator the
        // balance is unavailable, rather than that it cannot read wallets.
        let dir = tempfile::tempdir().expect("tempdir");
        let deps = billing_deps(dir.path(), Arc::new(BillingSecrets::default()));
        assert!(
            HarnessPool::new()
                .resolve_paypal(&record_granting(&["paypal"]), &deps)
                .await
                .is_none()
        );
    }

    /// End-to-end proof that a chat reply is assembled WITH this desk's recent
    /// journaled history in front of the model (issue #1840), driven through the
    /// real `HarnessPool::run` path with only the model captured.
    ///
    /// Each test is RED on the pre-fix code: the old switch branch re-seeded via
    /// OpenHuman's `seed_resume_from_thread_transcript`, which reads a file
    /// OpenCompany never writes for a `chat_id`, so the model saw `history_len =
    /// 0` and none of these markers reached it.
    mod chat_seed_regression {
        use super::*;

        use std::sync::Mutex as StdMutex;

        use futures::stream::{self, BoxStream};
        use tinyinference::model::{ModelRequest, ModelResponse};

        use crate::ports::events::EventStreamItem;
        use crate::ports::types::{CompanyEvent, EventSeq, StoredEvent};

        /// An appendable in-memory journal. `read_from` returns ascending order,
        /// so the trait's default `read_before` yields the newest-first paging the
        /// seed projector walks.
        ///
        /// `reads` counts every `read_from` call (the default `read_before`'s
        /// only path into a backend) — a stand-in for the filesystem backend's
        /// whole-file JSONL scan (`store::fs::read_before`'s docs), so a test
        /// can assert the seed projector only walks the journal when a chat
        /// switch actually needs it, not on every chat turn (codex review
        /// finding).
        #[derive(Default)]
        struct InMemoryLog {
            events: StdMutex<Vec<StoredEvent>>,
            reads: std::sync::atomic::AtomicUsize,
        }

        impl InMemoryLog {
            fn reads(&self) -> usize {
                self.reads.load(std::sync::atomic::Ordering::SeqCst)
            }

            fn operator(&self, chat: &str, text: &str) {
                self.push(CompanyEvent::OperatorMessage {
                    text: text.to_string(),
                    by: None,
                    chat: Some(chat.to_string()),
                    parent: None,
                    deliverable: None,
                    mentions: Vec::new(),
                    attachments: Vec::new(),
                });
            }
            /// An operator message posted inside the thread rooted at `parent`.
            fn operator_in(&self, chat: &str, text: &str, parent: u64) {
                self.push(CompanyEvent::OperatorMessage {
                    text: text.to_string(),
                    by: None,
                    chat: Some(chat.to_string()),
                    parent: Some(EventSeq::new(parent)),
                    deliverable: None,
                    mentions: Vec::new(),
                    attachments: Vec::new(),
                });
            }
            fn reply(&self, chat_id: &str, text: &str) {
                self.push(CompanyEvent::AgentReply {
                    chat_id: chat_id.to_string(),
                    agent_id: "ceo".to_string(),
                    text: text.to_string(),
                    steps: Vec::new(),
                    task_id: None,
                    parent: None,
                    mentions: Vec::new(),
                    mention_depth: 0,
                });
            }
            fn push(&self, event: CompanyEvent) {
                let mut log = self.events.lock().unwrap();
                let seq = EventSeq::new(log.len() as u64);
                log.push(StoredEvent {
                    seq,
                    company: CompanyId::new("acme"),
                    event,
                    at_millis: seq.value(),
                });
            }
        }

        #[async_trait]
        impl EventLog for InMemoryLog {
            async fn append(
                &self,
                _id: &CompanyId,
                event: CompanyEvent,
            ) -> crate::Result<EventSeq> {
                let mut log = self.events.lock().unwrap();
                let seq = EventSeq::new(log.len() as u64);
                log.push(StoredEvent {
                    seq,
                    company: CompanyId::new("acme"),
                    event,
                    at_millis: seq.value(),
                });
                Ok(seq)
            }
            async fn read_from(
                &self,
                _id: &CompanyId,
                seq: EventSeq,
                limit: usize,
            ) -> crate::Result<Vec<StoredEvent>> {
                self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(self
                    .events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|e| e.seq.value() >= seq.value())
                    .take(limit)
                    .cloned()
                    .collect())
            }
            fn subscribe(&self, _id: &CompanyId) -> BoxStream<'static, EventStreamItem> {
                Box::pin(stream::empty())
            }
        }

        /// A model that records the full text of every request it is handed, so a
        /// test can assert which prior turns reached the model's context.
        #[derive(Default)]
        struct RecordingProvider {
            seen: Arc<StdMutex<Vec<String>>>,
        }

        #[async_trait]
        impl ChatModel<()> for RecordingProvider {
            async fn invoke(
                &self,
                _state: &(),
                request: ModelRequest,
            ) -> tinyinference::Result<ModelResponse> {
                let joined = request
                    .messages
                    .iter()
                    .map(|m| m.text())
                    .collect::<Vec<_>>()
                    .join("\n");
                self.seen.lock().unwrap().push(joined);
                // A fixed non-empty reply: empty would trip the empty-response
                // retry wrapper into a second invoke.
                Ok(ModelResponse::assistant("ok"))
            }
        }

        impl HarnessModel for RecordingProvider {
            fn telemetry_provider_id(&self) -> String {
                "recording".to_string()
            }
        }

        /// A fixture whose journal and model are observable: the returned `log` is
        /// pre-populated by the test, and `seen` collects every model request.
        fn recording_fixture() -> (Fixture, Arc<InMemoryLog>, Arc<StdMutex<Vec<String>>>) {
            let mut fx = fixture();
            let log = Arc::new(InMemoryLog::default());
            let provider = Arc::new(RecordingProvider::default());
            let seen = provider.seen.clone();
            fx.deps.events = Some(log.clone());
            fx.deps.provider = provider;
            (fx, log, seen)
        }

        /// A — fresh process, first chat turn on `general` (bound = None): the
        /// prior journaled exchange is seeded into the model, and the current
        /// message (already journaled) is not duplicated.
        #[tokio::test]
        async fn first_chat_turn_seeds_prior_journaled_exchange() {
            let (fx, log, seen) = recording_fixture();
            let rec = record();
            log.operator("general", "PRIOR_USER_MARKER");
            log.reply("general", "PRIOR_AGENT_MARKER");
            // The current operator message is journaled BEFORE the turn runs, just
            // as the server does — so the projector sees it as the newest event.
            log.operator("general", "CURRENT_MARKER");

            let pool = HarnessPool::new();
            pool.ensure(&rec, &fx.deps).await.expect("ensure");
            pool.run(
                &rec.id,
                "ceo",
                "CURRENT_MARKER",
                &fx.deps,
                crate::runtime::delegation::ChatTarget::channel(Some("general")),
            )
            .await
            .expect("chat turn runs");

            let all = seen.lock().unwrap().join("\n===\n");
            assert!(
                all.contains("PRIOR_USER_MARKER") && all.contains("PRIOR_AGENT_MARKER"),
                "the prior journaled exchange must reach the model: {all:?}"
            );
            assert_eq!(
                all.matches("CURRENT_MARKER").count(),
                1,
                "the current message is stripped from the seed, so it appears once \
                 (as this turn's user message), not duplicated: {all:?}"
            );
        }

        /// B — an unthreaded (background) turn between two chat turns resets
        /// `bound_chat` to None, making the next chat turn a switch. It must STILL
        /// re-seed the desk's history — the exact "every background turn blinds the
        /// next chat reply" failure the fix removes.
        #[tokio::test]
        async fn chat_turn_after_a_background_turn_still_seeds_history() {
            let (fx, log, seen) = recording_fixture();
            let rec = record();
            log.operator("general", "HISTORY_USER_MARKER");
            log.reply("general", "HISTORY_AGENT_MARKER");

            let pool = HarnessPool::new();
            pool.ensure(&rec, &fx.deps).await.expect("ensure");

            // First chat turn binds to general.
            pool.run(
                &rec.id,
                "ceo",
                "first",
                &fx.deps,
                crate::runtime::delegation::ChatTarget::channel(Some("general")),
            )
            .await
            .expect("first chat turn");
            // A background/unthreaded turn: resets bound_chat to None.
            pool.run_background(&rec.id, "ceo", "background", &fx.deps, None)
                .await
                .expect("background turn");

            let before = seen.lock().unwrap().len();
            // Second chat turn on general — a switch again, because the background
            // turn invalidated the binding.
            pool.run(
                &rec.id,
                "ceo",
                "second",
                &fx.deps,
                crate::runtime::delegation::ChatTarget::channel(Some("general")),
            )
            .await
            .expect("second chat turn");

            let after: Vec<String> = seen.lock().unwrap()[before..].to_vec();
            let last = after
                .last()
                .expect("the second chat turn made a model call");
            assert!(
                last.contains("HISTORY_USER_MARKER") && last.contains("HISTORY_AGENT_MARKER"),
                "a chat turn after a background turn must still see the desk's \
                 recent history: {last:?}"
            );
        }

        /// C — isolation: history on desk A must never leak into a turn on desk B.
        #[tokio::test]
        async fn a_switch_seeds_only_the_incoming_desks_history() {
            let (fx, log, seen) = recording_fixture();
            let rec = record();
            log.operator("alpha", "ALPHA_USER_MARKER");
            log.reply("alpha", "ALPHA_AGENT_MARKER");
            log.operator("beta", "BETA_USER_MARKER");
            log.reply("beta", "BETA_AGENT_MARKER");

            let pool = HarnessPool::new();
            pool.ensure(&rec, &fx.deps).await.expect("ensure");
            pool.run(
                &rec.id,
                "ceo",
                "hello beta",
                &fx.deps,
                crate::runtime::delegation::ChatTarget::channel(Some("beta")),
            )
            .await
            .expect("beta chat turn");

            let all = seen.lock().unwrap().join("\n===\n");
            assert!(
                all.contains("BETA_USER_MARKER") && all.contains("BETA_AGENT_MARKER"),
                "beta's own history must be seeded: {all:?}"
            );
            assert!(
                !all.contains("ALPHA_USER_MARKER") && !all.contains("ALPHA_AGENT_MARKER"),
                "alpha's history must NEVER leak into a beta turn: {all:?}"
            );
        }

        /// D — DM parity: a `dm:<id>` thread seeds exactly like a named desk.
        #[tokio::test]
        async fn a_dm_thread_seeds_its_own_history() {
            let (fx, log, seen) = recording_fixture();
            let rec = record();
            log.operator("dm:teammate", "DM_USER_MARKER");
            log.reply("dm:teammate", "DM_AGENT_MARKER");

            let pool = HarnessPool::new();
            pool.ensure(&rec, &fx.deps).await.expect("ensure");
            pool.run(
                &rec.id,
                "ceo",
                "hey there",
                &fx.deps,
                crate::runtime::delegation::ChatTarget::channel(Some("dm:teammate")),
            )
            .await
            .expect("dm chat turn");

            let all = seen.lock().unwrap().join("\n===\n");
            assert!(
                all.contains("DM_USER_MARKER") && all.contains("DM_AGENT_MARKER"),
                "a DM thread's own history must be seeded (parity with named desks): {all:?}"
            );
        }

        /// E — a second chat turn on the SAME desk, back to back, is not a
        /// switch: `bound_chat` already points at it, so `run_with_steer`'s
        /// switch check must skip both the re-seed AND the journal read that
        /// builds it. RED on the pre-fix code, which built the (costly on the
        /// filesystem backend — `chat_seed::build_chat_seed`'s docs) seed in
        /// the caller for every chat turn, switch or not, and simply discarded
        /// it on a non-switch turn; GREEN once the projection only runs inside
        /// the confirmed-switch branch (codex review finding).
        #[tokio::test]
        async fn a_non_switch_chat_turn_does_not_re_read_the_journal() {
            let (fx, log, _seen) = recording_fixture();
            let rec = record();
            log.operator("general", "PRIOR_USER_MARKER");
            log.reply("general", "PRIOR_AGENT_MARKER");
            // The current operator message for turn 1, journaled before it runs.
            log.operator("general", "first");

            let pool = HarnessPool::new();
            pool.ensure(&rec, &fx.deps).await.expect("ensure");

            // Turn 1 on "general": a switch (bound_chat starts None) — must
            // read the journal to build the seed.
            pool.run(
                &rec.id,
                "ceo",
                "first",
                &fx.deps,
                crate::runtime::delegation::ChatTarget::channel(Some("general")),
            )
            .await
            .expect("first chat turn");
            let reads_after_first = log.reads();
            assert!(
                reads_after_first > 0,
                "the first (switching) turn must read the journal to build its seed"
            );

            // The current operator message for turn 2, journaled before it runs
            // — same desk as turn 1, so `bound_chat` already matches it.
            log.operator("general", "second");

            // Turn 2 on "general" — NOT a switch. Must not touch the journal
            // again to build a seed nothing downstream will use.
            pool.run(
                &rec.id,
                "ceo",
                "second",
                &fx.deps,
                crate::runtime::delegation::ChatTarget::channel(Some("general")),
            )
            .await
            .expect("second chat turn");
            assert_eq!(
                log.reads(),
                reads_after_first,
                "a same-desk, non-switch chat turn must not re-read the \
                 journal to build a seed the switch check will discard"
            );
        }

        /// Two threads of ONE channel are two conversations (#1890). Moving
        /// between them was not a switch while the binding was keyed on the
        /// chat id alone, so the clear-and-re-seed never ran and the second
        /// thread answered with the first one's turns still in `history`.
        #[tokio::test]
        async fn a_thread_switch_within_one_channel_re_seeds() {
            let (fx, log, _seen) = recording_fixture();
            let rec = record();
            log.operator("general", "root A"); // seq 0
            log.operator("general", "root B"); // seq 1
            log.operator_in("general", "first", 0); // seq 2 — turn 1, thread A

            let pool = HarnessPool::new();
            pool.ensure(&rec, &fx.deps).await.expect("ensure");

            pool.run(
                &rec.id,
                "ceo",
                "first",
                &fx.deps,
                crate::runtime::delegation::ChatTarget::in_thread(
                    Some("general"),
                    Some(EventSeq::new(0)),
                ),
            )
            .await
            .expect("first chat turn");
            let reads_after_first = log.reads();
            assert!(reads_after_first > 0, "the first turn binds and seeds");

            log.operator_in("general", "second", 1); // seq 3 — turn 2, thread B

            pool.run(
                &rec.id,
                "ceo",
                "second",
                &fx.deps,
                crate::runtime::delegation::ChatTarget::in_thread(
                    Some("general"),
                    Some(EventSeq::new(1)),
                ),
            )
            .await
            .expect("second chat turn");
            assert!(
                log.reads() > reads_after_first,
                "a different thread of the same channel is a switch: it must \
                 clear the previous thread's history and re-seed from its own"
            );
        }

        /// An UNADDRESSED threaded message still binds to its thread.
        ///
        /// Issue #1890 I: an **unstreamed** turn still binds when its caller
        /// names a conversation.
        ///
        /// The approval re-dispatch is the case. It runs through
        /// `run_steered_background` — no live stream, because a re-issued call
        /// shows no chat bubble — and before this its identity was read off
        /// that absent stream, so it bound to nothing: it ran against whatever
        /// history the agent happened to be holding and then published its
        /// answer into the origin thread regardless.
        ///
        /// A dispatched card's turn is the other side of the same rule and must
        /// keep binding to nothing, since it answers the board rather than a
        /// conversation.
        #[test]
        fn identity_and_streaming_are_separate_questions() {
            use crate::runtime::delegation::ChatTarget;

            // What the approval re-dispatch now passes: the conversation the
            // grant recorded, with no stream at all.
            let reissued = ChatTarget::in_thread(Some("growth"), Some(EventSeq::new(41)));
            assert_eq!(reissued.chat_id, Some("growth"));
            assert_eq!(reissued.thread_root, Some(EventSeq::new(41)));

            // What a dispatched card's turn passes — unchanged behaviour.
            let card = ChatTarget::default();
            assert_eq!(card.chat_id, None);
            assert_eq!(card.thread_root, None);

            // The two are distinguishable, which is the whole of the fix: before
            // it, both arrived at the binding as "no stream, therefore no
            // conversation".
            assert_ne!(reissued, card);
        }

        /// A codex review on #1896 read `run_with_steer`'s `if let Some(incoming)
        /// = turn_chat_id` guard and concluded that a client sending `parent`
        /// without `chat` loses its root, so sibling threads on the default desk
        /// keep sharing one history. That is not what happens: `turn_chat_id`
        /// comes from the turn-stream route, which already falls back to
        /// `DEFAULT_DESK` when no desk was addressed.
        ///
        /// This test exists because the obvious "fix" — normalizing the id where
        /// the target is built — is actively harmful: the same `chat_id` reaches
        /// card creation and a card's `origin_chat_id`, where `None` means "no
        /// conversation raised this card" and `chat_history::owns` deliberately
        /// routes it to no desk. Pinning the real behaviour here is what stops
        /// that being re-applied.
        #[tokio::test]
        async fn an_unaddressed_message_still_binds_to_its_thread() {
            let fx = fixture();
            let rec = record();
            let pool = HarnessPool::new();
            pool.ensure(&rec, &fx.deps).await.expect("ensure");

            // `chat_id: None` — the operator addressed no desk — with a root.
            pool.run(
                &rec.id,
                "ceo",
                "first",
                &fx.deps,
                crate::runtime::delegation::ChatTarget::in_thread(None, Some(EventSeq::new(1))),
            )
            .await
            .expect("unaddressed threaded turn");

            let agent = {
                let guard = pool.agents.read().await;
                guard
                    .get(&rec.id)
                    .and_then(|roster| roster.iter().find(|a| a.agent_id == "ceo"))
                    .cloned()
                    .expect("the agent stays resident")
            };
            assert_eq!(
                *agent.bound_chat.lock().await,
                Some((
                    crate::server::ops::language::DEFAULT_DESK.to_string(),
                    Some(EventSeq::new(1))
                )),
                "an unaddressed turn binds to the General desk AND keeps its \
                 thread root — the stream route already supplies the fallback"
            );
        }

        /// The binding must not depend on the event log being wired
        /// (coderabbit review finding).
        ///
        /// `incoming_root` used to be read off `chat_seed`, which is `None`
        /// whenever `deps.events` is — so on such a host two different threads
        /// of one channel compared equal, the clear-and-re-seed never ran, and
        /// the leak this change exists to close was reopened in exactly the
        /// configuration that cannot re-seed its way out of it.
        ///
        /// Asserted through `bound_chat` rather than through a seed, because a
        /// host with no journal has no seed to inspect: the binding is the only
        /// observable, and it is the thing that was wrong.
        #[tokio::test]
        async fn the_thread_binding_holds_with_no_event_log_wired() {
            let mut fx = fixture();
            fx.deps.events = None;
            let rec = record();

            let pool = HarnessPool::new();
            pool.ensure(&rec, &fx.deps).await.expect("ensure");

            let thread_a = crate::runtime::delegation::ChatTarget::in_thread(
                Some("general"),
                Some(EventSeq::new(1)),
            );
            let thread_b = crate::runtime::delegation::ChatTarget::in_thread(
                Some("general"),
                Some(EventSeq::new(2)),
            );

            pool.run(&rec.id, "ceo", "first", &fx.deps, thread_a)
                .await
                .expect("thread A turn");
            // The pool keeps one `CompanyAgent` per (company, agent) and
            // reuses it across turns — which is exactly why the binding exists.
            let agent = {
                let guard = pool.agents.read().await;
                guard
                    .get(&rec.id)
                    .and_then(|roster| roster.iter().find(|a| a.agent_id == "ceo"))
                    .cloned()
                    .expect("the agent stays resident between turns")
            };
            assert_eq!(
                *agent.bound_chat.lock().await,
                Some(("general".to_string(), Some(EventSeq::new(1)))),
                "the first turn binds to its own thread, journal or no journal"
            );

            pool.run(&rec.id, "ceo", "second", &fx.deps, thread_b)
                .await
                .expect("thread B turn");
            assert_eq!(
                *agent.bound_chat.lock().await,
                Some(("general".to_string(), Some(EventSeq::new(2)))),
                "a different thread of the same channel must rebind — with no \
                 event log the re-seed is empty, but the history clear is not \
                 optional"
            );
        }

        /// ...and the cost guard survives the finer key: consecutive turns in
        /// the SAME thread are still not a switch, so they still pay for no
        /// journal walk. Keyed only on the channel this held by accident; it
        /// has to hold on purpose now.
        #[tokio::test]
        async fn a_second_turn_in_the_same_thread_does_not_re_read_the_journal() {
            let (fx, log, _seen) = recording_fixture();
            let rec = record();
            log.operator("general", "root"); // seq 0
            log.operator_in("general", "first", 0); // seq 1

            let pool = HarnessPool::new();
            pool.ensure(&rec, &fx.deps).await.expect("ensure");
            let thread = crate::runtime::delegation::ChatTarget::in_thread(
                Some("general"),
                Some(EventSeq::new(0)),
            );

            pool.run(&rec.id, "ceo", "first", &fx.deps, thread)
                .await
                .expect("first chat turn");
            let reads_after_first = log.reads();

            log.operator_in("general", "second", 0); // seq 2, same thread
            pool.run(&rec.id, "ceo", "second", &fx.deps, thread)
                .await
                .expect("second chat turn");
            assert_eq!(
                log.reads(),
                reads_after_first,
                "a second turn in the same thread is not a switch"
            );
        }
    }
}
