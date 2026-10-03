//! `built_in`'s own inline tests, part 8 of 10. Split out of the
//! single inline `mod tests` block because it exceeded the 750-line file
//! limit; grouped in the original file's order, not by topic (the block
//! covered dozens of unrelated issues with no existing topical boundaries).
//! Shared setup lives in [`super::built_in_test_fixtures`] and
//! [`super::built_in_test_fixtures_2`].

use super::built_in_test_fixtures::*;
use super::built_in_test_fixtures_2::*;
use super::*;
use async_trait::async_trait;
use tinyinference::model::{ChatModel, ModelRequest, ModelResponse};

use crate::ports::UsageSample;
use crate::ports::types::LedgerEntry;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_inference_turns_cannot_both_spend_the_last_monthly_budget() {
    let _serial = CEILING_SERIAL.lock().await;
    struct DelayedProvider(ScriptedProvider);

    #[async_trait]
    impl ChatModel<()> for DelayedProvider {
        async fn invoke(
            &self,
            state: &(),
            request: ModelRequest,
        ) -> tinyinference::Result<ModelResponse> {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            self.0.invoke(state, request).await
        }
    }

    impl HarnessModel for DelayedProvider {
        fn telemetry_provider_id(&self) -> String {
            "monthly-budget-race".to_string()
        }
    }

    for attempt in 0..5 {
        let dir = tempfile::tempdir().expect("temporary workspace");
        let store = Arc::new(crate::store::FsCompanyStore::new(dir.path()));
        let provider = Arc::new(DelayedProvider(
            ScriptedProvider::new(vec![Ok("completed".to_string()); 2]).reporting_usage(
                tinyinference::Usage {
                    input_tokens: 1_200,
                    output_tokens: 340,
                    total_tokens: 1_540,
                    ..Default::default()
                },
            ),
        ));
        let mut deps = deps_with_plan(dir.path(), Arc::new(MockContext::default()), None, None);
        deps.store = store.clone();
        deps.provider = provider.clone();
        let deps = Arc::new(deps);

        let mut rec = record();
        rec.id = CompanyId::new(format!("monthly-budget-race-{attempt}"));
        rec.manifest.budget.monthly_usd = Some(1.0);
        store.save(&rec).await.expect("company is persisted");
        store
            .append_ledger(
                &rec.id,
                LedgerEntry {
                    at_millis: crate::ports::now_millis(),
                    kind: "inference.spend".to_string(),
                    amount_usd: -0.999_999,
                    memo: "prior inference".to_string(),
                },
            )
            .await
            .expect("prior inference spend is persisted");

        let pool = Arc::new(HarnessPool::new());
        pool.ensure(&rec, &deps).await.expect("roster builds");
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let mut racers = tokio::task::JoinSet::new();
        for _ in 0..2 {
            let barrier = barrier.clone();
            let pool = pool.clone();
            let deps = deps.clone();
            let company = rec.id.clone();
            racers.spawn(async move {
                barrier.wait().await;
                pool.run(
                    &company,
                    "ceo",
                    "answer once",
                    &deps,
                    crate::runtime::delegation::ChatTarget::default(),
                )
                .await
            });
        }
        while let Some(result) = racers.join_next().await {
            result.expect("racer joins").expect("dispatch resolves");
        }
        assert_eq!(
            provider.0.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "attempt {attempt}: one remaining monthly budget must admit exactly one model call"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_turns_cannot_dispatch_against_the_same_total_budget() {
    let _serial = CEILING_SERIAL.lock().await;
    struct DelayedUsageProvider(ScriptedProvider);

    #[async_trait]
    impl ChatModel<()> for DelayedUsageProvider {
        async fn invoke(
            &self,
            state: &(),
            request: ModelRequest,
        ) -> tinyinference::Result<ModelResponse> {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            self.0.invoke(state, request).await
        }
    }

    impl HarnessModel for DelayedUsageProvider {
        fn telemetry_provider_id(&self) -> String {
            "budget-race".to_string()
        }
    }

    for attempt in 0..5 {
        let dir = tempfile::tempdir().expect("temporary workspace");
        let meter = Arc::new(RecordingMeter::default());
        let provider = Arc::new(DelayedUsageProvider(
            ScriptedProvider::new(vec![Ok("completed".to_string()); 4]).reporting_usage(
                tinyinference::Usage {
                    input_tokens: 60,
                    output_tokens: 40,
                    total_tokens: 100,
                    ..Default::default()
                },
            ),
        ));
        let mut deps = deps_with_plan(
            dir.path(),
            Arc::new(MockContext::default()),
            Some(meter.clone()),
            Some(crate::harness::capability_budget::CapabilityPlan {
                period: crate::harness::capability_budget::BudgetPeriod::Daily,
                budgets: std::collections::BTreeMap::new(),
                total_budget: Some(100),
            }),
        );
        deps.provider = provider.clone();
        let deps = Arc::new(deps);
        let pool = Arc::new(HarnessPool::new());
        let mut rec = record();
        rec.id = CompanyId::new(format!("budget-race-{attempt}"));
        pool.ensure(&rec, &deps).await.expect("roster builds");
        let barrier = Arc::new(tokio::sync::Barrier::new(4));
        let mut racers = tokio::task::JoinSet::new();
        for _ in 0..4 {
            let barrier = barrier.clone();
            let pool = pool.clone();
            let deps = deps.clone();
            let company = rec.id.clone();
            racers.spawn(async move {
                barrier.wait().await;
                pool.run(
                    &company,
                    "ceo",
                    "answer once",
                    &deps,
                    crate::runtime::delegation::ChatTarget::default(),
                )
                .await
            });
        }
        let mut completed = 0;
        while let Some(result) = racers.join_next().await {
            let outcome = result.expect("racer joins").expect("dispatch resolves");
            if outcome.reply != TOTAL_BUDGET_EXHAUSTED_NOTICE {
                completed += 1;
            }
        }
        assert_eq!(
            provider.0.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "attempt {attempt}: one remaining budget must admit exactly one model call"
        );
        assert_eq!(completed, 1, "exactly one turn completes under the ceiling");
        assert_eq!(
            capability_budget::tokens_in(&meter.query(&rec.id, 0).await.expect("spend")),
            100,
            "concurrent dispatch must not multiply the available token budget"
        );
    }
}

/// Every test in this file drives the total-ceiling gate for the same company
/// (`record()` is always `acme`), and that gate reserves against
/// `metering::reservation::IN_FLIGHT`, a process-global map keyed by company.
/// Run concurrently, one test's held reservation is another test's "ceiling
/// crossed". They were spread across the old 8k-line inline module and rarely
/// collided; grouped here they collide on every run, so each holds this lock
/// for its duration. Serialising the file, not the crate: the lock is local.
static CEILING_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The hard total-token ceiling (issue #188): once the tenant's total period
/// spend crosses the plan's `total_budget`, the very next dispatch is refused
/// **before any model call** — the reply is the fixed operator notice, the
/// prompt is never echoed (proving the model was not run), and no fabricated
/// outcome lands in memory. A turn under the ceiling still runs normally.
#[tokio::test]
async fn run_refuses_dispatch_once_the_total_ceiling_is_crossed() {
    let _serial = CEILING_SERIAL.lock().await;
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
    let _serial = CEILING_SERIAL.lock().await;
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

/// A declared total ceiling that cannot be read refuses dispatch (both
/// arms), rather than admitting a priced turn against a bound nobody can
/// measure. The two unreadable cases are distinct faults and say so: an
/// absent meter can never enforce the cap on this host, an erroring meter
/// is a transient read that clears on the next one.
#[tokio::test]
async fn run_refuses_when_a_declared_total_ceiling_cannot_be_read() {
    let _serial = CEILING_SERIAL.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let context = Arc::new(MockContext::default());
    // A generous ceiling: a readable meter would admit this turn, so the
    // refusal below can only come from the spend read failing.
    let plan = || crate::harness::capability_budget::CapabilityPlan {
        period: crate::harness::capability_budget::BudgetPeriod::Daily,
        budgets: std::collections::BTreeMap::new(),
        total_budget: Some(1_000_000),
    };
    let rec = record();

    // No meter wired: the cap is declared on a host that can never measure
    // it — a deployment fault, not a transient one.
    let no_meter = deps_with_plan(dir.path(), context.clone(), None, Some(plan()));
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
        .expect("a refusal is a benign outcome, not a hard error")
        .reply;
    assert!(
        !reply.contains("hello-marker"),
        "no model call may run against an unmeasurable ceiling: {reply:?}"
    );
    assert_eq!(
        reply,
        unmeasurable_ceiling_notice(&SpendReadFault::NoMeter),
        "an absent meter is reported as the deployment fault it is, and the reply says what \
         the operator has to change"
    );
    let no_meter_reply = reply;

    // A meter that errors: the same refusal, a different fault.
    let failing = deps_with_plan(
        dir.path(),
        context.clone(),
        Some(Arc::new(FailingMeter) as Arc<dyn UsageMeter>),
        Some(plan()),
    );
    let pool = HarnessPool::new();
    pool.ensure(&rec, &failing).await.expect("ensure");
    let reply = pool
        .run(
            &rec.id,
            "ceo",
            "hello-marker",
            &failing,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect("a refusal is a benign outcome, not a hard error")
        .reply;
    assert!(
        !reply.contains("hello-marker"),
        "no model call may run against an unreadable ceiling: {reply:?}"
    );
    assert_eq!(
        reply,
        unmeasurable_ceiling_notice(&SpendReadFault::QueryFailed(OpenCompanyError::Store(
            "meter unavailable".into()
        ))),
        "a failed read is reported as transient, not as a misconfigured host"
    );
    assert_ne!(
        reply, no_meter_reply,
        "the two faults are not the same fault and must not read as one"
    );
}

/// A company that declares NO total ceiling is untouched by the rule above:
/// nothing to enforce means nothing to fail closed on, meter or no meter.
#[tokio::test]
async fn a_company_with_no_declared_ceiling_runs_without_a_meter() {
    let _serial = CEILING_SERIAL.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let context = Arc::new(MockContext::default());
    let deps = deps_with_plan(dir.path(), context.clone(), None, None);
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
        .expect("an unbounded company keeps running")
        .reply;
    assert!(
        reply.contains("hello-marker"),
        "no declared cap means no gate: {reply:?}"
    );
}

/// `spend_gate_refusal` must set `abnormal_stop`: `HarnessAgentRunner`
/// (`workflows::caps`) and hive's `terminal_budget_error` both key off it
/// to keep a pre-dispatch refusal from settling a workflow/card attempt or
/// a hive turn `Succeeded` and binding the refusal notice downstream as if
/// it were the node's or the teammate's real answer.
#[test]
fn spend_gate_refusal_carries_an_abnormal_stop() {
    let outcome = spend_gate_refusal("refused".to_string(), SpendGateCause::Unmeasurable);
    assert!(
        outcome.abnormal_stop.is_some(),
        "a pre-dispatch refusal must not read like a clean finish downstream"
    );
    assert!(!outcome.hit_iteration_cap);
    assert!(outcome.halted_for_spend.is_none());
    assert!(outcome.budget_paused.is_none());
}

/// The heart of #304 at the layer that carries the money: once a teammate
/// has spent its manifest `budget_usd_daily`, its next dispatch is refused
/// **before any model call** — while its uncapped colleague keeps working.
///
/// This is the layer that matters, because the dominant spend stream is
/// inference and inference never reaches a `ToolPolicy`. Gating only priced
/// tool calls would leave a capped teammate free to burn its budget many
/// times over on model turns alone.
#[test]
fn an_exhausted_cap_and_an_unreadable_meter_do_not_read_alike() {
    let exhausted = spend_gate_refusal("refused".to_string(), SpendGateCause::Exhausted);
    let unmeasurable = spend_gate_refusal("refused".to_string(), SpendGateCause::Unmeasurable);
    assert_ne!(
        exhausted.abnormal_stop, unmeasurable.abnormal_stop,
        "an exhausted cap sent to the meter-fault reason points the operator at a meter that works"
    );
    assert!(
        exhausted
            .abnormal_stop
            .as_deref()
            .is_some_and(|stop| stop.contains("exhausted")),
        "the exhausted reason must name the cap, not the measurement"
    );
}

#[tokio::test]
async fn run_refuses_dispatch_for_a_teammate_over_its_daily_cap() {
    let _serial = CEILING_SERIAL.lock().await;
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

/// a cap of exactly `0.0` passes validation as "non-negative and
/// finite" and then permanently refuses every dispatch, because `spent >=
/// cap` holds even at zero spend on the very first turn — before the
/// teammate has ever run once. Setting `0.0` bricks the teammate; it does
/// not uncap it, and nothing here says so.
#[tokio::test]
async fn a_zero_daily_cap_refuses_the_teammates_very_first_turn() {
    let _serial = CEILING_SERIAL.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let context = Arc::new(MockContext::default());
    let meter = Arc::new(RecordingMeter::default());
    let rec = zero_capped_record();

    // No spend has ever been recorded for this teammate — a fresh day, a
    // fresh company, or a cap just set to `0.0` from the console.
    let deps = deps_with_plan(
        dir.path(),
        context.clone(),
        Some(meter.clone() as Arc<dyn UsageMeter>),
        None,
    );
    let pool = HarnessPool::new();
    pool.ensure(&rec, &deps).await.expect("ensure");

    let refused = pool
        .run(
            &rec.id,
            "treasurer",
            "should-not-echo",
            &deps,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect("a refusal is a benign outcome, not a hard error")
        .reply;
    assert_eq!(
        refused,
        agent_budget_exhausted_notice("treasurer", 0.0),
        "the very first dispatch is refused, though this teammate has spent nothing yet"
    );
    assert!(!refused.contains("should-not-echo"));

    // The cap is per-teammate: the uncapped engineer is untouched.
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
    assert!(ok.contains("hello-marker"), "{ok:?}");
}
