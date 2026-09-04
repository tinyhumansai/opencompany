//! What deciding a parked run's cards actually does — one sentence, one source
//! (defect B-072, 2026-09-02).
//!
//! # Why this module exists
//!
//! A run that stopped for a person parks two different things on one id list,
//! and each restarts differently. A gated tool call's park carries the node's
//! turn key, so a verdict re-runs the turn and the run goes on by itself. A
//! blocker — a question the agent raised — is parked `Unlinked` with no
//! continuation, deliberately, because answering a question is not the act of
//! authorising a call; deciding it is recorded against the card, and then the
//! node itself is re-dispatched from the run's trigger input carrying the
//! answer (issues #1863, #2005) — approving runs the node again, denying stops
//! the run.
//!
//! [`ParkedCalls::blockers`](crate::workflows::caps::ParkedCalls) and
//! [`WorkflowBlockedNode::blockers`](crate::ports::WorkflowBlockedNode) are how
//! the host tells the two apart, and it has always known. What it did not have
//! was one place to *say* it.
//!
//! Defect B-013 was the console making the gated-call claim for both kinds; its
//! fix put `blockers` on the wire and taught the console's `resume-claim.ts`
//! the distinction. But the host composes this sentence twice —
//! `caps::blocked_diagnosis`, which the Observatory renders, and
//! `runner::blocked_notice`, which the run's own notices carry — and only the
//! first branched. So a corrected run panel printed, directly beneath its own
//! "this run cannot be continued", the host's stored *"approve it in Approvals
//! and this run continues on its own"*. Two contradicting sentences in one
//! panel, from one host, about one run.
//!
//! Extracting the branch is the fix rather than editing the second composer,
//! because a second composer is what produced the defect: correcting it in
//! place leaves the next one free to drift the same way. There is now one
//! function, both callers use it, and
//! [`test::both_host_composers_make_the_same_claim`] fails if a third appears
//! that does not.
//!
//! The wording is `blocked_diagnosis`'s, unchanged — it was already the correct
//! half, and B-013's whole complaint was that the two disagreed, so the fix
//! moves the wrong one onto the right one rather than inventing a third
//! spelling for both.

/// The sentence saying what deciding this node's cards does, or `None` when
/// there is nothing decidable to say it about.
///
/// * `waiting` — how many approvals the node actually opened.
/// * `blockers` — how many of those are a question the agent raised rather than
///   a gated tool call.
///
/// Returned without a leading space: a caller joins it to its own prose, and
/// the two callers punctuate differently.
///
/// `pub(crate)` rather than `pub(super)` because the claim outgrew this module
/// with defect B-070: the chat note a resolved workflow blocker posts is the
/// same statement about the same card, so `company::runtime`'s own tests check
/// they never disagree, even though the two are worded for different surfaces
/// (a run panel here, a DM acknowledgement there).
pub(crate) fn resume_claim(waiting: usize, blockers: usize) -> Option<String> {
    if waiting == 0 {
        // Nothing was parked, so there is no card to decide and no promise to
        // make about deciding one. The `unparkable` case lands here too, and
        // that is correct: its own sentence already says nobody will be asked.
        return None;
    }
    if blockers == 0 {
        return Some(
            "Approving the card continues this run automatically; because approving re-runs \
             the agent's turn, a changed decision may ask again."
                .to_string(),
        );
    }
    if blockers >= waiting {
        return Some(format!(
            "{} a question the agent raised, not a call waiting to be authorised: answering it \
             re-enters this step — approving runs it again, and denying stops the run.",
            if waiting == 1 {
                "The card is"
            } else {
                "The cards are"
            }
        ));
    }
    Some(
        "Some of these are gated tool calls, which continue this run when approved; the rest \
         are questions the agent raised, which re-enter the step they stopped — approving runs \
         it again, and denying stops the run."
            .to_string(),
    )
}

#[cfg(test)]
mod test;
