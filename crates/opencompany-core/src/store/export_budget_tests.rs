use super::tests::{admin_actor, budget_manifest, fs_ports, manifest, tmp_root};
use super::*;

/// The same guard on the way IN: a bundle written by a host that predates
/// #358 carries the withdrawn text beside its tombstone, and importing it
/// must not write that text into the fresh journal.
///
/// Built by hand-editing the exported `events.jsonl` back to the
/// pre-redaction bytes, which is exactly the shape such a bundle has.
#[tokio::test]
async fn an_old_bundle_cannot_smuggle_a_withdrawn_message_back_in() {
    let home1 = tmp_root("smuggle-src");
    let home2 = tmp_root("smuggle-dst");
    let dest = tmp_root("smuggle-bundle");
    let id = CompanyId::new("smuggle-co");
    const SECRET: &str = "sk-live-SMUGGLED";

    let (s1, e1, m1, c1) = fs_ports(&home1);
    s1.save(&CompanyRecord {
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        overlay_desk_hive: Vec::new(),
        id: id.clone(),
        manifest: manifest(),
        ledger: Vec::new(),
        lifecycle: "running".into(),
        overlay_agents: Vec::new(),
        overlay_desk_members: Vec::new(),
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
    })
    .await
    .unwrap();
    let leaked = e1
        .append(
            &id,
            CompanyEvent::TaskDiscussionPosted {
                task_id: "t1".into(),
                text: SECRET.into(),
                by: None,
            },
        )
        .await
        .unwrap();
    e1.append(
        &id,
        CompanyEvent::TaskDiscussionRedacted {
            task_id: "t1".into(),
            seq: leaked.value(),
            by: None,
        },
    )
    .await
    .unwrap();

    export_bundle(&id, &dest, s1, e1, m1, c1, None, ExportOpts::default())
        .await
        .unwrap();

    // Put the secret back, as an older exporter would have written it.
    let path = dest.join(EVENTS_JSONL);
    let scrubbed = tokio::fs::read_to_string(&path).await.unwrap();
    let old_shape = scrubbed.replace(crate::ports::tasks::REDACTED_DISCUSSION_TEXT, SECRET);
    assert!(old_shape.contains(SECRET), "the fixture did not rewrite");
    tokio::fs::write(&path, old_shape).await.unwrap();

    let (s2, e2, m2, c2) = fs_ports(&home2);
    import_bundle(&dest, s2, e2.clone(), m2, c2, None)
        .await
        .unwrap();
    let events = e2
        .read_from(&id, EventSeq::new(0), usize::MAX)
        .await
        .unwrap();
    assert!(
        !events.iter().any(|stored| matches!(
            &stored.event,
            CompanyEvent::TaskDiscussionPosted { text, .. } if text.contains(SECRET)
        )),
        "an old bundle smuggled a withdrawn message into the new journal"
    );

    for dir in [home1, home2, dest] {
        tokio::fs::remove_dir_all(&dir).await.ok();
    }
}

/// Issue #85: a template-launched company's `template_provenance` survives an
/// export → import round-trip intact (source_id, version, and path all carry
/// through the bundle's `meta.json`), so exporting then importing never
/// silently strips a company's origin template.
#[tokio::test]
async fn template_provenance_survives_roundtrip() {
    let home1 = tmp_root("prov-src");
    let home2 = tmp_root("prov-dst");
    let dest = tmp_root("prov-bundle");
    let id = CompanyId::new("prov-co");

    let provenance = TemplateProvenance {
        source_id: "law_firm".into(),
        version: None,
        path: Some("law_firm".into()),
    };

    // Register a company carrying template provenance in the source home.
    let (s1, e1, m1, c1) = fs_ports(&home1);
    s1.save(&CompanyRecord {
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        overlay_desk_hive: Vec::new(),
        id: id.clone(),
        manifest: manifest(),
        ledger: Vec::new(),
        lifecycle: "running".into(),
        overlay_agents: Vec::new(),
        overlay_desk_members: Vec::new(),
        overlay_desk_order: Vec::new(),
        overlay_desks: Vec::new(),
        overlay_workflows: Vec::new(),
        overlay_budgets: Vec::new(),
        overlay_policy: None,
        overlay_tool_grants: None,
        overlay_desk_tools: Default::default(),
        disabled_workflows: Vec::new(),
        template_provenance: Some(provenance.clone()),
        setup: None,
        name_confirmed: false,
        activation_completed_at: None,
        created_at_millis: None,
    })
    .await
    .unwrap();

    // Export → import into a fresh home.
    export_bundle(&id, &dest, s1, e1, m1, c1, None, ExportOpts::default())
        .await
        .unwrap();
    let (s2, e2, m2, c2) = fs_ports(&home2);
    let imported = import_bundle(&dest, s2.clone(), e2, m2, c2, None)
        .await
        .unwrap();
    assert_eq!(imported, id, "id preserved through the bundle");

    // The imported record carries the identical provenance — all three fields.
    let rec = s2.load(&id).await.unwrap().expect("imported record");
    assert_eq!(
        rec.template_provenance,
        Some(provenance),
        "template provenance lost across the bundle round-trip"
    );

    for dir in [home1, home2, dest] {
        tokio::fs::remove_dir_all(&dir).await.ok();
    }
}

/// EVERY operator overlay — the team (`overlay_agents`), desk memberships
/// (`overlay_desk_members`), the desk-order hierarchy (`overlay_desk_order`),
/// operator-created desks (`overlay_desks`), and runtime-authored workflow
/// graphs (`overlay_workflows`) — survives an export→import.
/// A prior version threaded only `overlay_desk_order` through the bundle, so a
/// round-trip silently ERASED operator-added teammates, desk memberships, and
/// operator-created desks (data loss). This asserts all four come back intact
/// and that the desk hierarchy still drives the routing lead.
#[tokio::test]
async fn operator_overlays_including_desk_order_survive_roundtrip() {
    let home1 = tmp_root("order-src");
    let home2 = tmp_root("order-dst");
    let dest = tmp_root("order-bundle");
    let id = CompanyId::new("order-co");

    // A manifest desk whose blueprint lead is `ceo`.
    let manifest: CompanyManifest = toml::from_str(
        r#"
            [company]
            name = "Order Co"
            output = "widgets"

            [[agent]]
            id = "ceo"
            role = "Chief"

            [[agent]]
            id = "cto"
            role = "Tech"

            [[group_chat]]
            id = "eng"
            name = "Engineering"
            members = ["ceo", "cto"]
        "#,
    )
    .expect("parse manifest");

    // Operator reorders the desk so `cto` becomes the lead — a non-empty order.
    let order = vec![OverlayDeskOrder {
        desk_id: "eng".into(),
        ordered: vec!["cto".into(), "ceo".into()],
    }];
    // An operator-added teammate, not in the manifest.
    let agents = vec![OverlayAgent {
        provider: None,
        id: "designer".into(),
        name: "Dana Designer".into(),
        role: "Design".into(),
        description: Some("Owns the brand".into()),
        tools: None,
        skills: None,
        model: None,
        harness: None,
    }];
    // That teammate added to the `eng` desk through the membership overlay.
    let desk_members = vec![OverlayDeskMember {
        desk_id: "eng".into(),
        agent_id: "designer".into(),
    }];
    // Operator-created desks the manifest never declared — one of each
    // responder mode, so the round-trip below (whole-vec equality) proves
    // the `auto` flag survives export→import rather than silently
    // reverting a leadless channel to a lead desk (issue #1835).
    let desks = vec![
        OverlayDesk {
            id: "growth".into(),
            name: "Growth".into(),
            description: Some("Marketing pod".into()),
            members: vec!["ceo".into()],
            responder: crate::ports::types::ResponderMode::default(),
            hive: Default::default(),
        },
        OverlayDesk {
            id: "launch".into(),
            name: "Launch".into(),
            description: None,
            members: vec!["ceo".into(), "cto".into()],
            responder: crate::ports::types::ResponderMode::Auto,
            hive: Default::default(),
        },
    ];
    // A workflow graph authored at runtime (issue #168). On a hosted tenant
    // this body is the ONLY copy — a bundle that dropped it would lose the
    // workflow outright.
    let workflows = vec![OverlayWorkflow {
        id: "console_flow".into(),
        toml: "id = \"console_flow\"\nname = \"Console flow\"\n\
                   [[node]]\nid = \"start\"\nkind = \"trigger\"\nname = \"Start\"\n"
            .into(),
    }];

    let (s1, e1, m1, c1) = fs_ports(&home1);
    s1.save(&CompanyRecord {
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        overlay_desk_hive: Vec::new(),
        id: id.clone(),
        manifest: manifest.clone(),
        ledger: Vec::new(),
        lifecycle: "running".into(),
        overlay_agents: agents.clone(),
        overlay_desk_members: desk_members.clone(),
        overlay_desk_order: order.clone(),
        overlay_desks: desks.clone(),
        overlay_workflows: workflows.clone(),
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
    })
    .await
    .unwrap();

    // Sanity: the override already flips the lead in the source.
    let src_record = s1.load(&id).await.unwrap().unwrap();
    assert_eq!(src_record.effective_desk_members("eng")[0], "cto");

    export_bundle(&id, &dest, s1, e1, m1, c1, None, ExportOpts::default())
        .await
        .unwrap();
    let (s2, e2, m2, c2) = fs_ports(&home2);
    import_bundle(&dest, s2.clone(), e2, m2, c2, None)
        .await
        .unwrap();

    // Every overlay came across intact — not reset to an empty list.
    let dst_record = s2.load(&id).await.unwrap().unwrap();
    assert!(
        !dst_record.overlay_agents.is_empty(),
        "team overlay erased by the bundle round-trip"
    );
    assert_eq!(
        dst_record.overlay_agents, agents,
        "team overlay altered by the bundle round-trip"
    );
    assert!(
        !dst_record.overlay_desk_members.is_empty(),
        "desk-membership overlay erased by the bundle round-trip"
    );
    assert_eq!(
        dst_record.overlay_desk_members, desk_members,
        "desk-membership overlay altered by the bundle round-trip"
    );
    assert!(
        !dst_record.overlay_desk_order.is_empty(),
        "desk-order overlay erased by the bundle round-trip"
    );
    assert_eq!(
        dst_record.overlay_desk_order, order,
        "desk order overlay altered by the bundle round-trip"
    );
    assert!(
        !dst_record.overlay_desks.is_empty(),
        "operator-created desks erased by the bundle round-trip"
    );
    assert_eq!(
        dst_record.overlay_desks, desks,
        "operator-created desks altered by the bundle round-trip"
    );
    assert!(
        !dst_record.overlay_workflows.is_empty(),
        "runtime-authored workflows erased by the bundle round-trip"
    );
    assert_eq!(
        dst_record.overlay_workflows, workflows,
        "runtime-authored workflows altered by the bundle round-trip"
    );
    // And the hierarchy still drives routing: `cto` remains the lead after
    // import.
    assert_eq!(
        dst_record.effective_desk_members("eng")[0],
        "cto",
        "routing lead reverted to blueprint after import"
    );

    for dir in [home1, home2, dest] {
        tokio::fs::remove_dir_all(&dir).await.ok();
    }
}

/// Issue #343: **all three** budget states survive an export→import — not
/// just the empty overlay every other fixture carries.
///
/// The three are only distinct if serialization keeps them distinct, and two
/// of the three collapse into each other under the obvious mistakes:
/// `Some(0.0)` becomes `None` if the field is ever serialized with
/// `skip_serializing_if = "is_zero"`-style cleverness, and an explicit `None`
/// becomes "no entry at all" if the row is dropped when it carries no cap.
/// Either collapse is a silent unrecoverable change to a spend cap: a
/// teammate an admin muted starts spending again, or a teammate an admin
/// deliberately uncapped inherits the manifest's cap back. Attribution is
/// asserted alongside the cap because a restored cap nobody appears to have
/// set is its own defect.
#[tokio::test]
async fn budget_overrides_survive_roundtrip_including_zero_and_explicit_none() {
    let home1 = tmp_root("budget-src");
    let home2 = tmp_root("budget-dst");
    let dest = tmp_root("budget-bundle");
    let id = CompanyId::new("budget-co");

    let budgets = vec![
        // Cap of exactly zero: "this teammate may not spend", NOT "no cap".
        BudgetOverride {
            agent_id: "ceo".into(),
            budget_usd_daily: Some(0.0),
            set_by: admin_actor(),
            at_millis: 1_700_000_000_000,
        },
        // Explicitly uncapped, beating the manifest's $9.
        BudgetOverride {
            agent_id: "cto".into(),
            budget_usd_daily: None,
            set_by: admin_actor(),
            at_millis: 1_700_000_000_001,
        },
    ];

    let (s1, e1, m1, c1) = fs_ports(&home1);
    s1.save(&CompanyRecord {
        overlay_desk_hive: Vec::new(),
        id: id.clone(),
        manifest: budget_manifest(),
        ledger: Vec::new(),
        lifecycle: "running".into(),
        overlay_agents: Vec::new(),
        overlay_desk_members: Vec::new(),
        overlay_desk_order: Vec::new(),
        overlay_desks: Vec::new(),
        overlay_workflows: Vec::new(),
        overlay_budgets: budgets.clone(),
        // Issue #562: a console-set tier rides the same bundle, and for the
        // same reason the paused workflow below does — a `None` here could
        // not have detected the field being dropped, and dropping it would
        // silently move an imported company's approval gate back to whatever
        // the manifest shipped with.
        overlay_policy: Some(PolicyOverride {
            mode: Some("auto".to_string()),
            always_approve: Some(vec!["payment.send".to_string()]),
            auto_approve_under_usd: Some(Some(25.0)),
            approval_ttl_hours: Some(48),
            set_by: admin_actor(),
            at_millis: 1_700_000_000_002,
        }),
        // Issue #1796: the console tool grants ride the same bundle, and
        // need it more sharply than the desk ceiling below — this is the
        // one overlay that WIDENS `[tools].allow`, so dropping it would
        // silently revoke an integration the operator granted from a
        // connect surface, leaving the imported company "Connected" and
        // reaching nobody.
        overlay_tool_grants: Some(ToolGrantsOverride {
            added: vec!["chargebee".to_string()],
            set_by: admin_actor(),
            at_millis: 1_700_000_000_003,
        }),
        // Non-empty for the same reason the tier above is: an empty map here
        // could not detect the field being dropped from the bundle, and
        // dropping it would silently restore an imported company's narrowed
        // desk at the company's full tool grant.
        overlay_desk_tools: std::collections::BTreeMap::from([(
            "research".to_string(),
            vec!["docs.*".to_string()],
        )]),
        // Issue #276: a paused workflow rides the same bundle. The empty
        // list this fixture used to carry could not have detected the field
        // being dropped — and dropping it would silently re-arm a schedule
        // an operator had switched off, which is the one direction an
        // import must never move on its own.
        disabled_workflows: vec!["digest".to_string()],
        // A console-shaped roster rides the same bundle: without the field
        // an imported company would silently come back on the blueprint's
        // names, roles and scopes, undoing every edit an operator made.
        overlay_agent_edits: vec![AgentOverride {
            agent_id: "ceo".to_string(),
            role: Some("Chief Vibes".to_string()),
            ..Default::default()
        }],
        // And a tombstone, for the sharper version of the same loss: the
        // blueprint still declares this teammate, so a bundle that dropped
        // the field would restore somebody the operator had removed.
        //
        // `ops` rather than one of the capped pair on purpose. Removing a
        // teammate drops its budget override with it, so a record holding
        // both a tombstone and a cap for the same id is a state no write
        // path can reach — a fixture that carried one would be asserting
        // that the bundle faithfully preserves something the product never
        // writes.
        overlay_retired_agents: vec!["ops".to_string()],
        template_provenance: None,
        setup: None,
        name_confirmed: false,
        activation_completed_at: None,
        created_at_millis: None,
    })
    .await
    .unwrap();

    // Sanity: both overrides already beat the manifest in the source.
    let src_record = s1.load(&id).await.unwrap().unwrap();
    assert_eq!(src_record.effective_budget("ceo"), Some(0.0));
    assert_eq!(src_record.effective_budget("cto"), None);

    export_bundle(&id, &dest, s1, e1, m1, c1, None, ExportOpts::default())
        .await
        .unwrap();
    let (s2, e2, m2, c2) = fs_ports(&home2);
    import_bundle(&dest, s2.clone(), e2, m2, c2, None)
        .await
        .unwrap();

    let dst_record = s2.load(&id).await.unwrap().unwrap();
    assert_eq!(
        dst_record.overlay_budgets, budgets,
        "budget overrides altered by the bundle round-trip"
    );
    assert!(
        !dst_record.workflow_enabled("digest"),
        "the bundle round-trip re-armed a paused workflow"
    );
    assert_eq!(
        dst_record
            .effective_agent("ceo")
            .expect("the roster still names the ceo")
            .role,
        "Chief Vibes",
        "the bundle round-trip restored the blueprint's role over the operator's edit"
    );
    assert!(
        dst_record.effective_agent("ops").is_none(),
        "the bundle round-trip restored a teammate the operator had removed"
    );
    // The capped pair is still on the roster, so the budget assertions below
    // are read through teammates that actually exist.
    assert!(
        dst_record.effective_agent("cto").is_some(),
        "cto was retired by accident"
    );
    // Issue #562: the console-set tier survives export→import, attribution
    // included. Without this the seeded fixture proves nothing — a bundle
    // path that dropped the field would still pass every other assertion
    // here, and an imported company would silently run the manifest's gate.
    let policy = dst_record
        .overlay_policy
        .as_ref()
        .expect("the policy override was dropped by the bundle round-trip");
    assert_eq!(policy.mode.as_deref(), Some("auto"));
    assert_eq!(
        policy.always_approve.as_deref(),
        Some(["payment.send".to_string()].as_slice())
    );
    assert_eq!(policy.auto_approve_under_usd, Some(Some(25.0)));
    assert_eq!(policy.approval_ttl_hours, Some(48));
    assert_eq!(policy.set_by, admin_actor());
    assert_eq!(policy.at_millis, 1_700_000_000_002);
    assert_eq!(
        dst_record.effective_policy().mode,
        "auto",
        "the imported company must run the tier the operator set, not the manifest's"
    );

    // Cap, attribution and timestamp, read the way every surface reads them.
    assert_eq!(
        dst_record.effective_budget("ceo"),
        Some(0.0),
        "a zero cap must survive as zero, not decay into uncapped"
    );
    assert_eq!(
        dst_record.effective_budget("cto"),
        None,
        "an explicitly-uncapped override must survive and still beat the manifest's $9"
    );
    let ceo = dst_record.budget_override("ceo").expect("ceo attribution");
    assert_eq!(ceo.set_by, admin_actor());
    assert_eq!(ceo.at_millis, 1_700_000_000_000);
    let cto = dst_record.budget_override("cto").expect(
        "an explicitly-uncapped override must keep its attribution row — it is exactly the \
             case an operator needs to see attributed",
    );
    assert_eq!(cto.set_by, admin_actor());
    assert_eq!(cto.at_millis, 1_700_000_000_001);

    for dir in [home1, home2, dest] {
        tokio::fs::remove_dir_all(&dir).await.ok();
    }
}
