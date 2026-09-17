use super::*;

/// Two publishes of the same path within one run extend one record rather
/// than opening two — the working set the loop keeps has to stay current.
#[tokio::test]
async fn republishing_the_same_path_twice_in_one_run_extends_once() {
    use crate::harness::publish::PendingPublish;
    use crate::ports::artifacts::ArtifactStore;

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain_with_artifacts(dir.path());
    let c = card("t-1", "maya");
    let publish = |body: &str| PendingPublish {
        agent: "maya".to_string(),
        source: "spec.md".to_string(),
        title: "spec.md".to_string(),
        kind: crate::ports::artifacts::ArtifactKind::Markdown,
        note: None,
        payload: crate::harness::publish::PublishPayload::Text(body.to_string()),
    };

    brain
        .record_published_artifacts(&c, "maya", vec![publish("draft"), publish("final")], None)
        .await
        .expect("records");

    let listed = ArtifactStore::list(&*ops, &CompanyId::new("acme"), Some("t-1"))
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].versions.len(), 2);
    assert_eq!(listed[0].latest().unwrap().body, "final");
}

/// A store fault on an **explicit** publish propagates. The old path
/// returned a silent `Ok(())`, which made a lost deliverable
/// indistinguishable from a successful one.
#[tokio::test]
async fn a_store_error_on_a_published_file_propagates() {
    use crate::harness::publish::PendingPublish;
    use crate::ports::artifacts::{ArtifactRecord, ArtifactStore};

    /// Reads fine, refuses every write.
    struct BrokenArtifacts;
    #[async_trait]
    impl ArtifactStore for BrokenArtifacts {
        async fn list(&self, _: &CompanyId, _: Option<&str>) -> crate::Result<Vec<ArtifactRecord>> {
            Ok(Vec::new())
        }
        async fn get(&self, _: &CompanyId, _: &str) -> crate::Result<Option<ArtifactRecord>> {
            Ok(None)
        }
        async fn upsert(&self, _: &CompanyId, _: &ArtifactRecord) -> crate::Result<()> {
            Err(crate::error::OpenCompanyError::Store(
                "the disk is full".to_string(),
            ))
        }
        async fn delete(&self, _: &CompanyId, _: &str) -> crate::Result<bool> {
            Ok(false)
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let (mut brain, _ops) = brain_with_artifacts(dir.path());
    // Sole owner at this point — no turn has run, so nothing has cloned the
    // deps into a lane yet.
    Arc::get_mut(&mut brain.deps)
        .expect("the brain is the only holder of its deps before any turn")
        .artifacts = Some(Arc::new(BrokenArtifacts));

    let err = brain
        .record_published_artifacts(
            &card("t-1", "maya"),
            "maya",
            vec![PendingPublish {
                agent: "maya".to_string(),
                source: "spec.md".to_string(),
                title: "spec.md".to_string(),
                kind: crate::ports::artifacts::ArtifactKind::Markdown,
                note: None,
                payload: crate::harness::publish::PublishPayload::Text("# Spec".to_string()),
            }],
            None,
        )
        .await
        .expect_err("a lost deliverable must not read as a success");
    assert!(err.to_string().contains("the disk is full"), "{err}");
}

/// Issue #463, the headline. A publish made when this message already has a
/// card files **onto that card** rather than minting a second one beside it.
///
/// #445 minted unconditionally, which was right on its own and wrong beside
/// #442's card-by-construction: one substantial ask that ended in a
/// published file left two cards, and the reply bubble linked to the empty
/// one because that is the card the turn opened.
#[tokio::test]
async fn a_publish_with_a_card_in_scope_files_onto_it_instead_of_minting() {
    use crate::harness::publish::PendingPublish;
    use crate::ports::artifacts::ArtifactStore;

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain_with_artifacts(dir.path());
    let company = CompanyId::new("acme");
    // The card #442 opens for the work — no owner yet, exactly like the
    // To-do card the REST chat handler writes.
    let mut open = card("t-open", "");
    open.column = COLUMN_TODO.to_string();
    TaskStore::upsert(&*ops, &company, &open)
        .await
        .expect("seed");

    let filed = brain
        .file_publishes_on_card(
            "t-open",
            "writer",
            ChatTarget::default(),
            vec![PendingPublish {
                agent: "writer".to_string(),
                source: "memo.md".to_string(),
                title: "Q3 board memo".to_string(),
                kind: crate::ports::artifacts::ArtifactKind::Markdown,
                note: None,
                payload: crate::harness::publish::PublishPayload::Text("# Memo".to_string()),
            }],
        )
        .await
        .expect("files onto the card");

    assert_eq!(filed, "t-open", "no second card was minted");
    let cards = TaskStore::list(&*ops, &company).await.expect("list");
    assert_eq!(cards.len(), 1, "{cards:?}");
    assert_eq!(
        cards[0].column, COLUMN_IN_REVIEW,
        "a delivered file lands for a person to accept"
    );
    assert_eq!(
        cards[0].assignee, "writer",
        "an unowned card becomes the publisher's"
    );
    assert!(
        cards[0]
            .note
            .as_deref()
            .unwrap_or_default()
            .contains("memo.md"),
        "the note names what landed: {:?}",
        cards[0].note
    );
    let artifacts = ArtifactStore::list(&*ops, &company, Some("t-open"))
        .await
        .expect("list artifacts");
    assert_eq!(artifacts.len(), 1, "the deliverable is ON the card");
    assert_eq!(artifacts[0].title, "Q3 board memo");
}

/// …but filing a file never takes somebody's card away from them. Only an
/// **unowned** card is claimed by the publisher.
#[tokio::test]
async fn filing_a_publish_leaves_an_owned_card_with_its_owner() {
    use crate::harness::publish::PendingPublish;

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain_with_artifacts(dir.path());
    let company = CompanyId::new("acme");
    ops.upsert(&company, &card("t-owned", "maya"))
        .await
        .expect("seed");

    brain
        .file_publishes_on_card(
            "t-owned",
            "writer",
            ChatTarget::default(),
            vec![PendingPublish {
                agent: "writer".to_string(),
                source: "memo.md".to_string(),
                title: "Memo".to_string(),
                kind: crate::ports::artifacts::ArtifactKind::Markdown,
                note: None,
                payload: crate::harness::publish::PublishPayload::Text("# Memo".to_string()),
            }],
        )
        .await
        .expect("files onto the card");

    assert_eq!(only_card(&ops).await.assignee, "maya");
}

/// A card that vanished between the turn and the drain falls back to
/// minting. The rule exists to stop a second card, not to lose a
/// deliverable — dropping the artifact would be #445 all over again.
#[tokio::test]
async fn a_publish_onto_a_card_that_vanished_mints_one_rather_than_dropping_it() {
    use crate::harness::publish::PendingPublish;

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain_with_artifacts(dir.path());
    let company = CompanyId::new("acme");

    let filed = brain
        .file_publishes_on_card(
            "t-gone",
            "writer",
            ChatTarget::channel(Some("strategy")),
            vec![PendingPublish {
                agent: "writer".to_string(),
                source: "memo.md".to_string(),
                title: "Memo".to_string(),
                kind: crate::ports::artifacts::ArtifactKind::Markdown,
                note: None,
                payload: crate::harness::publish::PublishPayload::Text("# Memo".to_string()),
            }],
        )
        .await
        .expect("mints a card instead");

    let cards = TaskStore::list(&*ops, &company).await.expect("list");
    assert_eq!(cards.len(), 1, "the deliverable still has a card");
    assert_eq!(cards[0].assignee, "writer");
    // The returned id must be the REPLACEMENT, not the id that is gone: the
    // caller links the operator's reply to it (#463 review).
    assert_eq!(
        filed, cards[0].id,
        "the returned id must name the card the deliverable landed on"
    );
    assert_ne!(filed, "t-gone");
    // …and it belongs to the same conversation, like the card the
    // no-card-in-scope path mints. Two minting paths must not disagree
    // about where their card posts back.
    assert_eq!(cards[0].origin_chat_id(), Some("strategy"));
}

/// Each artifact records the agent that published **it** (#463 review).
///
/// One drain can hold publishes from more than one agent — the desk lead's
/// turn and the orchestrator's own turn both run with the full toolbelt
/// under a single `Conversation` claim. Collapsing the batch to one author
/// stamps the writer's name on the orchestrator's file and the reverse.
#[tokio::test]
async fn each_published_artifact_records_its_own_author() {
    use crate::harness::publish::PendingPublish;
    use crate::ports::artifacts::ArtifactStore;

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain_with_artifacts(dir.path());
    let company = CompanyId::new("acme");
    let publish = |agent: &str, source: &str| PendingPublish {
        agent: agent.to_string(),
        source: source.to_string(),
        title: source.to_string(),
        kind: crate::ports::artifacts::ArtifactKind::Markdown,
        note: None,
        payload: crate::harness::publish::PublishPayload::Text(format!("# {source}")),
    };

    brain
        .record_published_artifacts(
            &card("t-1", "maya"),
            // The batch-level fallback, which must NOT win over the
            // per-item agents below.
            "maya",
            vec![publish("writer", "memo.md"), publish("ceo", "notes.md")],
            None,
        )
        .await
        .expect("records");

    let mut authors: Vec<(String, String)> = ArtifactStore::list(&*ops, &company, Some("t-1"))
        .await
        .expect("list")
        .into_iter()
        .map(|a| {
            (
                a.source.clone().unwrap_or_default(),
                a.versions[0].author_id.clone(),
            )
        })
        .collect();
    authors.sort();
    assert_eq!(
        authors,
        vec![
            ("memo.md".to_string(), "writer".to_string()),
            ("notes.md".to_string(), "ceo".to_string()),
        ]
    );
}

/// …and a `PendingPublish` built by hand — not by the tool, which always
/// stamps its agent — still falls back to the caller's responder rather
/// than recording a blank author.
#[tokio::test]
async fn a_publish_with_no_agent_falls_back_to_the_responder() {
    use crate::harness::publish::PendingPublish;
    use crate::ports::artifacts::ArtifactStore;

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain_with_artifacts(dir.path());

    brain
        .record_published_artifacts(
            &card("t-1", "maya"),
            "maya",
            vec![PendingPublish {
                agent: String::new(),
                source: "memo.md".to_string(),
                title: "Memo".to_string(),
                kind: crate::ports::artifacts::ArtifactKind::Markdown,
                note: None,
                payload: crate::harness::publish::PublishPayload::Text("# Memo".to_string()),
            }],
            None,
        )
        .await
        .expect("records");

    let listed = ArtifactStore::list(&*ops, &CompanyId::new("acme"), Some("t-1"))
        .await
        .expect("list");
    assert_eq!(listed[0].versions[0].author_id, "maya");
}

/// The other half: a run that did NOT succeed records nothing either, and
/// its note still says what happened.
#[tokio::test]
async fn a_cancelled_delegated_card_records_no_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, ops, _provider) =
        brain_that_steers_itself(dir.path(), "t-cancel", vec![SteerAction::Cancel]);
    let mut c = card("t-cancel", "");
    c.origin = TaskOrigin::new(Some("strategy".to_string()), None);
    ops.upsert(&CompanyId::new("acme"), &c).await.expect("seed");

    brain
        .run_cycle(
            request(vec![CompanyEvent::TaskDispatched {
                task_id: "t-cancel".into(),
                run_id: None,
                origin_chat_id: None,
                origin_parent: None,
            }]),
            &NoopHost,
        )
        .await
        .expect("cycle runs");

    assert_eq!(only_card(&ops).await.column, COLUMN_TODO);
    let artifacts = crate::ports::artifacts::ArtifactStore::list(
        &*ops,
        &CompanyId::new("acme"),
        Some("t-cancel"),
    )
    .await
    .expect("list");
    assert!(
        artifacts.is_empty(),
        "a cancelled run has no deliverable to version"
    );
}

/// **Rewritten by #337.** The landing no longer depends on the card at
/// all: a card's origin used to pick between two success terminals, and now
/// there is one. The decision itself lives in
/// [`crate::ports::tasks::column_for_settled_run`] (and is unit-tested
/// there); this pins that `settle` — every run-ending path in this file —
/// actually consults it, for both card shapes.
#[test]
fn settle_lands_every_finished_card_in_review_whatever_its_origin() {
    let mut board_card = card("t1", "maya");
    settle(&mut board_card, TaskRunEnd::Completed, "maya", "shipped");
    assert_eq!(board_card.column, COLUMN_IN_REVIEW);

    let mut delegated = card("t2", "maya");
    delegated.origin = TaskOrigin::new(Some("strategy".to_string()), None);
    settle(&mut delegated, TaskRunEnd::Completed, "maya", "shipped");
    assert_eq!(
        delegated.column, COLUMN_IN_REVIEW,
        "an origin no longer buys a card its own terminal"
    );
}

/// The redirect-cap finalize branch is the other success ending, so it has
/// to make the same choice — otherwise a steered handoff diverges from an
/// unsteered one.
#[tokio::test]
async fn redirect_cap_finalizes_a_card_with_an_origin_to_review() {
    let dir = tempfile::tempdir().unwrap();
    let redirect = || SteerAction::Redirect {
        instruction: "focus on the API".to_string(),
    };
    let (brain, tasks, _provider) = brain_that_steers_itself(
        dir.path(),
        "t1",
        vec![redirect(), redirect(), redirect(), redirect()],
    );
    let mut c = card("t1", "");
    c.origin = TaskOrigin::new(Some("strategy".to_string()), None);
    tasks.upsert(&CompanyId::new("acme"), &c).await.unwrap();

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

    assert_eq!(only_card(&tasks).await.column, COLUMN_IN_REVIEW);
}

/// The relay has to have wording for the `done` landing column — without it
/// the fallback arm renders the raw column id into the sentence.
#[test]
fn postback_reads_naturally_for_a_done_card() {
    let mut finished = card("t1", "maya");
    finished.column = "done".to_string();
    finished.note = None;
    assert_eq!(
        lifecycle::relay_text(&finished, "maya", "ceo", &[]),
        "\"Ship the thing\" is done (maya ran it)."
    );
}

/// An `assignee` that names a roster member routes the turn to that member.
#[tokio::test]
async fn task_dispatch_routes_to_assignee() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, tasks) = brain_with_tasks(dir.path());
    tasks
        .upsert(&CompanyId::new("acme"), &card("t1", "engineer"))
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
    assert!(note.contains("[engineer]"), "{note:?}");
}

/// The settle. This fixture's pool holds no roster, so the turn errors —
/// which is exactly the `TaskRunEnd::Failed` path — and the row must end
/// **terminal**, carrying the reason the card's note carries, rather than
/// sitting `Pending` for the boot reaper to find.
#[tokio::test]
async fn a_dispatch_settles_its_attempt_row_from_how_the_run_ended() {
    use crate::ports::runs::RunStatus;

    let dir = tempfile::tempdir().unwrap();
    let (brain, tasks, runs) = brain_with_a_pending_run(dir.path(), "engineer").await;
    let company = CompanyId::new("acme");

    brain.run_task("t-1", Some("run-1")).await.expect("run");

    let settled = runs
        .get_run(&company, "run-1")
        .await
        .expect("read")
        .expect("the row survives");
    assert_eq!(settled.status, RunStatus::Failed);
    assert!(
        settled.finished_at_millis.is_some(),
        "a terminal settle stamps when the attempt ended"
    );
    let reason = settled.error.expect("a failure carries its reason");
    assert!(reason.contains("dispatch failed"), "{reason}");
    // …and the row agrees with the card, rather than telling a second story.
    let note = only_card(&tasks).await.note.expect("note");
    assert!(note.contains("dispatch failed"), "{note}");
    // No turn ran on this offline fixture, so there is nothing to charge.
    assert_eq!(settled.step_count, 0);
    assert_eq!(settled.usage, TokenUsage::default());
}

/// Issue #1865 (CodeRabbit review, PR #1883 review comment 3892338104): an
/// ordinary assigned board card — no `origin_chat_id`, so no relay target
/// — whose turn genuinely fails (not a refusal) reaches this same
/// rich-settle tail with a bounce chip but, before this fix, filed no
/// `dispatch_failed` notification. `refuse_dispatch` files this
/// notification for an off-roster assignee, and the cycle's terminality
/// backstop files it for a crash-recovered dispatch — but the backstop
/// explicitly skips any run no longer active, and `settle_run` just above
/// this test's call site already terminalizes the attempt, so the
/// backstop never sees it either. That left an ordinary failed dispatch
/// with no origin chat completely silent: no chat reply, no badge,
/// nothing but the board itself.
#[tokio::test]
async fn an_ordinary_failed_dispatch_with_no_origin_chat_files_a_dispatch_failed_notification() {
    use crate::ports::runs::NewRun;

    let dir = tempfile::tempdir().unwrap();
    let (brain, tasks) = brain_with_tasks_notified(dir.path(), true);
    let runs: Arc<dyn crate::ports::RunStore> = Arc::new(FsOps::new(dir.path()));
    let company = CompanyId::new("acme");
    tasks
        .upsert(&company, &card("t-1", "engineer"))
        .await
        .expect("seed");
    runs.create_run(&company, NewRun::for_task("run-1", "t-1", "engineer"))
        .await
        .expect("mint");
    let brain = brain.with_runs(Arc::clone(&runs));

    brain.run_task("t-1", Some("run-1")).await.expect("run");

    let settled = only_card(&tasks).await;
    assert_eq!(settled.column, COLUMN_TODO);
    assert!(
        settled.bounced.is_some(),
        "an ordinary turn failure must carry the bounce chip: {settled:?}"
    );
    assert!(
        settled.origin_chat_id().is_none(),
        "this is exactly the board-created shape with no relay target: {settled:?}"
    );

    let notes =
        crate::ports::notifications::NotificationStore::list(tasks.as_ref(), &company, "anyone")
            .await
            .expect("list notifications");
    assert!(
        notes
            .iter()
            .any(|n| n.notification.kind == "dispatch_failed"
                && n.notification.subject.id == "t-1"),
        "an ordinary failed dispatch with no origin chat must still file a \
         dispatch_failed notification, got {notes:?}"
    );
}

/// A refusal is an attempt too. It spends nothing and runs no turn, but it
/// is a real, terminal outcome — the card's history must show "this was
/// tried and refused, and why" rather than a gap where an attempt was.
#[tokio::test]
async fn a_refused_dispatch_settles_its_attempt_rather_than_leaving_a_gap() {
    use crate::ports::runs::RunStatus;

    let dir = tempfile::tempdir().unwrap();
    let (brain, _tasks, runs) = brain_with_a_pending_run(dir.path(), "Shane").await;
    let company = CompanyId::new("acme");

    brain.run_task("t-1", Some("run-1")).await.expect("run");

    let settled = runs
        .get_run(&company, "run-1")
        .await
        .expect("read")
        .expect("row");
    assert_eq!(settled.status, RunStatus::Failed);
    let reason = settled.error.expect("reason");
    assert!(reason.contains("dispatch refused"), "{reason}");
    assert!(
        reason.contains("Shane"),
        "the row must name what was wrong, like the note does: {reason}"
    );
}

/// The degraded path stays degraded, not broken: a dispatch carrying no run
/// id runs the card exactly as before and invents no row for it.
#[tokio::test]
async fn an_untracked_dispatch_runs_the_card_and_records_no_attempt() {
    use crate::ports::runs::RunFilter;

    let dir = tempfile::tempdir().unwrap();
    let (brain, tasks) = brain_with_tasks(dir.path());
    let runs: Arc<dyn crate::ports::RunStore> = Arc::new(FsOps::new(dir.path()));
    let brain = brain.with_runs(Arc::clone(&runs));
    let company = CompanyId::new("acme");
    tasks
        .upsert(&company, &card("t-1", "engineer"))
        .await
        .expect("seed");

    brain.run_task("t-1", None).await.expect("run");

    assert!(
        only_card(&tasks).await.note.is_some(),
        "the card still ran and still recorded its outcome"
    );
    assert!(
        runs.list_runs(&company, &RunFilter::default())
            .await
            .expect("list")
            .is_empty(),
        "no row was minted for this dispatch, so none may be invented"
    );
}

/// A card that vanished between the dispatch write and the cycle still
/// closes its attempt. Otherwise the row would sit `Pending` until a
/// restart reaped it with a misleading "the host restarted" reason.
#[tokio::test]
async fn a_dispatch_whose_card_is_gone_still_closes_its_attempt() {
    use crate::ports::runs::{NewRun, RunStatus};

    let dir = tempfile::tempdir().unwrap();
    let (brain, _tasks) = brain_with_tasks(dir.path());
    let runs: Arc<dyn crate::ports::RunStore> = Arc::new(FsOps::new(dir.path()));
    let brain = brain.with_runs(Arc::clone(&runs));
    let company = CompanyId::new("acme");
    runs.create_run(&company, NewRun::for_task("run-1", "t-gone", "engineer"))
        .await
        .expect("mint");

    assert!(
        brain
            .run_task("t-gone", Some("run-1"))
            .await
            .expect("run")
            .is_none(),
        "a missing card posts nothing back"
    );

    let settled = runs
        .get_run(&company, "run-1")
        .await
        .expect("read")
        .expect("row");
    assert_eq!(settled.status, RunStatus::Failed);
    assert_eq!(settled.error.as_deref(), Some(CARD_VANISHED));
}
