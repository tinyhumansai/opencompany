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
use crate::ports::types::{CompanyId, CompanyRecord, OutboundMessage, TurnStep};
use crate::ports::{TaskRecord, TaskStore, generate_id, now_millis};
use crate::runtime::assignee;

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
    async fn run_steered(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        control: &SteerControl,
        chat_id: Option<&str>,
    ) -> Result<TurnOutcome>;

    /// A steerable turn WITHOUT live streaming — for a dispatched card whose
    /// steps are discarded into its note and must not leak onto the console
    /// timeline.
    async fn run_steered_background(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        control: &SteerControl,
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
/// A `spawn_task` yields nothing operator-visible (it only opens a board card).
/// A synchronous `delegate_to_desk` yields a [`DeskReply`] — the teammate's
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

/// The operator-facing result of one operator message after delegation: the
/// bubble's reply and folded step timeline, plus any standalone delegation
/// bubbles to append as sibling channel responses. None of the current
/// delegations surface a standalone bubble; the field keeps the seam open for
/// one that does.
pub(crate) struct OperatorTurn {
    pub(crate) reply: String,
    pub(crate) steps: Vec<TurnStep>,
    pub(crate) bubbles: Vec<OutboundMessage>,
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
        }
    }

    /// Scopes this runner to a dispatched card, so anything the turn spawns
    /// records that card as its parent (issue #185's `parent_task_id`).
    pub(crate) fn for_task(mut self, task_id: &str) -> Self {
        self.task = Some(task_id.to_string());
        self
    }

    /// Handles one operator message end-to-end: clear stale delegations, run the
    /// responder's turn, drain whatever it queued (capped, discarded past the
    /// cap), and — when a synchronous desk delegation answered — run exactly one
    /// CEO-relay hand-back turn whose reply replaces the operator-facing text.
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
        // Clear stale delegations so nothing leaks from a prior turn, run the
        // responder's turn (metered behind the `RunTurn` impl), then drain
        // whatever it queued.
        self.queue.clear();
        let outcome = self
            .run_turn
            .run(self.company, responder, message, chat_id)
            .await?;
        // The responder's own steps ride on the operator bubble; its reply is the
        // operator-facing text UNLESS a synchronous desk delegation runs, in which
        // case the relay turn's reply replaces it (below).
        let mut operator_steps = outcome.steps;
        let mut operator_reply = outcome.reply;
        // A `spawn_task` opens a card silently; a `delegate_to_desk` runs the desk
        // lead and hands its answer back to RELAY rather than surfacing as a
        // disconnected sibling bubble. Any future delegation that surfaces its own
        // bubble lands in `bubbles`.
        let mut bubbles = Vec::new();
        let mut desk_replies: Vec<(String, String)> = Vec::new();
        for delegation in self.queue.drain(self.max_delegations) {
            let out = self.run_delegation(delegation, chat_id).await?;
            if let Some(bubble) = out.bubble {
                bubbles.push(bubble);
            }
            if let Some(desk) = out.desk_reply {
                // Fold the teammate's activity onto the operator timeline, then
                // remember the answer to relay.
                operator_steps.extend(desk.steps);
                desk_replies.push((desk.member, desk.reply));
            }
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
            // Discard anything the relay turn queued — it can only relay, never
            // re-delegate.
            let _ = self.queue.drain(self.max_delegations);
            operator_reply = relay.reply;
            operator_steps.extend(relay.steps);
        }
        Ok(OperatorTurn {
            reply: operator_reply,
            steps: operator_steps,
            bubbles,
        })
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
                self.run_delegation(delegation, None).await?;
                continue;
            };
            // The card belongs to the first hand-off that actually PRODUCES
            // something. A hand-off whose run was cancelled produced nothing, so
            // it does not get to keep the card: it would settle `Cancelled` ->
            // `backlog` while a later hand-off that really ran had its answer
            // merely appended to the note — filing work that happened under a
            // card marked cancelled. So an empty hand-off is *provisional* and a
            // later one that answers takes the card over from it (issue #213
            // review finding 3).
            let owns_card = handoff.as_ref().is_none_or(|prior| prior.reply.is_none());
            if owns_card {
                self.hand_card_over(card, delegator, &member, instruction_of(&delegation))
                    .await?;
            }
            let outcome = self.run_delegation(delegation, None).await?;
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
        card.column = lifecycle::landing_column(TaskRunEnd::Delegated, card).to_string();
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
    /// `spawn_task` opens a backlog card through the
    /// [`TaskStore::upsert`](crate::ports::TaskStore) path the console uses and
    /// surfaces nothing extra (a missing task store is a silent no-op).
    /// `delegate_to_desk` runs a single turn on the desk's lead member and
    /// **returns its reply for the orchestrator to relay** (a [`DeskReply`]). An
    /// unknown desk (no roster-backed lead) or a cancelled run yields nothing to
    /// relay. No sub-agent re-delegation in v1: desk members carry no delegation
    /// tools, so their turns queue nothing.
    pub(crate) async fn run_delegation(
        &self,
        delegation: Delegation,
        chat_id: Option<&str>,
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
                    column: "backlog".to_string(),
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
                };
                tasks.upsert(self.company, &card).await?;
                Ok(DelegationOutcome::default())
            }
            Delegation::DelegateToDesk { desk, instruction } => {
                let Some(member) = desk_lead(self.record, &desk) else {
                    return Ok(DelegationOutcome::default());
                };
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
                let outcome = self
                    .run_turn
                    .run_steered(self.company, &member, &instruction, &control, chat_id)
                    .await?;
                // A cancel issued mid-flight discards the delegated reply —
                // nothing is relayed. Flagged as a cancellation so a caller that
                // has to explain the empty result can name the cause instead of
                // guessing at it (issue #213 review).
                if matches!(control.take(), Some(SteerAction::Cancel)) {
                    return Ok(DelegationOutcome {
                        cancelled: true,
                        ..DelegationOutcome::default()
                    });
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
                    Some(canonical) if canonical.is_empty() => {
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
