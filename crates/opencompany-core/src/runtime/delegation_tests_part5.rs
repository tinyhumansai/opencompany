use super::tests_core2::*;

/// **Issue #984, the second caller.** `open_work_card` has two callers, and
/// the test above this one only drives the direct path. This drives the
/// hand-off path: the orchestrator queues `delegate_to_desk` on a message
/// the lexical layer abstained on and the model read as `chatter`.
///
/// The shape mirrors `a_hand_off_runs_on_a_question_turn_but_opens_no_card`
/// exactly, because the requirement is the same one: only the **card**
/// stands down. The hand-off is not refused, the desk lead's turn really
/// runs, and the relayed answer still reaches the operator — a verdict that
/// silenced the company instead of the board would be a worse bug than the
/// one #984 reports.
///
/// This is the test that would have caught #442's mistake, which put a
/// stand-down in one caller and left the other opening cards.
#[tokio::test]
async fn a_hand_off_of_a_message_the_model_calls_chatter_opens_no_card() {
    let residue = "the deck looks good to me";
    assert!(
        crate::company::task_intent::triage_message_detailed(residue).abstained(),
        "fixture must be a message no lexical rule decides"
    );
    assert!(
        is_trackable_work(residue),
        "and one the card detector would otherwise track, or this proves nothing"
    );
    let fx = Fixture::new();
    let escalation = ScriptedTriage::new(crate::harness::triage::TriageVerdict::Chatter);
    let turns = ScriptedTurns::new(
        &fx,
        vec![
            Turn::tooling(
                "asking engineering",
                vec![handoff("take a look at the deck")],
            ),
            Turn::reply("looks fine to me too"),
            Turn::reply("engineering agrees the deck is fine"),
        ],
    );
    let turn = fx
        .runner(&turns)
        .with_triage(&escalation)
        .handle_operator_message("chief", residue, Some("general"))
        .await
        .expect("operator message handled");

    assert_eq!(
        turns.staged(),
        vec![orchestrator::Staged::Queued],
        "a chatter verdict must NOT refuse the hand-off — it does not gate tools"
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
        turn.reply, "engineering agrees the deck is fine",
        "and the operator still gets the relayed answer"
    );
    assert!(
        fx.cards().await.is_empty(),
        "but the hand-off card stands down: the model read this as conversation"
    );
    assert!(
        turn.spawned_task.is_none(),
        "and nothing is linked to a card"
    );
}

/// The paired opposite, for the same reason the question pair is paired: a
/// "fix" that simply stopped opening hand-off cards would satisfy the test
/// above. A `work` verdict on the same abstaining message still cards.
#[tokio::test]
async fn the_same_hand_off_on_a_work_verdict_still_opens_its_card() {
    let residue = "the deck looks good to me";
    let fx = Fixture::new();
    let escalation = ScriptedTriage::new(crate::harness::triage::TriageVerdict::Work);
    let turns = ScriptedTurns::new(
        &fx,
        vec![
            Turn::tooling(
                "asking engineering",
                vec![handoff("take a look at the deck")],
            ),
            Turn::reply("looks fine to me too"),
            Turn::reply("engineering agrees the deck is fine"),
        ],
    );
    fx.runner(&turns)
        .with_triage(&escalation)
        .handle_operator_message("chief", residue, Some("general"))
        .await
        .expect("operator message handled");

    let cards = fx.cards().await;
    assert_eq!(
        cards.len(),
        1,
        "a work verdict leaves the hand-off card the abstention would have opened"
    );
    assert_eq!(cards[0].assignee, "engineer");
}

/// **Issue #1152, the second caller.** `open_work_card` has two callers, and
/// the direct-path test only drives one. This drives the hand-off: the
/// orchestrator queues `delegate_to_desk` on a message the operator sent as
/// chat.
///
/// The shape mirrors `a_hand_off_of_a_message_the_model_calls_chatter_opens_no_card`
/// exactly, because the requirement is the same: **only the card** stands
/// down. Saying "I'm just chatting" must not silence the company — the
/// hand-off is not refused, the desk lead's turn really runs, and the
/// relayed answer still reaches the operator.
///
/// The `staged()` assertion is the load-bearing one, and it is what pins the
/// scope this deliberately does not take: the turn's board tools are NOT
/// narrowed, so a card can still appear if the orchestrator explicitly
/// spawns one. "Just chatting" means the company will not *automatically*
/// card the message.
///
/// Non-vacuous by the same pairing as the chatter test above it:
/// `the_same_hand_off_on_a_work_verdict_still_opens_its_card` opens a card
/// on these very words with no composer choice.
#[tokio::test]
async fn a_hand_off_of_a_message_the_operator_sent_as_chat_opens_no_card() {
    let residue = "the deck looks good to me";
    assert!(
        is_trackable_work(residue),
        "fixture must be one the card detector would otherwise track"
    );
    let fx = Fixture::new();
    let turns = ScriptedTurns::new(
        &fx,
        vec![
            Turn::tooling(
                "asking engineering",
                vec![handoff("take a look at the deck")],
            ),
            Turn::reply("looks fine to me too"),
            Turn::reply("engineering agrees the deck is fine"),
        ],
    );
    let turn = fx
        .runner(&turns)
        .requested(Some(crate::ports::types::MessageIntent::Chat))
        .handle_operator_message("chief", residue, Some("general"))
        .await
        .expect("operator message handled");

    assert_eq!(
        turns.staged(),
        vec![orchestrator::Staged::Queued],
        "a chat intent must NOT refuse the hand-off — it does not gate tools"
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
        turn.reply, "engineering agrees the deck is fine",
        "and the operator still gets the relayed answer"
    );
    assert!(
        fx.cards().await.is_empty(),
        "but the hand-off card stands down: the operator said this is not work"
    );
    assert!(
        turn.spawned_task.is_none(),
        "and nothing is linked to a card"
    );
}

/// The same hand-off on a message that is NOT a question still opens its
/// card, so the suppression above is keyed on the triage rather than having
/// quietly disabled the #442 card path.
///
/// Paired with the test above for the reason `a_desk_that_finished_cleanly_
/// still_lands_in_review` is paired with its own opposite: a "fix" that
/// simply stopped opening hand-off cards would satisfy one of them.
#[tokio::test]
async fn the_same_hand_off_on_a_non_question_still_opens_its_card() {
    let fx = Fixture::new();
    let turns = ScriptedTurns::new(
        &fx,
        vec![
            Turn::tooling(
                "asking engineering",
                vec![handoff("Read the pricing repo and write modules.md")],
            ),
            Turn::reply("modules.md is written"),
            Turn::reply("engineering wrote it up"),
        ],
    );
    let turn = fx
        .runner(&turns)
        .handle_operator_message("chief", "the pricing repo needs a map", Some("general"))
        .await
        .expect("operator message handled");

    assert_eq!(turns.staged(), vec![orchestrator::Staged::Queued]);
    let cards = fx.cards().await;
    assert_eq!(
        cards.len(),
        1,
        "the hand-off card is still opened: {cards:?}"
    );
    assert_eq!(cards[0].assignee, "engineer");
    assert_eq!(turn.spawned_task.as_deref(), Some(cards[0].id.as_str()));
}

/// `Chatter` is NOT gated. It is the ambiguous bucket, and taking board
/// tools away on a maybe would turn a triage miss into work the company
/// silently refuses to do.
#[tokio::test]
async fn an_ambiguous_message_keeps_its_board_tools() {
    let neutral = "the deck looks good to me";
    assert_eq!(
        crate::company::task_intent::triage_message(neutral),
        crate::company::task_intent::MessageTriage::Chatter,
        "fixture must be the ambiguous bucket, or this proves nothing"
    );
    let fx = Fixture::new();
    let turns = ScriptedTurns::new(
        &fx,
        vec![
            Turn::tooling(
                "noted — I'll track the follow-up",
                vec![Delegation::SpawnTask {
                    title: "Follow up on the deck".to_string(),
                    note: None,
                    assignee: None,
                }],
            ),
            Turn::reply("relayed"),
        ],
    );
    let turn = fx
        .runner(&turns)
        .handle_operator_message("chief", neutral, None)
        .await
        .expect("operator message handled");

    assert!(
        turns.committed_at_turn(0),
        "chatter must still run under a claim"
    );
    assert_eq!(turns.staged(), vec![orchestrator::Staged::Queued]);
    let cards = fx.cards().await;
    assert_eq!(cards.len(), 1, "the delegation still ran: {cards:?}");
    assert_eq!(cards[0].title, "Follow up on the deck");
    assert_eq!(turn.spawned_task.as_deref(), Some(cards[0].id.as_str()));
}

/// A bare greeting is `Chatter` by a RULE FIRING (`is_matched_chatter`),
/// not by abstention like `an_ambiguous_message_keeps_its_board_tools`'s
/// fixture above — so it never reaches the escalation block, which only
/// ever runs on an abstained triage. The greeting fast path (issue #1725)
/// must still fire for it.
///
/// Goes through the real classification path
/// (`handle_operator_message`) rather than forcing
/// `with_chat_only_hint(true, ..)` directly, per review: a test that
/// forces the scope itself cannot catch the classifier failing to derive
/// the hint in the first place.
#[tokio::test]
async fn a_bare_greeting_enters_the_chat_only_fast_path() {
    let greeting = "hi";
    let triaged = crate::company::task_intent::triage_message_detailed(greeting);
    assert_eq!(
        triaged.triage,
        crate::company::task_intent::MessageTriage::Chatter,
        "fixture must be chatter, or this proves nothing"
    );
    assert!(
        !triaged.abstained(),
        "fixture must be a MATCHED chatter (a bare-greeting rule firing) — \
         the exact case that never reaches the escalation block, and the \
         one the classifier used to miss"
    );

    let fx = Fixture::new();
    let turns = ScriptedTurns::new(&fx, vec![Turn::reply("Hi! How can I help you today?")]);
    fx.runner(&turns)
        .handle_operator_message("chief", greeting, Some("general"))
        .await
        .expect("operator message handled");

    assert!(
        turns.chat_only_at_turn(0),
        "a bare greeting must enter CHAT_ONLY_TURN"
    );
}

/// Codex review round 2: `is_pure_small_talk`'s `SMALLTALK_OPENERS` (a
/// first-WORD opener list, `runtime::delegation`) is independently
/// maintained from `task_intent::GREETINGS` (a whole-MESSAGE match list) —
/// so a message the lexical triage matches as `Chatter` can still fail
/// `is_pure_small_talk` and fall back to the full agentic turn. "sup" is in
/// `GREETINGS` but has no corresponding entry in `SMALLTALK_OPENERS`,
/// making it a fixture the vocabularies disagree on.
#[tokio::test]
async fn a_matched_greeting_absent_from_smalltalk_openers_still_fast_paths() {
    let greeting = "sup";
    let triaged = crate::company::task_intent::triage_message_detailed(greeting);
    assert_eq!(
        triaged.triage,
        crate::company::task_intent::MessageTriage::Chatter,
        "fixture must be chatter, or this proves nothing"
    );
    assert!(
        !triaged.abstained(),
        "fixture must be a MATCHED chatter (a GREETINGS whole-message hit)"
    );

    let fx = Fixture::new();
    let turns = ScriptedTurns::new(&fx, vec![Turn::reply("Not much, what's up?")]);
    fx.runner(&turns)
        .handle_operator_message("chief", greeting, Some("general"))
        .await
        .expect("operator message handled");

    assert!(
        turns.chat_only_at_turn(0),
        "a lexically matched greeting must enter CHAT_ONLY_TURN even when \
         `is_pure_small_talk`'s independently maintained opener list has no \
         matching entry for it"
    );
}

/// `Track` is unchanged: a real instruction still runs under a claim, still
/// delegates, and the #463 stand-down still holds.
#[tokio::test]
async fn a_tracked_instruction_still_delegates_under_a_claim() {
    let imperative = "draft the launch plan for next quarter";
    assert!(
        crate::company::task_intent::detect_task_intent(imperative).is_some(),
        "fixture must be a message the chat handler cards"
    );
    let fx = Fixture::new();
    let turns = ScriptedTurns::new(
        &fx,
        vec![
            Turn::tooling("handing it over", vec![handoff("Draft the launch plan.")]),
            Turn::reply("drafted"),
            Turn::reply("the desk drafted it"),
        ],
    );
    fx.runner(&turns)
        .handle_operator_message("chief", imperative, None)
        .await
        .expect("operator message handled");

    assert!(turns.committed_at_turn(0), "an instruction turn is claimed");
    assert_eq!(turns.staged(), vec![orchestrator::Staged::Queued]);
    assert_eq!(
        turns.calls().len(),
        3,
        "the hand-off, the desk turn and the relay all ran: {:?}",
        turns.calls()
    );
    // The hand-off opens its own card — the one card for this message, since
    // the chat handler no longer cards on the triage (the board is a tool
    // call), and this hand-off IS the tool call.
    let cards = fx.cards().await;
    assert_eq!(cards.len(), 1, "one hand-off, one card: {cards:?}");
    assert_eq!(cards[0].assignee, "engineer");
}

/// The third card path (issue #442 path one), which a triage layer looking
/// only at the orchestrator would miss: asking a desk lead a question about
/// their own work runs their turn directly, and `is_trackable_work` — whose
/// default is `true` — used to read a sentence that long as work.
#[tokio::test]
async fn a_desk_lead_asked_a_question_directly_opens_no_card() {
    // Long enough and free enough of question punctuation that the
    // opposite-defaults detector on this path calls it work on its own.
    let question = "Tell me what you shipped this week and what is still open on your plate";
    assert!(
        is_trackable_work(question),
        "the fixture must be work by the direct path's own default, or the \
         stand-down under test is doing nothing"
    );
    assert!(
        crate::company::task_intent::triage_message(question).is_answer(),
        "…and a question by the triage, which is what must outrank it"
    );
    let fx = Fixture::new();
    let turns = ScriptedTurns::new(&fx, vec![Turn::reply("shipped the importer")]);
    let turn = fx
        .runner(&turns)
        .handle_operator_message("engineer", question, Some("eng_desk"))
        .await
        .expect("operator message handled");

    assert!(
        fx.cards().await.is_empty(),
        "asking a desk lead what they shipped opened a card"
    );
    assert!(turn.spawned_task.is_none());
    assert_eq!(turn.reply, "shipped the importer");
}

// ── Issue #884 D1: a desk lead can hand a slice to a peer ───────────────

/// **The D1 regression.** An operator posts into a three-person desk asking
/// for one member by name. The desk lead answers — `responder_for` resolves
/// a desk to its lead — reads the request correctly, and hands the slice on
/// with `delegate_to_teammate`. The named teammate's turn must actually run.
///
/// Before #884 that hand-off had no tool behind it: `delegate_to_desk` only
/// ever resolves to the desk's lead, and handing the lead's own desk back to
/// itself is refused as self-delegation. The lead's only remaining move was
/// the refusal the issue reports — "this task isn't mine, it's addressed to
/// the SEO Specialist" — and the request produced nothing.
///
/// Asserted on the **resolved agent id** that ran, never on rendered text.
/// The plausible wrong implementation resolves a teammate hand-off through
/// `desk_lead` like its sibling does, which would run the Brand Strategist a
/// second time and satisfy any assertion phrased about the reply.
#[tokio::test]
async fn a_desk_lead_hands_a_slice_to_the_teammate_the_operator_named() {
    let fx = Fixture::peers();
    let brief = "run an SEO pass over the pricing page";
    let turns = ScriptedTurns::new(
        &fx,
        vec![
            // The desk lead's own turn: it hands the slice on.
            Turn::tooling(
                "passing this to our SEO specialist",
                vec![peer_handoff("seo_specialist", brief)],
            ),
            // The teammate's turn — the one that did not exist before #884.
            Turn::reply("done: 12 findings, 3 blocking"),
            // The lead relays the answer back.
            Turn::reply("SEO pass done — 12 findings, 3 blocking"),
        ],
    );

    fx.runner(&turns)
        .handle_operator_message(
            "brand_strategist",
            "SEO Specialist: run an SEO pass over the pricing page",
            Some("strategy"),
        )
        .await
        .expect("operator message handled");

    assert_eq!(
        turns.staged(),
        vec![orchestrator::Staged::Queued],
        "the hand-off must be accepted at the real tool boundary"
    );
    let agents: Vec<String> = turns.calls().into_iter().map(|(agent, _)| agent).collect();
    assert_eq!(
        agents,
        [
            "brand_strategist".to_string(),
            "seo_specialist".to_string(),
            "brand_strategist".to_string()
        ],
        "the SEO specialist's own turn must run between the lead's and its relay"
    );
    assert_eq!(
        turns.calls()[1].1,
        brief,
        "and it must be handed the instruction, not the operator's raw message"
    );
    assert_eq!(
        *turns.history_seed_at_turn.lock().unwrap(),
        vec![true, false, true]
    );
    // The hand-off is tracked by construction, assigned to the teammate that
    // ran it — the same guarantee #442 gave the desk form.
    let cards = fx.cards().await;
    assert!(
        cards.iter().any(|c| c.assignee == "seo_specialist"),
        "the hand-off must open a card owned by whoever actually did it: {cards:?}"
    );
    // ONE card: the hand-off's. A desk lead asked directly opens nothing for
    // the operator's own message any more — tracking is its own tool call —
    // so the specialist's card is the only one this exchange leaves.
    assert_eq!(cards.len(), 1, "{cards:?}");
}

/// A teammate removed from the roster between the tool call and the drain
/// cannot be handed anything, and the drain says so rather than running
/// somebody else — the mirror of the desk form's leadless refusal.
#[tokio::test]
async fn a_teammate_hand_off_to_a_non_roster_id_runs_nobody() {
    let fx = Fixture::peers();
    let turns = ScriptedTurns::new(
        &fx,
        vec![Turn::queueing(
            "handing it on",
            vec![peer_handoff("ghost", "do the thing")],
        )],
    );
    fx.runner(&turns)
        .handle_operator_message(
            "brand_strategist",
            "sort out the pricing page",
            Some("strategy"),
        )
        .await
        .expect("operator message handled");

    let agents: Vec<String> = turns.calls().into_iter().map(|(agent, _)| agent).collect();
    assert_eq!(
        agents,
        ["brand_strategist".to_string()],
        "an unresolvable teammate must run NO second turn — least of all the \
         orchestrator's, which is the D2 failure one seam over"
    );
}

/// A→B→A cannot ping-pong. Two guards, and this pins the one that is
/// reachable from a fixture: the depth cap refuses the hand-off back at the
/// tool boundary, in the model's own turn, and nothing is queued.
///
/// (The cycle guard proper — "that teammate is where the work came from" —
/// lives on the tool's grounding and is pinned in `delegation_tools`; this
/// is the bound that holds even for a ring the cycle guard cannot see.)
#[tokio::test]
async fn a_teammate_cannot_hand_the_work_back_past_the_depth_cap() {
    let fx = Fixture::peers();
    let turns = ScriptedTurns::new(
        &fx,
        vec![
            Turn::tooling(
                "passing it on",
                vec![peer_handoff("seo_specialist", "look at the pricing page")],
            ),
            // B tries to hand it straight back to A, one level down.
            Turn::tooling(
                "back to you",
                vec![peer_handoff("brand_strategist", "you take it")],
            ),
            Turn::reply("here is what came back"),
        ],
    )
    .with_max_depth(1);

    fx.runner(&turns)
        .handle_operator_message(
            "brand_strategist",
            "sort out the pricing page",
            Some("strategy"),
        )
        .await
        .expect("operator message handled");

    assert_eq!(
        turns.staged(),
        vec![
            orchestrator::Staged::Queued,
            orchestrator::Staged::NoDrain(orchestrator::NoDrainReason::Depth),
        ],
        "the hand-off back must be refused at the bound, not queued and run"
    );
    let agents: Vec<String> = turns.calls().into_iter().map(|(agent, _)| agent).collect();
    assert_eq!(
        agents,
        [
            "brand_strategist".to_string(),
            "seo_specialist".to_string(),
            "brand_strategist".to_string()
        ],
        "exactly one delegated turn ran, then the relay — no third hop"
    );
    assert_eq!(fx.queue.queued(), 0, "nothing survived the refusal");
}

/// The orchestrator's own copy reaches a teammate directly too, without the
/// desk's lead standing in the way.
#[tokio::test]
async fn the_orchestrator_can_hand_work_to_a_non_lead_teammate() {
    let fx = Fixture::peers();
    let turns = ScriptedTurns::new(
        &fx,
        vec![
            Turn::tooling(
                "asking our copywriter",
                vec![peer_handoff("copywriter", "draft the launch note")],
            ),
            Turn::reply("draft attached"),
            Turn::reply("here is the draft"),
        ],
    );
    fx.runner(&turns)
        .handle_operator_message("chief", "get the launch note drafted", None)
        .await
        .expect("operator message handled");

    let agents: Vec<String> = turns.calls().into_iter().map(|(agent, _)| agent).collect();
    assert_eq!(
        agents,
        [
            "chief".to_string(),
            "copywriter".to_string(),
            "chief".to_string()
        ],
        "the copywriter is not the strategy desk's lead, and must still be reachable"
    );
}

/// The orchestrator's own chat turn is not tracked here. It is the front
/// door — every message arrives there — and what it does with *work* is hand
/// it off, which opens a card on the other path.
#[tokio::test]
async fn the_orchestrators_own_chat_turn_opens_no_card() {
    let fx = Fixture::new();
    let turns = ScriptedTurns::new(&fx, vec![Turn::reply("noted")]);
    fx.runner(&turns)
        .handle_operator_message("chief", "draft the investor update", None)
        .await
        .expect("operator message handled");
    assert!(fx.cards().await.is_empty());
}
