//! Tests for the four repairs the live Project Euler 12 episode asked for.
//!
//! One six-member desk, `quorum = 3`, `require_evidential = true`, and a
//! `moves` table restricting `commit`, produced: quorum at the third evidential
//! support, the phase flipped to `Commit`, and the floor then handed to three
//! seats the table had barred from committing. Each was shown a no-move block,
//! wrote prose, and the room reported itself `Exhausted` on an answer it had
//! already carried. Earlier turns went on three synonyms for one topic id and
//! on a `!support` citing the proposal rather than a fact.
//!
//! Each test here pins one of those four back down, scripted through
//! [`HiveTurnRunner`] so what is asserted is this host's behaviour rather than
//! a model's compliance.

use std::sync::Arc;

use super::moves_test::{Runner, manifest_with, open};
use super::test::{MemoryLog, desk_of};
use super::*;
use crate::ports::events::EventLog;

// ---------------------------------------------------------------------------
// 1. `commit` is never a restricted move
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_seat_the_table_never_named_for_commit_still_records_the_decision() {
    let log = Arc::new(MemoryLog::default());
    let trigger = open(&log).await;
    // The live shape, in miniature: planner proposes, scout grounds it, and
    // critic — whose entry names neither `commit` nor anything else that could
    // carry a topic — is the seat that has to record what carried.
    let manifest = manifest_with(
        "hive = { turn_budget = 9, quorum = 2, blind_round = false, require_evidential = true, \
         moves = { planner = [\"propose\", \"support\"], scout = [\"evidence\", \"support\"], \
         critic = [\"object\"] } }",
    );
    let desk = desk_of(&manifest, "eng").expect("a room");
    let runner = Runner::new(&[
        (
            "planner",
            "!propose #stage Stage the rollout behind a flag.",
        ),
        (
            "scout",
            "!evidence #stage ^1 The last full rollout took checkout down for 40 minutes.",
        ),
        (
            "scout",
            "!support #stage ^3 The outage is the reason to stage.",
        ),
        ("critic", "!commit #stage ^3 Recorded."),
        ("planner", "!commit #stage ^3 Recorded."),
    ]);

    let outcome = EpisodeDriver::new(
        MemoryLog::company(),
        desk,
        Arc::clone(&log) as Arc<dyn EventLog>,
        &runner,
        "Decide the rollout.",
    )
    .run(trigger)
    .await
    .expect("the episode runs");

    assert!(
        matches!(&outcome.ending, EpisodeEnding::Converged { topic, .. } if topic == "stage"),
        "a room whose committing seat was never given `commit` still converges: {outcome:?}"
    );
    // Nothing was demoted: `!commit` is not a move any table gates.
    assert!(outcome.violations.is_empty(), "{:?}", outcome.violations);
    let replies = log.replies("eng");
    assert!(
        replies
            .iter()
            .any(|(_, text)| text.starts_with("!commit #stage")),
        "the commit line keeps its marker: {replies:?}"
    );
}

#[test]
fn the_commit_prompt_names_the_carried_topic() {
    let desk = desk_of(
        &manifest_with(
            "hive = { quorum = 2, blind_round = false, \
             moves = { critic = [\"object\"] } }",
        ),
        "eng",
    )
    .expect("a room");
    let member = desk.member("critic").expect("critic is seated").clone();
    let quorum = desk.policy().quorum;
    let messages = [
        message(1, "planner", "!propose #euler12-triangle 76576500"),
        message(2, "scout", "!support #euler12-triangle ^1 It checks out."),
    ];
    let visible = messages.clone();
    let prompt = EpisodePrompt::new(&member, &desk, "Project Euler 12.", quorum, &[])
        .render(&turn("critic", tinyhivemind_hive::Phase::Commit), &visible);

    assert!(
        prompt.contains("carried `#euler12-triangle`"),
        "the seat is told which id to record:\n{prompt}"
    );
    assert!(
        prompt.contains("!commit #euler12-triangle ^N"),
        "and shown the exact line:\n{prompt}"
    );
    assert!(
        !prompt.contains("This desk gives you no marker"),
        "no seat is left without a move in the Commit phase:\n{prompt}"
    );
    assert!(
        prompt.contains("bookkeeping, not a fresh judgement"),
        "{prompt}"
    );
}

// ---------------------------------------------------------------------------
// 2. `question` and `defer` are always available
// ---------------------------------------------------------------------------

#[test]
fn an_exclusive_table_still_leaves_question_and_defer() {
    let desk = desk_of(
        &manifest_with("hive = { blind_round = false, moves = { scout = [\"evidence\"] } }"),
        "eng",
    )
    .expect("a room");
    assert!(desk.config.may("scout", "question"));
    assert!(desk.config.may("scout", "defer"));
    assert!(desk.config.may("scout", "commit"));
    assert!(!desk.config.may("scout", "propose"));

    let member = desk.member("scout").expect("scout is seated").clone();
    let quorum = desk.policy().quorum;
    let prompt = EpisodePrompt::new(&member, &desk, "Decide the rollout.", quorum, &[])
        .render(&turn("scout", tinyhivemind_hive::Phase::Deliberate), &[]);
    assert!(prompt.contains("!question  then what you need"), "{prompt}");
    assert!(
        prompt.contains("!defer #topic  then who should"),
        "{prompt}"
    );
    assert!(
        prompt.contains(
            "If you have nothing to add, reply !defer #topic naming who should act next, or \
             !question. Prose without a marker counts for nothing and costs the room a turn."
        ),
        "{prompt}"
    );
    assert!(!prompt.contains("!propose #topic"), "{prompt}");
}

#[tokio::test]
async fn a_deferring_seat_is_never_corrected_however_narrow_its_entry() {
    let log = Arc::new(MemoryLog::default());
    let trigger = open(&log).await;
    // Both narrow seats hold `support` so the desk can reach its quorum of
    // three at all — `desk_episode` declines a room whose eligible supporters
    // are fewer than its quorum. Neither entry lists `defer`, which is the
    // only thing this test is about: an ungated move is never corrected,
    // however narrow the entry that omits it.
    let manifest = manifest_with(
        "hive = { turn_budget = 3, quorum = 3, blind_round = false, \
         moves = { scout = [\"evidence\", \"support\"], \
         critic = [\"evidence\", \"support\"] } }",
    );
    let desk = desk_of(&manifest, "eng").expect("a room");
    let runner = Runner::new(&[
        ("planner", "!propose #stage Stage the rollout."),
        ("scout", "!defer #stage @planner owns the rollout plan."),
        ("critic", "!question What broke last time?"),
    ]);
    let outcome = EpisodeDriver::new(
        MemoryLog::company(),
        desk,
        Arc::clone(&log) as Arc<dyn EventLog>,
        &runner,
        "Decide the rollout.",
    )
    .run(trigger)
    .await
    .expect("the episode runs");

    assert!(outcome.violations.is_empty(), "{:?}", outcome.violations);
    let replies = log.replies("eng");
    assert!(
        replies
            .iter()
            .any(|(author, text)| author == "scout" && text.starts_with("!defer")),
        "a defer keeps its marker on the narrowest seat: {replies:?}"
    );
    assert!(
        replies
            .iter()
            .any(|(author, text)| author == "critic" && text.starts_with("!question")),
        "{replies:?}"
    );
    // Asked once each: neither move triggers a correction.
    assert_eq!(runner.prompts_for("scout").len(), 1);
    assert_eq!(runner.prompts_for("critic").len(), 1);
}

// ---------------------------------------------------------------------------
// 3. Topic-id discipline
// ---------------------------------------------------------------------------

#[test]
fn the_canonical_topic_is_derived_from_the_task() {
    // The live case: one problem, three synonyms, and support split three ways.
    assert_eq!(
        canonical_topic("Project Euler 12: Highly divisible triangular number"),
        "euler12"
    );
    assert_eq!(canonical_topic("Project Euler 145"), "euler145");
    // An explicit line wins, with or without the sigil, wherever it appears.
    assert_eq!(
        canonical_topic("Project Euler 12: …\ntopic: #euler12-triangle\n"),
        "euler12-triangle"
    );
    assert_eq!(canonical_topic("Topic: rollout\nDecide it."), "rollout");
    // An ordinary sentence still gets a short, stable id.
    assert_eq!(canonical_topic("Decide the rollout."), "decide-rollout");
    // And a line with nothing to slug falls back rather than naming nothing.
    assert_eq!(canonical_topic(""), "answer");
    assert_eq!(canonical_topic("the a of\n"), "answer");
}

#[test]
fn every_deliberating_prompt_states_the_topic_id() {
    let desk = desk_of(&manifest_with("hive = { blind_round = false }"), "eng").expect("a room");
    let member = desk.member("planner").expect("planner is seated").clone();
    let quorum = desk.policy().quorum;
    let prompt = EpisodePrompt::new(
        &member,
        &desk,
        "Project Euler 12: Highly divisible triangular number",
        quorum,
        &[],
    )
    .render(&turn("planner", tinyhivemind_hive::Phase::Deliberate), &[]);

    assert!(
        prompt.contains("Topic id for this task's answer: `#euler12`."),
        "{prompt}"
    );
    assert!(
        prompt.contains(
            "Every !propose, !support and !evidence about the answer uses exactly this id."
        ),
        "{prompt}"
    );
    assert!(prompt.contains("(`#euler12-2`)"), "{prompt}");
    assert!(
        prompt.contains("Never invent a synonym for an id already on the floor."),
        "{prompt}"
    );
    // The floor block is still there, and still lists what is on it.
    assert!(prompt.contains("No option is on the floor yet"), "{prompt}");
}

// ---------------------------------------------------------------------------
// 4. Citation discipline under `require_evidential`
// ---------------------------------------------------------------------------

#[test]
fn a_support_citing_only_a_proposal_reaches_no_evidence() {
    let messages = [
        message(1, "planner", "!propose #euler12 76576500"),
        message(2, "scout", "!evidence #euler12 ^1 The 12375th triangular."),
    ];
    let visible = messages.clone();
    assert!(
        evidential::support_misses_evidence(
            "!support #euler12 ^1 It looks right.",
            "critic",
            &visible
        ),
        "citing the proposal reaches no fact"
    );
    assert!(
        !evidential::support_misses_evidence(
            "!support #euler12 ^2 The count is checked.",
            "critic",
            &visible
        ),
        "citing the evidence does"
    );
    // Anything that is not a support is none of this check's business.
    assert!(!evidential::support_misses_evidence(
        "!object >1 ^1 Not so.",
        "critic",
        &visible
    ));
    assert_eq!(evidential::evidence_sequences(&visible), vec![2]);
}

#[tokio::test]
async fn a_support_that_reaches_no_evidence_is_re_prompted_once() {
    let log = Arc::new(MemoryLog::default());
    let trigger = open(&log).await;
    let manifest = manifest_with(
        "hive = { turn_budget = 9, quorum = 2, blind_round = false, require_evidential = true }",
    );
    let desk = desk_of(&manifest, "eng").expect("a room");
    let runner = Runner::new(&[
        ("planner", "!propose #stage Stage the rollout."),
        (
            "scout",
            "!evidence #stage ^1 The last full rollout took checkout down.",
        ),
        // Cites the proposal, which counts for nothing here…
        ("critic", "!support #stage ^2 Staging is obviously right."),
        // …and the corrected attempt cites the fact.
        (
            "critic",
            "!support #stage ^3 The outage is the reason to stage.",
        ),
        // Once quorum carries, the room records it.
        ("planner", "!commit #stage ^3 Recorded."),
        ("scout", "!commit #stage ^3 Recorded."),
        ("critic", "!commit #stage ^3 Recorded."),
    ]);
    let outcome = EpisodeDriver::new(
        MemoryLog::company(),
        desk,
        Arc::clone(&log) as Arc<dyn EventLog>,
        &runner,
        "Decide the rollout.",
    )
    .run(trigger)
    .await
    .expect("the episode runs");

    let prompts = runner.prompts_for("critic");
    assert!(
        prompts.len() >= 2,
        "critic must be re-prompted: {prompts:?}"
    );
    assert!(
        prompts[1].contains("Your `!support` reaches no `!evidence`"),
        "{}",
        prompts[1]
    );
    assert!(
        prompts[1].contains("Evidence on the floor: ^3"),
        "the correction names the sequences that would work: {}",
        prompts[1]
    );
    let replies = log.replies("eng");
    assert!(
        replies
            .iter()
            .any(|(author, text)| author == "critic" && text.starts_with("!support #stage ^3")),
        "the second attempt is journaled as-is: {replies:?}"
    );
    assert!(
        matches!(&outcome.ending, EpisodeEnding::Converged { topic, .. } if topic == "stage"),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn the_second_attempt_is_journaled_even_when_it_still_misses() {
    let log = Arc::new(MemoryLog::default());
    let trigger = open(&log).await;
    let manifest = manifest_with(
        "hive = { turn_budget = 4, quorum = 3, blind_round = false, require_evidential = true }",
    );
    let desk = desk_of(&manifest, "eng").expect("a room");
    let runner = Runner::new(&[
        ("planner", "!propose #stage Stage the rollout."),
        ("scout", "!support #stage ^2 I agree with the planner."),
        (
            "scout",
            "!support #stage ^2 I still agree with the planner.",
        ),
    ]);
    EpisodeDriver::new(
        MemoryLog::company(),
        desk,
        Arc::clone(&log) as Arc<dyn EventLog>,
        &runner,
        "Decide the rollout.",
    )
    .run(trigger)
    .await
    .expect("the episode runs");

    let prompts = runner.prompts_for("scout");
    assert!(prompts.len() >= 2, "{prompts:?}");
    // Nothing is on the floor to cite, and the correction says so rather than
    // naming an empty list.
    assert!(
        prompts[1].contains("No evidence is on the floor yet"),
        "{}",
        prompts[1]
    );
    let replies = log.replies("eng");
    assert!(
        replies.iter().any(|(author, text)| author == "scout"
            && text == "!support #stage ^2 I still agree with the planner."),
        "the host never demotes a support; the fold decides what it is worth: {replies:?}"
    );
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// One transcript row, as the projection hands it to a prompt.
fn message(sequence: u64, agent: &str, content: &str) -> tinyhivemind_hive::SessionMessage {
    tinyhivemind_hive::SessionMessage {
        sequence: tinyhivemind_hive::Sequence(sequence),
        author: tinyhivemind_hive::SessionAuthor::Agent {
            id: agent.to_owned(),
            label: agent.to_owned(),
        },
        content: content.to_owned(),
        audience: tinyhivemind_hive::aside::Audience::Desk,
        elided: None,
    }
}

#[test]
fn a_seat_reads_its_other_conversations_and_is_told_they_are_not_the_floor() {
    let desk = desk_of(
        &manifest_with("hive = { quorum = 2, blind_round = false }"),
        "eng",
    )
    .expect("a room");
    let member = desk.member("critic").expect("critic is seated").clone();
    let quorum = desk.policy().quorum;
    let visible = [message(
        1,
        "planner",
        "!propose #ship-friday Ship on Friday.",
    )];
    // What the seat is holding from the desk it *also* sits on, plus its own
    // direct line — each already projected for it by the driver.
    let elsewhere = vec![
        (
            "#returns (Returns)".to_owned(),
            vec![message(
                7,
                "refunds",
                "The W12 exchange is past its window.",
            )],
        ),
        (
            "Your direct line (@critic)".to_owned(),
            vec![message(3, "critic", "Noted, I will raise it.")],
        ),
    ];
    let prompt = EpisodePrompt::new(&member, &desk, "Decide the rollout.", quorum, &[])
        .with_elsewhere(&elsewhere)
        .render(
            &turn("critic", tinyhivemind_hive::Phase::Deliberate),
            &visible,
        );

    assert!(
        prompt.contains("The W12 exchange is past its window."),
        "the seat reads the other desk it sits on:\n{prompt}"
    );
    assert!(
        prompt.contains("#returns (Returns)") && prompt.contains("Your direct line (@critic)"),
        "each conversation is labelled and kept separate, so two `[7]`s cannot be \
         confused for one row:\n{prompt}"
    );
    assert!(
        prompt.contains("not on this desk's floor"),
        "and is told these rows settle nothing here — information crosses, support \
         does not:\n{prompt}"
    );
    // Its own row elsewhere is still its own: the `(you)` marker is what stops a
    // seat reading its own words back as somebody else's.
    assert!(
        prompt.contains("critic (you): Noted, I will raise it."),
        "a seat's own row on its own line stays attributed to it:\n{prompt}"
    );
}

#[test]
fn a_seat_with_nowhere_else_renders_no_elsewhere_block() {
    let desk = desk_of(
        &manifest_with("hive = { quorum = 2, blind_round = false }"),
        "eng",
    )
    .expect("a room");
    let member = desk.member("critic").expect("critic is seated").clone();
    let quorum = desk.policy().quorum;
    let visible = [message(
        1,
        "planner",
        "!propose #ship-friday Ship on Friday.",
    )];

    let bare = EpisodePrompt::new(&member, &desk, "Decide the rollout.", quorum, &[]).render(
        &turn("critic", tinyhivemind_hive::Phase::Deliberate),
        &visible,
    );
    // An entry that projected to nothing is not a heading with nothing under it.
    let empty = vec![("#returns (Returns)".to_owned(), Vec::new())];
    let with_empty = EpisodePrompt::new(&member, &desk, "Decide the rollout.", quorum, &[])
        .with_elsewhere(&empty)
        .render(
            &turn("critic", tinyhivemind_hive::Phase::Deliberate),
            &visible,
        );

    assert!(
        !bare.contains("Elsewhere you are part of"),
        "a seat with nowhere else to be pays nothing for the feature:\n{bare}"
    );
    assert_eq!(
        bare, with_empty,
        "and neither does one whose other conversations are all empty"
    );
}

/// One authorized turn, for rendering a prompt without driving an episode.
fn turn(agent: &str, phase: tinyhivemind_hive::Phase) -> tinyhivemind_hive::HiveTurn {
    tinyhivemind_hive::HiveTurn {
        agent_id: agent.to_owned(),
        phase,
        visibility: tinyhivemind_hive::Visibility::Full,
        reason: tinyhivemind_hive::BidReason::Addressed,
        next_state: tinyhivemind_hive::EpisodeState::opened(
            tinyhivemind_hive::Conversation {
                desk_id: "eng".to_owned(),
                desk_name: "Engineering".to_owned(),
                thread_root: None,
            },
            tinyhivemind_hive::Sequence(0),
        ),
    }
}
