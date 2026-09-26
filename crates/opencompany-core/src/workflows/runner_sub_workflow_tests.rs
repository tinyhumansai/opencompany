use super::tests_capped_halt::{deps, deps_with_source, record, tools_record, write_wf};
use super::tests_node_kinds::RecordingSlowProvider;
use super::*;

use crate::company::parse_workflow;

#[async_trait]
impl tinyinference::model::ChatModel<()> for RecordingSlowProvider {
    async fn invoke(
        &self,
        _state: &(),
        request: tinyinference::model::ModelRequest,
    ) -> tinyinference::Result<tinyinference::model::ModelResponse> {
        // Scan the whole conversation, not just the last user turn: the
        // openhuman harness reshapes an agent node's authored instruction
        // into a multi-message prompt, so the node's marker can land in any
        // role. Matching the joined text keeps the probe robust to that.
        let all_text = request
            .messages
            .iter()
            .map(|m| m.text())
            .collect::<Vec<_>>()
            .join("\n");
        self.seen.lock().expect("seen mutex").push(all_text.clone());
        if all_text.contains("SLOW-NODE") {
            // Announce arrival, then hold the child at this node until the run
            // is cancelled (bounded, so a broken build cannot hang CI). This
            // pins the cancel to land while `slow` is in flight and makes the
            // wind-down a clean node-boundary stop, not a hard abort.
            self.entered_slow.notify_waiters();
            tokio::select! {
                () = self.cancel.cancelled() => {}
                () = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
            }
        }
        Ok(tinyinference::model::ModelResponse::assistant(
            "acknowledged".to_string(),
        ))
    }
}

impl crate::harness::provider::HarnessModel for RecordingSlowProvider {
    fn telemetry_provider_id(&self) -> String {
        "recording-slow".to_string()
    }
}
/// **Full-stack cancel propagation into a `sub_workflow` child (issue #675).**
/// Parent `trigger → sub_workflow(child) → done`, child
/// `trigger → slow → marker → done` where `slow`/`marker` are agent nodes.
/// The operator cancels while the child's `slow` node is mid-flight; the
/// parent's `CancellationToken` must reach the child run so its `marker` node
/// never executes, the run settles `cancelled`, and it comes back promptly
/// (the clean node-boundary wind-down bounded by `slow`'s remainder — not the
/// hard-abort grace). Before the fix the child ran behind a fresh token, so
/// the cancel never crossed the boundary and `marker` executed.
#[tokio::test]
async fn a_parent_cancel_propagates_into_a_sub_workflow_child() {
    let source = tempfile::tempdir().unwrap();
    write_wf(
        source.path(),
        "child",
        r#"
id = "child"
name = "Child"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "slow"
kind = "agent"
name = "Slow"
agent = "ceo"
summary = "SLOW-NODE hold here until cancelled"
prompt = "SLOW-NODE hold here until cancelled"
[[node]]
id = "marker"
kind = "agent"
name = "Marker"
agent = "ceo"
summary = "MARKER-NODE must never run once cancellation propagates"
prompt = "MARKER-NODE must never run once cancellation propagates"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "slow"
[[edge]]
from = "slow"
to = "marker"
[[edge]]
from = "marker"
to = "done"
"#,
    );
    let parent = r#"
id = "parent"
name = "Parent"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "sub"
kind = "sub_workflow"
name = "Sub"
[node.config]
workflow_id = "child"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "sub"
[[edge]]
from = "sub"
to = "done"
"#;

    let home = tempfile::tempdir().unwrap();
    let pool = Arc::new(HarnessPool::new());
    let rec = record();
    let ctx = WorkflowRunContext::new(false);
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let entered = Arc::new(tokio::sync::Notify::new());

    let mut deps = deps_with_source(home.path(), source.path());
    deps.provider = Arc::new(RecordingSlowProvider {
        seen: seen.clone(),
        entered_slow: entered.clone(),
        cancel: ctx.cancel.clone(),
    });
    deps.provider_slug = "recording-slow".to_string();
    pool.ensure(&rec, &deps).await.expect("roster builds");

    let file = parse_workflow(parent).expect("parent parses");
    // Registered before the run starts so `slow` cannot slip past it.
    let reached_slow = entered.notified();

    let mut run = Box::pin(run_workflow(pool, deps, &rec, &file, Value::Null, &ctx));
    tokio::select! {
        _ = &mut run => panic!(
            "the run finished before `slow` was reached; provider saw: {:?}",
            seen.lock().expect("seen mutex")
        ),
        () = reached_slow => {}
    }

    // The operator presses Cancel while the child's `slow` node is in flight.
    let pressed = std::time::Instant::now();
    ctx.cancel.cancel();
    let run = tokio::time::timeout(std::time::Duration::from_secs(30), run)
        .await
        .expect("the cancelled run never returned")
        .expect("a cancelled run is Ok, not Err");
    let elapsed = pressed.elapsed();

    assert!(run.cancelled, "the run must report that it was stopped");

    let seen = seen.lock().expect("seen mutex").clone();
    assert!(
        seen.iter().any(|m| m.contains("SLOW-NODE")),
        "the in-flight child `slow` node ran: {seen:?}"
    );
    assert!(
        !seen.iter().any(|m| m.contains("MARKER-NODE")),
        "cancellation must propagate into the child: its `marker` node should never run, \
         got {seen:?}"
    );
    // A clean node-boundary wind-down bounded by `slow`'s remainder — nowhere
    // near the hard-abort grace, which only a wedged (never-returning) node
    // would reach.
    assert!(
        elapsed < CANCEL_HARD_ABORT_GRACE,
        "the child wound down cleanly, so settle time should be well under the hard-abort \
         grace; took {elapsed:?}"
    );
}

/// T-cycle — two on-disk workflows referencing each other by id hard-reject
/// with the static cycle message, not the depth backstop.
#[tokio::test]
async fn t_mutual_sub_workflows_hard_reject() {
    let source = tempfile::tempdir().unwrap();
    let flow = |id: &str, other: &str| {
        format!(
            r#"
id = "{id}"
name = "{id}"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "sub"
kind = "sub_workflow"
name = "Sub"
[node.config]
workflow_id = "{other}"
[[edge]]
from = "start"
to = "sub"
"#
        )
    };
    write_wf(source.path(), "flow_a", &flow("flow_a", "flow_b"));
    write_wf(source.path(), "flow_b", &flow("flow_b", "flow_a"));

    let home = tempfile::tempdir().unwrap();
    let file = parse_workflow(&flow("flow_a", "flow_b")).expect("parent parses");
    let err = run_workflow(
        Arc::new(HarnessPool::new()),
        deps_with_source(home.path(), source.path()),
        &tools_record(),
        &file,
        serde_json::json!({}),
        &WorkflowRunContext::new(false),
    )
    .await
    .expect_err("a mutual sub_workflow reference must be refused");
    assert!(err.to_string().contains("cycle"), "{err}");
}

/// T-dynamic-id — a `=expr`-bound `workflow_id` resolves the child at run
/// time from the trigger input, proving dynamic references work.
#[tokio::test]
async fn t_expr_bound_workflow_id_resolves_dynamically() {
    let source = tempfile::tempdir().unwrap();
    write_wf(
        source.path(),
        "greet_child",
        r#"
id = "greet_child"
name = "Greet child"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "mark"
kind = "transform"
name = "Mark"
[node.config.set]
dynamic_marker = "=99"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "mark"
[[edge]]
from = "mark"
to = "done"
"#,
    );
    // The parent's sub_workflow reads its child id from the trigger input.
    let parent = r#"
id = "dyn_parent"
name = "Dynamic parent"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "sub"
kind = "sub_workflow"
name = "Sub"
[node.config]
workflow_id = "=item.target"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "sub"
[[edge]]
from = "sub"
to = "done"
"#;
    let home = tempfile::tempdir().unwrap();
    let file = parse_workflow(parent).expect("parent parses");
    let run = run_workflow(
        Arc::new(HarnessPool::new()),
        deps_with_source(home.path(), source.path()),
        &tools_record(),
        &file,
        serde_json::json!({ "target": "greet_child" }),
        &WorkflowRunContext::new(false),
    )
    .await
    .expect("dynamic sub_workflow run completes");
    assert!(
        run.output.to_string().contains("dynamic_marker"),
        "the expr-resolved child should have run: {}",
        run.output
    );
}

/// A trivial graph is enough — the guard fires before translation.
const TRIVIAL: &str = r#"
id = "trivial"
name = "Trivial"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "done"
"#;

/// Outside any run the chain is empty, so nothing is refused.
#[tokio::test]
async fn depth_is_zero_outside_a_run() {
    assert_eq!(current_workflow_depth(), 0);
}

/// Each nested run sees the ones already on the chain — this is what makes
/// the guard count a causal chain rather than a moment in time.
#[tokio::test]
async fn depth_accumulates_down_a_nested_chain() {
    WORKFLOW_DEPTH
        .scope(1, async {
            assert_eq!(current_workflow_depth(), 1);
            WORKFLOW_DEPTH
                .scope(2, async {
                    assert_eq!(current_workflow_depth(), 2);
                })
                .await;
            // Leaving the inner scope restores the outer depth.
            assert_eq!(current_workflow_depth(), 1);
        })
        .await;
    assert_eq!(current_workflow_depth(), 0);
}

/// Two runs side by side are not a chain. A shared counter would refuse the
/// second; a task-local correctly sees each at depth 0.
#[tokio::test]
async fn concurrent_unrelated_runs_do_not_stack() {
    let a = WORKFLOW_DEPTH.scope(1, async { current_workflow_depth() });
    let b = async { current_workflow_depth() };
    let (inside, outside) = tokio::join!(a, b);
    assert_eq!(inside, 1);
    assert_eq!(
        outside, 0,
        "a concurrent run must not inherit another chain's depth"
    );
}

/// At the limit the run is refused with a message naming the workflow and
/// the limit — and, critically, it returns rather than recursing.
#[tokio::test]
async fn a_run_at_the_limit_is_refused_with_an_actionable_error() {
    let dir = tempfile::tempdir().unwrap();
    let file = crate::company::parse_workflow(TRIVIAL).expect("parses");

    let err = WORKFLOW_DEPTH
        .scope(MAX_WORKFLOW_DEPTH, async {
            run_workflow(
                Arc::new(HarnessPool::new()),
                deps(dir.path()),
                &tools_record(),
                &file,
                Value::Null,
                &WorkflowRunContext::new(false),
            )
            .await
        })
        .await
        .expect_err("a run at the re-entry limit must be refused");

    let msg = err.to_string();
    assert!(msg.contains("trivial"), "must name the workflow: {msg}");
    assert!(msg.contains("re-entry limit"), "{msg}");
    assert!(
        msg.contains(&MAX_WORKFLOW_DEPTH.to_string()),
        "must state the limit: {msg}"
    );
}

/// One level below the limit still runs — the guard bounds recursion, it
/// does not ban nesting.
#[tokio::test]
async fn a_run_below_the_limit_still_executes() {
    let dir = tempfile::tempdir().unwrap();
    let file = crate::company::parse_workflow(TRIVIAL).expect("parses");

    let out = WORKFLOW_DEPTH
        .scope(MAX_WORKFLOW_DEPTH - 1, async {
            run_workflow(
                Arc::new(HarnessPool::new()),
                deps(dir.path()),
                &tools_record(),
                &file,
                Value::Null,
                &WorkflowRunContext::new(false),
            )
            .await
        })
        .await;
    assert!(out.is_ok(), "a run below the limit must execute: {out:?}");
}

// --- #395: a paused gate becomes a decidable approval --------------------

/// The graph T4 uses, with the gate node reachable and an output behind it.
pub(super) const GATED: &str = r#"
id = "gated"
name = "Gated"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "gate"
kind = "tool_call"
name = "Gate"
requires_approval = true
[node.config]
slug = "csv_export"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "gate"
[[edge]]
from = "gate"
to = "done"
"#;

/// Deps with a real approvals queue — the production gate over a `full`
/// policy and a real on-disk journal.
///
/// `full` is the mode that matters: it is the tier under which the manifest
/// gate's `evaluate` would *allow* most effects. Parking under it proves the
/// gate park is the already-decided path rather than a re-evaluation that
/// would quietly let the run continue.
pub(super) fn deps_with_parking(
    dir: &std::path::Path,
) -> (HarnessDeps, Arc<crate::runtime::journal::RuntimeJournal>) {
    let policy = toml::from_str("mode = \"full\"\n").expect("valid [policy] block");
    let gate = Arc::new(crate::policy::ManifestApprovalGate::new(policy));
    let journal = Arc::new(crate::runtime::journal::RuntimeJournal::new(
        dir.join("journal.jsonl"),
    ));
    let mut deps = deps(dir);
    deps.delivery = Some(super::super::delivery::WorkflowDeliveryDeps {
        mail: None,
        inbox: Arc::new(crate::store::FsInboxStore::new(dir)),
        users: Arc::new(crate::store::FsOps::new(dir)),
        bootstrap_admin: None,
        channels: Vec::new(),
        notifications: None,
        parking: Some(super::super::delivery::DeliveryParking {
            approvals: gate,
            journal: journal.clone(),
            // Issue #978: a test fixture parks into its own queues. The
            // production wiring is `RuntimeBuilder`, which hands the
            // runtime's own handles in so a park arms what the resolve
            // path releases.
            continuations: Default::default(),
            gates: Default::default(),
            blocked_nodes: Default::default(),
            grants: Default::default(),
            events: Arc::new(crate::store::FsEventLog::new(dir)),
        }),
        events: Arc::new(crate::store::FsEventLog::new(dir)),
    });
    (deps, journal)
}

/// The headline regression. A run that pauses on `requires_approval` must
/// leave a **parked effect** behind, not just an id on a response body —
/// the Approvals page reads the journal, so before #395 it stayed empty
/// however many gates a run paused on.
#[tokio::test]
async fn a_paused_gate_becomes_a_parked_approval() {
    let dir = tempfile::tempdir().unwrap();
    let (deps, journal) = deps_with_parking(dir.path());
    let file = parse_workflow(GATED).expect("parses");

    let run = run_workflow(
        Arc::new(HarnessPool::new()),
        deps,
        &tools_record(),
        &file,
        serde_json::json!({ "request": "quarterly numbers" }),
        &WorkflowRunContext::new(false),
    )
    .await
    .expect("run pauses cleanly");
    assert!(run.pending_approvals.iter().any(|id| id == "gate"));

    let pending = journal.pending();
    let card = pending
        .iter()
        .find(|p| p.effect.kind == crate::runtime::WORKFLOW_APPROVE_KIND)
        .expect("the paused gate is waiting on the operator");
    assert_eq!(card.effect.payload["workflow_id"], "gated");
    assert_eq!(card.effect.payload["node_id"], "gate");
    // Self-contained: the trigger input rides the card, which is what makes
    // approve-after-restart resume without any live state.
    assert_eq!(card.effect.payload["input"]["request"], "quarterly numbers");
    // Native — no teammate asked, so approving must not mint a tool grant.
    assert!(card.effect.agent.is_none());
    // The run that paused, so the console can tie the card to the history.
    assert!(card.effect.run_id.is_some());
}

/// Re-running the same graph with the same input must not stack a second
/// card for one decision — that is how an approvals queue becomes something
/// an operator rubber-stamps.
#[tokio::test]
async fn re_reaching_the_same_gate_does_not_ask_twice() {
    let dir = tempfile::tempdir().unwrap();
    let (deps, journal) = deps_with_parking(dir.path());
    let file = parse_workflow(GATED).expect("parses");
    let input = serde_json::json!({ "request": "same" });

    for _ in 0..3 {
        run_workflow(
            Arc::new(HarnessPool::new()),
            deps.clone(),
            &tools_record(),
            &file,
            input.clone(),
            &WorkflowRunContext::new(false),
        )
        .await
        .expect("run pauses cleanly");
    }

    let gates = journal
        .pending()
        .into_iter()
        .filter(|p| p.effect.kind == crate::runtime::WORKFLOW_APPROVE_KIND)
        .count();
    assert_eq!(gates, 1, "one gate, one decision, one card");
}

/// …but a **different** input at the same gate is a genuinely different
/// decision and must be asked about separately.
#[tokio::test]
async fn the_same_gate_on_a_different_input_is_a_second_decision() {
    let dir = tempfile::tempdir().unwrap();
    let (deps, journal) = deps_with_parking(dir.path());
    let file = parse_workflow(GATED).expect("parses");

    for request in ["first", "second"] {
        run_workflow(
            Arc::new(HarnessPool::new()),
            deps.clone(),
            &tools_record(),
            &file,
            serde_json::json!({ "request": request }),
            &WorkflowRunContext::new(false),
        )
        .await
        .expect("run pauses cleanly");
    }

    let gates = journal
        .pending()
        .into_iter()
        .filter(|p| p.effect.kind == crate::runtime::WORKFLOW_APPROVE_KIND)
        .count();
    assert_eq!(gates, 2);
}

/// The graph the next test uses: three sibling `requires_approval` gates
/// fanning out from one trigger — the run-level analogue of
/// `parallel_gate_fanout_test`'s `FANOUT_TOML`, sized to exercise
/// `park_pending_gates`'s own loop directly rather than a full
/// `CompanyRuntime`.
pub(super) const THREE_GATES: &str = r#"
id = "three-gate"
name = "Three Gate"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "gate1"
kind = "tool_call"
name = "Gate1"
requires_approval = true
[node.config]
slug = "csv_export"
[[node]]
id = "gate2"
kind = "tool_call"
name = "Gate2"
requires_approval = true
[node.config]
slug = "csv_export"
[[node]]
id = "gate3"
kind = "tool_call"
name = "Gate3"
requires_approval = true
[node.config]
slug = "csv_export"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "gate1"
[[edge]]
from = "start"
to = "gate2"
[[edge]]
from = "start"
to = "gate3"
[[edge]]
from = "gate1"
to = "done"
[[edge]]
from = "gate2"
to = "done"
[[edge]]
from = "gate3"
to = "done"
"#;
