use super::tests_core::*;
use super::tests_core2::*;
use super::*;

/// A hand-off the MEMBER's own tool refused reaches the card and the
/// operator, attributed to the member that attempted it.
///
/// A refusal never becomes a `Delegation`, so the only other record is the
/// tool result — which the member is free to describe however it likes, and
/// "I consulted design" is exactly the claim that must not stand unchecked.
/// The delegator's own unread refusals must NOT be swept into the member's
/// account of its turn, which is what the before/after sampling buys.
#[tokio::test]
async fn a_refusal_inside_a_members_turn_is_recorded_against_that_member() {
    let fx = Fixture::nested();
    let turns = ScriptedTurns::new(
        &fx,
        vec![
            // The orchestrator hands off AND has a refusal of its own,
            // which belongs to its turn and must not be folded into the
            // member's account of theirs.
            Turn {
                reply: "handing it to engineering".to_string(),
                tool_pushes: vec![handoff("ship the API")],
                refuses: vec!["nowhere_desk".to_string()],
                ..Turn::default()
            },
            // The member reaches for a desk it may not have — refused.
            Turn::refused("built it; design did not pick it up", &["design_desk"]),
            Turn::reply("Shipped."),
        ],
    );

    fx.runner(&turns)
        .handle_operator_message("chief", "ship the API", Some("general"))
        .await
        .expect("operator message handled");

    let cards = fx.cards().await;
    assert_eq!(cards.len(), 1, "{cards:?}");
    let note = cards[0].note.clone().unwrap_or_default();
    assert!(
        note.contains("design_desk") && note.contains("refused"),
        "the member's refused hand-off must reach the card: {note}"
    );
    assert!(
        !note.contains("nowhere_desk"),
        "the delegator's own unread refusal must not be attributed to the member: {note}"
    );
}

/// On the DISPATCHED-card path the card stays owned by the level-1 member
/// the orchestrator handed it to — nested delegation is visible in the note
/// and the steps, not by the card changing hands again.
///
/// # This test changed with the async hand-off, and the change is the point
///
/// It used to additionally assert that `handed.reply` carried the level-1
/// member's answer, with the nested researcher's reply folded into it. That
/// was a true statement about a SYNCHRONOUS hand-off: the delegate ran
/// inside the delegator's attempt, so its answer came back up the stack.
///
/// It cannot be true of an asynchronous one. The delegator now hands over
/// and settles `Delegated`; the delegate is dispatched as its OWN attempt,
/// which is what buys one attempt row per agent (so spend is attributable)
/// and releases the per-company serial lock between hops. The answer still
/// reaches the operator — through the delegate's own settle and relay —
/// just not on the delegator's reply.
///
/// What the test was *named* for is unchanged and still asserted: the card
/// belongs to the member the orchestrator handed it to, and nested
/// delegation does not move it a second time.
#[tokio::test]
async fn a_dispatched_card_stays_with_the_level_one_member() {
    let fx = Fixture::nested();
    let mut card = TaskRecord {
        id: "card-1".to_string(),
        title: TaskTitle::authored("Ship the API"),
        note: None,
        column: COLUMN_TODO.to_string(),
        priority: "medium".to_string(),
        assignee: "chief".to_string(),
        updated_at_millis: now_millis(),
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
    };
    fx.tasks.upsert(&fx.record.id, &card).await.expect("seed");

    let turns = ScriptedTurns::new(
        &fx,
        vec![
            // The engineering lead's turn, run by the dispatched card.
            Turn::tooling(
                "asking research",
                vec![nested_handoff("what rate limits do competitors use?")],
            ),
            Turn::reply("everyone lands around 100 rps"),
        ],
    );
    // The dispatched turn's own delegations are staged by the orchestrator
    // before this, so the queue is claimed the way `run_task` claims it.
    let _claim = fx.queue.claim();
    fx.queue.push(handoff("ship the API"));

    let handed = fx
        .runner(&turns)
        .for_task("card-1")
        .handle_task_delegations(&mut card, "chief")
        .await
        .expect("delegations drained")
        .expect("a hand-off happened");

    assert_eq!(
        handed.delegate, "engineer",
        "the card belongs to the member the ORCHESTRATOR handed it to"
    );
    assert!(
        handed.pending,
        "the hand-off is pending: the delegate has NOT run inside this attempt"
    );
    assert!(
        handed.reply.is_none(),
        "a pending hand-off carries no reply — the delegate answers from its own \
         attempt, which is what makes the spend attributable to them: {:?}",
        handed.reply
    );
    // **An owner is an agent; a desk is a channel.** Handing to a desk hands
    // the work to that desk's lead, and it is the lead the card names.
    //
    // This assertion previously demanded `eng_desk`, on the reading that a
    // desk hand-off leaves the DESK owning the card. That put a channel id
    // in an ownership field — and `assignee` is what the thread's overseer
    // is read from, so a card handed to a desk named nobody who could
    // answer for it. `AssigneeResolution::canonical` maps a desk to its own
    // id because it is the stored-key helper for whatever a card happens to
    // say, not a claim that a desk owns work.
    assert_eq!(
        card.assignee, "engineer",
        "the desk's lead owns the card; nested delegation must not move it \
         a second time"
    );
}

// ── Issue #453 residual: an id that names no card ───────────────────────

#[tokio::test]
async fn assigning_a_card_that_is_not_on_the_board_does_not_report_success() {
    let fx = Fixture::new();
    let turns = ScriptedTurns::new(
        &fx,
        vec![Turn::queueing(
            "assigning it",
            vec![Delegation::AssignTask {
                task_id: "card-that-never-existed".to_string(),
                assignee: "engineer".to_string(),
                note: Some("please pick this up".to_string()),
            }],
        )],
    );

    let outcome = fx
        .runner(&turns)
        .handle_operator_message(
            "chief",
            "put the launch plan on engineering",
            Some("general"),
        )
        .await
        .expect("an unknown card is a reported fact, not a turn failure");

    assert!(
        fx.cards().await.iter().all(|card| card.assignee.is_empty()),
        "nothing may be assigned on the strength of an id that names no card"
    );
    assert!(
        outcome.reply.contains("card-that-never-existed"),
        "the operator's reply must name the card the assignment could not reach rather than \
         warn into the log and let the receipt stand silently: {:?}",
        outcome.reply
    );
}

/// The same residual on the arm the code's own comment calls the more
/// consequential one: `review_task`'s receipt says the card "moves to done
/// as this turn completes". An unknown id moves nothing and records the
/// verdict nowhere, surfaced the same way `assign_task`'s is.
#[tokio::test]
async fn approving_a_card_that_is_not_on_the_board_does_not_report_success() {
    let fx = Fixture::new();
    let turns = ScriptedTurns::new(
        &fx,
        vec![Turn::queueing(
            "approved",
            vec![Delegation::ReviewTask {
                task_id: "card-that-never-existed".to_string(),
                decision: lifecycle::ReviewDecision::Approve,
                note: Some("looks good".to_string()),
            }],
        )],
    );

    let outcome = fx
        .runner(&turns)
        .handle_operator_message("chief", "approve the launch plan card", Some("general"))
        .await
        .expect("an unknown card is a reported fact, not a turn failure");

    assert!(
        fx.cards().await.is_empty(),
        "a verdict on an id that names no card may not mint one"
    );
    assert!(
        outcome.reply.contains("card-that-never-existed"),
        "the operator's reply must name the card whose approval landed nowhere rather than \
         warn into the log while the turn is told it moved: {:?}",
        outcome.reply
    );
}

/// The defect one layer over the false-success receipt: `run_delegation`
/// is driven from `drain_and_execute`'s loop with `?`, so an `Err` on one
/// delegation would abort the whole drain and discard every delegation
/// queued behind it in the SAME turn — including ones the model queued
/// validly. A hallucinated `task_id` is a routine model mistake, not an
/// exotic one, so a batch of two — an `assign_task` naming no card,
/// followed by one naming a real card — must still land the second write
/// AND still report the first's failure. The two tests above alone cannot
/// catch a regression to `Err`: they each queue exactly one delegation, so
/// an abort and a reported fact look identical from their vantage point.
#[tokio::test]
async fn a_valid_delegation_after_an_unknown_card_still_lands() {
    let fx = Fixture::new();
    let card = TaskRecord {
        id: "card-real".to_string(),
        title: TaskTitle::authored("Draft the launch plan"),
        note: None,
        column: COLUMN_TODO.to_string(),
        priority: "medium".to_string(),
        assignee: String::new(),
        updated_at_millis: now_millis(),
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
    };
    fx.tasks
        .upsert(&fx.record.id, &card)
        .await
        .expect("seed the real card");

    let turns = ScriptedTurns::new(
        &fx,
        vec![Turn::queueing(
            "assigning both",
            vec![
                Delegation::AssignTask {
                    task_id: "card-that-never-existed".to_string(),
                    assignee: "engineer".to_string(),
                    note: None,
                },
                Delegation::AssignTask {
                    task_id: "card-real".to_string(),
                    assignee: "engineer".to_string(),
                    note: None,
                },
            ],
        )],
    );

    let outcome = fx
        .runner(&turns)
        .handle_operator_message(
            "chief",
            "put the launch plan and the real card on engineering",
            Some("general"),
        )
        .await
        .expect("one unknown card must not fail the turn");

    let cards = fx.cards().await;
    let real = cards
        .iter()
        .find(|c| c.id == "card-real")
        .expect("the real card is still on the board");
    assert_eq!(
        real.assignee, "engineer",
        "the valid delegation queued AFTER the unknown-card one must still land — a false \
         success traded for silently discarding queued work is the same defect family, one \
         layer over"
    );
    assert!(
        outcome.reply.contains("card-that-never-existed"),
        "the unknown card's failure must still be reported even though the drain kept \
         going: {:?}",
        outcome.reply
    );
}

/// The bound on both refusals above: an id that DOES name a card must not
/// be caught by them. Without this the two tests are satisfied by a drain
/// that refuses every lifecycle write.
#[tokio::test]
async fn a_known_card_id_still_assigns_and_reports_no_failure() {
    let fx = Fixture::new();
    let card = TaskRecord {
        id: "card-real".to_string(),
        title: TaskTitle::authored("Draft the launch plan"),
        note: None,
        column: COLUMN_TODO.to_string(),
        priority: "medium".to_string(),
        assignee: String::new(),
        updated_at_millis: now_millis(),
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
    };
    fx.tasks
        .upsert(&fx.record.id, &card)
        .await
        .expect("seed the card");

    let turns = ScriptedTurns::new(
        &fx,
        vec![Turn::queueing(
            "assigning it",
            vec![Delegation::AssignTask {
                task_id: "card-real".to_string(),
                assignee: "engineer".to_string(),
                note: None,
            }],
        )],
    );

    fx.runner(&turns)
        .handle_operator_message(
            "chief",
            "put the launch plan on engineering",
            Some("general"),
        )
        .await
        .expect("a real card assigns without complaint");

    let cards = fx.cards().await;
    assert_eq!(cards.len(), 1, "{cards:?}");
    assert_eq!(cards[0].assignee, "engineer");
}

/// `assign_task`'s write is deliberately narrow — "the column is untouched
/// on purpose" per the arm's own comment — but nothing drove that through
/// a card that was NOT freshly opened in `todo`. A card already finished
/// is the state where a column write sneaking in in the future would be
/// most visible and most wrong: reassigning a `done` card must not reopen
/// it.
#[tokio::test]
async fn assigning_a_done_card_moves_only_the_assignee_not_the_column() {
    let fx = Fixture::new();
    fx.tasks
        .upsert(&fx.record.id, &card_in("card-done", COLUMN_DONE))
        .await
        .expect("seed a finished card");

    let turns = ScriptedTurns::new(
        &fx,
        vec![Turn::queueing(
            "assigning it",
            vec![Delegation::AssignTask {
                task_id: "card-done".to_string(),
                assignee: "engineer".to_string(),
                note: None,
            }],
        )],
    );
    fx.runner(&turns)
        .handle_operator_message(
            "chief",
            "hand the finished plan to engineering",
            Some("general"),
        )
        .await
        .expect("assigning a finished card is not refused");

    let cards = fx.cards().await;
    assert_eq!(cards.len(), 1);
    assert_eq!(
        cards[0].assignee, "engineer",
        "the assignee write still lands"
    );
    assert_eq!(
        cards[0].column, COLUMN_DONE,
        "assigning a card must never move it — a finished card stays finished"
    );
}

#[tokio::test]
async fn approving_a_card_never_dispatched_is_refused_without_changing_it() {
    let fx = Fixture::new();
    fx.tasks
        .upsert(&fx.record.id, &card_in("card-untouched", COLUMN_TODO))
        .await
        .expect("seed a card that was never dispatched");

    let turns = ScriptedTurns::new(
        &fx,
        vec![Turn::queueing(
            "approved",
            vec![Delegation::ReviewTask {
                task_id: "card-untouched".to_string(),
                decision: lifecycle::ReviewDecision::Approve,
                note: None,
            }],
        )],
    );
    let outcome = fx
        .runner(&turns)
        .handle_operator_message("chief", "approve the launch plan card", Some("general"))
        .await
        .expect("a refused review must not abort the delegation drain");

    let cards = fx.cards().await;
    assert_eq!(cards.len(), 1);
    assert_eq!(
        cards[0].column, COLUMN_TODO,
        "review_task must refuse a card that was never under review"
    );
    assert!(
        cards[0].note.is_none(),
        "a refused review must not record a verdict"
    );
    assert!(
        outcome.reply.contains("card-untouched") && outcome.reply.contains("not in_review"),
        "the operator must see why the review was refused: {}",
        outcome.reply
    );
}

#[tokio::test]
async fn review_refuses_every_non_review_column_and_preserves_later_valid_work() {
    for column in [
        COLUMN_TODO,
        COLUMN_IN_PROGRESS,
        COLUMN_DONE,
        COLUMN_PAUSED,
        COLUMN_PLANNING,
        "custom",
    ] {
        for decision in [
            lifecycle::ReviewDecision::Approve,
            lifecycle::ReviewDecision::Revise,
        ] {
            let fx = Fixture::new();
            let mut original = card_in("card-refused", column);
            original.note = Some("original note".to_string());
            original.updated_at_millis = 123;
            fx.tasks.upsert(&fx.record.id, &original).await.unwrap();
            fx.tasks
                .upsert(&fx.record.id, &card_in("card-reviewable", COLUMN_IN_REVIEW))
                .await
                .unwrap();
            let turns = ScriptedTurns::new(
                &fx,
                vec![Turn::queueing(
                    "reviewed",
                    vec![
                        Delegation::ReviewTask {
                            task_id: original.id.clone(),
                            decision,
                            note: Some("must not land".to_string()),
                        },
                        Delegation::ReviewTask {
                            task_id: "card-reviewable".to_string(),
                            decision,
                            note: Some("valid review".to_string()),
                        },
                    ],
                )],
            );
            let outcome = fx
                .runner(&turns)
                .handle_operator_message("chief", "review the launch plan cards", Some("general"))
                .await
                .expect("a refused review does not discard a valid sibling");
            let cards = fx.cards().await;
            let refused = cards.iter().find(|card| card.id == original.id).unwrap();
            assert_eq!(
                refused.column, original.column,
                "refused review must preserve its column"
            );
            assert_eq!(
                refused.note, original.note,
                "refused review must preserve its note"
            );
            assert_eq!(
                refused.updated_at_millis, original.updated_at_millis,
                "refused review must preserve its revision"
            );
            let reviewed = cards
                .iter()
                .find(|card| card.id == "card-reviewable")
                .unwrap();
            assert_eq!(reviewed.column, lifecycle::review_landing_column(decision));
            assert!(reviewed.note.as_deref().unwrap().contains("valid review"));
            assert!(
                outcome.reply.contains("card-refused") && outcome.reply.contains("not in_review"),
                "refusal must reach the operator: {}",
                outcome.reply
            );
        }
    }
}

#[tokio::test]
async fn a_task_store_write_failure_on_assign_task_surfaces_as_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backing: Arc<dyn TaskStore> = Arc::new(FsOps::new(dir.path()));
    let record = record();
    backing
        .upsert(&record.id, &card_in("card-real", COLUMN_TODO))
        .await
        .expect("seed the real card");
    let tasks: Arc<dyn TaskStore> = Arc::new(FailingUpsertStore {
        inner: backing.clone(),
    });
    let queue = DelegationQueue::default();
    let steer = InflightRegistry::default();
    let idle_turns_fx = Fixture::new();
    let idle_turns = ScriptedTurns::new(&idle_turns_fx, vec![]);

    let runner = DelegationRunner::new(
        &idle_turns,
        &record,
        Some(&tasks),
        &steer,
        &record.id,
        &queue,
        orchestrator::MAX_DELEGATIONS_PER_TURN,
    );
    let outcome = runner
        .run_delegation(
            Delegation::AssignTask {
                task_id: "card-real".to_string(),
                assignee: "engineer".to_string(),
                note: None,
            },
            None,
            MessageContext::default(),
        )
        .await;
    assert!(
        outcome.is_err(),
        "a real write failure must surface as an error, not a reported fact: {:?}",
        outcome.err().map(|e| e.to_string())
    );

    let cards = backing.list(&record.id).await.unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(
        cards[0].assignee, "",
        "the card must be untouched by the failed write"
    );
}

/// The same store-fault distinction for `review_task`.
#[tokio::test]
async fn a_task_store_write_failure_on_review_task_surfaces_as_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backing: Arc<dyn TaskStore> = Arc::new(FsOps::new(dir.path()));
    let record = record();
    backing
        .upsert(&record.id, &card_in("card-real", COLUMN_IN_REVIEW))
        .await
        .expect("seed the real card");
    let tasks: Arc<dyn TaskStore> = Arc::new(FailingUpsertStore {
        inner: backing.clone(),
    });
    let queue = DelegationQueue::default();
    let steer = InflightRegistry::default();
    let idle_turns_fx = Fixture::new();
    let idle_turns = ScriptedTurns::new(&idle_turns_fx, vec![]);

    let runner = DelegationRunner::new(
        &idle_turns,
        &record,
        Some(&tasks),
        &steer,
        &record.id,
        &queue,
        orchestrator::MAX_DELEGATIONS_PER_TURN,
    );
    let outcome = runner
        .run_delegation(
            Delegation::ReviewTask {
                task_id: "card-real".to_string(),
                decision: lifecycle::ReviewDecision::Approve,
                note: None,
            },
            None,
            MessageContext::default(),
        )
        .await;
    assert!(
        outcome.is_err(),
        "a real write failure must surface as an error, not a reported fact: {:?}",
        outcome.err().map(|e| e.to_string())
    );

    let cards = backing.list(&record.id).await.unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(
        cards[0].column, COLUMN_IN_REVIEW,
        "the card must be untouched by the failed write"
    );
}

/// The three shapes a dispatched turn's target can take, which is what decides
/// where its live frames go.
///
/// `run_steered_dispatch` selects its stream from `chat_id` alone — present
/// streams to that desk, absent streams nothing — so these three cases *are*
/// the destination table: a threaded origin keeps its thread, a channel-level
/// origin carries none, and a board-created card names no conversation and so
/// publishes nothing. A later change to this constructor could silently route
/// frames to the wrong conversation, or expose board-only work
/// (tinysweeper, #2369).
///
/// And in every case the history seed stays off: the turn brings its own
/// instruction and the card's history, so binding the conversation must not
/// change what the agent reads.
#[test]
fn a_dispatched_target_names_its_conversation_without_seeding_it() {
    // Raised inside a thread: both halves, and the thread is preserved.
    let threaded = ChatTarget::dispatched_from(Some("order_ops"), Some(EventSeq::new(34)));
    assert_eq!(threaded.chat_id, Some("order_ops"));
    assert_eq!(threaded.thread_root, Some(EventSeq::new(34)));
    assert!(
        !threaded.history_seed,
        "addressing is not seeding: the turn brings its own context"
    );

    // Raised at channel level: the desk, with no thread root. `None` here is
    // the channel itself, not a gap — the same reading `TaskOrigin` documents.
    let channel = ChatTarget::dispatched_from(Some("order_ops"), None);
    assert_eq!(channel.chat_id, Some("order_ops"));
    assert_eq!(channel.thread_root, None);
    assert!(!channel.history_seed);

    // Raised on the board: no conversation at all, which is what keeps its
    // frames off whichever thread the console happens to be watching.
    let from_the_board = ChatTarget::dispatched_from(None, None);
    assert_eq!(from_the_board.chat_id, None);
    assert_eq!(from_the_board.thread_root, None);
    assert!(!from_the_board.history_seed);
}
