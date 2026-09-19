//! Tests for [`super::completion`]: explicit termination and reopening.
//!
//! The cases that matter are the ones quorum cannot express — a room of one —
//! and the one a handoff chain depends on: a member that reported finished and
//! is then given more work is live again, alone.

use super::{
    COMPLETE_MARKER, assigned, completed, opened, pending, reply_reports_completion,
    reports_completion, step,
};
use tinyhivemind::{Conversation, Sequence};
use tinyhivemind_hive::CompletionStep;

fn conversation() -> Conversation {
    Conversation {
        desk_id: "eng".to_owned(),
        desk_name: "Engineering".to_owned(),
        thread_root: None,
    }
}

fn room(members: &[&str]) -> tinyhivemind_hive::CompletionEpisodeState {
    let assigned: Vec<String> = members.iter().map(|id| (*id).to_owned()).collect();
    // Roster and assignment are the same list here: these tests are about
    // what happens to members who DO owe a report.
    opened(conversation(), Sequence(10), &assigned, &assigned).expect("a well-formed opening")
}

#[test]
fn a_bare_marker_reports_and_a_mention_of_it_does_not() {
    assert!(reports_completion(COMPLETE_MARKER));
    assert!(reports_completion("!complete the residue holds at every N"));
    assert!(reports_completion("   !complete"));
    // Prose *about* completing is not a report of it.
    assert!(!reports_completion("I will !complete once checker signs"));
    // A different marker that merely starts with the same letters.
    assert!(!reports_completion("!completed"));
}

#[test]
fn a_report_may_follow_what_the_member_found() {
    let reply = "The optimized solver matches direct interpolation for N=10..20.\n\
                 !complete coefficient confirmed, nothing left open.";
    assert!(reply_reports_completion(reply));
}

#[test]
fn a_room_of_one_is_well_defined_where_quorum_refuses_it() {
    // `HiveConfig::deliberates` floors a quorum room at two members and
    // `desk_episode` additionally needs a reachable quorum. Completion needs
    // neither: one assignee, one report, done.
    let state = room(&["checker"]);
    assert_eq!(pending(&state), vec!["checker".to_owned()]);

    let state = completed(&state, "checker", Sequence(11)).expect("a valid report");
    assert!(matches!(step(&state), CompletionStep::Complete { .. }));
    assert!(pending(&state).is_empty());
}

#[test]
fn an_episode_waits_for_every_assigned_member() {
    let state = room(&["solver", "checker"]);
    let state = completed(&state, "solver", Sequence(11)).expect("a valid report");

    // One report is not the room's report.
    assert_eq!(pending(&state), vec!["checker".to_owned()]);
    assert!(matches!(step(&state), CompletionStep::Active { .. }));

    let state = completed(&state, "checker", Sequence(12)).expect("a valid report");
    assert!(pending(&state).is_empty());
}

#[test]
fn a_later_assignment_reopens_exactly_its_recipients() {
    let state = room(&["solver", "checker"]);
    let state = completed(&state, "solver", Sequence(11)).expect("valid");
    let state = completed(&state, "checker", Sequence(12)).expect("valid");
    assert!(pending(&state).is_empty(), "the room had ended");

    // Checker found a fault and handed it back. Solver is live again; checker,
    // which did not receive the assignment, stays finished.
    let state = assigned(&state, &["solver".to_owned()], Sequence(13)).expect("valid");

    assert_eq!(
        pending(&state),
        vec!["solver".to_owned()],
        "only the recipient reopens"
    );
}

#[test]
fn reporting_for_somebody_who_was_never_assigned_is_refused() {
    let state = room(&["solver"]);
    let error = completed(&state, "checker", Sequence(11))
        .expect_err("checker is not a participant of this episode");
    assert!(
        error.to_string().contains("hive completion"),
        "the failure should name this seam: {error}"
    );
}

#[test]
fn an_opening_that_assigns_nobody_is_refused() {
    let error = opened(conversation(), Sequence(10), &[], &[])
        .expect_err("an episode with no assignee can never end");
    assert!(
        error.to_string().contains("hive completion episode"),
        "the failure should name this seam: {error}"
    );
}

#[test]
fn a_broadcast_carries_the_work_and_nothing_else() {
    assert_eq!(
        super::broadcast_body("!broadcast the degree-9 identity needs a proof"),
        Some("the degree-9 identity needs a proof")
    );
    // A bare marker hands on nothing, so there is nothing to route.
    assert_eq!(super::broadcast_body("!broadcast"), None);
    assert_eq!(super::broadcast_body("!broadcast   "), None);
    // Prose about broadcasting is not a broadcast.
    assert_eq!(super::broadcast_body("I will !broadcast this later"), None);
}

#[test]
fn only_the_first_broadcast_in_a_reply_is_taken() {
    // A turn is one move. Two broadcasts would be two routing calls — two extra
    // model calls — charged to a single turn.
    let reply = "Solver ran it clean.\n\
                 !broadcast checker should verify the k=7 case\n\
                 !broadcast and theory should look at the proof";
    assert_eq!(
        super::reply_broadcast(reply),
        Some("checker should verify the k=7 case")
    );
}

#[test]
fn a_reply_may_hand_on_and_then_report_finished() {
    let reply = "Found the fault.\n\
                 !broadcast solver should re-run with exact arithmetic\n\
                 !complete my audit is done";
    assert_eq!(
        super::reply_broadcast(reply),
        Some("solver should re-run with exact arithmetic")
    );
    assert!(super::reply_reports_completion(reply));
}

#[test]
fn a_broadcast_reads_as_the_work_without_its_marker() {
    assert_eq!(
        super::readable("!broadcast motion tokens need to land in the component spec"),
        Some("motion tokens need to land in the component spec".to_owned())
    );
}

#[test]
fn a_completion_reads_as_its_result_without_its_marker() {
    assert_eq!(
        super::readable("!complete contrast passes AA at every variant"),
        Some("contrast passes AA at every variant".to_owned())
    );
}

#[test]
fn a_bare_marker_still_surfaces_as_a_sentence() {
    // Observed live: a seat wrote its work as one reply and `!broadcast` alone
    // as a second, which rendered to nothing. A row that disappears leaves an
    // operator looking at a turn that appears not to have happened, so a bare
    // marker is rendered rather than dropped — the marker is plumbing, the fact
    // that it was written is not.
    assert_eq!(
        super::readable("!complete"),
        Some("Reported this assignment finished.".to_owned())
    );
    assert_eq!(
        super::readable("!broadcast"),
        Some("Handed this on, but carried no detail with it.".to_owned())
    );
    assert!(super::is_completion_line("!complete"));
}

#[test]
fn a_bare_marker_earns_a_correction_the_seat_can_act_on() {
    let broadcast = super::bare_marker_correction("!broadcast").expect("a bare broadcast is bare");
    assert!(broadcast.contains("carried nothing"));
    assert!(broadcast.contains("ONE line"));

    let complete = super::bare_marker_correction("!complete").expect("a bare complete is bare");
    assert!(complete.contains("no result"));

    // A marker that carries its work is not corrected.
    assert!(super::bare_marker_correction("!broadcast verify the k=7 case").is_none());
    assert!(super::bare_marker_correction("!complete residue holds").is_none());
    assert!(super::bare_marker_correction("ordinary prose").is_none());
}

#[test]
fn an_ordinary_line_is_not_completion_grammar() {
    assert_eq!(super::readable("the palette passes AA"), None);
    assert!(!super::is_completion_line("the palette passes AA"));
    // Prose mentioning a marker mid-sentence is not a marker line.
    assert!(!super::is_completion_line(
        "I will !complete once checker signs"
    ));
}

#[test]
fn only_the_assigned_owe_a_report() {
    // The bug this fixes, seen live: opening every seat pending meant a
    // six-member desk could not end until all six spoke, and a member the work
    // never reached wrote a bare `!complete` because it had nothing to report.
    let roster: Vec<String> = ["lead", "solver", "checker"]
        .iter()
        .map(|id| (*id).to_owned())
        .collect();
    let state = opened(
        conversation(),
        Sequence(10),
        &roster,
        &["solver".to_owned()],
    )
    .expect("a well-formed opening");

    assert_eq!(
        pending(&state),
        vec!["solver".to_owned()],
        "only the routed member is waited on"
    );

    let state = completed(&state, "solver", Sequence(11)).expect("a valid report");
    assert!(
        pending(&state).is_empty(),
        "and the room ends without lead or checker saying anything"
    );
}

#[test]
fn an_unassigned_seat_can_still_be_handed_work() {
    // The roster is who MAY be assigned. A routed handoff reaches a seat that
    // started finished.
    let roster: Vec<String> = ["lead", "checker"]
        .iter()
        .map(|id| (*id).to_owned())
        .collect();
    let state = opened(conversation(), Sequence(10), &roster, &["lead".to_owned()]).expect("valid");

    let state = assigned(&state, &["checker".to_owned()], Sequence(11)).expect("valid");
    assert!(pending(&state).contains(&"checker".to_owned()));
}
