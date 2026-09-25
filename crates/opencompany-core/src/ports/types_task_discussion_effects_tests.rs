use super::types_test_support::*;
use super::*;

/// Issue #335: an unattributed post must serialize with **no** `by` key, so
/// the variant's wire shape is the same one a machine-credentialled post
/// wrote before attribution could ever be present — and an attributed one
/// round-trips its actor.
#[test]
fn task_discussion_posted_round_trips_and_omits_an_absent_actor() {
    let anonymous = CompanyEvent::TaskDiscussionPosted {
        task_id: "t1".into(),
        text: "blocked on the API key".into(),
        by: None,
    };
    assert_eq!(round_trip(&anonymous), anonymous);
    assert_eq!(
        serde_json::to_string(&anonymous).unwrap(),
        r#"{"kind":"TaskDiscussionPosted","task_id":"t1","text":"blocked on the API key"}"#
    );

    let attributed = CompanyEvent::TaskDiscussionPosted {
        task_id: "t1".into(),
        text: "unblocked".into(),
        by: Some(Actor {
            kind: ActorKind::User,
            id: "u-7".into(),
        }),
    };
    assert_eq!(round_trip(&attributed), attributed);
}

/// Issue #358: the tombstone's wire shape, pinned because it is written
/// into `events.jsonl` and read back by a *different* instance on import.
/// The pair (post, tombstone) is what stops a withdrawn message being
/// resurrected, so a tombstone that failed to round-trip would silently
/// restore the text it was appended to remove.
#[test]
fn task_discussion_redacted_round_trips_and_omits_an_absent_actor() {
    let anonymous = CompanyEvent::TaskDiscussionRedacted {
        task_id: "t1".into(),
        seq: 42,
        by: None,
    };
    assert_eq!(round_trip(&anonymous), anonymous);
    assert_eq!(
        serde_json::to_string(&anonymous).unwrap(),
        r#"{"kind":"TaskDiscussionRedacted","task_id":"t1","seq":42}"#
    );

    let attributed = CompanyEvent::TaskDiscussionRedacted {
        task_id: "t1".into(),
        seq: 42,
        by: Some(Actor {
            kind: ActorKind::User,
            id: "u-7".into(),
        }),
    };
    assert_eq!(round_trip(&attributed), attributed);
}

#[test]
fn verdict_serializes_lowercase() {
    assert_eq!(
        serde_json::to_string(&Verdict::Approve).unwrap(),
        "\"approve\""
    );
    assert_eq!(serde_json::to_string(&Verdict::Deny).unwrap(), "\"deny\"");
    assert_eq!(
        serde_json::from_str::<Verdict>("\"approve\"").unwrap(),
        Verdict::Approve
    );
}

#[test]
fn effect_round_trips_and_accessors_read_fields() {
    let effect = Effect {
        kind: "payment.send".into(),
        group: EffectGroup::Spend,
        amount_usd: Some(42.5),
        established_thread: true,
        first_time_counterparty: false,
        payload: serde_json::json!({"to": "@vendor"}),
        agent: None,
        run_id: None,
    };
    let back = round_trip(&effect);
    assert_eq!(back, effect);
    assert_eq!(effect.kind(), "payment.send");
    assert_eq!(effect.group(), EffectGroup::Spend);
    assert_eq!(effect.amount_usd(), Some(42.5));
    assert!(effect.is_established_thread());
    assert!(!effect.is_first_time_counterparty());
}

#[test]
fn effect_disposition_round_trips() {
    for disp in [
        EffectDisposition::Executed,
        EffectDisposition::PendingApproval(ApprovalId::new("x")),
        EffectDisposition::Denied {
            reason: "over cap".into(),
        },
    ] {
        assert_eq!(round_trip(&disp), disp);
    }
}

#[test]
fn policy_decision_round_trips() {
    for dec in [
        PolicyDecision::Allow,
        PolicyDecision::RequireApproval,
        PolicyDecision::Deny,
    ] {
        assert_eq!(round_trip(&dec), dec);
    }
}

#[test]
fn event_seq_orders_numerically() {
    assert!(EventSeq::new(1) < EventSeq::new(2));
    assert_eq!(EventSeq::new(7).value(), 7);
}

#[test]
fn agent_card_round_trips_with_extended_fields() {
    let card = AgentCard {
        handle: "acme".into(),
        description: "We audit SEO.".into(),
        skills: vec!["seo.audit".into()],
        name: "Acme SEO".into(),
        actor_type: "agent".into(),
        endpoint: "https://host/a2a/acme".into(),
        supported_interfaces: vec!["a2a-jsonrpc".into()],
        capabilities: vec!["seo.audit".into()],
        tags: vec!["seo.audit".into()],
        payment_requirements: vec![CardPayment {
            skill_id: "seo.audit".into(),
            price: "25.00".into(),
            asset: "USDC".into(),
            network: "solana".into(),
        }],
    };
    assert_eq!(round_trip(&card), card);
}

fn desk_record(toml_src: &str, overlay: Vec<OverlayDeskMember>) -> CompanyRecord {
    CompanyRecord {
        overlay_desk_hive: Vec::new(),
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        id: CompanyId::new("acme"),
        manifest: toml::from_str(toml_src).expect("parse manifest"),
        ledger: Vec::new(),
        lifecycle: "running".to_string(),
        overlay_agents: Vec::new(),
        overlay_desk_members: overlay,
        overlay_desk_order: Vec::new(),
        overlay_desks: Vec::new(),
        overlay_workflows: Vec::new(),
        overlay_budgets: Vec::new(),
        overlay_policy: None,
        overlay_tool_grants: None,
        overlay_desk_tools: Default::default(),
        disabled_workflows: Vec::new(),
        template_provenance: None,
        setup: None,
        name_confirmed: false,
        activation_completed_at: None,
        created_at_millis: None,
    }
}

/// Like [`desk_record`] but with an explicit per-desk order overlay, for the
/// desk-hierarchy tests.
fn desk_record_ordered(
    toml_src: &str,
    overlay: Vec<OverlayDeskMember>,
    order: Vec<OverlayDeskOrder>,
) -> CompanyRecord {
    let mut record = desk_record(toml_src, overlay);
    record.overlay_desk_order = order;
    record
}

#[test]
fn desk_hive_overrides_precede_manifest_and_are_replaced_or_cleared() {
    let manifest = "[company]\nname = \"Acme\"\n\
         [[group_chat]]\nid = \"studio\"\nname = \"Studio\"\nmembers = []\n\
         [group_chat.routing]\nround_width = 2\n";
    let mut record = desk_record(manifest, Vec::new());

    // The manifest wins where no edit exists, and an unknown desk falls
    // through to the default rather than borrowing another desk's table.
    assert_eq!(record.effective_desk_hive("studio").round_width, Some(2));
    assert_eq!(
        record.effective_desk_hive("unknown"),
        crate::hive::routing::RoutingConfig::default()
    );
    assert!(!record.desk_hive_is_installed("studio"));

    let first = crate::hive::routing::RoutingConfig {
        round_width: Some(1),
        ..Default::default()
    };
    record.upsert_desk_hive(DeskHiveOverride {
        desk_id: "studio".into(),
        hive: first,
    });
    assert!(record.desk_hive_is_installed("studio"));
    assert_eq!(record.effective_desk_hive("studio").round_width, Some(1));

    let replacement = crate::hive::routing::RoutingConfig {
        round_width: Some(3),
        ..Default::default()
    };
    record.upsert_desk_hive(DeskHiveOverride {
        desk_id: "studio".into(),
        hive: replacement,
    });
    assert_eq!(record.overlay_desk_hive.len(), 1);
    assert_eq!(record.effective_desk_hive("studio").round_width, Some(3));
    assert!(record.clear_desk_hive("studio"));
    assert!(!record.clear_desk_hive("studio"));
    assert_eq!(record.effective_desk_hive("studio").round_width, Some(2));
}

/// The effective membership is the manifest members first, then overlay
/// additions in insertion order, deduplicated — the shared rule the REST
/// list and the harness desk-lead resolver both read.
#[test]
fn effective_desk_members_unions_manifest_and_overlay_deduped() {
    let manifest = "[company]\nname = \"Acme\"\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n\
         [[agent]]\nid = \"eng\"\nrole = \"Engineer\"\n\
         [[group_chat]]\nid = \"studio\"\nname = \"Studio\"\nmembers = [\"ceo\"]\n";
    let record = desk_record(
        manifest,
        vec![
            OverlayDeskMember {
                desk_id: "studio".into(),
                agent_id: "eng".into(),
            },
            // A duplicate of a manifest member is not added twice.
            OverlayDeskMember {
                desk_id: "studio".into(),
                agent_id: "ceo".into(),
            },
            // An addition for a different desk is ignored here.
            OverlayDeskMember {
                desk_id: "other".into(),
                agent_id: "eng".into(),
            },
        ],
    );
    assert_eq!(
        record.effective_desk_members("studio"),
        vec!["ceo".to_string(), "eng".to_string()]
    );
    // An unknown desk with only an overlay addition still resolves it.
    assert_eq!(
        record.effective_desk_members("other"),
        vec!["eng".to_string()]
    );
}

/// A three-member manifest desk whose order override permutes the members.
const HIERARCHY_MANIFEST: &str = "[company]\nname = \"Acme\"\n\
     [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n\
     [[agent]]\nid = \"eng\"\nrole = \"Engineer\"\n\
     [[agent]]\nid = \"des\"\nrole = \"Designer\"\n\
     [[group_chat]]\nid = \"studio\"\nname = \"Studio\"\nmembers = [\"ceo\", \"eng\", \"des\"]\n";

fn order(desk: &str, ids: &[&str]) -> Vec<OverlayDeskOrder> {
    vec![OverlayDeskOrder {
        desk_id: desk.into(),
        ordered: ids.iter().map(|s| s.to_string()).collect(),
    }]
}

/// A full permutation reorders the manifest members exactly as given.
#[test]
fn desk_order_reorders_manifest_members() {
    let record = desk_record_ordered(
        HIERARCHY_MANIFEST,
        Vec::new(),
        order("studio", &["des", "ceo", "eng"]),
    );
    assert_eq!(
        record.effective_desk_members("studio"),
        vec!["des".to_string(), "ceo".to_string(), "eng".to_string()]
    );
}

/// The override can promote an overlay-added member above manifest members —
/// the whole-set permutation a per-member rank could not express.
#[test]
fn desk_order_promotes_overlay_member_to_lead() {
    let record = desk_record_ordered(
        HIERARCHY_MANIFEST,
        vec![OverlayDeskMember {
            desk_id: "studio".into(),
            agent_id: "cto".into(),
        }],
        order("studio", &["cto", "ceo", "eng", "des"]),
    );
    let members = record.effective_desk_members("studio");
    assert_eq!(members[0], "cto");
    assert_eq!(
        members,
        vec![
            "cto".to_string(),
            "ceo".to_string(),
            "eng".to_string(),
            "des".to_string()
        ]
    );
}

/// An absent or empty override reproduces the base order byte-for-byte.
#[test]
fn desk_order_absent_or_empty_keeps_base_order() {
    let base = desk_record(HIERARCHY_MANIFEST, Vec::new());
    let base_members = base.effective_desk_members("studio");
    assert_eq!(base_members, vec!["ceo", "eng", "des"]);

    // An explicit empty override for the desk is a no-op too.
    let empty = desk_record_ordered(HIERARCHY_MANIFEST, Vec::new(), order("studio", &[]));
    assert_eq!(empty.effective_desk_members("studio"), base_members);
}

/// Ids in the override that are no longer desk members are ignored.
#[test]
fn desk_order_ignores_stale_ids() {
    let record = desk_record_ordered(
        HIERARCHY_MANIFEST,
        Vec::new(),
        order("studio", &["ghost", "des", "ceo", "eng"]),
    );
    // `ghost` is not a member, so it contributes nothing; the rest apply.
    assert_eq!(
        record.effective_desk_members("studio"),
        vec!["des".to_string(), "ceo".to_string(), "eng".to_string()]
    );
}

/// A subset override lists its ids first, then the unlisted members keep
/// their base relative order after.
#[test]
fn desk_order_subset_is_listed_first_then_default() {
    let record = desk_record_ordered(HIERARCHY_MANIFEST, Vec::new(), order("studio", &["des"]));
    // `des` promoted first; `ceo`, `eng` keep their base order behind it.
    assert_eq!(
        record.effective_desk_members("studio"),
        vec!["des".to_string(), "ceo".to_string(), "eng".to_string()]
    );
}

/// The persisted overlay blob round-trips the desk-order collection.
#[test]
fn overlay_blob_round_trips_desk_order() {
    let record = desk_record_ordered(
        HIERARCHY_MANIFEST,
        Vec::new(),
        order("studio", &["des", "ceo", "eng"]),
    );
    let blob = OverlayBlob::from_record(&record);
    let json = serde_json::to_string(&blob).expect("serialize blob");
    let parsed = OverlayBlob::parse(&json).expect("parse blob");
    assert_eq!(parsed.desk_order, record.overlay_desk_order);
}

/// The persisted overlay blob round-trips the `[policy]` override, and a
/// blob written before it existed still parses (issue #562).
///
/// Both halves matter. Without the first, a serialization path that dropped
/// the field would move an operator's approval gate back to the manifest on
/// the next load, silently. Without the second, every company record written
/// before this feature would fail to parse at all.
#[test]
fn overlay_blob_round_trips_the_policy_override() {
    let mut record = desk_record(POLICY_MANIFEST, Vec::new());
    record.overlay_policy = Some(policy_entry(Some("auto"), Some(vec!["payment.send"])));

    let blob = OverlayBlob::from_record(&record);
    let json = serde_json::to_string(&blob).expect("serialize blob");
    let parsed = OverlayBlob::parse(&json).expect("parse blob");
    assert_eq!(parsed.policy, record.overlay_policy);

    // A blob from before this field existed loads as "not overridden",
    // which is the pre-#562 behaviour exactly.
    let legacy = r#"{"agents":[],"desk_members":[],"budgets":[]}"#;
    let blob = OverlayBlob::parse(legacy).expect("blob without a policy key");
    assert!(
        blob.policy.is_none(),
        "an older record must load with the manifest's policy in charge"
    );

    // And so does the oldest form of all, the bare agent array.
    let bare = OverlayBlob::parse("[]").expect("legacy array");
    assert!(bare.policy.is_none());
}

/// An object-form blob written before `desk_order` existed still parses, and
/// the legacy bare-array form still parses — both with an empty order.
#[test]
fn overlay_blob_parses_without_desk_order_key() {
    // Object form missing the `desk_order` key (pre-#131 rows).
    let object = r#"{"agents":[{"id":"a","name":"A","role":"r"}],"desk_members":[{"desk_id":"d","agent_id":"a"}]}"#;
    let blob = OverlayBlob::parse(object).expect("object without desk_order");
    assert_eq!(blob.desk_members.len(), 1);
    assert!(blob.desk_order.is_empty());

    // Legacy bare `Vec<OverlayAgent>` form.
    let legacy = r#"[{"id":"a","name":"A","role":"r"}]"#;
    let blob = OverlayBlob::parse(legacy).expect("legacy array");
    assert!(blob.desk_order.is_empty());
}

/// `is_roster_agent` accepts both manifest agents and overlay teammates, and
/// rejects an unknown id — the validation the desk-add route relies on.
#[test]
fn is_roster_agent_covers_manifest_and_overlay() {
    let manifest = "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n";
    let mut record = desk_record(manifest, Vec::new());
    record.overlay_agents.push(OverlayAgent {
        provider: None,
        id: "nova".into(),
        name: "Nova".into(),
        role: "Growth".into(),
        description: None,
        tools: None,
        skills: None,
        model: None,
        harness: None,
    });
    assert!(record.is_roster_agent("ceo"));
    assert!(record.is_roster_agent("nova"));
    assert!(!record.is_roster_agent("ghost"));
}

/// Issue #661 / L5 serde, updated for #1804's three-state grant: an absent
/// `tools` key deserializes to `None` (the standard grant) and a `None`
/// grant serializes with no `tools` key — so a record written before the
/// field existed round-trips unchanged. The two new states are wire-visible:
/// an explicit deny-all (`Some(vec![])`) serializes as `tools: []` (present,
/// NOT skipped), and a narrowed grant serializes its list.
#[test]
fn overlay_agent_tools_three_state_serde_round_trip() {
    // An old record with no `tools` key deserializes to `None` (standard).
    let legacy: OverlayAgent =
        serde_json::from_str(r#"{"id":"a","name":"A","role":"r"}"#).expect("legacy overlay");
    assert_eq!(legacy.tools, None);

    // A `None` grant is omitted from the serialized form — a standard-grant
    // teammate is byte-for-byte what it was before this field existed.
    let value = serde_json::to_value(&legacy).unwrap();
    assert!(
        value.get("tools").is_none(),
        "a None (standard) grant must not serialize a `tools` key: {value}"
    );

    // An explicit deny-all IS on the wire, as `tools: []` — it must NOT be
    // skipped, or it would read back as the standard grant (the inversion).
    let denied = OverlayAgent {
        provider: None,
        id: "d".into(),
        name: "D".into(),
        role: "r".into(),
        description: None,
        tools: Some(Vec::new()),
        skills: None,
        model: None,
        harness: None,
    };
    let denied_value = serde_json::to_value(&denied).unwrap();
    assert_eq!(
        denied_value.get("tools"),
        Some(&serde_json::json!([])),
        "an explicit deny-all must serialize `tools: []`, not skip the key: {denied_value}"
    );
    let denied_round: OverlayAgent =
        serde_json::from_str(&serde_json::to_string(&denied).unwrap()).unwrap();
    assert_eq!(denied_round.tools, Some(Vec::new()));

    // A non-empty grant round-trips in order.
    let scoped = OverlayAgent {
        provider: None,
        id: "s".into(),
        name: "S".into(),
        role: "r".into(),
        description: None,
        tools: Some(vec!["docs.*".into(), "email".into()]),
        skills: None,
        model: None,
        harness: None,
    };
    let round: OverlayAgent =
        serde_json::from_str(&serde_json::to_string(&scoped).unwrap()).unwrap();
    assert_eq!(
        round.tools,
        Some(vec!["docs.*".to_string(), "email".to_string()])
    );
}

/// A record with one manifest agent and no desks, for the minting tests.
fn mint_record() -> CompanyRecord {
    desk_record(
        "[company]\nname = \"Acme\"\n\
         [[agent]]\nid = \"backend_engineer\"\nrole = \"Backend Engineer\"\n",
        Vec::new(),
    )
}

fn add_overlay(record: &mut CompanyRecord, id: &str, name: &str) {
    record.overlay_agents.push(OverlayAgent {
        provider: None,
        id: id.into(),
        name: name.into(),
        role: "Worker".into(),
        description: None,
        tools: None,
        skills: None,
        model: None,
        harness: None,
    });
}

/// A free slug is minted bare — the whole point of issue #686 is that the
/// common case reads as `agents/dana_designer/`.
#[test]
fn mint_agent_id_takes_the_bare_slug_when_it_is_free() {
    let record = mint_record();
    assert_eq!(record.mint_agent_id("Dana Designer"), "dana_designer");
    assert_eq!(record.mint_agent_id("Designer!!"), "designer");
    assert_eq!(record.mint_agent_id("24/7 Support"), "teammate");
}

/// The collision that matters: an overlay id equal to a **manifest** id is
/// skipped by `build_roster`, so the teammate would save and never
/// materialise. Suffixing is what keeps it reachable.
#[test]
fn mint_agent_id_suffixes_past_a_manifest_agent() {
    let record = mint_record();
    assert_eq!(
        record.mint_agent_id("Backend Engineer"),
        "backend_engineer_2"
    );
}

/// Repeated adds of one name walk `_2`, `_3`, … in order, so the ids a
/// company ends up with are a function of its roster and not of arrival
/// timing.
#[test]
fn mint_agent_id_walks_suffixes_deterministically() {
    let mut record = mint_record();
    let first = record.mint_agent_id("Designer");
    assert_eq!(first, "designer");
    add_overlay(&mut record, &first, "Designer");

    let second = record.mint_agent_id("Designer");
    assert_eq!(second, "designer_2");
    add_overlay(&mut record, &second, "Designer");

    assert_eq!(record.mint_agent_id("Designer"), "designer_3");

    // A degenerate name is not a special case — it suffixes like any other.
    add_overlay(&mut record, "teammate", "***");
    assert_eq!(record.mint_agent_id("🙂"), "teammate_2");
}

/// Case is not a difference: an overlay id typed with capitals still blocks
/// the lowercase slug, because `resolve_roster_agent_id` folds case and two
/// teammates one capital apart would be one unroutable key.
#[test]
fn mint_agent_id_treats_a_case_variant_id_as_taken() {
    let mut record = mint_record();
    add_overlay(&mut record, "Dana_Designer", "Dana Designer");
    assert_eq!(record.mint_agent_id("Dana Designer"), "dana_designer_2");
}

/// **Issue #1862 review**: an exact desk id must win over another desk's
/// display name.
///
/// Desk creation enforces id uniqueness but not name uniqueness, so
/// `{id: "ops", name: "sales"}` is a valid desk that can sit ahead of
/// `{id: "sales", …}`. A single pass whose predicate is
/// `id == key || name == key` returns whichever comes first, so asking for
/// the id `sales` answered `ops` — an ownership write silently targeting a
/// different desk than the caller named.
#[test]
fn an_exact_desk_id_beats_another_desks_display_name() {
    let manifest = "[company]\nname = \"Acme\"\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n";
    let mut record = desk_record(manifest, Vec::new());
    // Deliberately in the order that loses under a first-match search: the
    // desk merely *named* "sales" is created first.
    record.overlay_desks.push(OverlayDesk {
        id: "ops".into(),
        name: "sales".into(),
        description: None,
        members: Vec::new(),
        responder: crate::ports::types::ResponderMode::default(),
        hive: Default::default(),
    });
    record.overlay_desks.push(OverlayDesk {
        id: "sales".into(),
        name: "Revenue".into(),
        description: None,
        members: Vec::new(),
        responder: crate::ports::types::ResponderMode::default(),
        hive: Default::default(),
    });

    assert_eq!(
        record.resolve_desk_id("sales").as_deref(),
        Some("sales"),
        "an exact id must resolve to itself, not to a desk that merely \
         carries it as a display name"
    );
    // The alias still resolves for a key no desk owns as an id.
    assert_eq!(record.resolve_desk_id("Revenue").as_deref(), Some("sales"));
    assert_eq!(record.resolve_desk_id("ops").as_deref(), Some("ops"));
}

/// A manifest desk's id also beats an overlay desk's display name — the
/// exact-id pass spans both lists, so ordering between them cannot decide
/// an ownership write either.
#[test]
fn a_manifest_desk_id_beats_an_overlay_desks_display_name() {
    let manifest = "[company]\nname = \"Acme\"\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n\
         [[group_chat]]\nid = \"growth\"\nname = \"Content\"\nmembers = [\"ceo\"]\n";
    let mut record = desk_record(manifest, Vec::new());
    record.overlay_desks.push(OverlayDesk {
        id: "studio".into(),
        name: "growth".into(),
        description: None,
        members: Vec::new(),
        responder: crate::ports::types::ResponderMode::default(),
        hive: Default::default(),
    });

    assert_eq!(record.resolve_desk_id("growth").as_deref(), Some("growth"));
}
