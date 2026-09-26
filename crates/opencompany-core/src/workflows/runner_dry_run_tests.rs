use super::tests_capped_halt::{GREET, deps, record, tools_record};
use super::tests_checkpoint_cancel::GatedProvider;
use super::tests_delivery_gate::REPORT_TO_DESK;
use super::tests_journal::{STALLS, deps_with_events, journaled};
use super::*;

use crate::company::parse_workflow;
use crate::harness::provider::MockProvider;
use crate::store::FsOps;

/// **The clean-cancel arm (issue #398).** The counterpart to the hard-abort
/// keystone: here the wedged node is *released* right after the stop, so the
/// agent node finishes, the engine reaches the next boundary, observes the
/// flipped token, and winds the run down cleanly — returning a real
/// `RunOutcome` with `cancelled` set instead of having its future dropped.
///
/// Three things distinguish this from the hard abort:
///
/// 1. the run still settles `cancelled` and still routes nothing;
/// 2. **the completed nodes ride the run response** — `run.nodes` carries the
/// 2. **the completed nodes ride the run response** — `run.nodes` carries the
///    trail, because a clean wind-down keeps the collected rows the dropped
///    future had to throw away; and
/// 3. the node past the stop point (`done`) never runs — the token halts the
///    graph at the boundary, so it is neither started nor finished.
#[tokio::test]
async fn a_cleanly_cancelled_run_winds_down_at_the_boundary_keeping_its_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(HarnessPool::new());
    let rec = record();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (mut deps, events) = deps_with_events(dir.path());
    deps.provider = Arc::new(GatedProvider {
        inner: MockProvider::new("mock: "),
        entered: entered.clone(),
        release: release.clone(),
    });
    deps.provider_slug = "gated".to_string();
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

    // Stop the run, THEN let the wedged node complete. The token is already
    // flipped by the time the agent finishes, so the engine winds down at the
    // boundary before `done` — well within the grace, so no hard abort.
    let pressed = std::time::Instant::now();
    cancel.cancel();
    release.notify_one();
    let run = tokio::time::timeout(std::time::Duration::from_secs(30), run)
        .await
        .expect("the cleanly cancelled run never returned")
        .expect("a cancelled run is Ok, not Err");
    let elapsed = pressed.elapsed();

    assert!(
        run.cancelled,
        "a clean node-boundary stop still reports cancelled"
    );
    assert!(
        run.deliveries.is_empty(),
        "a stopped run routes nothing, clean or not"
    );
    // Settled by winding down, NOT by the hard-abort fallback: it must come
    // back well inside the grace window.
    assert!(
        elapsed < CANCEL_HARD_ABORT_GRACE,
        "a clean wind-down took {elapsed:?} — it should settle before the grace, not fall back \
         to the hard abort"
    );

    // The clean arm carries the trail on the RESPONSE (the hard-abort arm
    // returns an empty one). `shape` and `ceo` finished; `done` is past the
    // stop boundary and never ran.
    let ran: Vec<&str> = run.nodes.iter().map(|n| n.node_id.as_str()).collect();
    assert!(ran.contains(&"shape"), "the transform completed: {ran:?}");
    assert!(
        ran.contains(&"ceo"),
        "the agent node finished before the wind-down: {ran:?}"
    );
    assert!(
        !ran.contains(&"done"),
        "the node past the stop boundary must never run: {ran:?}"
    );

    // The journal agrees, and every node that finished shows both brackets.
    let journal = journaled(&events, &rec.id).await;
    let finished: Vec<String> = journal
        .iter()
        .filter_map(|e| match e {
            CompanyEvent::WorkflowNodeFinished { node_id, .. } => Some(node_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(finished, vec!["shape".to_string(), "ceo".to_string()]);
    assert!(
        !journal.iter().any(
            |e| matches!(e, CompanyEvent::WorkflowNodeStarted { node_id, .. } if node_id == "done")
        ),
        "the halted node must not even open a started bracket"
    );
}

// ── Issue #542: dry run / test mode ─────────────────────────────────────

/// A run context flagged dry, built off the unregistered constructor.
fn dry_ctx() -> WorkflowRunContext {
    let mut ctx = WorkflowRunContext::new(false);
    ctx.dry_run = true;
    ctx
}

/// A [`MockProvider`] wrapper that counts every inference `invoke`, so a
/// test can prove a dry agent node makes **zero** of them (T2).
#[derive(Clone)]
struct CountingProvider {
    inner: MockProvider,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl tinyinference::model::ChatModel<()> for CountingProvider {
    async fn invoke(
        &self,
        state: &(),
        request: tinyinference::model::ModelRequest,
    ) -> tinyinference::Result<tinyinference::model::ModelResponse> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        tinyinference::model::ChatModel::invoke(&self.inner, state, request).await
    }
}

impl crate::harness::provider::HarnessModel for CountingProvider {
    fn telemetry_provider_id(&self) -> String {
        "counting-mock".to_string()
    }
}

/// A two-way branching graph: a `switch` on `kind` routes to one of two
/// output arms. Only the matched arm's node finishes.
const BRANCH: &str = r#"
id = "branch"
name = "Branch"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "route"
kind = "switch"
name = "Route"
[node.config]
field = "kind"
[[node]]
id = "paid_out"
kind = "output"
name = "Paid"
[[node]]
id = "free_out"
kind = "output"
name = "Free"
[[edge]]
from = "start"
to = "route"
[[edge]]
from = "route"
to = "paid_out"
label = "paid"
[[edge]]
from = "route"
to = "free_out"
label = "free"
"#;

/// T1 — a dry run walks the REAL graph with real branch selection: the taken
/// arm's node appears in the per-node trail, the untaken one never does.
#[tokio::test]
async fn t1_dry_run_reports_only_the_taken_branch_in_its_node_trail() {
    let dir = tempfile::tempdir().unwrap();
    let file = parse_workflow(BRANCH).expect("parses");
    let run = run_workflow(
        Arc::new(HarnessPool::new()),
        deps(dir.path()),
        &tools_record(),
        &file,
        serde_json::json!({ "kind": "paid" }),
        &dry_ctx(),
    )
    .await
    .expect("dry run completes");

    let ran: Vec<&str> = run.nodes.iter().map(|n| n.node_id.as_str()).collect();
    assert!(
        ran.contains(&"paid_out"),
        "the taken branch should be in the trail: {ran:?}"
    );
    assert!(
        !ran.contains(&"free_out"),
        "the untaken branch must never appear: {ran:?}"
    );
}

/// T2 — a dry agent node makes ZERO inference calls; the live control makes
/// at least one. The flag alone separates the two.
#[tokio::test]
async fn t2_dry_agent_node_makes_no_inference_calls() {
    let dir = tempfile::tempdir().unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut deps = deps(dir.path());
    deps.provider = Arc::new(CountingProvider {
        inner: MockProvider::new("mock: "),
        calls: calls.clone(),
    });
    let pool = Arc::new(HarnessPool::new());
    let rec = record();
    pool.ensure(&rec, &deps).await.expect("roster builds");
    let file = parse_workflow(GREET).expect("parses");

    // Live control: the agent node runs a real turn, so the counter moves.
    run_workflow(
        pool.clone(),
        deps.clone(),
        &rec,
        &file,
        serde_json::json!({}),
        &WorkflowRunContext::new(false),
    )
    .await
    .expect("live run completes");
    let after_live = calls.load(std::sync::atomic::Ordering::SeqCst);
    assert!(after_live > 0, "the live control must invoke inference");

    // Dry run: the stub agent echoes, so the counter does NOT move.
    let dry = run_workflow(pool, deps, &rec, &file, serde_json::json!({}), &dry_ctx())
        .await
        .expect("dry run completes");
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        after_live,
        "a dry agent node must make no inference calls"
    );
    // …and its output carries the dry marker rather than a real reply.
    assert!(
        dry.output.to_string().contains("[dry run]"),
        "dry output should echo the stub fixture: {}",
        dry.output
    );
}

/// T3 — a dry `tool_call` executes NOTHING: the CSV the live tool would write
/// never appears, and no per-run workspace is even created.
#[tokio::test]
async fn t3_dry_tool_call_writes_nothing_to_disk() {
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
data = "[{\"name\":\"Ada\"}]"
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
        &dry_ctx(),
    )
    .await
    .expect("dry run completes");
    assert!(run.pending_approvals.is_empty());
    // A dry run creates no workspace at all — so the tool could not have run.
    assert!(
        !dir.path().join("acme").join("_workflow").exists(),
        "a dry run must not create a per-run workspace"
    );
}

/// T4 — a dry run delivers NOTHING and journals NOTHING: the recording
/// channel gets zero dispatches, the delivery row is `Skipped`/`DryRun`, and
/// the journal holds no Started / NodeFinished / Finished / ReportDelivered
/// for the run.
#[tokio::test]
async fn t4_dry_run_delivers_nothing_and_journals_nothing() {
    use crate::runtime::channel::RecordingChannel;

    let dir = tempfile::tempdir().unwrap();
    let channel = RecordingChannel::new("engineering");
    let events: Arc<dyn crate::ports::EventLog> =
        Arc::new(crate::store::FsEventLog::new(dir.path()));
    let mut deps = deps(dir.path());
    deps.events = Some(events.clone());
    deps.delivery = Some(crate::workflows::WorkflowDeliveryDeps {
        mail: None,
        inbox: Arc::new(crate::store::FsInboxStore::new(dir.path())),
        users: Arc::new(FsOps::new(dir.path())),
        bootstrap_admin: None,
        channels: vec![Arc::new(channel.clone())],
        notifications: None,
        parking: None,
        events: events.clone(),
    });

    let file = parse_workflow(REPORT_TO_DESK).expect("parses");
    let run = run_workflow(
        Arc::new(HarnessPool::new()),
        deps,
        &record(),
        &file,
        serde_json::json!({ "brief": "numbers" }),
        &dry_ctx(),
    )
    .await
    .expect("dry run completes");

    assert_eq!(channel.sent().len(), 0, "a dry run must post nothing");
    assert_eq!(run.deliveries.len(), 1, "{:?}", run.deliveries);
    assert_eq!(
        run.deliveries[0].status,
        crate::ports::DeliveryStatus::Skipped
    );
    assert_eq!(
        run.deliveries[0].reason,
        crate::ports::DeliveryReason::DryRun
    );

    let journal = journaled(&events, &record().id).await;
    assert!(
        !journal.iter().any(|e| matches!(
            e,
            CompanyEvent::WorkflowRunStarted { .. }
                | CompanyEvent::WorkflowNodeStarted { .. }
                | CompanyEvent::WorkflowNodeFinished { .. }
                | CompanyEvent::WorkflowRunFinished { .. }
                | CompanyEvent::WorkflowReportDelivered { .. }
        )),
        "a dry run must journal nothing: {journal:?}"
    );
}

/// T5 — the REQUIRED negative control: the SAME graph run with `dry_run =
/// false` DOES dispatch and DOES journal. The flag alone separates the two
/// behaviours.
#[tokio::test]
async fn t5_the_same_graph_run_for_real_dispatches_and_journals() {
    use crate::runtime::channel::RecordingChannel;

    let dir = tempfile::tempdir().unwrap();
    let channel = RecordingChannel::new("engineering");
    let events: Arc<dyn crate::ports::EventLog> =
        Arc::new(crate::store::FsEventLog::new(dir.path()));
    let mut deps = deps(dir.path());
    deps.events = Some(events.clone());
    deps.delivery = Some(crate::workflows::WorkflowDeliveryDeps {
        mail: None,
        inbox: Arc::new(crate::store::FsInboxStore::new(dir.path())),
        users: Arc::new(FsOps::new(dir.path())),
        bootstrap_admin: None,
        channels: vec![Arc::new(channel.clone())],
        notifications: None,
        parking: None,
        events: events.clone(),
    });

    let file = parse_workflow(REPORT_TO_DESK).expect("parses");
    let run = run_workflow(
        Arc::new(HarnessPool::new()),
        deps,
        &record(),
        &file,
        serde_json::json!({ "brief": "numbers" }),
        &WorkflowRunContext::new(false),
    )
    .await
    .expect("real run completes");

    assert_eq!(channel.sent().len(), 1, "a real run posts the report");
    assert_eq!(run.deliveries[0].status, crate::ports::DeliveryStatus::Sent);

    let journal = journaled(&events, &record().id).await;
    assert!(
        journal
            .iter()
            .any(|e| matches!(e, CompanyEvent::WorkflowRunStarted { .. })),
        "a real run journals its start: {journal:?}"
    );
    assert!(
        journal
            .iter()
            .any(|e| matches!(e, CompanyEvent::WorkflowReportDelivered { .. })),
        "a real run journals its delivery: {journal:?}"
    );
}

/// T6 — an ungranted `tool_call` refuses in a dry run EXACTLY as it does live
/// (the grant gate is pure and kept). `record()` grants no tools, so the
/// `code`-namespace `csv_export` is denied and, with `on_error` defaulting to
/// stop, the run fails with the same "not granted" error.
#[tokio::test]
async fn t6_ungranted_tool_refuses_identically_in_dry_mode() {
    let src = r#"
id = "t6"
name = "T6"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "call"
kind = "tool_call"
name = "Call"
[node.config]
slug = "csv_export"
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
    // A company that grants `web` but NOT `code`, so the `code`-namespace
    // `csv_export` is refused — the same gate the live invoker applies.
    let web_only: CompanyRecord = {
        let mut rec = tools_record();
        rec.manifest.tools.allow = vec!["web.*".to_string()];
        rec
    };
    let dir = tempfile::tempdir().unwrap();
    let file = parse_workflow(src).expect("parses");
    let err = run_workflow(
        Arc::new(HarnessPool::new()),
        deps(dir.path()),
        &web_only, // grants web, not code
        &file,
        serde_json::json!({ "seed": 1 }),
        &dry_ctx(),
    )
    .await
    .expect_err("an ungranted tool must fail the dry run too");
    assert!(
        err.to_string().contains("not granted"),
        "the dry grant gate must refuse identically: {err}"
    );
}

/// T7 — a dry run reports the gate on `pending_approvals` but parks NOTHING
/// durable: the journal stays empty (park_pending_gates is skipped).
#[tokio::test]
async fn t7_dry_gate_reports_pending_but_parks_nothing() {
    let src = r#"
id = "t7"
name = "T7"
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
    let dir = tempfile::tempdir().unwrap();
    let (deps, events) = deps_with_events(dir.path());
    let file = parse_workflow(src).expect("parses");
    let run = run_workflow(
        Arc::new(HarnessPool::new()),
        deps,
        &tools_record(),
        &file,
        serde_json::json!({ "seed": 1 }),
        &dry_ctx(),
    )
    .await
    .expect("dry run pauses cleanly");
    assert!(
        run.pending_approvals.iter().any(|id| id == "gate"),
        "the gate should be reported pending: {:?}",
        run.pending_approvals
    );
    let journal = journaled(&events, &tools_record().id).await;
    assert!(
        journal.is_empty(),
        "a dry run parks nothing and journals nothing: {journal:?}"
    );
}

/// T8 (issue #382) — a dry run emits **no `WorkflowNodeStarted`** either, for
/// the same reason it emits no finish: the started event is journaling-gated
/// (`events` wired AND not dry), not observer-gated. The observer still fires
/// — the per-node trail on the RESPONSE proves the nodes ran — but nothing
/// durable is written. The negative control that a real run of the same graph
/// DOES journal starts is `a_run_journals_a_start_then_a_started_finished_pair…`.
#[tokio::test]
async fn t8_dry_run_collects_the_node_trail_but_journals_no_node_started() {
    let dir = tempfile::tempdir().unwrap();
    let (deps, events) = deps_with_events(dir.path());
    let file = parse_workflow(GREET).expect("parses");
    let run = run_workflow(
        Arc::new(HarnessPool::new()),
        deps,
        &record(),
        &file,
        serde_json::json!({}),
        &dry_ctx(),
    )
    .await
    .expect("dry run completes");

    // The observer ran — the response carries the trail even for a dry run.
    let ran: Vec<&str> = run.nodes.iter().map(|n| n.node_id.as_str()).collect();
    assert!(ran.contains(&"ceo") && ran.contains(&"done"), "{ran:?}");

    // …but nothing durable, node-started included.
    let journal = journaled(&events, &record().id).await;
    assert!(
        !journal
            .iter()
            .any(|e| matches!(e, CompanyEvent::WorkflowNodeStarted { .. })),
        "a dry run must journal no node-started event: {journal:?}"
    );
    assert!(
        journal.is_empty(),
        "a dry run journals nothing at all: {journal:?}"
    );
}

// --- #1825 (P1, found by chatgpt-codex-connector): arm every blocked
// node before awaiting journal I/O ----------------------------------

/// A [`JournalStore`] whose `append_journal` parks the caller mid-await the
/// first time a line matches `match_substr`, after signalling `reached` —
/// so a test can inspect state from a second task while the first is
/// genuinely suspended inside the write, not merely about to make it.
/// [`release`](Self::release) lets the parked append through; every append
/// after that — including a second match — passes straight through so
/// nothing deadlocks the loop under test.
pub(super) struct GatedJournalStore {
    inner: crate::ports::journal::MemoryJournalStore,
    match_substr: &'static str,
    armed: std::sync::atomic::AtomicBool,
    pub(super) reached: tokio::sync::Notify,
    pub(super) release: tokio::sync::Notify,
}

impl GatedJournalStore {
    pub(super) fn new(match_substr: &'static str) -> Self {
        Self {
            inner: Default::default(),
            match_substr,
            armed: std::sync::atomic::AtomicBool::new(true),
            reached: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        }
    }
}

#[async_trait]
impl crate::ports::JournalStore for GatedJournalStore {
    async fn append_journal(
        &self,
        id: &CompanyId,
        line: &str,
        durability: crate::ports::Durability,
    ) -> Result<()> {
        // CodeRabbit nitpick (review 5038258829): check the substring
        // before disarming. `swap` first meant any append that reached
        // this store ahead of the `match_substr` line consumed the armed
        // flag on a non-match, so the real target line would never gate —
        // today's tests only pass because nothing writes here before it,
        // a property this double shares with nothing that enforces it.
        if line.contains(self.match_substr)
            && self.armed.swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.reached.notify_one();
            self.release.notified().await;
        }
        self.inner.append_journal(id, line, durability).await
    }

    async fn read_journal(&self, id: &CompanyId) -> Result<Vec<String>> {
        self.inner.read_journal(id).await
    }

    async fn journal_imported(&self, id: &CompanyId) -> Result<bool> {
        self.inner.journal_imported(id).await
    }

    async fn complete_import(&self, id: &CompanyId, lines: Vec<String>) -> Result<()> {
        self.inner.complete_import(id, lines).await
    }
}
