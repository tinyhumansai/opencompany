use super::*;

#[test]
fn workflow_runner_handle_is_empty_until_filled() {
    let handle = WorkflowRunnerHandle::default();
    assert!(handle.get().is_none());
    let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::empty());
    handle.set(&runner);
    assert!(handle.get().is_some());
}

#[test]
fn workflow_runner_handle_holds_only_a_weak_reference() {
    // Proves the deps↔runner cell is not a strong cycle: once the sole strong
    // owner drops, the handle can no longer upgrade.
    let handle = WorkflowRunnerHandle::default();
    {
        let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::empty());
        handle.set(&runner);
        assert!(handle.get().is_some());
    }
    assert!(
        handle.get().is_none(),
        "the handle must not keep the runner alive"
    );
}

#[test]
fn orchestrator_tools_includes_all_sixteen() {
    use crate::harness::workflow_admin::{
        DELETE_WORKFLOW_TOOL, READ_WORKFLOW_TOOL, UPDATE_WORKFLOW_TOOL,
    };
    let queue = DelegationQueue::default();
    let tools = orchestrator_tools(
        CompanyId::new("acme"),
        None,
        None,
        None,
        None,
        None,
        &queue,
        None,
        WorkflowRunnerHandle::default(),
        crate::runtime::RunSupervisor::default(),
        Arc::new(MemStore::default()),
        None,
        WorkflowRefQueue::default(),
        RunOutputCache::default(),
        "ceo".to_string(),
        None,
        vec!["fs:*".to_string()],
        None,
    );
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    // Six before #186; `assign_task` + `review_task` made eight; #418's
    // `read_run_output` makes nine; #661's read/update/delete_workflow
    // trio makes twelve; #884's `delegate_to_teammate` makes thirteen;
    // #1859's `list_tasks` / `read_task` / `read_run` trio makes sixteen.
    assert_eq!(names.len(), 16, "got {names:?}");
    assert!(names.contains(&DELEGATE_TO_TEAMMATE_TOOL), "got {names:?}");
    assert!(names.contains(&RUN_WORKFLOW_TOOL), "got {names:?}");
    assert!(names.contains(&READ_RUN_OUTPUT_TOOL), "got {names:?}");
    assert!(names.contains(&CREATE_WORKFLOW_TOOL), "got {names:?}");
    assert!(names.contains(&READ_WORKFLOW_TOOL), "got {names:?}");
    assert!(names.contains(&UPDATE_WORKFLOW_TOOL), "got {names:?}");
    assert!(names.contains(&DELETE_WORKFLOW_TOOL), "got {names:?}");
    assert!(names.contains(&ADD_AGENT_TOOL), "got {names:?}");
    assert!(names.contains(&QUERY_COMPANY_TOOL), "got {names:?}");
    assert!(names.contains(&SPAWN_TASK_TOOL), "got {names:?}");
    assert!(names.contains(&DELEGATE_TO_DESK_TOOL), "got {names:?}");
    assert!(names.contains(&ASSIGN_TASK_TOOL), "got {names:?}");
    assert!(names.contains(&REVIEW_TASK_TOOL), "got {names:?}");
    assert!(names.contains(&LIST_TASKS_TOOL), "got {names:?}");
    assert!(names.contains(&READ_TASK_TOOL), "got {names:?}");
    assert!(names.contains(&READ_RUN_TOOL), "got {names:?}");
    // `read_run_output` sits immediately after `run_workflow`.
    let run_at = names.iter().position(|n| *n == RUN_WORKFLOW_TOOL).unwrap();
    assert_eq!(names[run_at + 1], READ_RUN_OUTPUT_TOOL, "got {names:?}");
    // The #661 trio sits immediately after `create_workflow`: they are its
    // lifecycle, and a model reads the belt in order.
    let created_at = names
        .iter()
        .position(|n| *n == CREATE_WORKFLOW_TOOL)
        .unwrap();
    assert_eq!(
        &names[created_at + 1..created_at + 4],
        &[
            READ_WORKFLOW_TOOL,
            UPDATE_WORKFLOW_TOOL,
            DELETE_WORKFLOW_TOOL
        ],
        "got {names:?}"
    );
    // #1859's read trio sits immediately after `query_company`: all four
    // answer "what does the company know?" rather than acting on it.
    let query_at = names.iter().position(|n| *n == QUERY_COMPANY_TOOL).unwrap();
    assert_eq!(
        &names[query_at + 1..query_at + 4],
        &[LIST_TASKS_TOOL, READ_TASK_TOOL, READ_RUN_TOOL],
        "got {names:?}"
    );
}

/// A runner panic is converted into an agent-visible error, and the RAII
/// supervisor slot is gone when the tool returns. This covers both cleanup
/// obligations without changing the runner architecture.
#[tokio::test]
async fn panicking_run_cleans_up_its_active_attempt() {
    struct PanickingRunner;

    #[async_trait::async_trait]
    impl WorkflowRunner for PanickingRunner {
        async fn run(
            &self,
            _company: &CompanyId,
            _workflow: &WorkflowFile,
            _input: Value,
            _ctx: &crate::ports::WorkflowRunContext,
        ) -> crate::Result<WorkflowRun> {
            panic!("test runner panic")
        }
    }

    let dir = tempfile::tempdir().unwrap();
    seed_demo_workflow(dir.path());
    let runner: Arc<dyn WorkflowRunner> = Arc::new(PanickingRunner);
    let handle = WorkflowRunnerHandle::default();
    handle.set(&runner);
    let supervisor = crate::runtime::RunSupervisor::default();
    let tool = RunWorkflowTool::new(
        CompanyId::new("acme"),
        Some(dir.path().to_path_buf()),
        Arc::new(MemStore::default()),
        handle,
        supervisor.clone(),
        None,
        WorkflowRefQueue::default(),
        RunOutputCache::default(),
        None,
    );

    let result = tool
        .execute(json!({"id": "demo"}))
        .await
        .expect("panic is converted to a tool result");
    assert!(result.is_error, "panic must be agent-visible: {result:?}");
    assert!(
        result.output_for_llm(false).contains("internal error"),
        "the result should not leak panic payload: {result:?}"
    );
    assert_eq!(supervisor.len(), 0, "the active attempt must be cleaned up");
}

#[tokio::test]
async fn run_workflow_tool_loads_and_invokes_the_runner() {
    let dir = tempfile::tempdir().unwrap();
    seed_demo_workflow(dir.path());

    let runner_impl = StubRunner::new(WorkflowRun {
        output: json!({
            "run": {},
            "nodes": { "worker": { "items": ["did the thing"] }, "done": { "items": [] } }
        }),
        pending_approvals: Vec::new(),
        deliveries: Vec::new(),
        cancelled: false,
        nodes: Vec::new(),
        notices: Vec::new(),
        board: Vec::new(),
        blocked_nodes: Vec::new(),
        approvals: Vec::new(),
    });
    let calls = runner_impl.calls.clone();
    let runner: Arc<dyn WorkflowRunner> = Arc::new(runner_impl);
    let handle = WorkflowRunnerHandle::default();
    handle.set(&runner);

    let tool = RunWorkflowTool::new(
        CompanyId::new("acme"),
        Some(dir.path().to_path_buf()),
        Arc::new(MemStore::default()),
        handle,
        crate::runtime::RunSupervisor::default(),
        None,
        WorkflowRefQueue::default(),
        RunOutputCache::default(),
        None,
    );
    let result = tool
        .execute(json!({ "id": "demo", "input": { "seed": 1 } }))
        .await
        .expect("execute");

    assert!(!result.is_error, "expected success, got {result:?}");
    assert_eq!(calls.lock().unwrap().as_slice(), ["demo"]);
    let out = result.output_for_llm(true);
    assert!(out.contains("Demo flow"), "{out}");
    assert!(out.contains("did the thing"), "{out}");
    assert!(out.contains("without pausing for approval"), "{out}");
}

/// Issue #339: a run this tool started is staged for the dispatched card
/// that started it, carrying the run id so the card's link can open the
/// overlay showing what actually executed.
#[tokio::test]
async fn a_successful_run_stages_a_workflow_reference_for_the_card() {
    let dir = tempfile::tempdir().unwrap();
    seed_demo_workflow(dir.path());

    let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::new(WorkflowRun {
        output: json!({ "nodes": { "worker": { "items": ["did the thing"] } } }),
        pending_approvals: Vec::new(),
        deliveries: Vec::new(),
        cancelled: false,
        nodes: Vec::new(),
        notices: Vec::new(),
        board: Vec::new(),
        blocked_nodes: Vec::new(),
        approvals: Vec::new(),
    }));
    let handle = WorkflowRunnerHandle::default();
    handle.set(&runner);
    let refs = WorkflowRefQueue::default();

    let tool = RunWorkflowTool::new(
        CompanyId::new("acme"),
        Some(dir.path().to_path_buf()),
        Arc::new(MemStore::default()),
        handle,
        crate::runtime::RunSupervisor::default(),
        None,
        refs.clone(),
        RunOutputCache::default(),
        None,
    );
    let result = tool
        .execute(json!({ "id": "demo" }))
        .await
        .expect("execute");
    assert!(!result.is_error, "{result:?}");

    let staged = refs.drain();
    assert_eq!(staged.len(), 1, "got {staged:?}");
    assert_eq!(staged[0].workflow_id, "demo");
    assert_eq!(staged[0].action, TaskOutputAction::Ran);
    assert!(
        staged[0].run_id.is_some(),
        "a run the card links to must name the run that happened"
    );
}

/// The other half, and the one worth pinning: a run that never happened
/// stages nothing. An unknown id, an unwired runner and a failed run all
/// produced no deliverable, so a card must not advertise one.
#[tokio::test]
async fn a_run_that_did_not_happen_stages_nothing() {
    let dir = tempfile::tempdir().unwrap();
    seed_demo_workflow(dir.path());
    let refs = WorkflowRefQueue::default();

    // No runner wired.
    let unwired = RunWorkflowTool::new(
        CompanyId::new("acme"),
        Some(dir.path().to_path_buf()),
        Arc::new(MemStore::default()),
        WorkflowRunnerHandle::default(),
        crate::runtime::RunSupervisor::default(),
        None,
        refs.clone(),
        RunOutputCache::default(),
        None,
    );
    assert!(
        unwired
            .execute(json!({ "id": "demo" }))
            .await
            .expect("execute")
            .is_error
    );

    // A wired runner, but an id neither source has.
    let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::empty());
    let handle = WorkflowRunnerHandle::default();
    handle.set(&runner);
    let unknown = RunWorkflowTool::new(
        CompanyId::new("acme"),
        Some(dir.path().to_path_buf()),
        Arc::new(MemStore::default()),
        handle,
        crate::runtime::RunSupervisor::default(),
        None,
        refs.clone(),
        RunOutputCache::default(),
        None,
    );
    assert!(
        unknown
            .execute(json!({ "id": "nope" }))
            .await
            .expect("execute")
            .is_error
    );

    assert_eq!(refs.queued(), 0, "nothing ran, so nothing may be linked");
}

/// A run an operator stopped is not a deliverable to put on a card. Its
/// partial steps stay in the run history either way, so nothing is lost —
/// what is avoided is a card advertising work somebody deliberately halted.
#[tokio::test]
async fn a_cancelled_run_stages_nothing() {
    let dir = tempfile::tempdir().unwrap();
    seed_demo_workflow(dir.path());

    let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::new(WorkflowRun {
        output: json!({ "nodes": {} }),
        pending_approvals: Vec::new(),
        deliveries: Vec::new(),
        cancelled: true,
        nodes: Vec::new(),
        notices: Vec::new(),
        board: Vec::new(),
        blocked_nodes: Vec::new(),
        approvals: Vec::new(),
    }));
    let handle = WorkflowRunnerHandle::default();
    handle.set(&runner);
    let refs = WorkflowRefQueue::default();

    let tool = RunWorkflowTool::new(
        CompanyId::new("acme"),
        Some(dir.path().to_path_buf()),
        Arc::new(MemStore::default()),
        handle,
        crate::runtime::RunSupervisor::default(),
        None,
        refs.clone(),
        RunOutputCache::default(),
        None,
    );
    let result = tool
        .execute(json!({ "id": "demo" }))
        .await
        .expect("execute");
    assert!(result.is_error, "a cancelled run reports as a stop");
    assert_eq!(refs.queued(), 0);
}

/// Issue #1861: an agent-initiated run that ends blocked badges the
/// operator, exactly as the console's and the scheduler's runs do.
///
/// This is the one trigger nobody is watching a progress bar for, so the
/// badge is the only way a run that stopped waiting on a person becomes
/// visible without somebody thinking to open the run history.
#[tokio::test]
async fn a_blocked_agent_run_badges_the_operator() {
    let dir = tempfile::tempdir().unwrap();
    seed_demo_workflow(dir.path());

    let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::new(WorkflowRun {
        output: json!({ "nodes": {} }),
        pending_approvals: vec!["worker".to_string()],
        deliveries: Vec::new(),
        cancelled: false,
        nodes: Vec::new(),
        notices: Vec::new(),
        board: Vec::new(),
        blocked_nodes: vec![crate::ports::workflow_runner::WorkflowBlockedNode {
            node_id: "worker".to_string(),
            tools: vec!["send_email".to_string()],
            approval_ids: vec!["ap-1".to_string()],
            unparkable: 0,
            stranded: 0,
            blockers: 0,
        }],
        approvals: Vec::new(),
    }));
    let handle = WorkflowRunnerHandle::default();
    handle.set(&runner);

    let notifications: Arc<dyn crate::ports::notifications::NotificationStore> =
        Arc::new(crate::store::FsOps::new(dir.path().to_path_buf()));
    let tool = RunWorkflowTool::new(
        CompanyId::new("acme"),
        Some(dir.path().to_path_buf()),
        Arc::new(MemStore::default()),
        handle,
        crate::runtime::RunSupervisor::default(),
        None,
        WorkflowRefQueue::default(),
        RunOutputCache::default(),
        Some(notifications.clone()),
    );
    tool.execute(json!({ "id": "demo" }))
        .await
        .expect("execute");

    let feed = notifications
        .list(&CompanyId::new("acme"), "ceo")
        .await
        .expect("list");
    assert_eq!(feed.len(), 1, "one badge for one unhealthy run: {feed:?}");
    assert_eq!(feed[0].notification.kind, "workflow_run_blocked");
}

/// The same contract for the other unhealthy end: the run could not park
/// the approval at all, so nobody was asked and nothing is waiting.
#[tokio::test]
async fn a_stranded_agent_run_badges_the_operator() {
    let dir = tempfile::tempdir().unwrap();
    seed_demo_workflow(dir.path());

    let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::new(WorkflowRun {
        output: json!({ "nodes": {} }),
        // Stranded is counted per pending *node* (`stranded_approvals`),
        // not per gated call: the node is pending, and every approval row
        // it owns failed to park, so there is nothing an operator can be
        // asked about. A fixture with no pending node at all is not a
        // stranded run under that reconciliation — it is an empty one.
        pending_approvals: vec!["worker".to_string()],
        deliveries: Vec::new(),
        cancelled: false,
        nodes: Vec::new(),
        notices: Vec::new(),
        board: Vec::new(),
        blocked_nodes: Vec::new(),
        approvals: vec![crate::ports::workflow_runner::WorkflowRunApprovalRow {
            node_id: Some("worker".to_string()),
            tool: Some("send_email".to_string()),
            outcome: crate::ports::workflow_runner::WorkflowApprovalOutcome::ParkFailed,
            approval_id: None,
        }],
    }));
    let handle = WorkflowRunnerHandle::default();
    handle.set(&runner);

    let notifications: Arc<dyn crate::ports::notifications::NotificationStore> =
        Arc::new(crate::store::FsOps::new(dir.path().to_path_buf()));
    let tool = RunWorkflowTool::new(
        CompanyId::new("acme"),
        Some(dir.path().to_path_buf()),
        Arc::new(MemStore::default()),
        handle,
        crate::runtime::RunSupervisor::default(),
        None,
        WorkflowRefQueue::default(),
        RunOutputCache::default(),
        Some(notifications.clone()),
    );
    tool.execute(json!({ "id": "demo" }))
        .await
        .expect("execute");

    let feed = notifications
        .list(&CompanyId::new("acme"), "ceo")
        .await
        .expect("list");
    assert_eq!(feed.len(), 1, "{feed:?}");
    assert_eq!(feed[0].notification.kind, "workflow_run_stranded");
}

/// A run that finished cleanly badges nobody. The badge means "this needs
/// you"; one per successful run would train the operator to ignore it.
#[tokio::test]
async fn a_healthy_agent_run_badges_nobody() {
    let dir = tempfile::tempdir().unwrap();
    seed_demo_workflow(dir.path());

    let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::new(WorkflowRun {
        output: json!({ "nodes": { "worker": { "items": [] } } }),
        pending_approvals: Vec::new(),
        deliveries: Vec::new(),
        cancelled: false,
        nodes: Vec::new(),
        notices: Vec::new(),
        board: Vec::new(),
        blocked_nodes: Vec::new(),
        approvals: Vec::new(),
    }));
    let handle = WorkflowRunnerHandle::default();
    handle.set(&runner);

    let notifications: Arc<dyn crate::ports::notifications::NotificationStore> =
        Arc::new(crate::store::FsOps::new(dir.path().to_path_buf()));
    let tool = RunWorkflowTool::new(
        CompanyId::new("acme"),
        Some(dir.path().to_path_buf()),
        Arc::new(MemStore::default()),
        handle,
        crate::runtime::RunSupervisor::default(),
        None,
        WorkflowRefQueue::default(),
        RunOutputCache::default(),
        Some(notifications.clone()),
    );
    tool.execute(json!({ "id": "demo" }))
        .await
        .expect("execute");

    let feed = notifications
        .list(&CompanyId::new("acme"), "ceo")
        .await
        .expect("list");
    assert!(feed.is_empty(), "{feed:?}");
}

#[tokio::test]
async fn run_workflow_tool_surfaces_pending_approvals() {
    let dir = tempfile::tempdir().unwrap();
    seed_demo_workflow(dir.path());

    let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::new(WorkflowRun {
        output: json!({ "nodes": { "worker": { "items": [] } } }),
        pending_approvals: vec!["worker".to_string()],
        deliveries: Vec::new(),
        cancelled: false,
        nodes: Vec::new(),
        notices: Vec::new(),
        board: Vec::new(),
        blocked_nodes: Vec::new(),
        approvals: Vec::new(),
    }));
    let handle = WorkflowRunnerHandle::default();
    handle.set(&runner);

    let tool = RunWorkflowTool::new(
        CompanyId::new("acme"),
        Some(dir.path().to_path_buf()),
        Arc::new(MemStore::default()),
        handle,
        crate::runtime::RunSupervisor::default(),
        None,
        WorkflowRefQueue::default(),
        RunOutputCache::default(),
        None,
    );
    let result = tool
        .execute(json!({ "id": "demo" }))
        .await
        .expect("execute");
    assert!(!result.is_error);
    let out = result.output_for_llm(true);
    assert!(out.contains("Paused for approval"), "{out}");
    assert!(out.contains("worker"), "{out}");
}

/// Issue #900 (tinysweeper `missing-test`): `summarize_run`'s blocked branch
/// had no coverage at all, and the doc comment on `blocked` / `paused`
/// (issue #881) — that a blocked node and a paused gate need separate
/// sentences even though both ride `pending_approvals` — was untested along
/// with it. One node blocks, a second is an ordinary paused gate: the
/// summary must name the blocked node under "Blocked, waiting on a person"
/// (never under "Paused for approval", which would tell the agent the run
/// resumes on its own) and the paused node under "Paused for approval"
/// only. The structural JSON counts (issue #881) must agree.
#[tokio::test]
async fn run_workflow_tool_separates_blocked_nodes_from_paused_gates() {
    let dir = tempfile::tempdir().unwrap();
    seed_demo_workflow(dir.path());

    let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::new(WorkflowRun {
        output: json!({ "nodes": { "worker": { "items": [] } } }),
        // Issue #881: the union — the blocked node's id rides here too, and
        // `summarize_run` is what has to keep it out of the "Paused for
        // approval" line.
        pending_approvals: vec!["worker".to_string(), "gate".to_string()],
        deliveries: Vec::new(),
        cancelled: false,
        nodes: Vec::new(),
        notices: Vec::new(),
        board: Vec::new(),
        blocked_nodes: vec![crate::ports::WorkflowBlockedNode {
            node_id: "worker".to_string(),
            tools: vec!["publish_artifact".to_string()],
            approval_ids: vec!["appr-1".to_string()],
            unparkable: 0,
            stranded: 0,
            blockers: 0,
        }],
        approvals: vec![
            crate::ports::WorkflowRunApprovalRow {
                node_id: Some("worker".to_string()),
                tool: Some("publish_artifact".to_string()),
                outcome: crate::ports::WorkflowApprovalOutcome::Parked,
                approval_id: Some("appr-1".to_string()),
            },
            // Issue #900: a receipt for a call that did NOT land a card.
            // `run.approvals.len()` would count this as a second "parked"
            // approval; the JSON's `approvals_parked` must not.
            crate::ports::WorkflowRunApprovalRow {
                node_id: Some("worker".to_string()),
                tool: Some("publish_artifact".to_string()),
                outcome: crate::ports::WorkflowApprovalOutcome::ParkFailed,
                approval_id: None,
            },
        ],
    }));
    let handle = WorkflowRunnerHandle::default();
    handle.set(&runner);

    let refs = WorkflowRefQueue::default();
    let tool = RunWorkflowTool::new(
        CompanyId::new("acme"),
        Some(dir.path().to_path_buf()),
        Arc::new(MemStore::default()),
        handle,
        crate::runtime::RunSupervisor::default(),
        None,
        refs,
        RunOutputCache::default(),
        None,
    );
    let result = tool
        .execute(json!({ "id": "demo" }))
        .await
        .expect("execute");
    assert!(!result.is_error);
    let out = result.output_for_llm(true);
    assert!(
        out.contains("Blocked, waiting on a person") && out.contains("worker"),
        "{out}"
    );
    assert!(
        out.contains("Paused for approval") && out.contains("gate"),
        "{out}"
    );
    // The blocked node must not also read as an ordinary paused gate — that
    // sentence promises the run continues once it is decided, which is
    // false for a block (issue #881).
    let paused_line = out
        .lines()
        .find(|l| l.contains("Paused for approval"))
        .expect("a Paused for approval line");
    assert!(!paused_line.contains("worker"), "{out}");

    let payload = match &result.content[0] {
        openhuman_core::skills::types::ToolContent::Json { data } => data.clone(),
        other => panic!("expected JSON payload, got {other:?}"),
    };
    assert_eq!(
        payload.get("blocked_nodes").and_then(Value::as_u64),
        Some(1)
    );
    // Issue #900: two receipts on this run (one parked, one that failed to
    // park), and the JSON count must name only the decidable one.
    assert_eq!(
        payload.get("approvals_parked").and_then(Value::as_u64),
        Some(1),
        "approvals_parked must exclude the ParkFailed receipt: {payload}"
    );
}

#[tokio::test(start_paused = true)]
async fn workflow_tool_survives_single_call_deadline_and_stages_result() {
    struct SlowRunner;
    #[async_trait::async_trait]
    impl WorkflowRunner for SlowRunner {
        async fn run(
            &self,
            _company: &CompanyId,
            _workflow: &WorkflowFile,
            _input: Value,
            _ctx: &crate::ports::WorkflowRunContext,
        ) -> crate::Result<WorkflowRun> {
            tokio::time::sleep(std::time::Duration::from_secs(121)).await;
            Ok(StubRunner::empty().run)
        }
    }
    let dir = tempfile::tempdir().unwrap();
    seed_demo_workflow(dir.path());
    let runner: Arc<dyn WorkflowRunner> = Arc::new(SlowRunner);
    let handle = WorkflowRunnerHandle::default();
    handle.set(&runner);
    let refs = WorkflowRefQueue::default();
    let tool = RunWorkflowTool::new(
        CompanyId::new("acme"),
        Some(dir.path().to_path_buf()),
        Arc::new(MemStore::default()),
        handle,
        crate::runtime::RunSupervisor::default(),
        None,
        refs.clone(),
        RunOutputCache::default(),
        None,
    );
    let args = json!({"id": "demo"});
    let (deadline, _) = oh::tools::timeout::resolve_tool_deadline(tool.timeout_policy(&args));
    assert!(
        deadline.is_none(),
        "the harness must not truncate a supervised workflow"
    );
    let result = tool.execute(args).await.unwrap();
    assert!(!result.is_error, "{result:?}");
    assert_eq!(
        refs.drain().len(),
        1,
        "completion still stages the real workflow result"
    );
}
