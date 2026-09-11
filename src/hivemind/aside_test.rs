//! What a private aside does to a real episode.
//!
//! [`super::aside`]'s own tests pin the fold — the party key, the budget, the
//! settlement debt — against synthetic transcripts. These drive the whole
//! [`EpisodeDriver`] and assert the two properties that only show up end to
//! end: that an authorized aside is journaled with a narrower audience, and
//! that it is worth **nothing** to the room's counting.
//!
//! See `docs/spec/runtime/hivemind-asides.md`.

use std::sync::Arc;

use super::test::{MemoryLog, ScriptedRunner, desk_of, seed_desk};
use super::*;
use crate::ports::events::EventLog;

/// Three seats on one desk with asides on, at the library's own default bounds.
fn aside_manifest() -> String {
    "[company]\nname = \"Acme\"\n\
     [[agent]]\nid = \"planner\"\nrole = \"Planner\"\n\
     [[agent]]\nid = \"scout\"\nrole = \"Scout\"\n\
     [[agent]]\nid = \"critic\"\nrole = \"Critic\"\n\
     [[group_chat]]\nid = \"eng\"\nname = \"Engineering\"\n\
     description = \"Ship the rollout\"\n\
     members = [\"planner\", \"scout\", \"critic\"]\n\
     hive = { enabled = true, aside = { enabled = true } }\n"
        .to_string()
}

/// The same desk with the block absent entirely — the default every other
/// bundle in this repo runs under.
fn no_aside_manifest() -> String {
    "[company]\nname = \"Acme\"\n\
     [[agent]]\nid = \"planner\"\nrole = \"Planner\"\n\
     [[agent]]\nid = \"scout\"\nrole = \"Scout\"\n\
     [[agent]]\nid = \"critic\"\nrole = \"Critic\"\n\
     [[group_chat]]\nid = \"eng\"\nname = \"Engineering\"\n\
     description = \"Ship the rollout\"\n\
     members = [\"planner\", \"scout\", \"critic\"]\n"
        .to_string()
}

async fn run(manifest: &str, script: &[(&str, &str)]) -> (Arc<MemoryLog>, EpisodeOutcome) {
    let log = Arc::new(MemoryLog::default());
    let trigger = seed_desk(&log).await;
    let desk = desk_of(manifest, "eng").expect("a room");
    let runner = ScriptedRunner::new(script);
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
    (log, outcome)
}

/// Every private row's audience, in journal order.
///
/// Keyed on the **audience** rather than on the text: since ADR 0011 an aside
/// is a second row riding alongside a turn, and a refused one is dropped
/// entirely, so "which rows are private" is a question about what was stored
/// and not about what somebody wrote.
fn aside_audiences(log: &MemoryLog) -> Vec<Vec<String>> {
    log.addressed_replies("eng")
        .into_iter()
        .filter(|(_, _, audience)| !audience.is_empty())
        .map(|(_, _, audience)| audience)
        .collect()
}

/// Every desk-visible row's text, in journal order.
fn desk_lines(log: &MemoryLog) -> Vec<String> {
    log.addressed_replies("eng")
        .into_iter()
        .filter(|(_, _, audience)| audience.is_empty())
        .map(|(_, text, _)| text)
        .collect()
}

/// The whole path, in one test: a room writes an aside, and an operator reading
/// that desk gets it as a collapsed conversation on the move it rode under.
///
/// The two halves of this were already covered separately — the tests below
/// prove an aside is journaled with an audience, and `chat_history::test` proves
/// the fold groups such rows. Neither noticed that the projection had no way to
/// tell an aside from an ordinary reply, so the row reached the console verbatim
/// with `!aside @scout` still in its body. This is the seam, and it is the one
/// place the defect could hide.
#[tokio::test]
async fn an_aside_reaches_the_operator_as_a_collapsed_conversation() {
    use crate::server::chat_history::{MessageView, fold_asides};

    let (log, _) = run(
        &aside_manifest(),
        &[
            (
                "planner",
                "!propose #stage Stage the rollout.\n!aside @scout is the canary quota still 5%?",
            ),
            ("scout", "!support #stage ^3 Agreed, and it is reversible."),
            (
                "critic",
                "!support #stage ^3 Staging bounds the blast radius.",
            ),
        ],
    )
    .await;

    // Build the operator's view the way the projection does: every journaled row
    // on this desk, carrying whatever audience it was stored under.
    let mut messages: Vec<MessageView> = log
        .addressed_replies("eng")
        .into_iter()
        .enumerate()
        .map(|(index, (author, text, audience))| {
            MessageView::for_test(&index.to_string(), &author, &text, audience)
        })
        .collect();
    let before = messages.len();
    fold_asides(&mut messages);

    assert_eq!(
        messages.len(),
        before - 1,
        "the aside no longer stands as a row of its own: {messages:#?}"
    );
    let folded: Vec<_> = messages
        .iter()
        .filter_map(|m| m.aside_conversation.as_ref().map(|a| (&m.author, a)))
        .collect();
    assert_eq!(folded.len(), 1, "exactly one collapsed conversation");
    let (anchor_author, conversation) = folded[0];
    assert_eq!(
        anchor_author, "planner",
        "it hangs on its own author's move"
    );
    assert_eq!(
        conversation.members,
        vec!["planner".to_owned(), "scout".to_owned()],
        "author first, then who was addressed"
    );
    assert_eq!(conversation.lines.len(), 1);
    assert_eq!(
        conversation.lines[0].text, "is the canary quota still 5%?",
        "and the grammar never reaches the operator"
    );
}

#[tokio::test]
async fn an_authorized_aside_is_journaled_to_its_addressee_only() {
    let (log, _) = run(
        &aside_manifest(),
        &[
            (
                "planner",
                "!propose #stage Stage the rollout.\n!aside @scout is the checkout metric yours?",
            ),
            ("scout", "!propose #ship Ship it all at once."),
            ("critic", "!propose #stage Stage it."),
        ],
    )
    .await;

    assert_eq!(
        aside_audiences(&log),
        vec![vec!["scout".to_owned()]],
        "the addressee, and not the author, is what the row carries",
    );
}

/// **An aside rides alongside the turn that authored it** (ADR 0011).
///
/// One authorized turn produces the member's ordinary desk-visible
/// contribution *and* the private row — two rows, one turn. Under the old rule
/// the aside *was* the turn, and a live six-day run of
/// `companies/vending_machine_co` used the move zero times: asking a peer meant
/// not depositing, not objecting and not refuting, while the rest of the room
/// went on accumulating support for the option the member had stepped away to
/// ask about.
#[tokio::test]
async fn an_aside_costs_no_turn_and_the_desk_still_gets_the_move() {
    let (log, outcome) = run(
        &aside_manifest(),
        &[
            (
                "planner",
                "!propose #stage Stage the rollout.\n!aside @scout is the checkout metric yours?",
            ),
            ("scout", "!propose #ship Ship it all at once."),
            ("critic", "!propose #stage Stage it."),
        ],
    )
    .await;

    // The room got the proposal, not a member that went quiet to ask a question.
    assert!(
        desk_lines(&log)
            .iter()
            .any(|line| line.starts_with("!propose #stage Stage the rollout.")),
        "{:?}",
        desk_lines(&log),
    );
    assert_eq!(
        aside_audiences(&log).len(),
        1,
        "and the private row rode with it"
    );
    // Turns are counted, rows are not: the aside must not have been charged.
    assert!(outcome.turns >= 1);
    let rows = log.addressed_replies("eng").len();
    assert!(
        rows > outcome.turns as usize,
        "an aside adds a row without adding a turn: {rows} rows, {} turns",
        outcome.turns,
    );
}

/// Every other row on the desk stays desk-visible. An aside must not be
/// contagious: the mechanism is one row at a time, not a mode the desk enters.
#[tokio::test]
async fn only_the_aside_row_is_narrowed() {
    let (log, _) = run(
        &aside_manifest(),
        &[
            (
                "planner",
                "!propose #stage Stage it.\n!aside @scout is the checkout metric yours?",
            ),
            ("scout", "!propose #ship Ship it all at once."),
            ("critic", "!propose #stage Stage it."),
        ],
    )
    .await;

    for (author, text, audience) in log.addressed_replies("eng") {
        if text.starts_with("!aside") {
            continue;
        }
        assert!(
            audience.is_empty(),
            "`{author}` wrote a narrowed row it did not ask to narrow: {text}",
        );
    }
}

/// The rule the whole mechanism rests on. A `!support` written privately adds
/// no supporter, so a room that could otherwise carry an option does not.
///
/// The desk's quorum is two. The script deposits evidence, then a private
/// support and a public one — two supports in the transcript, only one of which
/// the fold may count — so an episode that converged here would be one that had
/// counted a vote nobody in the room could read.
#[tokio::test]
async fn a_support_written_inside_an_aside_carries_nothing() {
    let script: &[(&str, &str)] = &[
        (
            "planner",
            "!propose #stage Stage the rollout behind a flag.",
        ),
        (
            "critic",
            "!evidence #stage ^3 The last full rollout broke checkout.",
        ),
        // The scout's desk line says nothing that counts; its support is
        // private, and privacy is exactly what makes it worth nothing.
        (
            "scout",
            "!question does anybody hold this?\n!aside @planner !support #stage ^4 I am with you.",
        ),
        ("critic", "!question does anybody else hold this?"),
    ];
    let (private, private_outcome) = run(&aside_manifest(), script).await;
    assert_eq!(
        aside_audiences(&private).len(),
        1,
        "the fixture only means anything if the aside was authorized",
    );
    assert!(
        !matches!(private_outcome.ending, EpisodeEnding::Converged { .. }),
        "a private support carried the room: {private_outcome:?}",
    );
}

/// A line naming a peer who is not on this desk authorizes nothing, and the row
/// is **dropped** rather than published to the desk.
///
/// Publishing it would put a second desk-visible contribution on one turn,
/// which is the one thing a turn may not produce — and the member has already
/// said its piece in its ordinary line.
#[tokio::test]
async fn an_aside_naming_a_stranger_is_dropped() {
    let (log, _) = run(
        &aside_manifest(),
        &[
            (
                "planner",
                "!propose #stage Stage it.\n!aside @auditor can you check this?",
            ),
            ("scout", "!propose #ship Ship it."),
            ("critic", "!propose #stage Stage it."),
        ],
    )
    .await;

    assert!(
        aside_audiences(&log).is_empty(),
        "`auditor` is on no desk here, so nothing should have been made private",
    );
    assert!(
        !desk_lines(&log)
            .iter()
            .any(|line| line.contains("@auditor")),
        "a refused aside is dropped, not published: {:?}",
        desk_lines(&log),
    );
    assert!(
        desk_lines(&log)
            .iter()
            .any(|line| line.starts_with("!propose #stage Stage it.")),
        "the turn's own contribution still reaches the room",
    );
}

/// `must_surface` is the bound that binds first: a pair that has not paid its
/// last aside back to the room cannot open another, and the refused row is
/// dropped.
#[tokio::test]
async fn a_pair_that_owes_the_room_a_settlement_cannot_open_another_aside() {
    let (log, _) = run(
        &aside_manifest(),
        &[
            (
                "planner",
                "!propose #stage Stage it.\n!aside @scout first question",
            ),
            (
                "planner",
                "!support #stage ^3 Still staging.\n!aside @scout second, still owing",
            ),
            ("scout", "!propose #ship Ship it."),
            ("critic", "!propose #stage Stage it."),
        ],
    )
    .await;

    assert_eq!(
        aside_audiences(&log).len(),
        1,
        "the first opens the aside; the second is owed a settlement and is dropped",
    );
}

/// And a `!surface` discharges the debt, so the pair may open another. This is
/// the half that makes `must_surface` a protocol rather than a one-shot limit.
#[tokio::test]
async fn a_settlement_lets_the_pair_open_another_aside() {
    let (log, _) = run(
        &aside_manifest(),
        &[
            (
                "planner",
                "!propose #stage Stage it.\n!aside @scout first question",
            ),
            ("planner", "!surface scout confirms the metric is theirs"),
            (
                "planner",
                "!support #stage ^3 Confirmed.\n!aside @scout second question",
            ),
            ("scout", "!propose #ship Ship it."),
            ("critic", "!propose #stage Stage it."),
        ],
    )
    .await;

    assert_eq!(
        aside_audiences(&log).len(),
        2,
        "the surface settled the first, so the second is authorized again",
    );
}

/// With `must_surface` off, `max_messages` is what stops a pair.
#[tokio::test]
async fn a_pair_that_spends_max_messages_is_dropped() {
    let manifest = "[company]\nname = \"Acme\"\n\
         [[agent]]\nid = \"planner\"\nrole = \"Planner\"\n\
         [[agent]]\nid = \"scout\"\nrole = \"Scout\"\n\
         [[agent]]\nid = \"critic\"\nrole = \"Critic\"\n\
         [[group_chat]]\nid = \"eng\"\nname = \"Engineering\"\n\
         description = \"Ship the rollout\"\n\
         members = [\"planner\", \"scout\", \"critic\"]\n\
         hive = { enabled = true, aside = { enabled = true, max_messages = 2, must_surface = false } }\n";

    let (log, _) = run(
        manifest,
        &[
            ("planner", "!propose #stage Stage it.\n!aside @scout first"),
            (
                "planner",
                "!support #stage ^3 Yes.\n!aside @scout second, last within budget",
            ),
            (
                "planner",
                "!support #stage ^3 Still yes.\n!aside @scout third, over budget",
            ),
            ("scout", "!propose #ship Ship it."),
            ("critic", "!propose #stage Stage it."),
        ],
    )
    .await;

    assert_eq!(
        aside_audiences(&log).len(),
        2,
        "two are within budget and the third is dropped",
    );
}

/// A desk that never enabled asides behaves byte-for-byte as it did before the
/// mechanism existed: the marker is just text, and every row is desk-visible.
#[tokio::test]
async fn a_desk_that_did_not_opt_in_narrows_nothing() {
    let (log, _) = run(
        &no_aside_manifest(),
        &[
            ("planner", "!aside @scout is the checkout metric yours?"),
            ("scout", "!propose #ship Ship it."),
            ("critic", "!propose #stage Stage it."),
        ],
    )
    .await;

    assert!(
        log.addressed_replies("eng")
            .iter()
            .all(|(_, _, audience)| audience.is_empty()),
        "an opted-out desk journaled a narrowed row",
    );
}

/// The two markers are taught only where they can be used. A grammar is a fixed
/// cost paid in every agent's system text on every turn, and teaching a move
/// nobody may make spends that budget for nothing.
#[tokio::test]
async fn the_grammar_is_taught_only_to_a_desk_that_enabled_it() {
    for (manifest, expected) in [(aside_manifest(), true), (no_aside_manifest(), false)] {
        let log = Arc::new(MemoryLog::default());
        let trigger = seed_desk(&log).await;
        let desk = desk_of(&manifest, "eng").expect("a room");
        let runner = ScriptedRunner::new(&[("planner", "!propose #stage Stage it.")]);
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

        let taught = runner
            .asked()
            .iter()
            .any(|(_, prompt)| prompt.contains(ASIDE_MARKER) && prompt.contains(SURFACE_MARKER));
        assert_eq!(taught, expected, "aside grammar taught={taught}");
    }
}
