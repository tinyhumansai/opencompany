//! Orchestrator-owned lifecycle for a dispatched board task (issue #186).
//!
//! Before this module, `run_task` decided a dispatched card's fate inline: the
//! landing column was a bare `"in_review"` / `"backlog"` / `"paused"` string
//! literal written at each of five break points in the steer loop, and the
//! completion bubble was attributed to whichever agent happened to run the
//! turn. Two problems with that:
//!
//! * **No authority.** The intended model is "the orchestrator maintains
//!   assignment, review, and done" — but the transitions were mechanical, with
//!   no single place that owns them and nothing for a policy to hook.
//! * **The wrong voice.** The assignee posted its own result straight back to
//!   the operator, which breaks the single-accountable-voice model the
//!   delegation path already follows (`HarnessBrain::run_delegation`): the
//!   orchestrator is the operator's one point of contact, and a desk member
//!   answering directly bypasses it.
//!
//! This module is the seam. It holds no state and performs no I/O — it is the
//! pure *decision* layer (`TaskRunEnd` → landing column, and the relay bubble's
//! shape), so it is unit-testable without a harness pool, a task store, or a
//! live agent. `run_task` keeps the I/O and calls in here for every choice.
//!
//! # Pending dependencies
//!
//! * **#171 (`in_review` → `done`, PR #179, still open).** [`COLUMN_DONE`] is
//!   defined here but deliberately never returned by [`landing_column`]: no
//!   code path in this crate writes the done column yet, and this issue must
//!   not duplicate that transition. When #179 lands, the done-write belongs in
//!   [`landing_column`] (or a reviewer-driven successor to it) rather than as a
//!   sixth inline string somewhere else.
//! * **#185 (per-task event correlation, PR #190, still open).** #190 journals
//!   `CompanyEvent::DeskTaskCompleted { column, .. }` from the tail of
//!   `run_task`. That `column` is exactly what [`landing_column`] decides, so
//!   once both land the event reports this module's decision rather than a
//!   re-derived literal. **This issue does not emit that event** — doing so
//!   would double-journal the timeline's terminal anchor.

use crate::ports::TaskRecord;
use crate::ports::types::{OutboundMessage, ReplyTo};

/// The board column a card in review sits in, awaiting the orchestrator.
pub const COLUMN_IN_REVIEW: &str = "in_review";
/// The board column a stopped or failed run returns its card to.
pub const COLUMN_BACKLOG: &str = "backlog";
/// The board column a paused run parks its card in. Resume is a plain
/// `column → in_progress` PATCH, which re-triggers dispatch.
pub const COLUMN_PAUSED: &str = "paused";
/// The terminal column. **Nothing in this crate writes it yet** — the
/// transition is issue #171's (PR #179, open). Defined here so the constant
/// lives beside its siblings and #171 has an obvious seat, not because this
/// issue uses it.
pub const COLUMN_DONE: &str = "done";

/// The note attribution used for an operator-initiated stop, as opposed to a
/// result the assignee produced.
pub const OPERATOR_ATTRIBUTION: &str = "operator";

/// How one dispatch run ended, independent of who ran it or what it said.
///
/// This is the whole input to the lifecycle decision. Keeping it separate from
/// the result *text* is what lets the column choice be tested without running
/// an agent, and what stops a sixth call site inventing a sixth column literal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskRunEnd {
    /// The assignee finished its turn and produced a result.
    Completed,
    /// The turn itself errored (`dispatch failed: …`).
    Failed,
    /// An operator cancelled mid-flight. Partial work is discarded.
    Cancelled,
    /// An operator paused mid-flight. Partial work is preserved in the note.
    Paused,
    /// The operator spent the redirect budget
    /// (`MAX_REDIRECTS_PER_DISPATCH`); the last run's reply is finalized
    /// rather than looping forever.
    RedirectsExhausted,
}

/// The orchestrator's verdict on a card sitting in `in_review` (issue #186
/// part b).
///
/// Deliberately only two outcomes, and deliberately neither of them writes
/// [`COLUMN_DONE`] — see [`review_landing_column`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewDecision {
    /// The work is accepted. The card stays in `in_review`, which is the state
    /// #171's done-transition consumes.
    Approve,
    /// The work needs another pass. The card returns to `backlog` so it can be
    /// re-dispatched.
    Revise,
}

impl ReviewDecision {
    /// Parses the `decision` argument of the `review_task` tool. Accepts the
    /// obvious synonyms an LLM reaches for, because a rejected tool call costs
    /// the orchestrator a whole turn.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "approve" | "approved" | "accept" | "accepted" | "ok" => Some(Self::Approve),
            "revise" | "reject" | "rejected" | "rework" | "changes" => Some(Self::Revise),
            _ => None,
        }
    }
}

/// The board column a reviewed card lands in.
///
/// **`Approve` deliberately leaves the card in `in_review` rather than moving
/// it to `done`.** The `in_review → done` write is issue #171's (PR #179,
/// open), and #186's own scope note says not to duplicate it. What this issue
/// supplies is the orchestrator *authority* around that transition: an
/// approving verdict recorded on the card, in the column #171 consumes. When
/// #179 lands, this is the one function that changes.
pub fn review_landing_column(decision: ReviewDecision) -> &'static str {
    match decision {
        ReviewDecision::Approve => COLUMN_IN_REVIEW,
        ReviewDecision::Revise => COLUMN_BACKLOG,
    }
}

/// The note block a review records on the card, in the orchestrator's voice.
pub fn review_note(decision: ReviewDecision, note: Option<&str>) -> String {
    let verdict = match decision {
        ReviewDecision::Approve => "reviewed: approved",
        ReviewDecision::Revise => "reviewed: needs another pass",
    };
    match note.map(str::trim).filter(|n| !n.is_empty()) {
        Some(note) => format!("{verdict} — {note}"),
        None => verdict.to_string(),
    }
}

/// The board column a run ending this way lands its card in.
///
/// The single authority for that mapping. A failed or cancelled run goes back
/// to `backlog` (it is not reviewable work); a paused one parks; everything
/// that produced a result lands in `in_review` for the orchestrator to judge.
///
/// Never returns [`COLUMN_DONE`] — see the module docs. `done` is #171's
/// transition and is reached by review, not by a run ending.
pub fn landing_column(end: TaskRunEnd) -> &'static str {
    match end {
        TaskRunEnd::Completed | TaskRunEnd::RedirectsExhausted => COLUMN_IN_REVIEW,
        TaskRunEnd::Failed | TaskRunEnd::Cancelled => COLUMN_BACKLOG,
        TaskRunEnd::Paused => COLUMN_PAUSED,
    }
}

/// Who the note block for this ending is attributed to.
///
/// A cancellation is the operator's act, not the assignee's, so it is recorded
/// as theirs — the assignee never said "cancelled while in flight". Every other
/// ending carries the assignee's own words (or the dispatch error raised while
/// running on their behalf).
pub fn note_attribution(end: TaskRunEnd, responder: &str) -> String {
    match end {
        TaskRunEnd::Cancelled => OPERATOR_ATTRIBUTION.to_string(),
        _ => responder.to_string(),
    }
}

/// The operator-facing sentence for a finished card: what happened to it, who
/// did the work, and the accumulated note.
///
/// The `responder` is named only when it is someone other than the relaying
/// orchestrator. That is the one-voice rule: the orchestrator always speaks,
/// and it credits the doer when the doer is somebody else. A card the
/// orchestrator ran itself would otherwise read "… (ceo ran it)" in a bubble
/// already attributed to `ceo`.
pub fn relay_text(card: &TaskRecord, responder: &str, orchestrator: &str) -> String {
    let status = match card.column.as_str() {
        COLUMN_IN_REVIEW => "is ready for review",
        COLUMN_PAUSED => "is paused",
        COLUMN_BACKLOG => "went back to the backlog",
        COLUMN_DONE => "is done",
        other => other,
    };
    let credit = if responder.is_empty() || responder == orchestrator {
        String::new()
    } else {
        format!(" ({responder} ran it)")
    };
    let headline = format!("\"{}\" {status}{credit}.", card.title);
    match card.note.as_deref().filter(|n| !n.trim().is_empty()) {
        Some(note) => format!("{headline}\n\n{note}"),
        None => headline,
    }
}

/// The orchestrator's relay of a finished card back into the conversation it
/// was spawned from.
///
/// Attributed to `orchestrator`, **not** to the agent that did the work —
/// mirroring `run_delegation`'s single-accountable-voice model. The assignee is
/// credited inside the text instead, so the operator still knows who did it
/// without a second agent speaking to them directly.
///
/// `steps` is empty by construction: a dispatched card has no chat bubble to
/// render a timeline on, so its steps go into the note (and, once #190 lands,
/// onto the task's own `task_id`-correlated timeline).
pub fn relay_reply(
    card: &TaskRecord,
    responder: &str,
    orchestrator: &str,
    origin_chat_id: String,
) -> OutboundMessage {
    OutboundMessage {
        channel: orchestrator.to_string(),
        text: relay_text(card, responder, orchestrator),
        reply_to: Some(ReplyTo {
            chat_id: origin_chat_id,
        }),
        steps: Vec::new(),
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn card(column: &str, note: Option<&str>) -> TaskRecord {
        TaskRecord {
            id: "t-1".to_string(),
            title: "Ship the thing".to_string(),
            note: note.map(str::to_string),
            column: column.to_string(),
            priority: "medium".to_string(),
            assignee: "maya".to_string(),
            updated_at_millis: 0,
            origin_chat_id: None,
        }
    }

    /// The mapping every break point in `run_task`'s steer loop now defers to.
    /// Pinned exhaustively so a new `TaskRunEnd` cannot be added without a
    /// deliberate decision about where its card lands.
    #[test]
    fn landing_column_is_the_single_authority_for_a_cards_fate() {
        assert_eq!(landing_column(TaskRunEnd::Completed), COLUMN_IN_REVIEW);
        assert_eq!(
            landing_column(TaskRunEnd::RedirectsExhausted),
            COLUMN_IN_REVIEW
        );
        assert_eq!(landing_column(TaskRunEnd::Failed), COLUMN_BACKLOG);
        assert_eq!(landing_column(TaskRunEnd::Cancelled), COLUMN_BACKLOG);
        assert_eq!(landing_column(TaskRunEnd::Paused), COLUMN_PAUSED);
    }

    /// #171 is still open (PR #179): nothing here may write the done column, or
    /// this issue would duplicate that transition.
    #[test]
    fn no_run_ending_lands_a_card_in_done() {
        for end in [
            TaskRunEnd::Completed,
            TaskRunEnd::Failed,
            TaskRunEnd::Cancelled,
            TaskRunEnd::Paused,
            TaskRunEnd::RedirectsExhausted,
        ] {
            assert_ne!(
                landing_column(end),
                COLUMN_DONE,
                "the done transition belongs to #171, not to a run ending"
            );
        }
    }

    /// #186 part b: the orchestrator's review verdict. `Approve` must NOT
    /// write `done` — that transition is #171's (PR #179, open) — so an
    /// approved card stays in `in_review`, which is exactly the state #171
    /// consumes. `Revise` sends it back to be re-dispatched.
    #[test]
    fn an_approved_review_waits_in_review_for_171_rather_than_writing_done() {
        assert_eq!(
            review_landing_column(ReviewDecision::Approve),
            COLUMN_IN_REVIEW,
            "approving must not duplicate #171's done-write"
        );
        assert_ne!(review_landing_column(ReviewDecision::Approve), COLUMN_DONE);
        assert_eq!(
            review_landing_column(ReviewDecision::Revise),
            COLUMN_BACKLOG
        );
    }

    #[test]
    fn a_review_decision_accepts_the_synonyms_a_model_reaches_for() {
        for raw in ["approve", "Approved", " ACCEPT ", "ok"] {
            assert_eq!(
                ReviewDecision::parse(raw),
                Some(ReviewDecision::Approve),
                "{raw}"
            );
        }
        for raw in ["revise", "Reject", "rework", "changes"] {
            assert_eq!(
                ReviewDecision::parse(raw),
                Some(ReviewDecision::Revise),
                "{raw}"
            );
        }
        // An unrecognised verdict is rejected rather than silently approved —
        // guessing here would let a card through review on a typo.
        assert_eq!(ReviewDecision::parse("maybe"), None);
        assert_eq!(ReviewDecision::parse(""), None);
    }

    #[test]
    fn a_review_note_records_the_verdict_and_any_reviewer_comment() {
        assert_eq!(
            review_note(ReviewDecision::Approve, None),
            "reviewed: approved"
        );
        assert_eq!(
            review_note(ReviewDecision::Revise, Some("tighten the intro")),
            "reviewed: needs another pass — tighten the intro"
        );
        // A blank comment must not leave a dangling em dash.
        assert_eq!(
            review_note(ReviewDecision::Approve, Some("   ")),
            "reviewed: approved"
        );
    }

    #[test]
    fn a_cancellation_is_attributed_to_the_operator_not_the_assignee() {
        assert_eq!(note_attribution(TaskRunEnd::Cancelled, "maya"), "operator");
        assert_eq!(note_attribution(TaskRunEnd::Completed, "maya"), "maya");
        assert_eq!(note_attribution(TaskRunEnd::Paused, "maya"), "maya");
        assert_eq!(note_attribution(TaskRunEnd::Failed, "maya"), "maya");
    }

    /// The one-voice change: the bubble is the orchestrator's, and the assignee
    /// is credited in the text rather than speaking to the operator directly.
    #[test]
    fn the_relay_bubble_is_the_orchestrators_and_credits_the_assignee() {
        let finished = card(COLUMN_IN_REVIEW, Some("[maya] shipped it"));
        let msg = relay_reply(&finished, "maya", "ceo", "strategy".to_string());

        assert_eq!(msg.channel, "ceo", "the orchestrator owns the reply");
        assert_eq!(
            msg.reply_to.as_ref().map(|r| r.chat_id.as_str()),
            Some("strategy")
        );
        assert!(msg.text.contains("Ship the thing"), "{}", msg.text);
        assert!(msg.text.contains("is ready for review"), "{}", msg.text);
        assert!(msg.text.contains("(maya ran it)"), "{}", msg.text);
        assert!(msg.text.contains("shipped it"), "{}", msg.text);
        assert!(
            msg.steps.is_empty(),
            "a dispatched card discards its steps into the note"
        );
    }

    /// …but it does not credit itself. A card the orchestrator ran reads as one
    /// voice, not as the orchestrator narrating its own work in the third
    /// person.
    #[test]
    fn the_relay_does_not_credit_the_orchestrator_to_itself() {
        let finished = card(COLUMN_IN_REVIEW, None);
        let msg = relay_reply(&finished, "ceo", "ceo", "main".to_string());
        assert!(!msg.text.contains("ran it"), "{}", msg.text);
        assert_eq!(msg.text, "\"Ship the thing\" is ready for review.");

        // An unresolved assignee credits nobody rather than an empty paren.
        let orphan = relay_reply(&finished, "", "ceo", "main".to_string());
        assert!(!orphan.text.contains("ran it"), "{}", orphan.text);
    }

    /// The relay reports where the card actually landed — a paused or failed
    /// run must never read as a success.
    #[test]
    fn the_relay_reflects_the_landing_column_not_a_presumed_success() {
        let paused = card(COLUMN_PAUSED, None);
        assert!(
            relay_text(&paused, "maya", "ceo").contains("is paused"),
            "paused card must not read as finished"
        );

        let returned = card(COLUMN_BACKLOG, Some("[operator] cancelled while in flight"));
        let text = relay_text(&returned, "maya", "ceo");
        assert!(text.contains("went back to the backlog"), "{text}");
        assert!(text.contains("cancelled while in flight"), "{text}");
    }

    /// A card with no note (or a whitespace-only one) still relays a complete
    /// sentence rather than a dangling blank block.
    #[test]
    fn a_noteless_card_still_relays_a_complete_sentence() {
        let bare = card(COLUMN_IN_REVIEW, None);
        assert_eq!(
            relay_text(&bare, "maya", "ceo"),
            "\"Ship the thing\" is ready for review (maya ran it)."
        );
        let blank = card(COLUMN_IN_REVIEW, Some("   \n  "));
        assert_eq!(
            relay_text(&blank, "maya", "ceo"),
            "\"Ship the thing\" is ready for review (maya ran it)."
        );
    }
}
