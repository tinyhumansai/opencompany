use super::tests_capped_halt::{GREET, deps, record};
use super::tests_journal::{STALLS, StallingProvider, deps_with_events, journaled};
use super::*;

use crate::company::parse_workflow;
use crate::harness::provider::MockProvider;
use crate::store::FsOps;

#[tokio::test]
async fn a_checkpoint_resume_can_be_hard_aborted_keeping_completed_nodes() {
    const GATED_STALL: &str = r#"
id = "gated-stall"
name = "Gated stall"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "gate"
kind = "transform"
name = "Gate"
requires_approval = true
[[node]]
id = "shape"
kind = "transform"
name = "Shape"
[[node]]
id = "ceo"
kind = "agent"
name = "CEO"
agent = "ceo"
prompt = "Think about it."
[[edge]]
from = "start"
to = "gate"
[[edge]]
from = "gate"
to = "shape"
[[edge]]
from = "shape"
to = "ceo"
"#;
    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(HarnessPool::new());
    let rec = record();
    let entered = Arc::new(tokio::sync::Notify::new());
    let (mut deps, events) = deps_with_events(dir.path());
    deps.provider = Arc::new(StallingProvider {
        entered: entered.clone(),
    });
    deps.provider_slug = "stalling".to_string();
    pool.ensure(&rec, &deps).await.expect("roster builds");
    let turn: Arc<dyn crate::runtime::delegation::RunTurn> = Arc::new(
        crate::harness::built_in::run_turn::HarnessRunTurn::new(pool, Arc::new(deps.clone())),
    );
    let file = parse_workflow(GATED_STALL).expect("workflow parses");
    let checkpoints = Arc::new(
        crate::workflows::checkpoint_store::WorkflowCheckpointStore::new(
            dir.path().join("checkpoints"),
        ),
    );
    let first_ctx = WorkflowRunContext::new(false);
    let paused = run_workflow_lane_aware_checkpointed(
        turn.clone(),
        deps.clone(),
        &rec,
        &file,
        Value::Null,
        &first_ctx,
        Some(checkpoints.clone()),
    )
    .await
    .expect("initial run pauses");
    assert_eq!(paused.pending_approvals, vec!["gate"]);

    let resume_ctx = WorkflowRunContext::new(false).with_checkpoint_resume(
        first_ctx.run_id,
        vec!["gate".to_string()],
        Vec::new(),
    );
    let cancel = resume_ctx.cancel.clone();
    let reached_agent = entered.notified();
    let mut resumed = Box::pin(run_workflow_lane_aware_checkpointed(
        turn,
        deps,
        &rec,
        &file,
        Value::Null,
        &resume_ctx,
        Some(checkpoints),
    ));
    tokio::select! {
        _ = &mut resumed => panic!("the resumed agent did not stall"),
        () = reached_agent => {}
    }
    cancel.cancel();
    let run = tokio::time::timeout(std::time::Duration::from_secs(5), resumed)
        .await
        .expect("checkpoint resume did not hard abort")
        .expect("cancelled checkpoint resume is not a failure");
    assert!(run.cancelled);

    let completed: Vec<_> = journaled(&events, &rec.id)
        .await
        .into_iter()
        .filter_map(|event| match event {
            CompanyEvent::WorkflowNodeFinished {
                run_id, node_id, ..
            } if run_id == resume_ctx.run_id => Some(node_id),
            _ => None,
        })
        .collect();
    assert_eq!(completed, vec!["gate", "shape"]);
}

/// A resumed run has no [`CancellationToken`](tinyflows::engine::CancellationToken)
/// wired into the engine call (`resume_with_checkpointer_journaled_observed`
/// takes none), so `resuming` short-circuits straight to the hard-abort arm
/// instead of flipping a token and waiting `CANCEL_HARD_ABORT_GRACE` for a
/// clean node-boundary wind-down the way a fresh or checkpointed-initial run
/// does. This is the timing half of
/// `a_checkpoint_resume_can_be_hard_aborted_keeping_completed_nodes`, which
/// only bounds the wait at the grace window itself (5s) and so cannot tell
/// "aborted immediately" from "aborted right at the edge of the grace".
#[tokio::test]
async fn a_checkpoint_resume_cancel_skips_the_grace_window() {
    const GATED_STALL: &str = r#"
id = "gated-stall"
name = "Gated stall"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "gate"
kind = "transform"
name = "Gate"
requires_approval = true
[[node]]
id = "shape"
kind = "transform"
name = "Shape"
[[node]]
id = "ceo"
kind = "agent"
name = "CEO"
agent = "ceo"
prompt = "Think about it."
[[edge]]
from = "start"
to = "gate"
[[edge]]
from = "gate"
to = "shape"
[[edge]]
from = "shape"
to = "ceo"
"#;
    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(HarnessPool::new());
    let rec = record();
    let entered = Arc::new(tokio::sync::Notify::new());
    let (mut deps, _events) = deps_with_events(dir.path());
    deps.provider = Arc::new(StallingProvider {
        entered: entered.clone(),
    });
    deps.provider_slug = "stalling".to_string();
    pool.ensure(&rec, &deps).await.expect("roster builds");
    let turn: Arc<dyn crate::runtime::delegation::RunTurn> = Arc::new(
        crate::harness::built_in::run_turn::HarnessRunTurn::new(pool, Arc::new(deps.clone())),
    );
    let file = parse_workflow(GATED_STALL).expect("workflow parses");
    let checkpoints = Arc::new(
        crate::workflows::checkpoint_store::WorkflowCheckpointStore::new(
            dir.path().join("checkpoints"),
        ),
    );
    let first_ctx = WorkflowRunContext::new(false);
    let paused = run_workflow_lane_aware_checkpointed(
        turn.clone(),
        deps.clone(),
        &rec,
        &file,
        Value::Null,
        &first_ctx,
        Some(checkpoints.clone()),
    )
    .await
    .expect("initial run pauses");
    assert_eq!(paused.pending_approvals, vec!["gate"]);

    let resume_ctx = WorkflowRunContext::new(false).with_checkpoint_resume(
        first_ctx.run_id,
        vec!["gate".to_string()],
        Vec::new(),
    );
    let cancel = resume_ctx.cancel.clone();
    let reached_agent = entered.notified();
    let mut resumed = Box::pin(run_workflow_lane_aware_checkpointed(
        turn,
        deps,
        &rec,
        &file,
        Value::Null,
        &resume_ctx,
        Some(checkpoints),
    ));
    tokio::select! {
        _ = &mut resumed => panic!("the resumed agent did not stall"),
        () = reached_agent => {}
    }

    let pressed = std::time::Instant::now();
    cancel.cancel();
    let run = tokio::time::timeout(CANCEL_HARD_ABORT_GRACE, resumed)
        .await
        .expect("checkpoint resume did not hard abort within the grace window")
        .expect("cancelled checkpoint resume is not a failure");
    let elapsed = pressed.elapsed();

    assert!(run.cancelled);
    assert!(
        elapsed < CANCEL_HARD_ABORT_GRACE,
        "cancelling a checkpoint resume took {elapsed:?} — at or past the grace window, \
         meaning the resume path waited for a clean node-boundary wind-down instead of \
         hard-aborting immediately"
    );
}

/// A checkpointed **initial** run (no `checkpoint_resume`) is not a resume,
/// but production always attaches a checkpoint store — see
/// `RuntimeBuilder`, which installs one unconditionally per company and
/// hands it to every `HarnessWorkflowRunner` it builds. Cancelling a run
/// like this must still spend the grace window before dropping the engine
/// future, the same as an unchequered run: an immediate drop would
/// interrupt whatever the current node's external effect was mid-request.
#[tokio::test]
async fn a_checkpointed_initial_run_still_gets_the_grace_window_on_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(HarnessPool::new());
    let rec = record();
    let entered = Arc::new(tokio::sync::Notify::new());
    let (mut deps, events) = deps_with_events(dir.path());
    deps.provider = Arc::new(StallingProvider {
        entered: entered.clone(),
    });
    deps.provider_slug = "stalling".to_string();
    pool.ensure(&rec, &deps).await.expect("roster builds");
    let turn: Arc<dyn crate::runtime::delegation::RunTurn> = Arc::new(
        crate::harness::built_in::run_turn::HarnessRunTurn::new(pool, Arc::new(deps.clone())),
    );
    let file = parse_workflow(STALLS).expect("workflow parses");
    let checkpoints = Arc::new(
        crate::workflows::checkpoint_store::WorkflowCheckpointStore::new(
            dir.path().join("checkpoints"),
        ),
    );
    let ctx = WorkflowRunContext::new(false);
    let cancel = ctx.cancel.clone();
    let reached_the_agent = entered.notified();

    let mut run = Box::pin(run_workflow_lane_aware_checkpointed(
        turn,
        deps,
        &rec,
        &file,
        Value::Null,
        &ctx,
        Some(checkpoints),
    ));
    tokio::select! {
        _ = &mut run => panic!("the run finished, so the agent node did not stall"),
        () = reached_the_agent => {}
    }

    let pressed = std::time::Instant::now();
    cancel.cancel();
    let run = tokio::time::timeout(std::time::Duration::from_secs(30), run)
        .await
        .expect("the cancelled checkpointed run never returned")
        .expect("a cancelled checkpointed run is not a failure");
    let elapsed = pressed.elapsed();

    assert!(run.cancelled, "the run must report that it was stopped");
    assert!(
        elapsed >= CANCEL_HARD_ABORT_GRACE,
        "cancelling took {elapsed:?} — an immediate hard-abort skipped the grace window that \
         gives an in-flight node on a checkpointed initial run a chance to finish before its \
         outcome is discarded"
    );
    assert!(
        elapsed < CANCEL_HARD_ABORT_GRACE + std::time::Duration::from_secs(2),
        "cancelling took {elapsed:?} — past the grace the run did not settle quickly"
    );

    let nodes: Vec<String> = journaled(&events, &rec.id)
        .await
        .into_iter()
        .filter_map(|event| match event {
            CompanyEvent::WorkflowNodeFinished { node_id, .. } => Some(node_id),
            _ => None,
        })
        .collect();
    assert_eq!(nodes, vec!["shape".to_string()]);
}

/// A model that announces it was reached, then waits to be released before
/// answering. The lever for the race below: unlike [`StallingProvider`],
/// this one is meant to finish — quickly, and only once the test says so —
/// so the cancelled run's grace window can be won by the engine's own
/// completion rather than by outlasting it.
struct ReleasableProvider {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl tinyinference::model::ChatModel<()> for ReleasableProvider {
    async fn invoke(
        &self,
        _state: &(),
        _request: tinyinference::model::ModelRequest,
    ) -> tinyinference::Result<tinyinference::model::ModelResponse> {
        self.entered.notify_waiters();
        self.release.notified().await;
        Ok(tinyinference::model::ModelResponse::assistant(
            "released".to_string(),
        ))
    }
}

impl crate::harness::provider::HarnessModel for ReleasableProvider {
    fn telemetry_provider_id(&self) -> String {
        "releasable".to_string()
    }
}

/// `start → shape → ceo → done`, same shape as [`STALLS`] but `done` routes
/// to a desk channel — so a run that reaches it actually sends something a
/// test can catch.
const RELEASABLE_THEN_DELIVERS: &str = r#"
id = "releasable"
name = "Releasable"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "shape"
kind = "transform"
name = "Shape"
[[node]]
id = "ceo"
kind = "agent"
name = "CEO"
agent = "ceo"
prompt = "Think about it."
[[node]]
id = "done"
kind = "output"
name = "Owner summary"
[node.destination]
kind = "channel"
target = "engineering"
[[edge]]
from = "start"
to = "shape"
[[edge]]
from = "shape"
to = "ceo"
[[edge]]
from = "ceo"
to = "done"
"#;

/// **The race the grace window can lose (PR #1991 review, `3903797606`).**
/// A checkpointed *initial* run's cancellation token is a no-op —
/// `run_with_checkpointer_journaled_observed` takes no `CancellationToken`
/// — so the grace window this runner spends before dropping the future is
/// not racing the operator's cancel against the node; it is racing it
/// against nothing. If the agent node answers inside the grace window (as
/// one genuinely close to finishing would), the engine hands back an
/// ordinary successful `RunOutcome` with `cancelled == false`. Without the
/// override in `run_workflow_inner`, that outcome falls straight through
/// to the delivery routing at the bottom of the function — a cancelled run
/// mailing its report as though the cancel never happened.
#[tokio::test]
async fn a_cancelled_checkpointed_initial_run_that_finishes_inside_grace_does_not_deliver() {
    use crate::runtime::channel::RecordingChannel;

    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(HarnessPool::new());
    let rec = record();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let channel = RecordingChannel::new("engineering");
    let mut deps = deps(dir.path());
    deps.provider = Arc::new(ReleasableProvider {
        entered: entered.clone(),
        release: release.clone(),
    });
    deps.provider_slug = "releasable".to_string();
    deps.delivery = Some(crate::workflows::WorkflowDeliveryDeps {
        mail: None,
        inbox: Arc::new(crate::store::FsInboxStore::new(dir.path())),
        users: Arc::new(FsOps::new(dir.path())),
        bootstrap_admin: None,
        channels: vec![Arc::new(channel.clone())],
        notifications: None,
        parking: None,
        events: Arc::new(crate::store::FsEventLog::new(dir.path())),
    });
    pool.ensure(&rec, &deps).await.expect("roster builds");
    let turn: Arc<dyn crate::runtime::delegation::RunTurn> = Arc::new(
        crate::harness::built_in::run_turn::HarnessRunTurn::new(pool, Arc::new(deps.clone())),
    );
    let file = parse_workflow(RELEASABLE_THEN_DELIVERS).expect("workflow parses");
    let checkpoints = Arc::new(
        crate::workflows::checkpoint_store::WorkflowCheckpointStore::new(
            dir.path().join("checkpoints"),
        ),
    );
    let ctx = WorkflowRunContext::new(false);
    let cancel = ctx.cancel.clone();
    let reached_the_agent = entered.notified();

    let mut run = Box::pin(run_workflow_lane_aware_checkpointed(
        turn,
        deps,
        &rec,
        &file,
        Value::Null,
        &ctx,
        Some(checkpoints),
    ));
    tokio::select! {
        _ = &mut run => panic!("the run finished, so the agent node did not stall"),
        () = reached_the_agent => {}
    }

    let pressed = std::time::Instant::now();
    cancel.cancel();
    // Release the node's answer immediately after the cancel: the whole
    // point is that the engine settles well inside the grace window by
    // finishing on its own, not by outlasting it.
    release.notify_one();
    let run = tokio::time::timeout(CANCEL_HARD_ABORT_GRACE, run)
        .await
        .expect("the cancelled run never returned")
        .expect("a cancelled checkpointed run is not a failure");
    let elapsed = pressed.elapsed();

    assert!(
        elapsed < CANCEL_HARD_ABORT_GRACE,
        "the node answered almost immediately after cancel — {elapsed:?} to settle means \
         this run reached the grace-window timeout instead of racing the engine's own \
         completion, which defeats the point of this test"
    );
    assert!(
        run.cancelled,
        "a run cancelled mid-flight must report that it was stopped even when the \
         checkpointer's no-op token let the engine finish anyway"
    );
    assert!(
        run.deliveries.is_empty(),
        "a cancelled run must not proceed to delivery just because its no-op-token engine \
         call happened to finish inside the grace window: {:?}",
        run.deliveries
    );
    assert!(
        channel.sent().is_empty(),
        "the desk channel must never receive a report from a run the operator cancelled"
    );
}

/// The same stop works with **no journal wired** — the default-build shape,
/// where there is no observer and no collector at all.
///
/// A separate test because it is a separate `select!`: the two arms of the
/// `events` match each drive the engine differently, and an early version of
/// this change made only the observed one cancellable. Nothing else would
/// have noticed.
#[tokio::test]
async fn a_run_with_no_journal_is_cancellable_too() {
    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(HarnessPool::new());
    let rec = record();
    let entered = Arc::new(tokio::sync::Notify::new());
    let mut deps = deps(dir.path());
    assert!(deps.events.is_none(), "this is the default-build shape");
    deps.provider = Arc::new(StallingProvider {
        entered: entered.clone(),
    });
    pool.ensure(&rec, &deps).await.expect("roster builds");

    let file = parse_workflow(STALLS).expect("workflow parses");
    let ctx = WorkflowRunContext::new(false);
    let cancel = ctx.cancel.clone();
    let reached_the_agent = entered.notified();

    let mut run = Box::pin(run_workflow(pool, deps, &rec, &file, Value::Null, &ctx));
    tokio::select! {
        _ = &mut run => panic!("the run finished, so the agent node did not stall"),
        () = reached_the_agent => {}
    }

    cancel.cancel();
    let run = tokio::time::timeout(std::time::Duration::from_secs(30), run)
        .await
        .expect("the cancelled run never returned")
        .expect("a cancelled run is Ok, not Err");
    assert!(run.cancelled);
}

/// A run whose signal is fired **before** it starts stops immediately rather
/// than walking the graph anyway.
///
/// This is the watch-vs-`Notify` property, proven through the runner rather
/// than the primitive: with an edge-triggered signal the `select!` would
/// miss the already-fired cancel and the run would complete normally, which
/// is exactly the race a cancel arriving during graph compilation would hit.
#[tokio::test]
async fn a_run_cancelled_before_it_starts_does_not_walk_the_graph() {
    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(HarnessPool::new());
    let rec = record();
    let (deps, events) = deps_with_events(dir.path());
    pool.ensure(&rec, &deps).await.expect("roster builds");

    let file = parse_workflow(GREET).expect("workflow parses");
    let ctx = WorkflowRunContext::new(false);
    ctx.cancel.cancel();

    let run = run_workflow(pool, deps, &rec, &file, Value::Null, &ctx)
        .await
        .expect("a cancelled run is Ok, not Err");
    assert!(run.cancelled);

    let journal = journaled(&events, &rec.id).await;
    assert!(
        !journal
            .iter()
            .any(|e| matches!(e, CompanyEvent::WorkflowNodeFinished { .. })),
        "a run cancelled before it began must not report any node as finished"
    );
}

/// A model that blocks its first call until the test releases it, after
/// announcing that it got there — but then, unlike [`StallingProvider`],
/// **returns normally**. That is what makes a *clean* cancel possible: the
/// agent node completes, the engine hits the next boundary, sees the flipped
/// token, and winds the run down rather than being dropped mid-await.
pub(super) struct GatedProvider {
    pub(super) inner: MockProvider,
    pub(super) entered: Arc<tokio::sync::Notify>,
    pub(super) release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl tinyinference::model::ChatModel<()> for GatedProvider {
    async fn invoke(
        &self,
        state: &(),
        request: tinyinference::model::ModelRequest,
    ) -> tinyinference::Result<tinyinference::model::ModelResponse> {
        self.entered.notify_waiters();
        // `notify_one` carries a permit, so a release that lands before this
        // registers is not lost — no ordering race with the test.
        self.release.notified().await;
        tinyinference::model::ChatModel::invoke(&self.inner, state, request).await
    }
}

impl crate::harness::provider::HarnessModel for GatedProvider {
    fn telemetry_provider_id(&self) -> String {
        "gated".to_string()
    }
}
