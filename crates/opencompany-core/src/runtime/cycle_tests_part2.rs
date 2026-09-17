use super::tests_core::*;
use super::tests_core2::*;
use super::*;

/// Issue #845, the wiring: the briefing actually reaches the brain.
///
/// [`only_a_workflow_message_gets_the_builder_briefing`] pins what the
/// injection *does* by calling it; this pins that `run_cycle` calls it. The
/// two are separate failures — a correct injection nothing invokes leaves
/// the bug exactly where it was — and only this one covers the wiring, so
/// deleting the call site has to fail a test.
///
/// `EffectBrain` echoes the text it was handed, so the reply is a faithful
/// window onto what the brain actually saw.
#[tokio::test]
async fn the_builder_briefing_reaches_the_brain_through_run_cycle() {
    let home_dir = tmp_home();
    let home = home_dir.path().to_path_buf();
    let effect = Effect {
        kind: "noop".into(),
        group: EffectGroup::Other,
        amount_usd: None,
        established_thread: false,
        first_time_counterparty: false,
        payload: serde_json::Value::Null,
        agent: None,
        run_id: None,
    };
    let rt = Arc::new(
        RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_brain(Arc::new(EffectBrain { effect }))
            .build()
            .await
            .unwrap(),
    );

    let ask = |deliverable| CompanyEvent::OperatorMessage {
        mentions: Vec::new(),
        text: "set up a weekly AEO audit".into(),
        by: None,
        chat: None,
        parent: None,
        deliverable,
        attachments: Vec::new(),
    };

    let workflow = rt
        .run_cycle(vec![ask(Some(MessageIntent::Workflow))])
        .await
        .unwrap();
    let seen = workflow
        .responses
        .iter()
        .map(|r| r.text.clone())
        .collect::<String>();
    assert!(
        seen.contains(BUILDER_ANNOTATION),
        "the brain must be told the builder owns this: {seen}"
    );

    // …and a one-off is handed through byte-for-byte, so the annotation is
    // not simply always on.
    let once = rt
        .run_cycle(vec![ask(Some(MessageIntent::Once))])
        .await
        .unwrap();
    let seen_once = once
        .responses
        .iter()
        .map(|r| r.text.clone())
        .collect::<String>();
    assert!(!seen_once.contains(BUILDER_ANNOTATION), "{seen_once}");
    assert!(
        seen_once.contains("set up a weekly AEO audit"),
        "{seen_once}"
    );
}

/// Issue #796: a DM's thread becomes a safe, `dm-`-prefixed branch segment
/// `RepoManager::validate_task_segment` accepts; an empty or all-garbage
/// thread yields nothing, so `repo_publish` refuses rather than build a
/// broken ref.
#[test]
fn sanitize_work_segment_makes_a_safe_branch_segment() {
    // An already-safe thread is unchanged and keeps a readable key.
    assert_eq!(sanitize_work_segment("coder"), Some("dm-coder".into()));
    // Dots and underscores are already valid and survive.
    assert_eq!(sanitize_work_segment("a_b.c"), Some("dm-a_b.c".into()));
    // Nothing usable.
    assert_eq!(sanitize_work_segment(""), None);
    assert_eq!(sanitize_work_segment("///"), None);

    // When folding/trimming loses information, the readable body is kept and
    // a digest of the raw thread is appended so distinct threads never share
    // a work key. Colons, slashes and spaces fold to '-'; leading/trailing
    // separators are trimmed before the prefix.
    let folded = sanitize_work_segment("dm:coder/main x").unwrap();
    assert!(folded.starts_with("dm-dm-coder-main-x-"), "{folded}");
    let trimmed = sanitize_work_segment("--weird--").unwrap();
    assert!(trimmed.starts_with("dm-weird-"), "{trimmed}");

    // The collision the digest closes: two threads that fold to the same body
    // get distinct keys — and the digest is deterministic across calls.
    assert_ne!(
        sanitize_work_segment("coder/main"),
        sanitize_work_segment("coder-main"),
        "distinct threads must not share a work key"
    );
    assert_eq!(
        sanitize_work_segment("coder/main"),
        sanitize_work_segment("coder/main")
    );
}

/// Issue #151: a delegated reply is addressed by agent id, so no adapter
/// matches it. It used to be dropped silently — the operator REST route
/// never noticed because it reads `CycleReport.responses` directly, but a
/// company reached over a channel adapter lost every delegated reply while
/// still receiving the orchestrator's.
#[tokio::test]
async fn a_reply_addressed_by_agent_id_reaches_the_operator_channel() {
    let home_dir = tmp_home();
    let home = home_dir.path().to_path_buf();
    let operator_channel = OperatorChannel::new();
    let channels: Vec<Arc<dyn ChannelAdapter>> = vec![Arc::new(operator_channel.clone())];
    let rt = RuntimeBuilder::new(home.clone(), manifest("supervised"))
        .with_brain(Arc::new(DelegatingBrain))
        .with_channels(channels)
        .build()
        .await
        .unwrap();

    rt.run_cycle(vec![CompanyEvent::OperatorMessage {
        mentions: Vec::new(),
        parent: None,
        text: "hand it off".into(),
        by: None,
        chat: None,
        deliverable: None,
        attachments: Vec::new(),
    }])
    .await
    .unwrap();

    let sent = operator_channel.sent();
    assert_eq!(sent.len(), 2, "both replies must be delivered: {sent:?}");
    // Attribution survives the fallback — the bubble still names the agent.
    let delegated = sent
        .iter()
        .find(|m| m.text == "delegated reply")
        .expect("the delegated reply must be delivered");
    assert_eq!(delegated.channel, "maya");
    assert_eq!(
        delegated.reply_to.as_ref().map(|r| r.chat_id.as_str()),
        Some("strategy")
    );
}

/// The rich settle always wins: `run_task` finishes the row *inside*
/// `brain.run_cycle`, which is awaited before the backstop, so there is no
/// race for the backstop to lose. Pinned rather than argued, because a
/// backstop that overwrote a real outcome with a generic failure would be
/// worse than no backstop at all.
///
/// Both cases matter. A **terminal** settle must survive; so must a
/// **parked** one — `Paused` and `WaitingApproval` are waiting on something
/// outside the cycle, and reclaiming them would delete real pending work
/// every time a cycle ended.
#[tokio::test]
async fn the_backstop_never_overwrites_a_settle_the_brain_already_made() {
    for (status, error) in [
        (RunStatus::Succeeded, None),
        (RunStatus::Paused, None),
        (RunStatus::WaitingApproval, None),
        (RunStatus::Failed, Some("the brain said so")),
    ] {
        let home_dir = tmp_home();
        let runs: Arc<dyn crate::ports::RunStore> =
            Arc::new(crate::store::FsOps::new(home_dir.path().to_path_buf()));
        let rt = RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest("full"))
            .with_runs(Arc::clone(&runs))
            .with_brain(Arc::new(SettlingBrain {
                runs: Arc::clone(&runs),
                status,
            }))
            .build()
            .await
            .unwrap();
        let run_id = pending_run(&rt, "t-1").await;

        rt.run_cycle(vec![CompanyEvent::TaskDispatched {
            task_id: "t-1".into(),
            run_id: Some(run_id.clone()),
            origin_chat_id: None,
            origin_parent: None,
        }])
        .await
        .expect("cycle");

        let settled = rt
            .runs()
            .get_run(rt.id(), &run_id)
            .await
            .expect("read")
            .expect("row");
        assert_eq!(
            settled.status, status,
            "the backstop must not overwrite a {status} settle"
        );
        assert_eq!(settled.error.as_deref(), error);
    }
}

/// Issue #242, the `begin_run` half: the run moves `Pending` → `Running`
/// stamped with the **seq of the very `TaskDispatched` event that drove
/// it**, and by the end of the cycle it is terminal rather than stranded —
/// even though the default build's brain ignores `TaskDispatched` entirely
/// and settles nothing.
#[tokio::test]
async fn a_dispatch_cycle_starts_its_run_and_never_leaves_it_claiming_to_be_live() {
    let home_dir = tmp_home();
    let rt = RuntimeBuilder::fs_defaults(home_dir.path().to_path_buf(), manifest("full"))
        .await
        .unwrap();
    let run_id = pending_run(&rt, "t-1").await;

    let report = rt
        .run_cycle(vec![CompanyEvent::TaskDispatched {
            task_id: "t-1".into(),
            run_id: Some(run_id.clone()),
            origin_chat_id: None,
            origin_parent: None,
        }])
        .await
        .expect("the cycle itself succeeds");

    let run = rt
        .runs()
        .get_run(rt.id(), &run_id)
        .await
        .expect("read")
        .expect("the run survives its cycle");
    assert_eq!(
        run.trigger_event_seq, report.persisted_seq,
        "the run must name the exact log line that drove it"
    );
    assert!(
        run.started_at_millis.is_some(),
        "begin_run stamps when the attempt actually began"
    );
    assert_eq!(
        run.status,
        RunStatus::Failed,
        "an echo-brain dispatch produces no rich settle, so the backstop closes it"
    );
    assert_eq!(run.error.as_deref(), Some(RUN_UNSETTLED_ERROR));
    assert!(run.finished_at_millis.is_some());
}

/// Issue #337: the backstop settles the **card** as well as the row.
///
/// Driven offline through the default build's echo brain, which ignores
/// `TaskDispatched` entirely — so nothing produces a rich settle and the
/// backstop is the only thing that can move anything. Before this, it
/// closed the row and left the card in In Progress: the board claimed work
/// that provably was not happening, and nothing would re-drive it, because
/// `task_enters_in_progress` fires on the transition and that already
/// happened.
#[tokio::test]
async fn the_backstop_returns_a_card_its_run_abandoned() {
    use crate::ports::tasks::{COLUMN_IN_PROGRESS, COLUMN_TODO, TaskRecord};

    let home_dir = tmp_home();
    let rt = RuntimeBuilder::fs_defaults(home_dir.path().to_path_buf(), manifest("full"))
        .await
        .unwrap();
    rt.tasks()
        .upsert(
            rt.id(),
            &TaskRecord {
                id: "t-1".to_string(),
                title: TaskTitle::authored("Draft the spec"),
                note: None,
                column: COLUMN_IN_PROGRESS.to_string(),
                priority: "medium".to_string(),
                assignee: "ceo".to_string(),
                updated_at_millis: 1,
                origin: None,
                parent_task_id: None,
                // Nothing has run yet, so there is no deliverable to point at
                // (issue #339). The first successful settle stamps it.
                output: None,
                plan: None,
                planning_attempts: Vec::new(),
                deliverable: crate::ports::tasks::TaskDeliverable::Once,
                workflow_proposal: None,
                origin_run_id: None,
                origin_workflow_id: None,
                origin_message_seq: None,
                bounced: None,
            },
        )
        .await
        .unwrap();
    let run_id = pending_run(&rt, "t-1").await;

    rt.run_cycle(vec![CompanyEvent::TaskDispatched {
        task_id: "t-1".into(),
        run_id: Some(run_id),
        origin_chat_id: None,
        origin_parent: None,
    }])
    .await
    .expect("the cycle itself succeeds");

    let card = rt
        .tasks()
        .list(rt.id())
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.id == "t-1")
        .expect("card");
    assert_eq!(
        card.column, COLUMN_TODO,
        "an unsettled attempt must not leave its card claiming to be worked"
    );
    let note = card.note.expect("the board must say why");
    assert!(note.contains(RUN_UNSETTLED_ERROR), "{note}");

    // The caller-level backstop must announce the same bounce through the
    // durable notification feed, not merely update the board.
    let notifications = rt
        .notifications()
        .list(rt.id(), "owner")
        .await
        .expect("read notifications");
    let notification = notifications
        .iter()
        .find(|n| n.notification.kind == "dispatch_failed")
        .expect("a bounced To-do card emits a dispatch-failed notification");
    assert_eq!(
        notification.notification.subject.id, "t-1",
        "the notification must point at the affected task"
    );
    assert!(
        notification
            .notification
            .title
            .contains(RUN_UNSETTLED_ERROR),
        "the notification must carry the failure reason: {:?}",
        notification.notification.title
    );
}

/// The guard, at the backstop: a card an operator has already parked is
/// **not** dragged back to To-do by a late settle. The row still closes —
/// the two are independent, and only one of them is the operator's.
#[tokio::test]
async fn the_backstop_leaves_a_parked_card_exactly_where_the_operator_put_it() {
    use crate::ports::tasks::{COLUMN_PAUSED, TaskRecord};

    let home_dir = tmp_home();
    let rt = RuntimeBuilder::fs_defaults(home_dir.path().to_path_buf(), manifest("full"))
        .await
        .unwrap();
    rt.tasks()
        .upsert(
            rt.id(),
            &TaskRecord {
                id: "t-1".to_string(),
                title: TaskTitle::authored("Draft the spec"),
                note: Some("[operator] parked this".to_string()),
                column: COLUMN_PAUSED.to_string(),
                priority: "medium".to_string(),
                assignee: "ceo".to_string(),
                updated_at_millis: 1,
                origin: None,
                parent_task_id: None,
                // Nothing has run yet, so there is no deliverable to point at
                // (issue #339). The first successful settle stamps it.
                output: None,
                plan: None,
                planning_attempts: Vec::new(),
                deliverable: crate::ports::tasks::TaskDeliverable::Once,
                workflow_proposal: None,
                origin_run_id: None,
                origin_workflow_id: None,
                origin_message_seq: None,
                bounced: None,
            },
        )
        .await
        .unwrap();
    let run_id = pending_run(&rt, "t-1").await;

    rt.run_cycle(vec![CompanyEvent::TaskDispatched {
        task_id: "t-1".into(),
        run_id: Some(run_id.clone()),
        origin_chat_id: None,
        origin_parent: None,
    }])
    .await
    .expect("the cycle itself succeeds");

    let card = rt
        .tasks()
        .list(rt.id())
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.id == "t-1")
        .expect("card");
    assert_eq!(card.column, COLUMN_PAUSED);
    assert_eq!(
        card.note.as_deref(),
        Some("[operator] parked this"),
        "a refused move must not annotate the card either"
    );
    // …and the row is still closed, because bookkeeping is not the
    // operator's business.
    assert_eq!(
        rt.runs()
            .get_run(rt.id(), &run_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        RunStatus::Failed
    );
}

/// The other backstop arm: the brain **errored**. The cycle error still
/// propagates to the caller (nothing is swallowed), and the row settles
/// carrying that same reason instead of sitting `Running` forever.
#[tokio::test]
async fn a_failed_cycle_settles_its_run_and_still_reports_the_failure() {
    let home_dir = tmp_home();
    let rt = RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest("full"))
        .with_brain(Arc::new(FailingBrain))
        .build()
        .await
        .unwrap();
    let run_id = pending_run(&rt, "t-1").await;

    let err = rt
        .run_cycle(vec![CompanyEvent::TaskDispatched {
            task_id: "t-1".into(),
            run_id: Some(run_id.clone()),
            origin_chat_id: None,
            origin_parent: None,
        }])
        .await
        .expect_err("a failing brain still fails the cycle");
    assert!(err.to_string().contains("the brain fell over"), "{err}");

    let run = rt
        .runs()
        .get_run(rt.id(), &run_id)
        .await
        .expect("read")
        .expect("run");
    assert_eq!(run.status, RunStatus::Failed);
    let reason = run.error.unwrap_or_default();
    assert!(reason.starts_with(RUN_CYCLE_FAILED_ERROR), "{reason}");
    assert!(
        reason.contains("the brain fell over"),
        "the row must carry the reason the caller saw: {reason}"
    );

    // …and the company-wide badge must NOT (CodeRabbit review on #1905).
    // The attempt row above is scoped to whoever can see the card; a
    // notification title is broadcast to every member, and
    // `advance::notify_dispatch_failed` only flattens newlines — so a
    // provider body quoting a key, a URL or a customer's name would go out
    // to the whole company. The badge names the class; the words stay on
    // the row and the card note.
    for filed in rt
        .notifications()
        .list(rt.id(), "owner")
        .await
        .expect("read notifications")
        .iter()
        .filter(|n| n.notification.kind == "dispatch_failed")
    {
        assert!(
            !filed.notification.title.contains("the brain fell over"),
            "the cycle error must not reach a company-wide title: {:?}",
            filed.notification.title
        );
        assert!(
            filed.notification.title.contains(RUN_CYCLE_FAILED_ERROR),
            "it still has to say what happened: {:?}",
            filed.notification.title
        );
    }
}

/// A dispatch whose run row could not be minted (`run_id: None`) — the
/// documented degraded path — must still run the cycle normally. The
/// dispatch is the work; the row is only the record of it.
#[tokio::test]
async fn an_untracked_dispatch_still_runs_its_cycle() {
    let home_dir = tmp_home();
    let rt = RuntimeBuilder::fs_defaults(home_dir.path().to_path_buf(), manifest("full"))
        .await
        .unwrap();

    rt.run_cycle(vec![CompanyEvent::TaskDispatched {
        task_id: "t-1".into(),
        run_id: None,
        origin_chat_id: None,
        origin_parent: None,
    }])
    .await
    .expect("an untracked dispatch is still a dispatch");

    assert!(
        rt.runs()
            .list_runs(rt.id(), &crate::ports::runs::RunFilter::default())
            .await
            .expect("list")
            .is_empty(),
        "no row was minted, so none may be invented"
    );
}

/// A `run_id` naming a row that does not exist (a replayed journal line, a
/// row lost with its store) must not fail the cycle either — and must not
/// be tracked, so the backstop has nothing to settle.
#[tokio::test]
async fn a_dispatch_naming_an_unknown_run_does_not_fail_the_cycle() {
    let home_dir = tmp_home();
    let rt = RuntimeBuilder::fs_defaults(home_dir.path().to_path_buf(), manifest("full"))
        .await
        .unwrap();

    rt.run_cycle(vec![CompanyEvent::TaskDispatched {
        task_id: "t-1".into(),
        run_id: Some("run-that-never-was".into()),
        origin_chat_id: None,
        origin_parent: None,
    }])
    .await
    .expect("an unknown run id is a bookkeeping miss, not a cycle failure");
}

/// Issue #1175: a cycle used to load 32 recent traces *and the whole context
/// index* (`list(company, "")` — no prefix, no limit) into `CycleRequest`,
/// where no brain read either. Both reads are gone, and the context one was
/// the expensive half: it grew with every turn the company had ever run.
///
/// The trace *write* deliberately stayed — traces travel with the export
/// bundle — so this asserts the save as well. Without that half, a later
/// "nothing reads traces, delete the write" would pass silently.
#[tokio::test]
async fn a_cycle_reads_neither_recent_traces_nor_the_context_index() {
    let home_dir = tmp_home();
    let home = home_dir.path().to_path_buf();
    let memory = Arc::new(CountingMemory::new(FsMemoryStore::new(home.clone())));
    let context = Arc::new(CountingContext::new(FsContextStore::new(home.clone())));
    let rt = RuntimeBuilder::new(home, manifest("full"))
        .with_memory(memory.clone())
        .with_context(context.clone())
        .build()
        .await
        .unwrap();

    // Boot is not what this test is about; only what one cycle costs.
    memory.reads.store(0, Ordering::SeqCst);
    memory.writes.store(0, Ordering::SeqCst);
    context.lists.store(0, Ordering::SeqCst);

    rt.run_cycle(vec![CompanyEvent::OperatorMessage {
        mentions: Vec::new(),
        parent: None,
        // Issue #1725: not "hi". A bare pleasantry is now answered without
        // a turn at all, and this test is about what a turn costs.
        text: "ship the landing page".into(),
        by: None,
        chat: None,
        deliverable: None,
        attachments: Vec::new(),
    }])
    .await
    .unwrap();

    assert_eq!(
        memory.reads.load(Ordering::SeqCst),
        0,
        "a cycle must not read traces back: no brain consumes them"
    );
    assert_eq!(
        context.lists.load(Ordering::SeqCst),
        0,
        "a cycle must not scan the context index: no brain consumes it"
    );
    assert_eq!(
        memory.writes.load(Ordering::SeqCst),
        1,
        "the trace write is not dead code — it feeds the export bundle"
    );
}

#[tokio::test]
async fn end_to_end_operator_message_echoes_and_persists() {
    let home_dir = tmp_home();
    let home = home_dir.path().to_path_buf();
    let rt = RuntimeBuilder::fs_defaults(home.clone(), manifest("full"))
        .await
        .unwrap();

    let report = rt
        .run_cycle(vec![CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            parent: None,
            // Issue #1725: not "hi". A bare pleasantry never reaches the
            // brain now, and the echo this asserts is the brain's.
            text: "ship the landing page".into(),
            by: None,
            chat: None,
            deliverable: None,
            attachments: Vec::new(),
        }])
        .await
        .unwrap();

    // (a) an operator response came back.
    assert_eq!(report.responses.len(), 1);
    assert_eq!(report.responses[0].channel, "operator");
    assert_eq!(report.responses[0].text, "You said: ship the landing page");

    // (b) the event was appended to the log.
    //
    // Filtered rather than counted: since issue #327 boot's workspace
    // scaffold journals a `WorkspaceChanged` per reserved root, so the log
    // already holds entries this test is not about.
    let stored = rt
        .events
        .read_from(rt.id(), EventSeq::new(0), 10)
        .await
        .unwrap();
    let operator: Vec<_> = stored
        .iter()
        .filter(|e| matches!(e.event, CompanyEvent::OperatorMessage { .. }))
        .collect();
    assert_eq!(operator.len(), 1);
    assert_eq!(
        operator[0].event,
        CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            parent: None,
            text: "ship the landing page".into(),
            by: None,
            chat: None,
            deliverable: None,
            attachments: Vec::new(),
        }
    );

    // (c) a compressed trace was persisted.
    let traces = rt.memory.recent_traces(rt.id(), 10).await.unwrap();
    assert!(!traces.is_empty());
}
