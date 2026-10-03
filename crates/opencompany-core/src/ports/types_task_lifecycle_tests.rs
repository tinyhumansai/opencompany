use super::types_test_support::*;
use super::*;

/// #185: the `task_id` correlation key is additive in both directions —
/// an event journaled before it existed still loads, and an untagged event
/// still serializes byte-for-byte as it did before the field was added.
///
/// That second half is the migration-free guarantee: every already-persisted
/// `AgentReply` / `McpCallFailed` in every company's log must round-trip
/// unchanged, or the cross-backend export/import comparison breaks.
#[test]
fn task_id_correlation_is_additive_and_omitted_when_absent() {
    let legacy: CompanyEvent = serde_json::from_str(
        r#"{"kind":"AgentReply","chat_id":"main","agent_id":"ceo","text":"hi"}"#,
    )
    .expect("a pre-task_id AgentReply still loads");
    match &legacy {
        CompanyEvent::AgentReply { task_id, .. } => assert!(task_id.is_none()),
        other => panic!("expected AgentReply, got {other:?}"),
    }

    // An untagged reply keeps the legacy wire shape exactly.
    let untagged = CompanyEvent::AgentReply {
        audience: Vec::new(),
        mentions: Vec::new(),
        mention_depth: 0,
        parent: None,
        chat_id: "main".to_string(),
        agent_id: "ceo".to_string(),
        text: "hi".to_string(),
        steps: Vec::new(),
        task_id: None,
        outputs: Vec::new(),
        episode: None,
    };
    assert_eq!(
        serde_json::to_string(&untagged).unwrap(),
        r#"{"kind":"AgentReply","chat_id":"main","agent_id":"ceo","text":"hi"}"#
    );

    // A dispatch-produced reply carries the key and round-trips.
    let tagged = CompanyEvent::AgentReply {
        audience: Vec::new(),
        mentions: Vec::new(),
        mention_depth: 0,
        parent: None,
        chat_id: "t-1".to_string(),
        agent_id: "ceo".to_string(),
        text: "done".to_string(),
        steps: Vec::new(),
        task_id: Some("t-1".to_string()),
        outputs: Vec::new(),
        episode: None,
    };
    let back: CompanyEvent =
        serde_json::from_str(&serde_json::to_string(&tagged).unwrap()).unwrap();
    assert_eq!(back, tagged);

    // Same contract on the failure event.
    let legacy_mcp: CompanyEvent = serde_json::from_str(
        r#"{"kind":"McpCallFailed","server":"gh","tool":"issues","status":"credential_required","message":"needs auth"}"#,
    )
    .expect("a pre-task_id McpCallFailed still loads");
    match &legacy_mcp {
        CompanyEvent::McpCallFailed { task_id, .. } => assert!(task_id.is_none()),
        other => panic!("expected McpCallFailed, got {other:?}"),
    }
}

/// #185: the dispatch terminal round-trips, and reports where the card
/// landed so a stopped run is distinguishable from a successful one.
#[test]
fn desk_task_completed_round_trips() {
    let done = CompanyEvent::DeskTaskCompleted {
        task_id: "t-1".to_string(),
        desk: "ceo".to_string(),
        output: "shipped".to_string(),
        column: "in_review".to_string(),
        artifact_ids: Vec::new(),
        origin_chat_id: None,
        origin_parent: None,
    };
    let json = serde_json::to_string(&done).unwrap();
    assert!(json.contains(r#""kind":"DeskTaskCompleted""#));
    assert!(
        !json.contains("artifact_ids"),
        "a task that published nothing must add nothing to the log: {json}"
    );
    assert!(
        !json.contains("origin_chat_id"),
        "a board-created card names no conversation, so it must add nothing \
         to the log either: {json}"
    );
    let back: CompanyEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(back, done);
}

/// Issue #377: the terminal carries the conversation the card was raised
/// from, and a line written before the field existed still replays as
/// origin-less — which is the truth about it (nobody raised it from a chat
/// that this log records), not a default standing in for a lost id.
///
/// The legacy blob is asserted verbatim for the same reason #244's is: it
/// is exactly what is already on disk in every company's event log. If this
/// fails, the change needs a migration rather than a `#[serde(default)]`.
#[test]
fn desk_task_completed_carries_its_origin_chat_and_still_reads_the_old_shape() {
    let done = CompanyEvent::DeskTaskCompleted {
        task_id: "t-1".to_string(),
        desk: "engineer".to_string(),
        output: "shipped".to_string(),
        column: "in_review".to_string(),
        artifact_ids: Vec::new(),
        origin_chat_id: Some("engineering".to_string()),
        origin_parent: None,
    };
    let json = serde_json::to_string(&done).unwrap();
    assert!(json.contains(r#""origin_chat_id":"engineering""#), "{json}");
    assert_eq!(
        serde_json::from_str::<CompanyEvent>(&json).unwrap(),
        done,
        "the origin must survive the round trip"
    );

    // The responder and the channel are different words on purpose — this
    // is why the origin has to be carried rather than derived from `desk`.
    assert!(
        !json.contains(r#""desk":"engineering""#),
        "the responder is not the channel: {json}"
    );

    let legacy = r#"{"kind":"DeskTaskCompleted","task_id":"t-1","desk":"ceo","output":"shipped","column":"in_review"}"#;
    assert_eq!(
        serde_json::from_str::<CompanyEvent>(legacy).unwrap(),
        CompanyEvent::DeskTaskCompleted {
            task_id: "t-1".to_string(),
            desk: "ceo".to_string(),
            output: "shipped".to_string(),
            column: "in_review".to_string(),
            artifact_ids: Vec::new(),
            origin_chat_id: None,
            origin_parent: None,
        },
        "a pre-#377 journal line must replay with no origin, not fail"
    );
}

/// Issue #1890 B: the thread half of that origin round-trips, is skipped
/// when absent, and a line written before it existed replays as
/// channel-level — which is the truth about such a line, not a default
/// standing in for one.
#[test]
fn the_terminal_carries_the_thread_its_card_was_raised_in() {
    let threaded = CompanyEvent::DeskTaskCompleted {
        task_id: "t-1".to_string(),
        desk: "ceo".to_string(),
        output: "shipped".to_string(),
        column: "in_review".to_string(),
        artifact_ids: Vec::new(),
        origin_chat_id: Some("growth".to_string()),
        origin_parent: Some(EventSeq::new(41)),
    };
    let json = serde_json::to_string(&threaded).unwrap();
    assert!(json.contains(r#""origin_parent":41"#), "{json}");
    assert_eq!(
        serde_json::from_str::<CompanyEvent>(&json).unwrap(),
        threaded,
        "the root must survive the round trip"
    );

    // Skipped when absent, so an unthreaded settle is byte-identical to a
    // pre-B one and adds nothing to the log.
    let flat = CompanyEvent::DeskTaskCompleted {
        task_id: "t-1".to_string(),
        desk: "ceo".to_string(),
        output: "shipped".to_string(),
        column: "in_review".to_string(),
        artifact_ids: Vec::new(),
        origin_chat_id: Some("growth".to_string()),
        origin_parent: None,
    };
    let json = serde_json::to_string(&flat).unwrap();
    assert!(!json.contains("origin_parent"), "{json}");

    let legacy = r#"{"kind":"DeskTaskCompleted","task_id":"t-1","desk":"ceo","output":"shipped","column":"in_review","origin_chat_id":"growth"}"#;
    assert_eq!(
        serde_json::from_str::<CompanyEvent>(legacy).unwrap(),
        flat,
        "a pre-#1890-B line must replay as channel-level, not fail"
    );
}

/// Issue #244: the terminal anchor names what the run published, and a line
/// written before the field existed still replays.
///
/// The legacy blob is asserted verbatim because it is exactly what is
/// already on disk in every company's event log — if this ever fails, the
/// change needs a migration rather than a `#[serde(default)]`.
#[test]
fn desk_task_completed_carries_artifact_ids_and_still_reads_the_old_shape() {
    let done = CompanyEvent::DeskTaskCompleted {
        task_id: "t-1".to_string(),
        desk: "ceo".to_string(),
        output: "Drafted the launch spec.".to_string(),
        column: "in_review".to_string(),
        artifact_ids: vec!["art-1".to_string(), "art-2".to_string()],
        origin_chat_id: None,
        origin_parent: None,
    };
    let json = serde_json::to_string(&done).unwrap();
    assert!(
        json.contains(r#""artifact_ids":["art-1","art-2"]"#),
        "{json}"
    );
    assert_eq!(
        serde_json::from_str::<CompanyEvent>(&json).unwrap(),
        done,
        "the ids must survive the round trip"
    );

    let legacy = r#"{"kind":"DeskTaskCompleted","task_id":"t-1","desk":"ceo","output":"shipped","column":"in_review"}"#;
    assert_eq!(
        serde_json::from_str::<CompanyEvent>(legacy).unwrap(),
        CompanyEvent::DeskTaskCompleted {
            task_id: "t-1".to_string(),
            desk: "ceo".to_string(),
            output: "shipped".to_string(),
            column: "in_review".to_string(),
            artifact_ids: Vec::new(),
            origin_chat_id: None,
            origin_parent: None,
        },
        "a pre-#244 journal line must replay with no artifacts, not fail"
    );
}

/// Issue #364: a thread parent round-trips, and a message journaled before
/// threads existed still replays — as unparented, which is the truth about
/// it and not a default standing in for one.
///
/// The legacy blobs are asserted verbatim because they are exactly what is
/// already on disk in every company's log. A message that never was a thread
/// reply must serialize byte-for-byte as it always did, so export/import and
/// the cross-backend round-trip need no migration.
#[test]
fn a_thread_parent_round_trips_and_a_pre_thread_line_still_loads() {
    for legacy in [
        r#"{"kind":"OperatorMessage","text":"hi"}"#,
        r#"{"kind":"AgentReply","chat_id":"general","agent_id":"ceo","text":"hi"}"#,
    ] {
        let event: CompanyEvent = serde_json::from_str(legacy).unwrap();
        match &event {
            CompanyEvent::OperatorMessage { parent, .. }
            | CompanyEvent::AgentReply { parent, .. } => assert!(
                parent.is_none(),
                "a pre-#364 line was never a thread reply: {legacy}"
            ),
            other => panic!("unexpected variant: {other:?}"),
        }
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            legacy,
            "an unparented message must serialize exactly as it did before"
        );
    }

    let threaded = CompanyEvent::OperatorMessage {
        mentions: Vec::new(),
        parent: Some(EventSeq::new(41)),
        text: "a follow-up".into(),
        by: None,
        chat: Some("studio".into()),
        deliverable: None,
        attachments: Vec::new(),
    };
    let json = serde_json::to_string(&threaded).unwrap();
    assert!(json.contains(r#""parent":41"#), "{json}");
    assert_eq!(
        serde_json::from_str::<CompanyEvent>(&json).unwrap(),
        threaded
    );

    let answered = CompanyEvent::AgentReply {
        audience: Vec::new(),
        mentions: Vec::new(),
        mention_depth: 0,
        parent: Some(EventSeq::new(41)),
        task_id: None,
        outputs: Vec::new(),
        chat_id: "studio".into(),
        agent_id: "ceo".into(),
        text: "on it".into(),
        steps: Vec::new(),
        episode: None,
    };
    let json = serde_json::to_string(&answered).unwrap();
    assert!(json.contains(r#""parent":41"#), "{json}");
    assert_eq!(
        serde_json::from_str::<CompanyEvent>(&json).unwrap(),
        answered
    );
}

/// Issue #364: a reaction round-trips, and an unattributed one adds no
/// `by` key — the same additive contract every optional actor here keeps.
#[test]
fn a_reaction_round_trips() {
    let anonymous = CompanyEvent::ReactionToggled {
        message_seq: EventSeq::new(4),
        emoji: "👍".into(),
        on: true,
        by: None,
    };
    let json = serde_json::to_string(&anonymous).unwrap();
    assert_eq!(
        json,
        r#"{"kind":"ReactionToggled","message_seq":4,"emoji":"👍","on":true}"#
    );
    assert_eq!(
        serde_json::from_str::<CompanyEvent>(&json).unwrap(),
        anonymous
    );

    let attributed = CompanyEvent::ReactionToggled {
        message_seq: EventSeq::new(4),
        emoji: "🎉".into(),
        on: false,
        by: Some(Actor {
            kind: ActorKind::User,
            id: "u1".into(),
        }),
    };
    let json = serde_json::to_string(&attributed).unwrap();
    assert_eq!(
        serde_json::from_str::<CompanyEvent>(&json).unwrap(),
        attributed
    );
}

/// The three intents are exactly the three wire words, and only those
/// (issue #1152).
///
/// Pinned as literals because the console and the journal both write them:
/// a rename here silently stops matching every message already on disk.
#[test]
fn a_message_intent_round_trips_through_its_wire_word() {
    for (intent, word) in [
        (MessageIntent::Chat, "chat"),
        (MessageIntent::Once, "once"),
        (MessageIntent::Workflow, "workflow"),
    ] {
        let json = serde_json::to_string(&intent).unwrap();
        assert_eq!(json, format!("\"{word}\""));
        assert_eq!(
            serde_json::from_str::<MessageIntent>(&json).unwrap(),
            intent
        );
        assert_eq!(intent.as_str(), word);
    }
    assert!(
        serde_json::from_str::<MessageIntent>(r#""build""#).is_err(),
        "the set is closed: an unknown word is a 400, not a silent default"
    );
}

/// "Just chatting" has no deliverable, and that is the whole point of the
/// type (issue #1152).
///
/// A card can never *be* "not work", so the honest mapping from a `Chat`
/// message onto the card field is "there is no card" — `None` — rather than
/// a third `TaskDeliverable` variant every stored reader would owe a branch
/// for.
#[test]
fn only_a_work_intent_maps_onto_a_card_deliverable() {
    use crate::ports::tasks::TaskDeliverable;

    assert_eq!(MessageIntent::Chat.deliverable(), None);
    assert_eq!(
        MessageIntent::Once.deliverable(),
        Some(TaskDeliverable::Once)
    );
    assert_eq!(
        MessageIntent::Workflow.deliverable(),
        Some(TaskDeliverable::Workflow)
    );
    assert!(MessageIntent::Chat.is_chat());
    assert!(!MessageIntent::Once.is_chat());
    assert!(!MessageIntent::Workflow.is_chat());
}

/// **No journaled record migrates** (issue #1152).
///
/// Retyping `OperatorMessage::deliverable` from `TaskDeliverable` to
/// [`MessageIntent`] is only safe if every value already written under that
/// key still loads, and still writes back the same bytes. Getting this wrong
/// does not fail CI — it fails on somebody's event log, on whichever of the
/// three backends they run, the next time a company boots. So the claim is a
/// test rather than a sentence in a doc comment.
///
/// The blobs are asserted verbatim in both directions: parsed to the value
/// the new type gives them, and re-serialized byte-for-byte back to what is
/// on disk.
#[test]
fn every_journaled_deliverable_value_still_loads_and_writes_back_identically() {
    for (blob, expected) in [
        (r#"{"kind":"OperatorMessage","text":"hi"}"#, None),
        (
            r#"{"kind":"OperatorMessage","text":"ship the landing page","deliverable":"once"}"#,
            Some(MessageIntent::Once),
        ),
        (
            r#"{"kind":"OperatorMessage","text":"build me a weekly report","deliverable":"workflow"}"#,
            Some(MessageIntent::Workflow),
        ),
    ] {
        let event: CompanyEvent = serde_json::from_str(blob).unwrap_or_else(|e| {
            panic!("a stored line must still load: {blob} — {e}");
        });
        match &event {
            CompanyEvent::OperatorMessage { deliverable, .. } => {
                assert_eq!(*deliverable, expected, "{blob}")
            }
            other => panic!("unexpected variant: {other:?}"),
        }
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            blob,
            "a stored line must serialize back byte-for-byte"
        );
    }
}

/// The new word travels on the same key, and only when it was chosen
/// (issue #1152).
///
/// The absent case is the compatibility half that matters most: "Do it
/// once" is not the default *because it is sent* — it is the default
/// because nothing is sent, so an unmarked message is byte-identical on the
/// wire to every message journaled before this control existed.
#[test]
fn a_chat_intent_journals_under_the_same_key_and_absence_stays_absent() {
    let chatting = CompanyEvent::OperatorMessage {
        mentions: Vec::new(),
        text: "morning all".into(),
        by: None,
        chat: None,
        parent: None,
        deliverable: Some(MessageIntent::Chat),
        attachments: Vec::new(),
    };
    assert_eq!(
        serde_json::to_string(&chatting).unwrap(),
        r#"{"kind":"OperatorMessage","text":"morning all","deliverable":"chat"}"#
    );
    assert_eq!(
        serde_json::from_str::<CompanyEvent>(
            r#"{"kind":"OperatorMessage","text":"morning all","deliverable":"chat"}"#
        )
        .unwrap(),
        chatting
    );

    let unmarked = CompanyEvent::OperatorMessage {
        mentions: Vec::new(),
        text: "morning all".into(),
        by: None,
        chat: None,
        parent: None,
        deliverable: None,
        attachments: Vec::new(),
    };
    assert_eq!(
        serde_json::to_string(&unmarked).unwrap(),
        r#"{"kind":"OperatorMessage","text":"morning all"}"#,
        "no choice must still put nothing on the wire"
    );
}

#[test]
fn an_operator_message_journaled_before_attribution_still_loads() {
    // Exactly what is already on disk in every existing company's event
    // log. If this ever fails, the change needs a migration.
    let legacy = r#"{"kind":"OperatorMessage","text":"hi"}"#;
    let event: CompanyEvent = serde_json::from_str(legacy).unwrap();
    assert_eq!(
        event,
        CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            parent: None,
            text: "hi".into(),
            by: None,
            chat: None,
            deliverable: None,
            attachments: Vec::new(),
        }
    );
}

#[test]
fn an_unattributed_message_serializes_exactly_as_it_did_before() {
    // `skip_serializing_if` keeps the old bytes. This is what lets
    // export/import and the fs/sqlite/mongo round-trip stay green without
    // touching a single stored record.
    let event = CompanyEvent::OperatorMessage {
        mentions: Vec::new(),
        parent: None,
        text: "hi".into(),
        by: None,
        chat: None,
        deliverable: None,
        attachments: Vec::new(),
    };
    assert_eq!(
        serde_json::to_string(&event).unwrap(),
        r#"{"kind":"OperatorMessage","text":"hi"}"#
    );
}

#[test]
fn an_attributed_message_round_trips_with_its_actor() {
    let event = CompanyEvent::OperatorMessage {
        mentions: Vec::new(),
        parent: None,
        text: "hi".into(),
        by: Some(Actor {
            kind: ActorKind::User,
            id: "u1".into(),
        }),
        chat: None,
        deliverable: None,
        attachments: Vec::new(),
    };
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["by"]["kind"], "user");
    assert_eq!(json["by"]["id"], "u1");
    assert_eq!(serde_json::from_value::<CompanyEvent>(json).unwrap(), event);
}

#[test]
fn actor_kind_is_still_copy() {
    // A `String`-carrying variant would have taken this away from every
    // existing holder, which is why the User id lives on `Actor` instead.
    fn assert_copy(kind: ActorKind) -> (ActorKind, ActorKind) {
        (kind, kind)
    }
    let (a, b) = assert_copy(ActorKind::User);
    assert_eq!(a, b);
}

#[test]
fn company_event_variants_round_trip_tagged() {
    let events = vec![
        CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            parent: None,
            text: "hi".into(),
            by: None,
            chat: None,
            deliverable: None,
            attachments: Vec::new(),
        },
        CompanyEvent::WebhookReceived {
            channel: "email".into(),
            body: serde_json::json!({"subject": "hello"}),
        },
        CompanyEvent::ScheduleFired {
            cron: "0 9 * * *".into(),
            prompt: "daily standup".into(),
        },
        CompanyEvent::ApprovalResolved {
            approval_id: ApprovalId::new("a1"),
            verdict: Verdict::Approve,
            by: Actor {
                kind: ActorKind::Operator,
                id: "owner".into(),
            },
        },
        CompanyEvent::FeedbackFiled {
            note: "too slow".into(),
        },
        CompanyEvent::PaymentReceived {
            amount_usd: 25.0,
            memo: "invoice #1".into(),
        },
    ];
    for event in &events {
        assert_eq!(&round_trip(event), event);
    }

    // The tag field is emitted under `kind`.
    let json = serde_json::to_value(&events[0]).unwrap();
    assert_eq!(json["kind"], "OperatorMessage");
    assert_eq!(json["text"], "hi");
}

#[test]
fn mcp_call_failed_round_trips_and_is_byte_stable() {
    let event = CompanyEvent::McpCallFailed {
        task_id: None,
        server: "browserbase".into(),
        tool: "browse".into(),
        status: "tool_call_rejected".into(),
        message: "server rejected the call".into(),
    };
    assert_eq!(round_trip(&event), event);
    // The tag is emitted under `kind`, and the field set is fixed — a byte
    // guard so a later field addition is a deliberate, tested change.
    assert_eq!(
        serde_json::to_string(&event).unwrap(),
        r#"{"kind":"McpCallFailed","server":"browserbase","tool":"browse","status":"tool_call_rejected","message":"server rejected the call"}"#
    );
}

#[test]
fn task_steered_round_trips_and_omits_empty_fields() {
    // A plain pause: no `instruction`, no `by` — both must be OMITTED from
    // the wire (skip_serializing_if), so old logs stay byte-stable.
    let pause = CompanyEvent::TaskSteered {
        task_id: "t1".into(),
        action: "pause".into(),
        instruction: None,
        by: None,
    };
    assert_eq!(round_trip(&pause), pause);
    assert_eq!(
        serde_json::to_string(&pause).unwrap(),
        r#"{"kind":"TaskSteered","task_id":"t1","action":"pause"}"#
    );

    // A redirect carries its (capped) instruction; still no actor.
    let redirect = CompanyEvent::TaskSteered {
        task_id: "t1".into(),
        action: "redirect".into(),
        instruction: Some("focus on the API".into()),
        by: None,
    };
    assert_eq!(round_trip(&redirect), redirect);
    assert_eq!(
        serde_json::to_string(&redirect).unwrap(),
        r#"{"kind":"TaskSteered","task_id":"t1","action":"redirect","instruction":"focus on the API"}"#
    );
}

/// A retired event kind still loads. The journal is append-only, so a row
/// written by a build that knew `A2aTaskReceived` (the tiny.place inbound A2A
/// task, removed with tiny.place) must not make the whole log unreadable.
#[test]
fn a_retired_a2a_task_row_deserializes_as_unknown() {
    let legacy = r#"{"kind":"A2aTaskReceived","from":"@peer","task":{"skill":"seo.audit"}}"#;
    let event: CompanyEvent = serde_json::from_str(legacy).expect("legacy row loads");
    assert_eq!(event, CompanyEvent::Unknown);
    assert_eq!(event.kind(), "Unknown");
}

/// Any kind this build has never heard of — a newer host's event, a typo in a
/// hand-edited log — falls back the same way rather than failing the read.
#[test]
fn an_unrecognised_event_kind_deserializes_as_unknown() {
    let event: CompanyEvent =
        serde_json::from_str(r#"{"kind":"SomethingNew"}"#).expect("unknown kind loads");
    assert_eq!(event, CompanyEvent::Unknown);
}
