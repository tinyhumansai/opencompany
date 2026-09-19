//! Tests for [`super::schedule`]: the two ways a room ends.
//!
//! [`Quorum`](super::Quorum) is covered by the episode suite that already drives
//! it end to end. What is new here is [`Completion`](super::Completion) — the
//! queue, reopening after an assignment, and the turn cap — none of which the
//! existing suite reaches.

use super::{Completion, Scheduled, Scheduler};
use tinyhivemind_hive::{
    Conversation, Sequence, SessionAuthor, SessionMessage, aside::Audience, desk::DeskSet,
    roster::Roster,
};

fn conversation() -> Conversation {
    Conversation {
        desk_id: "eng".to_owned(),
        desk_name: "Engineering".to_owned(),
        thread_root: None,
    }
}

fn opened(members: &[&str]) -> Completion {
    let assigned: Vec<String> = members.iter().map(|id| (*id).to_owned()).collect();
    Completion::new(
        super::super::completion::opened(conversation(), Sequence(10), &assigned, &assigned)
            .expect("a well-formed opening"),
        25,
    )
}

/// One desk row by `agent`, carrying `body`.
fn said(sequence: u64, agent: &str, body: &str) -> SessionMessage {
    SessionMessage {
        sequence: Sequence(sequence),
        author: SessionAuthor::Agent {
            id: agent.to_owned(),
            label: agent.to_owned(),
        },
        content: body.to_owned(),
        audience: Audience::Desk,
        elided: None,
    }
}

/// The scheduler needs a roster and desks to satisfy the trait; completion
/// consults neither, so empty snapshots are honest rather than a stub.
fn ask(scheduler: &mut Completion, transcript: &[SessionMessage]) -> Scheduled {
    let members = Vec::new();
    let desks = Vec::new();
    let retired = Vec::new();
    let roster = Roster::new(&members, &[], &retired);
    let desk_set = DeskSet::new(&desks, &[], &[], &[], &retired);
    scheduler
        .next(transcript, &roster, &desk_set)
        .expect("completion consults no snapshot, so it cannot reject one")
}

#[test]
fn a_room_of_one_is_scheduled_where_quorum_would_refuse_it() {
    let mut scheduler = opened(&["checker"]);
    match ask(&mut scheduler, &[]) {
        Scheduled::Round(turns) => {
            assert_eq!(turns.len(), 1, "one turn at a time");
            assert_eq!(turns[0].agent_id, "checker");
        }
        Scheduled::Ended(ending) => panic!("a pending member must be scheduled, got {ending:?}"),
    }
}

#[test]
fn a_report_in_the_transcript_ends_the_room() {
    let mut scheduler = opened(&["checker"]);
    let transcript = vec![said(11, "checker", "!complete the residue holds")];

    match ask(&mut scheduler, &transcript) {
        Scheduled::Ended(super::EpisodeEnding::Completed { completed }) => {
            assert_eq!(completed, vec!["checker".to_owned()]);
        }
        other => panic!("an explicit report must end the room, got {other:?}"),
    }
}

#[test]
fn one_report_does_not_end_a_room_of_two() {
    let mut scheduler = opened(&["solver", "checker"]);
    let transcript = vec![said(11, "solver", "!complete solver is done")];

    match ask(&mut scheduler, &transcript) {
        Scheduled::Round(turns) => assert_eq!(turns[0].agent_id, "checker"),
        Scheduled::Ended(ending) => panic!("checker has not reported yet, got {ending:?}"),
    }
}

#[test]
fn a_row_from_somebody_this_episode_never_assigned_is_not_a_report() {
    // A shared desk carries other people's rows. A stranger writing the marker
    // must not end an episode they were never part of.
    let mut scheduler = opened(&["checker"]);
    let transcript = vec![said(11, "marketing", "!complete unrelated work")];

    match ask(&mut scheduler, &transcript) {
        Scheduled::Round(turns) => assert_eq!(turns[0].agent_id, "checker"),
        Scheduled::Ended(ending) => {
            panic!("a stranger's row must not end the room, got {ending:?}")
        }
    }
}

#[test]
fn the_turn_cap_stops_a_room_that_never_reports() {
    let mut scheduler = Completion::new(
        super::super::completion::opened(
            conversation(),
            Sequence(10),
            &["checker".to_owned()],
            &["checker".to_owned()],
        )
        .expect("valid"),
        2,
    );

    // Two turns are authorized, and nothing is ever reported.
    assert!(matches!(ask(&mut scheduler, &[]), Scheduled::Round(_)));
    assert!(matches!(ask(&mut scheduler, &[]), Scheduled::Round(_)));

    match ask(&mut scheduler, &[]) {
        Scheduled::Ended(super::EpisodeEnding::Exhausted) => {}
        other => panic!("the cap must stop a silent room, got {other:?}"),
    }
}

#[test]
fn a_reopened_member_is_scheduled_again() {
    let mut scheduler = opened(&["solver"]);

    // Solver reports, so the room would end here.
    let mut transcript = vec![said(11, "solver", "!complete first pass done")];
    assert!(matches!(
        ask(&mut scheduler, &transcript),
        Scheduled::Ended(_)
    ));

    // Checker hands work back. The queue emptied when solver was popped, so
    // without the re-queue this member would be pending with nothing to run it.
    scheduler.assign(&["solver".to_owned()], Sequence(12));

    transcript.push(said(12, "checker", "found a fault, back to you"));
    match ask(&mut scheduler, &transcript) {
        Scheduled::Round(turns) => assert_eq!(turns[0].agent_id, "solver"),
        Scheduled::Ended(ending) => {
            panic!("an assignment must reopen its recipient, got {ending:?}")
        }
    }
}
