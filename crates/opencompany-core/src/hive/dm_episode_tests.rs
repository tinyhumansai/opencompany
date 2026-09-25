//! **An operator DM, driven end to end.**
//!
//! Everything else about DMs is asserted a decision at a time: the hive is
//! built right, the chat resolves to a room, the owner answers rather than a
//! ranker. None of that proves the parts meet. This drives the real path --
//! real `dm_hives`, real `surface_of`, real dispatcher, real `conducted::run`,
//! real `HostedRunner`, real seats on the pool's own agents -- and stubs one
//! thing, at the one boundary that would need a credential: the model's
//! choices, via a scripted OpenAI-compatible endpoint on loopback.
//!
//! That is the shape `gated_tool_turn_tests` established, reused here because
//! an episode is exactly the kind of wiring a unit test stays green through.

use std::sync::Arc;

use crate::harness::HarnessPool;
use crate::hive::test_support::{MemoryLog, TWO_DESKS, record};
use crate::ports::events::EventLog;
use crate::workflows::gated_tool_turn_tests::{Turn, deps, spawn_script_recording};

/// An operator's message in a teammate's DM runs an episode, and the teammate
/// whose DM it is answers it.
///
/// The two halves are separately fragile. A DM that never becomes a room
/// silently falls back to the pooled turn -- the old behaviour, no error. A DM
/// that becomes a room but routes by rank is answered by whoever the ranker
/// preferred, which is also not an error. Both look like working software.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_operator_dm_runs_an_episode_answered_by_its_own_teammate() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    // A reply that calls no tool records nothing, so a seat that only *says*
    // something leaves the episode open until it hits the turn wall. The
    // script completes, which is what a seat answering its operator does.
    let (base_url, script) = spawn_script_recording(vec![Turn::Call {
        tool: "desk_complete_episode",
        args: serde_json::json!({
            "message": "passkeys land next sprint",
            "chat": "dm:ceo",
            "parent": null
        }),
    }])
    .await;
    let (deps, _journal) = deps(base_url, dir.path());
    let record = record(TWO_DESKS);

    let pool = HarnessPool::new();
    pool.ensure(&record, &deps).await.expect("the roster boots");

    // The hives a DM chat resolves through, built the way `hives_for` builds
    // them when the flag is on.
    let (hives, errors) = crate::hive::graph::dm_hives(&record, 3, &|id| {
        futures::executor::block_on(pool.agent(&record.id, id))
            .map(|agent| agent.runtime_agent().clone())
    });
    assert!(errors.is_empty(), "{errors:?}");

    let surface = crate::hive::dispatch::surface_of(&record, &hives, Some("dm:ceo"));
    let crate::hive::dispatch::Surface::Room { desk_id } = surface else {
        panic!("an operator DM with a hive is a room, not a pooled turn");
    };
    assert_eq!(desk_id, "dm:ceo");

    let log = Arc::new(MemoryLog::default());
    let events: Arc<dyn EventLog> = log.clone();
    let dispatcher = crate::hive::dispatch::dispatcher(
        Arc::new(record.clone()),
        Arc::clone(&events),
        hives,
        Arc::new(deps),
        Arc::new(pool),
        None,
    );

    // Journalled first, as `run_cycle` does. A trigger naming a row that was
    // never said threads the episode under a root nothing exists at, and the
    // seat is left holding work it cannot close.
    let seq = events
        .append(
            &record.id,
            crate::hive::test_support::operator_message(
                &desk_id,
                "ship passkeys next sprint?",
                None,
            ),
        )
        .await
        .expect("the operator's message is a real row");
    let report: crate::Result<_> = dispatcher
        .run_desk_message(
            &desk_id,
            crate::hive::conducted::Trigger {
                seq,
                text: "ship passkeys next sprint?".to_owned(),
                parent: None,
                mentions: Vec::new(),
            },
        )
        .await;
    let report = report.expect("the episode runs");

    assert!(report.turns > 0, "a seat took a turn: {report:?}");
    assert!(
        !script.seen.lock().unwrap().is_empty(),
        "the seat actually reached the model"
    );

    let replies = log.replies("dm:ceo");
    assert!(
        replies.iter().any(|(who, _)| who == "ceo"),
        "the teammate whose DM it is answered: {replies:?}"
    );
    assert!(
        !replies.iter().any(|(who, _)| who != "ceo"),
        "and nobody else did -- the roster is bound to be asked, not to reply: {replies:?}"
    );
}

/// A teammate that takes something on says so in its own line, and that line
/// is one the operator can write back to.
///
/// The announcement has to be a real row. A note returned to the asker for
/// relaying would leave the operator writing back into the first teammate's
/// line -- the line that no longer holds the work.
///
/// That the line then runs an episode is
/// [`an_operator_dm_runs_an_episode_answered_by_its_own_teammate`]'s claim, not
/// this one's: the point here is that the operator is told where, by the
/// teammate that has it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_teammate_that_takes_over_says_so_in_a_line_the_operator_can_reach() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let (base_url, _script) = spawn_script_recording(Vec::new()).await;
    let (deps, _journal) = deps(base_url, dir.path());
    let record = record(TWO_DESKS);

    let pool = HarnessPool::new();
    pool.ensure(&record, &deps).await.expect("the roster boots");

    let log = Arc::new(MemoryLog::default());
    let events: Arc<dyn EventLog> = log.clone();

    let (chat, _seq) = crate::hive::dispatch::announce_takeover(
        events.as_ref(),
        &record.id,
        "engineer",
        "I have the webauthn estimate, I will follow up here.",
    )
    .await
    .expect("the announcement lands");
    assert_eq!(chat, "dm:engineer", "in its own line, not the asker's");

    let told = log.replies(&chat);
    assert!(
        told.iter()
            .any(|(who, text)| who == "engineer" && text.contains("webauthn")),
        "the operator can see who has it: {told:?}"
    );
    assert!(
        log.replies("dm:ceo").is_empty(),
        "and the teammate first written to has promised nothing on its behalf"
    );

    // And that line is a room, so the operator's reply there reaches the
    // teammate that claimed the work rather than the one they first wrote to.
    let (hives, errors) = crate::hive::graph::dm_hives(&record, 3, &|id| {
        futures::executor::block_on(pool.agent(&record.id, id))
            .map(|agent| agent.runtime_agent().clone())
    });
    assert!(errors.is_empty(), "{errors:?}");
    assert!(
        matches!(
            crate::hive::dispatch::surface_of(&record, &hives, Some(&chat)),
            crate::hive::dispatch::Surface::Room { ref desk_id } if desk_id == &chat
        ),
        "the line it claimed is one the operator can write back to"
    );
}

/// Reproduces the stall: a teammate announces, then the operator replies.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn announce_then_reply_does_not_stall() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let completing = || Turn::Call {
        tool: "desk_complete_episode",
        args: serde_json::json!({
            "message": "two sprints",
            "chat": "dm:engineer",
            "parent": null
        }),
    };
    let (base_url, _script) =
        spawn_script_recording(vec![completing(), completing(), completing()]).await;
    let (deps, _journal) = deps(base_url, dir.path());
    let record = record(TWO_DESKS);
    let pool = HarnessPool::new();
    pool.ensure(&record, &deps).await.expect("roster");
    let log = Arc::new(MemoryLog::default());
    let events: Arc<dyn EventLog> = log.clone();

    let (chat, _announced_at) = crate::hive::dispatch::announce_takeover(
        events.as_ref(),
        &record.id,
        "engineer",
        "I have the webauthn estimate.",
    )
    .await
    .expect("announced");

    let (hives, _) = crate::hive::graph::dm_hives(&record, 3, &|id| {
        futures::executor::block_on(pool.agent(&record.id, id))
            .map(|agent| agent.runtime_agent().clone())
    });
    let dispatcher = crate::hive::dispatch::dispatcher(
        Arc::new(record.clone()),
        Arc::clone(&events),
        hives,
        Arc::new(deps),
        Arc::new(pool),
        None,
    );
    let reply_seq = events
        .append(
            &record.id,
            crate::hive::test_support::operator_message(&chat, "two sprints is fine, go", None),
        )
        .await
        .expect("the operator's reply is a real row");
    let outcome = dispatcher
        .run_desk_message(
            &chat,
            crate::hive::conducted::Trigger {
                seq: reply_seq,
                text: "two sprints is fine, go".to_owned(),
                parent: None,
                mentions: Vec::new(),
            },
        )
        .await;
    outcome.expect("the episode should settle, not stall");
}

/// The same shape on a **desk**: does prior history stall an episode there too?
///
/// If it does, the stall is the hive path's and predates DMs. If it does not,
/// something about how a DM is built is the cause.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_desk_episode_with_prior_history_settles() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let completing = || Turn::Call {
        tool: "desk_complete_episode",
        args: serde_json::json!({
            "message": "noted",
            "chat": "engineering",
            "parent": null
        }),
    };
    let (base_url, _script) =
        spawn_script_recording(vec![completing(), completing(), completing(), completing()]).await;
    let (deps, _journal) = deps(base_url, dir.path());
    let record = record(TWO_DESKS);
    let pool = HarnessPool::new();
    pool.ensure(&record, &deps).await.expect("roster");
    let log = Arc::new(MemoryLog::default());
    let events: Arc<dyn EventLog> = log.clone();

    // A prior row, exactly as in the DM case.
    let _first = events
        .append(
            &record.id,
            crate::hive::test_support::operator_message("content", "unrelated", None),
        )
        .await
        .expect("prior row");

    let (hives, errors) = crate::hive::graph::desk_hives(&record, 3, &|id| {
        futures::executor::block_on(pool.agent(&record.id, id))
            .map(|agent| agent.runtime_agent().clone())
    });
    assert!(errors.is_empty(), "{errors:?}");
    assert!(hives.contains_key("engineering"), "the desk has a hive");

    let dispatcher = crate::hive::dispatch::dispatcher(
        Arc::new(record.clone()),
        Arc::clone(&events),
        hives,
        Arc::new(deps),
        Arc::new(pool),
        None,
    );
    // Journal the triggering message, as `run_cycle` does in production, and
    // dispatch on *its* sequence. A fabricated trigger names a row that does
    // not exist, and the episode threads under a root nothing was said at.
    let trigger_seq = events
        .append(
            &record.id,
            crate::hive::test_support::operator_message(
                "engineering",
                "two sprints is fine, go",
                None,
            ),
        )
        .await
        .expect("the trigger is a real row");
    dispatcher
        .run_desk_message(
            "engineering",
            crate::hive::conducted::Trigger {
                seq: trigger_seq,
                text: "two sprints is fine, go".to_owned(),
                parent: None,
                mentions: Vec::new(),
            },
        )
        .await
        .expect("a desk episode with prior history should settle");
}
