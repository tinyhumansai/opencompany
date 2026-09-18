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
        _chat: ChatTarget<'_>,
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
        _chat: ChatTarget<'_>,
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
        overlay_desk_hive: Vec::new(),
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
            .run_steered_background(
                &company(),
                "researcher",
                "hi",
                &control,
                ChatTarget::default(),
                None,
            )
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

/// The classifier's harness arm is keyed on `engine_for`'s own wording, so
/// this drives the real router and asserts the real error still classifies.
/// A reworded sentence above fails here rather than dropping a bound-harness
/// failure back into the generic "send the message again" notice.
#[tokio::test]
async fn an_unavailable_harness_classifies_for_the_operator() {
    let router = HarnessRouter::new("embedded")
        .with_engine("embedded", SpyEngine::new("embedded"))
        .with_unavailable(
            "claude-code",
            "it is an ACP harness and this build has no ACP transport wired",
        )
        .bind("researcher", "claude-code");

    let err = router
        .run(&company(), "researcher", "hi", ChatTarget::default())
        .await
        .expect_err("a bound-but-unavailable harness must fail the turn");

    let got = crate::company::inference::copy::classify(&err.to_string())
        .expect("the router's own sentence must classify");
    assert_eq!(
        got.code,
        crate::company::inference::copy::HARNESS_UNAVAILABLE_CODE
    );
    assert_eq!(
        got.pair_agent_id.as_deref(),
        Some("researcher"),
        "the console links its action button at this id"
    );
    assert!(
        got.message.contains("claude-code"),
        "the harness must be named: {}",
        got.message
    );
    assert!(
        got.message
            .contains("this build has no ACP transport wired"),
        "the specific reason must survive: {}",
        got.message
    );
    assert!(
        !got.message.starts_with("configuration error"),
        "the error-type prefix is a diagnostic, not operator copy: {}",
        got.message
    );
}

/// The warm-up-failure sentence is the second spelling of the same shape and
/// must classify too — a lane that came up and then broke is the case an
/// operator is most likely to meet.
#[tokio::test]
async fn a_failed_warm_up_classifies_for_the_operator() {
    let flaky = FlakyEngine::new("deep");
    flaky.set_fail(true);
    let router = HarnessRouter::new("embedded")
        .with_engine("embedded", SpyEngine::new("embedded"))
        .with_engine("deep", flaky.clone())
        .bind("researcher", "deep");
    router.ensure(&record()).await.expect("ensure never fails");

    let err = router
        .run(&company(), "researcher", "hi", ChatTarget::default())
        .await
        .expect_err("a lane whose warm-up failed must fail its turns");

    let got = crate::company::inference::copy::classify(&err.to_string())
        .expect("the warm-up sentence must classify too");
    assert_eq!(
        got.code,
        crate::company::inference::copy::HARNESS_UNAVAILABLE_CODE
    );
    assert_eq!(got.pair_agent_id.as_deref(), Some("researcher"));
    assert!(
        !got.message.contains("roster warm-up failed"),
        "the lane's own warm-up error must not reach chat: {}",
        got.message
    );
    assert!(
        got.message.contains("whose last warm-up failed."),
        "the operator still learns which harness failed to warm up: {}",
        got.message
    );
}

/// `MessageView::project` and `chat_history` re-classify the stored text on
/// every read, and what is stored is this arm's own output — so classifying
/// it again must be a no-op rather than appending the note a second time.
#[tokio::test]
async fn reclassifying_the_stored_sentence_is_a_no_op() {
    let router = HarnessRouter::new("embedded")
        .with_unavailable("claude-code", "this host wires no engine for it")
        .bind("researcher", "claude-code");

    let err = router
        .run(&company(), "researcher", "hi", ChatTarget::default())
        .await
        .expect_err("a bound-but-unavailable harness must fail the turn");

    let once = crate::company::inference::copy::classify(&err.to_string())
        .expect("the router's own sentence must classify");
    let twice = crate::company::inference::copy::classify(&once.message)
        .expect("its own output must classify again");
    assert_eq!(
        once, twice,
        "a stored sentence must survive every re-read unchanged"
    );
}
