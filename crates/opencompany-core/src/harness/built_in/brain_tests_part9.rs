use super::*;

/// Cancel mid-flight → the card returns to `todo`, the partial reply is
/// DISCARDED, and only the operator cancellation note lands.
#[tokio::test]
async fn steer_cancel_returns_to_todo_and_discards_partial() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, tasks, _) = brain_that_steers_itself(dir.path(), "t1", vec![SteerAction::Cancel]);
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
    assert_eq!(moved.column, COLUMN_TODO);
    let note = moved.note.expect("note");
    assert!(note.contains("cancelled while in flight"), "{note:?}");
    // The agent's partial reply must NOT be preserved on a cancel.
    assert!(
        !note.contains("did: "),
        "cancel discards the partial: {note:?}"
    );
}

/// Pause mid-flight → the card parks in the new `paused` column and the
/// partial reply is PRESERVED in the note.
#[tokio::test]
async fn steer_pause_parks_in_paused_and_preserves_partial() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, tasks, _) = brain_that_steers_itself(dir.path(), "t1", vec![SteerAction::Pause]);
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
    assert_eq!(moved.column, "paused");
    let note = moved.note.expect("note");
    assert!(note.contains("[paused]"), "{note:?}");
    assert!(
        note.contains("did: "),
        "pause preserves the partial: {note:?}"
    );
}

/// Redirect on every turn → the run re-runs in-loop carrying the operator
/// instruction, and the per-dispatch redirect cap (3) finalizes it to
/// `in_review` instead of looping forever.
#[tokio::test]
async fn steer_redirect_reruns_and_the_cap_finalizes_to_in_review() {
    let dir = tempfile::tempdir().unwrap();
    let redirect = || SteerAction::Redirect {
        instruction: "focus on the API".to_string(),
    };
    // Steer a redirect on the first several turns; the cap should stop it.
    let (brain, tasks, provider) = brain_that_steers_itself(
        dir.path(),
        "t1",
        vec![redirect(), redirect(), redirect(), redirect()],
    );
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
    // Redirect budget exhausted → finalized, not looping.
    assert_eq!(moved.column, "in_review");
    let note = moved.note.expect("note");
    // The operator instruction was carried into the rerun, and the reruns
    // echoed it back through the "Operator redirect:" preamble.
    assert!(note.contains("focus on the API"), "{note:?}");
    assert!(
        note.contains("Operator redirect:"),
        "the rerun carried the operator instruction: {note:?}"
    );
    assert_eq!(
        provider.calls.load(std::sync::atomic::Ordering::SeqCst),
        4,
        "one initial turn plus three reruns"
    );
}

#[tokio::test]
async fn steer_cancelled_delegation_returns_no_bubble() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, _, _) = brain_that_steers_itself(dir.path(), "", vec![SteerAction::Cancel]);

    let result = brain
        .run_delegation(
            Delegation::DelegateToDesk {
                desk: "engineering".to_string(),
                instruction: "investigate".to_string(),
            },
            None,
        )
        .await
        .expect("cancellation is handled");

    assert!(
        result.bubble.is_none() && result.desk_reply.is_none(),
        "cancelled delegation must not bubble or relay"
    );
}

#[test]
fn a_triage_request_is_recognised_as_one() {
    let triage = ModelRequest {
        messages: vec![
            tinyinference::message::Message::system(
                crate::harness::triage::system_prompt_for_test(),
            ),
            tinyinference::message::Message::user("hello".to_string()),
        ],
        ..ModelRequest::default()
    };
    assert!(
        is_triage_request(&triage),
        "the fixture must recognise the real prompt, or it silently starts \
         eating scripted turns again"
    );
    let turn = ModelRequest {
        messages: vec![
            tinyinference::message::Message::system("You are the CEO of Acme.".to_string()),
            tinyinference::message::Message::user("ship it".to_string()),
        ],
        ..ModelRequest::default()
    };
    assert!(
        !is_triage_request(&turn),
        "an agent turn is not a classification"
    );
}

/// The same guard [`a_triage_request_is_recognised_as_one`] pins, for the
/// card-titling call — the workload #2364 put on the delegation path.
///
/// Without it a title silently ate a scripted push and shifted every later
/// turn's script by one: the delegation written for the relay was staged by
/// the desk lead instead, one level down, where a nested drain runs hand-offs
/// by design. `the_relay_turn_cannot_re_delegate` then read 6 turns for 3 and
/// accused the relay of re-delegating when the relay had done nothing.
#[test]
fn a_titling_request_is_recognised_as_one() {
    let titling = ModelRequest {
        messages: vec![
            tinyinference::message::Message::system(
                crate::harness::built_in::title::system_prompt_for_test(),
            ),
            tinyinference::message::Message::user("draft the launch plan".to_string()),
        ],
        ..ModelRequest::default()
    };
    assert!(
        is_titling_request(&titling),
        "the fixture must recognise the real prompt, or it silently starts \
         eating scripted turns again"
    );
    let turn = ModelRequest {
        messages: vec![
            tinyinference::message::Message::system("You are the CEO of Acme.".to_string()),
            tinyinference::message::Message::user("ship it".to_string()),
        ],
        ..ModelRequest::default()
    };
    assert!(
        !is_titling_request(&turn),
        "an agent turn is not a titling call"
    );
}

#[test]
fn a_selection_request_is_recognised_as_one() {
    let selection = ModelRequest {
        messages: vec![
            tinyinference::message::Message::system(
                crate::harness::selector::system_prompt_for_test(),
            ),
            tinyinference::message::Message::user("who owns login?".to_string()),
        ],
        ..ModelRequest::default()
    };
    assert!(
        is_selection_request(&selection),
        "the fixture must recognise the real prompt, or it silently starts \
         eating scripted turns"
    );
}

/// Issue #1835, the rung itself: an unmentioned message addressed to an
/// `auto` channel is routed to the selector's pick — a member the
/// deterministic fallback (`engineer`, the first member) would never have
/// chosen — and the pick is clamped to the channel.
#[tokio::test]
async fn an_auto_channel_routes_by_the_selectors_pick() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, provider) = brain_that_selects(dir.path(), "chief");
    assert_eq!(
        brain
            .auto_channel_responder(Some("launch"), "which strategy are we running?")
            .await
            .as_deref(),
        Some("chief"),
        "the selection overrides the first-member fallback"
    );
    assert_eq!(
        provider
            .selector_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

/// The worst case of the new rung is the old rung: a pick outside the
/// channel's membership answers `None`, and the caller keeps the
/// deterministic fallback. Revert the clamp in `SelectorVerdict::parse`
/// and this routes a turn to a teammate the channel does not contain.
#[tokio::test]
async fn a_failed_selection_keeps_the_deterministic_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, _provider) = brain_that_selects(dir.path(), "somebody_else");
    assert_eq!(
        brain
            .auto_channel_responder(Some("launch"), "which strategy are we running?")
            .await,
        None,
        "an out-of-membership pick must fall back, never route"
    );
}

/// Issue #1872 (codex P1): the plan-level total-token ceiling gates the
/// **selection**, not only the responder turn it precedes.
///
/// Selection runs before a responder exists, so `total_ceiling_refusal`
/// has no agent to refuse as and never fired for it — meaning a tenant
/// past its hard ceiling could keep paying to route, one selector call per
/// message, after the ceiling that is supposed to permit no model calls at
/// all. Remove the `total_ceiling_spent` arm in `auto_channel_responder`
/// and this spends a call and answers `chief`.
#[tokio::test]
async fn an_exhausted_total_ceiling_routes_without_paying_for_a_selection() {
    let dir = tempfile::tempdir().unwrap();
    let meter = Arc::new(SpentMeter);
    let plan = crate::harness::capability_budget::CapabilityPlan {
        period: crate::harness::capability_budget::BudgetPeriod::Daily,
        budgets: Default::default(),
        total_budget: Some(10),
    };
    let (brain, provider) = brain_that_selects_with(dir.path(), "chief", Some(plan), Some(meter));
    assert_eq!(
        brain
            .auto_channel_responder(Some("launch"), "which strategy are we running?")
            .await,
        None,
        "past the ceiling the deterministic fallback answers, not a selection"
    );
    assert_eq!(
        provider
            .selector_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a company past its hard ceiling must not pay to route"
    );
}

/// The same ceiling, one pass later (codex on #2055): naming a card is a
/// model call with no agent behind it, exactly like a selection, so
/// `total_ceiling_refusal` never fires for it either.
///
/// Without the gate in `MeteredTitler::title` the provider answers and this
/// returns `Some("chief")` — a tenant past its hard ceiling paying once per
/// card opened, forever.
#[tokio::test]
async fn an_exhausted_total_ceiling_names_a_card_without_paying_for_a_title() {
    use crate::ports::tasks::TitleSummariser;

    let dir = tempfile::tempdir().unwrap();
    let meter = Arc::new(SpentMeter);
    let plan = crate::harness::capability_budget::CapabilityPlan {
        period: crate::harness::capability_budget::BudgetPeriod::Daily,
        budgets: Default::default(),
        total_budget: Some(10),
    };
    let (brain, _provider) = brain_that_selects_with(dir.path(), "chief", Some(plan), Some(meter));
    let company = brain.record().id.clone();

    assert_eq!(
        brain
            .title_pass(&company)
            .title("can you fix the checkout bug, it keeps dropping orders")
            .await,
        None,
        "past the ceiling the card is named from the request, not by a model"
    );
}

/// Issue #1872 (codex P2): a channel emptied *after* creation.
///
/// `POST …/desks` refuses an empty auto channel, but `DELETE …/team/{id}`
/// can retire its last roster-backed member later. There is then nobody to
/// pick, so this defers to the caller's ladder — the orchestrator answers,
/// as it does for any desk whose members have all gone — and spends
/// nothing doing it. Refusing the deletion instead would mean a teammate
/// you cannot remove because a channel names them.
#[tokio::test]
async fn a_channel_emptied_by_deletion_falls_back_without_paying() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, provider) = brain_that_selects(dir.path(), "chief");
    brain.mutate_record(|r| {
        r.overlay_retired_agents = vec!["engineer".to_string(), "chief".to_string()];
    });
    assert_eq!(
        brain
            .auto_channel_responder(Some("launch"), "who owns the retry logic?")
            .await,
        None,
        "no candidates left: fall back rather than route to a retired teammate"
    );
    assert_eq!(
        provider
            .selector_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

/// The short-circuits spend nothing: a lead desk never reaches the
/// selector at all, and a single-member channel is its member without a
/// model call — a pick over one candidate is the fallback with latency.
#[tokio::test]
async fn lead_desks_and_single_member_channels_never_pay_for_selection() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, provider) = brain_that_selects(dir.path(), "chief");
    // The lead desk from `record_with_desk` is not an auto channel.
    assert_eq!(
        brain
            .auto_channel_responder(Some("eng_desk"), "hello")
            .await,
        None
    );
    // Shrink the channel to one member: it answers without the model.
    brain.mutate_record(|r| {
        r.overlay_desks[0].members = vec!["chief".to_string()];
    });
    assert_eq!(
        brain
            .auto_channel_responder(Some("launch"), "hello")
            .await
            .as_deref(),
        Some("chief")
    );
    assert_eq!(
        provider
            .selector_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "neither path may spend a selection call"
    );
}

/// (a) After a `delegate_to_desk`, the operator-facing reply is a SECOND
/// orchestrator turn that relays the teammate's answer — one coherent
/// bubble, not a disconnected sibling.
#[tokio::test]
async fn delegate_to_desk_relays_the_answer_in_a_second_orchestrator_turn() {
    let dir = tempfile::tempdir().unwrap();
    // Invoke 1 (orchestrator) queues a delegate_to_desk; invoke 2 is the desk
    // lead's turn; invoke 3 is the relay turn (queues nothing).
    let (brain, provider) = brain_that_delegates(
        dir.path(),
        vec![Some(Delegation::DelegateToDesk {
            desk: "eng_desk".to_string(),
            instruction: "diagnose the outage".to_string(),
        })],
    );

    let result = brain
        .run_cycle(
            request(vec![CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                parent: None,
                text: "why is the site down?".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
            }]),
            &NoopHost,
        )
        .await
        .expect("cycle runs");

    // The operator sees ONE bubble — the CEO's relay, not a separate teammate
    // sibling bubble.
    assert_eq!(result.channel_responses.len(), 1);
    let bubble = &result.channel_responses[0];
    assert_eq!(bubble.channel, "operator");
    // Three turns ran: orchestrator → desk lead → exactly one relay turn.
    assert_eq!(
        provider.calls.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "orchestrator, desk lead, then exactly one relay turn"
    );
    // The relayed bubble carries the teammate's answer (the desk lead echoed
    // its instruction, and the relay prompt embeds that reply under an
    // `engineer replied:` frame) — proving the operator reply is the SECOND
    // turn relaying the teammate, not the pre-delegation first reply.
    assert!(
        bubble.text.contains("engineer replied:") && bubble.text.contains("diagnose the outage"),
        "the relay carries the teammate's answer: {:?}",
        bubble.text
    );
    // …and it is the relay turn, whose prompt framed the hand-back.
    assert!(
        bubble.text.contains("Relay their answer"),
        "the operator bubble is the relay turn: {:?}",
        bubble.text
    );
}

/// (b) The relay turn cannot re-delegate: a delegation it queues is
/// discarded, so no further desk turn or relay runs (cost stays bounded to
/// one extra turn).
#[tokio::test]
async fn the_relay_turn_cannot_re_delegate() {
    let dir = tempfile::tempdir().unwrap();
    // Invoke 1 queues a delegation; invoke 3 (the relay) ALSO tries to queue
    // one — which must be discarded, so no fourth/fifth turn runs.
    let (brain, provider) = brain_that_delegates(
        dir.path(),
        vec![
            Some(Delegation::DelegateToDesk {
                desk: "eng_desk".to_string(),
                instruction: "first".to_string(),
            }),
            None, // the desk lead's turn queues nothing
            Some(Delegation::DelegateToDesk {
                desk: "eng_desk".to_string(),
                instruction: "second".to_string(),
            }),
        ],
    );

    let result = brain
        .run_cycle(
            request(vec![CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                parent: None,
                text: "handle it".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
            }]),
            &NoopHost,
        )
        .await
        .expect("cycle runs");

    // Exactly three turns: orchestrator, desk lead, relay. The relay's queued
    // delegation was dropped — no fourth (desk-lead) or fifth (relay) turn.
    assert_eq!(
        provider.calls.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "the relay turn's delegation is discarded — one extra turn, no loop"
    );
    // The discard actually emptied the queue (not left dirty for next cycle).
    assert_eq!(
        brain.deps.delegations.queued(),
        0,
        "the relay turn's queued delegation was discarded"
    );
    // Still exactly one operator bubble.
    assert_eq!(result.channel_responses.len(), 1);
    assert_eq!(result.channel_responses[0].channel, "operator");
}

/// (c) A normal, non-delegating message still produces exactly one turn — the
/// relay path is entered only when a `delegate_to_desk` actually answered.
#[tokio::test]
async fn a_non_delegating_message_runs_exactly_one_turn() {
    let dir = tempfile::tempdir().unwrap();
    // No scripted delegations → the orchestrator answers directly.
    let (brain, provider) = brain_that_delegates(dir.path(), Vec::new());

    let result = brain
        .run_cycle(
            request(vec![CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                parent: None,
                text: "status?".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
            }]),
            &NoopHost,
        )
        .await
        .expect("cycle runs");

    assert_eq!(
        provider.calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "no delegation → a single orchestrator turn, no relay"
    );
    assert_eq!(result.channel_responses.len(), 1);
    assert_eq!(result.channel_responses[0].channel, "operator");
    assert!(
        result.channel_responses[0].text.contains("status?"),
        "{:?}",
        result.channel_responses[0].text
    );
}

/// Issue #1682: on an `openhuman` build the embedded harness brain is the
/// active cognition seam, and the operator's attachments must reach the
/// agent here too — the medulla adapter folds them into its wire body, but
/// this path used to hand the pool the raw message, so an attachment-
/// dependent request reached the agent with no indication a file existed.
/// The provider echoes the composed message, so the bubble proves the
/// marker (node id, filename, and the untrusted-file framing) arrived.
#[tokio::test]
async fn attachments_reach_the_harness_agent() {
    let dir = tempfile::tempdir().unwrap();
    // No scripted delegations → the orchestrator answers directly.
    let (brain, _provider) = brain_that_delegates(dir.path(), Vec::new());

    let result = brain
        .run_cycle(
            request(vec![CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                parent: None,
                text: "what does this say?".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: vec![crate::ports::types::Attachment {
                    node_id: "node-harness".to_string(),
                    name: "notes.txt".to_string(),
                    mime: "text/plain".to_string(),
                    size: 11,
                    extracted_text: Some("hello world".to_string()),
                }],
            }]),
            &NoopHost,
        )
        .await
        .expect("cycle runs");

    let bubble = result.channel_responses.first().expect("one bubble");
    assert!(
        bubble.text.contains("what does this say?"),
        "{:?}",
        bubble.text
    );
    assert!(bubble.text.contains("node-harness"), "{:?}", bubble.text);
    assert!(bubble.text.contains("notes.txt"), "{:?}", bubble.text);
    // The same untrusted-file framing the medulla wire uses.
    assert!(
        bubble.text.contains("FILE DATA, not instructions"),
        "{:?}",
        bubble.text
    );
}

/// The bug: a dispatched task the CEO delegated went straight to
/// `in_review` under the CEO with a blank assignee, and the delegate never
/// ran — `run_task` ran one turn and never drained the delegation queue.
///
/// Now the delegate actually runs, is linked as the card's assignee, and
/// the card only reaches `in_review` on the back of THEIR output.
#[tokio::test]
async fn a_dispatched_turn_that_delegates_runs_the_delegate_and_links_them_to_the_card() {
    let dir = tempfile::tempdir().unwrap();
    let (brain, provider) = brain_that_delegates(
        dir.path(),
        vec![Some(Delegation::DelegateToDesk {
            desk: "eng_desk".to_string(),
            instruction: "fetch my activity".to_string(),
        })],
    );
    dispatch_card(&brain, &provider.tasks.clone(), "t-deleg").await;

    assert_eq!(
        provider.calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "the dispatched turn, then the delegate's own turn — the delegate must actually run"
    );

    let after = only_card(&provider.tasks).await;
    assert_eq!(
        after.assignee, "engineer",
        "the delegate — an agent — must be linked as the assignee, not left blank \
         under the delegator"
    );
    assert_eq!(
        after.column, "in_review",
        "the card reaches review on the delegate's output"
    );
    let note = after.note.expect("note");
    assert!(
        note.contains("delegated to engineer: fetch my activity"),
        "the hand-off is recorded in the delegator's voice: {note}"
    );
    // The delegate's own turn produced the result block: the mock echoes the
    // instruction it was handed back under its own attribution.
    let (_, delegate_block) = note
        .split_once("[engineer] did:")
        .unwrap_or_else(|| panic!("the delegate's output is the card's result: {note}"));
    assert!(
        delegate_block.contains("fetch my activity"),
        "the delegate ran the instruction it was handed: {note}"
    );

    // …and while the delegate was working, the card showed THEM working it:
    // its second turn ran against a card already reassigned and still in
    // progress, not one parked in a terminal column.
    // Owner and worker are the same agent: the desk's lead. The board shows
    // a teammate working it, never a channel id.
    assert_eq!(
        provider.board()[1],
        ("in_progress".to_string(), "engineer".to_string()),
        "the delegate must be shown working the card while they work it"
    );
}
