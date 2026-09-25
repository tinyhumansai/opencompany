use super::*;

/// The default responder is the `orchestrator`-tier agent, even when it is
/// not first on the roster.
#[test]
fn default_responder_is_the_orchestrator() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, _tasks) = brain_with_desk(dir.path());
    assert_eq!(brain.responder, "chief");
}

/// An addressed desk routes to its lead member (by id or name); anything else
/// — the "General" desk, an unknown id, or no address — falls to the
/// orchestrator.
#[test]
fn responder_for_routes_desk_to_lead_else_orchestrator() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, _tasks) = brain_with_desk(dir.path());
    assert_eq!(brain.responder_for(Some("eng_desk")), "engineer");
    assert_eq!(brain.responder_for(Some("Engineering")), "engineer");
    assert_eq!(brain.responder_for(Some("General")), "chief");
    assert_eq!(brain.responder_for(Some("nope")), "chief");
    assert_eq!(brain.responder_for(None), "chief");
}

/// A chat id naming a roster teammate answers as that teammate, which is
/// what a per-agent DM thread is. Before this it fell through to the
/// orchestrator, so the console would show an agent's thread while someone
/// else answered in it.
#[test]
fn responder_for_routes_a_roster_agent_id_to_that_agent() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, _tasks) = brain_with_desk(dir.path());
    assert_eq!(brain.responder_for(Some("engineer")), "engineer");
    assert_eq!(brain.responder_for(Some("chief")), "chief");
}

/// An **overlay** desk that took a General spelling before those were
/// reserved must not answer the company-wide line.
///
/// `create_desk` accepted `main` until issue #1743, so this is persisted
/// state rather than a hypothesis. Such a desk is already hidden from
/// `GET .../desks` and refused every mutation, but hiding a desk does not
/// stop it routing: `desk_lead` resolves through
/// `CompanyRecord::resolve_desk_id`, which used to match it, so the console
/// rendered `#general` and named the orchestrator as who answers while this
/// desk's lead answered instead. The resolver declines the key now, and the
/// arm below it hands the line back to the orchestrator.
#[test]
fn responder_for_does_not_let_a_hidden_overlay_desk_answer_the_general_line() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, _tasks) = brain_with_desk(dir.path());
    brain.mutate_record(|r| {
        r.overlay_desks.push(crate::ports::types::OverlayDesk {
            id: "main".into(),
            name: "Front office".into(),
            description: None,
            responder: Default::default(),
            members: vec!["engineer".into()],
            hive: Default::default(),
        })
    });
    for spelling in ["", "main", "Main", "general", "General"] {
        assert_eq!(
            brain.responder_for(Some(spelling)),
            "chief",
            "the orchestrator answers the company-wide line as {spelling:?}"
        );
    }
    // The desk is not retired — it simply has no General key any more, and
    // the desk it is still routes to its own lead.
    assert_eq!(brain.responder_for(Some("eng_desk")), "engineer");
}

/// A **teammate** whose id is a General spelling keeps its DM, and does not
/// take the company-wide line with it (issue #1743).
///
/// `mint_agent_id` reserves `main` and `General`, but a manifest can still
/// declare one, and a manifest is not something this host overrules. Before
/// this, `resolve_roster_agent_id` matched the bare key and that teammate
/// answered every unaddressed message — while `GET chat/history?desk=main`
/// returned the *folded General conversation* (`is_general_chat` has folded
/// `""`, `main`, `General` and `general` into one since issue #65). The
/// responder and the transcript disagreed about whose conversation it was.
/// The bare key is the line; `dm:<id>` is the teammate.
#[test]
fn responder_for_gives_the_general_line_to_the_orchestrator_not_a_teammate_called_main() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, _tasks) = brain_with_desk(dir.path());
    brain.mutate_record(|r| {
        r.overlay_agents.push(OverlayAgent {
            provider: None,
            id: "main".into(),
            name: "Mainard".into(),
            role: "Analyst".into(),
            description: None,
            tools: None,
            skills: None,
            model: None,
            harness: None,
        })
    });
    assert!(
        brain.record().is_roster_agent("main"),
        "the teammate really is on the roster, so the old arm would have matched"
    );
    assert_eq!(
        brain.responder_for(Some("main")),
        "chief",
        "the bare key is the company's line, whatever a teammate is called"
    );
    assert_eq!(
        brain.responder_for(Some("dm:main")),
        "main",
        "and the teammate keeps its own DM, addressed the way the console addresses one"
    );
}

/// The grandfathered case the two tests above must not break: a
/// `[[group_chat]]` the **blueprint** declares under a General spelling is
/// the company's own General desk, and its lead still answers it.
#[test]
fn responder_for_still_routes_a_blueprint_general_desk_to_its_lead() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, _tasks) = brain_with_desk(dir.path());
    let declared = toml::from_str::<CompanyManifest>(
        r#"
[company]
name = "Acme"

[[agent]]
id = "engineer"
role = "Engineer"

[[group_chat]]
id = "main"
name = "Front office"
members = ["engineer"]
"#,
    )
    .expect("valid manifest")
    .group_chats;
    brain.mutate_record(|r| r.manifest.group_chats.extend(declared));
    assert_eq!(
        brain.responder_for(Some("main")),
        "engineer",
        "a blueprint desk keeps the line and its lead keeps answering it"
    );
}

/// A key that resolves to no desk and no teammate still answers as the
/// orchestrator — the fallback is deliberate — but it now says so.
///
/// This is the whole of D2: before it, "nobody addressed anybody" and
/// "somebody addressed a teammate that does not exist" produced the same
/// confident answer from an agent nobody asked, and the tenant log carried
/// nothing to tell them apart.
#[test]
fn responder_for_warns_before_falling_back_to_the_orchestrator() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, _tasks) = brain_with_desk(dir.path());
    // `dm:engineer` was this fixture until issue #982 made it resolve; the
    // key here has to be one that names nothing at all, or the test would
    // pass by asserting the wrong fact and #884's coverage would be gone.
    let logs = logs_from(|| {
        assert_eq!(
            brain.responder_for(Some("dm:nobody_by_that_name")),
            "chief",
            "the fallback itself is unchanged"
        );
    });
    assert!(
        logs.contains("dm:nobody_by_that_name"),
        "the unresolved key must be named so the fall-through is greppable: {logs}"
    );
    assert!(logs.contains("WARN"), "{logs}");

    // …and a key that DOES resolve stays silent, or the line is noise
    // rather than a signal.
    let quiet = logs_from(|| {
        assert_eq!(brain.responder_for(Some("engineer")), "engineer");
        assert_eq!(brain.responder_for(Some("eng_desk")), "engineer");
    });
    assert!(quiet.is_empty(), "a resolved key must not warn: {quiet}");
}

/// Issue #982: the console mints a DM channel id as `dm:<teammate-id>`, and
/// a sibling route documents that form as a valid channel key — so a thread
/// keyed on it has to be answered by the teammate it names, not by the
/// orchestrator.
///
/// The prefix is stripped **after** the desk and roster attempts, so this
/// can only ever claim a key that resolved to nothing: `engineer` and
/// `eng_desk` still route exactly as they did, which the test above pins.
#[test]
fn responder_for_answers_a_console_dm_channel_key_as_the_teammate() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, _tasks) = brain_with_desk(dir.path());
    assert_eq!(
        brain.responder_for(Some("dm:engineer")),
        "engineer",
        "a DM channel key addresses the teammate it names"
    );
    assert_eq!(
        brain.responder_for(Some("dm:")),
        "chief",
        "a prefix with nothing after it names nobody"
    );
}

/// A human- or console-typed teammate key resolves case-insensitively to the
/// **canonical** roster id, so a capital letter no longer reads as "nobody"
/// and hands the turn to the orchestrator.
///
/// Returning the canonical id rather than the key as typed is the load-bearing
/// half: the persona lookup downstream matches on the roster id, so echoing
/// `"Engineer"` back would move the miss one layer along instead of fixing it.
#[test]
fn responder_for_resolves_a_teammate_key_case_insensitively() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, _tasks) = brain_with_desk(dir.path());
    for typed in ["Engineer", "ENGINEER", "engineer"] {
        assert_eq!(
            brain.responder_for(Some(typed)),
            "engineer",
            "`{typed}` must reach the engineer under its canonical id"
        );
    }
    // A key that resolves to nothing is still the orchestrator's — folding
    // the case may only claim keys that reached nobody before.
    assert_eq!(brain.responder_for(Some("engineeer")), "chief");
}

/// Desks still win. A desk id is resolved as a desk even if an agent shares
/// the name, so no existing thread changes where it lands.
#[test]
fn a_desk_still_outranks_an_agent_of_the_same_name() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, _tasks) = brain_with_desk(dir.path());
    // `eng_desk` is a desk led by `engineer`; it must resolve through the
    // desk path, not the DM path.
    assert_eq!(brain.responder_for(Some("eng_desk")), "engineer");
    // And an id that is neither still reaches the orchestrator.
    assert_eq!(brain.responder_for(Some("not-a-teammate")), "chief");
}

/// An operator-added overlay member is resolved as a desk's lead (issue #72):
/// on a desk the manifest left empty, the overlay addition becomes the lead,
/// and an addressed message routes to it. Proves `desk_lead`/`responder_for`
/// read the effective (manifest ∪ overlay) membership.
#[test]
fn overlay_member_resolves_as_desk_lead() {
    let dir = tempfile::tempdir().unwrap();
    // `design` is a manifest desk with no declared members; the operator adds
    // `engineer` to it through the overlay.
    let manifest = toml::from_str(
        r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "chief"
role = "Chief of Staff"
tier = "orchestrator"

[[agent]]
id = "engineer"
role = "Engineer"

[[group_chat]]
id = "design"
name = "Design"
"#,
    )
    .expect("valid manifest");
    let record = CompanyRecord {
        overlay_desk_hive: Vec::new(),
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        id: CompanyId::new("acme"),
        manifest,
        ledger: Vec::new(),
        lifecycle: "running".to_string(),
        overlay_agents: Vec::new(),
        overlay_desk_members: vec![crate::ports::types::OverlayDeskMember {
            desk_id: "design".to_string(),
            agent_id: "engineer".to_string(),
        }],
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
    };
    let (brain, _tasks) = brain_over(dir.path(), record);
    assert_eq!(
        delegation::desk_lead(&brain.record(), "design"),
        Some("engineer".to_string())
    );
    assert_eq!(brain.responder_for(Some("design")), "engineer");
}

/// The operator's desk hierarchy drives the desk lead: a desk with manifest
/// members `[eng1, eng2]` plus an overlay `cto`, ordered `[cto, eng1, eng2]`,
/// resolves its lead to `cto` — `desk_lead` reads `effective_desk_members`,
/// so the reorder flows through with no change to the resolver (issue #131).
#[test]
fn desk_order_drives_the_desk_lead() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = toml::from_str(
        r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "eng1"
role = "Engineer One"

[[agent]]
id = "eng2"
role = "Engineer Two"

[[group_chat]]
id = "eng"
name = "Engineering"
members = ["eng1", "eng2"]
"#,
    )
    .expect("valid manifest");
    let record = CompanyRecord {
        overlay_desk_hive: Vec::new(),
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        id: CompanyId::new("acme"),
        manifest,
        ledger: Vec::new(),
        lifecycle: "running".to_string(),
        overlay_agents: vec![crate::ports::types::OverlayAgent {
            provider: None,
            id: "cto".to_string(),
            name: "Cto".to_string(),
            role: "CTO".to_string(),
            description: None,
            tools: None,
            skills: None,
            model: None,
            harness: None,
        }],
        overlay_desk_members: vec![crate::ports::types::OverlayDeskMember {
            desk_id: "eng".to_string(),
            agent_id: "cto".to_string(),
        }],
        overlay_desk_order: vec![crate::ports::types::OverlayDeskOrder {
            desk_id: "eng".to_string(),
            ordered: vec!["cto".to_string(), "eng1".to_string(), "eng2".to_string()],
        }],
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
    };
    let (brain, _tasks) = brain_over(dir.path(), record);
    assert_eq!(
        delegation::desk_lead(&brain.record(), "eng"),
        Some("cto".to_string())
    );
}

/// Regression for the builder seeding path (#133): a desk-order change written
/// to the store must take effect on routing once the brain is rebuilt from the
/// persisted record. The builder used to construct the brain with an empty
/// `overlay_desk_order`, so desk chats kept routing to the pre-reorder lead.
/// Here we persist a record, build a brain from the loaded record (blueprint
/// lead), then write a new order and rebuild the brain from the reloaded record
/// — the lead must update, not stay stale.
#[tokio::test]
async fn desk_order_change_updates_routing_after_rebuild() {
    use crate::ports::store::CompanyStore;

    let dir = tempfile::tempdir().unwrap();
    let store = FsCompanyStore::new(dir.path());
    let manifest = toml::from_str(
        r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "eng1"
role = "Engineer One"

[[agent]]
id = "eng2"
role = "Engineer Two"

[[group_chat]]
id = "eng"
name = "Engineering"
members = ["eng1", "eng2"]
"#,
    )
    .expect("valid manifest");
    let id = CompanyId::new("acme");
    store
        .save(&CompanyRecord {
            overlay_desk_hive: Vec::new(),
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
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

    // Brain built from the persisted record before any reorder: blueprint lead.
    let loaded = store.load(&id).await.unwrap().unwrap();
    let (brain, _tasks) = brain_over(dir.path(), loaded);
    assert_eq!(
        delegation::desk_lead(&brain.record(), "eng"),
        Some("eng1".to_string()),
        "blueprint lead before reorder"
    );

    // Operator reorders the desk (as `set_desk_order` does), promoting eng2.
    let mut record = store.load(&id).await.unwrap().unwrap();
    record
        .overlay_desk_order
        .push(crate::ports::types::OverlayDeskOrder {
            desk_id: "eng".to_string(),
            ordered: vec!["eng2".to_string(), "eng1".to_string()],
        });
    store.save(&record).await.unwrap();

    // Rebuild the brain from the reloaded record: routing follows the reorder,
    // no stale lead.
    let reloaded = store.load(&id).await.unwrap().unwrap();
    let (rebuilt, _tasks2) = brain_over(dir.path(), reloaded);
    assert_eq!(
        delegation::desk_lead(&rebuilt.record(), "eng"),
        Some("eng2".to_string()),
        "reorder did not take effect on routing after rebuild"
    );
}

/// A `spawn_task` delegation opens a To-do card and surfaces no bubble.
#[tokio::test]
async fn spawn_task_delegation_opens_a_todo_card() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, tasks) = brain_with_desk(dir.path());
    let out = brain
        .run_delegation(
            Delegation::SpawnTask {
                title: "Draft the plan".to_string(),
                note: Some("by friday".to_string()),
                assignee: Some("engineer".to_string()),
            },
            None,
        )
        .await
        .expect("delegation runs");
    assert!(
        out.bubble.is_none() && out.desk_reply.is_none(),
        "spawn_task surfaces nothing to relay or bubble"
    );

    let cards = tasks.list(&CompanyId::new("acme")).await.unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].title, "Draft the plan");
    assert_eq!(cards[0].column, COLUMN_TODO);
    assert_eq!(cards[0].assignee, "engineer");
    // Issue #246: it surfaces no *bubble*, but it no longer surfaces
    // *nothing* — the card it opened is reported, which is what lets the
    // caller tell the operator a card exists instead of leaving them to
    // notice it on the board.
    assert_eq!(
        out.spawned_task.as_deref(),
        Some(cards[0].id.as_str()),
        "the opened card must be reported, and be the one actually written"
    );
}

/// A spawned card is grounded on the same terms an assigned one is: a name
/// that resolves to nobody opens the card unowned, rather than stamping an
/// owner the board renders and no dispatch can reach.
#[tokio::test]
async fn spawn_task_refuses_to_stamp_an_off_roster_owner_on_a_new_card() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, tasks) = brain_with_desk(dir.path());
    brain
        .run_delegation(
            Delegation::SpawnTask {
                title: "Draft the plan".to_string(),
                note: None,
                assignee: Some("not-a-real-agent-xyz".to_string()),
            },
            None,
        )
        .await
        .expect("delegation runs");

    let cards = tasks.list(&CompanyId::new("acme")).await.unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(
        cards[0].assignee, "",
        "an unresolvable name leaves the card unowned"
    );
}

/// Issue #246: a chat turn that opened a card says so on the bubble it
/// answered from. Before this the card appeared on the board and the reply
/// carried nothing tying the two together, so an operator had no way to
/// tell a turn that opened work from one that only talked about it.
#[tokio::test]
async fn a_turn_that_opens_a_card_reports_it_on_the_operator_bubble() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, _provider) = brain_that_delegates(
        dir.path(),
        vec![Some(Delegation::SpawnTask {
            title: "Draft the announcement".to_string(),
            note: None,
            assignee: None,
        })],
    );

    let result = brain
        .run_cycle(
            request(vec![CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                parent: None,
                text: "we should announce this".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
            }]),
            &NoopHost,
        )
        .await
        .expect("cycle runs");

    assert_eq!(result.channel_responses.len(), 1);
    let bubble = &result.channel_responses[0];
    let reported = bubble.task_id.as_deref().expect("the bubble names a card");
    let cards = brain
        .deps
        .tasks
        .as_ref()
        .unwrap()
        .list(&brain.record().id)
        .await
        .unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(
        reported, cards[0].id,
        "the reported card must be the one on the board"
    );
}

/// Issue #246, the documented limitation stated as a test rather than only
/// as prose: a turn that opens several cards reports the **first**. The
/// journal field this feeds is a single optional id, so widening it would
/// break the byte-identical round-trip every already-stored reply relies
/// on. Pinned to *first* — not "whichever won" — because a later spawn
/// silently overwriting an earlier one would make the reported card depend
/// on queue order, which is the model's choice, not a contract.
#[tokio::test]
async fn a_turn_that_opens_several_cards_reports_the_first() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, _provider) = brain_that_delegates_with(
        dir.path(),
        vec![vec![
            Delegation::SpawnTask {
                title: "First".to_string(),
                note: None,
                assignee: None,
            },
            Delegation::SpawnTask {
                title: "Second".to_string(),
                note: None,
                assignee: None,
            },
        ]],
        TurnFaults::default(),
    );

    let result = brain
        .run_cycle(
            request(vec![CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                parent: None,
                text: "two things".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
            }]),
            &NoopHost,
        )
        .await
        .expect("cycle runs");

    let reported = result.channel_responses[0]
        .task_id
        .as_deref()
        .expect("the bubble names a card");
    let cards = brain
        .deps
        .tasks
        .as_ref()
        .unwrap()
        .list(&brain.record().id)
        .await
        .unwrap();
    assert_eq!(cards.len(), 2, "both cards are opened either way");
    let first = cards
        .iter()
        .find(|c| c.title == "First")
        .expect("the first card exists");
    assert_eq!(
        reported, first.id,
        "the bubble reports the first card opened, not the last"
    );
}
