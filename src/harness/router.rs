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
        run_sink: Option<Arc<RunTraceSink>>,
    ) -> Result<TurnOutcome> {
        self.engine_for(agent_id)?
            .run_steered_background(company, agent_id, message, control, run_sink)
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
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// An engine that records which agent it was asked to run, so a test can
    /// assert on *which* harness served a turn rather than only that one did.
    /// Also records `end_cycle` releases, so a test can assert the fan-out.
    struct SpyEngine {
        label: String,
        seen: Mutex<Vec<String>>,
        cycle_ends: Mutex<Vec<CompanyId>>,
    }

    impl SpyEngine {
        fn new(label: &str) -> Arc<Self> {
            Arc::new(Self {
                label: label.to_string(),
                seen: Mutex::new(Vec::new()),
                cycle_ends: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl RunTurn for SpyEngine {
        async fn run(
            &self,
            _company: &CompanyId,
            agent_id: &str,
            _message: &str,
            _chat_id: ChatTarget<'_>,
        ) -> Result<TurnOutcome> {
            self.seen.lock().unwrap().push(agent_id.to_string());
            Ok(TurnOutcome {
                reply: self.label.clone(),
                steps: Vec::new(),
                hit_iteration_cap: false,
                // Test fixture, not the ACP fold (PR #1880 review).
                abnormal_stop: None,
                halted_for_spend: None,
                budget_paused: None,
            })
        }

        async fn run_steered(
            &self,
            company: &CompanyId,
            agent_id: &str,
            message: &str,
            _control: &SteerControl,
            chat_id: ChatTarget<'_>,
            _run_sink: Option<Arc<RunTraceSink>>,
        ) -> Result<TurnOutcome> {
            self.run(company, agent_id, message, chat_id).await
        }

        async fn run_steered_background(
            &self,
            company: &CompanyId,
            agent_id: &str,
            message: &str,
            _control: &SteerControl,
            _run_sink: Option<Arc<RunTraceSink>>,
        ) -> Result<TurnOutcome> {
            self.run(company, agent_id, message, ChatTarget::default())
                .await
        }

        async fn end_cycle(&self, company: &CompanyId) {
            self.cycle_ends.lock().unwrap().push(company.clone());
        }

        fn release_policy_pin_sync(&self, company: &CompanyId) {
            self.cycle_ends.lock().unwrap().push(company.clone());
        }
    }

    /// An engine whose `ensure` can be made to fail on command, so a test can
    /// check that one lane's warm-up failure does not take every other lane
    /// down, and that a later successful `ensure` brings the lane back.
    struct FlakyEngine {
        label: String,
        fail_ensure: Mutex<bool>,
    }

    impl FlakyEngine {
        fn new(label: &str) -> Arc<Self> {
            Arc::new(Self {
                label: label.to_string(),
                fail_ensure: Mutex::new(false),
            })
        }

        fn set_fail(&self, fail: bool) {
            *self.fail_ensure.lock().unwrap() = fail;
        }
    }

    #[async_trait]
    impl RunTurn for FlakyEngine {
        async fn run(
            &self,
            _company: &CompanyId,
            _agent_id: &str,
            _message: &str,
            _chat_id: ChatTarget<'_>,
        ) -> Result<TurnOutcome> {
            Ok(TurnOutcome {
                reply: self.label.clone(),
                steps: Vec::new(),
                hit_iteration_cap: false,
                // Test fixture, not the ACP fold (PR #1880 review).
                abnormal_stop: None,
                halted_for_spend: None,
                budget_paused: None,
            })
        }

        async fn run_steered(
            &self,
            company: &CompanyId,
            agent_id: &str,
            message: &str,
            _control: &SteerControl,
            chat_id: ChatTarget<'_>,
            _run_sink: Option<Arc<RunTraceSink>>,
        ) -> Result<TurnOutcome> {
            self.run(company, agent_id, message, chat_id).await
        }

        async fn run_steered_background(
            &self,
            company: &CompanyId,
            agent_id: &str,
            message: &str,
            _control: &SteerControl,
            _run_sink: Option<Arc<RunTraceSink>>,
        ) -> Result<TurnOutcome> {
            self.run(company, agent_id, message, ChatTarget::default())
                .await
        }

        async fn ensure(&self, _company: &CompanyRecord) -> Result<()> {
            if *self.fail_ensure.lock().unwrap() {
                return Err(OpenCompanyError::Config(
                    "roster warm-up failed".to_string(),
                ));
            }
            Ok(())
        }
    }

    /// A minimal record for `ensure` to warm against; the engines in these
    /// tests ignore it, so only the manifest-less shape is needed.
    fn record() -> CompanyRecord {
        let manifest: crate::company::CompanyManifest =
            toml::from_str("[company]\nname = \"Acme\"\n").expect("manifest parses");
        CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: company(),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
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
            setup: None,
            name_confirmed: false,
            activation_completed_at: None,
            created_at_millis: None,
        }
    }

    fn company() -> CompanyId {
        CompanyId::new("acme")
    }

    /// The headline: two agents in one company, two engines, and each turn goes
    /// to the one its agent named.
    #[tokio::test]
    async fn each_agent_runs_on_the_harness_it_names() {
        let embedded = SpyEngine::new("embedded");
        let deep = SpyEngine::new("deep");
        let router = HarnessRouter::new("embedded")
            .with_engine("embedded", embedded.clone())
            .with_engine("deep", deep.clone())
            .bind("researcher", "deep");

        let out = router
            .run(&company(), "researcher", "hi", ChatTarget::default())
            .await
            .unwrap();
        assert_eq!(out.reply, "deep");

        let out = router
            .run(&company(), "ceo", "hi", ChatTarget::default())
            .await
            .unwrap();
        assert_eq!(out.reply, "embedded", "an unbound agent takes the default");

        assert_eq!(&*deep.seen.lock().unwrap(), &["researcher".to_string()]);
        assert_eq!(&*embedded.seen.lock().unwrap(), &["ceo".to_string()]);
    }

    /// Every `RunTurn` method routes, not just the streamed one. A method
    /// that forwarded to a fixed engine would send *dispatched card* turns to
    /// the wrong model while operator chat looked correct.
    #[tokio::test]
    async fn every_run_turn_method_routes() {
        let embedded = SpyEngine::new("embedded");
        let deep = SpyEngine::new("deep");
        let router = HarnessRouter::new("embedded")
            .with_engine("embedded", embedded.clone())
            .with_engine("deep", deep.clone())
            .bind("researcher", "deep");
        let control = SteerControl::default();

        assert_eq!(
            router
                .run_steered(
                    &company(),
                    "researcher",
                    "hi",
                    &control,
                    ChatTarget::default(),
                    None
                )
                .await
                .unwrap()
                .reply,
            "deep"
        );
        assert_eq!(
            router
                .run_steered_background(&company(), "researcher", "hi", &control, None)
                .await
                .unwrap()
                .reply,
            "deep"
        );
        assert_eq!(
            router
                .run_background(&company(), "researcher", "hi", None)
                .await
                .unwrap()
                .reply,
            "deep"
        );
        assert!(
            embedded.seen.lock().unwrap().is_empty(),
            "no method leaked to the default engine"
        );
    }

    /// A harness with no engine fails the turn, naming the harness and the
    /// reason. It must never quietly borrow another harness's engine: that turn
    /// would succeed on a model and a credential nobody chose.
    #[tokio::test]
    async fn a_harness_with_no_engine_fails_rather_than_falling_back() {
        let embedded = SpyEngine::new("embedded");
        let router = HarnessRouter::new("embedded")
            .with_engine("embedded", embedded.clone())
            .with_unavailable(
                "my_laptop",
                "this build was compiled without the `acp` feature",
            )
            .bind("coder", "my_laptop");

        let err = router
            .run(&company(), "coder", "hi", ChatTarget::default())
            .await
            .expect_err("must not fall back");
        let msg = err.to_string();
        assert!(msg.contains("coder"), "{msg}");
        assert!(msg.contains("my_laptop"), "{msg}");
        assert!(msg.contains("`acp` feature"), "names the fix: {msg}");
        assert!(
            embedded.seen.lock().unwrap().is_empty(),
            "the default engine was never reached"
        );
    }

    /// A binding to a harness nobody declared still fails closed, even though
    /// manifest validation should have caught it first. Defence in depth: the
    /// router is also reachable from runtime-constructed rosters that no
    /// manifest validated.
    #[tokio::test]
    async fn an_unknown_harness_binding_fails_closed() {
        let router = HarnessRouter::new("embedded").with_engine("embedded", SpyEngine::new("e"));
        let err = router
            .run(&company(), "ghost_bound", "hi", ChatTarget::default())
            .await
            .expect("agent is unbound, so it takes the default")
            .reply;
        assert_eq!(err, "e");

        let router = router.bind("ghost_bound", "nowhere");
        assert!(
            router
                .run(&company(), "ghost_bound", "hi", ChatTarget::default())
                .await
                .is_err()
        );
    }

    /// One lane failing to warm does not take down the others: `ensure` warms
    /// every engine, records the failed lane, and only that lane's turns error.
    #[tokio::test]
    async fn one_lane_failing_to_warm_does_not_take_down_the_others() {
        let embedded = FlakyEngine::new("embedded");
        let deep = FlakyEngine::new("deep");
        let router = HarnessRouter::new("embedded")
            .with_engine("embedded", embedded.clone())
            .with_engine("deep", deep.clone())
            .bind("researcher", "deep")
            .bind("ceo", "embedded");

        deep.set_fail(true);
        router.ensure(&record()).await.unwrap();

        let err = router
            .run(&company(), "researcher", "hi", ChatTarget::default())
            .await
            .expect_err("the failed lane's turn must error");
        let msg = err.to_string();
        assert!(msg.contains("researcher"), "{msg}");
        assert!(msg.contains("deep"), "{msg}");
        assert!(msg.contains("warm-up"), "names the failed warm-up: {msg}");

        let out = router
            .run(&company(), "ceo", "hi", ChatTarget::default())
            .await
            .unwrap();
        assert_eq!(out.reply, "embedded", "the healthy lane keeps working");
    }

    /// A lane that failed to warm comes back once a later `ensure` succeeds —
    /// the recorded failure is cleared, so recovery needs no restart.
    #[tokio::test]
    async fn a_failed_lane_recovers_on_a_later_ensure() {
        let embedded = FlakyEngine::new("embedded");
        let deep = FlakyEngine::new("deep");
        let router = HarnessRouter::new("embedded")
            .with_engine("embedded", embedded.clone())
            .with_engine("deep", deep.clone())
            .bind("researcher", "deep");

        deep.set_fail(true);
        router.ensure(&record()).await.unwrap();
        assert!(
            router
                .run(&company(), "researcher", "hi", ChatTarget::default())
                .await
                .is_err(),
            "the failed lane errors before recovery"
        );

        deep.set_fail(false);
        router.ensure(&record()).await.unwrap();
        let out = router
            .run(&company(), "researcher", "hi", ChatTarget::default())
            .await
            .unwrap();
        assert_eq!(out.reply, "deep", "recovery needs no restart");
    }

    /// `ensure_with_policy` fans out the same failure bookkeeping as `ensure`:
    /// one lane failing to pin its roster against the cycle snapshot must block
    /// only that lane's agents, and a later successful `ensure_with_policy`
    /// clears the recorded failure. `FlakyEngine` overrides only `ensure`, and
    /// the trait default routes `ensure_with_policy` to it, so the same double
    /// exercises the router's own bookkeeping on this path.
    #[tokio::test]
    async fn ensure_with_policy_records_and_recovers_a_failed_lane() {
        let policy = Policy {
            mode: "supervised".to_string(),
            always_approve: Vec::new(),
            auto_approve_under_usd: None,
            approval_ttl_hours: None,
        };
        let embedded = FlakyEngine::new("embedded");
        let deep = FlakyEngine::new("deep");
        let router = HarnessRouter::new("embedded")
            .with_engine("embedded", embedded.clone())
            .with_engine("deep", deep.clone())
            .bind("researcher", "deep")
            .bind("ceo", "embedded");

        deep.set_fail(true);
        router.ensure_with_policy(&record(), &policy).await.unwrap();

        let err = router
            .run(&company(), "researcher", "hi", ChatTarget::default())
            .await
            .expect_err("the failed lane's turn must error");
        let msg = err.to_string();
        assert!(msg.contains("researcher"), "{msg}");
        assert!(msg.contains("deep"), "{msg}");
        assert!(msg.contains("warm-up"), "names the failed warm-up: {msg}");

        let out = router
            .run(&company(), "ceo", "hi", ChatTarget::default())
            .await
            .unwrap();
        assert_eq!(out.reply, "embedded", "the healthy lane keeps working");

        // A later success clears the entry, so the lane comes back.
        deep.set_fail(false);
        router.ensure_with_policy(&record(), &policy).await.unwrap();
        let out = router
            .run(&company(), "researcher", "hi", ChatTarget::default())
            .await
            .unwrap();
        assert_eq!(out.reply, "deep", "recovery needs no restart");
    }

    /// `end_cycle` fans the policy-pin release out to *every* lane, so a named
    /// lane's pool cannot keep rebuilding against a stale cycle snapshot after
    /// its cycle is over (issue #1455).
    #[tokio::test]
    async fn end_cycle_fans_out_to_every_lane() {
        let embedded = SpyEngine::new("embedded");
        let deep = SpyEngine::new("deep");
        let router = HarnessRouter::new("embedded")
            .with_engine("embedded", embedded.clone())
            .with_engine("deep", deep.clone())
            .bind("researcher", "deep")
            .bind("ceo", "embedded");

        router.end_cycle(&company()).await;

        assert_eq!(
            *embedded.cycle_ends.lock().unwrap(),
            vec![company()],
            "the default lane must receive the release"
        );
        assert_eq!(
            *deep.cycle_ends.lock().unwrap(),
            vec![company()],
            "a named lane must receive the release"
        );
    }

    /// The drop-guard half of the same fan-out: the synchronous release reaches
    /// every lane too, so a cancelled or panicked cycle (which cannot await
    /// `end_cycle`) still releases each pool's pin (issue #1455).
    #[test]
    fn release_policy_pin_sync_fans_out_to_every_lane() {
        let embedded = SpyEngine::new("embedded");
        let deep = SpyEngine::new("deep");
        let router = HarnessRouter::new("embedded")
            .with_engine("embedded", embedded.clone())
            .with_engine("deep", deep.clone())
            .bind("researcher", "deep")
            .bind("ceo", "embedded");

        router.release_policy_pin_sync(&company());

        assert_eq!(
            *embedded.cycle_ends.lock().unwrap(),
            vec![company()],
            "the default lane must receive the synchronous release"
        );
        assert_eq!(
            *deep.cycle_ends.lock().unwrap(),
            vec![company()],
            "a named lane must receive the synchronous release"
        );
    }
}
