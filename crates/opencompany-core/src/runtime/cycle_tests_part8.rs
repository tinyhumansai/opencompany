use super::tests_core::*;
use super::tests_core2::*;

#[tokio::test]
async fn send_email_runs_without_policy_hitl_for_a_new_recipient() {
    let home_dir = tmp_home();
    let home = home_dir.path().to_path_buf();
    let sender = Arc::new(RecordingMailSender::new());
    let rt = RuntimeBuilder::new(home.clone(), manifest("supervised"))
        .with_mail(CompanyMail {
            sender: sender.clone(),
            smtp: test_smtp("ceo@acme.test"),
        })
        .build()
        .await
        .unwrap();
    let host = CycleHostImpl::new(
        rt.id().clone(),
        "cyc-park".into(),
        &rt,
        None,
        false,
        ApprovalConversation::default(),
    );

    let res = host
        .send_email(serde_json::json!({ "to": "new@ext.com", "subject": "s", "body": "b" }))
        .await
        .unwrap();
    assert_eq!(res.output["status"], "sent");
    assert_eq!(sender.sent().len(), 1);
}

/// Issue #333: an effect parked by a card's dispatch cycle is journaled
/// against that card, so the card's Approvals tab can find it.
#[tokio::test]
async fn a_dispatch_cycle_stamps_its_task_onto_every_approval_it_parks() {
    let home_dir = tmp_home();
    let home = home_dir.path().to_path_buf();
    let sender = Arc::new(RecordingMailSender::new());
    let rt = RuntimeBuilder::new(home.clone(), manifest("supervised"))
        .with_mail(CompanyMail {
            sender: sender.clone(),
            smtp: test_smtp("ceo@acme.test"),
        })
        .build()
        .await
        .unwrap();
    let host = CycleHostImpl::new(
        rt.id().clone(),
        "cyc-task".into(),
        &rt,
        Some("t-42".to_string()),
        false,
        ApprovalConversation::default(),
    );

    host.park_effect(harness_effect(
        "ceo",
        crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND,
        serde_json::json!({ "title": "Send the message", "question": "May I send it?" }),
    ))
    .await
    .unwrap();

    let pending = rt.pending_approvals();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].task,
        Some(TaskLink::Task {
            id: "t-42".to_string()
        }),
        "the parked approval must name the card that asked for it",
    );
    assert_eq!(
        rt.approval_origins()
            .get(&pending[0].id)
            .and_then(|o| o.task.clone()),
        Some(TaskLink::Task {
            id: "t-42".to_string()
        }),
        "and the link must outlive the queue entry",
    );
}

/// A cycle with no card behind it records the park as *explicitly* unlinked
/// rather than leaving the link blank (#333 review follow-up).
///
/// The blank is reserved for pre-#333 journal lines, and it is the only
/// thing the read side still window-guesses on. If a chat turn's park were
/// written that way too, every one of them would land on whatever card
/// happened to be mid-run — the bug this issue exists to close.
#[tokio::test]
async fn a_cycle_with_no_card_parks_explicitly_unlinked() {
    let home_dir = tmp_home();
    let home = home_dir.path().to_path_buf();
    let sender = Arc::new(RecordingMailSender::new());
    let rt = RuntimeBuilder::new(home.clone(), manifest("supervised"))
        .with_mail(CompanyMail {
            sender: sender.clone(),
            smtp: test_smtp("ceo@acme.test"),
        })
        .build()
        .await
        .unwrap();
    let host = CycleHostImpl::new(
        rt.id().clone(),
        "cyc-chat".into(),
        &rt,
        None,
        false,
        ApprovalConversation::default(),
    );

    host.park_effect(harness_effect(
        "ceo",
        crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND,
        serde_json::json!({ "title": "Send the message", "question": "May I send it?" }),
    ))
    .await
    .unwrap();

    let pending = rt.pending_approvals();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].task,
        Some(TaskLink::Unlinked),
        "an unlinked park must say so, not leave the link absent",
    );
}

/// Issue #379: an effect parked by a desk channel's turn carries that
/// channel onto the summary the console reads, **and** announces itself on
/// the event log so an inline card can appear without waiting for a poll.
#[tokio::test]
async fn a_chat_cycle_stamps_its_thread_and_announces_the_park() {
    let home_dir = tmp_home();
    let home = home_dir.path().to_path_buf();
    let sender = Arc::new(RecordingMailSender::new());
    let rt = RuntimeBuilder::new(home.clone(), manifest("supervised"))
        .with_mail(CompanyMail {
            sender: sender.clone(),
            smtp: test_smtp("ceo@acme.test"),
        })
        .build()
        .await
        .unwrap();
    let host = CycleHostImpl::new(
        rt.id().clone(),
        "cyc-thread".into(),
        &rt,
        None,
        false,
        ApprovalConversation {
            thread: Some("desk-finance".to_string()),
            parent: None,
        },
    );

    host.park_effect(harness_effect(
        "ceo",
        crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND,
        serde_json::json!({ "title": "Send the message", "question": "May I send it?" }),
    ))
    .await
    .unwrap();

    let pending = rt.pending_approvals();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].thread,
        Some("desk-finance".to_string()),
        "the parked approval must name the conversation that asked for it",
    );

    let logged = rt
        .events()
        .read_from(rt.id(), EventSeq::new(0), 50)
        .await
        .unwrap();
    let parked: Vec<_> = logged
        .iter()
        .filter_map(|e| match &e.event {
            CompanyEvent::ApprovalParked {
                approval_id,
                effect_kind,
                thread,
            } => Some((approval_id.clone(), effect_kind.clone(), thread.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(parked.len(), 1, "exactly one park announcement: {logged:?}");
    assert_eq!(parked[0].0, pending[0].id);
    assert_eq!(
        parked[0].1,
        crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND
    );
    assert_eq!(parked[0].2, Some("desk-finance".to_string()));
}

/// The same park with no conversation behind it announces itself with **no
/// thread**, which is what keeps it Approvals-page-only. Inline is additive,
/// never a replacement (#379).
#[tokio::test]
async fn a_threadless_park_announces_itself_without_a_channel() {
    let home_dir = tmp_home();
    let home = home_dir.path().to_path_buf();
    let sender = Arc::new(RecordingMailSender::new());
    let rt = RuntimeBuilder::new(home.clone(), manifest("supervised"))
        .with_mail(CompanyMail {
            sender: sender.clone(),
            smtp: test_smtp("ceo@acme.test"),
        })
        .build()
        .await
        .unwrap();
    let host = CycleHostImpl::new(
        rt.id().clone(),
        "cyc-none".into(),
        &rt,
        None,
        false,
        ApprovalConversation::default(),
    );

    host.park_effect(harness_effect(
        "ceo",
        crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND,
        serde_json::json!({ "title": "Send the message", "question": "May I send it?" }),
    ))
    .await
    .unwrap();

    let pending = rt.pending_approvals();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].thread, None);
    // And it is omitted from the serialized summary entirely, so an older
    // console sees the wire shape it already knows.
    let wire = serde_json::to_value(&pending[0]).unwrap();
    assert!(
        wire.get("thread").is_none(),
        "an approval with no conversation must not carry an empty thread key: {wire}",
    );

    let logged = rt
        .events()
        .read_from(rt.id(), EventSeq::new(0), 50)
        .await
        .unwrap();
    assert!(
        logged
            .iter()
            .any(|e| matches!(&e.event, CompanyEvent::ApprovalParked { thread: None, .. })),
        "the park is still announced, it simply names no channel: {logged:?}",
    );
}

/// Issue #842: **every gated call one turn parks carries that turn's key**,
/// and a different turn's parks carry a different one.
///
/// This is the whole of the batching mechanism, and it is deliberately not
/// a new one. #469 already records the parking cycle so a turn blocked on
/// four decisions is continued once rather than four times; the operator
/// was simply never shown that grouping, so a research turn that reached
/// three sites interrupted the conversation three times to ask about one
/// piece of work. Projecting the key it already had is what lets the
/// conversation ask once.
///
/// The second host is the half that matters. A key every park shares would
/// consolidate correctly and also fold two unrelated turns into one card —
/// an operator approving a batch they never saw raised. Grouping is only
/// safe because the key separates turns, so both directions are asserted.
///
/// What is *not* changed here, and is asserted to make the point: the parks
/// stay two records with two ids. Chat groups them for display; each is
/// still decided on its own and still mints its own host-scoped grant
/// (#739).
#[tokio::test]
async fn every_approval_one_turn_parks_carries_that_turns_batch_key() {
    let home_dir = tmp_home();
    let home = home_dir.path().to_path_buf();
    let sender = Arc::new(RecordingMailSender::new());
    let rt = RuntimeBuilder::new(home.clone(), manifest("supervised"))
        .with_mail(CompanyMail {
            sender: sender.clone(),
            smtp: test_smtp("ceo@acme.test"),
        })
        .build()
        .await
        .unwrap();

    // One turn, two gated calls — the shape the issue reports, where a
    // research turn reaches several outside hosts before it yields.
    let turn = CycleHostImpl::new(
        rt.id().clone(),
        "cyc-research".into(),
        &rt,
        None,
        false,
        ApprovalConversation {
            thread: Some("desk-marketing".to_string()),
            parent: None,
        },
    );
    turn.park_effect(harness_effect(
        "seo",
        "web_fetch",
        serde_json::json!({ "url": "https://espn.com/nba" }),
    ))
    .await
    .unwrap();
    turn.park_effect(harness_effect(
        "seo",
        "web_fetch",
        serde_json::json!({ "url": "https://bbc.com/sport" }),
    ))
    .await
    .unwrap();

    // A later, unrelated turn in the same conversation.
    let other = CycleHostImpl::new(
        rt.id().clone(),
        "cyc-later".into(),
        &rt,
        None,
        false,
        ApprovalConversation {
            thread: Some("desk-marketing".to_string()),
            parent: None,
        },
    );
    other
        .park_effect(harness_effect(
            "seo",
            "web_fetch",
            serde_json::json!({ "url": "https://theguardian.com/uk" }),
        ))
        .await
        .unwrap();

    let pending = rt.pending_approvals();
    assert_eq!(pending.len(), 3, "one record per gated call, still");
    let batches: Vec<Option<String>> = pending.iter().map(|p| p.batch.clone()).collect();
    assert!(
        batches.iter().all(Option::is_some),
        "a park raised by a turn must name it: {batches:?}"
    );

    let by_url = |url: &str| {
        pending
            .iter()
            .find(|p| p.payload.as_ref().is_some_and(|v| v["url"] == url))
            .unwrap_or_else(|| panic!("no parked approval for {url}"))
    };
    let espn = by_url("https://espn.com/nba");
    let bbc = by_url("https://bbc.com/sport");
    let guardian = by_url("https://theguardian.com/uk");

    assert_eq!(
        espn.batch, bbc.batch,
        "two calls one turn parked belong to one batch, so the operator is asked once"
    );
    assert_ne!(
        espn.batch, guardian.batch,
        "a different turn is a different question — consolidating across turns would ask \
         an operator to approve work they never saw raised"
    );
    // Still three decisions underneath. The batch is presentation; the park
    // is the unit of truth, and each keeps its own id to be resolved by.
    assert_eq!(
        std::collections::HashSet::from([&espn.id, &bbc.id, &guardian.id]).len(),
        3,
        "grouping must not merge the records it groups"
    );
}

/// The correlation key itself (#333): which card a cycle is working, read
/// off its own trigger events.
#[test]
fn cycle_task_id_reads_a_dispatch_inherits_a_resolution_and_refuses_to_guess() {
    use crate::ports::types::{Actor, ActorKind, ApprovalId, Verdict};

    // The lookup a live cycle does per id, stubbed: `appr-1` belongs to a
    // card, `appr-none` is a recorded unlinked park, `appr-legacy` is a
    // pre-#333 line, and anything else has no origin at all.
    let approval_task = |id: &ApprovalId| match id.as_ref() {
        "appr-1" => Some(Some(TaskLink::Task { id: "t-1".into() })),
        "appr-none" => Some(Some(TaskLink::Unlinked)),
        "appr-legacy" => Some(None),
        _ => None,
    };
    let dispatched = |id: &str| CompanyEvent::TaskDispatched {
        task_id: id.to_string(),
        run_id: None,
        origin_chat_id: None,
        origin_parent: None,
    };
    let resolved = |id: &str| CompanyEvent::ApprovalResolved {
        approval_id: ApprovalId::new(id),
        verdict: Verdict::Approve,
        by: Actor {
            kind: ActorKind::Operator,
            id: "owner".into(),
        },
    };

    let chat = || CompanyEvent::OperatorMessage {
        mentions: Vec::new(),
        parent: None,
        text: "hi".into(),
        by: None,
        chat: None,
        deliverable: None,
        attachments: Vec::new(),
    };

    // A dispatch names the card outright.
    assert_eq!(
        cycle_task_id(&[dispatched("t-1")], approval_task),
        Some("t-1".into())
    );
    // A follow-up cycle inherits it from the approval it is resolving, so a
    // run needing two sign-offs keeps the link through the first.
    assert_eq!(
        cycle_task_id(&[resolved("appr-1")], approval_task),
        Some("t-1".into())
    );
    // An approval with no origin at all claims nothing.
    assert_eq!(
        cycle_task_id(&[resolved("appr-unknown")], approval_task),
        None
    );
    // Nor does a pre-#333 one.
    assert_eq!(
        cycle_task_id(&[resolved("appr-legacy")], approval_task),
        None
    );
    // Nothing task-shaped at all.
    assert_eq!(cycle_task_id(&[chat()], approval_task), None);
    // Two different cards in one batch: refuse to guess rather than hand one
    // of them the other's approvals.
    assert_eq!(
        cycle_task_id(&[dispatched("t-1"), dispatched("t-2")], approval_task),
        None
    );
    // The same card twice is not ambiguous.
    assert_eq!(
        cycle_task_id(&[dispatched("t-1"), resolved("appr-1")], approval_task),
        Some("t-1".into())
    );

    // Review follow-up: a cycle is a batch, not a turn. A chat message
    // riding the same batch as a dispatch is its own work, and an effect it
    // parks is not the card's — so the batch is ambiguous, exactly as two
    // cards would be. Same for a webhook, a schedule tick, or an A2A task.
    assert_eq!(
        cycle_task_id(&[dispatched("t-1"), chat()], approval_task),
        None,
        "a chat turn batched with a dispatch must not be stamped with the card",
    );
    assert_eq!(
        cycle_task_id(
            &[
                dispatched("t-1"),
                CompanyEvent::ScheduleFired {
                    cron: "* * * * *".into(),
                    prompt: "tick".into(),
                },
            ],
            approval_task,
        ),
        None,
    );
    // A payment and a filed feedback item are inbound triggers too — they
    // drive their own turn, so neither may inherit the card's stamp.
    assert_eq!(
        cycle_task_id(
            &[
                dispatched("t-1"),
                CompanyEvent::PaymentReceived {
                    amount_usd: 10.0,
                    memo: "invoice".into(),
                },
            ],
            approval_task,
        ),
        None,
    );
    assert_eq!(
        cycle_task_id(
            &[
                dispatched("t-1"),
                CompanyEvent::FeedbackFiled {
                    note: "it mis-filed".into(),
                },
            ],
            approval_task,
        ),
        None,
    );
    // A record of something that already happened is not a rival: it names
    // no card and competes for none, so the dispatch still stamps.
    assert_eq!(
        cycle_task_id(
            &[
                dispatched("t-1"),
                CompanyEvent::DeskTaskCompleted {
                    task_id: "t-9".into(),
                    desk: "ops".into(),
                    column: "done".into(),
                    artifact_ids: Vec::new(),
                    output: String::new(),
                    origin_chat_id: None,
                    origin_parent: None,
                },
            ],
            approval_task,
        ),
        Some("t-1".into()),
        "a completion record must not disqualify the batch",
    );
    // And a resolution known to belong to no card is a rival trigger too,
    // not a neutral event — it is somebody's work, just not a card's.
    assert_eq!(
        cycle_task_id(&[dispatched("t-1"), resolved("appr-none")], approval_task),
        None,
    );

    // Issue #327: a workspace write is a record of something that already
    // happened, so it is neutral on both counts. Alone it names no card…
    assert_eq!(cycle_task_id(&[workspace_changed()], approval_task), None);
    // …and — the arm that actually matters — it must not *disqualify* a
    // dispatch it happens to share a batch with. An agent writing a note
    // while its cycle runs is the ordinary case, and treating that write as
    // a rival trigger would strip the card off its own stamp.
    assert_eq!(
        cycle_task_id(&[dispatched("t-1"), workspace_changed()], approval_task),
        Some("t-1".into()),
        "a workspace write must not disqualify the dispatch beside it",
    );
    assert_eq!(
        cycle_task_id(
            &[workspace_changed(), dispatched("t-1"), workspace_changed()],
            approval_task
        ),
        Some("t-1".into()),
        "and not from either side of it",
    );

    // Issue #382: a per-node start bracket is the same kind of record — a
    // workflow walking its graph, not a stimulus. Alone it names no card…
    assert_eq!(
        cycle_task_id(&[workflow_node_started()], approval_task),
        None,
    );
    // …and it must not disqualify a dispatch it shares a batch with (a
    // workflow node beginning while a cycle's card runs is the ordinary
    // case).
    assert_eq!(
        cycle_task_id(&[dispatched("t-1"), workflow_node_started()], approval_task),
        Some("t-1".into()),
        "a node-start bracket must not disqualify the dispatch beside it",
    );
}
