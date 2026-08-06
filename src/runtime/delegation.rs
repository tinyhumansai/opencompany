//! Brain-agnostic delegation seam (issue #176, slice 1).
//!
//! Delegation — opening board tasks, handing a turn to a desk's lead member, and
//! the CEO-relay hand-back — used to live wired directly into
//! [`HarnessBrain`](crate::harness::brain::HarnessBrain), hard-coupled to the
//! [`HarnessPool`](crate::harness::HarnessPool). This module lifts that
//! orchestration out behind the [`RunTurn`] trait so a later slice can give the
//! hosted Medulla brain the same delegation without duplicating it.
//!
//! Slice 1 is a behaviour-preserving refactor: the harness path stays
//! byte-for-byte equivalent — [`HarnessBrain`](crate::harness::brain::HarnessBrain)
//! now drives a [`DelegationRunner`] over a
//! [`HarnessRunTurn`](crate::harness::run_turn::HarnessRunTurn) (the trait impl
//! that re-attaches [`HarnessDeps`](crate::harness::HarnessDeps)).
//!
//! Compiled only under `feature = "openhuman"`: [`TurnOutcome`] and the
//! delegation queue the runner drains are harness types. Slice 2 will generalise
//! the trait's turn types so a non-harness brain can implement it.

use std::sync::Arc;

use async_trait::async_trait;

use crate::Result;
use crate::company::steer::{
    InflightEntry, InflightKind, InflightRegistry, SteerAction, SteerControl,
};
use crate::harness::TurnOutcome;
use crate::harness::lifecycle::{self, TaskRunEnd};
use crate::harness::orchestrator::{self, Delegation, DelegationQueue};
use crate::harness::policy::ApprovalRequestQueue;
use crate::harness::run_trace::RunTraceSink;
use crate::ports::tasks::COLUMN_TODO;
use crate::ports::types::{CompanyId, CompanyRecord, OutboundMessage, TurnStep};
use crate::ports::{TaskRecord, TaskStore, generate_id, now_millis};
use crate::runtime::assignee;
use crate::runtime::cycle::OPEN_WORK_ANNOTATION;

/// One agent turn, abstracted so delegation orchestration never touches the
/// harness-specific [`HarnessDeps`](crate::harness::HarnessDeps).
///
/// The three methods mirror the [`HarnessPool`](crate::harness::HarnessPool)
/// turn-runners the harness brain used inline: a streamed operator/desk turn
/// ([`run`](RunTurn::run)), a steered streamed turn
/// ([`run_steered`](RunTurn::run_steered)), and a steered, un-streamed background
/// turn ([`run_steered_background`](RunTurn::run_steered_background)). The impl
/// re-attaches whatever dependencies the concrete runtime needs; the runner only
/// ever sees a [`TurnOutcome`].
#[async_trait]
pub trait RunTurn: Send + Sync {
    /// A streamed turn on `agent_id` answering `message` in `chat_id`.
    async fn run(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        chat_id: Option<&str>,
    ) -> Result<TurnOutcome>;

    /// A streamed, operator-steerable turn (pause / cancel / redirect).
    ///
    /// `run_sink` is the dispatched attempt this turn belongs to, when it
    /// belongs to one (issue #242): a desk turn a *dispatched card* handed its
    /// work to traces into that card's run, while the same delegation reached
    /// from operator chat passes `None` and behaves exactly as before.
    async fn run_steered(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        control: &SteerControl,
        chat_id: Option<&str>,
        run_sink: Option<Arc<RunTraceSink>>,
    ) -> Result<TurnOutcome>;

    /// A steerable turn WITHOUT live streaming — for a dispatched card whose
    /// steps are discarded into its note and must not leak onto the console
    /// timeline.
    ///
    /// `run_sink` carries the same meaning as on
    /// [`run_steered`](Self::run_steered). It is *this* method the dispatched
    /// card's own turns pass a sink to, which is what makes the card's trace
    /// durable while the turn runs even though nothing is streamed.
    async fn run_steered_background(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        control: &SteerControl,
        run_sink: Option<Arc<RunTraceSink>>,
    ) -> Result<TurnOutcome>;
}

// `desk_lead` is the brain-agnostic desk-lead resolver — it moved to
// `runtime::delegation_tools` (issue #176) so the hosted path can resolve a
// desk lead without the `openhuman` feature. Re-exported here so this module's
// callers (and its tests) keep using `desk_lead(...)` unchanged.
pub(crate) use crate::runtime::delegation_tools::desk_lead;

/// The prompt for the CEO-relay hand-back turn: the operator's original message
/// plus each teammate's reply, framed so the orchestrator relays the answer back
/// as its own single, coherent response and does not delegate again.
pub(crate) fn build_relay_prompt(original: &str, desk_replies: &[(String, String)]) -> String {
    let mut prompt = format!(
        "The operator asked:\n{original}\n\nYou delegated this to your team and their reply is \
below. Relay their answer back to the operator now as your own single, coherent response — \
summarize it or pass it along. Do not delegate again; just relay what came back."
    );
    for (member, reply) in desk_replies {
        prompt.push_str(&format!("\n\n{member} replied:\n{reply}"));
    }
    prompt
}

/// The outcome of draining one queued delegation.
///
/// A `spawn_task` yields no bubble of its own — it opens a board card and
/// reports that card's id (issue #246), which is what the caller stamps onto the
/// bubble it was already sending. A synchronous `delegate_to_desk` yields a
/// [`DeskReply`] — the teammate's
/// answer captured so the orchestrator can **relay** it in a follow-up turn (the
/// CEO-relay hand-back) instead of leaving it as a disconnected sibling bubble.
/// `bubble` stays for any future delegation that surfaces its own standalone
/// message directly.
#[derive(Default)]
pub(crate) struct DelegationOutcome {
    /// A chat bubble to surface as-is (unused by the current delegations).
    pub(crate) bubble: Option<OutboundMessage>,
    /// A synchronous desk reply to relay through a second orchestrator turn.
    pub(crate) desk_reply: Option<DeskReply>,
    /// Set when an operator CANCELLED this delegation's run mid-flight, so its
    /// reply was discarded.
    ///
    /// `desk_reply: None` on its own does not mean "cancelled" —
    /// `run_delegation` also returns an empty outcome for a desk with no
    /// resolvable lead, and for every delegation that is not a hand-off. This
    /// flag carries the cancellation as a **fact** so a caller can report the
    /// cause rather than inferring one from an absence (issue #213 review).
    pub(crate) cancelled: bool,
    /// The id of the board card a `spawn_task` opened (issue #246).
    ///
    /// A `spawn_task` used to be entirely silent: it returned
    /// `DelegationOutcome::default()`, documented as surfacing nothing, so the
    /// operator got a reply with no sign that work had been opened. Reporting
    /// the id here is what lets the caller stamp it onto the bubble — and, from
    /// there, onto the journaled reply — so "a card was opened" is a fact the
    /// console can render rather than something the operator has to spot on the
    /// board.
    ///
    /// `None` for every other delegation kind, and for a `spawn_task` that
    /// found no task store to write to.
    pub(crate) spawned_task: Option<String>,
}

/// A synchronous desk-lead answer captured for the orchestrator to relay: which
/// member answered, their reply text, and their own turn steps (folded onto the
/// operator timeline so the teammate's activity stays visible on the single
/// relayed bubble).
pub(crate) struct DeskReply {
    pub(crate) member: String,
    pub(crate) reply: String,
    pub(crate) steps: Vec<TurnStep>,
}

/// Whether a drain may run the hand-offs it finds, or must drop them.
///
/// The CEO-relay turn is the one caller that must drop: a second hand-off from
/// there is the re-delegation loop the drain exists to stop. Board writes are
/// still executed — a card the relay turn opened is not a re-delegation (issue
/// #442).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HandOffs {
    /// Run them, and report what came back.
    Run,
    /// Drop them, with a log line; execute everything else.
    Drop,
}

/// What one drain of the delegation queue produced (issue #453).
///
/// Extracted so every path that runs a turn can reuse the *exact* execution
/// semantics rather than re-deriving them. Before this, only the two callers
/// that happened to contain the loop had them, and the approval re-dispatch —
/// which runs a full toolbelt turn — silently had none at all.
#[derive(Default)]
pub(crate) struct Drained {
    /// Standalone chat bubbles to surface as-is.
    pub(crate) bubbles: Vec<OutboundMessage>,
    /// Synchronous desk answers, in the order they came back. Empty when
    /// [`HandOffs::Drop`] was asked for.
    pub(crate) desk_replies: Vec<DeskReply>,
    /// The **first** board card this drain opened, matching
    /// [`OperatorTurn::spawned_task`]'s first-wins rule.
    pub(crate) spawned_task: Option<String>,
}

/// The operator-facing result of one operator message after delegation: the
/// bubble's reply and folded step timeline, plus any standalone delegation
/// bubbles to append as sibling channel responses. None of the current
/// delegations surface a standalone bubble; the field keeps the seam open for
/// one that does.
pub(crate) struct OperatorTurn {
    pub(crate) reply: String,
    pub(crate) steps: Vec<TurnStep>,
    pub(crate) bubbles: Vec<OutboundMessage>,
    /// The board card this turn opened, when it opened one (issue #246) — the
    /// **first**, if it opened several.
    ///
    /// Carried to the caller so the operator bubble can say a card was opened,
    /// which a `spawn_task` never did before: the card appeared on the board
    /// and the reply gave no sign of it.
    ///
    /// First-only because the field this ultimately lands in — the journaled
    /// `AgentReply.task_id` — is a single optional id, and widening it would
    /// break the byte-identical round-trip every already-stored reply relies
    /// on. The resulting claim is incomplete but never wrong, and the bubble's
    /// step timeline still shows every `spawn_task` the turn made.
    pub(crate) spawned_task: Option<String>,
}

/// What a **dispatched card's** turn handed off (issue #204).
///
/// Returned by [`DelegationRunner::handle_task_delegations`] when the turn
/// called `delegate_to_desk` and the desk resolved to a real teammate: that
/// teammate is now the card's assignee and has already run. `reply` is what
/// they produced.
///
/// A `TaskHandoff` with `reply: None` is only ever built from a run an operator
/// actually CANCELLED — [`DelegationOutcome::cancelled`] is the input, not the
/// absence of a reply. A hand-off that yields nothing for any *other* reason
/// reports no hand-off at all, so the delegator's own turn settles the card
/// (the same path a desk with no resolvable lead already takes) rather than the
/// card being settled as a cancellation that never happened (issue #213
/// review).
pub(crate) struct TaskHandoff {
    /// The delegate that took the card over — now its `assignee`.
    pub(crate) delegate: String,
    /// What the delegate produced. `None` means their run was cancelled
    /// mid-flight, and nothing else.
    pub(crate) reply: Option<String>,
}

/// Drives the brain-agnostic delegation orchestration over a [`RunTurn`]: run the
/// responder's turn, drain and execute whatever it queued, and — when a desk
/// answered synchronously — relay through exactly one more responder turn.
///
/// Holds only brain-agnostic handles: the company record (for desk-lead
/// resolution), the task store, the steer registry, the company id, the shared
/// delegation queue the turn pushes onto, and the per-turn delegation cap. The
/// harness-specific [`HarnessDeps`](crate::harness::HarnessDeps) is deliberately
/// absent — that is the whole point of the seam; it lives behind the [`RunTurn`]
/// impl.
pub(crate) struct DelegationRunner<'a> {
    run_turn: &'a dyn RunTurn,
    record: &'a CompanyRecord,
    tasks: Option<&'a Arc<dyn TaskStore>>,
    steer: &'a InflightRegistry,
    company: &'a CompanyId,
    queue: &'a DelegationQueue,
    max_delegations: usize,
    /// The dispatched card this drain is running inside, when it is one (issue
    /// #204). Owned rather than borrowed so a caller can hold the card mutably
    /// while the runner runs. `None` for an operator chat turn.
    task: Option<String>,
    /// The attempt row that card is running under (issue #242), so a delegate's
    /// turn traces and meters into the *same* run rather than disappearing from
    /// the record the moment the work changes hands. `None` for an operator chat
    /// turn, and for a dispatch whose run row could not be minted.
    run_sink: Option<Arc<RunTraceSink>>,
    /// The cycle's approval queue, read (never written) to tell whether a turn
    /// this runner drove parked an approval (issue #465).
    ///
    /// Only the count matters, taken either side of the turn: a card whose turn
    /// stopped at an unauthorised call has produced nothing to review, and
    /// [`settle_work_card`](Self::settle_work_card) needs to know that before it
    /// picks a landing. Optional so the ~dozen `DelegationRunner::new` sites in
    /// tests stay untouched; `None` reads as "nothing parked", which is the
    /// pre-#465 behaviour and correct for any runner that cannot park.
    approvals: Option<&'a ApprovalRequestQueue>,
}

impl<'a> DelegationRunner<'a> {
    /// Wires a runner over `run_turn` with the brain-agnostic handles it needs.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        run_turn: &'a dyn RunTurn,
        record: &'a CompanyRecord,
        tasks: Option<&'a Arc<dyn TaskStore>>,
        steer: &'a InflightRegistry,
        company: &'a CompanyId,
        queue: &'a DelegationQueue,
        max_delegations: usize,
    ) -> Self {
        Self {
            run_turn,
            record,
            tasks,
            steer,
            company,
            queue,
            max_delegations,
            task: None,
            run_sink: None,
            approvals: None,
        }
    }

    /// Wires the cycle's approval queue so a settle can tell whether the turn it
    /// is recording parked an approval (issue #465).
    ///
    /// Without it a turn whose first tool call parked settled as a plain
    /// success and its card landed in In Review — announcing a result to check
    /// on work that had never started.
    pub(crate) fn with_approvals(mut self, approvals: &'a ApprovalRequestQueue) -> Self {
        self.approvals = Some(approvals);
        self
    }

    /// How many approval requests are parked right now, or `0` when no queue is
    /// wired. Differenced across a turn to attribute parks to *that* turn.
    fn approvals_queued(&self) -> usize {
        self.approvals.map_or(0, ApprovalRequestQueue::queued)
    }

    /// Scopes this runner to a dispatched card, so anything the turn spawns
    /// records that card as its parent (issue #185's `parent_task_id`).
    pub(crate) fn for_task(mut self, task_id: &str) -> Self {
        self.task = Some(task_id.to_string());
        self
    }

    /// Scopes this runner to the card's **attempt** (issue #242), so a delegated
    /// turn's steps and spend land on the same run the dispatch opened.
    pub(crate) fn for_run(mut self, run_sink: Option<Arc<RunTraceSink>>) -> Self {
        self.run_sink = run_sink;
        self
    }

    /// Handles one operator message end-to-end: claim the delegation queue for
    /// this turn (issue #453 — the acquire also clears, so nothing stale leaks
    /// in), run the responder's turn, drain whatever it queued (capped,
    /// discarded past the cap), and — when a synchronous desk delegation
    /// answered — run exactly one CEO-relay hand-back turn whose reply replaces
    /// the operator-facing text.
    ///
    /// The relay turn must not re-delegate: its prompt is relay-only, and as a
    /// safety net the delegation queue is cleared before it and drained-and-
    /// discarded after, so anything it queues is dropped (cost bounded to one
    /// extra turn, no re-delegation loop). With no delegation the single first
    /// turn is surfaced unchanged.
    pub(crate) async fn handle_operator_message(
        &self,
        responder: &str,
        message: &str,
        chat_id: Option<&str>,
    ) -> Result<OperatorTurn> {
        // Claim the delegation queue for this turn and its drain (issue #453).
        //
        // The acquire-clear subsumes the bare `clear()` this used to open with —
        // same guarantee, that nothing a prior turn staged leaks into this one —
        // and adds the half that was missing: for the span of this claim, and
        // only for it, the delegation tools are allowed to queue at all. On every
        // exit path, including the `?`s below, `Drop` un-commits and empties, so
        // a turn that dies mid-drain cannot leave work staged for whoever runs
        // next.
        //
        // Additive beside the `approvals` handle #474 wired: they do not
        // interact — one reads a count either side of a turn, this one owns the
        // queue's write window.
        let _claim = self.queue.claim();
        // Issue #463: did the REST chat handler already card this message?
        //
        // Evaluated ONCE, here, and for one reason: this is the only place the
        // operator's OWN words are still in scope. `run_delegation` receives the
        // instruction the model wrote, which is a different sentence — a guard
        // placed there would be asking the detector about the model's prose
        // rather than about the message the handler classified. #442 put the
        // stand-down in `open_direct_work_card` only, so a recognised imperative
        // the orchestrator handed off produced the REST card AND the delegation
        // card. One message, one card — so every card-opening path below reads
        // this same answer.
        let carded_by_handler =
            crate::company::task_intent::detect_task_intent(operator_words(message)).is_some();
        // …and *which* card that is, when it is still on the board. Adopting it
        // is what carries "one message, one card" through the publish drain too:
        // the caller files a published deliverable onto `spawned_task` rather
        // than minting a second card beside this one (issue #463).
        let handler_card = match carded_by_handler {
            true => self.chat_handler_card(message).await?,
            false => None,
        };
        // Issue #442, path one: a desk lead or teammate asked DIRECTLY carries
        // no delegation tools — the card-opening tools are wired only onto the
        // orchestrator — so it has no way to open a card even if it wanted one
        // and does the only thing available: the work itself, inline, untracked.
        // Opening the card here, before their turn, is what closes that path:
        // the tracking decision stops depending on which agent answered or which
        // tools it happens to carry.
        let mut direct_card = self
            .open_direct_work_card(responder, message, chat_id, carded_by_handler)
            .await?;
        // Issue #465: sampled either side of the turn so the settle below reads
        // what *this* turn parked, not what the cycle was already holding from
        // an earlier one.
        let approvals_before = self.approvals_queued();
        let outcome = self
            .run_turn
            .run(self.company, responder, message, chat_id)
            .await?;
        let parked = self.approvals_queued().saturating_sub(approvals_before);
        // The responder's own steps ride on the operator bubble; its reply is the
        // operator-facing text UNLESS a synchronous desk delegation runs, in which
        // case the relay turn's reply replaces it (below).
        let mut operator_steps = outcome.steps;
        let mut operator_reply = outcome.reply;
        // Settle the direct-answer card from the turn that just ran. Done before
        // the delegation drain because a direct responder queues nothing — it
        // has no delegation tools — so there is no relay turn coming that could
        // change the answer this card records.
        let mut direct_card_id = None;
        if let Some(card) = direct_card.as_mut() {
            self.settle_work_card(
                card,
                responder,
                TaskRunEnd::Completed,
                parked,
                &operator_reply,
            )
            .await?;
            direct_card_id = Some(card.id.clone());
        }
        // A `spawn_task` opens a card silently; a `delegate_to_desk` runs the desk
        // lead and hands its answer back to RELAY rather than surfacing as a
        // disconnected sibling bubble. Any future delegation that surfaces its own
        // bubble lands in `bubbles`.
        let mut bubbles = Vec::new();
        let mut desk_replies: Vec<(String, String)> = Vec::new();
        // Issue #246: the first card this turn opened, which is what the
        // operator bubble reports. `get_or_insert` rather than assignment keeps
        // it the FIRST — a later spawn must not overwrite the id an earlier one
        // already claimed, or the reported card would be whichever the model
        // happened to queue last.
        //
        // The chat handler's card comes first when it opened one: it predates
        // every card this turn could open, so it IS the first card for this
        // message (issue #463). Before this it was invisible from here, which is
        // why the operator bubble linked to nothing on a recognised imperative
        // even though the board carried a card for it.
        let mut spawned_task: Option<String> = handler_card.or(direct_card_id);
        let drained = self
            .drain_and_execute(chat_id, carded_by_handler, HandOffs::Run)
            .await?;
        if let Some(id) = drained.spawned_task {
            spawned_task.get_or_insert(id);
        }
        bubbles.extend(drained.bubbles);
        for desk in drained.desk_replies {
            // Fold the teammate's activity onto the operator timeline, then
            // remember the answer to relay.
            operator_steps.extend(desk.steps);
            desk_replies.push((desk.member, desk.reply));
        }
        // CEO-relay hand-back: when a synchronous desk delegation answered, run
        // exactly ONE more responder turn whose prompt is the original message
        // plus the teammate reply, and surface THAT as the operator bubble — so
        // the orchestrator comes back with the answer in one coherent
        // conversation.
        if !desk_replies.is_empty() {
            let relay_prompt = build_relay_prompt(message, &desk_replies);
            self.queue.clear();
            let relay = self
                .run_turn
                .run(self.company, responder, &relay_prompt, chat_id)
                .await?;
            // The relay turn may only relay — a second hand-off from here is the
            // re-delegation loop this drain exists to stop, and it is dropped.
            //
            // But dropping *everything* was over-broad (issue #442): a card the
            // relay turn opened is not a re-delegation, it is the orchestrator
            // deciding — having now seen what came back — that this should be
            // tracked. Discarding that meant a card could be lost purely because
            // the turn that wanted it happened to be a relay, which is invisible
            // from the operator's side. Board writes are executed; hand-offs are
            // still dropped, so the bound stays exactly one extra turn.
            let drained = self
                .drain_and_execute(chat_id, carded_by_handler, HandOffs::Drop)
                .await?;
            if let Some(id) = drained.spawned_task {
                spawned_task.get_or_insert(id);
            }
            bubbles.extend(drained.bubbles);
            // A hand-off the relay turn's tool refused is dropped with the
            // hand-offs themselves — there is no card in scope to record it on,
            // and the drain would otherwise leak it into the next turn.
            //
            // Dropped, but not in silence. `push_refusal` exists precisely so
            // the board carries the fact independently of what the turn says
            // about it (issue #272); on this one path that independent record
            // has nowhere to go, so the log is the record.
            let refused = self.queue.drain_refusals(self.max_delegations);
            if !refused.is_empty() {
                tracing::warn!(
                    company = %self.company,
                    refused = refused.len(),
                    "[delegation] the relay turn attempted hand-offs to desks this company does not \
                     have; they are dropped with the relay's other delegations and recorded nowhere \
                     but here"
                );
            }
            operator_reply = relay.reply;
            operator_steps.extend(relay.steps);
        }
        Ok(OperatorTurn {
            reply: operator_reply,
            steps: operator_steps,
            bubbles,
            spawned_task,
        })
    }

    /// Drains the queue (capped, discarded past the cap) and executes every
    /// delegation on it, reporting what came back (issue #453).
    ///
    /// # Why this is a function
    ///
    /// It used to be a loop written out twice inside
    /// [`handle_operator_message`](Self::handle_operator_message), which meant a
    /// path that ran a turn without copying that loop drained nothing — and two
    /// production paths did exactly that. The approval re-dispatch runs a full
    /// toolbelt turn and claimed only publishes, so a `review_task` made from a
    /// re-issued call was staged, never executed, and destroyed by the next
    /// turn's clear. Extracting the loop is what lets that path reuse the *exact*
    /// execution semantics — the cap, the first-wins card id, the refusal
    /// handling — instead of a near-copy that drifts.
    ///
    /// **This is not the guarantee.** The guarantee is
    /// [`DelegationQueue::claim`]: a caller that forgets to drain gets an
    /// in-turn refusal at the tool rather than a receipt for work that will not
    /// happen. This is the convenience that makes doing it right the short path.
    ///
    /// Refused desk keys are deliberately NOT drained here — they are recorded
    /// on the card by [`handle_task_delegations`](Self::handle_task_delegations)
    /// and logged by the relay path, and those are two different treatments of
    /// one fact rather than something a shared drain can decide.
    pub(crate) async fn drain_and_execute(
        &self,
        chat_id: Option<&str>,
        carded_by_handler: bool,
        hand_offs: HandOffs,
    ) -> Result<Drained> {
        let mut drained = Drained::default();
        for delegation in self.queue.drain(self.max_delegations) {
            if hand_offs == HandOffs::Drop
                && let Delegation::DelegateToDesk { desk, .. } = &delegation
            {
                tracing::debug!(
                    company = %self.company,
                    desk = %desk,
                    "[delegation] dropped a hand-off queued by the relay turn: a relay may only \
                     relay"
                );
                continue;
            }
            let out = self
                .run_delegation(delegation, chat_id, carded_by_handler)
                .await?;
            if let Some(id) = out.spawned_task {
                drained.spawned_task.get_or_insert(id);
            }
            if let Some(bubble) = out.bubble {
                drained.bubbles.push(bubble);
            }
            if let Some(desk) = out.desk_reply {
                drained.desk_replies.push(desk);
            }
        }
        Ok(drained)
    }

    /// Drains and executes whatever a **dispatched card's** turn queued (issue
    /// #204), and reports the hand-off when one happened.
    ///
    /// Before this, `run_task` ran exactly one background turn and never
    /// touched the queue — so when the dispatched responder (the orchestrator,
    /// which carries the delegation tools) called `delegate_to_desk`, the
    /// delegation was silently dropped, the turn still returned `Ok`, and the
    /// card landed in `in_review` under the delegator with a blank assignee and
    /// no delegate ever having run.
    ///
    /// The first hand-off to a desk with a resolvable lead that **produces
    /// something** owns the card: the card is reassigned to that lead and
    /// persisted in [`COLUMN_IN_PROGRESS`](lifecycle::COLUMN_IN_PROGRESS)
    /// *before* their turn starts, so the board shows who is working it while
    /// they work it, and the caller settles the card from their output
    /// afterwards.
    ///
    /// An earlier hand-off that produced nothing (its run was cancelled
    /// mid-flight) holds the card only *provisionally* — a later hand-off that
    /// answers takes it over. Otherwise the card would settle from the
    /// cancellation while work that actually ran was merely appended to the
    /// note, filing a real deliverable under a card marked cancelled.
    ///
    /// Every other delegation — `spawn_task`, `assign_task`, `review_task`, a
    /// hand-off to a desk nobody leads, and any further hand-off once one has
    /// answered — executes for its side effect; a later hand-off's answer is
    /// appended to the note so it is recorded rather than silently discarded.
    ///
    /// `chat_id` is `None` throughout: a dispatched card has no chat thread, and
    /// stamping one would make a spawned card post back into an unrelated
    /// conversation.
    pub(crate) async fn handle_task_delegations(
        &self,
        card: &mut TaskRecord,
        delegator: &str,
    ) -> Result<Option<TaskHandoff>> {
        // A hand-off the tool refused (issue #272) never becomes a
        // `Delegation`, so it is read separately — and recorded on the card
        // before anything else, because the turn's own account of it is exactly
        // what cannot be trusted: a refused hand-off is precisely the case where
        // the reply claimed work had changed hands and the board showed
        // otherwise.
        for desk in self.queue.drain_refusals(self.max_delegations) {
            tracing::warn!(
                task_id = %card.id,
                delegator = %delegator,
                "[task] a hand-off was refused before it could be queued"
            );
            card.note = Some(append_note(
                card.note.as_deref(),
                delegator,
                &undeliverable_handoff(
                    &desk,
                    delegator,
                    "it is not a desk this company can hand work to",
                ),
            ));
        }
        let queued = self.queue.drain(self.max_delegations);
        if queued.is_empty() {
            return Ok(None);
        }
        tracing::debug!(
            task_id = %card.id,
            delegator = %delegator,
            queued = queued.len(),
            "[task] draining delegations queued by a dispatched turn"
        );
        let mut handoff: Option<TaskHandoff> = None;
        for delegation in queued {
            // Resolve the hand-off target BEFORE running it, so the card can be
            // reassigned while the delegate works. `desk_lead` is pure, so the
            // second resolution inside `run_delegation` yields the same member.
            let lead = match &delegation {
                Delegation::DelegateToDesk { desk, .. } => desk_lead(self.record, desk),
                _ => None,
            };
            let Some(member) = lead else {
                // A hand-off whose desk resolves to no lead cannot be
                // delivered. #213 settles the card under the delegator rather
                // than stranding it, which is right — but until #272 it did so
                // silently, leaving a card whose note claimed a hand-off that
                // never happened and whose assignee was the delegator, with
                // nothing on the board connecting the two. Record the
                // undeliverable hand-off on the card so the operator reads the
                // fact instead of inferring it from an absence. Every other
                // delegation kind carries no desk and is unaffected.
                let desk = desk_of(&delegation).map(str::to_string);
                // `false`: a dispatched card's drain has no operator message and
                // therefore no chat-handler card to defer to. It opens no card
                // of its own regardless — `for_task` is set, which
                // `open_work_card` refuses on first.
                self.run_delegation(delegation, None, false).await?;
                if let Some(desk) = desk {
                    card.note = Some(append_note(
                        card.note.as_deref(),
                        delegator,
                        &undeliverable_handoff(
                            &desk,
                            delegator,
                            "no desk with that id has a lead on the roster",
                        ),
                    ));
                }
                continue;
            };
            // The card belongs to the first hand-off that actually PRODUCES
            // something. A hand-off whose run was cancelled produced nothing, so
            // it does not get to keep the card: it would settle `Cancelled` ->
            // `todo` while a later hand-off that really ran had its answer
            // merely appended to the note — filing work that happened under a
            // card marked cancelled. So an empty hand-off is *provisional* and a
            // later one that answers takes the card over from it (issue #213
            // review finding 3).
            let owns_card = handoff.as_ref().is_none_or(|prior| prior.reply.is_none());
            if owns_card {
                self.hand_card_over(card, delegator, &member, instruction_of(&delegation))
                    .await?;
            }
            let outcome = self.run_delegation(delegation, None, false).await?;
            match (owns_card, outcome.desk_reply, outcome.cancelled) {
                // The delegate answered: they own the card and it settles from
                // their output.
                (true, Some(desk), _) => {
                    handoff = Some(TaskHandoff {
                        delegate: member,
                        reply: Some(desk.reply),
                    });
                }
                // An operator cancelled their run mid-flight, so it produced
                // nothing. Reported as a cancellation because `run_delegation`
                // said it was one — not because the reply is missing.
                (true, None, true) => {
                    handoff = Some(TaskHandoff {
                        delegate: member,
                        reply: None,
                    });
                }
                // Nothing produced and NOT a cancellation. `run_delegation`'s
                // only other empty exit for a hand-off is a desk with no
                // resolvable lead, which cannot be reached here — the lead
                // resolved above and `desk_lead` is pure over the same record.
                // If it ever becomes reachable, this reports *no hand-off*, so
                // the delegator's own reply settles the card exactly as an
                // unresolvable desk already does, rather than telling the
                // operator their run was cancelled when it was not.
                (true, None, false) => {}
                // A later hand-off does not take the card over, but its answer
                // is real work — record it rather than dropping it.
                (false, Some(desk), _) => {
                    card.note = Some(append_note(card.note.as_deref(), &member, &desk.reply));
                }
                (false, None, _) => {}
            }
        }
        Ok(handoff)
    }

    /// Opens the board card that tracks a piece of work, **before** the turn
    /// that does it runs (issue #442).
    ///
    /// This is the whole fix in one method: the card is opened by the runner as
    /// a structural consequence of work being handed to an agent, rather than
    /// by the model happening to reach for the card-shaped tool. Every caller
    /// that is about to run somebody's turn goes through here first, so there is
    /// no path on which work starts and the board stays empty.
    ///
    /// Returns `None` — no card, nothing to settle — in exactly five cases:
    ///
    /// * **no task store wired**, the silent no-op every task path on this seam
    ///   takes;
    /// * **already inside a dispatched card** (`for_task`), which is the card;
    ///   opening a second one would double-count one piece of work;
    /// * **the chat handler already carded this message** (`carded_by_handler`,
    ///   issue #463) — see [`handle_operator_message`](Self::handle_operator_message);
    /// * **nothing substantial was asked** — see [`is_trackable_work`]; this is
    ///   the carve-out that keeps a trivial question from minting a card;
    /// * the write failed, which propagates rather than returning `None`.
    ///
    /// The write goes through the [`TaskStore`] port rather than
    /// `CompanyRuntime::upsert_task`, so landing the card straight in
    /// [`COLUMN_IN_PROGRESS`](lifecycle::COLUMN_IN_PROGRESS) cannot re-fire the
    /// `column → in_progress` dispatch edge — the agent is already running it.
    async fn open_work_card(
        &self,
        assignee: &str,
        request: &str,
        chat_id: Option<&str>,
        carded_by_handler: bool,
    ) -> Result<Option<TaskRecord>> {
        let Some(tasks) = self.tasks else {
            return Ok(None);
        };
        if self.task.is_some() {
            return Ok(None);
        }
        // Issue #463: the REST chat handler read the operator's original words
        // and already opened a To-do card for them. One message must not become
        // two cards, whichever of the two card-opening paths below is running —
        // #442 guarded only the direct path, and a recognised imperative that
        // was handed off doubled through this one.
        if carded_by_handler {
            tracing::debug!(
                company = %self.company,
                assignee = %assignee,
                "[delegation] not opening a card: the chat handler already opened one for this \
                 message"
            );
            return Ok(None);
        }
        // What the operator actually asked for, without the open-work briefing
        // the cycle appends to a desk-addressed message. Everything below reads
        // this rather than `request`: the decision, the title, and the note —
        // a card whose title was half a listing of other cards is the same bug
        // wearing a different hat.
        let request = operator_words(request).trim();
        if !is_trackable_work(request) {
            tracing::debug!(
                company = %self.company,
                assignee = %assignee,
                "[delegation] not opening a card: nothing substantial was asked"
            );
            return Ok(None);
        }
        let card = TaskRecord {
            id: generate_id(),
            title: card_title(request),
            note: Some(append_note(None, "operator", request)),
            // The agent runs it in this turn, so the board shows it in progress
            // while that happens — the same window `hand_card_over` opens for a
            // dispatched card's delegate.
            column: lifecycle::COLUMN_IN_PROGRESS.to_string(),
            priority: "medium".to_string(),
            assignee: assignee.to_string(),
            updated_at_millis: now_millis(),
            origin_chat_id: chat_id.map(str::to_string),
            parent_task_id: None,
            output: None,
            plan: None,
        };
        tasks.upsert(self.company, &card).await?;
        tracing::debug!(
            company = %self.company,
            task_id = %card.id,
            assignee = %assignee,
            "[delegation] opened a card for work handed to an agent"
        );
        Ok(Some(card))
    }

    /// The card for a **desk lead or teammate asked directly** (issue #442,
    /// path one), or `None` when this turn is not that.
    ///
    /// The orchestrator's own chat turn is deliberately excluded. It is the
    /// operator's front door — every message arrives there, most of them are
    /// answered in a line, and tracking all of them would bury the board. What
    /// the orchestrator does with work is *hand it off*, and each hand-off opens
    /// its own card in [`run_delegation`](Self::run_delegation). A desk thread
    /// or a teammate DM is the opposite case: nothing downstream of it opens a
    /// card, because the agent answering carries no tool that could.
    ///
    /// # It defers to the card the chat handler already opened
    ///
    /// The REST chat handler runs
    /// [`detect_task_intent`](crate::company::task_intent::detect_task_intent)
    /// over the same message **before** the cycle starts, and opens a To-do card
    /// when it reads as a leading imperative ("draft the launch plan"). That is
    /// the deterministic half that already existed; #442 is about everything it
    /// does not catch. So when it has already fired, this opens nothing — one
    /// message must not become two cards.
    ///
    /// Found live: without this, three consecutive desk messages opened four
    /// cards, one of them a duplicate of the request beside it. The two
    /// detectors are deliberately not merged — they answer different questions
    /// with opposite defaults (that one asks "is this unambiguously an
    /// instruction?", this one asks "is there any reason NOT to track it?") —
    /// but exactly one of them may open the card.
    ///
    /// The stand-down itself now lives in
    /// [`open_work_card`](Self::open_work_card), reached through
    /// `carded_by_handler`, because the hand-off path needed the same guard and
    /// only [`handle_operator_message`](Self::handle_operator_message) can
    /// answer the question (issue #463).
    async fn open_direct_work_card(
        &self,
        responder: &str,
        message: &str,
        chat_id: Option<&str>,
        carded_by_handler: bool,
    ) -> Result<Option<TaskRecord>> {
        if responder == self.orchestrator_id() {
            return Ok(None);
        }
        self.open_work_card(responder, message, chat_id, carded_by_handler)
            .await
    }

    /// The To-do card the REST chat handler opened for this message, when it
    /// opened one and it is still on the board (issue #463).
    ///
    /// Only ever called once [`detect_task_intent`] has already fired, so the
    /// title it derives is byte-for-byte the one the handler wrote — the handler
    /// runs the same detector over the same words moments earlier. The match is
    /// deliberately narrow, and every clause is a property of a card **that
    /// handler** writes: To-do, no assignee, no origin chat. `list` is
    /// newest-first, so the first match is the one just written rather than a
    /// months-old card that happens to share a title.
    ///
    /// `None` when no store is wired, or when nothing matches — which is the
    /// honest answer for a handler write that failed (it is best-effort there)
    /// and for every non-REST caller of this seam, none of which have a chat
    /// handler in front of them. Callers must not read `None` as "the handler
    /// did not fire": the stand-down is keyed on the detector, not on this.
    async fn chat_handler_card(&self, message: &str) -> Result<Option<String>> {
        let Some(tasks) = self.tasks else {
            return Ok(None);
        };
        let Some(title) = crate::company::task_intent::detect_task_intent(operator_words(message))
        else {
            return Ok(None);
        };
        Ok(tasks
            .list(self.company)
            .await?
            .into_iter()
            .find(|card| {
                card.title == title
                    && card.column == COLUMN_TODO
                    && card.assignee.is_empty()
                    && card.origin_chat_id.is_none()
            })
            .map(|card| card.id))
    }

    /// Settles a card [`open_work_card`](Self::open_work_card) opened, once the
    /// turn it was tracking has ended.
    ///
    /// The landing column comes from [`lifecycle::settled_landing_column`] like
    /// every other settle on this seam, so a card opened by construction is
    /// finished by the same rule as one that came off the board: a produced
    /// answer stops in In Review for a person, a cancelled run goes back to
    /// To-do, and a run that stopped at an unauthorised call parks.
    ///
    /// `parked_approvals` is how many approvals **the turn this card is
    /// recording** left outstanding, differenced across that turn by the caller.
    /// Issue #465: this used to be [`lifecycle::landing_column`] with a
    /// hardcoded [`TaskRunEnd::Completed`], so a desk whose first tool call
    /// parked settled as a plain success — the card announced a result to review
    /// while the work had not started. The ending alone cannot see that; only
    /// the count can.
    async fn settle_work_card(
        &self,
        card: &mut TaskRecord,
        responder: &str,
        end: TaskRunEnd,
        parked_approvals: usize,
        body: &str,
    ) -> Result<()> {
        card.note = Some(append_note(
            card.note.as_deref(),
            &lifecycle::note_attribution(end, responder),
            body,
        ));
        card.column = lifecycle::settled_landing_column(end, parked_approvals).to_string();
        card.updated_at_millis = now_millis();
        if let Some(tasks) = self.tasks {
            tasks.upsert(self.company, card).await?;
        }
        tracing::debug!(
            company = %self.company,
            task_id = %card.id,
            column = %card.column,
            "[delegation] settled the card opened for this turn"
        );
        Ok(())
    }

    /// Reassigns a dispatched card to the delegate taking it over and persists
    /// it, so the board shows them working it *while* they work rather than
    /// only once they are done (issue #204).
    ///
    /// The write goes through the [`TaskStore`] port, **not**
    /// `CompanyRuntime::upsert_task`, so it cannot re-fire the
    /// `column → in_progress` dispatch edge — the card is already in
    /// `in_progress` and this only re-states it. No task store wired is a silent
    /// no-op, matching every other task path on this seam.
    async fn hand_card_over(
        &self,
        card: &mut TaskRecord,
        delegator: &str,
        member: &str,
        instruction: &str,
    ) -> Result<()> {
        card.assignee = member.to_string();
        card.note = Some(append_note(
            card.note.as_deref(),
            delegator,
            &match instruction.trim() {
                "" => format!("delegated to {member}"),
                instruction => format!("delegated to {member}: {instruction}"),
            },
        ));
        card.column = lifecycle::landing_column(TaskRunEnd::Delegated).to_string();
        card.updated_at_millis = now_millis();
        tracing::debug!(
            task_id = %card.id,
            delegate = %member,
            column = %card.column,
            "[task] card handed over to the delegate"
        );
        if let Some(tasks) = self.tasks {
            tasks.upsert(self.company, card).await?;
        }
        Ok(())
    }

    /// Executes one drained delegation.
    ///
    /// `spawn_task` opens a To-do card through the
    /// [`TaskStore::upsert`](crate::ports::TaskStore) path the console uses and
    /// **reports the card's id** so the caller can say one was opened (issue
    /// #246) — it surfaces no bubble of its own, which is a different thing
    /// from the nothing it used to surface. A missing task store is a silent
    /// no-op.
    /// `delegate_to_desk` runs a single turn on the desk's lead member and
    /// **returns its reply for the orchestrator to relay** (a [`DeskReply`]). An
    /// unknown desk (no roster-backed lead) or a cancelled run yields nothing to
    /// relay. No sub-agent re-delegation in v1: desk members carry no delegation
    /// tools, so their turns queue nothing.
    ///
    /// `carded_by_handler` is [`handle_operator_message`](Self::handle_operator_message)'s
    /// answer to "did the REST chat handler already card the operator message
    /// this drain belongs to?" (issue #463). It is threaded in rather than
    /// recomputed because the only text in scope here is the instruction the
    /// model wrote, which is not what the handler classified. A dispatched
    /// card's drain has no operator message and passes `false`.
    pub(crate) async fn run_delegation(
        &self,
        delegation: Delegation,
        chat_id: Option<&str>,
        carded_by_handler: bool,
    ) -> Result<DelegationOutcome> {
        match delegation {
            Delegation::SpawnTask {
                title,
                note,
                assignee,
            } => {
                let Some(tasks) = self.tasks else {
                    return Ok(DelegationOutcome::default());
                };
                let card = TaskRecord {
                    id: generate_id(),
                    title,
                    note,
                    column: COLUMN_TODO.to_string(),
                    priority: "medium".to_string(),
                    assignee: assignee.unwrap_or_default(),
                    updated_at_millis: now_millis(),
                    // Issue #151 §3.2: remember which conversation asked for this,
                    // so the completion can answer there instead of only landing in
                    // the note.
                    origin_chat_id: chat_id.map(str::to_string),
                    // Lineage (#185): the dispatched card whose turn queued this
                    // one, when the drain is running inside a task
                    // (`for_task`) — since #204 a dispatched turn drains the
                    // queue too, so a task IS in scope here and this is the site
                    // that stamps it. An orchestrator *chat* turn has no task in
                    // scope and still writes `None`; lineage for those is written
                    // through the task API's `parentTaskId` instead.
                    parent_task_id: self.task.clone(),
                    // Nothing has run yet, so there is no deliverable to point
                    // at (issue #339). The first successful settle stamps it.
                    output: None,
                    plan: None,
                };
                tasks.upsert(self.company, &card).await?;
                // Issue #246: report the card so the caller can surface it. The
                // id is reported only after the write succeeded, so a bubble can
                // never claim a card that is not on the board.
                Ok(DelegationOutcome {
                    spawned_task: Some(card.id),
                    ..DelegationOutcome::default()
                })
            }
            Delegation::DelegateToDesk { desk, instruction } => {
                let Some(member) = desk_lead(self.record, &desk) else {
                    // Since #272 the harness tool refuses an ungrounded target
                    // before it is ever queued, so reaching here means the desk
                    // lost its lead between the tool call and this drain (or the
                    // delegation came from a path with no tool boundary). Either
                    // way it is a hand-off that will not happen: say so in the
                    // log, and — on the task path — on the card itself.
                    tracing::warn!(
                        company = %self.company,
                        desk = %desk,
                        "[delegation] hand-off could not be delivered: no desk with that id has a \
                         lead on the roster"
                    );
                    return Ok(DelegationOutcome::default());
                };
                // Issue #442, path two: the card is opened HERE, before the desk
                // lead runs, as a consequence of work being handed off — not
                // because the model reached for `spawn_task` instead of this
                // tool. `spawn_task` and `delegate_to_desk` were both described
                // to the model as delegation and only one of them touched the
                // board, so a hand-off produced a real deliverable and left
                // nothing behind. Both now do.
                //
                // Nothing is opened when the drain is already running inside a
                // dispatched card (that card *is* the tracking, and #204 hands it
                // over to this delegate below), when the REST chat handler
                // already carded the operator message this hand-off came out of
                // (issue #463), or when the instruction is not a piece of work —
                // see `is_trackable_work`.
                let mut card = self
                    .open_work_card(&member, &instruction, chat_id, carded_by_handler)
                    .await?;
                // Register the delegated turn so an operator can CANCEL it
                // mid-flight (cancel-only in v1 — pause/redirect are rejected at
                // the route). RAII guard deregisters on every exit path.
                let guard = self.steer.register(
                    self.company,
                    InflightEntry {
                        key: generate_id(),
                        task_id: None,
                        kind: InflightKind::Delegation,
                        title: desk.clone(),
                        agent_id: member.clone(),
                        started_at_millis: now_millis(),
                        pending_action: None,
                    },
                );
                let control = guard.control().clone();
                // Issue #465, same sampling as the direct-answer path: a desk
                // delegation whose first call parks has produced nothing to
                // review either.
                let approvals_before = self.approvals_queued();
                let outcome = self
                    .run_turn
                    .run_steered(
                        self.company,
                        &member,
                        &instruction,
                        &control,
                        chat_id,
                        // Issue #242: when this drain is running inside a
                        // dispatched card, the delegate's turn is part of that
                        // card's attempt — its steps and its spend belong to the
                        // same run. `None` for a chat-path delegation.
                        self.run_sink.clone(),
                    )
                    .await?;
                let parked = self.approvals_queued().saturating_sub(approvals_before);
                // A cancel issued mid-flight discards the delegated reply —
                // nothing is relayed. Flagged as a cancellation so a caller that
                // has to explain the empty result can name the cause instead of
                // guessing at it (issue #213 review).
                if matches!(control.take(), Some(SteerAction::Cancel)) {
                    // The card this hand-off opened outlives the cancellation:
                    // settling it returns it to To-do with the cancellation on
                    // its note, so an operator sees the work was asked for and
                    // stopped rather than the card vanishing with the reply.
                    if let Some(card) = card.as_mut() {
                        self.settle_work_card(
                            card,
                            &member,
                            TaskRunEnd::Cancelled,
                            parked,
                            "the run was cancelled mid-flight",
                        )
                        .await?;
                    }
                    return Ok(DelegationOutcome {
                        cancelled: true,
                        spawned_task: card.map(|c| c.id),
                        ..DelegationOutcome::default()
                    });
                }
                if let Some(card) = card.as_mut() {
                    self.settle_work_card(
                        card,
                        &member,
                        TaskRunEnd::Completed,
                        parked,
                        &outcome.reply,
                    )
                    .await?;
                }
                // Hand the teammate's answer back to RELAY through a second
                // orchestrator turn (the CEO-relay hand-back). Their steps ride
                // along and get folded onto the relayed operator bubble.
                Ok(DelegationOutcome {
                    bubble: None,
                    desk_reply: Some(DeskReply {
                        member,
                        reply: outcome.reply,
                        steps: outcome.steps,
                    }),
                    cancelled: false,
                    // Issue #442: the hand-off's own card, reported the same way
                    // a `spawn_task` reports its card — so the operator bubble
                    // says a card was opened whichever hand-off the orchestrator
                    // chose. This is the field the console's "Card opened" chip
                    // renders from.
                    spawned_task: card.map(|c| c.id),
                })
            }
            // ── Issue #186 part b: orchestrator lifecycle authority ─────────
            //
            // Both write through the same `TaskStore` path the console uses, so
            // an orchestrator-driven change is persisted identically to an
            // operator-driven one. Neither yields anything for the cycle to
            // surface — no bubble and nothing to relay: the orchestrator is
            // mid-turn and will describe what it did in its own reply, and a
            // second voice would be it talking to itself.
            //
            // A card that has since vanished is a silent no-op, matching every
            // other task path on this seam.
            Delegation::AssignTask {
                task_id,
                assignee,
                note,
            } => {
                let Some((tasks, mut card)) = self.load_card(&task_id).await? else {
                    // Issue #453: the residual case. The tool told the model the
                    // assignment takes effect as the turn completes, the drain
                    // ran, and there was no card to write to — so the receipt
                    // promised something no store hiccup or missing wiring
                    // explains. Silent no-op is right for the *card* (there is
                    // nothing to do), and wrong for the operator, who is the
                    // only one who can tell a mistyped id from a deleted card.
                    tracing::warn!(
                        company = %self.company,
                        task_id = %task_id,
                        "[delegation] assign_task named a card that is not on the board (or no \
                         task store is wired); nothing was assigned, and the turn was told it \
                         would be"
                    );
                    return Ok(DelegationOutcome::default());
                };
                // Issue #205: the orchestrator writes this `assignee` out of an
                // LLM tool call, so it is exactly as capable of naming somebody
                // who does not exist as the operator's free-text field is. Held
                // to the same contract: an unresolvable name is not written to
                // the card at all — leaving the previous owner in place — and
                // the refusal is recorded in the orchestrator's own voice, so
                // the board neither shows a phantom owner nor loses the fact
                // that an assignment was attempted.
                let resolved = assignee::resolve(self.record, &assignee);
                let entry = match resolved.canonical() {
                    // A blank or whitespace-only `assignee` resolves to
                    // `Unassigned`, whose canonical form is `""`. Clearing the
                    // owner is the right write — unassigning a card is a real
                    // thing to ask for — but there is no name to put in the
                    // note, and `assigned to {assignee}` would record a
                    // sentence that trails off with nothing after it. Name the
                    // effect instead, so the timeline says what happened.
                    Some("") => {
                        card.assignee = String::new();
                        match note {
                            Some(note) => format!("cleared the assignee — {note}"),
                            None => "cleared the assignee".to_string(),
                        }
                    }
                    Some(canonical) => {
                        card.assignee = canonical.to_string();
                        match note {
                            Some(note) => format!("assigned to {assignee} — {note}"),
                            None => format!("assigned to {assignee}"),
                        }
                    }
                    None => format!(
                        "could not assign to {assignee}: {}",
                        resolved
                            .rejection()
                            .unwrap_or_else(|| "not on the roster".to_string())
                    ),
                };
                card.note = Some(append_note(
                    card.note.as_deref(),
                    &self.orchestrator_id(),
                    &entry,
                ));
                // The column is untouched on purpose: dispatch fires from
                // `CompanyRuntime::upsert_task`, which this port cannot reach.
                // Assignment records ownership; the board's
                // `column → in_progress` PATCH still starts the work.
                card.updated_at_millis = now_millis();
                tasks.upsert(self.company, &card).await?;
                Ok(DelegationOutcome::default())
            }
            Delegation::ReviewTask {
                task_id,
                decision,
                note,
            } => {
                let Some((tasks, mut card)) = self.load_card(&task_id).await? else {
                    // Issue #453, the same residual case one arm up and the more
                    // consequential of the two: the model has just been told the
                    // card "moves to done as this turn completes", and this is
                    // the drain completing with nothing to move. The claim
                    // guarantees the drain ran; it cannot guarantee the id names
                    // a real card.
                    tracing::warn!(
                        company = %self.company,
                        task_id = %task_id,
                        ?decision,
                        "[delegation] review_task named a card that is not on the board (or no \
                         task store is wired); the verdict was recorded nowhere, and the turn was \
                         told the card had moved"
                    );
                    return Ok(DelegationOutcome::default());
                };
                card.note = Some(append_note(
                    card.note.as_deref(),
                    &self.orchestrator_id(),
                    &lifecycle::review_note(decision, note.as_deref()),
                ));
                // `Approve` finishes the card — this is #171's `in_review →
                // done` write (PR #179) for a board-created card, which #179's
                // own origin rule cannot reach.
                card.column = lifecycle::review_landing_column(decision).to_string();
                card.updated_at_millis = now_millis();
                tasks.upsert(self.company, &card).await?;
                Ok(DelegationOutcome::default())
            }
        }
    }

    /// Loads one board card by id, with the store handle. `None` when there is
    /// no task store wired, or the card has since been deleted — both a silent
    /// no-op rather than an error (issue #186).
    async fn load_card(
        &self,
        task_id: &str,
    ) -> Result<Option<(&'a Arc<dyn TaskStore>, TaskRecord)>> {
        let Some(tasks) = self.tasks else {
            return Ok(None);
        };
        let card = tasks
            .list(self.company)
            .await?
            .into_iter()
            .find(|t| t.id == task_id);
        Ok(card.map(|card| (tasks, card)))
    }

    /// The company orchestrator's agent id — the single voice a lifecycle
    /// delegation's note is recorded under (issue #186). Mirrors
    /// `HarnessBrain::orchestrator`; on an empty roster it is the empty string,
    /// which `orchestrator_id` already tolerates.
    fn orchestrator_id(&self) -> String {
        orchestrator::orchestrator_id(&self.record.manifest.agents).unwrap_or_default()
    }
}

/// The instruction a hand-off carries, for the note that records it (issue
/// #204). Empty for every other delegation kind — callers only ask this of a
/// [`Delegation::DelegateToDesk`].
fn instruction_of(delegation: &Delegation) -> &str {
    match delegation {
        Delegation::DelegateToDesk { instruction, .. } => instruction,
        _ => "",
    }
}

/// The desk a hand-off targets, or `None` for every other delegation kind —
/// which is what distinguishes "this delegation had a target that did not
/// resolve" from "this delegation never had a target" (issue #272).
fn desk_of(delegation: &Delegation) -> Option<&str> {
    match delegation {
        Delegation::DelegateToDesk { desk, .. } => Some(desk),
        _ => None,
    }
}

/// The note recorded on a card when a hand-off could not be delivered (issue
/// #272).
///
/// Written in the delegator's voice, like every other note this seam appends,
/// and deliberately explicit about the two facts an operator otherwise has to
/// infer: nothing was handed off, and the card is still theirs. Names only the
/// desk key, the cause, and the delegator — no instruction text, no delegate
/// output.
fn undeliverable_handoff(desk: &str, delegator: &str, cause: &str) -> String {
    format!(
        "hand-off to the \"{desk}\" desk was not delivered — {cause}. Nothing was delegated; this \
card is still with {delegator}."
    )
}

/// Appends a responder-attributed result block to a card's note, preserving any
/// prior note above it (issue #186). Mirrors `harness::brain::append_result`,
/// kept local to the seam so the lifecycle arms never reach back into the brain.
fn append_note(prev: Option<&str>, responder: &str, body: &str) -> String {
    let block = format!("[{responder}] {body}");
    match prev.filter(|p| !p.is_empty()) {
        Some(p) => format!("{p}\n\n{block}"),
        None => block,
    }
}

// ---------------------------------------------------------------------------
// Is this substantial enough to be tracked? (issue #442)
// ---------------------------------------------------------------------------
//
// A card is the DEFAULT for anything substantial. That is the whole product
// promise — ask for something, watch it become work with an output you can open
// — and #442 is what happens when it is not: an agent reads a repository,
// writes the file you asked for, and the board stays empty because the work only
// ever existed as a conversation.
//
// So this is not a "should I open a card?" judgement handed to the model. It is
// a **carve-out**: everything is tracked unless there is positive evidence that
// nothing was asked for. The bias is deliberate and one-directional — a
// spurious card is visible on the board and can be dismissed in one click; a
// missing card is invisible, and every downstream station (planning, the
// prerequisite check, the gate, the settled-run mover, the deliverable link)
// hangs off it.

/// Past this many words, a request is substantial no matter what it says. A
/// genuinely trivial question is short; nothing this long is "just asking".
const TRACK_ALWAYS_WORDS: usize = 25;

/// The longest an utterance opening with small talk may run before it stops
/// being small talk. "thanks!" is chatter; "thanks — now pull together the Q3
/// numbers, the deck and the board memo" is not.
const SMALLTALK_MAX_WORDS: usize = 6;

/// Verbs that name something being **produced or changed**. Their presence is
/// decisive: whatever else the sentence is doing, it is asking for work.
///
/// Deliberately excludes words that are far more often nouns in this domain —
/// `build`, `report`, `plan`, `review`, `design`, `check`, `update`, `test` —
/// because "what's the status of the build?" is a question, not a request, and
/// a classifier that mints a card for it is the fix becoming its own bug.
const WORK_VERBS: &[&str] = &[
    "analyse",
    "analyze",
    "assemble",
    "audit",
    "author",
    "collate",
    "compile",
    "compose",
    "draft",
    "implement",
    "investigate",
    "migrate",
    "prepare",
    "produce",
    "refactor",
    "rewrite",
    "summarise",
    "summarize",
    "write",
];

/// Wh-words that open a request **to know** rather than a request to do. Only
/// the unambiguous ones: `do` / `can` / `is` open questions *and* imperatives
/// ("do the quarterly close"), so they are read as interrogative only when the
/// text actually ends in a question mark.
const INTERROGATIVE_OPENERS: &[&str] = &[
    "what", "who", "whom", "whose", "when", "where", "which", "why", "how",
];

/// Openers that mark an utterance as conversation rather than a request.
const SMALLTALK_OPENERS: &[&str] = &[
    "hi",
    "hello",
    "hey",
    "yo",
    "morning",
    "afternoon",
    "evening",
    "gm",
    "thanks",
    "thank",
    "thx",
    "ty",
    "cheers",
    "ok",
    "okay",
    "k",
    "cool",
    "great",
    "nice",
    "perfect",
    "awesome",
    "lovely",
    "yes",
    "yeah",
    "yep",
    "yup",
    "no",
    "nope",
    "sure",
    "noted",
    "understood",
    "bye",
    "sounds",
    "got",
    "haha",
    "lol",
];

/// The lowercase alphanumeric word tokens of `text`.
///
/// Splitting on every non-alphanumeric character is what makes `what's` open
/// with `what` and `modules.md` two tokens — the classifier only ever asks
/// *which words are present*, so over-splitting costs nothing and under-
/// splitting would hide an opener behind an apostrophe.
fn work_words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The operator's own words, with anything the cycle appended stripped off.
///
/// A desk-addressed operator message does not reach the brain as typed: the
/// cycle folds a briefing of the target's open cards onto the end of it
/// ([`OPEN_WORK_ANNOTATION`]) so a direct "what are you working on?" is answered
/// truthfully. Everything downstream that reasons about *what the operator
/// asked for* has to cut that off first.
///
/// Found live, not by a unit test: without this, "thanks!" in a desk thread
/// scored as a substantial request — the appended card list is long, and length
/// is evidence of substance — and opened a card. Which then lengthened the
/// briefing on the next message. Each card made the next one likelier.
///
/// Splits on the shared constant rather than a transcribed copy of it, so the
/// two sides cannot drift.
pub(crate) fn operator_words(message: &str) -> &str {
    match message.find(OPEN_WORK_ANNOTATION) {
        Some(at) => &message[..at],
        None => message,
    }
}

/// Whether `text` asks for something substantial enough that the board should
/// carry it — the single decision behind every card this seam opens by
/// construction (issue #442).
///
/// Reads as a ladder of carve-outs over a `true` default:
///
/// 1. **Nothing was said** — empty, or punctuation/emoji only. No work.
/// 2. **Long** — past [`TRACK_ALWAYS_WORDS`]. Work.
/// 3. **Names a deliverable** — any [`WORK_VERBS`] entry appears. Work.
/// 4. **A plain question** — ends in `?`, or opens with a wh-word. No work.
/// 5. **Small talk** — opens with a greeting/acknowledgement and stays short.
///    No work.
/// 6. **Anything else** — work.
///
/// Rung 3 runs before rung 4 on purpose: "can you write up the Q3 numbers?" is
/// a question in shape and a request for work in substance, and the substance
/// wins. The known cost is that a genuine question *about* a deliverable
/// ("what should I write here?") is tracked. That is the bias pointing the way
/// it was chosen to point.
pub(crate) fn is_trackable_work(text: &str) -> bool {
    let trimmed = text.trim();
    let words = work_words(trimmed);
    if words.is_empty() {
        return false;
    }
    if words.len() > TRACK_ALWAYS_WORDS {
        return true;
    }
    if words.iter().any(|w| WORK_VERBS.contains(&w.as_str())) {
        return true;
    }
    if trimmed.ends_with('?') || INTERROGATIVE_OPENERS.contains(&words[0].as_str()) {
        return false;
    }
    if words.len() <= SMALLTALK_MAX_WORDS && SMALLTALK_OPENERS.contains(&words[0].as_str()) {
        return false;
    }
    true
}

/// How many characters of a request survive into the card's title.
const TITLE_CHARS: usize = 80;

/// A one-line card title from the request that opened it.
///
/// Collapses whitespace, then truncates on a **character** boundary — never a
/// byte one — and prefers the last whole word so a title never breaks mid-word.
/// The ellipsis is budgeted inside [`TITLE_CHARS`], so the result is never
/// longer than the cap it advertises.
fn card_title(text: &str) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= TITLE_CHARS {
        return one_line;
    }
    let head: String = one_line.chars().take(TITLE_CHARS - 1).collect();
    let head = match head.rsplit_once(' ') {
        Some((whole, _)) if !whole.is_empty() => whole,
        _ => head.as_str(),
    };
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::VecDeque;
    use std::sync::Mutex;

    use crate::ports::TaskStore;
    use crate::ports::tasks::{
        COLUMN_DONE, COLUMN_IN_PROGRESS, COLUMN_IN_REVIEW, COLUMN_PAUSED, COLUMN_TODO,
    };
    use crate::ports::types::LedgerEntry;
    use crate::store::FsOps;

    // ── the substantial / trivial line (issue #442) ──────────────────────────

    /// A card is the default. Anything that asks for something to be produced,
    /// or that is too long to be "just asking", is tracked.
    #[test]
    fn a_request_for_work_is_tracked() {
        for request in [
            "Read the pricing repository and write a summary of its module layout to modules.md",
            "draft the Q3 board memo",
            "Can you write up the revenue numbers for me?",
            "investigate why the nightly job keeps timing out",
            "prepare the onboarding pack for the two new hires",
            "do the quarterly close",
            "pull the last six months of churn out of the warehouse and tell me what changed, \
             then take a view on whether the pricing move in April is the cause",
        ] {
            assert!(is_trackable_work(request), "should be tracked: {request:?}");
        }
    }

    /// The constraint that stops the fix becoming its own bug: asking a question
    /// is not commissioning work, and must not mint a card.
    #[test]
    fn a_trivial_question_is_not_tracked() {
        for question in [
            "what's our runway?",
            "What's the status of the build?",
            "who leads the engineering desk",
            "how many cards are in review?",
            "which workflows do we have?",
            "why did that fail?",
            "is the deploy done?",
        ] {
            assert!(
                !is_trackable_work(question),
                "should NOT be tracked: {question:?}"
            );
        }
    }

    /// Neither is small talk, which is most of what actually lands in a desk
    /// thread between pieces of work.
    #[test]
    fn small_talk_is_not_tracked() {
        for chatter in [
            "hi",
            "hello there",
            "hey, how's it going",
            "thanks!",
            "thank you, that's perfect",
            "ok",
            "sounds good to me",
            "got it",
            "",
            "   ",
            "👍",
        ] {
            assert!(
                !is_trackable_work(chatter),
                "should NOT be tracked: {chatter:?}"
            );
        }
    }

    /// Small talk that turns into a request stops being small talk — the opener
    /// is not a licence to skip the board for whatever follows it.
    #[test]
    fn a_greeting_in_front_of_a_request_does_not_hide_it() {
        assert!(is_trackable_work("hi — please draft the investor update"));
        assert!(is_trackable_work(
            "thanks! now write that up as a one-pager"
        ));
    }

    /// The bias is one-directional and deliberate: an unclassifiable request
    /// falls through to *tracked*, because a spurious card is visible and a
    /// missing one is not.
    #[test]
    fn an_ambiguous_request_falls_through_to_tracked() {
        assert!(is_trackable_work("look into the churn spike"));
        assert!(is_trackable_work("the quarterly numbers, by Friday"));
    }

    /// The bug live testing found and the unit tests above could not: a
    /// desk-addressed message reaches this seam with the cycle's open-work
    /// briefing already appended, so "thanks!" arrived as a long block of card
    /// titles and scored as substantial work.
    ///
    /// Self-amplifying, which is what made it worse than a stray card: every
    /// card it opened lengthened the briefing on the next message, making the
    /// next card likelier still. Three consecutive messages to one desk —
    /// including "thanks!" — opened three cards on a live host.
    ///
    /// The input here is built from the **same constant** the cycle writes, so a
    /// change to that wording fails this test rather than silently restoring the
    /// bug.
    #[test]
    fn the_cycles_open_work_briefing_is_not_the_operators_request() {
        let briefed = format!(
            "thanks!{OPEN_WORK_ANNOTATION} (answer truthfully if asked what you are working \
on):\n- Read the pricing repository and write a summary of its module layout\n- Draft the \
investor update for the quarter\n]"
        );
        assert_eq!(operator_words(&briefed), "thanks!");
        assert!(
            !is_trackable_work(operator_words(&briefed)),
            "small talk stays small talk however much context is folded onto it"
        );
        // The briefing is long and full of work verbs, so scoring the whole
        // thing is what opened the card. This pins the failure it caused.
        assert!(
            is_trackable_work(&briefed),
            "the unstripped message really does read as work — which is why the \
             strip has to happen, not merely why it is tidy"
        );
        // An un-annotated message (the orchestrator's own thread) is untouched.
        assert_eq!(operator_words("draft the memo"), "draft the memo");
    }

    /// A title never breaks a character in half (the byte-slice trap) and never
    /// exceeds the cap it advertises — the ellipsis is budgeted inside it.
    #[test]
    fn a_card_title_is_bounded_and_utf8_safe() {
        let long = "рынок ".repeat(60);
        let title = card_title(&long);
        assert!(title.chars().count() <= TITLE_CHARS, "{title}");
        assert!(title.ends_with('…'), "{title}");
        assert_eq!(card_title("  keep   it   short  "), "keep it short");
    }

    // ── harness ─────────────────────────────────────────────────────────────

    /// One scripted turn: what the agent replies, what its turn queues onto the
    /// shared delegation queue, and whether an operator cancels it mid-flight.
    #[derive(Default)]
    struct Turn {
        reply: String,
        queues: Vec<Delegation>,
        cancel: bool,
        /// Tool calls this turn tried to make and had parked for approval
        /// (issue #465), pushed onto the shared approval queue exactly as the
        /// real [`ApprovalPolicy`](crate::harness::policy::ApprovalPolicy) does.
        parks: Vec<String>,
    }

    impl Turn {
        fn reply(reply: &str) -> Self {
            Self {
                reply: reply.to_string(),
                ..Self::default()
            }
        }

        fn queueing(reply: &str, queues: Vec<Delegation>) -> Self {
            Self {
                reply: reply.to_string(),
                queues,
                ..Self::default()
            }
        }

        fn cancelled(reply: &str) -> Self {
            Self {
                reply: reply.to_string(),
                cancel: true,
                ..Self::default()
            }
        }

        /// A turn whose **first** tool call parked for approval, so it produced
        /// nothing: the reply is the agent saying it is blocked, not a result.
        /// This is the shape in the issue #465 report.
        fn parked(reply: &str, tool: &str) -> Self {
            Self {
                reply: reply.to_string(),
                parks: vec![tool.to_string()],
                ..Self::default()
            }
        }
    }

    /// A [`RunTurn`] that plays a fixed script of turns and records who was
    /// asked to run what, so a test can assert on the *sequence* of turns a
    /// drain produced without a harness pool or a live model.
    struct ScriptedTurns {
        queue: DelegationQueue,
        /// The same handle the runner reads, so a parked call is visible to the
        /// settle exactly as it is in production (issue #465).
        approvals: ApprovalRequestQueue,
        script: Mutex<VecDeque<Turn>>,
        calls: Mutex<Vec<(String, String)>>,
        /// The board as it looked at the START of each turn, so a test can prove
        /// a card existed *while* an agent worked rather than only afterwards.
        board_at_turn: Mutex<Vec<Vec<(String, String)>>>,
        /// Whether the delegation queue was **claimed** while each turn ran
        /// (issue #453). This is what a real tool reads to decide between
        /// staging and refusing, so recording it here is how a test proves the
        /// turn was entitled to delegate at all — rather than only that the
        /// drain happened to run afterwards.
        committed_at_turn: Mutex<Vec<bool>>,
        tasks: Arc<dyn TaskStore>,
        company: CompanyId,
    }

    impl ScriptedTurns {
        fn new(fx: &Fixture, turns: Vec<Turn>) -> Self {
            Self {
                queue: fx.queue.clone(),
                approvals: fx.approvals.clone(),
                script: Mutex::new(turns.into()),
                calls: Mutex::new(Vec::new()),
                board_at_turn: Mutex::new(Vec::new()),
                committed_at_turn: Mutex::new(Vec::new()),
                tasks: fx.tasks.clone(),
                company: fx.record.id.clone(),
            }
        }

        /// `(agent_id, message)` for every turn run, in order.
        fn calls(&self) -> Vec<(String, String)> {
            self.calls.lock().expect("calls").clone()
        }

        /// `(assignee, column)` for every card on the board when turn `n`
        /// started.
        fn board_at_turn(&self, n: usize) -> Vec<(String, String)> {
            self.board_at_turn.lock().expect("board")[n].clone()
        }

        /// Whether the delegation queue was claimed while turn `n` ran (issue
        /// #453).
        fn committed_at_turn(&self, n: usize) -> bool {
            self.committed_at_turn.lock().expect("committed")[n]
        }

        async fn next(
            &self,
            agent_id: &str,
            message: &str,
            control: Option<&SteerControl>,
        ) -> TurnOutcome {
            self.calls
                .lock()
                .expect("calls")
                .push((agent_id.to_string(), message.to_string()));
            let board = self
                .tasks
                .list(&self.company)
                .await
                .expect("list cards")
                .into_iter()
                .map(|c| (c.assignee, c.column))
                .collect();
            self.board_at_turn.lock().expect("board").push(board);
            self.committed_at_turn
                .lock()
                .expect("committed")
                .push(self.queue.drain_committed());
            let turn = self
                .script
                .lock()
                .expect("script")
                .pop_front()
                .unwrap_or_else(|| panic!("unscripted turn: {agent_id} <- {message}"));
            for delegation in turn.queues {
                self.queue.push(delegation);
            }
            for tool in turn.parks {
                self.approvals
                    .push(crate::harness::policy::ApprovalRequest {
                        tool: tool.clone(),
                        reason: "supervised".to_string(),
                        effect: crate::ports::types::Effect {
                            kind: tool,
                            group: crate::ports::types::EffectGroup::Other,
                            amount_usd: None,
                            established_thread: false,
                            first_time_counterparty: false,
                            payload: serde_json::json!({}),
                            agent: Some(agent_id.to_string()),
                            run_id: None,
                        },
                    });
            }
            if turn.cancel
                && let Some(control) = control
            {
                control.request(SteerAction::Cancel);
            }
            TurnOutcome {
                reply: turn.reply,
                steps: Vec::new(),
            }
        }
    }

    #[async_trait]
    impl RunTurn for ScriptedTurns {
        async fn run(
            &self,
            _company: &CompanyId,
            agent_id: &str,
            message: &str,
            _chat_id: Option<&str>,
        ) -> Result<TurnOutcome> {
            Ok(self.next(agent_id, message, None).await)
        }

        async fn run_steered(
            &self,
            _company: &CompanyId,
            agent_id: &str,
            message: &str,
            control: &SteerControl,
            _chat_id: Option<&str>,
            _run_sink: Option<Arc<RunTraceSink>>,
        ) -> Result<TurnOutcome> {
            Ok(self.next(agent_id, message, Some(control)).await)
        }

        async fn run_steered_background(
            &self,
            _company: &CompanyId,
            agent_id: &str,
            message: &str,
            control: &SteerControl,
            _run_sink: Option<Arc<RunTraceSink>>,
        ) -> Result<TurnOutcome> {
            Ok(self.next(agent_id, message, Some(control)).await)
        }
    }

    /// `chief` is the orchestrator; `engineer` leads the `eng_desk` desk.
    fn record() -> CompanyRecord {
        let manifest = toml::from_str(
            r#"
[company]
name = "Acme"

[[agent]]
id = "chief"
role = "Chief of Staff"
tier = "orchestrator"

[[agent]]
id = "engineer"
role = "Engineer"

[[group_chat]]
id = "eng_desk"
name = "Engineering"
members = ["engineer"]
"#,
        )
        .expect("valid manifest");
        CompanyRecord {
            id: CompanyId::new("acme"),
            manifest,
            ledger: Vec::<LedgerEntry>::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            template_provenance: None,
        }
    }

    /// The wired pieces one drain needs: the company record, a real task store
    /// over a temp dir, the shared queue, and the steer registry.
    struct Fixture {
        _dir: tempfile::TempDir,
        record: CompanyRecord,
        tasks: Arc<dyn TaskStore>,
        queue: DelegationQueue,
        steer: InflightRegistry,
        /// Wired into every runner, so the parked-approval overlay (issue #465)
        /// is exercised by the whole existing suite rather than only by the
        /// tests that park something.
        approvals: ApprovalRequestQueue,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            Self {
                tasks: Arc::new(FsOps::new(dir.path())) as Arc<dyn TaskStore>,
                _dir: dir,
                record: record(),
                queue: DelegationQueue::default(),
                steer: InflightRegistry::default(),
                approvals: ApprovalRequestQueue::default(),
            }
        }

        fn runner<'a>(&'a self, turns: &'a ScriptedTurns) -> DelegationRunner<'a> {
            DelegationRunner::new(
                turns,
                &self.record,
                Some(&self.tasks),
                &self.steer,
                &self.record.id,
                &self.queue,
                orchestrator::MAX_DELEGATIONS_PER_TURN,
            )
            .with_approvals(&self.approvals)
        }

        async fn cards(&self) -> Vec<TaskRecord> {
            self.tasks.list(&self.record.id).await.expect("list cards")
        }
    }

    // ── Issue #453: the receipt and the board agree ─────────────────────────

    /// The plain claim `review_task`'s receipt makes: an operator turn that
    /// approves a card actually moves it.
    ///
    /// Both halves matter and they are different facts. `committed_at_turn`
    /// proves the turn ran **under a claim**, which is what entitled the tool to
    /// stage rather than refuse; the card's column proves the drain that claim
    /// promised really executed. A test with only the second half would pass on
    /// a path that drains but never claims — which is not the invariant, because
    /// the next such path written would inherit nothing.
    #[tokio::test]
    async fn an_operator_turn_approval_actually_lands_the_card() {
        let fx = Fixture::new();
        let card = TaskRecord {
            id: "card-1".to_string(),
            title: "Draft the launch plan".to_string(),
            note: Some("[engineer] drafted".to_string()),
            column: COLUMN_IN_REVIEW.to_string(),
            priority: "medium".to_string(),
            assignee: "engineer".to_string(),
            updated_at_millis: now_millis(),
            origin_chat_id: None,
            parent_task_id: None,
            output: None,
            plan: None,
        };
        fx.tasks
            .upsert(&fx.record.id, &card)
            .await
            .expect("seed the card under review");

        let turns = ScriptedTurns::new(
            &fx,
            vec![Turn::queueing(
                "approved — it's done",
                vec![Delegation::ReviewTask {
                    task_id: "card-1".to_string(),
                    decision: lifecycle::ReviewDecision::Approve,
                    note: Some("looks good".to_string()),
                }],
            )],
        );

        fx.runner(&turns)
            .handle_operator_message("chief", "approve the launch plan card", Some("general"))
            .await
            .expect("operator message handled");

        assert!(
            turns.committed_at_turn(0),
            "the turn must run under a claim, or the tool would have refused instead of staging"
        );
        let cards = fx.cards().await;
        assert_eq!(cards.len(), 1, "{cards:?}");
        assert_eq!(
            cards[0].column, COLUMN_DONE,
            "the card the operator was told had moved must actually have moved"
        );
        assert!(
            cards[0]
                .note
                .as_deref()
                .unwrap_or_default()
                .contains("looks good"),
            "the verdict is on the card: {:?}",
            cards[0].note
        );
        assert_eq!(fx.queue.queued(), 0, "the drain emptied the queue");
        assert!(
            !fx.queue.drain_committed(),
            "and the claim released with the turn, so the next caller inherits a refusal"
        );
    }

    fn handoff(instruction: &str) -> Delegation {
        Delegation::DelegateToDesk {
            desk: "eng_desk".to_string(),
            instruction: instruction.to_string(),
        }
    }

    // ── path two: the orchestrator hands off ────────────────────────────────

    /// The crux of #442. `delegate_to_desk` ran the desk lead's turn inline and
    /// created no card at all, so a request that produced a real deliverable
    /// left the board empty. The card is now opened by the runner as a
    /// consequence of the hand-off — the model never chose it.
    #[tokio::test]
    async fn a_desk_hand_off_opens_a_card_by_construction() {
        let fx = Fixture::new();
        let turns = ScriptedTurns::new(
            &fx,
            vec![
                Turn::queueing(
                    "on it",
                    vec![handoff("Read the pricing repo and write modules.md")],
                ),
                Turn::reply("here is what engineering produced"),
                Turn::reply("relayed"),
            ],
        );

        let turn = fx
            .runner(&turns)
            .handle_operator_message("chief", "map out the pricing repo", Some("general"))
            .await
            .expect("operator message handled");

        let cards = fx.cards().await;
        assert_eq!(
            cards.len(),
            1,
            "exactly one card for one hand-off: {cards:?}"
        );
        let card = &cards[0];
        assert_eq!(
            card.assignee, "engineer",
            "the card belongs to the delegate"
        );
        assert_eq!(card.column, COLUMN_IN_REVIEW, "it settles for a person");
        assert_eq!(card.origin_chat_id.as_deref(), Some("general"));
        assert!(
            card.note
                .as_deref()
                .unwrap_or_default()
                .contains("engineer"),
            "the delegate's answer is on the card: {:?}",
            card.note
        );
        // And the operator's bubble says so, which is what renders the console's
        // "Card opened" chip — the board and the conversation agree.
        assert_eq!(turn.spawned_task.as_deref(), Some(card.id.as_str()));
    }

    /// "By construction" means the card is on the board **while** the delegate
    /// works, not reconstructed once they are done. Proven by reading the board
    /// from inside the delegate's own turn: it is already there, already theirs,
    /// already In progress.
    ///
    /// This is the assertion that distinguishes the fix from a cosmetic one —
    /// a card written only after the answer came back would satisfy every
    /// count-based test above and still leave the work invisible for the whole
    /// time it was actually happening.
    #[tokio::test]
    async fn the_hand_off_card_is_on_the_board_while_the_delegate_works() {
        let fx = Fixture::new();
        let turns = ScriptedTurns::new(&fx, vec![Turn::reply("done")]);
        let outcome = fx
            .runner(&turns)
            .run_delegation(handoff("draft the launch plan"), None, false)
            .await
            .expect("delegation runs");
        assert!(outcome.spawned_task.is_some());
        assert_eq!(turns.calls().len(), 1, "the delegate ran exactly once");
        assert_eq!(
            turns.board_at_turn(0),
            vec![("engineer".to_string(), COLUMN_IN_PROGRESS.to_string())],
            "the card is open, assigned and in progress before the delegate starts"
        );
        // …and settles for a person once they are done.
        let cards = fx.cards().await;
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].column, COLUMN_IN_REVIEW);
    }

    /// A hand-off an operator cancels mid-flight keeps its card and returns it
    /// to To-do. The alternative — no card — would erase the fact that the work
    /// was ever asked for.
    #[tokio::test]
    async fn a_cancelled_hand_off_returns_its_card_to_todo() {
        let fx = Fixture::new();
        let turns = ScriptedTurns::new(&fx, vec![Turn::cancelled("half-written")]);
        let outcome = fx
            .runner(&turns)
            .run_delegation(handoff("write the migration plan"), None, false)
            .await
            .expect("delegation runs");
        assert!(outcome.cancelled);
        let cards = fx.cards().await;
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].column, COLUMN_TODO);
    }

    /// The constraint that keeps the fix from becoming its own bug, on the
    /// hand-off path: relaying a question to a desk is not commissioning work.
    #[tokio::test]
    async fn a_question_relayed_to_a_desk_mints_no_card() {
        let fx = Fixture::new();
        let turns = ScriptedTurns::new(
            &fx,
            vec![
                Turn::queueing("asking", vec![handoff("what's the status of the build?")]),
                Turn::reply("engineering says it's green"),
                Turn::reply("it's green"),
            ],
        );
        let turn = fx
            .runner(&turns)
            .handle_operator_message("chief", "is the build ok?", None)
            .await
            .expect("operator message handled");
        assert!(fx.cards().await.is_empty(), "a question is not work");
        assert!(turn.spawned_task.is_none());
    }

    /// A hand-off made from inside a **dispatched card** must not open a second
    /// one — that card already is the tracking, and #204 hands it to the
    /// delegate.
    #[tokio::test]
    async fn a_hand_off_inside_a_dispatched_card_opens_no_second_card() {
        let fx = Fixture::new();
        let turns = ScriptedTurns::new(&fx, vec![Turn::reply("done")]);
        let outcome = fx
            .runner(&turns)
            .for_task("card-1")
            .run_delegation(handoff("write the migration plan"), None, false)
            .await
            .expect("delegation runs");
        assert!(outcome.spawned_task.is_none());
        assert!(fx.cards().await.is_empty());
    }

    // ── path one: a desk asked directly ─────────────────────────────────────

    /// Asking a desk lead directly used to be the one path with no way to reach
    /// the board at all: the card-opening tools are wired only onto the
    /// orchestrator, so the desk did the work inline and nothing tracked it.
    #[tokio::test]
    async fn a_desk_asked_directly_opens_its_own_card() {
        let fx = Fixture::new();
        let turns = ScriptedTurns::new(&fx, vec![Turn::reply("modules.md is written")]);
        let turn = fx
            .runner(&turns)
            .handle_operator_message(
                "engineer",
                "read the pricing repo and write modules.md",
                Some("eng_desk"),
            )
            .await
            .expect("operator message handled");

        // The issue's requirement is that the tracking decision is settled
        // BEFORE the work starts, so this reads the board from inside the desk's
        // own turn rather than only afterwards.
        assert_eq!(
            turns.board_at_turn(0),
            vec![("engineer".to_string(), COLUMN_IN_PROGRESS.to_string())],
            "the card is open before the desk begins working"
        );
        let cards = fx.cards().await;
        assert_eq!(cards.len(), 1, "{cards:?}");
        assert_eq!(cards[0].assignee, "engineer");
        assert_eq!(cards[0].column, COLUMN_IN_REVIEW);
        assert_eq!(cards[0].origin_chat_id.as_deref(), Some("eng_desk"));
        assert_eq!(turn.spawned_task.as_deref(), Some(cards[0].id.as_str()));
    }

    /// **Issue #465, the reported card.** A desk asked directly, whose first
    /// tool call parks for approval, produced nothing — so its card must not
    /// present as reviewable work.
    ///
    /// This path settled with a hardcoded [`TaskRunEnd::Completed`] and never
    /// consulted the approval queue, so the card landed in In Review announcing
    /// a result to check on work that had not started. It now parks, which is
    /// where the operator can see it is blocked and where the console offers the
    /// Resume that puts it back in flight once the call is authorised.
    #[tokio::test]
    async fn a_desk_whose_first_call_parks_leaves_its_card_blocked_not_reviewable() {
        let fx = Fixture::new();
        let turns = ScriptedTurns::new(
            &fx,
            vec![Turn::parked(
                "I need approval before I can read the repo",
                "fs_read",
            )],
        );
        fx.runner(&turns)
            .handle_operator_message(
                "frontend_engineer",
                "read the pricing repo and write modules.md",
                Some("eng_desk"),
            )
            .await
            .expect("operator message handled");

        let cards = fx.cards().await;
        assert_eq!(cards.len(), 1, "{cards:?}");
        assert_eq!(
            cards[0].column, COLUMN_PAUSED,
            "a turn that parked its first call produced nothing to review"
        );
        assert_ne!(
            cards[0].column, COLUMN_IN_REVIEW,
            "In Review is what a review verdict approves straight to Done — \
             unstarted work must never sit there"
        );
    }

    /// The other half of the same decision: parking is what moves the landing,
    /// not the mere presence of an approval queue. A turn that ran clean still
    /// reaches the reviewer.
    ///
    /// Paired with the test above so a fix that simply stopped writing In Review
    /// would fail here.
    #[tokio::test]
    async fn a_desk_that_finished_cleanly_still_lands_in_review() {
        let fx = Fixture::new();
        let turns = ScriptedTurns::new(&fx, vec![Turn::reply("modules.md is written")]);
        fx.runner(&turns)
            .handle_operator_message(
                "frontend_engineer",
                "read the pricing repo and write modules.md",
                Some("eng_desk"),
            )
            .await
            .expect("operator message handled");

        let cards = fx.cards().await;
        assert_eq!(cards[0].column, COLUMN_IN_REVIEW, "{cards:?}");
        assert_eq!(fx.approvals.queued(), 0, "nothing was parked");
    }

    /// An approval left over from an *earlier* turn must not park this card.
    /// The count is differenced across the turn precisely so a queue the cycle
    /// was already holding cannot be misread as something this turn did.
    #[tokio::test]
    async fn an_approval_parked_before_this_turn_does_not_park_its_card() {
        let fx = Fixture::new();
        // Something a previous turn parked and nobody has resolved yet.
        fx.approvals.push(crate::harness::policy::ApprovalRequest {
            tool: "send_email".to_string(),
            reason: "supervised".to_string(),
            effect: crate::ports::types::Effect {
                kind: "send_email".to_string(),
                group: crate::ports::types::EffectGroup::Other,
                amount_usd: None,
                established_thread: false,
                first_time_counterparty: false,
                payload: serde_json::json!({}),
                agent: Some("someone_else".to_string()),
                run_id: None,
            },
        });

        let turns = ScriptedTurns::new(&fx, vec![Turn::reply("modules.md is written")]);
        fx.runner(&turns)
            .handle_operator_message(
                "frontend_engineer",
                "read the pricing repo and write modules.md",
                Some("eng_desk"),
            )
            .await
            .expect("operator message handled");

        let cards = fx.cards().await;
        assert_eq!(
            cards[0].column, COLUMN_IN_REVIEW,
            "this turn parked nothing of its own: {cards:?}"
        );
    }

    /// A desk **hand-off** whose turn parks has the same shape as the direct
    /// path, and settles the same way — the delegate stopped at an unauthorised
    /// call, so its card is blocked rather than reviewable.
    #[tokio::test]
    async fn a_hand_off_whose_turn_parks_also_leaves_its_card_blocked() {
        let fx = Fixture::new();
        let turns = ScriptedTurns::new(
            &fx,
            vec![
                Turn::queueing("on it", vec![handoff("read the pricing repo")]),
                Turn::parked("I need approval before I can read the repo", "fs_read"),
                Turn::reply("relayed"),
            ],
        );
        fx.runner(&turns)
            .handle_operator_message("chief", "map out the pricing repo", Some("general"))
            .await
            .expect("operator message handled");

        let cards = fx.cards().await;
        assert_eq!(cards.len(), 1, "{cards:?}");
        assert_eq!(
            cards[0].column, COLUMN_PAUSED,
            "the delegate parked its first call: {cards:?}"
        );
    }

    /// One message, one card. The REST chat handler already opens a To-do card
    /// for a leading-imperative message before the cycle starts, so this path
    /// must stand down for exactly those — otherwise "draft the launch plan"
    /// lands on the board twice.
    ///
    /// Found on a live host, not here: the unit tests above all used requests
    /// the other detector is silent on, so nothing caught the overlap.
    #[tokio::test]
    async fn a_message_the_chat_handler_already_carded_opens_no_second_card() {
        // A leading imperative — `detect_task_intent` fires on this, so the REST
        // layer has already opened its card by the time the cycle runs.
        let imperative = "draft the launch plan for next quarter";
        assert!(
            crate::company::task_intent::detect_task_intent(imperative).is_some(),
            "fixture must be a message the chat handler cards, or this proves nothing"
        );
        let fx = Fixture::new();
        let turns = ScriptedTurns::new(&fx, vec![Turn::reply("planned")]);
        let turn = fx
            .runner(&turns)
            .handle_operator_message("engineer", imperative, Some("eng_desk"))
            .await
            .expect("operator message handled");
        assert!(
            fx.cards().await.is_empty(),
            "the chat handler's card is the card; this path opens none"
        );
        assert!(turn.spawned_task.is_none());
    }

    /// The same stand-down on the **hand-off** path (issue #463). #442 guarded
    /// only the direct path, so a recognised imperative the orchestrator handed
    /// off produced the handler's card AND the delegation's — measured on a live
    /// host as two cards for one message.
    ///
    /// The guard cannot live in `run_delegation`: what reaches there is the
    /// instruction the model wrote, not the operator's words, and the handler
    /// classified the latter.
    #[tokio::test]
    async fn a_hand_off_of_a_message_the_chat_handler_carded_opens_no_second_card() {
        let imperative = "draft the launch plan for next quarter";
        let title = crate::company::task_intent::detect_task_intent(imperative)
            .expect("fixture must be a message the chat handler cards");
        let fx = Fixture::new();
        // The card the REST handler wrote moments before the cycle started.
        fx.tasks
            .upsert(
                &fx.record.id,
                &TaskRecord {
                    id: "handler-card".to_string(),
                    title,
                    note: None,
                    column: COLUMN_TODO.to_string(),
                    priority: "medium".to_string(),
                    assignee: String::new(),
                    updated_at_millis: now_millis(),
                    origin_chat_id: None,
                    parent_task_id: None,
                    output: None,
                    plan: None,
                },
            )
            .await
            .expect("seed the handler's card");

        let turns = ScriptedTurns::new(
            &fx,
            vec![
                Turn::queueing("on it", vec![handoff("Draft the launch plan.")]),
                Turn::reply("drafted"),
                Turn::reply("the desk drafted it"),
            ],
        );
        let turn = fx
            .runner(&turns)
            .handle_operator_message("chief", imperative, None)
            .await
            .expect("operator message handled");

        let cards = fx.cards().await;
        assert_eq!(cards.len(), 1, "one message, one card: {cards:?}");
        assert_eq!(cards[0].id, "handler-card");
        // …and the turn ADOPTS it, which is what lets a publish later in the
        // same message file onto it instead of minting a rival beside it.
        assert_eq!(turn.spawned_task.as_deref(), Some("handler-card"));
    }

    /// The stand-down is keyed on the **detector**, not on finding the card:
    /// the handler's write is best-effort, so a missing card must not be read as
    /// "the handler did not fire" and re-open one. `spawned_task` is then
    /// honestly empty — there is no card to point at.
    #[tokio::test]
    async fn the_stand_down_holds_even_when_the_handlers_card_cannot_be_found() {
        let fx = Fixture::new();
        let turns = ScriptedTurns::new(
            &fx,
            vec![
                Turn::queueing("on it", vec![handoff("Draft the launch plan.")]),
                Turn::reply("drafted"),
                Turn::reply("relayed"),
            ],
        );
        let turn = fx
            .runner(&turns)
            .handle_operator_message("chief", "draft the launch plan for next quarter", None)
            .await
            .expect("operator message handled");
        assert!(fx.cards().await.is_empty(), "no second card is opened");
        assert!(turn.spawned_task.is_none(), "and none is claimed");
    }

    /// …and the same thread stays quiet for a question, so a desk chat does not
    /// become a card mint.
    #[tokio::test]
    async fn a_desk_asked_a_question_directly_mints_no_card() {
        let fx = Fixture::new();
        let turns = ScriptedTurns::new(&fx, vec![Turn::reply("green")]);
        let turn = fx
            .runner(&turns)
            .handle_operator_message("engineer", "is the build green?", Some("eng_desk"))
            .await
            .expect("operator message handled");
        assert!(fx.cards().await.is_empty());
        assert!(turn.spawned_task.is_none());
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

    // ── path three: the relay turn's discard ────────────────────────────────

    /// A card the relay turn opens is no longer swallowed. The relay runs after
    /// the desk answered, which is exactly when the orchestrator is best placed
    /// to decide something should be followed up — and that decision used to be
    /// dropped by design.
    #[tokio::test]
    async fn the_relay_turn_can_no_longer_lose_a_card() {
        let fx = Fixture::new();
        let turns = ScriptedTurns::new(
            &fx,
            vec![
                Turn::queueing("asking", vec![handoff("what's the status of the build?")]),
                Turn::reply("it's red — someone should fix the flaky test"),
                Turn::queueing(
                    "it's red; I've opened a card",
                    vec![Delegation::SpawnTask {
                        title: "Fix the flaky test".to_string(),
                        note: None,
                        assignee: None,
                    }],
                ),
            ],
        );
        let turn = fx
            .runner(&turns)
            .handle_operator_message("chief", "is the build ok?", None)
            .await
            .expect("operator message handled");

        let cards = fx.cards().await;
        assert_eq!(cards.len(), 1, "the relay's card survives: {cards:?}");
        assert_eq!(cards[0].title, "Fix the flaky test");
        assert_eq!(turn.spawned_task.as_deref(), Some(cards[0].id.as_str()));
    }

    /// …while the rule the discard existed for still holds: a relay may relay,
    /// never re-delegate. A hand-off queued by the relay turn is dropped, so
    /// there is no second desk turn and no loop.
    #[tokio::test]
    async fn the_relay_turn_still_cannot_re_delegate() {
        let fx = Fixture::new();
        let turns = ScriptedTurns::new(
            &fx,
            vec![
                Turn::queueing("asking", vec![handoff("what's the status of the build?")]),
                Turn::reply("green"),
                Turn::queueing("relaying", vec![handoff("now write the release notes")]),
            ],
        );
        fx.runner(&turns)
            .handle_operator_message("chief", "is the build ok?", None)
            .await
            .expect("operator message handled");

        // Three turns total — orchestrator, desk, relay. A fourth would be the
        // re-delegation the drain exists to prevent.
        let calls = turns.calls();
        assert_eq!(calls.len(), 3, "{calls:?}");
        assert_eq!(calls[2].0, "chief", "the last turn is the relay: {calls:?}");
        assert!(
            fx.cards().await.is_empty(),
            "a dropped hand-off opens no card either"
        );
    }
}
