use super::tests_core::*;
use super::tests_core2::*;

/// The conversation key (#379): which chat thread a cycle is answering, read
/// off its own trigger events.
///
/// The trap this exists to close is the one `Effect::agent` cannot: a desk
/// channel and a direct message to that desk's lead are answered by the same
/// teammate and are **different threads**. `OperatorMessage.chat` is the only
/// field that tells them apart, which is why the stamp is read from there.
#[test]
fn cycle_thread_id_reads_an_addressed_message_inherits_a_resolution_and_refuses_to_guess() {
    use crate::ports::types::{Actor, ActorKind, ApprovalId, Verdict};

    // The lookup a live cycle does per id, stubbed: `appr-desk` was raised in
    // a desk channel, `appr-dm` in a direct message, `appr-none` had no
    // conversation behind it (or is a pre-#379 line — the same answer, on
    // purpose), and anything else has no origin at all.
    let conv = |thread: Option<&str>, parent: Option<u64>| ApprovalConversation {
        thread: thread.map(str::to_string),
        parent: parent.map(EventSeq::new),
    };
    let approval_conversation = move |id: &ApprovalId| match id.as_ref() {
        "appr-desk" => Some(conv(Some("desk-finance"), None)),
        "appr-dm" => Some(conv(Some("agent-cfo"), None)),
        "appr-none" => Some(conv(None, None)),
        // Issue #435: raised inside a thread of the desk channel.
        "appr-desk-threaded" => Some(conv(Some("desk-finance"), Some(7))),
        _ => None,
    };
    let addressed = |chat: &str| CompanyEvent::OperatorMessage {
        mentions: Vec::new(),
        parent: None,
        text: "pay the invoice".into(),
        by: None,
        chat: Some(chat.to_string()),
        deliverable: None,
        attachments: Vec::new(),
    };
    let unaddressed = || CompanyEvent::OperatorMessage {
        mentions: Vec::new(),
        parent: None,
        text: "hi".into(),
        by: None,
        chat: None,
        deliverable: None,
        attachments: Vec::new(),
    };
    let resolved = |id: &str| CompanyEvent::ApprovalResolved {
        approval_id: ApprovalId::new(id),
        verdict: Verdict::Approve,
        by: Actor {
            kind: ActorKind::Operator,
            id: "owner".into(),
        },
    };
    let dispatched = || CompanyEvent::TaskDispatched {
        task_id: "t-1".into(),
        run_id: None,
        origin_chat_id: None,
        origin_parent: None,
    };

    // An addressed message names the thread outright.
    assert_eq!(
        cycle_conversation(&[addressed("desk-finance")], &[], approval_conversation).thread,
        Some("desk-finance".into()),
    );
    // The whole point, stated as an assertion: the desk channel and a DM to
    // that desk's lead are different stamps, even though the same agent
    // answers both.
    assert_eq!(
        cycle_conversation(&[addressed("agent-cfo")], &[], approval_conversation).thread,
        Some("agent-cfo".into()),
    );
    // A follow-up cycle inherits the thread from the approval it resolves, so
    // a turn needing a second sign-off re-parks in the same channel.
    assert_eq!(
        cycle_conversation(&[resolved("appr-desk")], &[], approval_conversation).thread,
        Some("desk-finance".into()),
    );
    assert_eq!(
        cycle_conversation(&[resolved("appr-dm")], &[], approval_conversation).thread,
        Some("agent-cfo".into()),
    );
    // An approval with no origin at all claims nothing — and does not block.
    assert_eq!(
        cycle_conversation(
            &[resolved("appr-unknown"), addressed("desk-finance")],
            &[],
            approval_conversation
        )
        .thread,
        Some("desk-finance".into()),
    );
    // An unaddressed message went to the orchestrator with no conversation of
    // its own. It is a rival, not a pass-through.
    assert_eq!(
        cycle_conversation(&[unaddressed()], &[], approval_conversation).thread,
        None
    );
    assert_eq!(
        cycle_conversation(
            &[addressed("desk-finance"), unaddressed()],
            &[],
            approval_conversation
        )
        .thread,
        None,
        "an unaddressed turn batched with an addressed one must not borrow its channel",
    );
    // Two different threads in one batch: refuse rather than raise one
    // conversation's request inside the other.
    assert_eq!(
        cycle_conversation(
            &[addressed("desk-finance"), addressed("agent-cfo")],
            &[],
            approval_conversation,
        )
        .thread,
        None,
    );
    // The same thread twice is not ambiguous.
    assert_eq!(
        cycle_conversation(
            &[addressed("desk-finance"), resolved("appr-desk")],
            &[],
            approval_conversation,
        )
        .thread,
        Some("desk-finance".into()),
    );
    // A resolution known to have come from no conversation is a rival too.
    assert_eq!(
        cycle_conversation(
            &[addressed("desk-finance"), resolved("appr-none")],
            &[],
            approval_conversation,
        )
        .thread,
        None,
    );
    // Inbound triggers that are their own work disqualify the batch, exactly
    // as a rival chat turn does for the card stamp.
    for rival in [
        dispatched(),
        CompanyEvent::ScheduleFired {
            cron: "* * * * *".into(),
            prompt: "tick".into(),
        },
        CompanyEvent::WebhookReceived {
            channel: "stripe".into(),
            body: serde_json::json!({}),
        },
        CompanyEvent::A2aTaskReceived {
            from: "peer".into(),
            task: serde_json::json!({}),
        },
        CompanyEvent::PaymentReceived {
            amount_usd: 10.0,
            memo: "invoice".into(),
        },
        CompanyEvent::FeedbackFiled {
            note: "it mis-filed".into(),
        },
    ] {
        assert_eq!(
            cycle_conversation(
                &[addressed("desk-finance"), rival.clone()],
                &[],
                approval_conversation
            )
            .thread,
            None,
            "{rival:?} is its own work and must not inherit the channel",
        );
    }
    // A record of something that already happened is not a rival — including
    // this cycle's own park event, which is appended after the park it
    // describes and would otherwise disqualify a second one.
    for record in [
        CompanyEvent::ApprovalParked {
            approval_id: ApprovalId::new("appr-desk"),
            effect_kind: "payment.send".into(),
            thread: Some("desk-finance".into()),
        },
        CompanyEvent::DeskTaskCompleted {
            task_id: "t-9".into(),
            desk: "ops".into(),
            column: "done".into(),
            artifact_ids: Vec::new(),
            output: String::new(),
            origin_chat_id: None,
            origin_parent: None,
        },
        CompanyEvent::AgentReply {
            audience: Vec::new(),
            mentions: Vec::new(),
            mention_depth: 0,
            parent: None,
            chat_id: "desk-ops".into(),
            agent_id: "ops".into(),
            text: "done".into(),
            steps: Vec::new(),
            task_id: None,
            outputs: Vec::new(),
        },
        // Issue #327: appended by the workspace store after the write it
        // describes. An agent that answers a message and touches the tree
        // in the same turn must keep its channel stamp.
        workspace_changed(),
        // Issue #382: a per-node start bracket is a workflow walking its
        // graph — a record, not a trigger, so it must not steal the channel
        // off an addressed message beside it either.
        workflow_node_started(),
    ] {
        assert_eq!(
            cycle_conversation(
                &[addressed("desk-finance"), record.clone()],
                &[],
                approval_conversation
            )
            .thread,
            Some("desk-finance".into()),
            "{record:?} is a record, not a trigger, and must not disqualify the batch",
        );
    }
    // And alone neither claims a conversation of its own.
    assert_eq!(
        cycle_conversation(&[workspace_changed()], &[], approval_conversation).thread,
        None,
    );
    assert_eq!(
        cycle_conversation(&[workflow_node_started()], &[], approval_conversation).thread,
        None,
    );
}

/// A message sent straight into a channel is the root of its own thread,
/// and the approval raised from it inherits that root (issue #1890).
///
/// `OperatorMessage::parent` is `None` for such a message, and reading it
/// verbatim recorded "no thread". The visible cost was a transcript that
/// contradicted itself: the reply *before* the sign-off landed under the
/// question (`reply_thread` treats an unparented message as its own root),
/// and the continuation *after* it landed flat in the channel. Same
/// conversation, two different answers to "which thread is this".
///
/// Reproduced by hand on the repro rig before it was fixed:
///
/// ```text
/// 37  parentId=None  operator  THREAD-THREE: deploy to staging
/// 42  parentId=37    ceo       Done with step 3.      <- reply: threaded
/// 46  parentId=None  ceo       Done with step 5.      <- continuation: flat
/// ```
#[test]
fn a_channel_level_message_is_the_root_its_approval_resumes_in() {
    let addressed = || CompanyEvent::OperatorMessage {
        mentions: Vec::new(),
        parent: None,
        text: "deploy to staging".into(),
        by: None,
        chat: Some("general".into()),
        deliverable: None,
        attachments: Vec::new(),
    };
    let none = |_: &ApprovalId| None;

    assert_eq!(
        cycle_conversation(&[addressed()], &[EventSeq::new(37)], none),
        ApprovalConversation {
            thread: Some("general".into()),
            parent: Some(EventSeq::new(37)),
        },
        "the message's own seq is the thread it resumes in"
    );

    // **Absent seqs degrade to today's answer, never to a guess.** A caller
    // that builds a request without threading seqs is documented and
    // supported (`CycleRequest::event_seqs`), and inventing a root for one
    // would write a wrong parent where there is currently an honest absent
    // one.
    assert_eq!(
        cycle_conversation(&[addressed()], &[], none),
        ApprovalConversation {
            thread: Some("general".into()),
            parent: None,
        },
        "no seq, no root — the channel is still the answer"
    );
}

/// Issue #435: the thread *within* the channel, and the asymmetry between
/// the two keys.
///
/// The channel rule is #379's and is asserted above. This pins the part
/// that is easy to get wrong in the obvious way: resolving `(channel,
/// thread)` as a single unit would make two messages in one channel but
/// two different threads ambiguous, dropping an approval that lands
/// correctly today off its conversation entirely. A finer key must never
/// cost a coarser answer that was already right.
#[test]
fn cycle_conversation_carries_the_thread_root_and_degrades_it_before_the_channel() {
    use crate::ports::types::{Actor, ActorKind, ApprovalId, Verdict};

    let conv = |thread: Option<&str>, parent: Option<u64>| ApprovalConversation {
        thread: thread.map(str::to_string),
        parent: parent.map(EventSeq::new),
    };
    let approval_conversation = move |id: &ApprovalId| match id.as_ref() {
        // Raised inside thread 7 of the desk channel.
        "appr-threaded" => Some(conv(Some("desk-finance"), Some(7))),
        // Raised straight in the same channel, outside any thread.
        "appr-flat" => Some(conv(Some("desk-finance"), None)),
        _ => None,
    };
    let in_thread = |chat: &str, parent: Option<u64>| CompanyEvent::OperatorMessage {
        mentions: Vec::new(),
        parent: parent.map(EventSeq::new),
        text: "pay the invoice".into(),
        by: None,
        chat: Some(chat.to_string()),
        deliverable: None,
        attachments: Vec::new(),
    };
    let resolved = |id: &str| CompanyEvent::ApprovalResolved {
        approval_id: ApprovalId::new(id),
        verdict: Verdict::Approve,
        by: Actor {
            kind: ActorKind::Operator,
            id: "owner".into(),
        },
    };

    // A message asked inside a thread names both keys.
    assert_eq!(
        cycle_conversation(
            &[in_thread("desk-finance", Some(7))],
            &[],
            approval_conversation
        ),
        conv(Some("desk-finance"), Some(7)),
    );
    // A message asked straight in the channel names only the channel —
    // the pre-#435 behaviour, which must not change.
    assert_eq!(
        cycle_conversation(
            &[in_thread("desk-finance", None)],
            &[],
            approval_conversation
        ),
        conv(Some("desk-finance"), None),
    );
    // A follow-up cycle inherits the thread as well as the channel, so a
    // second sign-off re-parks under the same root rather than flat.
    assert_eq!(
        cycle_conversation(&[resolved("appr-threaded")], &[], approval_conversation),
        conv(Some("desk-finance"), Some(7)),
    );
    assert_eq!(
        cycle_conversation(&[resolved("appr-flat")], &[], approval_conversation),
        conv(Some("desk-finance"), None),
    );
    // The same thread twice is not ambiguous.
    assert_eq!(
        cycle_conversation(
            &[
                in_thread("desk-finance", Some(7)),
                resolved("appr-threaded")
            ],
            &[],
            approval_conversation,
        ),
        conv(Some("desk-finance"), Some(7)),
    );

    // THE ASYMMETRY. One channel, two threads: the channel survives and
    // only the thread is dropped. Answering in the channel is exactly what
    // this batch did before #435, so the fallback is the old behaviour
    // rather than a new failure.
    assert_eq!(
        cycle_conversation(
            &[
                in_thread("desk-finance", Some(7)),
                in_thread("desk-finance", Some(9)),
            ],
            &[],
            approval_conversation,
        ),
        conv(Some("desk-finance"), None),
        "a thread disagreement must cost the thread, never the channel",
    );
    // Threaded batched with flat is the same disagreement, both orders.
    for batch in [
        [
            in_thread("desk-finance", Some(7)),
            in_thread("desk-finance", None),
        ],
        [
            in_thread("desk-finance", None),
            in_thread("desk-finance", Some(7)),
        ],
    ] {
        assert_eq!(
            cycle_conversation(&batch, &[], approval_conversation),
            conv(Some("desk-finance"), None),
        );
    }
    // Inheriting a thread that disagrees with the batch's own is the same
    // rule, one hop further out.
    assert_eq!(
        cycle_conversation(
            &[
                in_thread("desk-finance", Some(9)),
                resolved("appr-threaded"),
            ],
            &[],
            approval_conversation,
        ),
        conv(Some("desk-finance"), None),
    );

    // A channel disagreement still costs everything — #379's rule, intact.
    // The thread must not survive its own channel.
    assert_eq!(
        cycle_conversation(
            &[
                in_thread("desk-finance", Some(7)),
                in_thread("agent-cfo", Some(7)),
            ],
            &[],
            approval_conversation,
        ),
        ApprovalConversation::default(),
        "a parent without a channel is a sequence number with nothing to \
         resolve it against",
    );
    // And a rival trigger clears both keys, not just the channel.
    assert_eq!(
        cycle_conversation(
            &[
                in_thread("desk-finance", Some(7)),
                CompanyEvent::TaskDispatched {
                    task_id: "t-1".into(),
                    run_id: None,
                    origin_chat_id: None,
                    origin_parent: None,
                },
            ],
            &[],
            approval_conversation,
        ),
        ApprovalConversation::default(),
    );
}

#[tokio::test]
async fn send_email_sends_for_established_recipient() {
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
    rt.inbox()
        .append(
            rt.id(),
            &crate::ports::inbox::EmailRecord {
                id: "1".into(),
                inbox: "ceo".into(),
                from_name: "".into(),
                from_email: "known@ext.com".into(),
                subject: "hi".into(),
                body: "".into(),
                at_millis: 0,
                read: false,
                outbound: false,
            },
        )
        .await
        .unwrap();
    let host = CycleHostImpl::new(
        rt.id().clone(),
        "cyc-send".into(),
        &rt,
        None,
        false,
        ApprovalConversation::default(),
    );

    let res = host
        .send_email(serde_json::json!({ "to": "known@ext.com", "subject": "s", "body": "b" }))
        .await
        .unwrap();
    assert_eq!(res.output["status"], "sent");
    assert_eq!(sender.sent().len(), 1);
}

/// Issue #232: the established-correspondent gate must not weaken as the
/// inbox grows.
///
/// [`InboxStore::messages`] returns **oldest-first**, so the old
/// `messages(.., 500, 0)` scan only ever saw the 500 *oldest* messages.
/// Past that size every newer correspondent read as unknown, and every
/// reply to a real thread parked for approval — an approval queue nobody
/// can distinguish from noise is an approval queue everyone rubber-stamps.
///
/// So the correspondent here is filed **last**, past the old cap. Policy is
/// `full` (every effect executes) to isolate the flags on the effect from
/// the gate decision they feed: this asserts what the send path *believes*
/// about the recipient, not what the policy did with that belief.
#[tokio::test]
async fn established_recipient_past_the_old_page_cap_is_not_first_time() {
    let home_dir = tmp_home();
    let home = home_dir.path().to_path_buf();
    let sender = Arc::new(RecordingMailSender::new());
    let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
        .with_mail(CompanyMail {
            sender: sender.clone(),
            smtp: test_smtp("ceo@acme.test"),
        })
        .build()
        .await
        .unwrap();

    let file = async |id: usize, from: &str| {
        rt.inbox()
            .append(
                rt.id(),
                &crate::ports::inbox::EmailRecord {
                    id: format!("m{id}"),
                    inbox: "ceo".into(),
                    from_name: String::new(),
                    from_email: from.to_string(),
                    subject: "hi".into(),
                    body: String::new(),
                    at_millis: id as u64,
                    read: false,
                    outbound: false,
                },
            )
            .await
            .unwrap();
    };

    // 501 older messages from other people, so the real correspondent lands
    // at index 501 — one past the end of the old 500-message page.
    for i in 0..501 {
        file(i, &format!("filler{i}@ext.com")).await;
    }
    file(501, "known@ext.com").await;

    let host = CycleHostImpl::new(
        rt.id().clone(),
        "cyc-deep".into(),
        &rt,
        None,
        false,
        ApprovalConversation::default(),
    );
    let res = host
        .send_email(serde_json::json!({ "to": "known@ext.com", "subject": "s", "body": "b" }))
        .await
        .unwrap();
    assert_eq!(res.output["status"], "sent");

    let (executed, parked) = host.into_outcomes();
    assert!(parked.is_empty(), "an established thread must not park");
    let effect = executed
        .iter()
        .find(|e| e.kind == EMAIL_SEND_KIND)
        .expect("the send path emitted an email.send effect");
    assert!(
        effect.established_thread,
        "a correspondent who wrote in past message 500 is still established"
    );
    assert!(
        !effect.first_time_counterparty,
        "a correspondent who wrote in is never a first-time counterparty"
    );
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn spawn_task_arm_opens_a_board_card() {
    let home_dir = tmp_home();
    let home = home_dir.path().to_path_buf();
    let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
        .build()
        .await
        .unwrap();
    let host = CycleHostImpl::new(
        rt.id().clone(),
        "cyc".into(),
        &rt,
        None,
        false,
        ApprovalConversation::default(),
    );

    let res = host
        .spawn_task(serde_json::json!({ "title": "  Ship it ", "assignee": " eng " }))
        .await
        .unwrap();
    assert!(res.ok);
    assert_eq!(res.output["status"], "queued");

    let cards = rt.tasks().list(rt.id()).await.unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].title, "Ship it");
    assert_eq!(cards[0].assignee, "eng");
    // Intake lands in To-do, never on the dispatch edge: a spawned card
    // must not spend an agent turn before an operator has seen it.
    assert_eq!(cards[0].column, COLUMN_TODO);

    // A blank title is a clean tool error, no card.
    let bad = host
        .spawn_task(serde_json::json!({ "title": "  " }))
        .await
        .unwrap();
    assert!(!bad.ok);
    assert_eq!(rt.tasks().list(rt.id()).await.unwrap().len(), 1);
}
