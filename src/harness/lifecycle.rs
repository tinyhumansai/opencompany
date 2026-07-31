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
//! # Relationship to neighbouring issues
//!
//! * **#171 (`in_review` → `done`, PR #179) — landed, and folded in here.**
//!   #179 shipped the done-write as a `success_terminal_column` helper inside
//!   `brain.rs`. That is exactly the decision this module exists to own, so the
//!   helper moved here and [`landing_column`] now consumes it. A card reaches
//!   [`COLUMN_DONE`] by one of two routes, and never both: a *delegated* card
//!   (one carrying an `origin_chat_id`) completes straight to `done`, because
//!   its answer is relayed into the conversation it came from and no operator
//!   is watching the board for it; a *board-created* card lands in
//!   [`COLUMN_IN_REVIEW`] and reaches `done` only through an approving
//!   orchestrator verdict ([`review_landing_column`]). Between them every
//!   card has a terminal, which is what #171 was about.
//! * **#185 (per-task event correlation, PR #190, still open).** #190 journals
//!   `CompanyEvent::DeskTaskCompleted { column, .. }` from the tail of
//!   `run_task`. That `column` is exactly what [`landing_column`] decides, so
//!   once it lands the event reports this module's decision rather than a
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
/// The terminal column — nothing dispatches out of it. Reached by
/// [`landing_column`] for a delegated card and by [`review_landing_column`] for
/// an approved board card (issue #171 / PR #179).
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
/// Deliberately only two outcomes — see [`review_landing_column`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewDecision {
    /// The work is accepted, which finishes the card: this is #171's
    /// done-transition for a board-created card.
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
/// **`Approve` writes [`COLUMN_DONE`].** This is the `in_review → done`
/// transition issue #171 asked for, in the place #186 built for it: the
/// orchestrator's verdict *is* the review a board-created card was parked
/// waiting for, so approving it finishes it. It cannot duplicate #179's
/// origin-based done-write, because only a card with **no** `origin_chat_id`
/// ever reaches `in_review` in the first place (see [`landing_column`]).
/// `Revise` sends the card back to be re-dispatched.
pub fn review_landing_column(decision: ReviewDecision) -> &'static str {
    match decision {
        ReviewDecision::Approve => COLUMN_DONE,
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

/// Where a run that produced a result lands its card (issue #171, PR #179).
///
/// `in_review` is a naming convention, not a mechanism: nothing consumes it
/// automatically. `task_enters_in_progress` only edge-fires a dispatch when a
/// card enters `in_progress`, so an `in_review` card triggers no further cycle.
///
/// That is right for a card an operator made themselves — they are the
/// reviewer, and the card is sitting in front of them (and the orchestrator can
/// close it out with [`ReviewDecision::Approve`]). It strands a card stamped
/// with `origin_chat_id`: that card came from `spawn_task` during an
/// agent-to-agent handoff, its result is relayed straight back into the
/// originating thread, and nobody is watching the board for it. So a card that
/// remembers an origin completes to `done`.
pub fn success_terminal_column(card: &TaskRecord) -> &'static str {
    if card.origin_chat_id.is_some() {
        COLUMN_DONE
    } else {
        COLUMN_IN_REVIEW
    }
}

/// The board column a run ending this way lands its card in.
///
/// The single authority for that mapping. A failed or cancelled run goes back
/// to `backlog` (it is not reviewable work); a paused one parks; everything
/// that produced a result goes to its [`success_terminal_column`] — `done` for
/// a delegated card, `in_review` for a board-created one.
pub fn landing_column(end: TaskRunEnd, card: &TaskRecord) -> &'static str {
    match end {
        TaskRunEnd::Completed | TaskRunEnd::RedirectsExhausted => success_terminal_column(card),
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
            parent_task_id: None,
        }
    }

    /// A card carrying an `origin_chat_id` — one spawned during a handoff.
    fn delegated_card(column: &str) -> TaskRecord {
        let mut c = card(column, None);
        c.origin_chat_id = Some("strategy".to_string());
        c
    }

    /// The mapping every break point in `run_task`'s steer loop now defers to.
    /// Pinned exhaustively so a new `TaskRunEnd` cannot be added without a
    /// deliberate decision about where its card lands.
    #[test]
    fn landing_column_is_the_single_authority_for_a_cards_fate() {
        let board = card(COLUMN_IN_REVIEW, None);
        assert_eq!(
            landing_column(TaskRunEnd::Completed, &board),
            COLUMN_IN_REVIEW
        );
        assert_eq!(
            landing_column(TaskRunEnd::RedirectsExhausted, &board),
            COLUMN_IN_REVIEW
        );
        assert_eq!(landing_column(TaskRunEnd::Failed, &board), COLUMN_BACKLOG);
        assert_eq!(
            landing_column(TaskRunEnd::Cancelled, &board),
            COLUMN_BACKLOG
        );
        assert_eq!(landing_column(TaskRunEnd::Paused, &board), COLUMN_PAUSED);
    }

    /// #171 (PR #179): a delegated card completes to `done` instead of
    /// stranding in `in_review` that nobody is watching. Both success
    /// terminals — a clean finish and the redirect cap — must agree, or a
    /// steered handoff still strands.
    #[test]
    fn a_delegated_card_completes_to_done_but_a_stopped_one_still_does_not() {
        let delegated = delegated_card(COLUMN_IN_REVIEW);
        assert_eq!(
            landing_column(TaskRunEnd::Completed, &delegated),
            COLUMN_DONE
        );
        assert_eq!(
            landing_column(TaskRunEnd::RedirectsExhausted, &delegated),
            COLUMN_DONE
        );

        // A run that produced no result is not finished work, whatever the
        // card's origin: it goes back or parks, never to the terminal column.
        for end in [
            TaskRunEnd::Failed,
            TaskRunEnd::Cancelled,
            TaskRunEnd::Paused,
        ] {
            assert_ne!(
                landing_column(end, &delegated),
                COLUMN_DONE,
                "an unfinished run must not reach the terminal column"
            );
        }
    }

    /// The success terminal is chosen by origin, not by outcome: a
    /// board-created card keeps its `in_review` review gate.
    #[test]
    fn success_terminal_column_is_done_only_for_a_card_with_an_origin() {
        assert_eq!(
            success_terminal_column(&card(COLUMN_IN_REVIEW, None)),
            COLUMN_IN_REVIEW
        );
        assert_eq!(
            success_terminal_column(&delegated_card(COLUMN_IN_REVIEW)),
            COLUMN_DONE
        );
    }

    /// #186 part b: the orchestrator's review verdict finishes a board card.
    /// This is #171's `in_review → done` write for the one card shape #179's
    /// origin rule cannot reach — a card with no origin never completes to
    /// `done` on its own, so without this it would sit in review forever.
    #[test]
    fn an_approving_review_finishes_the_card_and_revise_sends_it_back() {
        assert_eq!(review_landing_column(ReviewDecision::Approve), COLUMN_DONE);
        assert_eq!(
            review_landing_column(ReviewDecision::Revise),
            COLUMN_BACKLOG
        );
    }

    /// The two done-writes are disjoint: review only ever sees a card that
    /// reached `in_review`, and #179's rule sends every card with an origin
    /// straight to `done` instead. So no card is finished twice.
    #[test]
    fn only_a_card_without_an_origin_can_reach_review() {
        assert_ne!(
            landing_column(TaskRunEnd::Completed, &delegated_card(COLUMN_IN_REVIEW)),
            COLUMN_IN_REVIEW,
            "a delegated card must never park in the column review consumes"
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
