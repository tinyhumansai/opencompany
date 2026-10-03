use super::*;

/// Issue #259's two variants pin their wire shape the same way
/// `WorkflowCreated` does: `kind` + `workflow_id` + `name`, with `by`
/// omitted entirely when absent so the common unattributed line stays the
/// short one.
#[test]
fn workflow_updated_and_deleted_pin_their_wire_shape() {
    let updated = CompanyEvent::WorkflowUpdated {
        workflow_id: "digest".to_string(),
        name: "Daily digest".to_string(),
        by: None,
    };
    assert_eq!(
        serde_json::to_string(&updated).expect("serialize"),
        r#"{"kind":"WorkflowUpdated","workflow_id":"digest","name":"Daily digest"}"#
    );

    let deleted = CompanyEvent::WorkflowDeleted {
        workflow_id: "digest".to_string(),
        name: "Daily digest".to_string(),
        by: None,
    };
    assert_eq!(
        serde_json::to_string(&deleted).expect("serialize"),
        r#"{"kind":"WorkflowDeleted","workflow_id":"digest","name":"Daily digest"}"#
    );

    // Both round-trip.
    for event in [updated, deleted] {
        let line = serde_json::to_string(&event).expect("serialize");
        let back: CompanyEvent = serde_json::from_str(&line).expect("deserialize");
        assert_eq!(back, event);
    }
}

/// The graph body must never reach the journal — see the variant docs. A
/// reader of the shared append-only log (operator SSE, the inference
/// sidecar) has no business seeing agent prompts or destination addresses,
/// and the only way a body could leak here is someone adding a field.
#[test]
fn workflow_updated_carries_no_graph_body() {
    let line = serde_json::to_string(&CompanyEvent::WorkflowUpdated {
        workflow_id: "digest".to_string(),
        name: "Daily digest".to_string(),
        by: None,
    })
    .expect("serialize");
    assert!(!line.contains("toml"), "{line}");
    assert!(!line.contains("node"), "{line}");
    assert!(!line.contains("graph"), "{line}");
}

/// **The backcompat proof.** A journal written before this variant existed
/// still loads, line for line, and every one of those lines re-serializes
/// byte-identically — which is what "additive, no migration" actually
/// claims. Adding an enum variant cannot change how a sibling serializes,
/// but nothing else in the suite asserts it for the whole log, and a
/// regression here would corrupt an export/import round trip silently.
#[test]
fn a_journal_written_before_this_variant_still_loads_byte_identically() {
    // Verbatim lines in the pre-#228 on-disk shapes, including the pre-`by`
    // / pre-`chat` `OperatorMessage` and the pre-`steps` `AgentReply`.
    let legacy = [
        r#"{"kind":"OperatorMessage","text":"ship it"}"#,
        r#"{"kind":"AgentReply","chat_id":"general","agent_id":"ceo","text":"on it"}"#,
        r#"{"kind":"ScheduleFired","cron":"0 9 * * *","prompt":"daily"}"#,
        r#"{"kind":"WorkflowCreated","workflow_id":"digest","name":"Digest"}"#,
        r#"{"kind":"TaskDispatched","task_id":"t-1"}"#,
        r#"{"kind":"DeskTaskCompleted","task_id":"t-1","desk":"ceo","output":"done","column":"in_review"}"#,
    ];
    for line in legacy {
        let event: CompanyEvent = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("pre-#228 journal line must still load: {line} — {e}"));
        let again = serde_json::to_string(&event).expect("serialize");
        assert_eq!(again, line, "pre-#228 line must re-serialize unchanged");
    }
}

/// Issue #242: an effect remembers which task attempt produced it, and the
/// field is additive in the same way `Effect::agent` was — a journal line
/// written before it existed replays as `None` (no run correlation, the
/// pre-#242 behaviour) rather than failing to parse and taking the whole
/// approval queue down with it on replay.
#[test]
fn effect_run_id_round_trips_and_a_legacy_line_replays_as_none() {
    let mut effect = Effect {
        kind: "composio.execute".to_string(),
        group: EffectGroup::Other,
        amount_usd: None,
        established_thread: false,
        first_time_counterparty: false,
        payload: serde_json::json!({ "tool": "GMAIL_SEND_EMAIL" }),
        agent: Some("finance".to_string()),
        run_id: None,
    };
    let untagged = serde_json::to_string(&effect).expect("serialize");
    assert!(
        !untagged.contains("run_id"),
        "an untagged effect's wire form must be unchanged: {untagged}"
    );

    effect.run_id = Some("run-7".to_string());
    let tagged = serde_json::to_string(&effect).expect("serialize");
    assert!(tagged.contains(r#""run_id":"run-7""#), "{tagged}");
    assert_eq!(
        effect,
        serde_json::from_str::<Effect>(&tagged).expect("round trip")
    );

    // The pre-#242 line: same bytes, no field.
    let legacy: Effect = serde_json::from_str(&untagged).expect("legacy effect must load");
    assert_eq!(legacy.run_id, None);
    assert_eq!(
        legacy.agent.as_deref(),
        Some("finance"),
        "the earlier additive field must still be read alongside the new one"
    );
}

/// Issue #242: the run id rides the dispatch event, and it is additive in
/// both directions — a tagged dispatch round-trips it, and an untagged one
/// serializes exactly the shape a pre-#242 journal holds (asserted verbatim
/// above too, but here against the *writer* rather than the reader).
#[test]
fn task_dispatched_carries_its_run_id_without_changing_the_untagged_shape() {
    let untagged = CompanyEvent::TaskDispatched {
        task_id: "t-1".to_string(),
        run_id: None,
    };
    assert_eq!(
        serde_json::to_string(&untagged).expect("serialize"),
        r#"{"kind":"TaskDispatched","task_id":"t-1"}"#
    );

    let tagged = CompanyEvent::TaskDispatched {
        task_id: "t-1".to_string(),
        run_id: Some("run-7".to_string()),
    };
    let line = serde_json::to_string(&tagged).expect("serialize");
    assert!(line.contains(r#""run_id":"run-7""#), "{line}");
    assert_eq!(
        tagged,
        serde_json::from_str::<CompanyEvent>(&line).expect("round trip")
    );

    // A legacy line loads as an untagged dispatch rather than failing.
    let legacy: CompanyEvent =
        serde_json::from_str(r#"{"kind":"TaskDispatched","task_id":"t-1"}"#).expect("legacy");
    assert_eq!(legacy, untagged);
}

/// Issue #1682: an attachment round-trips on an `OperatorMessage`, and an
/// empty list serializes *away* — the additive shape that makes the field
/// zero-migration, on exactly the terms `mentions` / `deliverable` proved
/// for themselves above.
#[test]
fn operator_message_attachments_round_trip_and_skip_when_empty() {
    // Empty is absent: a message with no attachment serializes byte-for-byte
    // as it did before the field existed.
    let bare = CompanyEvent::OperatorMessage {
        text: "hi".into(),
        by: None,
        chat: None,
        parent: None,
        deliverable: None,
        mentions: Vec::new(),
        attachments: Vec::new(),
    };
    assert_eq!(
        serde_json::to_string(&bare).unwrap(),
        r#"{"kind":"OperatorMessage","text":"hi"}"#,
        "an empty attachment list must not appear on the wire"
    );

    // A carried attachment survives the round trip with every field intact.
    let carried = CompanyEvent::OperatorMessage {
        text: "see attached".into(),
        by: None,
        chat: None,
        parent: None,
        deliverable: None,
        mentions: Vec::new(),
        attachments: vec![Attachment {
            node_id: "node-1".into(),
            name: "diagram.png".into(),
            mime: "image/png".into(),
            size: 2048,
            extracted_text: None,
        }],
    };
    let json = serde_json::to_string(&carried).unwrap();
    assert!(json.contains(r#""nodeId":"node-1""#), "{json}");
    assert!(json.contains(r#""mime":"image/png""#), "{json}");
    let back: CompanyEvent = serde_json::from_str(&json).unwrap();
    match back {
        CompanyEvent::OperatorMessage { attachments, .. } => {
            assert_eq!(attachments.len(), 1);
            assert_eq!(attachments[0].name, "diagram.png");
            assert_eq!(attachments[0].size, 2048);
        }
        other => panic!("expected OperatorMessage, got {other:?}"),
    }

    // A pre-#1682 record with no `attachments` key still loads, as an empty
    // list — the `#[serde(default)]` half of the contract.
    let legacy = r#"{"kind":"OperatorMessage","text":"hi"}"#;
    match serde_json::from_str::<CompanyEvent>(legacy).unwrap() {
        CompanyEvent::OperatorMessage { attachments, .. } => assert!(attachments.is_empty()),
        other => panic!("expected OperatorMessage, got {other:?}"),
    }
}

/// Codex review finding on #1682, round 2: `extracted_text` is a later
/// addition to `Attachment` itself, so it needs the identical
/// omit-when-absent / default-on-load contract `attachments` got above —
/// a record journaled by the first round of the fix (a reference with no
/// extracted text) must still load, and a `None` must not put a stray key
/// on the wire.
#[test]
fn attachment_extracted_text_round_trips_and_skips_when_absent() {
    let no_text = Attachment {
        node_id: "node-1".into(),
        name: "photo.png".into(),
        mime: "image/png".into(),
        size: 2048,
        extracted_text: None,
    };
    let json = serde_json::to_string(&no_text).unwrap();
    assert!(
        !json.contains("extractedText"),
        "no extracted text must not appear on the wire: {json}"
    );
    assert_eq!(serde_json::from_str::<Attachment>(&json).unwrap(), no_text);

    let with_text = Attachment {
        node_id: "node-2".into(),
        name: "report.pdf".into(),
        mime: "application/pdf".into(),
        size: 4096,
        extracted_text: Some("Q3 revenue grew 12%.".to_string()),
    };
    let json = serde_json::to_string(&with_text).unwrap();
    assert!(
        json.contains(r#""extractedText":"Q3 revenue grew 12%.""#),
        "{json}"
    );
    assert_eq!(
        serde_json::from_str::<Attachment>(&json).unwrap(),
        with_text
    );

    // A round 1 record (the reference alone, no `extractedText` key) still
    // loads, defaulting to `None` — the same contract `attachments` itself
    // got when it was added onto `OperatorMessage`.
    let round_one = r#"{"nodeId":"node-3","name":"old.png","mime":"image/png","size":10}"#;
    let loaded: Attachment = serde_json::from_str(round_one).unwrap();
    assert_eq!(loaded.extracted_text, None);
}
