//! [`HarnessRouter`]: sending each agent's turn to the harness it is bound to.
//!
//! ## Why this is a router and not a setting
//!
//! Which engine runs a turn used to be one decision per company, taken at boot
//! from "did an inference credential resolve?". That made two things impossible
//! that a company actually wants: a roster spanning a cheap model and an
//! expensive one, and a single coding agent on the operator's own Claude Code
//! while everyone else stays on the embedded loop.
//!
//! [`RunTurn`] already carries `agent_id` on all three of its methods, so the
//! dispatch point was always there — nothing had ever varied on it. This type is
//! that seam: it holds one inner [`RunTurn`] per declared harness and forwards
//! each call to the one its agent names.
//!
//! ## Resolution, and why unbound agents are not an error
//!
//! An agent naming no harness runs on the company's default. That is not
//! leniency — it is what makes named harnesses additive: every roster written
//! before this existed binds nobody, and all of them must keep working. A
//! *named* harness that does not exist is a different matter and is rejected by
//! manifest validation long before a turn is attempted.
//!
//! ## What a missing engine means
//!
//! A harness can be declared, valid, and still have no engine here — an `acp`
//! harness in a build compiled without the `acp` feature, or a `built_in` one on
//! a host that resolved no inference. Those turns fail with a message naming the
//! harness and the reason, rather than silently falling back to another agent's
//! engine. Falling back would be the worst outcome available: the turn would
//! succeed, on a model and a credential nobody chose, and the only evidence
//! would be a billing line.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::Result;
use crate::company::Policy;
use crate::company::steer::SteerControl;
use crate::error::OpenCompanyError;
use crate::harness::built_in::TurnOutcome;
use crate::harness::built_in::run_trace::RunTraceSink;
use crate::ports::types::{CompanyId, CompanyRecord};
use crate::runtime::delegation::{ChatTarget, RunTurn};

/// Routes each agent's turn to the [`RunTurn`] of the harness it is bound to.
pub struct HarnessRouter {
    /// The harness id agents naming none run on.
    default_id: String,
    /// Agent id → harness id, for agents that named one. Agents absent from
    /// this map take [`default_id`](Self::default_id).
    by_agent: HashMap<String, String>,
    /// Harness id → the engine that serves it. A declared harness with no entry
    /// here is one this build or host cannot run; see the module docs.
    engines: HashMap<String, Arc<dyn RunTurn>>,
    /// Why a declared harness has no engine, so the failure can say which
    /// harness and what to do rather than "not found".
    unavailable: HashMap<String, String>,
    /// Harness id → why its engine's last warm-up failed. A lane with a
    /// recorded failure fails its turns with this reason while every other lane
    /// keeps working; a successful re-`ensure` clears the entry, so a recovered
    /// harness comes back without a restart.
    failures: Mutex<HashMap<String, String>>,
}

impl HarnessRouter {
    /// A router over `default_id`, with no bindings and no engines yet.
    pub fn new(default_id: impl Into<String>) -> Self {
        Self {
            default_id: default_id.into(),
            by_agent: HashMap::new(),
            engines: HashMap::new(),
            unavailable: HashMap::new(),
            failures: Mutex::new(HashMap::new()),
        }
    }

    /// Registers the engine serving `harness_id`.
    pub fn with_engine(mut self, harness_id: impl Into<String>, engine: Arc<dyn RunTurn>) -> Self {
        self.engines.insert(harness_id.into(), engine);
        self
    }

    /// Records that `harness_id` was declared but cannot run here, and why.
    ///
    /// `reason` is shown to the operator, so it should name the fix — "this
    /// build has no `acp` feature", not "unsupported".
    pub fn with_unavailable(
        mut self,
        harness_id: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        self.unavailable.insert(harness_id.into(), reason.into());
        self
    }

    /// Binds `agent_id` to `harness_id`.
    pub fn bind(mut self, agent_id: impl Into<String>, harness_id: impl Into<String>) -> Self {
        self.by_agent.insert(agent_id.into(), harness_id.into());
        self
    }

    /// A router over `default_harness`, seeded with the default lane, every
    /// extra lane, every unavailable harness, and every agent→harness binding.
    ///
    /// The single place both the brain and the workflow runner assemble a
    /// router from the same four pieces, so the two dispatch points cannot
    /// drift about which agent lands on which engine.
    ///
    /// `default_lane` is `None` when the default harness itself has no engine
    /// on this host (e.g. an `acp` default with no ACP transport wired) — the
    /// caller must have already recorded its reason in `unavailable`, keyed by
    /// `default_harness` ([`lanes::build`](crate::harness::lanes::build)
    /// guarantees this). `engine_for` then falls through to that entry the same
    /// way it does for any other harness with no engine, instead of this
    /// constructor silently substituting something.
    pub fn from_lanes(
        default_harness: &str,
        default_lane: Option<Arc<dyn RunTurn>>,
        lanes: &[(String, Arc<dyn RunTurn>)],
        unavailable: &[(String, String)],
        bindings: &HashMap<String, String>,
    ) -> Self {
        let mut router = Self::new(default_harness);
        if let Some(default_lane) = default_lane {
            router = router.with_engine(default_harness, default_lane);
        }
        for (id, engine) in lanes {
            router = router.with_engine(id, engine.clone());
        }
        for (id, reason) in unavailable {
            router = router.with_unavailable(id, reason);
        }
        for (agent, harness) in bindings {
            router = router.bind(agent, harness);
        }
        router
    }

    /// The harness id `agent_id` runs on.
    pub fn harness_for(&self, agent_id: &str) -> &str {
        self.by_agent
            .get(agent_id)
            .map(String::as_str)
            .unwrap_or(&self.default_id)
    }

    /// The engine for `agent_id`, or the error explaining why there is none.
    fn engine_for(&self, agent_id: &str) -> Result<&Arc<dyn RunTurn>> {
        let harness = self.harness_for(agent_id);
        if let Some(reason) = self.failures.lock().expect("router failures").get(harness) {
            return Err(OpenCompanyError::Config(format!(
                "agent `{agent_id}` is bound to harness `{harness}`, whose last warm-up failed: {reason}."
            )));
        }
        if let Some(engine) = self.engines.get(harness) {
            return Ok(engine);
        }
        let detail =
            self.unavailable.get(harness).map(String::as_str).unwrap_or(
                "no engine was wired for it — this host cannot run turns on this harness",
            );
        Err(OpenCompanyError::Config(format!(
            "agent `{agent_id}` is bound to harness `{harness}`, but {detail}."
        )))
    }

    /// Records each lane's warm-up outcome: a failure is remembered with its
    /// reason, a success clears any earlier one. Shared by
    /// [`ensure`](RunTurn::ensure) and
    /// [`ensure_with_policy`](RunTurn::ensure_with_policy) so the two warm-up
    /// paths cannot drift apart.
    fn record_warm_up(&self, outcomes: Vec<(String, Result<()>)>) {
        let mut failures = self.failures.lock().expect("router failures");
        for (harness, result) in outcomes {
            match result {
                Ok(()) => {
                    failures.remove(&harness);
                }
                Err(err) => {
                    failures.insert(harness, err.to_string());
                }
            }
        }
    }
}

#[async_trait]
impl RunTurn for HarnessRouter {
    async fn run(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        chat: ChatTarget<'_>,
    ) -> Result<TurnOutcome> {
        self.engine_for(agent_id)?
            .run(company, agent_id, message, chat)
            .await
    }

    async fn run_steered(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        control: &SteerControl,
        chat: ChatTarget<'_>,
        run_sink: Option<Arc<RunTraceSink>>,
    ) -> Result<TurnOutcome> {
        self.engine_for(agent_id)?
            .run_steered(company, agent_id, message, control, chat, run_sink)
            .await
    }

    async fn run_steered_background(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        control: &SteerControl,
        chat: ChatTarget<'_>,
        run_sink: Option<Arc<RunTraceSink>>,
    ) -> Result<TurnOutcome> {
        self.engine_for(agent_id)?
            .run_steered_background(company, agent_id, message, control, chat, run_sink)
            .await
    }

    async fn run_steered_dispatch(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        control: &SteerControl,
        chat: ChatTarget<'_>,
        run_sink: Option<Arc<RunTraceSink>>,
    ) -> Result<TurnOutcome> {
        self.engine_for(agent_id)?
            .run_steered_dispatch(company, agent_id, message, control, chat, run_sink)
            .await
    }

    async fn run_background(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        run_sink: Option<Arc<RunTraceSink>>,
    ) -> Result<TurnOutcome> {
        self.engine_for(agent_id)?
            .run_background(company, agent_id, message, run_sink)
            .await
    }

    async fn run_background_workflow(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        run_sink: Option<Arc<RunTraceSink>>,
        workflow_run_id: &str,
        node_id: &str,
    ) -> Result<TurnOutcome> {
        self.engine_for(agent_id)?
            .run_background_workflow(
                company,
                agent_id,
                message,
                run_sink,
                workflow_run_id,
                node_id,
            )
            .await
    }

    async fn ensure(&self, company: &CompanyRecord) -> Result<()> {
        // Warm every engine's roster before the first turn, recording each
        // lane's failure rather than stopping at the first: one bad lane must
        // not take every other agent down with it. A declared harness with no
        // engine here (an `acp` harness in a build without the feature, say) is
        // not an engine to warm — its turn fails later with the reason, which is
        // the point of `unavailable`. A lane that warms cleanly on a later
        // `ensure` clears its recorded failure, so recovery needs no restart.
        let mut outcomes = Vec::with_capacity(self.engines.len());
        for (harness, engine) in &self.engines {
            outcomes.push((harness.clone(), engine.ensure(company).await));
        }
        self.record_warm_up(outcomes);
        Ok(())
    }

    async fn ensure_with_policy(&self, company: &CompanyRecord, policy: &Policy) -> Result<()> {
        // The same fan-out as `ensure`, but every lane pins its policy axis to
        // the cycle-start snapshot, so no engine's roster can drift from the
        // native gate's mid-turn. Lanes that do not override this fall back to
        // their own `ensure`.
        let mut outcomes = Vec::with_capacity(self.engines.len());
        for (harness, engine) in &self.engines {
            outcomes.push((
                harness.clone(),
                engine.ensure_with_policy(company, policy).await,
            ));
        }
        self.record_warm_up(outcomes);
        Ok(())
    }

    async fn end_cycle(&self, company: &CompanyId) {
        // Fan the release out to every lane that pinned, so no engine's pool is
        // left holding a stale snapshot after its cycle ends (issue #1455).
        for engine in self.engines.values() {
            engine.end_cycle(company).await;
        }
    }

    fn release_policy_pin_sync(&self, company: &CompanyId) {
        // The synchronous fan-out for a cycle's drop guard: a cancelled or
        // panicked cycle cannot await `end_cycle`, but must still release the
        // pin it installed on every lane (issue #1455).
        for engine in self.engines.values() {
            engine.release_policy_pin_sync(company);
        }
    }
}

#[cfg(test)]
#[path = "router_tests.rs"]
mod tests;
