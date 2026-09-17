use super::types_test_support::*;
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
        origin_chat_id: None,
        origin_parent: None,
    };
    assert_eq!(
        serde_json::to_string(&untagged).expect("serialize"),
        r#"{"kind":"TaskDispatched","task_id":"t-1"}"#
    );

    let tagged = CompanyEvent::TaskDispatched {
        task_id: "t-1".to_string(),
        run_id: Some("run-7".to_string()),
        origin_chat_id: None,
        origin_parent: None,
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

#[test]
fn legacy_agent_card_json_deserializes_with_defaults() {
    // A card written by an earlier phase carried only three fields; the new
    // `#[serde(default)]` fields must fill in without error.
    let json = r#"{"handle":"acme","description":"d","skills":["a"]}"#;
    let card: AgentCard = serde_json::from_str(json).expect("deserialize legacy card");
    assert_eq!(card.handle, "acme");
    assert!(card.name.is_empty());
    assert!(card.payment_requirements.is_empty());
    assert!(card.supported_interfaces.is_empty());
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

/// Issue #1781 review (Codex P2): a grandfathered manifest teammate at the
/// literal id `operator` diverts the durable system feed to
/// `OPERATOR_CHANNEL_COLLISION_FALLBACK` (see `operator_feed_channel`
/// above). Retiring that teammate must not flip the feed back onto
/// `OPERATOR_CHANNEL` — the tombstone in `overlay_retired_agents` is
/// permanent (manifest removal always goes through `retire_agent`, never a
/// TOML rewrite), so the reports already journaled under the fallback
/// address would be orphaned from `/desks` and the retired teammate's own
/// historical DM rows (`chat_id == "operator"`) would start bleeding into
/// the "new" system feed the moment the id looked free again.
#[test]
fn operator_feed_channel_stays_diverted_after_the_collision_is_retired() {
    let manifest = "[company]\nname = \"Acme\"\n\
         [[agent]]\nid = \"operator\"\nrole = \"Chief of Staff\"\n";
    let mut record = desk_record(manifest, Vec::new());
    assert_eq!(
        record.operator_feed_channel(),
        crate::runtime::OPERATOR_CHANNEL_COLLISION_FALLBACK,
        "fixture must start in the collision state this test exercises"
    );

    record.retire_agent(crate::runtime::OPERATOR_CHANNEL);
    assert!(!record.is_roster_agent(crate::runtime::OPERATOR_CHANNEL));
    assert_eq!(
        record.operator_feed_channel(),
        crate::runtime::OPERATOR_CHANNEL_COLLISION_FALLBACK,
        "the feed address must stay stable once anything has ever held the \
         `operator` id — flipping back to OPERATOR_CHANNEL would orphan the \
         fallback's existing reports and resurface the retired teammate's \
         own DM history in the system feed"
    );
}

/// Issue #1781 review, Codex P2 follow-up: a direct, focused test of
/// `divert_operator_feed_permanently`/`is_operator_feed_diverted`
/// themselves, isolated from the HTTP route the desk- and teammate-
/// deletion regression tests exercise them through.
///
/// The specific risk this closes: `divert_operator_feed_permanently`
/// tombstones through `retire_agent`, keyed on
/// `OPERATOR_CHANNEL_COLLISION_FALLBACK` ("operator-feed") — a string
/// that fails the manifest agent-id format rule on its hyphen alone. If
/// `retire_agent` ever grew id validation (it does not today — it is a
/// bare idempotent push), that key would be silently rejected,
/// `is_operator_feed_diverted` would always read `false`, and the
/// tombstone this whole fix depends on would be a no-op with nothing
/// here to notice. Calling it on a record with **no live collision at
/// all** isolates exactly that: nothing but the divert call itself
/// explains the fallback staying live.
#[test]
fn divert_operator_feed_permanently_sticks_with_no_live_collision() {
    let manifest = "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n";
    let mut record = desk_record(manifest, Vec::new());
    assert_eq!(
        record.operator_feed_channel(),
        crate::runtime::OPERATOR_CHANNEL,
        "fixture must start on the literal address — nothing here collides \
         with `operator` yet"
    );
    assert!(!record.is_operator_feed_diverted());

    record.divert_operator_feed_permanently();

    assert!(
        record.is_operator_feed_diverted(),
        "the tombstone must read back as set immediately after the call"
    );
    assert_eq!(
        record.operator_feed_channel(),
        crate::runtime::OPERATOR_CHANNEL_COLLISION_FALLBACK,
        "operator_feed_channel must divert on the tombstone alone, with no \
         live desk/agent collision in the record at all — proving \
         `retire_agent` actually accepted the hyphenated fallback key \
         rather than silently rejecting it"
    );

    // Idempotent, like `retire_agent` itself: calling it again must not
    // duplicate the tombstone or otherwise change the outcome.
    record.divert_operator_feed_permanently();
    assert_eq!(
        record.overlay_retired_agents.len(),
        1,
        "a second call must not push a duplicate tombstone entry"
    );
}

/// The third grandfather case (PR #1781 review, CodeRabbit): a real
/// **desk** already owning `operator` must divert the feed exactly like
/// the roster-teammate case above, not stay on the literal id. Left on
/// `OPERATOR_CHANNEL`, the feed's id equals the desk's own id, and two
/// surfaces collide on it: `server::operator::operator_channel` hands
/// that id to the console as the pinned Operator row, appended (`
/// operatorSection`, `frontend/src/views/ChatView.tsx`) *after* the desk
/// section `buildChannels` already put the same id in — so `findChannel`,
/// which returns the first section match, resolves the pinned row to the
/// desk every time. And `send_to_channel_adapter` journals each workflow
/// report under `operator_feed_channel()`'s result, so with no divert
/// those reports land in `chat_id == "operator"` too — the desk's own
/// ordinary transcript, not a distinguishable feed.
#[test]
fn operator_feed_channel_diverts_off_a_grandfathered_desks_own_operator_line() {
    let manifest = "[company]\nname = \"Acme\"\n\
         [[group_chat]]\nid = \"operator\"\nname = \"Operator Desk\"\nmembers = []\n";
    let record = desk_record(manifest, Vec::new());
    assert!(record.desk_exists(crate::runtime::OPERATOR_CHANNEL));
    assert!(!record.is_roster_agent(crate::runtime::OPERATOR_CHANNEL));
    assert_eq!(
        record.operator_feed_channel(),
        crate::runtime::OPERATOR_CHANNEL_COLLISION_FALLBACK,
        "a desk already owning `operator` must divert the feed off that \
         same address, the same way a roster teammate holding it does — \
         otherwise the pinned Operator row and the desk share one id and \
         `findChannel` always resolves it to the desk"
    );
}

/// PR #1781 review follow-up (Codex P2, second pass): a desk grandfathered
/// at a harmless id but the display name `Operator` must divert the feed
/// exactly like the same-id case above — `desk_exists` alone (id-only)
/// missed it. `from_path_for_reload` already admits this exact shape
/// (`from_path_for_reload_grandfathers_a_group_chat_named_operator` in
/// `company::manifest`), and `server::operator::resolve_desk` matches a
/// `?desk=operator` selector by name as readily as by id, so the pinned
/// console row would resolve to this desk's own transcript instead of the
/// system feed if the divert never fired.
#[test]
fn operator_feed_channel_diverts_off_a_grandfathered_desks_own_operator_name() {
    let manifest = "[company]\nname = \"Acme\"\n\
         [[group_chat]]\nid = \"legacy_ops\"\nname = \"Operator\"\nmembers = []\n";
    let record = desk_record(manifest, Vec::new());
    assert!(
        !record.desk_exists(crate::runtime::OPERATOR_CHANNEL),
        "fixture must actually be in the id-is-free, name-collides state \
         this test exercises, or it is not distinguishing this case from \
         `operator_feed_channel_diverts_off_a_grandfathered_desks_own_operator_line`"
    );
    assert!(!record.is_roster_agent(crate::runtime::OPERATOR_CHANNEL));
    assert_eq!(
        record.operator_feed_channel(),
        crate::runtime::OPERATOR_CHANNEL_COLLISION_FALLBACK,
        "a desk named \"Operator\" must divert the feed off that address \
         even though its id is free — `resolve_desk` shadows by name too, \
         so the pinned Operator row would otherwise resolve to this \
         desk's own transcript"
    );
}

/// PR #1781 review follow-up (CodeRabbit P2): a double legacy collision —
/// one desk shadowing the primary `operator` address *and a second,
/// different* desk shadowing the collision-fallback's own display name
/// ("operator-feed") — leaves `operator_feed_channel` with nowhere safe
/// left to divert to. `316bc9229` and `16dcce235` block both names from
/// ever being (re-)created going forward, so this fixture only models a
/// manifest hand-edited outside those guards and reloaded via
/// `from_path_for_reload`, the same grandfathering the single-collision
/// cases above rely on.
///
/// `operator_feed_channel_fallback_shadowed` exists precisely so this
/// residual gap is detectable rather than silent — asserted here directly
/// since the logging it drives (`workflows::delivery::send_to_channel_adapter`)
/// has no return value to assert on.
#[test]
fn operator_feed_channel_fallback_shadowed_detects_a_double_collision() {
    let manifest = "[company]\nname = \"Acme\"\n\
         [[group_chat]]\nid = \"legacy_ops\"\nname = \"Operator\"\nmembers = []\n\
         [[group_chat]]\nid = \"ops2\"\nname = \"operator-feed\"\nmembers = []\n";
    let record = desk_record(manifest, Vec::new());
    assert_eq!(
        record.operator_feed_channel(),
        crate::runtime::OPERATOR_CHANNEL_COLLISION_FALLBACK,
        "the primary collision alone still diverts to the fallback address \
         — this fixture must reach the same divert as the single-collision \
         case above before the double-collision check means anything"
    );
    assert!(
        record.operator_feed_channel_fallback_shadowed(),
        "a second desk named \"operator-feed\" shadows the fallback the \
         same way the first desk shadows the primary — `resolve_desk` \
         would fold a `?desk=operator-feed` read onto that second desk \
         instead of the system feed, and this predicate must catch it"
    );
}

/// Sibling to the double-collision case above: a fallback-name collision
/// with **no** primary collision must not trip the predicate — the divert
/// never fires, so the fallback address was never actually depended on.
#[test]
fn operator_feed_channel_fallback_shadowed_is_false_without_a_primary_collision() {
    let manifest = "[company]\nname = \"Acme\"\n\
         [[group_chat]]\nid = \"ops2\"\nname = \"operator-feed\"\nmembers = []\n";
    let record = desk_record(manifest, Vec::new());
    assert_eq!(
        record.operator_feed_channel(),
        crate::runtime::OPERATOR_CHANNEL,
        "no primary collision exists in this fixture, so the feed must \
         stay on the literal `operator` address"
    );
    assert!(
        !record.operator_feed_channel_fallback_shadowed(),
        "the fallback address is never consulted unless the feed actually \
         diverted to it"
    );
}
