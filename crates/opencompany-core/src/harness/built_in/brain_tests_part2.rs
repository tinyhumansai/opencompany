use super::*;

/// The "zero tool work" claim in #552, proven rather than asserted: a
/// second agent reads the first agent's deliverable through the ordinary
/// `workspace_read` path, with nothing published-specific involved.
///
/// The read goes through the *same* index-and-resolve the tool uses (a
/// company-scoped `tree()` then a `read()` by id), so what this pins is
/// that the node is reachable by path from the shared tree — which is
/// exactly what makes it readable by every teammate.
#[tokio::test]
async fn a_second_agent_can_read_what_the_first_published() {
    use crate::harness::publish::PendingPublish;
    use crate::ports::workspace::WorkspaceStore;

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain_with_artifacts_and_workspace(dir.path());
    let company = CompanyId::new("acme");

    brain
        .record_published_artifacts(
            &card("t-1", "maya"),
            "maya",
            vec![PendingPublish {
                agent: "maya".to_string(),
                source: "launch.md".to_string(),
                title: "Launch spec".to_string(),
                kind: crate::ports::artifacts::ArtifactKind::Markdown,
                note: None,
                payload: crate::harness::publish::PublishPayload::Text(
                    "what maya produced".to_string(),
                ),
            }],
            None,
        )
        .await
        .expect("records");

    // Agent B knows nothing about the artifact. It walks the shared tree by
    // path, exactly as `workspace_list` / `workspace_read` do.
    let nodes = WorkspaceStore::tree(&*ops, &company).await.unwrap();
    let name_of = |id: &str| nodes.iter().find(|n| n.id == id).map(|n| n.name.clone());
    let found = nodes
        .iter()
        .find(|n| {
            n.name == "launch.md"
                && n.parent_id
                    .as_deref()
                    .and_then(name_of)
                    // Issue #1687: the task folder is named for the work
                    // and keyed by the card id — `<title>.<id>`, not the
                    // bare id — so browsing by path lands on that name.
                    .is_some_and(|parent| parent == "ship-the-thing.t-1")
        })
        .expect("agent B finds the deliverable by browsing the shared tree");

    let (_, body) = WorkspaceStore::read(&*ops, &company, &found.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(body, "what maya produced");
}

/// A re-publish revises the SAME node rather than opening a rival beside
/// it, so the operator's open tab and any link to it keep working — and
/// the new version carries the same node id, which is what lets the next
/// re-publish find it again.
#[tokio::test]
async fn a_republish_updates_the_same_node() {
    use crate::harness::publish::PendingPublish;
    use crate::ports::artifacts::ArtifactStore;
    use crate::ports::workspace::WorkspaceStore;

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain_with_artifacts_and_workspace(dir.path());
    let company = CompanyId::new("acme");
    let c = card("t-1", "maya");
    let publish = |body: &str| PendingPublish {
        agent: "maya".to_string(),
        source: "specs/launch.md".to_string(),
        title: "Launch spec".to_string(),
        kind: crate::ports::artifacts::ArtifactKind::Markdown,
        note: None,
        payload: crate::harness::publish::PublishPayload::Text(body.to_string()),
    };

    brain
        .record_published_artifacts(&c, "maya", vec![publish("v1")], Some("run-1"))
        .await
        .unwrap();
    let first_node = ArtifactStore::list(&*ops, &company, Some("t-1"))
        .await
        .unwrap()[0]
        .workspace_node_id()
        .expect("v1 named a node")
        .to_string();
    let tree_before = WorkspaceStore::tree(&*ops, &company).await.unwrap().len();

    brain
        .record_published_artifacts(&c, "maya", vec![publish("v2")], Some("run-2"))
        .await
        .unwrap();

    let record = ArtifactStore::list(&*ops, &company, Some("t-1"))
        .await
        .unwrap()[0]
        .clone();
    assert_eq!(record.versions.len(), 2, "one record, two versions");
    assert_eq!(
        record.workspace_node_id(),
        Some(first_node.as_str()),
        "the second version must name the node the first one already had"
    );
    assert_eq!(
        WorkspaceStore::tree(&*ops, &company).await.unwrap().len(),
        tree_before,
        "a re-publish must create no new nodes"
    );
    assert_eq!(
        WorkspaceStore::read(&*ops, &company, &first_node)
            .await
            .unwrap()
            .unwrap()
            .1,
        "v2",
        "the node holds the current body"
    );
}

/// An unwired workspace must behave **exactly** as before this cell: the
/// artifact is recorded, nothing is attempted against a tree that does not
/// exist, and no version claims a node.
///
/// This is what keeps every pre-#552 publish test honest, since they all
/// run on this path.
#[tokio::test]
async fn without_a_workspace_store_the_publish_path_is_unchanged() {
    use crate::harness::publish::PendingPublish;
    use crate::ports::artifacts::ArtifactStore;

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain_with_artifacts(dir.path());
    let company = CompanyId::new("acme");

    brain
        .record_published_artifacts(
            &card("t-1", "maya"),
            "maya",
            vec![PendingPublish {
                agent: "maya".to_string(),
                source: "specs/launch.md".to_string(),
                title: "Launch spec".to_string(),
                kind: crate::ports::artifacts::ArtifactKind::Markdown,
                note: None,
                payload: crate::harness::publish::PublishPayload::Text("body".to_string()),
            }],
            None,
        )
        .await
        .expect("the artifact is still recorded");

    let listed = ArtifactStore::list(&*ops, &company, Some("t-1"))
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].workspace_node_id(),
        None,
        "with no tree to mirror into, a version names no node"
    );
}

/// A deliverable is never dropped for tree bookkeeping. When the node
/// cannot be written — here an operator's *file* squatting the `Artifacts`
/// root name, which the fail-closed resolver refuses rather than guesses —
/// the artifact is still recorded, just without a node id.
///
/// The opposite behaviour (propagating the error) would lose an explicitly
/// published file because a folder could not be made, which is the worse
/// of the two failures by a wide margin.
#[tokio::test]
async fn a_failed_node_write_still_records_the_artifact() {
    use crate::company::workspace_scaffold::ARTIFACTS_ROOT;
    use crate::harness::publish::PendingPublish;
    use crate::ports::artifacts::ArtifactStore;
    use crate::ports::workspace::{NodeKind, WorkspaceNode, WorkspaceOrigin, WorkspaceStore};

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain_with_artifacts_and_workspace(dir.path());
    let company = CompanyId::new("acme");

    // A *file* named `Artifacts` at the workspace root — the root a publish
    // now resolves through. The minter refuses to resolve a folder through
    // it rather than clobbering an operator's note.
    WorkspaceStore::create(
        &*ops,
        &company,
        &WorkspaceNode {
            id: crate::ports::generate_id(),
            name: ARTIFACTS_ROOT.to_string(),
            kind: NodeKind::File,
            parent_id: None,
            updated_at_millis: now_millis(),
            created_by: WorkspaceOrigin::Operator,
            updated_by: WorkspaceOrigin::Operator,
            mime: None,
            size: None,
            sha256: None,
            adopted: false,
        },
        Some("an operator's note, in the way"),
    )
    .await
    .unwrap();

    let written = brain
        .record_published_artifacts(
            &card("t-1", "maya"),
            "maya",
            vec![PendingPublish {
                agent: "maya".to_string(),
                source: "launch.md".to_string(),
                title: "Launch spec".to_string(),
                kind: crate::ports::artifacts::ArtifactKind::Markdown,
                note: None,
                payload: crate::harness::publish::PublishPayload::Text(
                    "the deliverable".to_string(),
                ),
            }],
            None,
        )
        .await
        .expect("a tree that refuses must not fail the publish");

    assert_eq!(written.len(), 1, "the deliverable is still recorded");
    let listed = ArtifactStore::list(&*ops, &company, Some("t-1"))
        .await
        .unwrap();
    assert_eq!(listed[0].latest().unwrap().body, "the deliverable");
    assert_eq!(
        listed[0].workspace_node_id(),
        None,
        "no node was written, so no version may claim one"
    );
}
fn card(id: &str, assignee: &str) -> TaskRecord {
    TaskRecord {
        id: id.to_string(),
        title: TaskTitle::authored("Ship the thing"),
        note: None,
        column: "in_progress".to_string(),
        priority: "high".to_string(),
        assignee: assignee.to_string(),
        updated_at_millis: 0,
        origin: None,
        parent_task_id: None,
        output: None,
        plan: None,
        planning_attempts: Vec::new(),
        deliverable: crate::ports::tasks::TaskDeliverable::Once,
        workflow_proposal: None,
        origin_run_id: None,
        origin_workflow_id: None,
        origin_message_seq: None,
        bounced: None,
    }
}

/// The compatibility guarantee: a card with no remembered origin — one made
/// straight on the board, or written before `origin_chat_id` existed —
/// posts back nowhere and behaves exactly as it did before.
#[tokio::test]
async fn a_card_with_no_origin_posts_back_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, tasks) = brain_with_tasks(dir.path());
    // A roster assignee: since #205 an off-roster one is refused outright,
    // which would satisfy the no-post-back assertion below without ever
    // running the dispatch this test is about.
    let mut c = card("t-no-origin", "engineer");
    c.origin = TaskOrigin::new(None, None);
    tasks
        .upsert(&CompanyId::new("acme"), &c)
        .await
        .expect("seed");

    let posted = brain.run_task("t-no-origin", None).await.expect("run");
    assert!(
        posted.is_none(),
        "a card with no originating thread must not post back"
    );
    // The note is still the durable record.
    assert!(only_card(&tasks).await.note.is_some());
}

/// …and one that does remember its origin answers there, threaded with
/// `reply_to` and — since issue #186 — attributed to the **orchestrator**
/// rather than to the assignee that did the work.
#[tokio::test]
async fn a_card_with_an_origin_posts_back_to_that_thread() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, tasks) = brain_with_tasks(dir.path());
    // A roster assignee, deliberately: an off-roster one falls back to the
    // default responder (`task_responder`), which in this fixture *is* the
    // orchestrator — so the credit would be correctly suppressed and this
    // test would prove nothing about the one-voice relay.
    let mut c = card("t-origin", "engineer");
    c.origin = TaskOrigin::new(Some("strategy".to_string()), None);
    tasks
        .upsert(&CompanyId::new("acme"), &c)
        .await
        .expect("seed");

    let posted = brain
        .run_task("t-origin", None)
        .await
        .expect("run")
        .expect("a card with an origin must post back");
    assert_eq!(
        posted.reply_to.as_ref().map(|r| r.chat_id.as_str()),
        Some("strategy")
    );
    // Issue #186: one voice. The bubble belongs to the orchestrator, and
    // the assignee that ran the card is credited in the text instead of
    // speaking to the operator directly.
    assert_eq!(
        posted.channel,
        brain.orchestrator(),
        "the orchestrator relays a finished card, not the assignee"
    );
    assert_ne!(
        posted.channel, "engineer",
        "the assignee must not address the operator directly"
    );
    assert!(posted.text.contains("Ship the thing"), "{}", posted.text);
    assert!(
        posted.text.contains("engineer"),
        "the relay must still credit who did the work: {}",
        posted.text
    );
    // A dispatched card discards its steps into the note.
    assert!(posted.steps.is_empty());
}

/// Issue #1890 B: and the **terminal** carries both halves of that origin.
///
/// The relay bubble above answers in the origin thread on its own; the
/// marker is the structural half, and it is the one that was landing in the
/// wrong place. `desk` is a responder id and a channel is a desk id, so
/// nothing on this event could recover either half — it is captured off the
/// card at the single settle emission point every dispatch ending passes
/// through, which is why capturing it there cannot miss a path.
#[tokio::test]
async fn a_settled_card_journals_the_thread_it_was_raised_in() {
    use crate::ports::EventSeq;
    let dir = tempfile::tempdir().unwrap();
    let (brain, tasks, events) = brain_with_tasks_and_events(dir.path());
    let mut c = card("t-threaded", "engineer");
    c.origin = TaskOrigin::new(Some("strategy".to_string()), Some(EventSeq::new(41)));
    tasks
        .upsert(&CompanyId::new("acme"), &c)
        .await
        .expect("seed");

    brain.run_task("t-threaded", None).await.expect("run");

    let logged = events
        .read_from(&CompanyId::new("acme"), EventSeq::new(0), usize::MAX)
        .await
        .expect("read events");
    let terminal = logged
        .iter()
        .find_map(|e| match &e.event {
            CompanyEvent::DeskTaskCompleted {
                origin_chat_id,
                origin_parent,
                ..
            } => Some((origin_chat_id.clone(), *origin_parent)),
            _ => None,
        })
        .expect("the settle journals a terminal");
    assert_eq!(
        terminal,
        (Some("strategy".to_string()), Some(EventSeq::new(41))),
        "the terminal carries the channel AND the thread the card recorded",
    );
}

/// …and a card raised at channel level still settles flat there. `None` is
/// the channel-level conversation, not a lost id, and a marker that started
/// threading itself onto an unrelated root would be worse than no marker.
#[tokio::test]
async fn a_settled_channel_level_card_journals_no_thread() {
    use crate::ports::EventSeq;
    let dir = tempfile::tempdir().unwrap();
    let (brain, tasks, events) = brain_with_tasks_and_events(dir.path());
    let mut c = card("t-flat", "engineer");
    c.origin = TaskOrigin::new(Some("strategy".to_string()), None);
    tasks
        .upsert(&CompanyId::new("acme"), &c)
        .await
        .expect("seed");

    brain.run_task("t-flat", None).await.expect("run");

    let logged = events
        .read_from(&CompanyId::new("acme"), EventSeq::new(0), usize::MAX)
        .await
        .expect("read events");
    assert!(
        logged.iter().any(|e| matches!(
            &e.event,
            CompanyEvent::DeskTaskCompleted {
                origin_chat_id,
                origin_parent: None,
                ..
            } if origin_chat_id.as_deref() == Some("strategy")
        )),
        "an unthreaded settle names its channel and no thread: {logged:?}"
    );
}

/// A dispatched **board-created** card (no `origin_chat_id`) runs a turn and
/// moves to `in_review` — the operator who made it is the reviewer — with
/// its result folded into the note under the responder that ran it.
#[tokio::test]
async fn task_dispatch_runs_and_moves_to_in_review() {
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

    let moved = only_card(&tasks).await;
    assert_eq!(moved.column, "in_review");
    let note = moved.note.expect("result written to note");
    // Default responder (first roster agent) ran it, and the mock provider
    // echoes the instruction (the card title) back into the reply.
    assert!(note.contains("[ceo]"), "{note:?}");
    assert!(note.contains("Ship the thing"), "{note:?}");
}

/// **Rewritten by #337.** This used to pin the opposite: a card spawned by
/// a delegating turn (so it carries an `origin_chat_id`) completed straight
/// to `done`, on #171's argument that nobody was watching the board for it.
///
/// The operator decision of 2026-08-05 removed every automatic route to
/// Done, so it now stops in `in_review` like any other card. The thing #171
/// actually cared about — that the originating conversation gets its answer
/// rather than waiting on a board nobody is reading — is unaffected and is
/// asserted here: the post-back still fires, and it now says the card is
/// ready for review instead of claiming it is finished.
#[tokio::test]
async fn dispatched_card_with_an_origin_stops_in_review_and_still_posts_back() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, tasks) = brain_with_tasks(dir.path());
    // A roster assignee: since #205 an off-roster one never runs a turn, so
    // it would settle to `todo` and prove nothing about the terminal.
    let mut c = card("t-origin", "engineer");
    c.origin = TaskOrigin::new(Some("strategy".to_string()), None);
    tasks
        .upsert(&CompanyId::new("acme"), &c)
        .await
        .expect("seed");
    // `run_task` is driven directly here rather than through `run_cycle`,
    // so the roster the turn runs on has to be built explicitly. Without it
    // every dispatch fails with "company not found" and settles to
    // `todo` — which still satisfies this test's post-back assertions
    // while proving nothing about the terminal column.
    brain
        .pool
        .ensure(&brain.record(), &brain.deps)
        .await
        .expect("roster");

    let posted = brain
        .run_task("t-origin", None)
        .await
        .expect("run")
        .expect("a card with an origin posts back");

    let moved = only_card(&tasks).await;
    assert_eq!(
        moved.column, COLUMN_IN_REVIEW,
        "Done is a person's decision; no dispatch may reach it on its own"
    );
    // The note stays the durable record of what came back.
    assert!(moved.note.expect("note").contains("Ship the thing"));
    // …and the bubble answers in the originating thread either way, which
    // is what the handoff was actually waiting on.
    assert!(posted.text.contains("ready for review"), "{}", posted.text);
    assert!(!posted.text.contains("is done"), "{}", posted.text);
}

/// **The headline of #244, stated as its own test.** This used to assert
/// the opposite — that a completed dispatch always mints an artifact from
/// its chat reply.
///
/// It does not any more. A run that published nothing yields **no
/// artifact**, and that is a first-class outcome rather than a gap. The old
/// behaviour is exactly what made the Artifacts tab present refusals and
/// blocker messages as deliverables: capture was gated on run disposition
/// and never on whether anything had been produced.
///
/// Nothing is lost. The reply still reaches the card note, the timeline, the
/// completion event and the run trace — five records, none of which claims
/// to be a deliverable.
#[tokio::test]
async fn a_completed_run_that_published_nothing_records_no_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain_with_artifacts(dir.path());
    // Empty assignee → the default responder, so the turn actually runs.
    let mut c = card("t-origin", "");
    c.origin = TaskOrigin::new(Some("strategy".to_string()), None);
    ops.upsert(&CompanyId::new("acme"), &c).await.expect("seed");

    brain
        .run_cycle(
            request(vec![CompanyEvent::TaskDispatched {
                task_id: "t-origin".into(),
                run_id: None,
                origin_chat_id: None,
                origin_parent: None,
            }]),
            &NoopHost,
        )
        .await
        .expect("cycle runs");

    let moved = only_card(&ops).await;
    assert_eq!(
        moved.column, COLUMN_IN_REVIEW,
        "since #337 every finished card stops for a person"
    );
    let artifacts = crate::ports::artifacts::ArtifactStore::list(
        &*ops,
        &CompanyId::new("acme"),
        Some("t-origin"),
    )
    .await
    .expect("list");
    assert!(
        artifacts.is_empty(),
        "a run that published nothing has no deliverable: {artifacts:?}"
    );
    // …and the reply is still recorded where it belongs: on the card.
    assert!(
        moved.note.expect("note").contains("Ship the thing"),
        "the reply must survive even though it is not an artifact"
    );
}

/// **The identity-vs-recency regression** — the second defect #244 names,
/// and the one with teeth.
///
/// The old extend target was `max_by_key(updated_at_millis)`: whichever
/// artifact on the card had been touched most recently. An **operator edit**
/// bumps that timestamp. So an operator who tidied the invoice made the
/// invoice the target for the agent's next write to the spec — the spec's v2
/// landed as the invoice's v3, and `human_edit_diff` then reported a human
/// rewriting a document they had never opened.
///
/// The setup here reproduces exactly that: publish two files, operator-edit
/// the **second** so it is unambiguously the most recent, then republish the
/// **first**. Under recency this appends to the invoice. Under identity it
/// extends the spec, and the invoice is untouched.
#[tokio::test]
async fn a_republish_extends_by_identity_not_by_whatever_was_edited_last() {
    use crate::harness::publish::PendingPublish;
    use crate::ports::artifacts::{ArtifactAuthor, ArtifactStore};

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain_with_artifacts(dir.path());
    let company = CompanyId::new("acme");
    let c = card("t-1", "maya");
    let publish = |source: &str, body: &str| PendingPublish {
        agent: "maya".to_string(),
        source: source.to_string(),
        title: source.to_string(),
        kind: crate::ports::artifacts::ArtifactKind::Markdown,
        note: None,
        payload: crate::harness::publish::PublishPayload::Text(body.to_string()),
    };

    // Run 1 publishes both files.
    let ids = brain
        .record_published_artifacts(
            &c,
            "maya",
            vec![
                publish("specs/launch.md", "# Spec v1"),
                publish("billing/invoice.md", "# Invoice v1"),
            ],
            Some("run-1"),
        )
        .await
        .expect("records");
    assert_eq!(ids.len(), 2, "two files, two records");

    let by_source = |list: &[crate::ports::artifacts::ArtifactRecord], source: &str| {
        list.iter()
            .find(|a| a.source.as_deref() == Some(source))
            .expect("record for source")
            .clone()
    };
    let listed = ArtifactStore::list(&*ops, &company, Some("t-1"))
        .await
        .unwrap();
    let mut invoice = by_source(&listed, "billing/invoice.md");

    // The operator edits the INVOICE, making it the most recently updated
    // artifact on the card. This is the trap.
    invoice.push_version(
        "# Invoice v1, corrected",
        ArtifactAuthor::Operator,
        "operator",
        now_millis() + 1_000,
        Some("operator edit before approval".to_string()),
    );
    ArtifactStore::upsert(&*ops, &company, &invoice)
        .await
        .unwrap();

    // Run 2 republishes the SPEC only.
    brain
        .record_published_artifacts(
            &c,
            "maya",
            vec![publish("specs/launch.md", "# Spec v2")],
            Some("run-2"),
        )
        .await
        .expect("records");

    let after = ArtifactStore::list(&*ops, &company, Some("t-1"))
        .await
        .unwrap();
    assert_eq!(after.len(), 2, "no duplicate record was opened");

    let spec = by_source(&after, "specs/launch.md");
    assert_eq!(
        spec.versions.len(),
        2,
        "the republished path must extend its OWN record"
    );
    assert_eq!(spec.latest().unwrap().body, "# Spec v2");
    assert_eq!(spec.latest().unwrap().run_id.as_deref(), Some("run-2"));
    assert_eq!(
        spec.versions[0].run_id.as_deref(),
        Some("run-1"),
        "an earlier attempt keeps the attempt that wrote it"
    );

    let invoice = by_source(&after, "billing/invoice.md");
    assert_eq!(
        invoice.versions.len(),
        2,
        "the agent's spec must not have landed on the invoice"
    );
    assert_eq!(invoice.latest().unwrap().body, "# Invoice v1, corrected");
    assert_eq!(
        invoice.latest().unwrap().author,
        ArtifactAuthor::Operator,
        "the invoice's newest version is still the human's"
    );
    // And the reason all of this matters: the human-edit diff still says
    // what a human actually did, on each document separately.
    let diff = invoice.human_edit_diff().expect("the operator edited it");
    assert_eq!((diff.from_version, diff.to_version), (1, 2));
    assert!(
        spec.human_edit_diff().is_none(),
        "nobody edited the spec, so it must report no human edit"
    );
}
