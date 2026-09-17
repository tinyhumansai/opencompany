use super::tests_core::*;
use super::tests_core2::*;

/// The same adoption when the handler's card landed in **Planning**
/// (issue #576).
///
/// A person's prompt-box card is created directly in Planning now, and the
/// matcher used to require To-do — so the commonest card of the two stopped
/// being recognised. Nothing failed loudly: the card was still created and
/// still planned, `spawned_task` simply fell back to `None`, the operator
/// bubble reported no card, and the chip tying the reply to the board
/// vanished. Caught end to end by `chat-to-card.spec.ts` under the live
/// brain; pinned here because that lane runs only in CI and this is where
/// the rule lives.
#[tokio::test]
async fn a_handler_card_in_planning_is_adopted_like_one_in_todo() {
    let imperative = "draft the launch plan for next quarter";
    let title = crate::company::task_intent::detect_task_intent(imperative)
        .expect("fixture must be a message the chat handler cards");
    let fx = Fixture::new();
    // Exactly what the REST handler writes for a signed-in person.
    fx.tasks
        .upsert(
            &fx.record.id,
            &TaskRecord {
                id: "handler-card".to_string(),
                title: TaskTitle::authored(&title),
                note: None,
                column: COLUMN_PLANNING.to_string(),
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
                origin_message_seq: Some(handler_seq()),
                bounced: None,
            },
        )
        .await
        .expect("seed the handler's card");

    let turns = ScriptedTurns::new(&fx, vec![Turn::reply("on it")]);
    let turn = fx
        .runner(&turns)
        .answering(Some(handler_seq()))
        .handle_operator_message("chief", imperative, None)
        .await
        .expect("operator message handled");

    let cards = fx.cards().await;
    assert_eq!(cards.len(), 1, "one message, one card: {cards:?}");
    assert_eq!(
        turn.spawned_task.as_deref(),
        Some("handler-card"),
        "a Planning card is the handler's card too — the reply must link to it"
    );
}

/// Issue #982, and the half of it that fails silently: since the REST
/// handler assigns the card it opens to the thread the message was
/// addressed to, the card this seam has to adopt is no longer blank.
///
/// A test that only asserted the assignee would not see this. What breaks
/// when the two halves ship apart is `spawned_task` — the reply's "Card
/// opened" chip, and the handle `settle_authored_workflow_card` needs to
/// settle a workflow the turn authored — and nothing anywhere errors.
#[tokio::test]
async fn a_handler_card_assigned_to_the_addressed_teammate_is_still_adopted() {
    let imperative = "draft the launch plan for next quarter";
    let title = crate::company::task_intent::detect_task_intent(imperative)
        .expect("fixture must be a message the chat handler cards");
    let fx = Fixture::new();
    // Exactly what the REST handler writes for a person who DM'd a teammate.
    fx.tasks
        .upsert(
            &fx.record.id,
            &TaskRecord {
                id: "handler-card".to_string(),
                title: TaskTitle::authored(&title),
                note: None,
                column: COLUMN_PLANNING.to_string(),
                priority: "medium".to_string(),
                assignee: "engineer".to_string(),
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
                origin_message_seq: Some(handler_seq()),
                bounced: None,
            },
        )
        .await
        .expect("seed the handler's card");

    let turns = ScriptedTurns::new(&fx, vec![Turn::reply("on it")]);
    let turn = fx
        .runner(&turns)
        .answering(Some(handler_seq()))
        .handle_operator_message("engineer", imperative, Some("engineer"))
        .await
        .expect("operator message handled");

    let cards = fx.cards().await;
    assert_eq!(cards.len(), 1, "one message, one card: {cards:?}");
    assert_eq!(
        turn.spawned_task.as_deref(),
        Some("handler-card"),
        "an assigned handler card is still the handler's card — the reply must link to it"
    );
}

/// …including when the console addressed the teammate by their DM channel
/// id, which is the form the chat route resolves the card's assignee from.
#[tokio::test]
async fn a_dm_channel_id_adopts_the_card_it_addressed() {
    let imperative = "draft the launch plan for next quarter";
    let title = crate::company::task_intent::detect_task_intent(imperative)
        .expect("fixture must be a message the chat handler cards");
    let fx = Fixture::new();
    fx.tasks
        .upsert(
            &fx.record.id,
            &TaskRecord {
                id: "handler-card".to_string(),
                title: TaskTitle::authored(&title),
                note: None,
                column: COLUMN_PLANNING.to_string(),
                priority: "medium".to_string(),
                assignee: "engineer".to_string(),
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
                origin_message_seq: Some(handler_seq()),
                bounced: None,
            },
        )
        .await
        .expect("seed the handler's card");

    let turns = ScriptedTurns::new(&fx, vec![Turn::reply("on it")]);
    let turn = fx
        .runner(&turns)
        .answering(Some(handler_seq()))
        .handle_operator_message("engineer", imperative, Some("dm:engineer"))
        .await
        .expect("operator message handled");

    assert_eq!(turn.spawned_task.as_deref(), Some("handler-card"));
}

/// Issue #982 again, and the same shape one field over: the handler now
/// stamps the thread it opened the card from, so the origin clause has to
/// accept **this** thread as well as none. Without it the chip disappears
/// on exactly the messages the stamp was added for.
#[tokio::test]
async fn a_handler_card_stamped_with_this_turns_thread_is_adopted() {
    let imperative = "draft the launch plan for next quarter";
    let title = crate::company::task_intent::detect_task_intent(imperative)
        .expect("fixture must be a message the chat handler cards");
    let fx = Fixture::new();
    fx.tasks
        .upsert(
            &fx.record.id,
            &TaskRecord {
                id: "handler-card".to_string(),
                title: TaskTitle::authored(&title),
                note: None,
                column: COLUMN_PLANNING.to_string(),
                priority: "medium".to_string(),
                assignee: "engineer".to_string(),
                updated_at_millis: now_millis(),
                origin: TaskOrigin::new(Some("dm:engineer".to_string()), None),
                parent_task_id: None,
                output: None,
                plan: None,
                planning_attempts: Vec::new(),
                deliverable: crate::ports::tasks::TaskDeliverable::Once,
                workflow_proposal: None,
                origin_run_id: None,
                origin_workflow_id: None,
                origin_message_seq: Some(handler_seq()),
                bounced: None,
            },
        )
        .await
        .expect("seed the handler's card");

    let turns = ScriptedTurns::new(&fx, vec![Turn::reply("on it")]);
    let turn = fx
        .runner(&turns)
        .answering(Some(handler_seq()))
        .handle_operator_message("engineer", imperative, Some("dm:engineer"))
        .await
        .expect("operator message handled");

    assert_eq!(turn.spawned_task.as_deref(), Some("handler-card"));
}

/// …nor is one from a different **thread of the same desk**, which the
/// desk-only clause did not hold.
///
/// The desk matched, so a same-titled card raised in another thread was
/// adopted and settled by this turn: one thread received an answer it never
/// asked for while the other's card moved under it. That is #1890 B's split
/// reached from the other side — the conversation a card belongs to read as
/// a desk when it is a desk *and a thread* (coderabbit on #1982).
#[tokio::test]
async fn a_handler_card_from_another_thread_of_the_same_desk_is_not_adopted() {
    let imperative = "draft the launch plan for next quarter";
    let title = crate::company::task_intent::detect_task_intent(imperative)
        .expect("fixture must be a message the chat handler cards");
    let fx = Fixture::new();
    fx.tasks
        .upsert(
            &fx.record.id,
            &TaskRecord {
                id: "another-threads-card".to_string(),
                title: TaskTitle::authored(&title),
                note: None,
                column: COLUMN_PLANNING.to_string(),
                priority: "medium".to_string(),
                assignee: String::new(),
                updated_at_millis: now_millis(),
                // The same desk this message is addressed to — and a thread
                // inside it that this message is not in.
                origin: TaskOrigin::new(
                    Some("dm:engineer".to_string()),
                    Some(crate::ports::types::EventSeq::new(41)),
                ),
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
            },
        )
        .await
        .expect("seed the card");

    let turns = ScriptedTurns::new(&fx, vec![Turn::reply("on it")]);
    let turn = fx
        .runner(&turns)
        .handle_operator_message("chief", imperative, Some("dm:engineer"))
        .await
        .expect("operator message handled");

    assert_ne!(
        turn.spawned_task.as_deref(),
        Some("another-threads-card"),
        "a card from another thread of this desk is not this message's card",
    );
}

/// …and a card opened from a *different* conversation is still not ours,
/// which is the property that clause has always been holding.
#[tokio::test]
async fn a_handler_card_from_another_thread_is_not_adopted() {
    let imperative = "draft the launch plan for next quarter";
    let title = crate::company::task_intent::detect_task_intent(imperative)
        .expect("fixture must be a message the chat handler cards");
    let fx = Fixture::new();
    fx.tasks
        .upsert(
            &fx.record.id,
            &TaskRecord {
                id: "another-threads-card".to_string(),
                title: TaskTitle::authored(&title),
                note: None,
                column: COLUMN_PLANNING.to_string(),
                priority: "medium".to_string(),
                assignee: "".to_string(),
                updated_at_millis: now_millis(),
                origin: TaskOrigin::new(Some("eng_desk".to_string()), None),
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
            },
        )
        .await
        .expect("seed the card");

    let turns = ScriptedTurns::new(&fx, vec![Turn::reply("on it")]);
    let turn = fx
        .runner(&turns)
        .handle_operator_message("chief", imperative, Some("dm:engineer"))
        .await
        .expect("operator message handled");

    assert_eq!(
        turn.spawned_task, None,
        "another conversation's card is not this message's card"
    );
}

/// …and the relaxation is exactly as wide as it needs to be: a card
/// assigned to somebody the message was NOT addressed to is still refused,
/// which is the property the blank-only clause was really protecting.
#[tokio::test]
async fn a_handler_card_assigned_to_somebody_else_is_not_adopted() {
    let imperative = "draft the launch plan for next quarter";
    let title = crate::company::task_intent::detect_task_intent(imperative)
        .expect("fixture must be a message the chat handler cards");
    let fx = Fixture::new();
    fx.tasks
        .upsert(
            &fx.record.id,
            &TaskRecord {
                id: "someone-elses-card".to_string(),
                title: TaskTitle::authored(&title),
                note: None,
                column: COLUMN_PLANNING.to_string(),
                priority: "medium".to_string(),
                assignee: "engineer".to_string(),
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
            },
        )
        .await
        .expect("seed the card");

    let turns = ScriptedTurns::new(&fx, vec![Turn::reply("on it")]);
    let turn = fx
        .runner(&turns)
        .handle_operator_message("chief", imperative, None)
        .await
        .expect("operator message handled");

    assert_eq!(
        turn.spawned_task, None,
        "a card assigned to a teammate this message did not address is not ours to adopt"
    );
}

/// …and the clause still refuses a card resting anywhere else, because a
/// card in any other column was moved there by somebody and is no longer
/// the untouched write this seam is allowed to adopt.
#[tokio::test]
async fn a_handler_card_the_operator_moved_on_is_not_adopted() {
    let imperative = "draft the launch plan for next quarter";
    let title = crate::company::task_intent::detect_task_intent(imperative)
        .expect("fixture must be a message the chat handler cards");
    let fx = Fixture::new();
    fx.tasks
        .upsert(
            &fx.record.id,
            &TaskRecord {
                id: "moved-on".to_string(),
                title: TaskTitle::authored(&title),
                note: None,
                column: COLUMN_IN_PROGRESS.to_string(),
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
            },
        )
        .await
        .expect("seed the moved card");

    let turns = ScriptedTurns::new(&fx, vec![Turn::reply("on it")]);
    let turn = fx
        .runner(&turns)
        .handle_operator_message("chief", imperative, None)
        .await
        .expect("operator message handled");

    assert_eq!(
        turn.spawned_task, None,
        "a card somebody moved is not the handler's untouched write"
    );
}

/// One message, one card — the #463 guarantee, in the shape #2364 left it.
///
/// This test used to be `the_stand_down_holds_even_when_the_handlers_card_
/// cannot_be_found`, and asserted **no** card: the stand-down was keyed on the
/// task-intent *detector*, so a handler card that could not be found on the
/// board still suppressed this path rather than re-opening the same work.
///
/// #2364 replaced that rule. The chat handler no longer cards on the triage at
/// all — the board is a tool call now — so `carded_by_handler` reads a
/// persisted card instead of the detector, and the hand-off's own card is the
/// one card this message gets. The sibling that pins the same model from the
/// other side is `a_tracked_instruction_still_delegates_under_a_claim`, which
/// #2364 updated in the same way and which this file's version was missed
/// alongside — it is why the gated lane has been red on `main` since that
/// merge.
///
/// What is still worth pinning is the part that did NOT change: exactly one
/// card, never two. The mechanism moved from the detector to the tool call;
/// the guarantee did not.
#[tokio::test]
async fn a_handed_off_message_gets_exactly_one_card() {
    let fx = Fixture::new();
    let turns = ScriptedTurns::new(
        &fx,
        vec![
            Turn::queueing("on it", vec![handoff("Draft the launch plan.")]),
            Turn::reply("drafted"),
            Turn::reply("relayed"),
        ],
    );
    let turn = fx
        .runner(&turns)
        .handle_operator_message("chief", "draft the launch plan for next quarter", None)
        .await
        .expect("operator message handled");
    let cards = fx.cards().await;
    assert_eq!(
        cards.len(),
        1,
        "the hand-off's card is the only card this message gets: {cards:?}"
    );
    assert_eq!(
        cards[0].assignee, "engineer",
        "and it belongs to the delegate"
    );
    // …and the turn points at that same card rather than at nothing. The old
    // assertion here was `is_none()`, which was only true because the old rule
    // opened no card at all; with one card there is something honest to claim.
    assert_eq!(
        turn.spawned_task.as_deref(),
        Some(cards[0].id.as_str()),
        "the turn claims the card it actually opened"
    );
}

/// …and the same thread stays quiet for a question, so a desk chat does not
/// become a card mint.
#[tokio::test]
async fn a_desk_asked_a_question_directly_mints_no_card() {
    let fx = Fixture::new();
    let turns = ScriptedTurns::new(&fx, vec![Turn::reply("green")]);
    let turn = fx
        .runner(&turns)
        .handle_operator_message("engineer", "is the build green?", Some("eng_desk"))
        .await
        .expect("operator message handled");
    assert!(fx.cards().await.is_empty());
    assert!(turn.spawned_task.is_none());
}

// ── Issue #267: a question may not write to the board, by either door ────

/// The gate itself. On a question the queue is claimed for **answering
/// only**, so the model's own `spawn_task` is refused inside its turn —
/// with the `NoDrain` refusal, which is the one that tells it not to retry
/// — and no card exists afterwards.
///
/// This is the door Layer A cannot close. "Tell what is there in the tasks
/// list" has no action verb and no request frame, so the REST handler never
/// saw it as work; it became a card because the orchestrator called
/// `spawn_task` on a pure read.
#[tokio::test]
async fn a_question_turn_has_its_board_tools_refused_and_leaves_no_card() {
    let question = "Tell what is there in the tasks list";
    assert!(
        crate::company::task_intent::triage_message(question).is_answer(),
        "fixture must triage as a question, or this proves nothing"
    );
    let fx = Fixture::new();
    let turns = ScriptedTurns::new(
        &fx,
        vec![Turn::tooling(
            "here is the list",
            vec![Delegation::SpawnTask {
                title: "Tell what is there in the tasks list".to_string(),
                note: None,
                assignee: None,
            }],
        )],
    );
    let turn = fx
        .runner(&turns)
        .handle_operator_message("chief", question, None)
        .await
        .expect("operator message handled");

    assert_eq!(
        turns.claim_at_turn(0),
        orchestrator::DrainClaim::Answering,
        "a question turn runs under the narrowed claim"
    );
    assert_eq!(
        turns.staged(),
        vec![orchestrator::Staged::NoDrain(
            orchestrator::NoDrainReason::Triage
        )],
        "the refusal must be the do-not-retry one, and it must name the triage as the \
         cause rather than blaming a context that cannot do board work"
    );
    assert!(
        fx.cards().await.is_empty(),
        "a question left work on the board"
    );
    assert!(turn.spawned_task.is_none());
    // The reply still comes back: the gate removes the ability to write, not
    // the ability to answer.
    assert_eq!(turn.reply, "here is the list");
}

/// The other two pure board writes are refused on the same turn, for the
/// same reason: they change the board and return nothing to say, so they
/// have no answering role.
///
/// Pinned separately from `spawn_task` because the narrowed claim decides
/// per delegation kind, and a filter that let `assign_task` through would
/// pass every test above.
#[tokio::test]
async fn the_lifecycle_writes_are_refused_on_a_question_turn_too() {
    let fx = Fixture::new();
    let turns = ScriptedTurns::new(
        &fx,
        vec![Turn::tooling(
            "here is the list",
            vec![
                Delegation::AssignTask {
                    task_id: "card-1".to_string(),
                    assignee: "engineer".to_string(),
                    note: None,
                },
                Delegation::ReviewTask {
                    task_id: "card-1".to_string(),
                    decision: lifecycle::ReviewDecision::Approve,
                    note: None,
                },
            ],
        )],
    );
    fx.runner(&turns)
        .handle_operator_message("chief", "Tell what is there in the tasks list", None)
        .await
        .expect("operator message handled");

    assert_eq!(
        turns.staged(),
        vec![
            orchestrator::Staged::NoDrain(orchestrator::NoDrainReason::Triage),
            orchestrator::Staged::NoDrain(orchestrator::NoDrainReason::Triage),
        ],
        "neither lifecycle write may stage on a question turn"
    );
}

/// **Issue #267 review, finding 2.** `delegate_to_desk` is not only a board
/// write — it is how a question the orchestrator cannot answer alone gets
/// routed to a desk that can. Refusing it alongside the board writes left
/// "what did the design desk ship this week?" answerable by nobody.
///
/// So on a question turn the hand-off RUNS — it stages, the desk lead's
/// turn happens, and the CEO-relay hand-back surfaces their answer — and
/// only its *card* is suppressed. Every assertion here is one half of that:
/// the tool was not refused, three turns really ran, the answer came back,
/// and the board stayed empty.
#[tokio::test]
async fn a_hand_off_runs_on_a_question_turn_but_opens_no_card() {
    let question = "Tell what is there in the tasks list";
    assert!(
        crate::company::task_intent::triage_message(question).is_answer(),
        "fixture must triage as a question, or this proves nothing"
    );
    let fx = Fixture::new();
    let turns = ScriptedTurns::new(
        &fx,
        vec![
            Turn::tooling(
                "asking engineering",
                vec![handoff("what have you shipped?")],
            ),
            Turn::reply("we shipped the importer"),
            Turn::reply("engineering shipped the importer"),
        ],
    );
    let turn = fx
        .runner(&turns)
        .handle_operator_message("chief", question, Some("general"))
        .await
        .expect("operator message handled");

    assert_eq!(
        turns.staged(),
        vec![orchestrator::Staged::Queued],
        "the hand-off must NOT be refused: it is how the question gets answered"
    );
    let calls = turns.calls();
    assert_eq!(
        calls.len(),
        3,
        "the orchestrator, the desk lead and the relay all ran: {calls:?}"
    );
    assert_eq!(
        calls[1].0, "engineer",
        "the desk lead really ran: {calls:?}"
    );
    assert_eq!(
        turn.reply, "engineering shipped the importer",
        "and the operator gets the relayed answer"
    );
    assert!(
        fx.cards().await.is_empty(),
        "nobody commissioned work, so nothing is tracked"
    );
    assert!(
        turn.spawned_task.is_none(),
        "and the bubble claims no card, because there is none"
    );
}
