//! Defect B-072: the host composes the resume claim in two places, and only one
//! of them branched on the blocker/gated distinction it already had.

use super::resume_claim;
use crate::ports::WorkflowBlockedNode;
use crate::workflows::caps::{ParkedCalls, blocked_diagnosis};
use crate::workflows::runner::blocked_notice;

/// The claim a gated call makes, and the claim a blocker makes, as the
/// fragments a reader would pick out of either composer's paragraph.
const GATED: &str = "continues this run automatically";
const BLOCKER: &str = "re-enters this step";
const MIXED: &str = "continue this run when approved";

fn parked(waiting: usize, blockers: usize) -> ParkedCalls {
    ParkedCalls {
        tools: vec!["escalate_to_human".to_string()],
        approval_ids: (0..waiting).map(|n| format!("a-{n}")).collect(),
        unparkable: 0,
        blockers,
    }
}

fn blocked(waiting: usize, blockers: usize) -> WorkflowBlockedNode {
    WorkflowBlockedNode {
        node_id: "research".to_string(),
        tools: vec!["escalate_to_human".to_string()],
        approval_ids: (0..waiting).map(|n| format!("a-{n}")).collect(),
        unparkable: 0,
        stranded: 0,
        blockers,
    }
}

/// The whole defect, as one property: for every shape of park, the two host
/// composers say the same thing about what deciding it does.
///
/// Before the fix this failed at `(1, 1)` — `blocked_diagnosis` said "it does
/// not restart this run" and `blocked_notice` said "this run continues on its
/// own", and the console rendered both in one panel.
#[test]
fn both_host_composers_make_the_same_claim() {
    for (waiting, blockers) in [(0, 0), (1, 0), (1, 1), (2, 0), (2, 1), (2, 2), (3, 3)] {
        let diagnosis =
            blocked_diagnosis(Some("research"), "researcher", &parked(waiting, blockers));
        let notice = blocked_notice(&blocked(waiting, blockers));
        for claim in [GATED, BLOCKER, MIXED] {
            assert_eq!(
                diagnosis.contains(claim),
                notice.contains(claim),
                "waiting={waiting} blockers={blockers}: the Observatory and the run notice \
                 disagree about {claim:?}\n  diagnosis: {diagnosis}\n  notice: {notice}",
            );
        }
    }
}

/// A question the agent raised must never be described with the gated-call's
/// own wording — the sentence B-013 was filed about, now asserted against the
/// composer that kept making it. It really does re-enter the node now (issues
/// #1863, #2005), just by a different mechanism than a gated call's approval.
#[test]
fn a_notice_for_a_question_names_re_entry_not_automatic_continuation() {
    let notice = blocked_notice(&blocked(1, 1));
    assert!(
        notice.contains(BLOCKER),
        "a blocker's notice must say the node re-enters: {notice}",
    );
    assert!(
        !notice.contains("continues on its own"),
        "the pre-B-072 claim is still shipped: {notice}",
    );
    assert!(
        !notice.contains(GATED),
        "a blocker's notice must not make the gated-call claim: {notice}",
    );
}

/// The gated case is unchanged behaviour, stated so the fix cannot be "delete
/// the tail": approving a gated call really does continue the run.
#[test]
fn a_notice_for_a_gated_call_still_promises_the_resume() {
    let notice = blocked_notice(&blocked(1, 0));
    assert!(notice.contains(GATED), "{notice}");
    assert!(!notice.contains(BLOCKER), "{notice}");
}

/// A node that parked one of each gets the sentence that covers both, rather
/// than either single-kind claim.
#[test]
fn a_notice_for_a_mixed_park_claims_neither_alone() {
    let notice = blocked_notice(&blocked(2, 1));
    assert!(notice.contains(MIXED), "{notice}");
    assert!(!notice.contains(GATED), "{notice}");
}

/// Nothing parked means no promise at all — the `unparkable`-only case, where
/// the node's own sentence already says nobody will be asked.
#[test]
fn nothing_parked_makes_no_claim_about_deciding() {
    assert_eq!(resume_claim(0, 0), None);
    let notice = blocked_notice(&WorkflowBlockedNode {
        node_id: "research".to_string(),
        tools: vec!["publish_artifact".to_string()],
        approval_ids: Vec::new(),
        unparkable: 2,
        stranded: 0,
        blockers: 0,
    });
    for claim in [GATED, BLOCKER, MIXED] {
        assert!(!notice.contains(claim), "{notice}");
    }
    assert!(
        notice.contains("could not be queued for approval at all"),
        "{notice}"
    );
}

/// The receipt half of the notice survives the extraction: it still says what
/// this run parked, which is the fact that stays true after the queue moves.
#[test]
fn the_notice_is_still_a_receipt_of_what_was_parked() {
    assert!(
        blocked_notice(&blocked(1, 1)).contains("It parked 1 approval."),
        "{}",
        blocked_notice(&blocked(1, 1))
    );
    assert!(
        blocked_notice(&blocked(2, 0)).contains("It parked 2 approvals."),
        "{}",
        blocked_notice(&blocked(2, 0))
    );
}
