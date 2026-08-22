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

/// Issue #775: the fail-closed shell audit wrapper — one intent line appended
/// (and fsynced) *before* a command runs, refusing the command outright when
/// that append fails. Pairs with the host-owned, per-agent sink
/// [`toolbelt::shell_audit`] resolves. See [`audit`].
pub mod audit;
pub mod brain;
pub mod build;
pub mod capability_budget;
#[cfg(feature = "chargebee")]
pub mod chargebee;
mod checkpoint;
pub mod composio;
/// Issue #410: how a Composio action catalogue is narrowed and rendered for an
/// agent, and why every cut it makes describes itself. Pure and un-gated (the
/// live tools are behind `composio`, which CI never *runs*) — see
/// [`composio_catalog`].
pub mod composio_catalog;
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
/// Hosted embeddings compute for the in-pod memory engine's meaning tier (188c2).
/// Needs the `tinycortex` crate's `EmbeddingBackend` trait, so it links only when
/// both the harness (`openhuman`) and the memory engine (`tinycortex`) are built.
#[cfg(feature = "tinycortex")]
pub mod embeddings;
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
/// Issue #245, agent half: `repo_checkout` / `repo_pr` behind an explicit
/// `repo` grant — a **confined** working tree cloned out of the host's mirror
/// (a full object copy, then every reference back to the mirror severed), plus
/// the per-turn ledger that deletes it again. See [`repo`].
pub mod repo;
pub mod run_trace;
pub mod run_turn;
pub mod search;
/// End-to-end proof that the #238 `web_search` tool is reachable from a real
/// turn — the harness, the grant gates, the approval policy, the cap and the
/// meter are all real; only the model's choices and the search backend's
/// responses are scripted. Test-only.
#[cfg(test)]
mod search_turn_test;
pub mod skills;
pub mod steer;
pub mod steps;
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

pub use brain::HarnessBrain;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

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
    AgentOverride, BudgetOverride, CompanyId, CompanyRecord, OverlayAgent, OverlayDesk,
    OverlayDeskMember, PolicyOverride, TurnStep,
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
    /// is a tinyagents [`ChatModel<()>`](tinyagents::harness::model::ChatModel)
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
    /// Issue #245, agent half — the company's [`RepoManager`], so an agent that
    /// explicitly grants `repo` can check a bound repository out and read a
    /// pull request. `None` (the default at every construction site but the
    /// production runtime builder) **fails closed**: no repository tools are
    /// wired and agents behave exactly as before.
    ///
    /// [`RepoManager`]: crate::runtime::RepoManager
    pub repos: Option<Arc<crate::runtime::RepoManager>>,
    /// The company's bound repositories, resolved to **data** before deps
    /// construction — the `mcp_servers` doctrine, and for the same reason:
    /// [`build::build_agent`] is synchronous while reading the binding index is
    /// async, and the tool descriptions name what is bound so a model does not
    /// have to guess. Empty means nothing is bound, which is also what makes a
    /// `repo` grant with no bindings wire nothing and warn.
    pub repo_bindings: Vec<crate::runtime::repo_manager::types::RepoBinding>,
    /// The shared per-turn ledger of checkouts and diff spills, so the
    /// [`CheckoutJanitor`](brain::CheckoutJanitor) claimed at each entry point
    /// can delete them however the turn ends.
    ///
    /// Same cheap-shared-handle pattern as [`Self::pending_publishes`], and for
    /// the same structural reason: the tools are built **once per agent** while
    /// the deletion boundary is **per turn**. Default is an empty ledger, which
    /// simply means nothing is ever recorded for deletion — the boot sweep is
    /// the backstop.
    pub checkouts: repo::CheckoutLedger,
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

/// The classification of a single `agent.turn` attempt, for the retry wrapper.
enum AttemptOutcome {
    /// A non-empty reply.
    Reply(String),
    /// The transient empty-response class (an empty/blank reply, or the model's
    /// "empty response" error) — retryable.
    Empty,
    /// A hard error (budget/auth/build/etc.) — propagated loudly, never swallowed.
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
    pub async fn run(&self, message: &str) -> crate::Result<(TurnOutcome, Vec<TurnUsage>)> {
        self.run_with_steer(message, None, None, None).await
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
    ) -> crate::Result<(TurnOutcome, Vec<TurnUsage>)> {
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
        let (tx, mut rx) = tokio::sync::mpsc::channel::<oh::agent::progress::AgentProgress>(1024);
        let collector = tokio::spawn(async move {
            let mut events = Vec::new();
            let mut seq: u64 = 0;
            // Mirrors `fold_steps`' thinking-run coalescing so the live timeline
            // emits the same "Thinking" rows the final folded one does.
            let mut thinking_open = false;
            while let Some(event) = rx.recv().await {
                if let Some(ctx) = &stream
                    && let Some(frame) = steps::stream_event_from(&event, seq, &mut thinking_open)
                {
                    crate::turn_stream::publish(
                        &ctx.company,
                        frame
                            .with_agent(ctx.agent_id.clone())
                            .with_chat(ctx.chat_id.clone()),
                    );
                    seq += 1;
                }
                // Durable half (#242): persist the step before moving on, so a
                // process killed mid-run keeps every step written so far.
                if let Some(sink) = &run_sink {
                    sink.record(&event).await;
                }
                events.push(event);
            }
            events
        });

        let mut agent = self.agent.lock().await;
        agent.set_on_progress(Some(tx));

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

        // `Box::pin` at the task-local scope boundary (the nested-scope
        // stack-overflow trap). The turn body owns the retry classification and
        // reports every attempt's usage.
        let (reply, usages): (crate::Result<String>, Vec<TurnUsage>) =
            oh::agent::stop_hooks::with_stop_hooks(
                hooks,
                Box::pin(async {
                    let mut usages: Vec<TurnUsage> = Vec::new();
                    let first = agent.turn(message).await;
                    usages.push(read_turn_usage(&agent));
                    let reply: crate::Result<String> = match self.classify_turn(first) {
                        AttemptOutcome::Reply(reply) => Ok(reply),
                        AttemptOutcome::Hard(err) => Err(err),
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
                                let second = agent.turn(message).await;
                                usages.push(read_turn_usage(&agent));
                                match self.classify_turn(second) {
                                    AttemptOutcome::Reply(reply) => Ok(reply),
                                    AttemptOutcome::Empty => Ok(crate::harness::mcp_probe::scrub(
                                        GRACEFUL_EMPTY_REPLY,
                                        &[],
                                    )),
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
        let steps = steps::fold_steps(events);

        let reply = reply?;
        Ok((
            TurnOutcome {
                reply,
                steps,
                hit_iteration_cap,
                halted_for_spend,
            },
            usages,
        ))
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
    fn classify_turn(&self, result: anyhow::Result<String>) -> AttemptOutcome {
        match result {
            Ok(reply) if reply.trim().is_empty() => AttemptOutcome::Empty,
            Ok(reply) => AttemptOutcome::Reply(reply),
            Err(err) if is_transient_empty_response(&err) => AttemptOutcome::Empty,
            Err(err) => AttemptOutcome::Hard(OpenCompanyError::Harness(format!(
                "turn for '{}': {err}",
                self.agent_id
            ))),
        }
    }
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

/// Whether a turn error is the transient empty-response class openhuman raises
/// instead of a silent blank reply. Matched on the error chain's message
/// (`turn` returns `anyhow::Result`, so the typed `AgentError` is erased):
/// "The model returned an empty response…".
fn is_transient_empty_response(err: &anyhow::Error) -> bool {
    format!("{err:#}")
        .to_ascii_lowercase()
        .contains("empty response")
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
    /// Fingerprint of the company's bound-repository set the cached roster was
    /// built from, keyed by company (issue #245). Drives repository freshness:
    /// [`ensure`](Self::ensure) re-reads the binding index from the
    /// [`SecretStore`] on every call and rebuilds the roster whenever it moves —
    /// so a bind, a credential rotation and a revoke each reach the agent on the
    /// company's **next** turn with no restart.
    ///
    /// All three have to move it, which is why the fingerprint is over
    /// `(key, token_fingerprint, branches)` rather than over the key alone: a
    /// rotation changes nothing about *which* repositories exist, and a roster
    /// that kept a tool description naming a binding whose credential has since
    /// been revoked would offer an agent a checkout that can no longer fetch.
    /// With no secret store wired the set is the static
    /// [`HarnessDeps::repo_bindings`], whose fingerprint never moves.
    repo_fingerprints: RwLock<HashMap<CompanyId, u64>>,
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
    /// Per-company fingerprint of the operator `[policy]` override (issue #562),
    /// so a console tier change rebuilds the roster instead of waiting for a
    /// restart. Without this axis the override persists and is silently ignored:
    /// `ApprovalPolicy` is built once per roster, not once per call.
    policy_fingerprints: RwLock<HashMap<CompanyId, u64>>,
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
    On { chat_id: Option<&'a str> },
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
            repo_fingerprints: RwLock::new(HashMap::new()),
            skill_fingerprints: RwLock::new(HashMap::new()),
            budget_fingerprints: RwLock::new(HashMap::new()),
            override_fingerprints: RwLock::new(HashMap::new()),
            policy_fingerprints: RwLock::new(HashMap::new()),
            desk_fingerprints: RwLock::new(HashMap::new()),
            context_fingerprints: RwLock::new(HashMap::new()),
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
        let policy_fp = policy_fingerprint(overlay.policy.as_ref());
        // Desk scoping now decides capability (the middle level of the
        // three-level narrowing), so it joins the staleness check: without this
        // a console desk-ceiling edit — or seating a teammate on a restricted
        // desk — would not reach the roster until a restart.
        let desk_fp =
            desk_scope_fingerprint(&overlay.desks, &overlay.desk_members, &overlay.desk_tools);

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
            hasher.finish()
        };

        // Re-read + fingerprint the company's bound repositories (issue #245):
        // one index document, read live, so a bind / rotate / revoke reaches the
        // agent on the next turn. Only companies that explicitly grant `repo`
        // touch the store on this axis; everything else resolves to the static
        // `deps.repo_bindings` (empty at every construction site but the
        // production builder), whose fingerprint never moves.
        let repo_bindings = self.resolve_repo_bindings(company, deps).await;
        let repo_fp = repo_binding_fingerprint(&repo_bindings);

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
            let repo_fingerprints = self.repo_fingerprints.read().await;
            let skill_fingerprints = self.skill_fingerprints.read().await;
            let budget_fingerprints = self.budget_fingerprints.read().await;
            let override_fingerprints = self.override_fingerprints.read().await;
            let policy_fingerprints = self.policy_fingerprints.read().await;
            let desk_fingerprints = self.desk_fingerprints.read().await;
            let context_fingerprints = self.context_fingerprints.read().await;
            if agents.contains_key(&company.id)
                && mcp_fingerprints.get(&company.id) == Some(&mcp_fp)
                && overlay_fingerprints.get(&company.id) == Some(&overlay_fp)
                && capability_fingerprints.get(&company.id) == Some(&capability_fp)
                && composio_fingerprints.get(&company.id) == Some(&composio_fp)
                && billing_fingerprints.get(&company.id) == Some(&billing_fp)
                && repo_fingerprints.get(&company.id) == Some(&repo_fp)
                && skill_fingerprints.get(&company.id) == Some(&skill_fp)
                && budget_fingerprints.get(&company.id) == Some(&budget_fp)
                && override_fingerprints.get(&company.id) == Some(&override_fp)
                && policy_fingerprints.get(&company.id) == Some(&policy_fp)
                && desk_fingerprints.get(&company.id) == Some(&desk_fp)
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
        // And the freshly-read bindings (issue #245), so a repository bound or
        // revoked in the console is what the rebuilt agents' tools resolve
        // against — including the descriptions that name what is bound.
        fresh_deps.repo_bindings = repo_bindings;
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
        // Issue #562: same treatment for the policy override — `build_roster`
        // resolves the tier through `fresh_company.effective_policy`, so installing
        // the live value here is what carries a console tier change into the roster
        // the next turn runs on.
        fresh_company.overlay_policy = overlay.policy;

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
        self.repo_fingerprints
            .write()
            .await
            .insert(company.id.clone(), repo_fp);
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
        self.policy_fingerprints
            .write()
            .await
            .insert(company.id.clone(), policy_fp);
        self.desk_fingerprints
            .write()
            .await
            .insert(company.id.clone(), desk_fp);
        self.context_fingerprints
            .write()
            .await
            .insert(company.id.clone(), context_fp);
        Ok(())
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
    /// [`Self::resolve_repo_bindings`] and [`Self::resolve_effective_mcp`]
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

    /// Re-reads the company's bound repositories (issue #245) from the
    /// [`RepoManager`](crate::runtime::RepoManager), so a bind, a credential
    /// rotation or a revoke reaches the roster on the next turn.
    ///
    /// Only companies that **explicitly** grant `repo` read at all; everything
    /// else answers empty without touching the store, mirroring
    /// [`Self::resolve_composio`]. A transient read error degrades to the
    /// boot-resolved [`HarnessDeps::repo_bindings`] with a warning rather than
    /// dropping an agent's repository tools mid-session — the same direction
    /// [`Self::resolve_effective_mcp`] degrades in, and the safe one: a stale
    /// binding list still resolves against real bindings, while an empty one
    /// un-wires the tools entirely.
    async fn resolve_repo_bindings(
        &self,
        company: &CompanyRecord,
        deps: &HarnessDeps,
    ) -> Vec<crate::runtime::repo_manager::types::RepoBinding> {
        if !crate::company::grants_repo_explicit(&company.manifest.tools.allow) {
            return Vec::new();
        }
        let Some(repos) = deps.repos.as_ref() else {
            return deps.repo_bindings.clone();
        };
        match repos.list().await {
            Ok(bindings) => bindings,
            Err(err) => {
                tracing::warn!(
                    company = %company.id,
                    "[repo] could not read the repository bindings; keeping the last known set: {err}"
                );
                deps.repo_bindings.clone()
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
                agents: record.overlay_agents,
                agent_edits: record.overlay_agent_edits,
                retired: record.overlay_retired_agents,
                budgets: record.overlay_budgets,
                policy: record.overlay_policy,
                desks: record.overlay_desks,
                desk_members: record.overlay_desk_members,
                desk_tools: record.overlay_desk_tools,
            },
            _ => EffectiveOverlay {
                agents: company.overlay_agents.clone(),
                agent_edits: company.overlay_agent_edits.clone(),
                retired: company.overlay_retired_agents.clone(),
                budgets: company.overlay_budgets.clone(),
                policy: company.overlay_policy.clone(),
                desks: company.overlay_desks.clone(),
                desk_members: company.overlay_desk_members.clone(),
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

    /// The current bound-repository fingerprint for a company (test-only), so a
    /// bind / rotate / revoke freshness test can assert the roster was actually
    /// rebuilt rather than inferring it (issue #245).
    #[cfg(test)]
    pub async fn repo_fingerprint_of(&self, company: &CompanyId) -> Option<u64> {
        self.repo_fingerprints.read().await.get(company).copied()
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
    /// `chat_id` is the chat/desk **thread** this turn answers (the id journaled
    /// as `AgentReply.chat_id`). It rides each live turn-stream frame so the
    /// console routes the in-flight tool timeline to the right thread; `None`
    /// falls back to the default desk, matching the durable reply.
    pub async fn run(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        deps: &HarnessDeps,
        chat_id: Option<&str>,
    ) -> crate::Result<TurnOutcome> {
        self.run_inner(
            company,
            agent_id,
            message,
            deps,
            None,
            LiveStream::On { chat_id },
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
    ) -> crate::Result<TurnOutcome> {
        self.run_inner(
            company,
            agent_id,
            message,
            deps,
            None,
            LiveStream::Off,
            None,
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
        chat_id: Option<&str>,
        run_sink: Option<Arc<run_trace::RunTraceSink>>,
    ) -> crate::Result<TurnOutcome> {
        self.run_inner(
            company,
            agent_id,
            message,
            deps,
            Some(control),
            LiveStream::On { chat_id },
            run_sink,
        )
        .await
    }

    /// Like [`run_steered`](Self::run_steered) but WITHOUT live turn streaming —
    /// for a dispatched task card, which discards its steps and shows no chat
    /// bubble. Its transient turn frames must not reach the live console
    /// timeline (they'd misattribute to a chat thread), so this path publishes
    /// nothing while still honouring the operator steer control (#125 review).
    pub async fn run_steered_background(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        deps: &HarnessDeps,
        control: &SteerControl,
        run_sink: Option<Arc<run_trace::RunTraceSink>>,
    ) -> crate::Result<TurnOutcome> {
        self.run_inner(
            company,
            agent_id,
            message,
            deps,
            Some(control),
            LiveStream::Off,
            run_sink,
        )
        .await
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
                                // And no in-turn hook fired, because no turn
                                // ran (issue #1032). The reply already IS the
                                // budget notice; labelling this as a halt too
                                // would tell the operator the same thing twice.
                                halted_for_spend: None,
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

        let agent = CompanyAgent {
            agent_id: confine::CONFINED_AGENT_ID.to_string(),
            role: "Workflow copilot".to_string(),
            // A confined turn carries no manifest teammate, so there is no
            // per-agent daily cap to read; the company-wide ceiling above is the
            // one that applies to it.
            budget_usd_daily: None,
            agent: Mutex::new(confine::build_confined_agent(
                company,
                company_name,
                confinement,
                deps,
            )?),
        };

        let stream_ctx = Some(crate::turn_stream::TurnStreamCtx {
            company: company.clone(),
            agent_id: confine::CONFINED_AGENT_ID.to_string(),
            chat_id: chat_id
                .map(str::to_string)
                .unwrap_or_else(|| crate::server::ops::language::DEFAULT_DESK.to_string()),
        });

        // The message goes to the model AS SENT. This is the retrieve→inject
        // step's absence, and it is the difference between "grounded in one
        // workflow" and "confined to one workflow".
        let (outcome, turn_costs) = agent
            .run_with_steer(message, None, stream_ctx, None)
            .await?;

        let provider_slug = deps.provider.telemetry_provider_id();
        for turn_cost in &turn_costs {
            record_turn_cost(
                turn_cost,
                confine::CONFINED_AGENT_ID,
                &provider_slug,
                company,
                deps.store.as_ref(),
                deps.meter.as_deref(),
                None,
            )
            .await?;
        }

        Ok(outcome)
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
                                    // Same teammate cap, refused BEFORE the
                                    // turn (issue #1032). The in-turn brake
                                    // never armed, and the reply above already
                                    // names the cap it refused against.
                                    halted_for_spend: None,
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
        let hits = deps
            .context
            .search(company, message, memory_loop::RETRIEVE_TOP_K)
            .await?;
        let augmented = memory_loop::inject(message, &hits);

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
        let stream_ctx = match live {
            LiveStream::On { chat_id } => Some(crate::turn_stream::TurnStreamCtx {
                company: company.clone(),
                agent_id: agent_id.to_string(),
                // The chat/desk thread this turn answers — the same id journaled
                // as `AgentReply.chat_id`, so the console keys the live timeline
                // on it and concurrent turns on different threads never
                // cross-attribute. Falls back to the default desk to match the
                // durable reply when the caller addressed no desk (e.g. an API
                // client that omits `chat`).
                chat_id: chat_id
                    .map(str::to_string)
                    .unwrap_or_else(|| crate::server::ops::language::DEFAULT_DESK.to_string()),
            }),
            LiveStream::Off => None,
        };
        let (outcome, turn_costs) = agent
            .run_with_steer(&augmented, steer, stream_ctx, run_sink.clone())
            .await?;
        // Issue #242: fold this turn's spend into the attempt it belongs to.
        // Per turn, not once at the end, so a redirect re-run and a delegate's
        // turn both count — an attempt's cost is what the attempt spent. This is
        // a second *reader* of `turn_costs`, not a second writer: the ledger and
        // the usage meter below stay the only places money is recorded.
        if let Some(sink) = run_sink.as_ref() {
            for turn_cost in &turn_costs {
                sink.add_usage(turn_cost);
            }
        }
        // Attribute cost to the provider this turn actually resolved to. With a
        // per-tenant [`TenantProvider`](crate::harness::provider::TenantProvider)
        // a console BYOK switch changes the slug between turns, so read it live
        // rather than trusting the static `deps.provider_slug` baked at build.
        let provider_slug = deps.provider.telemetry_provider_id();
        for turn_cost in &turn_costs {
            record_turn_cost(
                turn_cost,
                agent_id,
                &provider_slug,
                company,
                deps.store.as_ref(),
                deps.meter.as_deref(),
                // Issue #242: attribute the sample to the attempt this turn ran
                // under, so "what did this run cost?" is answerable from the
                // meter as well as from the run row.
                run_sink.as_ref().map(|s| s.run_id()),
            )
            .await?;
        }

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
        if !matches!(
            steer.and_then(SteerControl::pending),
            Some(SteerAction::Cancel)
        ) {
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
    // live session for a change that touched nobody else.
    edits.len().hash(&mut hasher);
    for edit in edits {
        edit.agent_id.hash(&mut hasher);
        edit.name.hash(&mut hasher);
        edit.role.hash(&mut hasher);
        edit.description.hash(&mut hasher);
        edit.tools.hash(&mut hasher);
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
    }
    hasher.finish()
}

/// A stable hash of the operator's `[policy]` override, so a console tier change
/// rebuilds the roster on the company's next `ensure` (issue #562).
///
/// # Why this axis has to exist at all
///
/// `ApprovalPolicy` is constructed in [`build_roster`], **once per roster
/// build** — not once per call. The roster is cached and rebuilt only when one
/// of the fingerprints in the staleness check moves. So without this function a
/// console tier change would be written, persisted, and then **silently ignored
/// until the process restarted**: the write route would return `204`, the
/// console would show the new tier, and every agent would keep running the old
/// one. That is the same failure the skill-delta fingerprint above exists to
/// prevent, and it is invisible from the outside.
///
/// # What is hashed, and what deliberately is not
///
/// - `mode` and `always_approve` are hashed — they are what the gate reads.
/// - `always_approve` is hashed **in order**, unlike the budget set. The order
///   is the operator's own list as they wrote it, not an accumulation of
///   independent rows, so a reorder is a real edit rather than a spurious
///   difference. Its length is folded in first so `["a","b"]` cannot collide
///   with `["ab"]`.
/// - The `Some`/`None` distinction is hashed for both fields, because "not
///   overridden" and "overridden to the manifest's current value" must stay
///   apart: the manifest can change under a rebuild, and collapsing them would
///   pin the override to a value the operator never chose.
/// - **Attribution (`set_by`, `at_millis`) is deliberately NOT hashed**, for the
///   same reason the budget fingerprint omits it: who set the tier and when
///   changes nothing an agent can act on, and folding it in would rebuild the
///   roster — dropping live agent sessions — on a save that re-set the same tier.
fn policy_fingerprint(override_: Option<&PolicyOverride>) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    match override_ {
        None => 0u8.hash(&mut hasher),
        Some(entry) => {
            1u8.hash(&mut hasher);
            match &entry.mode {
                Some(mode) => {
                    1u8.hash(&mut hasher);
                    mode.hash(&mut hasher);
                }
                None => 0u8.hash(&mut hasher),
            }
            match &entry.always_approve {
                Some(kinds) => {
                    1u8.hash(&mut hasher);
                    kinds.len().hash(&mut hasher);
                    for kind in kinds {
                        kind.hash(&mut hasher);
                    }
                }
                None => 0u8.hash(&mut hasher),
            }
        }
    }
    hasher.finish()
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
    pub desk_tools: std::collections::BTreeMap<String, Vec<String>>,
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
fn override_fingerprint(overrides: &[AgentOverride]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut ordered: Vec<&AgentOverride> = overrides.iter().collect();
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
    }
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
/// * **key** — a bind adds one, a revoke removes one, and either changes what
///   `repo_checkout` can resolve and what its description names;
/// * **token fingerprint** — a rotation leaves the key alone, and a *revoked*
///   credential blanks it while the key survives, so keying on the set of
///   repositories would leave an agent holding a tool over a binding that can no
///   longer fetch;
/// * **branches** — the set a checkout may name, and the only other field the
///   tools read.
///
/// Deliberately not `size_bytes` or `last_fetched_millis`: both move on every
/// fetch, and a fetch is something the agent's own tool does — folding them in
/// would rebuild the roster after every checkout, for no change an agent can
/// observe.
fn repo_binding_fingerprint(bindings: &[crate::runtime::repo_manager::types::RepoBinding]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut ordered: Vec<&crate::runtime::repo_manager::types::RepoBinding> =
        bindings.iter().collect();
    ordered.sort_by(|a, b| a.key.cmp(&b.key));

    let mut hasher = DefaultHasher::new();
    ordered.len().hash(&mut hasher);
    for binding in ordered {
        binding.key.hash(&mut hasher);
        binding.token_fingerprint.hash(&mut hasher);
        binding.branches.hash(&mut hasher);
    }
    hasher.finish()
}

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
            .with_requests(deps.approval_requests.clone())
            // Issue #243: stamp who the parked effect belongs to, so approving it
            // can hand the grant back to this agent rather than to nobody.
            .with_agent(manifest_agent.id.clone())
            // Issue #1124: the per-server read-only MCP declaration, so a
            // server-declared read-only bridge call does not park under `auto`.
            .with_mcp_reads(mcp_reads.clone());
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
        let grants = agent_scoped_grants(allow, &desk_allows, &manifest_agent.tools);
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
            agent: Mutex::new(agent),
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
            .with_requests(deps.approval_requests.clone())
            // An overlay teammate is a real roster agent and re-dispatches the
            // same way a manifest one does (issue #243).
            .with_agent(manifest_agent.id.clone())
            // Issue #1124: the same per-server read-only MCP declaration the
            // manifest agents get — an overlay teammate calls the same servers.
            .with_mcp_reads(mcp_reads.clone());
        if let Some(meter) = deps.meter.as_ref() {
            agent_policy = agent_policy.with_spend(meter.clone(), company.id.clone());
        }
        // An overlay teammate is scoped by its desks the same as a manifest one:
        // it can be seated on a desk, and a desk ceiling that applied to only
        // half its members would not be a ceiling.
        let desk_tools = company.agent_desk_tools(&manifest_agent.id);
        let desk_allows: Vec<&[String]> = desk_tools.iter().map(Vec::as_slice).collect();
        let grants = agent_scoped_grants(allow, &desk_allows, &manifest_agent.tools);
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
            agent: Mutex::new(agent),
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
        // An operator- or orchestrator-added teammate runs on the company's
        // default harness. There is no console field to name one, and inventing
        // a binding here would put a teammate on a harness nobody chose.
        harness: None,
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
        ledgers: None,
        ledger_registry: Default::default(),
        provider: Arc::new(provider::MockProvider::default()),
        provider_slug: "mock".to_string(),
        serves: None,
        context: runtime.context.clone(),
        store: runtime.store.clone(),
        meter,
        workspace_root: std::env::temp_dir(),
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
        steer: crate::company::steer::InflightRegistry::default(),
        run_supervisor: crate::runtime::RunSupervisor::default(),
        delivery: None,
        workspace: None,
        repos: None,
        repo_bindings: Vec::new(),
        checkouts: repo::CheckoutLedger::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    use async_trait::async_trait;
    use tinyagents::harness::model::{ChatModel, ModelRequest, ModelResponse};

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

    fn fp_entry(mode: Option<&str>, always: Option<Vec<&str>>) -> PolicyOverride {
        use crate::ports::types::{Actor, ActorKind};
        PolicyOverride {
            mode: mode.map(str::to_string),
            always_approve: always.map(|v| v.into_iter().map(str::to_string).collect()),
            set_by: Actor {
                kind: ActorKind::User,
                id: "user-1".to_string(),
            },
            at_millis: 1_700_000_000_000,
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
        let none = policy_fingerprint(None);
        let supervised = policy_fingerprint(Some(&fp_entry(Some("supervised"), None)));
        let full = policy_fingerprint(Some(&fp_entry(Some("full"), None)));

        assert_ne!(
            supervised, full,
            "a tier change must move the fingerprint or the roster is never rebuilt"
        );
        assert_ne!(
            none, supervised,
            "setting an override must move the fingerprint even when it names the \
             tier the manifest already had — the manifest can change under a rebuild"
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
        let absent = policy_fingerprint(Some(&fp_entry(Some("auto"), None)));
        let empty = policy_fingerprint(Some(&fp_entry(Some("auto"), Some(vec![]))));
        let one = policy_fingerprint(Some(&fp_entry(Some("auto"), Some(vec!["payment.send"]))));
        let two = policy_fingerprint(Some(&fp_entry(
            Some("auto"),
            Some(vec!["payment.send", "filing.submit"]),
        )));

        assert_ne!(
            absent, empty,
            "clearing the list is not the same as not overriding it"
        );
        assert_ne!(empty, one);
        assert_ne!(one, two);

        // Order is part of the value: the list is the operator's own, not an
        // accumulation of independent rows, so a reorder is a real edit.
        let reordered = policy_fingerprint(Some(&fp_entry(
            Some("auto"),
            Some(vec!["filing.submit", "payment.send"]),
        )));
        assert_ne!(two, reordered);

        // Length is folded in, so concatenation cannot collide.
        let split = policy_fingerprint(Some(&fp_entry(Some("auto"), Some(vec!["a", "b"]))));
        let joined = policy_fingerprint(Some(&fp_entry(Some("auto"), Some(vec!["ab"]))));
        assert_ne!(split, joined);
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
            tools: vec!["docs.*".into(), "payment.send".into()],
        };
        let manifest = overlay_agent_to_manifest(&scoped);
        assert_eq!(
            manifest.tools,
            vec!["docs.*".to_string(), "payment.send".to_string()],
            "the overlay's own grant must reach the manifest shape"
        );
        assert_eq!(
            agent_effective_grants(&allow, &manifest.tools),
            vec!["docs.*".to_string()],
            "narrow-only: the un-allowed `payment.send` is intersected out"
        );

        // An empty overlay grant is the standard company-wide grant, unchanged.
        let standard = OverlayAgent {
            id: "std".into(),
            name: "Std".into(),
            role: "Generalist".into(),
            description: None,
            tools: Vec::new(),
        };
        let manifest = overlay_agent_to_manifest(&standard);
        assert!(manifest.tools.is_empty());
        assert_eq!(
            agent_effective_grants(&allow, &manifest.tools),
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
            tools: Vec::new(),
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
        let one = |tools: Vec<String>| {
            vec![OverlayAgent {
                id: "a".into(),
                name: "A".into(),
                role: "r".into(),
                description: None,
                tools,
            }]
        };
        let standard = one(Vec::new());
        let scoped = one(vec!["docs.*".into()]);
        let scoped_more = one(vec!["docs.*".into(), "email".into()]);

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
            overlay_fingerprint(&one(vec!["docs.*".into()]), &[], &[])
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

    /// Attribution is deliberately NOT hashed.
    ///
    /// Re-setting the same tier writes a fresh `set_by`/`at_millis`. If those
    /// moved the fingerprint, every such save would rebuild the roster and drop
    /// live agent sessions for a change no agent can observe — the same reason
    /// `budget_fingerprint` omits them.
    #[test]
    fn re_setting_the_same_tier_does_not_rebuild_the_roster() {
        use crate::ports::types::{Actor, ActorKind};
        let first = fp_entry(Some("auto"), Some(vec!["payment.send"]));
        let second = PolicyOverride {
            set_by: Actor {
                kind: ActorKind::User,
                id: "a-different-admin".to_string(),
            },
            at_millis: 1_900_000_000_000,
            ..first.clone()
        };
        assert_eq!(
            policy_fingerprint(Some(&first)),
            policy_fingerprint(Some(&second)),
            "attribution must not move the fingerprint"
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
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
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
                ledgers: None,
                ledger_registry: Default::default(),
                provider: Arc::new(MockProvider::new("mock: ")),
                provider_slug: "mock".to_string(),
                serves: None,
                context: Arc::new(MockContext::default()),
                store: store.clone(),
                meter: Some(meter.clone()),
                workspace_root: dir.path().to_path_buf(),
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
                workspace: None,
                repos: None,
                repo_bindings: Vec::new(),
                checkouts: crate::harness::repo::CheckoutLedger::default(),
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
            ledgers: None,
            ledger_registry: Default::default(),
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            serves: None,
            context: Arc::new(MockContext::default()),
            store: Arc::new(RecordingStore::default()),
            meter: None,
            workspace_root: dir.path().to_path_buf(),
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
            workspace: None,
            repos: None,
            repo_bindings: Vec::new(),
            checkouts: crate::harness::repo::CheckoutLedger::default(),
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
            tools: Vec::new(),
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
            tools: Vec::new(),
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
            tools: Vec::new(),
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
            .run(&rec.id, "ceo", "hello-marker", &fx.deps, None)
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
            .run(&rec.id, "ceo", "alpha task", &fx.deps, None)
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
            .run(&rec.id, "ceo", "alpha", &fx.deps, None)
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

    #[tokio::test]
    async fn turns_are_serialised_and_history_survives() {
        let fx = fixture();
        let pool = HarnessPool::new();
        let rec = record();
        pool.ensure(&rec, &fx.deps).await.expect("ensure");

        pool.run(&rec.id, "ceo", "first", &fx.deps, None)
            .await
            .expect("first turn");
        let second = pool
            .run(&rec.id, "ceo", "second", &fx.deps, None)
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
            .run(&rec.id, "ceo", question, &fx.deps, None)
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
            .run(&rec.id, confine::CONFINED_AGENT_ID, "hi", &fx.deps, None)
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
            .run(&rec.id, "nobody", "hi", &fx.deps, None)
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
            .run(&CompanyId::new("ghost"), "ceo", "hi", &fx.deps, None)
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
            pool.run(&rec.id, "ceo", "hi", &fx.deps, None)
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
        pool.run(&rec.id, "ceo", "hi", &fx.deps, None)
            .await
            .expect("turn");

        assert!(fx.store.ledger.lock().unwrap().is_empty());
        assert!(fx.meter.samples.lock().unwrap().is_empty());
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
    }

    impl ScriptedProvider {
        fn new(outcomes: Vec<Result<String, String>>) -> Self {
            Self {
                script: StdMutex::new(outcomes.into_iter().collect()),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl ChatModel<()> for ScriptedProvider {
        async fn invoke(
            &self,
            _state: &(),
            _request: ModelRequest,
        ) -> tinyagents::Result<ModelResponse> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match self.script.lock().unwrap().pop_front() {
                Some(Ok(reply)) => Ok(ModelResponse::assistant(reply)),
                Some(Err(err)) => Err(tinyagents::TinyAgentsError::Model(err)),
                None => Ok(ModelResponse::assistant("exhausted")),
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
        let dir = tempfile::tempdir().expect("tempdir");
        let deps = HarnessDeps {
            ledgers: None,
            ledger_registry: Default::default(),
            provider: Arc::new(ScriptedProvider::new(outcomes)),
            provider_slug: "scripted".to_string(),
            serves: None,
            context: Arc::new(MockContext::default()),
            store: Arc::new(RecordingStore::default()),
            meter: None,
            workspace_root: dir.path().to_path_buf(),
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
            workspace: None,
            repos: None,
            repo_bindings: Vec::new(),
            checkouts: crate::harness::repo::CheckoutLedger::default(),
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
        let (outcome, usages) = agent.run("hi").await.expect("wrapper recovers");
        assert!(
            outcome.reply.contains("recovered"),
            "got {:?}",
            outcome.reply
        );
        assert_eq!(usages.len(), 2, "both attempts' usage is returned");
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
            .run_with_steer("hi", Some(&control), None, None)
            .await
            .expect("runs");
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
        let (outcome, usages) = agent.run("hi").await.expect("graceful, not an Err");
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
            ledgers: None,
            ledger_registry: Default::default(),
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            serves: None,
            context: Arc::new(MockContext::default()),
            store: Arc::new(RecordingStore::default()),
            meter: None,
            workspace_root: dir.path().to_path_buf(),
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
            workspace: None,
            repos: None,
            repo_bindings: Vec::new(),
            checkouts: crate::harness::repo::CheckoutLedger::default(),
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

    /// A bind, a credential **rotation** and a revoke each rebuild the roster on
    /// the company's next turn, with no restart.
    ///
    /// All three are asserted because they fail differently, and the middle one
    /// is the reason the fingerprint is over `(key, token_fingerprint,
    /// branches)` rather than over the set of keys. A rotation changes nothing
    /// about *which* repositories exist; a revoke blanks a credential while the
    /// key survives for the moment before the entry is dropped. A roster keyed
    /// on the key set alone holds through both, and an agent is left holding a
    /// tool over a binding that can no longer fetch.
    ///
    /// The index is written straight into the live secret store rather than
    /// through `bind`, because what is under test is the *staleness gate*, and
    /// binding for real would drag a `git` fixture and a network-shaped code
    /// path into a test about a hash.
    #[tokio::test]
    async fn ensure_rebuilds_when_a_repository_is_bound_rotated_or_revoked() {
        use crate::runtime::repo_manager::types::RepoBinding;

        let secrets: Arc<dyn SecretStore> = Arc::new(MemSecrets::default());
        let dir = tempfile::tempdir().unwrap();
        let mut deps = deps_with_plan(dir.path(), Arc::new(MockContext::default()), None, None);
        deps.secrets = Some(secrets.clone());
        deps.repos = Some(Arc::new(crate::runtime::RepoManager::new(
            CompanyId::new("acme"),
            dir.path().join("repos"),
            secrets.clone(),
        )));

        // The grant is what opens this axis at all: a company that does not
        // explicitly grant `repo` never reads the index, so its fingerprint can
        // never move. That is the fast path every other company stays on.
        let mut rec = record();
        rec.manifest.tools.allow = vec!["repo".to_string()];

        let pool = HarnessPool::new();
        let write_index = |bindings: Vec<RepoBinding>| {
            let secrets = secrets.clone();
            async move {
                let json = serde_json::to_string(&serde_json::json!({ "bindings": bindings }))
                    .expect("index json");
                secrets
                    .set(
                        &CompanyId::new("acme"),
                        crate::runtime::repo_manager::REPO_INDEX_KEY,
                        crate::ports::types::SecretValue(json),
                    )
                    .await
                    .expect("write index");
            }
        };
        let binding = |fingerprint: &str| RepoBinding {
            key: "acme-widgets-000000000000".to_string(),
            url: "https://github.com/acme/widgets".to_string(),
            owner: "acme".to_string(),
            repo: "widgets".to_string(),
            branches: vec!["main".to_string()],
            token_fingerprint: fingerprint.to_string(),
            last_fetched_millis: None,
            size_bytes: 0,
            bound_at_millis: 1,
            can_push: None,
        };

        pool.ensure(&rec, &deps).await.expect("first ensure");
        let empty = pool
            .repo_fingerprint_of(&rec.id)
            .await
            .expect("fingerprint");

        // Stability first, so every change assertion below cannot pass by
        // coincidence.
        pool.ensure(&rec, &deps).await.expect("redundant ensure");
        assert_eq!(
            pool.repo_fingerprint_of(&rec.id).await,
            Some(empty),
            "an unchanged binding set must not move the fingerprint"
        );

        // Bind.
        write_index(vec![binding("0f1e2d3c4b5a")]).await;
        pool.ensure(&rec, &deps).await.expect("post-bind ensure");
        let bound = pool
            .repo_fingerprint_of(&rec.id)
            .await
            .expect("fingerprint");
        assert_ne!(empty, bound, "a bind must move the staleness fingerprint");

        // Rotate: same repository, same branches, new credential.
        write_index(vec![binding("aaaaaaaaaaaa")]).await;
        pool.ensure(&rec, &deps).await.expect("post-rotate ensure");
        let rotated = pool
            .repo_fingerprint_of(&rec.id)
            .await
            .expect("fingerprint");
        assert_ne!(
            bound, rotated,
            "a credential rotation must move the fingerprint even though the \
             repository set is identical"
        );

        // Revoke.
        write_index(Vec::new()).await;
        pool.ensure(&rec, &deps).await.expect("post-revoke ensure");
        let revoked = pool
            .repo_fingerprint_of(&rec.id)
            .await
            .expect("fingerprint");
        assert_ne!(rotated, revoked, "a revoke must move the fingerprint");
        assert_eq!(revoked, empty, "and must land back on the empty set");
        assert_eq!(
            pool.resident_companies().await,
            1,
            "same company, rebuilt in place — not a new residency"
        );
    }

    /// A company that does not explicitly grant `repo` never reads the binding
    /// index, so this axis is inert for it — the fast path every company that
    /// does not use the feature stays on.
    #[tokio::test]
    async fn a_company_without_the_repo_grant_never_moves_on_this_axis() {
        use crate::runtime::repo_manager::types::RepoBinding;

        let secrets: Arc<dyn SecretStore> = Arc::new(MemSecrets::default());
        let dir = tempfile::tempdir().unwrap();
        let mut deps = deps_with_plan(dir.path(), Arc::new(MockContext::default()), None, None);
        deps.secrets = Some(secrets.clone());
        deps.repos = Some(Arc::new(crate::runtime::RepoManager::new(
            CompanyId::new("acme"),
            dir.path().join("repos"),
            secrets.clone(),
        )));

        // A wildcard, deliberately: `*` does not confer `repo`, so even a
        // broadly-permissioned company stays off this axis.
        let mut rec = record();
        rec.manifest.tools.allow = vec!["*".to_string()];

        let pool = HarnessPool::new();
        pool.ensure(&rec, &deps).await.expect("first ensure");
        let before = pool
            .repo_fingerprint_of(&rec.id)
            .await
            .expect("fingerprint");

        let json = serde_json::to_string(&serde_json::json!({
            "bindings": [RepoBinding {
                key: "acme-widgets-000000000000".to_string(),
                url: "https://github.com/acme/widgets".to_string(),
                owner: "acme".to_string(),
                repo: "widgets".to_string(),
                branches: vec!["main".to_string()],
                token_fingerprint: "0f1e2d3c4b5a".to_string(),
                last_fetched_millis: None,
                size_bytes: 0,
                bound_at_millis: 1,
                can_push: None,
            }]
        }))
        .unwrap();
        secrets
            .set(
                &CompanyId::new("acme"),
                crate::runtime::repo_manager::REPO_INDEX_KEY,
                crate::ports::types::SecretValue(json),
            )
            .await
            .unwrap();

        pool.ensure(&rec, &deps).await.expect("post-bind ensure");
        assert_eq!(
            pool.repo_fingerprint_of(&rec.id).await,
            Some(before),
            "an ungranted company must not read the index, let alone rebuild on it"
        );
    }

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
            ledgers: None,
            ledger_registry: Default::default(),
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            serves: None,
            context: Arc::new(MockContext::default()),
            store: live_store.clone(),
            meter: None,
            workspace_root: dir.path().to_path_buf(),
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
            workspace: None,
            repos: None,
            repo_bindings: Vec::new(),
            checkouts: crate::harness::repo::CheckoutLedger::default(),
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
            pool.run(&rec.id, "growth", "hi", &deps, None)
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
            tools: Vec::new(),
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
            .run(&rec.id, "growth", "hello-marker", &deps, None)
            .await
            .expect("the new teammate is addressable on the very next turn")
            .reply;
        assert!(reply.contains("hello-marker"), "got {reply:?}");

        // A third ensure with no further change is a no-op (fingerprint stable).
        pool.ensure(&rec, &deps).await.expect("third ensure");
        assert_eq!(pool.overlay_fingerprint_of(&rec.id).await, Some(after));
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
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
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
            ledgers: None,
            ledger_registry: Default::default(),
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            serves: None,
            context: Arc::new(MockContext::default()),
            store: Arc::new(RecordingStore::default()),
            meter: Some(meter.clone()),
            workspace_root: dir.path().to_path_buf(),
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
            workspace: None,
            repos: None,
            repo_bindings: Vec::new(),
            checkouts: crate::harness::repo::CheckoutLedger::default(),
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
            ledgers: None,
            ledger_registry: Default::default(),
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            serves: None,
            context,
            store: Arc::new(RecordingStore::default()),
            meter,
            workspace_root: dir.to_path_buf(),
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
            workspace: None,
            repos: None,
            repo_bindings: Vec::new(),
            checkouts: crate::harness::repo::CheckoutLedger::default(),
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
            .run(&rec.id, "ceo", "hello-marker", &deps, None)
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
            .run(&rec.id, "ceo", "should-not-echo", &deps, None)
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
            .run(&rec.id, "ceo", "hello-marker", &deps, None)
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
            .run(&rec.id, "ceo", "should-not-echo", &deps, None)
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
            .run(&rec.id, "engineer", "hello-marker", &deps, None)
            .await
            .expect("an uncapped teammate keeps working")
            .reply;
        assert!(
            ok.contains("hello-marker"),
            "one teammate's exhausted budget must not stop the company: {ok:?}"
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
            .run(&rec.id, "ceo", "should-not-echo", &deps, None)
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
            .run(&rec.id, "ceo", "hello-marker", &deps, None)
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
            .run(&rec.id, "ceo", "should-not-echo", &deps, None)
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
            .run(&rec.id, "ceo", "hello-marker", &deps, None)
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
            tools: Vec::new(),
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
            .run(&rec.id, "growth", "hello-marker", &deps, None)
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
            .run(&rec.id, "growth", "should-not-echo", &deps, None)
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
            .run(&rec.id, "ceo", "hello-marker", &deps, None)
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
            .run(&rec.id, "ceo", "hello-marker", &no_meter, None)
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
            .run(&rec.id, "ceo", "hello-marker", &deps, None)
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
            // Issue #245: a repository manager AND a binding, because the tools
            // are gated on both — with a manager and nothing bound the belt
            // would be missing `repo_checkout` / `repo_pr` and this check would
            // pass while never having looked at them, which is the exact way
            // `describe_skill` stayed invisible here while parking in
            // production.
            // Issue #752 added a fourth gate: a backend that keeps the
            // credential off this container's disk. Declared here for the same
            // reason the binding below is — without it the belt would be
            // missing `repo_checkout` / `repo_pr` and this check would pass
            // while never having looked at them.
            deps.repos = Some(Arc::new(
                crate::runtime::RepoManager::new(
                    CompanyId::new("acme"),
                    dir.path().join("repos"),
                    Arc::new(crate::store::FsSecretStore::new(dir.path())),
                )
                .with_storage_kind(crate::store::StorageKind::Mongodb),
            ));
            deps.repo_bindings = vec![crate::runtime::repo_manager::types::RepoBinding {
                key: "acme-widgets-000000000000".to_string(),
                url: "https://github.com/acme/widgets".to_string(),
                owner: "acme".to_string(),
                repo: "widgets".to_string(),
                branches: vec!["main".to_string()],
                token_fingerprint: "0f1e2d3c4b5a".to_string(),
                last_fetched_millis: None,
                size_bytes: 0,
                bound_at_millis: 1,
                can_push: None,
            }];
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
            tools: Vec::new(),
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
            (
                &["workspace", "search", "media", "composio", "repo"][..],
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
            "repo_checkout",
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
            tools: Vec::new(),
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
}
