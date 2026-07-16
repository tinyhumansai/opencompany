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
//! * **Live turn cost.** openhuman exposes the completed turn's token/cost
//!   totals only through a `pub(crate)` accessor
//!   (`Agent::take_last_turn_usage_totals`), so a host crate cannot read the
//!   real [`TurnCost`] after `turn()`. Until openhuman adds a public accessor
//!   (or the harness ships a usage-accumulating provider), [`HarnessPool::run`]
//!   records a zero-usage turn — which, per the cost contract, writes nothing.
//!   The [`cost`] mapping itself is complete and tested.
//! * **Group-chat / desk routing** is opencompany's job (openhuman is
//!   single-agent). v1 is single-responder; the full ops `chat` handler that
//!   resolves a desk's members and journals the `AgentReply` is WS3.

pub mod build;
pub mod cost;
pub mod memory;
pub mod policy;
pub mod provider;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use openhuman_core::openhuman as oh;
use tokio::sync::{Mutex, RwLock};

use oh::agent::Agent;
use oh::inference::provider::Provider;

use crate::company::Policy;
use crate::error::OpenCompanyError;
use crate::harness::cost::{TurnUsage, record_turn_cost};
use crate::harness::policy::ApprovalPolicy;
use crate::ports::types::{CompanyId, CompanyRecord};
use crate::ports::{CompanyStore, ContextStore, UsageMeter};

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

impl CompanyAgent {
    /// Runs one turn against this agent, returning its reply text.
    pub async fn run(&self, message: &str) -> crate::Result<String> {
        let mut agent = self.agent.lock().await;
        agent
            .turn(message)
            .await
            .map_err(|e| OpenCompanyError::Harness(format!("turn for '{}': {e}", self.agent_id)))
    }
}

/// A pool of live agents, one roster per company.
pub struct HarnessPool {
    agents: RwLock<HashMap<CompanyId, Vec<Arc<CompanyAgent>>>>,
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
        }
    }

    /// Ensures a company's roster is built and cached. Idempotent: a second call
    /// for a company already in the pool is a no-op (the roster is rebuilt only
    /// when absent).
    pub async fn ensure(&self, company: &CompanyRecord, deps: &HarnessDeps) -> crate::Result<()> {
        {
            let guard = self.agents.read().await;
            if guard.contains_key(&company.id) {
                return Ok(());
            }
        }
        let roster = build_roster(company, deps)?;
        let mut guard = self.agents.write().await;
        guard.entry(company.id.clone()).or_insert(roster);
        Ok(())
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
    ) -> crate::Result<String> {
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

        let reply = agent.run(message).await?;

        // Cost accounting. openhuman only exposes the completed turn's usage via
        // a `pub(crate)` accessor, so we cannot read the real `TurnCost` here yet
        // (see module docs — flagged seam). A zero-usage turn writes nothing, so
        // this is correct-but-inert until a public accessor lands; the mapping is
        // fully wired so it becomes real with a one-line swap.
        let turn_cost = TurnUsage::default();
        record_turn_cost(
            &turn_cost,
            agent_id,
            &deps.provider_slug,
            company,
            deps.store.as_ref(),
            deps.meter.as_deref(),
        )
        .await?;

        Ok(reply)
    }

    /// Number of companies currently resident in the pool (test/observability).
    pub async fn resident_companies(&self) -> usize {
        self.agents.read().await.len()
    }
}

/// Build every roster agent for a company from its manifest.
fn build_roster(
    company: &CompanyRecord,
    deps: &HarnessDeps,
) -> crate::Result<Vec<Arc<CompanyAgent>>> {
    let policy: &Policy = &company.manifest.policy;
    company
        .manifest
        .agents
        .iter()
        .map(|manifest_agent| {
            let agent_policy = ApprovalPolicy::new(policy, manifest_agent.budget_usd_daily);
            let agent = build::build_agent(&company.id, manifest_agent, agent_policy, deps)?;
            Ok(Arc::new(CompanyAgent {
                agent_id: manifest_agent.id.clone(),
                role: manifest_agent.role.clone(),
                agent: Mutex::new(agent),
            }))
        })
        .collect()
}
