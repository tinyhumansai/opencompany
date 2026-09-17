use super::*;

/// The reported bug. A card assigned to "Shane" — nobody this company has —
/// used to dispatch to the orchestrator anyway, keeping `assignee = "Shane"`
/// while the timeline read "reply from ceo" and nothing said the name was
/// invalid. It must now be **refused**: the card goes back to `todo`
/// carrying the reason, and the orchestrator runs no turn on its behalf.
#[tokio::test]
async fn task_dispatch_off_roster_assignee_is_refused_not_silently_reassigned() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, tasks) = brain_with_tasks(dir.path());
    tasks
        .upsert(&CompanyId::new("acme"), &card("t1", "Shane"))
        .await
        .unwrap();

    brain
        .run_cycle(
            request(vec![CompanyEvent::TaskDispatched {
                task_id: "t1".into(),
                run_id: None,
                origin_chat_id: None,
                origin_parent: None,
            }]),
            &NoopHost,
        )
        .await
        .expect("cycle runs");

    let refused = only_card(&tasks).await;
    assert_eq!(
        refused.column, COLUMN_TODO,
        "a card nobody can work must not sit in in_progress"
    );
    assert_eq!(
        refused.assignee, "Shane",
        "the invalid name is left as typed for the operator to correct"
    );
    // Issue #1865 (CodeRabbit review, PR #1883): a refusal is a failed
    // dispatch landing on `todo` exactly like any other, so it must carry
    // the same bounce chip `run_task`'s rich settle and the system mover
    // apply — the board must not read this card any differently just
    // because nobody ever ran.
    assert!(
        refused.bounced.is_some(),
        "an off-roster refusal must set the bounce chip like any other failed dispatch: {refused:?}"
    );
    let note = refused.note.expect("the refusal is written to the note");
    assert!(
        note.contains("Shane"),
        "the operator must be told which name is not a teammate: {note:?}"
    );
    assert!(
        note.contains("dispatch refused"),
        "the note must read as a refusal, not as work the CEO did: {note:?}"
    );
    assert!(
        !note.contains("mock: "),
        "no turn may run for an assignee nobody answers to: {note:?}"
    );
}

/// Issue #1865 (CodeRabbit review, PR #1883 review comment 3878668326): a
/// board-created card (no `origin_chat_id`, exactly [`card`]'s shape) with
/// an off-roster assignee bounces to `todo` and gets the bounce chip
/// (c6c3a3083), but before this fix filed no `dispatch_failed`
/// notification — the relay `refuse_dispatch` falls back to only fires
/// when an `origin_chat_id` exists, and `settle_run_end` makes the
/// attempt terminal before the cycle's own backstop notifier ever sees
/// it. That left the refusal visible only to someone already looking at
/// the board, unlike every other bounced-dispatch path
/// (`CompanyRuntime::abandon_run`, the cycle's terminality backstop, the
/// boot reaper's card sweep, and `workflow_build`'s `settle_to_todo`),
/// which all raise this same notification.
#[tokio::test]
async fn a_refused_dispatch_with_no_origin_chat_files_a_dispatch_failed_notification() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, tasks) = brain_with_tasks_notified(dir.path(), true);
    tasks
        .upsert(&CompanyId::new("acme"), &card("t1", "Shane"))
        .await
        .unwrap();

    brain
        .run_cycle(
            request(vec![CompanyEvent::TaskDispatched {
                task_id: "t1".into(),
                run_id: None,
                origin_chat_id: None,
                origin_parent: None,
            }]),
            &NoopHost,
        )
        .await
        .expect("cycle runs");

    let refused = only_card(&tasks).await;
    assert_eq!(refused.column, COLUMN_TODO);
    assert!(
        refused.origin_chat_id().is_none(),
        "this is exactly the board-created shape with no relay target: {refused:?}"
    );

    let notes = crate::ports::notifications::NotificationStore::list(
        tasks.as_ref(),
        &CompanyId::new("acme"),
        "anyone",
    )
    .await
    .expect("list notifications");
    assert!(
        notes
            .iter()
            .any(|n| n.notification.kind == "dispatch_failed" && n.notification.subject.id == "t1"),
        "a board card refused with no origin chat must still file a \
         dispatch_failed notification, got {notes:?}"
    );
}

/// The other half of #205: a card the orchestrator picks up because nobody
/// was named gets that orchestrator written onto it, so the board names who
/// is actually doing the work instead of showing a blank assignee.
#[tokio::test]
async fn task_dispatch_links_the_working_agent_to_an_unassigned_card() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, tasks) = brain_with_tasks(dir.path());
    tasks
        .upsert(&CompanyId::new("acme"), &card("t1", ""))
        .await
        .unwrap();

    brain
        .run_cycle(
            request(vec![CompanyEvent::TaskDispatched {
                task_id: "t1".into(),
                run_id: None,
                origin_chat_id: None,
                origin_parent: None,
            }]),
            &NoopHost,
        )
        .await
        .expect("cycle runs");

    assert_eq!(
        only_card(&tasks).await.assignee,
        "ceo",
        "the orchestrator that worked the card must be linked to it"
    );
}

/// A card assigned to a **desk** is worked by that desk's lead, but the card
/// stays the desk's. `delegate_to_desk` writes a desk id into `assignee`, so
/// this is the shape a hand-off actually produces. Dispatch picks who runs
/// this turn, not who owns the card, so only the note names the member that
/// ran it — relinking the lead onto `assignee` would erase the desk from the
/// board the first time the card ran (#214 review).
#[tokio::test]
async fn task_dispatch_routes_a_desk_assignee_to_its_lead_but_keeps_the_desk() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, tasks) = brain_with_desk_tasks(dir.path());
    tasks
        .upsert(&CompanyId::new("acme"), &card("t1", "eng"))
        .await
        .unwrap();

    brain
        .run_cycle(
            request(vec![CompanyEvent::TaskDispatched {
                task_id: "t1".into(),
                run_id: None,
                origin_chat_id: None,
                origin_parent: None,
            }]),
            &NoopHost,
        )
        .await
        .expect("cycle runs");

    let worked = only_card(&tasks).await;
    assert_eq!(
        worked.assignee, "eng",
        "a desk assignment is ownership: the card stays the desk's"
    );
    let note = worked.note.expect("note");
    assert!(
        note.contains("[engineer]"),
        "the desk's lead member still did the work, and the note names them: {note:?}"
    );
}

/// An **operator-overlay** teammate is a roster teammate. The narrow
/// `manifest.agents`-only lookup used to drop these onto the orchestrator.
#[tokio::test]
async fn task_dispatch_routes_to_an_overlay_teammate() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, tasks) = brain_with_tasks(dir.path());
    brain.mutate_record(|r| {
        r.overlay_agents.push(OverlayAgent {
            provider: None,
            id: "nova".into(),
            name: "Nova".into(),
            role: "Growth".into(),
            description: None,
            tools: None,
            model: None,
            harness: None,
        })
    });
    tasks
        .upsert(&CompanyId::new("acme"), &card("t1", "nova"))
        .await
        .unwrap();

    brain
        .run_cycle(
            request(vec![CompanyEvent::TaskDispatched {
                task_id: "t1".into(),
                run_id: None,
                origin_chat_id: None,
                origin_parent: None,
            }]),
            &NoopHost,
        )
        .await
        .expect("cycle runs");

    let note = only_card(&tasks).await.note.expect("note");
    assert!(
        note.contains("[nova]"),
        "an overlay teammate must work their own card: {note:?}"
    );
}

/// A refused dispatch still answers in the thread the card came from —
/// otherwise a delegated hand-off to a bad assignee is silent twice over.
#[tokio::test]
async fn a_refused_dispatch_posts_the_reason_back_to_its_origin() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, tasks) = brain_with_tasks(dir.path());
    let mut c = card("t-origin", "Shane");
    c.origin = TaskOrigin::new(Some("strategy".to_string()), None);
    tasks
        .upsert(&CompanyId::new("acme"), &c)
        .await
        .expect("seed");

    let posted = brain
        .run_task("t-origin", None)
        .await
        .expect("run")
        .expect("a refused card with an origin must still post back");
    assert_eq!(
        posted.reply_to.as_ref().map(|r| r.chat_id.as_str()),
        Some("strategy")
    );
    assert_eq!(
        posted.channel,
        brain.orchestrator(),
        "the orchestrator answers for its own roster"
    );
    assert!(posted.text.contains("Shane"), "{}", posted.text);
    // Issue #1852: `refuse_dispatch` relays through the same
    // `relay_reply` as a settled run, so it carries the card id too — but
    // `CompanyRuntime::journal_dispatch_replies` strips it back to `None`
    // before journaling, since the settle already left a
    // `DeskTaskCompleted` link and this would only duplicate it.
    assert_eq!(posted.task_id.as_deref(), Some("t-origin"));
}

/// A refusal into a private DM must not claim anyone ran the card.
///
/// `refuse_dispatch` used to pass the orchestrator's own id into
/// `relay_reply`'s `responder` slot while the DM's speaker went into the
/// `orchestrator` slot — the opposite of every other call site. For a
/// desk/shared origin the two values collide (`relay_speaker` returns the
/// orchestrator) and the swap is invisible, but a private DM's speaker is
/// the teammate, not the orchestrator, so the mismatch fires the "ran it"
/// credit onto a card that never ran at all (CodeRabbit review, PR #1949
/// thread 3895107568).
#[tokio::test]
async fn a_refused_dispatch_into_a_dm_credits_no_one() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, tasks) = brain_with_tasks(dir.path());
    let mut c = card("t-dm-origin", "Shane");
    // "engineer" is a real roster agent (not the orchestrator "ceo"), so
    // `relay_speaker` claims this as a private DM and returns "engineer"
    // instead of falling back to the orchestrator.
    c.origin = TaskOrigin::new(Some("engineer".to_string()), None);
    tasks
        .upsert(&CompanyId::new("acme"), &c)
        .await
        .expect("seed");

    let posted = brain
        .run_task("t-dm-origin", None)
        .await
        .expect("run")
        .expect("a refused card with an origin must still post back");
    assert!(
        !posted.text.contains("ran it"),
        "a refusal must never credit anyone with running the card: {}",
        posted.text
    );
}

/// A dispatch for a card that no longer exists is a silent no-op, not an
/// error.
#[tokio::test]
async fn task_dispatch_missing_card_is_noop() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, tasks) = brain_with_tasks(dir.path());
    brain
        .run_cycle(
            request(vec![CompanyEvent::TaskDispatched {
                task_id: "nope".into(),
                run_id: None,
                origin_chat_id: None,
                origin_parent: None,
            }]),
            &NoopHost,
        )
        .await
        .expect("cycle runs without a card");
    assert!(
        tasks
            .list(&CompanyId::new("acme"))
            .await
            .unwrap()
            .is_empty()
    );
}

/// A dispatch relay into a teammate's private DM is authored by that
/// teammate; every shared surface keeps the orchestrator's voice.
#[test]
fn relay_speaker_claims_a_private_dm_for_its_own_agent() {
    let record = record_with_desk();
    // A teammate DM: the origin is that teammate, so they speak.
    assert_eq!(relay_speaker(&record, "engineer", "chief"), "engineer");
    // The console's `dm:<id>` channel key resolves the same teammate.
    assert_eq!(relay_speaker(&record, "dm:engineer", "chief"), "engineer");
    // A desk is a shared surface — the orchestrator stays the one voice.
    assert_eq!(relay_speaker(&record, "eng_desk", "chief"), "chief");
    // The orchestrator's own DM is answered by the orchestrator, not doubled.
    assert_eq!(relay_speaker(&record, "chief", "chief"), "chief");
    // General / empty / unknown origins all keep the orchestrator.
    assert_eq!(relay_speaker(&record, "General", "chief"), "chief");
    assert_eq!(relay_speaker(&record, "", "chief"), "chief");
    assert_eq!(relay_speaker(&record, "nobody-here", "chief"), "chief");
}

/// PR #1949 review (Codex thread 3895066480): a `dm:` key names a
/// teammate even when a desk shares that id — the same invariant issue
/// #1743 established for `chat_responder`
/// (`runtime::delegation_tools::a_prefixed_dm_reaches_the_teammate_even_
/// when_a_desk_shares_the_id`). `relay_speaker` used to re-run the bare,
/// desk-first `assignee::resolve` on the key once the prefix was
/// stripped, so a card dispatched from that teammate's private DM
/// resolved to `Desk` and fell through to the orchestrator — reopening
/// #1743's bug in the relay's own resolver, and misattributing a private
/// DM's card as though it were answered on the shared desk.
#[test]
fn relay_speaker_reaches_the_dm_teammate_even_when_a_desk_shares_the_id() {
    let record = record_with_colliding_desk_and_teammate_id();
    assert_eq!(
        relay_speaker(&record, "dm:engineer", "chief"),
        "engineer",
        "the prefix names the teammate, not the desk that shares its id"
    );
    // The bare key still belongs to the desk, exactly as it does for
    // `chat_responder` — only the prefixed address reaches the teammate.
    assert_eq!(
        relay_speaker(&record, "engineer", "chief"),
        "chief",
        "the desk still answers its own bare id"
    );
}

/// Issue #707, the retention half: a store with **no persisted record**
/// leaves the brain's record exactly as it was.
///
/// `Ok(None)` is not a failure and must not be treated as one — an absent
/// record is what a company whose bundle has not been written yet looks
/// like, and clearing on it would leave that company with no roster and no
/// desks at all. Uses the real `FsCompanyStore` over a directory nothing was
/// saved to, which is precisely the shape that returns `Ok(None)`.
#[tokio::test]
async fn a_refresh_with_no_persisted_record_keeps_the_one_it_has() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, _tasks) = brain_with_desk(dir.path());
    let before = brain.record();

    brain
        .refresh_record()
        .await
        .expect("an absent record is not an error");

    let after = brain.record();
    assert_eq!(
        after.manifest.company.name, before.manifest.company.name,
        "the brain kept its record"
    );
    assert_eq!(
        after.manifest.agents.len(),
        before.manifest.agents.len(),
        "an absent record must not empty the roster"
    );
    assert_eq!(
        delegation::desk_lead(&brain.record(), "eng_desk"),
        Some("engineer".to_string()),
        "nor cost the company its desks"
    );
}

/// Issue #707, the loud half: a store that **cannot be read** fails the
/// refresh rather than falling back to the record already in hand.
///
/// Falling back is the defect this whole change removes, and it would come
/// back invisibly — a turn that looked successful while routing on state the
/// operator had already replaced. So the error propagates and the cycle
/// fails. A corrupt `company.toml` is a real way to reach that arm, and
/// reaching it through the real store rather than a double is what keeps
/// this test honest about the failure it claims to cover.
#[tokio::test]
async fn a_refresh_that_cannot_read_the_store_fails_rather_than_going_stale() {
    use crate::ports::store::CompanyStore;

    let dir = tempfile::tempdir().unwrap();
    let (brain, _tasks) = brain_with_desk(dir.path());
    let store = FsCompanyStore::new(dir.path());
    store.save(&brain.record()).await.expect("seed the record");

    // A bundle whose manifest no longer parses: the store reports a failure
    // rather than an absence, which is the arm under test.
    let toml_path =
        crate::store::Bundle::new(dir.path().to_path_buf(), &brain.record().id).company_toml();
    tokio::fs::write(&toml_path, b"this is not = valid toml [[[")
        .await
        .expect("corrupt the manifest");

    let err = brain
        .refresh_record()
        .await
        .expect_err("an unreadable record must fail the refresh, not be ignored");
    assert!(
        err.to_string().contains("company.toml"),
        "the failure names what could not be read: {err}"
    );

    // And through `run_cycle`, which is the level that actually protects
    // the promise. Asserting only on `refresh_record` would leave the call
    // site free to become `let _ = self.refresh_record().await;` — the
    // refresh would still run, the error would be dropped, the turn would
    // report success while routing on the record it already held, and every
    // other test here would stay green. That is issue #707 returning by a
    // different door, so the propagation is pinned where it is relied on.
    let cycle = brain.run_cycle(request(Vec::new()), &NoopHost).await;
    assert!(
        cycle.is_err(),
        "a cycle must fail when the record cannot be read, rather than \
         quietly routing on a stale one"
    );
}

/// The whole point of the feature: `@engineer` in the main line is answered
/// by the engineer, not by the orchestrator that would otherwise take it.
#[test]
fn a_mentioned_teammate_answers_instead_of_the_default_responder() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, _tasks) = brain_with_desk(dir.path());
    // Without a mention, the orchestrator answers an unaddressed message.
    assert_eq!(brain.responder_for(None), "chief");
    // With one, the named teammate does.
    assert_eq!(
        crate::runtime::mentions::mention_responder(
            &brain.record(),
            None,
            &[mention_of("engineer")]
        ),
        Some("engineer".to_string()),
    );
}

/// And it outranks the *desk lead*, which is the stronger claim: a message
/// addressed to a desk still goes to the teammate it names.
#[test]
fn a_mention_outranks_the_addressed_desks_lead() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, _tasks) = brain_with_desk(dir.path());
    assert_eq!(brain.responder_for(Some("eng_desk")), "engineer");
    assert_eq!(
        crate::runtime::mentions::mention_responder(&brain.record(), None, &[mention_of("ceo")]),
        Some("ceo".to_string()),
        "the named teammate answers even on a desk with its own lead",
    );
}

/// A message that mentions nobody routes exactly as it did before mentions
/// existed — which is every message already in every journal.
#[test]
fn a_message_with_no_mentions_routes_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, _tasks) = brain_with_desk(dir.path());
    assert_eq!(
        crate::runtime::mentions::mention_responder(&brain.record(), None, &[]),
        None,
        "so the caller falls through to responder_for",
    );
    assert_eq!(brain.responder_for(Some("eng_desk")), "engineer");
    assert_eq!(brain.responder_for(None), "chief");
}

/// `@everyone` names the addressed desk's teammates for the turn's context
/// — and still leaves exactly one responder, because it is a list and not a
/// fan-out.
#[test]
fn everyone_names_the_desk_without_choosing_a_responder() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, _tasks) = brain_with_desk(dir.path());
    let mentions = [crate::ports::types::Mention {
        target: crate::ports::types::MentionTarget::Everyone,
        text: "@everyone".to_string(),
        offset: 0,
        quiet: false,
    }];
    assert_eq!(
        crate::runtime::mentions::mention_responder(&brain.record(), None, &mentions),
        None,
        "a broadcast names no single teammate, so the desk lead still answers",
    );
    assert_eq!(
        crate::runtime::mentions::mentioned_agents(
            &brain.record(),
            "eng_desk",
            &mentions,
            Some("engineer"),
        ),
        Vec::<String>::new(),
        "the only member is the responder, and it is not told it was mentioned",
    );
}

/// `@everyone` from the console's default thread (`chat: "main"`) expands
/// against the General desk, not no desk at all. The console-only alias is
/// not a desk key `resolve_desk_id` knows, so the brain folds it — and the
/// other General-desk spellings — to the General desk id before expanding.
#[test]
fn everyone_desk_folds_the_main_thread_alias_to_general() {
    let record = record_with_desk();
    assert_eq!(HarnessBrain::everyone_desk(&record, None), "General");
    assert_eq!(HarnessBrain::everyone_desk(&record, Some("")), "General");
    assert_eq!(
        HarnessBrain::everyone_desk(&record, Some("main")),
        "General"
    );
    assert_eq!(
        HarnessBrain::everyone_desk(&record, Some("General")),
        "General"
    );
    assert_eq!(
        HarnessBrain::everyone_desk(&record, Some("eng_desk")),
        "eng_desk"
    );
}

/// A blueprint that declares a desk under one of the General spellings is
/// grandfathered by this host — `is_general_channel` is guarded on
/// `!desk_exists`, the desk keeps its members, and `responder_for` routes
/// to its lead. The fold must not run over it: asking `resolve_desk_id`
/// for the *name* `General` misses a desk called anything else, and
/// `@everyone` would then expand to the whole roster instead of the two
/// people actually on the line — a broadcast escaping the scope of the one
/// case the fold exists to preserve.
#[test]
fn a_grandfathered_general_desk_keeps_its_own_membership() {
    let manifest: CompanyManifest = toml::from_str(
        r#"
[company]
name = "Acme"

[[agent]]
id = "ceo"
role = "Chief Executive"

[[agent]]
id = "chief"
role = "Chief of Staff"
tier = "orchestrator"

[[agent]]
id = "engineer"
role = "Engineer"

[[group_chat]]
id = "main"
name = "Front office"
members = ["ceo", "engineer"]
"#,
    )
    .expect("valid manifest");
    let mut record = record_with_desk();
    record.manifest.group_chats = manifest.group_chats;

    // The raw key, not the General fold: this desk answers to it.
    assert_eq!(HarnessBrain::everyone_desk(&record, Some("main")), "main");

    // Every folded alias names the same membership as the raw key. A desk
    // that claims the line by *id* is missed by `resolve_desk_id("General")`,
    // so the alias used to fall through to a `General` desk that does not
    // exist — scoping `@everyone` to the whole roster in a channel whose
    // own lead answers (issue #1743).
    for alias in ["", "General", "general", "MAIN"] {
        assert_eq!(
            HarnessBrain::everyone_desk(&record, Some(alias)),
            "main",
            "the alias {alias:?} must scope @everyone to the claiming desk"
        );
    }
    // And with no such desk, the fold still applies as before.
    assert_eq!(
        HarnessBrain::everyone_desk(&record_with_desk(), Some("main")),
        "General"
    );

    let mentions = [crate::ports::types::Mention {
        target: crate::ports::types::MentionTarget::Everyone,
        text: "@everyone".to_string(),
        offset: 0,
        quiet: false,
    }];
    let expanded = crate::runtime::mentions::mentioned_agents(
        &record,
        &HarnessBrain::everyone_desk(&record, Some("main")),
        &mentions,
        None,
    );
    assert_eq!(
        expanded,
        vec!["ceo".to_string(), "engineer".to_string()],
        "a broadcast stays inside the desk that was addressed"
    );
    assert!(
        !expanded.contains(&"chief".to_string()),
        "and does not reach a teammate who is not on it: {expanded:?}"
    );
}
