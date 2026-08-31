//! Orchestrator-owned lifecycle for a dispatched board task (issue #186).
//!
//! Before this module, `run_task` decided a dispatched card's fate inline: the
//! landing column was a bare `"in_review"` / `"todo"` / `"paused"` string
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
//! * **#171 (`in_review` → `done`, PR #179) — landed here, then narrowed by
//!   #337.** #179 shipped a `success_terminal_column` helper that sent a
//!   *delegated* card (one carrying an `origin_chat_id`) straight to
//!   [`COLUMN_DONE`], on the argument that its answer was relayed into the
//!   conversation it came from and nobody was watching the board for it.
//!
//!   The operator decision of 2026-08-05 removed that route: **[`COLUMN_DONE`]
//!   is reached only by a person**, through an approving orchestrator verdict
//!   ([`review_landing_column`]). Every card — delegated or board-created —
//!   now stops in [`COLUMN_IN_REVIEW`] first. So there is still exactly one
//!   terminal, which is what #171 was about; there is now exactly one route to
//!   it, and it runs through a human.
//! * **#337 (a settled run advances its card) — where the column table went.**
//!   [`landing_column`] is no longer its own mapping. It adapts a
//!   [`TaskRunEnd`] onto [`crate::ports::tasks::column_for_settled_run`], which
//!   is keyed on the settled [`RunStatus`] and lives on the **port** because
//!   two of its three callers (the cycle's terminality backstop and the boot
//!   reaper's card sweep) are ungated and cannot see this module.
//! * **#185 (per-task event correlation, PR #190, still open).** #190 journals
//!   `CompanyEvent::DeskTaskCompleted { column, .. }` from the tail of
//!   `run_task`. That `column` is exactly what [`landing_column`] decides, so
//!   once it lands the event reports this module's decision rather than a
//!   re-derived literal. **This issue does not emit that event** — doing so
//!   would double-journal the timeline's terminal anchor.

use crate::ports::TaskRecord;
use crate::ports::runs::RunStatus;
use crate::ports::types::{OutboundMessage, ReplyTo};

/// The board's column vocabulary, re-exported so every `lifecycle::COLUMN_*`
/// path here is unchanged.
///
/// The definitions moved to the task **port** in issue #205: this module is
/// `#[cfg(feature = "openhuman")]`, so the REST write boundary — which now
/// validates a card's column — could not see them. `COLUMN_IN_REVIEW` is where
/// a card awaits the orchestrator, `COLUMN_IN_PROGRESS` where a card is being
/// worked — where a dispatched card already is, and where one whose turn
/// **handed the work off** stays while the delegate runs (issue #204) —
/// `COLUMN_TODO` where a stopped or failed run returns it (issue #301 — it used
/// to return to a separate `backlog` pool, which epic #183 §3 collapsed into
/// To-do), `COLUMN_PAUSED` where a paused run parks it (resume is a plain
/// `column → in_progress` PATCH, which re-triggers dispatch), `COLUMN_PLANNING`
/// which nothing here writes yet (§4's planning pass owns it), and `COLUMN_DONE`
/// the terminal — since issue #337 reached **only** through
/// [`review_landing_column`], because Done is a person's decision and no run
/// ending writes it.
pub use crate::ports::tasks::{
    COLUMN_DONE, COLUMN_IN_PROGRESS, COLUMN_IN_REVIEW, COLUMN_PAUSED, COLUMN_PLANNING, COLUMN_TODO,
};

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
    /// The assignee's turn **handed the work off** to another agent rather than
    /// doing it (issue #204). This is not an ending: the card is reassigned to
    /// the delegate and stays in [`COLUMN_IN_PROGRESS`] while they run, and the
    /// delegate's own ending — a [`Completed`](Self::Completed) with their
    /// output, or a [`Cancelled`](Self::Cancelled) — settles it afterwards.
    ///
    /// Without this, "I finished the work" and "I handed it off" were the same
    /// `Completed`, so a delegating dispatch landed in `in_review` under the
    /// delegator with the delegate never having run.
    Delegated,
    /// The turn itself errored (`dispatch failed: …`).
    Failed,
    /// An operator cancelled mid-flight. Partial work is discarded.
    Cancelled,
    /// An operator paused mid-flight, **or** the turn itself paused for lack
    /// of inference budget/credits (issue #1846 review, Codex #3864988168) —
    /// a background dispatch's mirror of the operator-chat path's own
    /// budget-paused bubble. Partial work is preserved in the note either
    /// way; a budget pause additionally parks a durable per-agent re-issue
    /// marker (`crate::runtime::grants::budget_pauses_for`), redeemable from
    /// the console once credits are added.
    Paused,
    /// The operator spent the redirect budget
    /// (`MAX_REDIRECTS_PER_DISPATCH`); the last run's reply is finalized
    /// rather than looping forever.
    RedirectsExhausted,
    /// The turn stopped on something **a person can answer** and parked a
    /// durable blocker instead of settling (issue #1861): a rejected model id,
    /// an expired credential, a missing prerequisite, or the assignee's own
    /// `escalate_to_human`.
    ///
    /// Separate from [`Failed`](Self::Failed) because the two need opposite
    /// treatment. A failure is over — the card returns to To-do and the reason
    /// is history. A blocker is an open question: the card parks, somebody is
    /// asked, and the answer resumes the step. Reaching this arm means the
    /// classifier decided the stop was answerable; everything it does not
    /// recognise keeps settling `Failed`.
    Blocked,
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
    /// The work needs another pass. The card returns to `todo` so it can be
    /// re-dispatched, carrying the reviewer's reason in its note (issue #301 /
    /// epic #183 §3: a card that cannot proceed goes back to To-do with the
    /// reason on it, never into a stuck state of its own).
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
/// verdict *is* the review the card was parked waiting for, so approving it
/// finishes it. `Revise` sends the card back to be re-dispatched.
///
/// Since issue #337 this is the **only** write of [`COLUMN_DONE`] anywhere —
/// #179's origin-based shortcut is gone and no run ending lands there — so
/// "Done means a person accepted it" is now true by construction rather than
/// by convention.
pub fn review_landing_column(decision: ReviewDecision) -> &'static str {
    match decision {
        ReviewDecision::Approve => COLUMN_DONE,
        ReviewDecision::Revise => COLUMN_TODO,
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
/// **Issue #337 collapsed this onto [`column_for_settled_run`].** It used to be
/// its own table over [`TaskRunEnd`], which meant the landing was derived from
/// *how the turn ended* while the attempt row was derived from *what the run
/// settled into* — two tables that had already drifted apart. The visible
/// symptom: a run that finished its work but parked an approval settled
/// [`RunStatus::WaitingApproval`] and still landed in whatever its ending said,
/// which for a delegated card was `done`. A card filed as finished while a
/// person still had to authorise the call it was waiting on.
///
/// Now there is one table, keyed on the settled status, and this function is
/// the thin adapter that reaches it: `end` → [`settled_run_status`] →
/// [`column_for_settled_run`].
///
/// # This overload assumes nothing was parked
///
/// `landing_column(end)` is [`settled_landing_column`]`(end, 0)`. It is the
/// right call only where no approval can be outstanding, or where a later
/// authoritative write will land the card again with the count in hand. **A
/// settle that can park an approval must call [`settled_landing_column`]** —
/// issue #465: reading the landing from the ending alone is what let a turn
/// whose first call parked settle as a plain success and present as reviewable
/// work it had never started.
///
/// # Two deliberate losses, both wanted
///
/// * **`Succeeded` lands in `in_review`, never `done`.** Operator decision,
///   2026-08-05: Done is reached only by a person. This supersedes the
///   `Succeeded → Done` row in epic #183 §4.
/// * **`success_terminal_column` is gone with it.** It sent a card carrying an
///   `origin_chat_id` — one spawned during an agent-to-agent handoff — straight
///   to `done` on the argument that nobody was watching the board for it (#171
///   / PR #179). Under the decision above there is no automatic route to `done`
///   for *any* card, so that shortcut cannot survive; a delegated card now stops
///   in `in_review` like every other. Nothing is lost from the handoff itself —
///   the delegate's answer is still relayed straight into the originating thread
///   by [`relay_reply`], which is what that conversation was actually waiting
///   on. What changes is that the card stays visible until a person accepts it,
///   which is the point of the review stop.
///
/// [`column_for_settled_run`]: crate::ports::tasks::column_for_settled_run
pub fn landing_column(end: TaskRunEnd) -> &'static str {
    settled_landing_column(end, 0)
}

/// The board column a run lands its card in, given how it ended **and** whether
/// it left an approval parked (issue #465).
///
/// This is [`landing_column`] with the parked-approval overlay applied — the
/// same overlay [`settled_run_status`] applies to the attempt row, reached
/// through the same one table. `landing_column(end)` is exactly
/// `settled_landing_column(end, 0)`.
///
/// # Why the count has to reach the column
///
/// It already reached the *status*: a success that parked something settles
/// [`RunStatus::WaitingApproval`]. The column was derived from
/// [`run_status_for`] instead, which cannot produce that status, so the board
/// was answering from an ending that had been overtaken — and a turn whose very
/// first call parked, having produced nothing, landed in
/// [`COLUMN_IN_REVIEW`] announcing work to check.
///
/// That was reachable two ways, and this closes both:
///
/// * `run_task`'s settle (`brain.rs`) *did* compute the parked count, but the
///   break-point [`landing_column`] call ran first and the authoritative
///   overwrite went to [`column_for_settled_run`] direct — correct only once
///   that table stopped sending `WaitingApproval` to review.
/// * the delegation seam's `settle_work_card` (issue #442's card, opened by
///   construction for work handed to an agent) hardcoded
///   [`Completed`](TaskRunEnd::Completed) and never consulted the count at all.
///   That is the path in the report: a desk asked directly, its first call
///   parked, and the card settled as a plain success.
///
/// Routing both through here keeps the landing one decision — the property
/// issue #337 collapsed this module onto the port to get — rather than two
/// call sites each remembering to apply an overlay.
///
/// [`column_for_settled_run`]: crate::ports::tasks::column_for_settled_run
pub fn settled_landing_column(end: TaskRunEnd, parked_approvals: usize) -> &'static str {
    // A hand-off is explicitly NOT a settle: the delegate is running, so the
    // card keeps the column it was dispatched in and only lands once they come
    // back with something (issue #204). Handled here rather than in the mapping
    // because `run_status_for(Delegated)` is `Paused` — right for the attempt
    // row (it is waiting on something other than a person) and wrong for the
    // board (the work has changed hands, not stopped).
    //
    // It outranks the parked overlay too: a delegator that parked an approval
    // still handed the work on, and the delegate is running it now.
    if matches!(end, TaskRunEnd::Delegated) {
        return COLUMN_IN_PROGRESS;
    }
    // Every other ending settles, so the mapping always answers. The fallback
    // is unreachable and exists only so this stays total without an `expect`.
    crate::ports::tasks::column_for_settled_run(settled_run_status(end, parked_approvals))
        .unwrap_or(COLUMN_IN_PROGRESS)
}

/// The [`RunStatus`] a run ending this way settles into (issue #242).
///
/// The board column and the run status answer two different questions about the
/// same ending — *where does the card go* and *how did the attempt end* — so
/// both are decided here, from the one [`TaskRunEnd`], rather than one of them
/// being re-derived from the other. Deriving the status from the landing column
/// would be actively wrong: `Failed` and `Cancelled` both land in
/// [`COLUMN_TODO`] and are emphatically not the same outcome.
///
/// The mapping, and why:
///
/// * [`Completed`](TaskRunEnd::Completed) and
///   [`RedirectsExhausted`](TaskRunEnd::RedirectsExhausted) →
///   [`Succeeded`](RunStatus::Succeeded). Both produced a result and both land
///   in the card's success terminal; spending the redirect budget is how the
///   run *ended*, not evidence that it failed.
/// * [`Failed`](TaskRunEnd::Failed) → [`Failed`](RunStatus::Failed), carrying
///   the reason.
/// * [`Cancelled`](TaskRunEnd::Cancelled) →
///   [`Cancelled`](RunStatus::Cancelled) — an operator stopped it, which is
///   neither a success nor a defect.
/// * [`Paused`](TaskRunEnd::Paused) → [`Paused`](RunStatus::Paused). Epic #183
///   decision 2: an operator pause is resolved by *resuming*, not by a person
///   approving something, so it is `Paused` and never `WaitingApproval`.
/// * [`Blocked`](TaskRunEnd::Blocked) → [`Blocked`](RunStatus::Blocked). Epic
///   #183 decision 2 again, and the case it did not have a status for: the
///   attempt is waiting on *a person*, but on an answer rather than on an
///   approval. It parks rather than settling, so the card lands in
///   [`COLUMN_PAUSED`] with the question on it.
/// * [`Delegated`](TaskRunEnd::Delegated) → [`Paused`](RunStatus::Paused). A
///   hand-off is not an ending at all — the card stays in
///   [`COLUMN_IN_PROGRESS`] — and it is unreachable as a run settle today,
///   because `run_task` awaits the delegate inside the same attempt. Named
///   anyway so this mapping is exhaustive: if a future path ever settles a run
///   on a hand-off, the attempt is waiting on *something other than a person*
///   (the delegate), which decision 2 makes `Paused` — non-terminal and
///   resumable — rather than a terminal status that would file unfinished work
///   as done.
pub fn run_status_for(end: TaskRunEnd) -> RunStatus {
    match end {
        TaskRunEnd::Completed | TaskRunEnd::RedirectsExhausted => RunStatus::Succeeded,
        TaskRunEnd::Failed => RunStatus::Failed,
        TaskRunEnd::Cancelled => RunStatus::Cancelled,
        TaskRunEnd::Paused | TaskRunEnd::Delegated => RunStatus::Paused,
        TaskRunEnd::Blocked => RunStatus::Blocked,
    }
}

/// The [`RunStatus`] an attempt settles into, given how it ended **and**
/// whether it left a person something to act on (issue #242).
///
/// `parked_approvals` counts the approval requests this attempt's own turns
/// parked. A run that otherwise succeeded while parking at least one finishes
/// [`RunStatus::WaitingApproval`] rather than [`RunStatus::Succeeded`] — epic
/// #183 decision 2 in its purest form: *who* unblocks the work decides where it
/// parks, and here a person does.
///
/// A run that failed, was cancelled or was paused keeps the status its ending
/// gave it. The operator has a bigger problem than a pending approval, and
/// relabelling a failure as "waiting on you" would hide the reason it stopped.
///
/// [`RunStatus::WaitingApproval`] is terminal-in-v1 (resuming an approved
/// attempt is its own issue) and deliberately **re-enterable across attempts**:
/// the re-dispatch that follows an approval is a *new* run which can wait again.
/// That is what keeps #243's single-use, argument-exact grants coherent instead
/// of forcing an operator to batch several approvals into one.
pub fn settled_run_status(end: TaskRunEnd, parked_approvals: usize) -> RunStatus {
    match run_status_for(end) {
        RunStatus::Succeeded if parked_approvals > 0 => RunStatus::WaitingApproval,
        settled => settled,
    }
}

/// [`settled_run_status`] with issue #1861's blocker overlay: a turn that
/// raised a question the operator has to answer settles
/// [`Blocked`](RunStatus::Blocked).
///
/// `blockers` counts the blocker parks **this attempt's own turns** queued —
/// an `escalate_to_human` call, or a host-classified failure inside a
/// delegated turn.
///
/// # Why it outranks `WaitingApproval` but not a failure
///
/// Both park the card, so the board reads the same either way; the difference
/// is what the run history says the operator owes. An approval is a decision
/// about an effect that is ready to happen. A blocker is a question with
/// nothing behind it yet. Reporting the second as the first sends somebody to
/// the Approvals page looking for something to approve.
///
/// A run that **failed or was cancelled** keeps its own status, exactly as the
/// approval overlay leaves it: the operator has a bigger problem than an
/// unanswered question, and relabelling the failure would hide why the work
/// actually stopped.
///
/// # Why the ending is not rewritten instead
///
/// [`TaskRunEnd`] stays whatever the turn did. The success-terminal check that
/// records a run's artifacts and outputs reads the *ending*, not this status —
/// so an agent that wrote a spec and then asked a question keeps the spec. It
/// is the same separation the approval overlay draws, for the same reason.
pub fn settled_run_status_with_blockers(
    end: TaskRunEnd,
    parked_approvals: usize,
    blockers: usize,
) -> RunStatus {
    match settled_run_status(end, parked_approvals) {
        RunStatus::Succeeded | RunStatus::WaitingApproval if blockers > 0 => RunStatus::Blocked,
        settled => settled,
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
        COLUMN_IN_PROGRESS => "is still in progress",
        COLUMN_PAUSED => "is paused",
        COLUMN_TODO => "is back in Pending",
        // Nothing lands a card here yet (§4's auto-advance will), but naming it
        // keeps a future relay off the raw-column-id fallback below.
        COLUMN_PLANNING => "is being planned",
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
///
/// `task_id` carries the card's own id, for field-contract consistency with
/// every other `OutboundMessage` producer — but `CompanyRuntime::
/// journal_dispatch_replies` intentionally strips it back to `None` before
/// journaling this bubble: the settle that already ran left a
/// `DeskTaskCompleted` event pointed at this same thread, and carrying
/// `task_id` here too would render a second "card opened" chip for a card
/// that is not open by the time this bubble lands. It does not reach
/// `AgentReply::task_id` and does not survive a transcript reload.
pub fn relay_reply(
    card: &TaskRecord,
    responder: &str,
    orchestrator: &str,
    origin_chat_id: String,
) -> OutboundMessage {
    OutboundMessage {
        message_id: None,
        task_id: Some(card.id.clone()),
        channel: orchestrator.to_string(),
        agent: None,
        text: relay_text(card, responder, orchestrator),
        mentions: Vec::new(),
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
            output: None,
            plan: None,
            planning_attempts: Vec::new(),
            deliverable: crate::ports::tasks::TaskDeliverable::Once,
            workflow_proposal: None,
            origin_run_id: None,
            origin_workflow_id: None,
            bounced: None,
        }
    }

    /// A card carrying an `origin_chat_id` — one spawned during a handoff.
    fn delegated_card(column: &str) -> TaskRecord {
        let mut c = card(column, None);
        c.origin_chat_id = Some("strategy".to_string());
        c
    }

    /// The mapping every break point in `run_task`'s steer loop defers to.
    /// Pinned exhaustively so a new `TaskRunEnd` cannot be added without a
    /// deliberate decision about where its card lands.
    ///
    /// **Rewritten by issue #337** — it previously pinned `Completed →
    /// in_review` *for a board card* while a delegated one went to `done`. The
    /// landing no longer depends on the card at all.
    #[test]
    fn landing_column_is_the_single_authority_for_a_cards_fate() {
        assert_eq!(landing_column(TaskRunEnd::Completed), COLUMN_IN_REVIEW);
        assert_eq!(
            landing_column(TaskRunEnd::RedirectsExhausted),
            COLUMN_IN_REVIEW
        );
        assert_eq!(landing_column(TaskRunEnd::Failed), COLUMN_TODO);
        assert_eq!(landing_column(TaskRunEnd::Cancelled), COLUMN_TODO);
        assert_eq!(landing_column(TaskRunEnd::Paused), COLUMN_PAUSED);
        assert_eq!(landing_column(TaskRunEnd::Delegated), COLUMN_IN_PROGRESS);
    }

    /// Issue #204: handing the work off is not finishing it. A delegating turn
    /// used to settle as `Completed`, which parked the card under the
    /// *delegator* while the delegate had not even run — so the hand-off keeps
    /// the card in progress instead.
    ///
    /// It is the one ending that does **not** go through
    /// [`column_for_settled_run`], and this pins why: `run_status_for` calls a
    /// hand-off `Paused`, which is right for the attempt row and would park the
    /// card while its delegate is actively working.
    #[test]
    fn a_hand_off_keeps_the_card_in_progress_rather_than_finishing_it() {
        assert_eq!(landing_column(TaskRunEnd::Delegated), COLUMN_IN_PROGRESS);
        assert_eq!(run_status_for(TaskRunEnd::Delegated), RunStatus::Paused);
        assert_ne!(
            landing_column(TaskRunEnd::Delegated),
            crate::ports::tasks::column_for_settled_run(RunStatus::Paused).unwrap(),
            "a hand-off must not park the card its delegate is still working"
        );
    }

    /// **The #337 edit, stated as a test.** A card carrying an
    /// `origin_chat_id` — one spawned during an agent-to-agent handoff — used
    /// to complete straight to `done` (#171 / PR #179). It now stops in
    /// `in_review` like every other card, because the operator decision of
    /// 2026-08-05 leaves no automatic route to `done` at all.
    ///
    /// Nothing about the handoff is lost: [`relay_reply`] still answers in the
    /// thread the card came from. What changes is that the card stays visible
    /// until a person accepts it.
    #[test]
    fn a_delegated_card_now_stops_in_review_like_every_other_card() {
        // Both success endings agree, or a steered handoff would diverge.
        assert_eq!(landing_column(TaskRunEnd::Completed), COLUMN_IN_REVIEW);
        assert_eq!(
            landing_column(TaskRunEnd::RedirectsExhausted),
            COLUMN_IN_REVIEW
        );
        // A delegated card still relays its answer into the originating thread,
        // which is what that conversation was waiting on.
        let relayed = relay_reply(
            &delegated_card(COLUMN_IN_REVIEW),
            "maya",
            "ceo",
            "strategy".to_string(),
        );
        assert_eq!(
            relayed.reply_to.as_ref().map(|r| r.chat_id.as_str()),
            Some("strategy")
        );
        // Issue #1852: the relay carries the card's own id for field-contract
        // consistency, but `CompanyRuntime::journal_dispatch_replies` strips
        // it back to `None` before journaling — the settle already left a
        // `DeskTaskCompleted` link, and this would only duplicate it.
        assert_eq!(relayed.task_id.as_deref(), Some("t-1"));
        assert!(
            relayed.text.contains("is ready for review"),
            "{}",
            relayed.text
        );
    }

    /// **Done is reached only by a person.** No ending, on any card, may write
    /// the terminal column — the only route there is an approving review.
    #[test]
    fn no_run_ending_ever_writes_done() {
        for end in [
            TaskRunEnd::Completed,
            TaskRunEnd::RedirectsExhausted,
            TaskRunEnd::Failed,
            TaskRunEnd::Cancelled,
            TaskRunEnd::Paused,
            TaskRunEnd::Delegated,
        ] {
            assert_ne!(
                landing_column(end),
                COLUMN_DONE,
                "{end:?} auto-advanced a card to Done"
            );
        }
    }

    /// #186 part b: the orchestrator's review verdict finishes a card. Since
    /// #337 this is the **only** way any card reaches `done` — a person accepts
    /// the result, or it stays in review.
    #[test]
    fn an_approving_review_finishes_the_card_and_revise_sends_it_back() {
        assert_eq!(review_landing_column(ReviewDecision::Approve), COLUMN_DONE);
        assert_eq!(review_landing_column(ReviewDecision::Revise), COLUMN_TODO);
    }

    /// Every card that produces a result reaches the column review consumes —
    /// including a delegated one, which #179 used to route around. So no
    /// finished card is unreachable by a reviewer.
    #[test]
    fn every_finished_card_reaches_the_column_review_consumes() {
        assert_eq!(landing_column(TaskRunEnd::Completed), COLUMN_IN_REVIEW);
        // The review verdict is defined on exactly that column, so the two
        // halves of the lifecycle meet.
        assert_eq!(review_landing_column(ReviewDecision::Approve), COLUMN_DONE);
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

    /// Issue #242: every `TaskRunEnd` maps to exactly one `RunStatus`, and the
    /// mapping is pinned exhaustively so a new ending cannot be added without a
    /// deliberate decision about how its attempt reads.
    #[test]
    fn run_status_is_decided_per_ending_not_re_derived_from_the_column() {
        assert_eq!(run_status_for(TaskRunEnd::Completed), RunStatus::Succeeded);
        assert_eq!(
            run_status_for(TaskRunEnd::RedirectsExhausted),
            RunStatus::Succeeded,
            "spending the redirect budget is how the run ended, not a failure"
        );
        assert_eq!(run_status_for(TaskRunEnd::Failed), RunStatus::Failed);
        assert_eq!(run_status_for(TaskRunEnd::Cancelled), RunStatus::Cancelled);
        assert_eq!(run_status_for(TaskRunEnd::Paused), RunStatus::Paused);
        assert_eq!(run_status_for(TaskRunEnd::Delegated), RunStatus::Paused);

        // The reason the status cannot be derived *from* the column, even now
        // that the column is derived from the status: the arrow only points one
        // way. Two endings share a column and are emphatically not the same
        // outcome.
        assert_eq!(
            landing_column(TaskRunEnd::Failed),
            landing_column(TaskRunEnd::Cancelled)
        );
        assert_ne!(
            run_status_for(TaskRunEnd::Failed),
            run_status_for(TaskRunEnd::Cancelled)
        );
    }

    /// Epic #183 decision 2 at the settle boundary: only a *person* being
    /// required parks a run in review. An operator pause is resolved by
    /// resuming, so no ending alone may produce `WaitingApproval` — that status
    /// is minted solely by a parked approval.
    #[test]
    fn no_lifecycle_ending_alone_parks_a_run_in_review() {
        for end in ALL_ENDINGS {
            assert_ne!(
                run_status_for(end),
                RunStatus::WaitingApproval,
                "{end:?} must not claim a person is needed on the strength of the ending alone"
            );
            assert_eq!(
                settled_run_status(end, 0),
                run_status_for(end),
                "with nothing parked, the settle is exactly the ending's status"
            );
        }
    }

    /// Every `TaskRunEnd`, so a table-driven test cannot silently miss a new
    /// variant (the exhaustive `match` in `run_status_for` breaks first).
    const ALL_ENDINGS: [TaskRunEnd; 6] = [
        TaskRunEnd::Completed,
        TaskRunEnd::RedirectsExhausted,
        TaskRunEnd::Failed,
        TaskRunEnd::Cancelled,
        TaskRunEnd::Paused,
        TaskRunEnd::Delegated,
    ];

    /// Epic #183 decision 2: a run that did its work but left an approval for a
    /// **person** to act on is in review, not done. Only a success is
    /// reclassified — a failure, a cancellation and a pause keep the reason they
    /// stopped, because relabelling them "waiting on you" would hide it.
    #[test]
    fn a_parked_approval_turns_a_success_into_review_and_nothing_else() {
        assert_eq!(
            settled_run_status(TaskRunEnd::Completed, 1),
            RunStatus::WaitingApproval
        );
        assert_eq!(
            settled_run_status(TaskRunEnd::RedirectsExhausted, 3),
            RunStatus::WaitingApproval,
            "a redirect-capped run that parked something still needs a person"
        );

        for end in [
            TaskRunEnd::Failed,
            TaskRunEnd::Cancelled,
            TaskRunEnd::Paused,
            TaskRunEnd::Delegated,
        ] {
            assert_eq!(
                settled_run_status(end, 2),
                run_status_for(end),
                "{end:?} must keep the reason it stopped rather than reading as a review"
            );
        }
    }

    /// **Issue #465.** A run that parked an approval has not produced a result,
    /// so its card parks instead of presenting as reviewable work.
    ///
    /// The pairing with the row above is the whole point: the *attempt* still
    /// settles `WaitingApproval` — "a person must act" is preserved exactly
    /// where epic #183 decision 2 put it — while the *column* answers the other
    /// question, has the work happened. One ending, two answers, two places.
    #[test]
    fn a_parked_approval_parks_the_card_instead_of_offering_it_for_review() {
        assert_eq!(
            settled_landing_column(TaskRunEnd::Completed, 1),
            COLUMN_PAUSED
        );
        assert_eq!(
            settled_landing_column(TaskRunEnd::RedirectsExhausted, 2),
            COLUMN_PAUSED
        );
        // The run status is untouched, so the console still says who unblocks it.
        assert_eq!(
            settled_run_status(TaskRunEnd::Completed, 1),
            RunStatus::WaitingApproval
        );

        // A turn that genuinely completed still reaches the reviewer.
        assert_eq!(
            settled_landing_column(TaskRunEnd::Completed, 0),
            COLUMN_IN_REVIEW
        );
    }

    /// `landing_column` is the zero-parked case of `settled_landing_column`, so
    /// the two cannot drift into disagreeing about an ending.
    #[test]
    fn the_landing_is_one_decision_taken_with_or_without_a_parked_count() {
        for end in ALL_ENDINGS {
            assert_eq!(
                landing_column(end),
                settled_landing_column(end, 0),
                "{end:?} disagrees with itself once the overlay is spelled out"
            );
        }
    }

    /// **The sibling sweep issue #465 asked for**, as a test rather than a note:
    /// every ending answers *did the work happen?*, and only the ones that
    /// answer yes may land where a reviewer can approve them to Done.
    ///
    /// `Delegated` is the deliberate exception — it is not an ending at all, so
    /// it stays in progress under its delegate rather than settling anywhere.
    #[test]
    fn only_endings_that_produced_something_land_in_review() {
        // Produced a result — reviewable.
        for end in [TaskRunEnd::Completed, TaskRunEnd::RedirectsExhausted] {
            assert_eq!(landing_column(end), COLUMN_IN_REVIEW, "{end:?}");
        }
        // Produced nothing to accept — must not read as reviewable work.
        for end in [
            TaskRunEnd::Failed,
            TaskRunEnd::Cancelled,
            TaskRunEnd::Paused,
            TaskRunEnd::Delegated,
        ] {
            assert_ne!(
                landing_column(end),
                COLUMN_IN_REVIEW,
                "{end:?} produced no result and must not offer one for review"
            );
        }
        // …and neither may a success that stopped at an unauthorised call.
        for end in ALL_ENDINGS {
            assert_ne!(
                settled_landing_column(end, 1),
                COLUMN_IN_REVIEW,
                "{end:?} left an approval outstanding and must not present as reviewable"
            );
        }
    }

    /// A hand-off outranks the parked overlay: the delegator may have parked an
    /// approval, but it still handed the work on and the delegate is running it.
    /// Parking the card here would stall a card somebody is actively working.
    #[test]
    fn a_hand_off_stays_in_progress_even_if_the_delegator_parked_something() {
        assert_eq!(
            settled_landing_column(TaskRunEnd::Delegated, 3),
            COLUMN_IN_PROGRESS
        );
    }

    /// The review stop is **not** terminal-by-accident: it is non-terminal and
    /// re-enterable, which is what lets a re-dispatched card wait on a second
    /// approval instead of forcing #243's single-use grants to be batched.
    #[test]
    fn the_review_stop_stays_re_enterable() {
        let review = settled_run_status(TaskRunEnd::Completed, 1);
        assert!(review.is_parked());
        assert!(!review.is_terminal());
        assert!(RunStatus::Running.can_transition_to(review));
        assert!(review.can_transition_to(RunStatus::Running));
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

        let returned = card(COLUMN_TODO, Some("[operator] cancelled while in flight"));
        let text = relay_text(&returned, "maya", "ceo");
        assert!(text.contains("is back in Pending"), "{text}");
        // Issue #301: collapsing the backlog pool into To-do is only lossless
        // because the reason rides along on the card. The relay must keep
        // saying why, or "back in Pending" is indistinguishable from fresh work.
        assert!(text.contains("cancelled while in flight"), "{text}");

        // Every board column has a sentence. Planning is inert today, so
        // without an arm of its own a relay would fall through to the raw
        // column id and read `"Ship the thing" planning.`.
        for column in crate::ports::tasks::BOARD_COLUMNS {
            let text = relay_text(&card(column, None), "maya", "ceo");
            assert!(
                !text.contains(&format!("\" {column}")),
                "column {column} fell through to the raw-id fallback: {text}"
            );
        }
        assert!(
            relay_text(&card(COLUMN_PLANNING, None), "maya", "ceo").contains("is being planned"),
            "planning needs its own sentence"
        );
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

    /// Issue #1861: a turn that raised a question settles `Blocked`, not
    /// `Succeeded` and not `WaitingApproval` — the operator owes an answer, not
    /// a decision.
    #[test]
    fn a_question_outranks_a_plain_success_and_an_approval() {
        assert_eq!(
            settled_run_status_with_blockers(TaskRunEnd::Completed, 0, 1),
            RunStatus::Blocked
        );
        assert_eq!(
            settled_run_status_with_blockers(TaskRunEnd::Completed, 2, 1),
            RunStatus::Blocked,
            "a turn that both asked and parked an approval is waiting on the answer first"
        );
        assert_eq!(
            settled_run_status_with_blockers(TaskRunEnd::Completed, 0, 0),
            RunStatus::Succeeded,
            "no question, no relabel"
        );
    }

    /// A failure keeps its own status even with a question outstanding: the
    /// operator has a bigger problem, and relabelling would hide why the work
    /// stopped. The same stance the approval overlay takes.
    #[test]
    fn a_failure_keeps_its_status_even_with_a_question_pending() {
        assert_eq!(
            settled_run_status_with_blockers(TaskRunEnd::Failed, 0, 1),
            RunStatus::Failed
        );
        assert_eq!(
            settled_run_status_with_blockers(TaskRunEnd::Cancelled, 0, 1),
            RunStatus::Cancelled
        );
    }

    /// The card parks either way, and the blocker ending lands it in `paused`
    /// rather than back in To-do where a bounced card and an open question
    /// would look the same.
    #[test]
    fn a_blocked_ending_lands_the_card_paused() {
        assert_eq!(landing_column(TaskRunEnd::Blocked), COLUMN_PAUSED);
        assert_eq!(run_status_for(TaskRunEnd::Blocked), RunStatus::Blocked);
    }
}
