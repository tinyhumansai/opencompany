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
use crate::harness::lifecycle;
use crate::harness::orchestrator::{self, Delegation, DelegationQueue};
use crate::ports::types::{CompanyId, CompanyRecord, OutboundMessage, TurnStep};
use crate::ports::{TaskRecord, TaskStore, generate_id, now_millis};

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
        }
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
                    // No parent (#185). `run_delegation` is only reached from an
                    // orchestrator *chat* turn: `run_task` never drains the
                    // delegation queue, and a dispatched card's responder is a desk
                    // member, which carries no delegation tools ("no sub-agent
                    // re-delegation in v1"). So no task is ever in scope here to be
                    // the parent. Lineage is written through the task API's
                    // `parentTaskId` instead; when task turns do gain delegation
                    // tools, this is the site that stamps it.
                    parent_task_id: None,
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
                // A cancel issued mid-flight discards the delegated reply — nothing
                // is relayed.
                if matches!(control.take(), Some(SteerAction::Cancel)) {
                    return Ok(DelegationOutcome::default());
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
                card.assignee = assignee.clone();
                card.note = Some(append_note(
                    card.note.as_deref(),
                    &self.orchestrator_id(),
                    &match note {
                        Some(note) => format!("assigned to {assignee} — {note}"),
                        None => format!("assigned to {assignee}"),
                    },
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
