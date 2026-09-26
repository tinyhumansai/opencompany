use super::tests_capped_halt::{GREET, deps, record, tools_record, workflow_workspace};
use super::tests_reclassify::RecordingLane;
use super::*;

use crate::company::parse_workflow;
use crate::ports::run_output::WorkflowRunOutputStore;
use crate::store::FsOps;

/// The guard's whole reason to exist: a genuinely broken node alongside a
/// blocked one must still fail the check, so the real error is not hidden.
#[test]
fn a_genuinely_errored_node_alongside_a_blocked_one_fails_the_guard() {
    let blocked = vec![crate::ports::WorkflowBlockedNode {
        node_id: "work".to_string(),
        tools: vec!["shell".to_string()],
        approval_ids: vec!["appr-1".to_string()],
        unparkable: 0,
        stranded: 0,
        blockers: 0,
    }];
    let nodes = vec![
        crate::ports::WorkflowRunNodeRow {
            node_id: "work".to_string(),
            status: WorkflowNodeStatus::Error,
            elapsed_ms: 10,
            diagnostics: Vec::new(),
        },
        crate::ports::WorkflowRunNodeRow {
            node_id: "other".to_string(),
            status: WorkflowNodeStatus::Error,
            elapsed_ms: 5,
            diagnostics: Vec::new(),
        },
    ];
    assert!(
        !only_blocked_nodes_errored(&nodes, &blocked),
        "a genuinely broken node must not be masked by an unrelated block"
    );
}

/// A dry run writes NOTHING durable — no output snapshot, matching its "the
/// settled response body is the whole record" contract (#542).
#[tokio::test]
async fn a_dry_run_persists_no_output() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FsOps::new(dir.path()));
    let pool = Arc::new(HarnessPool::new());
    let rec = record();
    let mut deps = deps(dir.path());
    deps.run_output_store = Some(store.clone());
    pool.ensure(&rec, &deps).await.expect("roster builds");
    let file = parse_workflow(GREET).expect("parses");
    let mut ctx = WorkflowRunContext::new(false);
    ctx.dry_run = true;
    let run_id = ctx.run_id.clone();

    run_workflow(
        pool,
        deps,
        &rec,
        &file,
        serde_json::json!({ "brief": "launch" }),
        &ctx,
    )
    .await
    .expect("dry run completes");

    assert!(
        store
            .get_run_output(&rec.id, &run_id)
            .await
            .unwrap()
            .is_none(),
        "a dry run must persist nothing durable"
    );
}
/// Issue #1008 (the decisive change): a run whose first node SUCCEEDS and a
/// later node FAILS hard (default `on_error = "stop"`, so the engine returns
/// `Err`) must STILL persist the per-node output the observer captured before
/// the failure — flagged `partial`. Before #1008 this arm returned `Err`
/// without persisting anything, so the inspector wrongly claimed the run
/// predated output capture.
///
/// `start → export (csv_export, succeeds) → boom (bogus_tool, unknown slug →
/// hard stop)`: the export node's frame is captured off the progress
/// observer, and the run fails on `boom`.
#[tokio::test]
async fn a_failed_run_persists_the_partial_output_it_reached() {
    let src = r#"
id = "partial_fail"
name = "Partial fail"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "export"
kind = "tool_call"
name = "Export"
[node.config]
slug = "csv_export"
[node.config.args]
filename = "wf-out.csv"
data = "[{\"name\":\"Ada\"},{\"name\":\"Bob\"}]"
[[node]]
id = "boom"
kind = "tool_call"
name = "Boom"
[node.config]
slug = "bogus_tool"
[[edge]]
from = "start"
to = "export"
[[edge]]
from = "export"
to = "boom"
"#;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FsOps::new(dir.path()));
    let mut deps = deps(dir.path());
    deps.run_output_store = Some(store.clone());
    let rec = tools_record();
    let file = parse_workflow(src).expect("parses");
    let ctx = WorkflowRunContext::new(false);
    let run_id = ctx.run_id.clone();

    let err = run_workflow(
        Arc::new(HarnessPool::new()),
        deps,
        &rec,
        &file,
        serde_json::json!({ "seed": 1 }),
        &ctx,
    )
    .await
    .expect_err("the run fails hard on the unknown-slug node");
    assert!(
        err.to_string().contains("bogus_tool") || err.to_string().contains("boom"),
        "the failure should come from the failing node: {err}"
    );

    // The decisive assertion: the snapshot exists (was `None` pre-#1008,
    // because the `Err` arm persisted nothing).
    let stored = store
        .get_run_output(&rec.id, &run_id)
        .await
        .expect("store read")
        .expect("a failed run must still persist the output it reached before failing");
    assert!(
        stored.partial,
        "a failure-arm capture must be flagged partial: {stored:?}"
    );
    assert_eq!(stored.workflow_id, "partial_fail");
    assert_eq!(stored.run_id, run_id);
    // The successful `export` node's output is present under the canonical
    // `{ items: [...] }` shape the console renders.
    assert!(
        stored
            .nodes
            .get("export")
            .and_then(|n| n.get("items"))
            .is_some(),
        "the node that succeeded before the failure must be in the partial snapshot: {}",
        stored.nodes
    );

    // Issue #1008 (second half): the failure also carries the partial run up
    // to the caller, so `record_run_finished` can journal what the run did
    // before it broke instead of an all-empty row.
    let partial = err
        .partial_run()
        .expect("a run that broke mid-graph reports what it had already done");
    assert!(
        partial.nodes.iter().any(|n| n.node_id == "export"),
        "{:?}",
        partial.nodes
    );
    assert!(
        !partial.cancelled,
        "a failure is not an operator stop, whatever else it carries"
    );
    assert!(
        partial.output.get("export").is_some(),
        "the partial run reports the same capture that was persisted: {}",
        partial.output
    );
}

/// Issue #1008: `without_nodes` removes exactly the blocked nodes' entries
/// and leaves everything else — including a capture of another shape —
/// untouched.
///
/// Unit-level beside the end-to-end proof in `blocked_node_tests`, because
/// the "leaves a value it does not recognise alone" half has no reachable
/// path through a real run and would otherwise be an untested branch.
#[test]
fn without_nodes_removes_only_the_blocked_entries() {
    let blocked = |id: &str| crate::ports::WorkflowBlockedNode {
        node_id: id.to_string(),
        tools: vec!["shell".to_string()],
        approval_ids: Vec::new(),
        unparkable: 0,
        stranded: 0,
        blockers: 0,
    };

    let capture = serde_json::json!({
        "writer": { "items": ["draft"] },
        "publish": { "items": ["apology prose"] },
    });
    let narrowed = without_nodes(capture, &[blocked("publish")]);
    assert!(narrowed.get("writer").is_some(), "{narrowed}");
    assert!(
        narrowed.get("publish").is_none(),
        "the blocked node produced nothing, so it must have no entry: {narrowed}"
    );

    // Nothing blocked: returned verbatim rather than rebuilt.
    let untouched = serde_json::json!({ "writer": { "items": ["draft"] } });
    assert_eq!(
        without_nodes(untouched.clone(), &[]),
        untouched,
        "a run that blocked on nobody is byte-unchanged"
    );

    // A value of another shape is not a map to narrow — returned as-is
    // rather than replaced with an invented one.
    assert_eq!(
        without_nodes(Value::Null, &[blocked("publish")]),
        Value::Null
    );
}

// --- Output destinations, end to end (issue #170) ------------------------

/// A graph whose terminal `output` node routes its report to a desk
/// channel. `trigger → output` only, so it needs no roster.
pub(super) const REPORT_TO_DESK: &str = r#"
id = "report"
name = "Report"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "done"
kind = "output"
name = "Owner summary"
[node.destination]
kind = "channel"
target = "engineering"
[[edge]]
from = "start"
to = "done"
"#;

/// The end-to-end proof that the RUNNER (not the HTTP handler) delivers: a
/// run driven straight through `run_workflow` with a wired delivery bundle
/// posts the report and reports the send on the run result. The
/// orchestrator's `run_workflow` tool and the trigger scheduler reach this
/// same function, which is why delivery lives here.
#[tokio::test]
async fn a_run_delivers_its_output_report_through_the_runner() {
    use crate::runtime::channel::RecordingChannel;

    let dir = tempfile::tempdir().unwrap();
    let channel = RecordingChannel::new("engineering");
    let mut deps = deps(dir.path());
    deps.delivery = Some(crate::workflows::WorkflowDeliveryDeps {
        mail: None,
        inbox: Arc::new(crate::store::FsInboxStore::new(dir.path())),
        users: Arc::new(FsOps::new(dir.path())),
        bootstrap_admin: None,
        channels: vec![Arc::new(channel.clone())],
        notifications: None,
        // This case delivers to a channel, which never parks.
        parking: None,
        events: Arc::new(crate::store::FsEventLog::new(dir.path())),
    });

    let file = parse_workflow(REPORT_TO_DESK).expect("parses");
    let run = run_workflow(
        Arc::new(HarnessPool::new()),
        deps,
        &record(),
        &file,
        serde_json::json!({ "brief": "quarterly numbers" }),
        &WorkflowRunContext::new(false),
    )
    .await
    .expect("workflow runs");

    assert_eq!(run.deliveries.len(), 1, "{:?}", run.deliveries);
    assert_eq!(
        run.deliveries[0].status,
        crate::ports::DeliveryStatus::Sent,
        "{:?}",
        run.deliveries
    );
    assert_eq!(run.deliveries[0].node, "done");
    assert_eq!(
        channel.sent().len(),
        1,
        "the report should have been posted"
    );
}

/// The #169 lesson, at the run level: with no delivery ports wired the run
/// still SUCCEEDS (its work is valid) but the result carries a loud `failed`
/// row — an operator can tell a working destination from a broken one
/// without reading a log. Every other `deps()` in this suite is unwired, so
/// this is the default-build shape.
#[tokio::test]
async fn an_unwired_runtime_still_runs_but_says_the_report_was_not_sent() {
    let dir = tempfile::tempdir().unwrap();
    let file = parse_workflow(REPORT_TO_DESK).expect("parses");
    let run = run_workflow(
        Arc::new(HarnessPool::new()),
        deps(dir.path()),
        &record(),
        &file,
        serde_json::json!({}),
        &WorkflowRunContext::new(false),
    )
    .await
    .expect("an undeliverable report must not fail the run");

    assert_eq!(run.deliveries.len(), 1, "{:?}", run.deliveries);
    assert_eq!(
        run.deliveries[0].status,
        crate::ports::DeliveryStatus::Failed
    );
    assert!(
        run.deliveries[0].detail.contains("not wired"),
        "{:?}",
        run.deliveries
    );
}

/// The port implementation ensures the roster itself, so a caller need not
/// pre-`ensure`.
#[tokio::test]
async fn port_impl_ensures_roster_and_runs() {
    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(HarnessPool::new());
    let rec = record();
    let deps = deps(dir.path());
    let turn = Arc::new(crate::harness::built_in::run_turn::HarnessRunTurn::new(
        pool,
        Arc::new(deps.clone()),
    ));
    let runner = HarnessWorkflowRunner::new(turn, deps, rec.clone());

    let file = parse_workflow(GREET).expect("workflow parses");
    let run = WorkflowRunner::run(
        &runner,
        &rec.id,
        &file,
        serde_json::json!({}),
        &WorkflowRunContext::new(false),
    )
    .await
    .expect("workflow runs");
    assert!(run.output.to_string().contains("hello-marker"));
}

/// Issue #1455 — a workflow run classifies its tool-call gate under the
/// store's *current* policy, not the build-time snapshot. An operator who
/// moves a policy axis on the console and then starts an authored workflow
/// without rebuilding the runtime must have the run honour the new value.
#[tokio::test]
async fn workflow_run_gate_reads_live_policy_not_the_build_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let deps = deps(dir.path());
    let snapshot = record(); // build-time view: overlay_policy: None

    // The console wrote a policy override since the runtime was built:
    // every spend now parks (`auto_approve_under_usd` lowered to 0), a
    // change the run's gate must classify under.
    let live = CompanyRecord {
        overlay_policy: Some(crate::ports::PolicyOverride {
            mode: None,
            always_approve: None,
            auto_approve_under_usd: Some(Some(0.0)),
            approval_ttl_hours: None,
            set_by: crate::ports::Actor {
                kind: crate::ports::ActorKind::User,
                id: "console-operator".to_string(),
            },
            at_millis: 1_700_000_000_000,
        }),
        ..snapshot.clone()
    };
    deps.store
        .save(&live)
        .await
        .expect("store accepts the live record");

    let turn = Arc::new(crate::harness::built_in::run_turn::HarnessRunTurn::new(
        Arc::new(HarnessPool::new()),
        Arc::new(deps.clone()),
    ));
    let runner = HarnessWorkflowRunner::new(turn, deps, snapshot.clone());

    let effective = runner.effective_record().await.expect("effective record");
    assert_eq!(
        effective.overlay_policy, live.overlay_policy,
        "the run must gate against the store's policy, not the snapshot's"
    );
}

/// The workflow port keeps the lane-aware router intact: an agent bound to
/// a named harness must not fall back to the default engine.
#[tokio::test]
async fn port_impl_routes_an_agent_node_to_its_named_harness() {
    let dir = tempfile::tempdir().unwrap();
    let rec = record();
    let deps = deps(dir.path());
    let default = RecordingLane::new("default-lane");
    let deep = RecordingLane::new("deep-lane");
    let turn: Arc<dyn crate::runtime::delegation::RunTurn> = Arc::new(
        crate::harness::router::HarnessRouter::new("embedded")
            .with_engine("embedded", default.clone())
            .with_engine("deep", deep.clone())
            .bind("ceo", "deep"),
    );
    let runner = HarnessWorkflowRunner::new(turn, deps, rec.clone());

    let file = parse_workflow(GREET).expect("workflow parses");
    let run = WorkflowRunner::run(
        &runner,
        &rec.id,
        &file,
        serde_json::json!({}),
        &WorkflowRunContext::new(false),
    )
    .await
    .expect("workflow runs through the named lane");

    assert!(run.output.to_string().contains("deep-lane"));
    assert!(default.seen.lock().unwrap().is_empty());
    assert_eq!(&*deep.seen.lock().unwrap(), &["ceo".to_string()]);
}

/// A workflow with no trigger is a caller-facing bad request, not a harness
/// error. (Built by hand — `parse_workflow` would reject it earlier.)
#[tokio::test]
async fn missing_trigger_is_invalid_request() {
    use crate::company::{WorkflowFile, WorkflowNodeDef, WorkflowNodeKind};

    let dir = tempfile::tempdir().unwrap();
    let file = WorkflowFile {
        global: false,
        id: "bad".to_string(),
        name: "Bad".to_string(),
        description: None,
        owner_desk: None,
        nodes: vec![WorkflowNodeDef {
            id: "only".to_string(),
            kind: WorkflowNodeKind::Output,
            name: "Only".to_string(),
            summary: None,
            agent: None,
            schedule: None,
            config: None,
            on_error: None,
            retry: None,
            requires_approval: None,
            repeatable: None,
            destination: None,
            postcondition: None,
            verify: None,
        }],
        edges: Vec::new(),
    };
    let err = run_workflow(
        Arc::new(HarnessPool::new()),
        deps(dir.path()),
        &record(),
        &file,
        serde_json::json!({}),
        &WorkflowRunContext::new(false),
    )
    .await
    .expect_err("missing trigger rejected");
    assert!(
        matches!(err, OpenCompanyError::InvalidRequest(_)),
        "{err:?}"
    );
}

// --- P1: real capability wiring (T1–T5) --------------------------------

/// T1 — a config-driven `tool_call` (slug `csv_export`) executes through the
/// real Cell A toolbelt and the CSV lands on disk in the dedicated workflow
/// workspace (on-disk proof the tool actually ran).
#[tokio::test]
async fn t1_config_driven_tool_call_writes_csv_to_workflow_workspace() {
    let src = r#"
id = "csv"
name = "CSV"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "export"
kind = "tool_call"
name = "Export"
[node.config]
slug = "csv_export"
[node.config.args]
filename = "wf-out.csv"
data = "[{\"name\":\"Ada\"},{\"name\":\"Bob\"}]"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "export"
[[edge]]
from = "export"
to = "done"
"#;
    let dir = tempfile::tempdir().unwrap();
    let file = parse_workflow(src).expect("parses");
    let run = run_workflow(
        Arc::new(HarnessPool::new()),
        deps(dir.path()),
        &tools_record(),
        &file,
        serde_json::json!({ "seed": 1 }),
        &WorkflowRunContext::new(false),
    )
    .await
    .expect("workflow runs");
    assert!(run.pending_approvals.is_empty());

    let csv = workflow_workspace(dir.path(), "acme")
        .join("exports")
        .join("wf-out.csv");
    assert!(
        csv.is_file(),
        "csv_export should land the file in the workflow workspace: {}",
        csv.display()
    );
    let content = std::fs::read_to_string(&csv).unwrap();
    assert!(
        content.contains("Ada") && content.contains("Bob"),
        "{content}"
    );
}

/// T2 — an unknown slug with `retry.max_attempts = 2` and `on_error =
/// "continue"` exhausts its retries then turns the failure into a data item,
/// so the run completes (no hard error) carrying the error.
#[tokio::test]
async fn t2_unknown_slug_retries_then_continues_with_error_item() {
    let src = r#"
id = "t2"
name = "T2"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "call"
kind = "tool_call"
name = "Call"
on_error = "continue"
[node.config]
slug = "bogus_tool"
[node.retry]
max_attempts = 2
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "call"
[[edge]]
from = "call"
to = "done"
"#;
    let dir = tempfile::tempdir().unwrap();
    let file = parse_workflow(src).expect("parses");
    let run = run_workflow(
        Arc::new(HarnessPool::new()),
        deps(dir.path()),
        &tools_record(),
        &file,
        serde_json::json!({ "seed": 1 }),
        &WorkflowRunContext::new(false),
    )
    .await
    .expect("run completes despite the failing node");
    // `on_error = continue` turns the failure into a data item; the message
    // names the unwired slug.
    assert!(
        run.output.to_string().contains("bogus_tool"),
        "the continued error item should carry the failure: {}",
        run.output
    );
}
