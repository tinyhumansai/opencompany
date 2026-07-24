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
pub mod cost;
pub mod mcp;
pub mod mcp_probe;
pub mod memory;
pub mod memory_loop;
pub mod orchestrator;
pub mod policy;
pub mod provider;
pub mod skills;
pub mod steps;

pub use brain::HarnessBrain;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use openhuman_core::openhuman as oh;
use tokio::sync::{Mutex, RwLock};

use oh::agent::Agent;
use oh::inference::provider::Provider;

use crate::company::Agent as ManifestAgent;
use crate::company::Policy;
use crate::company::mcp::McpServerDecl;
use crate::error::OpenCompanyError;
use crate::harness::cost::{TurnUsage, record_turn_cost};
use crate::harness::mcp_probe::McpFailureQueue;
use crate::harness::orchestrator::DelegationQueue;
use crate::harness::policy::ApprovalPolicy;
use crate::ports::skills_state::{SkillState, SkillStateStore};
use crate::ports::types::{CompanyId, CompanyRecord, OverlayAgent, TurnStep};
use crate::ports::{
    CompanyStore, ContextStore, EventLog, FactStore, SecretStore, TaskStore, UsageMeter,
};
use crate::runtime::builder::agent_effective_grants;

/// Shared dependencies every harness-built agent draws on.
#[derive(Clone)]
pub struct HarnessDeps {
    /// The inference provider shared across a company's agents.
    pub provider: Arc<dyn Provider>,
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
    /// The company's [`SecretStore`], so [`HarnessPool::ensure`] can **re-resolve**
    /// the effective MCP server set on each call and rebuild the roster when a
    /// console add/remove/enable-toggle changes it — the MCP-freshness fix (a
    /// runtime-added server reaches the agent on its next turn, no restart).
    /// `None` (default/tests) keeps the boot-resolved [`Self::mcp_servers`]
    /// static, exactly as before.
    pub secrets: Option<Arc<dyn SecretStore>>,
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
        // Per-turn progress sink + an always-draining collector, so a burst of
        // events never blocks the turn loop on a full channel.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<oh::agent::progress::AgentProgress>(1024);
        let collector = tokio::spawn(async move {
            let mut events = Vec::new();
            while let Some(event) = rx.recv().await {
                events.push(event);
            }
            events
        });

        let mut agent = self.agent.lock().await;
        agent.set_on_progress(Some(tx));
        let mut usages: Vec<TurnUsage> = Vec::new();

        // Attempt 1, falling through to a single retry only on the transient
        // empty-response class. Both attempts feed the one progress channel.
        let first = agent.turn(message).await;
        usages.push(read_turn_usage(&agent));
        let reply: crate::Result<String> = match self.classify_turn(first) {
            AttemptOutcome::Reply(reply) => Ok(reply),
            AttemptOutcome::Hard(err) => Err(err),
            AttemptOutcome::Empty => {
                // Attempt 2 (retry once).
                let second = agent.turn(message).await;
                usages.push(read_turn_usage(&agent));
                match self.classify_turn(second) {
                    AttemptOutcome::Reply(reply) => Ok(reply),
                    // Still empty → graceful, scrubbed text (never an `Err`).
                    AttemptOutcome::Empty => {
                        Ok(crate::harness::mcp_probe::scrub(GRACEFUL_EMPTY_REPLY, &[]))
                    }
                    AttemptOutcome::Hard(err) => Err(err),
                }
            }
        };

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
}

impl Default for HarnessPool {
    fn default() -> Self {
        Self::new()
    }
}

impl HarnessPool {
    /// Builds an empty pool.
    pub fn new() -> Self {
        Self {
            agents: RwLock::new(HashMap::new()),
            mcp_fingerprints: RwLock::new(HashMap::new()),
            overlay_fingerprints: RwLock::new(HashMap::new()),
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
    pub async fn ensure(&self, company: &CompanyRecord, deps: &HarnessDeps) -> crate::Result<()> {
        // Re-resolve + fingerprint the effective MCP set (cheap; no rebuild yet).
        let effective_mcp = self.resolve_effective_mcp(company, deps).await;
        let mcp_fp = mcp_fingerprint(&effective_mcp);

        // Re-resolve + fingerprint the live overlay-agent set the same way.
        let overlay_agents = self.resolve_effective_overlay(company, deps).await;
        let overlay_fp = overlay_fingerprint(&overlay_agents);

        {
            let agents = self.agents.read().await;
            let mcp_fingerprints = self.mcp_fingerprints.read().await;
            let overlay_fingerprints = self.overlay_fingerprints.read().await;
            if agents.contains_key(&company.id)
                && mcp_fingerprints.get(&company.id) == Some(&mcp_fp)
                && overlay_fingerprints.get(&company.id) == Some(&overlay_fp)
            {
                return Ok(());
            }
        }

        // Fetch the operator skill deltas once (async) before building the
        // roster; `build_roster`/`build_agent` stay synchronous and fold the
        // deltas into each agent's effective skill set.
        let skill_deltas = match &deps.skills {
            Some(store) => store.list(&company.id).await?,
            None => Vec::new(),
        };
        // Fold the freshly-resolved MCP set into the deps the roster is built
        // from, so a changed set actually reaches the rebuilt agents. The clone
        // shares every Arc / queue handle — only `mcp_servers` is overridden.
        let mut fresh_deps = deps.clone();
        fresh_deps.mcp_servers = effective_mcp;
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
        Ok(())
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
            Some(secrets) => crate::company::mcp::resolve_effective(
                &company.id,
                &company.manifest.mcp_servers,
                secrets.as_ref(),
            )
            .await
            .unwrap_or_else(|_| deps.mcp_servers.clone()),
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

    /// Routes a message to one agent and returns its reply, recording the turn's
    /// cost. `agent_id` must name a member of the company's roster.
    ///
    /// Desk routing (which agent answers a group chat) is the caller's job — v1
    /// is single-responder and the WS3 chat handler picks the addressed member.
    pub async fn run(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        deps: &HarnessDeps,
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
        let (outcome, turn_costs) = agent.run(&augmented).await?;
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
        deps.context
            .put(
                company,
                memory_loop::outcome_chunk(agent_id, message, &outcome.reply),
            )
            .await?;

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
    }
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
        let agent_policy = ApprovalPolicy::new(policy, manifest_agent.budget_usd_daily);
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
        .map(|manifest_agent| {
            let agent_policy = ApprovalPolicy::new(policy, manifest_agent.budget_usd_daily);
            let is_orchestrator = orchestrator.as_deref() == Some(manifest_agent.id.as_str());
            // This agent's effective tool grants: its own `tools` narrowed by the
            // company `[tools].allow`-list (full allow-list when it lists none).
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
            Ok(Arc::new(CompanyAgent {
                agent_id: manifest_agent.id.clone(),
                role: manifest_agent.role.clone(),
                agent: Mutex::new(agent),
            }))
        })
        .collect::<crate::Result<Vec<_>>>()?;
    roster.extend(manifest_roster);

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
        let agent_policy = ApprovalPolicy::new(policy, manifest_agent.budget_usd_daily);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    use async_trait::async_trait;

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
                secrets: None,
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
            secrets: None,
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
            .run(&rec.id, "ceo", "hello-marker", &fx.deps)
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
            .run(&rec.id, "ceo", "alpha task", &fx.deps)
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
            .run(&rec.id, "ceo", "alpha", &fx.deps)
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

        pool.run(&rec.id, "ceo", "first", &fx.deps)
            .await
            .expect("first turn");
        let second = pool
            .run(&rec.id, "ceo", "second", &fx.deps)
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
            .run(&rec.id, "nobody", "hi", &fx.deps)
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
            .run(&CompanyId::new("ghost"), "ceo", "hi", &fx.deps)
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
        pool.run(&rec.id, "ceo", "hi", &fx.deps)
            .await
            .expect("turn");

        assert!(fx.store.ledger.lock().unwrap().is_empty());
        assert!(fx.meter.samples.lock().unwrap().is_empty());
    }

    // --- Empty-response turn wrapper ----------------------------------------

    /// A provider that plays back a scripted sequence of outcomes, one per
    /// `chat_with_system` call, so the empty-response retry wrapper can be driven
    /// deterministically. `Ok("")` is the transient empty class; `Err(_)` is a
    /// hard error.
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
    impl oh::inference::provider::Provider for ScriptedProvider {
        fn telemetry_provider_id(&self) -> String {
            "scripted".to_string()
        }
        async fn chat_with_system(
            &self,
            _system: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match self.script.lock().unwrap().pop_front() {
                Some(Ok(reply)) => Ok(reply),
                Some(Err(err)) => Err(anyhow::anyhow!("{err}")),
                None => Ok("exhausted".to_string()),
            }
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
            secrets: None,
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
            secrets: Some(secrets.clone()),
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
            mcp_failures: McpFailureQueue::default(),
            secrets: None,
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
            pool.run(&rec.id, "growth", "hi", &deps).await.is_err(),
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
            .run(&rec.id, "growth", "hello-marker", &deps)
            .await
            .expect("the new teammate is addressable on the very next turn")
            .reply;
        assert!(reply.contains("hello-marker"), "got {reply:?}");

        // A third ensure with no further change is a no-op (fingerprint stable).
        pool.ensure(&rec, &deps).await.expect("third ensure");
        assert_eq!(pool.overlay_fingerprint_of(&rec.id).await, Some(after));
    }
}
