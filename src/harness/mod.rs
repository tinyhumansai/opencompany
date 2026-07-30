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

pub mod brain;
pub mod build;
pub mod capability_budget;
pub mod composio;
pub mod cost;
pub mod mcp;
pub mod mcp_probe;
pub mod memory;
pub mod memory_loop;
pub mod orchestrator;
pub mod policy;
pub mod provider;
pub mod run_turn;
pub mod skills;
pub mod steer;
pub mod steps;
pub mod tool_dispatcher;
pub mod toolbelt;

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
use crate::ports::types::{CompanyId, CompanyRecord, OverlayAgent, TurnStep};
use crate::ports::{
    CompanyStore, ContextStore, EventLog, FactStore, SecretStore, TaskStore, UsageMeter,
};
use crate::runtime::builder::agent_effective_grants;

/// Shared dependencies every harness-built agent draws on.
#[derive(Clone)]
pub struct HarnessDeps {
    /// The inference model shared across a company's agents. A [`HarnessModel`]
    /// is a tinyagents [`ChatModel<()>`](tinyagents::harness::model::ChatModel)
    /// plus the telemetry slug the cost hook reads live per turn; it upcasts to
    /// `Arc<dyn ChatModel<()>>` at the openhuman `AgentBuilder::chat_model` seam.
    pub provider: Arc<dyn HarnessModel>,
    /// Stable provider slug attributed to usage samples (e.g. `managed`).
    pub provider_slug: String,
    /// Context store backing every agent's [`OcMemory`](memory::OcMemory).
    pub context: Arc<dyn ContextStore>,
    /// Company store the cost hook appends ledger entries to.
    pub store: Arc<dyn CompanyStore>,
    /// Optional usage meter (WS5 seam); `None` skips usage sampling.
    pub meter: Option<Arc<dyn UsageMeter>>,
    /// Root under which per-agent workspace directories are created
    /// (`{root}/{company}/{agent}/workspace`).
    pub workspace_root: PathBuf,
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
    /// The company's effective MCP servers (issue #50), resolved to **data**
    /// (manifest `[[mcp_server]]` ∪ the runtime index, with each server's
    /// outbound credential materialized to
    /// [`AuthMaterial`](crate::company::mcp::AuthMaterial)) before deps
    /// construction. `build_agent` is synchronous but the
    /// [`SecretStore`](crate::ports::SecretStore) is async, so the runtime
    /// builder resolves these ahead of time; each agent then filters the set by
    /// its `mcp:*` tool grants. Empty leaves the agent with no MCP bridge tools.
    pub mcp_servers: Vec<McpServerDecl>,
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
    /// [`HarnessPool::ensure`] re-resolves it from the [`SecretStore`] under
    /// [`composio::TOKEN_KEY`](crate::harness::composio::TOKEN_KEY) each turn
    /// (folded into the roster fingerprint) so a console token set/rotate takes
    /// effect next turn with no restart. Only wired when a company **explicitly**
    /// grants `composio` **and** a non-empty token is stored; the token has no
    /// env fallback, so an absent token means no tools (never a borrowed
    /// identity).
    pub composio: Option<composio::TenantComposio>,
    /// Issue #111 — the shared registry of in-flight, steerable runs. The
    /// [`HarnessBrain`] registers a dispatched task / desk delegation here before
    /// running it (and installs the steer stop-hook over the slot's control), so
    /// an operator can pause / cancel / redirect it mid-flight. The **same**
    /// handle is threaded onto the [`CompanyRuntime`](crate::company::runtime::CompanyRuntime)
    /// so the operator steer routes reach it. A cheap shared handle (like
    /// [`delegations`](Self::delegations)); the default is an empty registry,
    /// which simply lists nothing and rejects every steer as `not in flight`.
    pub steer: crate::company::steer::InflightRegistry,
}

/// One live openhuman agent, keyed by its manifest id.
pub struct CompanyAgent {
    /// The manifest agent id.
    pub agent_id: String,
    /// The manifest agent's human-readable role.
    pub role: String,
    /// The embedded openhuman session. A [`Mutex`] because a `turn` takes
    /// `&mut self` and one agent must serialise its own turns.
    agent: Mutex<Agent>,
}

/// The graceful reply returned when a turn yields the transient empty-response
/// class twice — so chat never shows a bare "Couldn't send" for a model hiccup.
const GRACEFUL_EMPTY_REPLY: &str = "Sorry — I hit a temporary model hiccup and couldn't produce a reply. Please resend your message.";

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
        self.run_with_steer(message, None, None).await
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
    pub async fn run_with_steer(
        &self,
        message: &str,
        steer: Option<&SteerControl>,
        stream: Option<crate::turn_stream::TurnStreamCtx>,
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
                events.push(event);
            }
            events
        });

        let mut agent = self.agent.lock().await;
        agent.set_on_progress(Some(tx));

        // Install the steer hook only when a control is provided; an empty hook
        // list is exactly the pre-#111 behaviour.
        let hooks: Vec<Arc<dyn oh::agent::stop_hooks::StopHook>> = match steer {
            Some(control) => vec![Arc::new(crate::harness::steer::SteerStopHook::new(
                control.clone(),
            ))],
            None => Vec::new(),
        };

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
                            if steer.map(|c| c.requested()).unwrap_or(false) {
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
        drop(agent);
        let events = collector.await.unwrap_or_default();
        let steps = steps::fold_steps(events);

        let reply = reply?;
        Ok((TurnOutcome { reply, steps }, usages))
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
            skill_fingerprints: RwLock::new(HashMap::new()),
        }
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
    pub async fn ensure(&self, company: &CompanyRecord, deps: &HarnessDeps) -> crate::Result<()> {
        // Re-resolve + fingerprint the effective MCP set (cheap; no rebuild yet).
        let effective_mcp = self.resolve_effective_mcp(company, deps).await;
        let mcp_fp = mcp_fingerprint(&effective_mcp);

        // Re-resolve + fingerprint the live overlay-agent set the same way.
        let overlay_agents = self.resolve_effective_overlay(company, deps).await;
        let overlay_fp = overlay_fingerprint(&overlay_agents);

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

        // Re-fetch + fingerprint the operator skill deltas (issue #41) BEFORE the
        // fast-path check. A skills-only change leaves every other axis stable, so
        // unless skills participate in the staleness check the cached roster is
        // wrongly reused and a console-authored / edited / disabled skill never
        // surfaces until a restart (the regression). `build_roster`/`build_agent`
        // stay synchronous and fold these deltas into each agent's effective
        // skill set; the same Vec is reused for the rebuild below (no re-fetch).
        let skill_deltas = match &deps.skills {
            Some(store) => store.list(&company.id).await?,
            None => Vec::new(),
        };
        let skill_fp = skill_delta_fingerprint(&skill_deltas);

        {
            let agents = self.agents.read().await;
            let mcp_fingerprints = self.mcp_fingerprints.read().await;
            let overlay_fingerprints = self.overlay_fingerprints.read().await;
            let capability_fingerprints = self.capability_fingerprints.read().await;
            let composio_fingerprints = self.composio_fingerprints.read().await;
            let skill_fingerprints = self.skill_fingerprints.read().await;
            if agents.contains_key(&company.id)
                && mcp_fingerprints.get(&company.id) == Some(&mcp_fp)
                && overlay_fingerprints.get(&company.id) == Some(&overlay_fp)
                && capability_fingerprints.get(&company.id) == Some(&capability_fp)
                && composio_fingerprints.get(&company.id) == Some(&composio_fp)
                && skill_fingerprints.get(&company.id) == Some(&skill_fp)
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
        // Same treatment for the overlay-agent set: `company` may be a stale
        // boot-time snapshot (e.g. `HarnessBrain::record`), so the roster is
        // built from the live-resolved overlay set, not `company.overlay_agents`.
        let mut fresh_company = company.clone();
        fresh_company.overlay_agents = overlay_agents;
        let roster = build_roster(&fresh_company, &fresh_deps, &skill_deltas)?;

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
        self.skill_fingerprints
            .write()
            .await
            .insert(company.id.clone(), skill_fp);
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
    /// The token has no env fallback — an absent/empty secret yields `None` (fail
    /// closed). The backend URL resolves from
    /// [`composio::COMPOSIO_BACKEND_URL_ENV`], then the tenant API base
    /// [`composio::TINYHUMANS_API_URL_ENV`], then the prod default — read
    /// process-globally so a live re-resolution keeps the override even when no
    /// token was stored at boot.
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
                )
                .await
            }
            None => deps.composio.clone(),
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

    /// Re-resolves the company's live overlay-agent set (issue #71): reloads the
    /// [`CompanyRecord`] from [`HarnessDeps::store`] so a teammate added through
    /// the console `POST .../team` route or the orchestrator's `add_agent` tool
    /// reaches the roster on the company's next `ensure` call — the same
    /// live-re-resolution pattern as [`Self::resolve_effective_mcp`]. A missing
    /// record or a store error degrades to the `company` snapshot passed in
    /// (never worse than the pre-#71 always-static behaviour).
    async fn resolve_effective_overlay(
        &self,
        company: &CompanyRecord,
        deps: &HarnessDeps,
    ) -> Vec<OverlayAgent> {
        match deps.store.load(&company.id).await {
            Ok(Some(record)) => record.overlay_agents,
            _ => company.overlay_agents.clone(),
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
        self.run_inner(company, agent_id, message, deps, None, LiveStream::Off)
            .await
    }

    /// Routes a message to one agent with an operator **steer** control installed
    /// (issue #111), so a dispatched task / desk delegation can be paused,
    /// cancelled, or redirected mid-flight. Otherwise identical to
    /// [`run`](Self::run) — same retrieve→inject, cost accounting, and
    /// memory-writeback. The steer hook fires only between tool-loop iterations.
    /// `chat_id` routes the live turn-stream frames exactly as in [`run`](Self::run).
    pub async fn run_steered(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        deps: &HarnessDeps,
        control: &SteerControl,
        chat_id: Option<&str>,
    ) -> crate::Result<TurnOutcome> {
        self.run_inner(
            company,
            agent_id,
            message,
            deps,
            Some(control),
            LiveStream::On { chat_id },
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
    ) -> crate::Result<TurnOutcome> {
        self.run_inner(
            company,
            agent_id,
            message,
            deps,
            Some(control),
            LiveStream::Off,
        )
        .await
    }

    async fn run_inner(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        deps: &HarnessDeps,
        steer: Option<&SteerControl>,
        live: LiveStream<'_>,
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
        let (outcome, turn_costs) = agent.run_with_steer(&augmented, steer, stream_ctx).await?;
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
            )
            .await?;
        }

        // Store: persist the outcome (original task + reply) so it compounds
        // into later turns. Without this the harness never writes memory back.
        // SECURITY: the reply **text only** — the scrubbed `outcome.steps` never
        // enter the memory store, so a step detail can never be retrieved and
        // re-injected into a later turn.
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

/// A stable fingerprint of an overlay-agent set (issue #71), used to detect a
/// teammate add/remove/edit between [`HarnessPool::ensure`] calls. Mirrors
/// [`mcp_fingerprint`]'s shape; no secrets are involved here so there is
/// nothing to scrub — an [`OverlayAgent`] is display data.
fn overlay_fingerprint(agents: &[OverlayAgent]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    agents.len().hash(&mut hasher);
    for agent in agents {
        agent.id.hash(&mut hasher);
        agent.name.hash(&mut hasher);
        agent.role.hash(&mut hasher);
        agent.description.hash(&mut hasher);
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
pub(crate) fn build_roster(
    company: &CompanyRecord,
    deps: &HarnessDeps,
    skill_deltas: &[SkillState],
) -> crate::Result<Vec<Arc<CompanyAgent>>> {
    let policy: &Policy = &company.manifest.policy;
    let company_name = &company.manifest.company.name;
    let allow = &company.manifest.tools.allow;
    // The orchestrator agent (tier `orchestrator`, else the first agent) receives
    // the delegating-orchestrator persona + tools (issue #53).
    let orchestrator = orchestrator::orchestrator_id(&company.manifest.agents);

    let mut roster =
        Vec::with_capacity(company.manifest.agents.len() + company.overlay_agents.len());

    for manifest_agent in &company.manifest.agents {
        let agent_policy = ApprovalPolicy::new(policy, manifest_agent.budget_usd_daily)
            .with_requests(deps.approval_requests.clone());
        let is_orchestrator = orchestrator.as_deref() == Some(manifest_agent.id.as_str());
        let grants = agent_effective_grants(allow, &manifest_agent.tools);
        let agent = build::build_agent(
            &company.id,
            company_name,
            manifest_agent,
            agent_policy,
            deps,
            &grants,
            skill_deltas,
            is_orchestrator,
        )?;
        roster.push(Arc::new(CompanyAgent {
            agent_id: manifest_agent.id.clone(),
            role: manifest_agent.role.clone(),
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
        let manifest_agent = overlay_agent_to_manifest(overlay);
        // No per-teammate budget cap or cognition-tier hint in v1 — see
        // `overlay_agent_to_manifest`.
        let agent_policy = ApprovalPolicy::new(policy, manifest_agent.budget_usd_daily)
            .with_requests(deps.approval_requests.clone());
        let grants = agent_effective_grants(allow, &manifest_agent.tools);
        let agent = build::build_agent(
            &company.id,
            company_name,
            &manifest_agent,
            agent_policy,
            deps,
            &grants,
            skill_deltas,
            /* is_orchestrator */ false,
        )?;
        roster.push(Arc::new(CompanyAgent {
            agent_id: manifest_agent.id.clone(),
            role: manifest_agent.role.clone(),
            agent: Mutex::new(agent),
        }));
    }

    Ok(roster)
}

/// Converts an operator-added [`OverlayAgent`] into the manifest agent shape
/// [`build::build_agent`] consumes: an empty `tools` list (so
/// [`agent_effective_grants`] falls back to the full company `[tools].allow`
/// — the "standard tool grant"), no cognition tier (→ the default `chat-v1`
/// model), and no per-agent budget cap. The overlay's `name` is a display
/// label only — already surfaced through
/// [`crate::metering::roster_display_names`] — so the persona is framed from
/// `role`/`description` alone, exactly like a manifest teammate
/// ([`build::persona_prompt`]).
fn overlay_agent_to_manifest(overlay: &OverlayAgent) -> ManifestAgent {
    ManifestAgent {
        id: overlay.id.clone(),
        role: overlay.role.clone(),
        description: overlay.description.clone(),
        tier: None,
        tools: Vec::new(),
        budget_usd_daily: None,
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

    /// In-memory `ContextStore` so `OcMemory` has somewhere to land.
    #[derive(Default)]
    struct MockContext {
        chunks: StdMutex<Vec<(ChunkAddr, ContextChunk)>>,
    }

    #[async_trait]
    impl ContextStore for MockContext {
        async fn put(&self, _id: &CompanyId, chunk: ContextChunk) -> crate::Result<ChunkAddr> {
            let mut guard = self.chunks.lock().unwrap();
            let addr = ChunkAddr::new(format!("addr-{}", guard.len()));
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
        async fn query(
            &self,
            _company: &CompanyId,
            _since: u64,
        ) -> crate::Result<Vec<UsageSample>> {
            Ok(self.samples.lock().unwrap().clone())
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
            id: CompanyId::new("acme"),
            manifest: manifest(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
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
                provider: Arc::new(MockProvider::new("mock: ")),
                provider_slug: "mock".to_string(),
                context: Arc::new(MockContext::default()),
                store: store.clone(),
                meter: Some(meter.clone()),
                workspace_root: dir.path().to_path_buf(),
                model_override: None,
                tasks: None,
                skills: None,
                skills_source_dir: None,
                mcp_servers: Vec::new(),
                facts: None,
                events: None,
                delegations: DelegationQueue::default(),
                workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
                mcp_failures: McpFailureQueue::default(),
                approval_requests: ApprovalRequestQueue::default(),
                secrets: None,
                web_allowed_domains: Vec::new(),
                capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
                workflow_source_dir: None,
                plan: None,
                media: None,
                composio: None,
                steer: crate::company::steer::InflightRegistry::default(),
            },
            store,
            meter,
            _dir: dir,
        }
    }

    #[tokio::test]
    async fn roster_builds_every_manifest_agent() {
        let fx = fixture();
        let roster = build_roster(&record(), &fx.deps, &[]).expect("roster builds");
        let ids: Vec<_> = roster.iter().map(|a| a.agent_id.as_str()).collect();
        assert_eq!(ids, vec!["ceo", "engineer"]);
        assert_eq!(roster[0].role, "Chief Executive");
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
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            context: Arc::new(MockContext::default()),
            store: Arc::new(RecordingStore::default()),
            meter: None,
            workspace_root: dir.path().to_path_buf(),
            model_override: None,
            tasks: None,
            skills: None,
            skills_source_dir: Some(source.path().to_path_buf()),
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: DelegationQueue::default(),
            workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: McpFailureQueue::default(),
            approval_requests: ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            steer: crate::company::steer::InflightRegistry::default(),
        };

        let roster = build_roster(&record(), &deps, &[]).expect("roster builds with skills");
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
        });

        let roster = build_roster(&rec, &fx.deps, &[]).expect("roster builds");
        let ids: Vec<_> = roster.iter().map(|a| a.agent_id.as_str()).collect();
        assert_eq!(ids, vec!["ceo", "engineer", "growth"], "got {ids:?}");
        let overlay_agent = roster
            .iter()
            .find(|a| a.agent_id == "growth")
            .expect("overlay teammate present in roster");
        assert_eq!(overlay_agent.role, "Growth Lead");
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
        });

        let roster = build_roster(&rec, &fx.deps, &[]).expect("roster builds");
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
            provider: Arc::new(ScriptedProvider::new(outcomes)),
            provider_slug: "scripted".to_string(),
            context: Arc::new(MockContext::default()),
            store: Arc::new(RecordingStore::default()),
            meter: None,
            workspace_root: dir.path().to_path_buf(),
            model_override: None,
            tasks: None,
            skills: None,
            skills_source_dir: None,
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: DelegationQueue::default(),
            workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: McpFailureQueue::default(),
            approval_requests: ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            steer: crate::company::steer::InflightRegistry::default(),
        };
        let roster = build_roster(&record(), &deps, &[]).expect("roster");
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
            .run_with_steer("hi", Some(&control), None)
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

    /// A console-added MCP server reaches the agent on the NEXT `ensure` — the
    /// roster is rebuilt because the effective set re-resolved from the LIVE
    /// secret store (not the boot snapshot) changed its fingerprint. This is the
    /// Parallel-Search / BrowserBase freshness bug, proven end-to-end.
    #[tokio::test]
    async fn ensure_rebuilds_when_a_runtime_mcp_server_is_added() {
        let secrets: Arc<dyn SecretStore> = Arc::new(MemSecrets::default());
        let dir = tempfile::tempdir().unwrap();
        let deps = HarnessDeps {
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            context: Arc::new(MockContext::default()),
            store: Arc::new(RecordingStore::default()),
            meter: None,
            workspace_root: dir.path().to_path_buf(),
            model_override: None,
            tasks: None,
            skills: None,
            skills_source_dir: None,
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: DelegationQueue::default(),
            workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: McpFailureQueue::default(),
            approval_requests: ApprovalRequestQueue::default(),
            secrets: Some(secrets.clone()),
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            steer: crate::company::steer::InflightRegistry::default(),
        };
        let pool = HarnessPool::new();
        let rec = record();

        pool.ensure(&rec, &deps).await.expect("first ensure");
        let before = pool
            .mcp_fingerprint_of(&rec.id)
            .await
            .expect("fingerprinted");

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
                timeout_secs: 30,
                enabled: true,
                auth_secret: None,
            }],
        )
        .await
        .unwrap();

        // Next ensure re-resolves from the live store → fingerprint changes →
        // roster rebuilt, so the new server reaches the agent without a restart.
        pool.ensure(&rec, &deps).await.expect("second ensure");
        let after = pool
            .mcp_fingerprint_of(&rec.id)
            .await
            .expect("fingerprinted");
        assert_ne!(before, after, "adding a server must change the fingerprint");
        assert_eq!(
            pool.resident_companies().await,
            1,
            "same company, rebuilt in place"
        );

        // A third ensure with no change is a no-op (fingerprint stable).
        pool.ensure(&rec, &deps).await.expect("third ensure");
        assert_eq!(pool.mcp_fingerprint_of(&rec.id).await, Some(after));
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
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            context: Arc::new(MockContext::default()),
            store: live_store.clone(),
            meter: None,
            workspace_root: dir.path().to_path_buf(),
            model_override: None,
            tasks: None,
            skills: None,
            skills_source_dir: None,
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: DelegationQueue::default(),
            workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: McpFailureQueue::default(),
            approval_requests: ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            steer: crate::company::steer::InflightRegistry::default(),
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
            id: CompanyId::new("acme"),
            manifest: granting_manifest(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
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
        };
        let deps = HarnessDeps {
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            context: Arc::new(MockContext::default()),
            store: Arc::new(RecordingStore::default()),
            meter: Some(meter.clone()),
            workspace_root: dir.path().to_path_buf(),
            model_override: None,
            tasks: None,
            skills: None,
            skills_source_dir: None,
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: DelegationQueue::default(),
            workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: McpFailureQueue::default(),
            approval_requests: ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: Some(plan),
            media: None,
            composio: None,
            steer: crate::company::steer::InflightRegistry::default(),
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
        assert!(
            before.contains(&"memory_store".to_string()),
            "intrinsic memory tool must be present: {before:?}"
        );
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
            after.contains(&"memory_store".to_string()),
            "intrinsic memory tool survives gating: {after:?}"
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
}
