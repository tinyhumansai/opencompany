use super::*;
use crate::ports::TaskStore;

// --- Issue #204: a dispatched turn that delegates -----------------------

/// Seeds one dispatched card (blank assignee → the orchestrator runs it,
/// which is the shape that carries the delegation tools) and dispatches it.
/// Dispatches a card and drives its hand-off chain to a settle, the way
/// `CompanyRuntime::run_dispatch_cycle` does in production.
///
/// One `run_cycle` is one ATTEMPT, and since the async hand-off an attempt
/// that hands the card on settles `Delegated` and leaves the card
/// `in_progress` for the new owner — the delegate runs in its own attempt,
/// which is what makes their spend attributable and releases the
/// per-company lock between hops. A helper that ran a single cycle would
/// therefore stop one attempt short of every hand-off's outcome, and a test
/// asking "where does a cancelled hand-off end up?" would be reading a
/// card mid-chain.
///
/// The loop condition is the runtime's own: a settled dispatch still in
/// `in_progress` has handed on, because every other ending lands the card
/// in a terminal column.
pub(super) async fn dispatch_card(brain: &HarnessBrain, tasks: &Arc<FsOps>, id: &str) {
    let mut c = card(id, "");
    c.column = "in_progress".to_string();
    tasks.upsert(&CompanyId::new("acme"), &c).await.unwrap();
    for _ in 0..=crate::company::runtime::MAX_HAND_OFF_HOPS {
        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: id.to_string(),
                    run_id: None,
                    origin_chat_id: None,
                    origin_parent: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");
        let handed_on = tasks
            .list(&CompanyId::new("acme"))
            .await
            .unwrap_or_default()
            .into_iter()
            .any(|card| card.id == id && card.column == crate::ports::tasks::COLUMN_IN_PROGRESS);
        if !handed_on {
            break;
        }
    }
}

// ---- named harnesses: does the wiring actually route? ------------------

/// Records which agents it ran, so a test can assert *which* lane served a
/// turn rather than only that one did.
pub(super) struct SpyLane {
    pub(super) label: String,
    pub(super) seen: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl crate::runtime::delegation::RunTurn for SpyLane {
    async fn run(
        &self,
        _company: &CompanyId,
        agent_id: &str,
        _message: &str,
        _chat: crate::runtime::delegation::ChatTarget<'_>,
    ) -> Result<crate::harness::built_in::TurnOutcome> {
        self.seen.lock().unwrap().push(agent_id.to_string());
        Ok(crate::harness::built_in::TurnOutcome {
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
        c: &CompanyId,
        a: &str,
        m: &str,
        _: &crate::company::steer::SteerControl,
        chat: crate::runtime::delegation::ChatTarget<'_>,
        _: Option<Arc<crate::harness::run_trace::RunTraceSink>>,
    ) -> Result<crate::harness::built_in::TurnOutcome> {
        self.run(c, a, m, chat).await
    }
    async fn run_steered_background(
        &self,
        c: &CompanyId,
        a: &str,
        m: &str,
        _: &crate::company::steer::SteerControl,
        _: crate::runtime::delegation::ChatTarget<'_>,
        _: Option<Arc<crate::harness::run_trace::RunTraceSink>>,
    ) -> Result<crate::harness::built_in::TurnOutcome> {
        self.run(c, a, m, crate::runtime::delegation::ChatTarget::default())
            .await
    }
}

/// A roster spanning two declared harnesses.
pub(super) fn two_harness_record() -> CompanyRecord {
    let manifest = toml::from_str(
        r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "ceo"
role = "Chief Executive"

[[agent]]
id = "researcher"
role = "Researcher"
harness = "deep"

[[harness]]
id = "embedded"
kind = "built_in"
default = true

[[harness]]
id = "deep"
kind = "built_in"
"#,
    )
    .expect("valid manifest");
    CompanyRecord {
        manifest,
        ..record()
    }
}

// ── Issue #1861: blockers park instead of settling Failed ───────────────
