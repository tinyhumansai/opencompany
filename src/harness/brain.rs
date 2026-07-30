//! [`HarnessBrain`]: the cognition [`Brain`] backed by the embedded OpenHuman
//! runtime.
//!
//! Where [`EchoBrain`](crate::brain::EchoBrain) turns every operator message
//! into `"You said: …"`, `HarnessBrain` routes it to a live openhuman
//! [`Agent`](openhuman_core::openhuman::agent::Agent) through a
//! [`HarnessPool`], so the reply comes from the hosted brain and the turn's
//! token/cost usage is metered into the company ledger.
//!
//! The default chat responder is the company **orchestrator** (issue #53): the
//! roster agent tagged `tier = "orchestrator"`, or the first agent when none is
//! (so a company without an orchestrator behaves exactly as before). An operator
//! message addressed to a desk (its `chat` field) is answered by that desk's
//! lead member; an unaddressed message goes to the orchestrator, which may
//! delegate — the queue its tools fill is drained here after its turn (v1:
//! synchronous, in-cycle, capped, no sub-agent re-delegation).
//!
//! Compiled only under `feature = "openhuman"`.

use std::sync::Arc;

use async_trait::async_trait;

use crate::Result;
use crate::company::steer::{InflightEntry, InflightKind, SteerAction, cap_redirect};
use crate::harness::orchestrator::{self, Delegation};
use crate::harness::{HarnessDeps, HarnessPool};

/// The most operator redirects honored within a single task dispatch (issue
/// #111). A redirect re-runs the turn in-loop with the fresh instruction
/// appended; past this cap the run is finalized to its terminal column (see
/// [`success_terminal_column`]) so a redirect storm can't loop forever.
const MAX_REDIRECTS_PER_DISPATCH: u32 = 3;

/// The board column a card lands in when the operator is the reviewer.
const IN_REVIEW: &str = "in_review";

/// The terminal board column — nothing dispatches out of it.
const DONE: &str = "done";
use crate::ports::brain::{Brain, CycleHost};
use crate::ports::types::{
    CompanyEvent, CompanyRecord, CompressedTrace, CycleRequest, CycleResult, OutboundMessage,
    TokenUsage, TurnStep, TurnStepKind, TurnStepStatus,
};
use crate::ports::{TaskRecord, generate_id, now_millis};

/// A [`Brain`] that answers with a live openhuman agent turn.
pub struct HarnessBrain {
    pool: Arc<HarnessPool>,
    deps: HarnessDeps,
    record: CompanyRecord,
    responder: String,
}

/// The outcome of draining one queued delegation.
///
/// A `spawn_task` yields nothing operator-visible (it only opens a board card).
/// A synchronous `delegate_to_desk` yields a [`DeskReply`] — the teammate's
/// answer captured so the orchestrator can **relay** it in a follow-up turn
/// (the CEO-relay hand-back) instead of leaving it as a disconnected sibling
/// bubble. `bubble` stays for any future delegation that surfaces its own
/// standalone message directly.
#[derive(Default)]
struct DelegationOutcome {
    /// A chat bubble to surface as-is (unused by the current delegations).
    bubble: Option<OutboundMessage>,
    /// A synchronous desk reply to relay through a second orchestrator turn.
    desk_reply: Option<DeskReply>,
}

/// A synchronous desk-lead answer captured for the orchestrator to relay: which
/// member answered, their reply text, and their own turn steps (folded onto the
/// operator timeline so the teammate's activity stays visible on the single
/// relayed bubble).
struct DeskReply {
    member: String,
    reply: String,
    steps: Vec<TurnStep>,
}

impl HarnessBrain {
    /// Builds a harness brain for `record`, answering unaddressed operator
    /// messages with the company orchestrator (the `tier = "orchestrator"` agent,
    /// else the first roster agent). The pool is shared so the roster is built
    /// once and reused across cycles.
    pub fn new(pool: Arc<HarnessPool>, deps: HarnessDeps, record: CompanyRecord) -> Self {
        let responder = orchestrator::orchestrator_id(&record.manifest.agents).unwrap_or_default();
        Self {
            pool,
            deps,
            record,
            responder,
        }
    }

    /// Overrides which roster agent answers operator messages.
    pub fn with_responder(mut self, agent_id: impl Into<String>) -> Self {
        self.responder = agent_id.into();
        self
    }

    /// Runs one dispatched board task: load the card, route it to its assignee
    /// (or the default responder) for a single turn, and write the outcome back
    /// onto the board — moved to its success terminal column on success (see
    /// [`success_terminal_column`]), back to `backlog` with the error noted on
    /// failure. A missing task store or a card that has since vanished is a
    /// silent no-op.
    /// Runs a dispatched card to completion and, when the card remembers the
    /// conversation it was spawned from, returns the reply to post back there
    /// (issue #151 §3.2).
    ///
    /// Before this the answer only ever reached `card.note`: the card runs
    /// asynchronously, long after the turn that spawned it has answered, so the
    /// operator had to know to go and look. The note is still written — it stays
    /// the durable record — and the post-back is additive.
    async fn run_task(&self, task_id: &str) -> Result<Option<OutboundMessage>> {
        let Some(tasks) = self.deps.tasks.as_ref() else {
            return Ok(None);
        };
        let Some(mut card) = tasks
            .list(&self.record.id)
            .await?
            .into_iter()
            .find(|t| t.id == task_id)
        else {
            return Ok(None);
        };

        let responder = self.task_responder(&card.assignee);

        // Register the run so an operator can steer it mid-flight. The guard's
        // RAII `Drop` deregisters on every exit path (success, error, redirect
        // exhaustion), so a crashed turn never leaves a ghost row in the strip.
        let guard = self.deps.steer.register(
            &self.record.id,
            InflightEntry {
                key: card.id.clone(),
                task_id: Some(card.id.clone()),
                kind: InflightKind::Task,
                title: card.title.clone(),
                agent_id: responder.clone(),
                started_at_millis: now_millis(),
                pending_action: None,
            },
        );
        let control = guard.control().clone();

        // The base turn instruction is frozen at dispatch (the card's note keeps
        // accumulating operator/agent blocks, but a redirect always re-runs from
        // the original brief plus the fresh instruction — last redirect wins).
        let base_instruction = task_instruction(&card);
        let mut instruction = base_instruction.clone();
        let mut redirects: u32 = 0;
        // The loop yields the run's operator-facing result on whichever path
        // ends it, so the completion event (#185) reports the same text that
        // lands in the card's note — never a second, divergent rendering.
        let result_text = loop {
            let outcome = self
                .pool
                // A dispatched task card carries no chat bubble (its steps are
                // discarded into the note), so its live turn frames must not leak
                // onto the console timeline — run it un-streamed (#125 review).
                .run_steered_background(
                    &self.record.id,
                    &responder,
                    &instruction,
                    &self.deps,
                    &control,
                )
                .await;
            // One-shot read of what (if anything) the operator asked for. `None`
            // is the ordinary, unsteered path.
            match control.take() {
                None => {
                    // A dispatched task discards its steps — the note is text-only.
                    match outcome {
                        Ok(outcome) => {
                            let result = outcome.reply;
                            card.note =
                                Some(append_result(card.note.as_deref(), &responder, &result));
                            let terminal = success_terminal_column(&card);
                            card.column = terminal.to_string();
                            break result;
                        }
                        Err(err) => {
                            let result = format!("dispatch failed: {err}");
                            card.note =
                                Some(append_result(card.note.as_deref(), &responder, &result));
                            card.column = "backlog".to_string();
                            break result;
                        }
                    }
                }
                Some(SteerAction::Cancel) => {
                    // Partial work is DISCARDED — only a cancellation note lands,
                    // and the card returns to `backlog`.
                    let result = "cancelled while in flight".to_string();
                    card.note = Some(append_result(card.note.as_deref(), "operator", &result));
                    card.column = "backlog".to_string();
                    break result;
                }
                Some(SteerAction::Pause) => {
                    // Partial work is PRESERVED in the note; the card parks in the
                    // `paused` column. The cycle ends normally, so the per-tenant
                    // serial lock releases while parked — resume is a plain
                    // `column → in_progress` PATCH that re-triggers dispatch.
                    let partial = match &outcome {
                        Ok(outcome) => format!("[paused] {}", outcome.reply),
                        Err(err) => format!("[paused] dispatch failed: {err}"),
                    };
                    card.note = Some(append_result(card.note.as_deref(), &responder, &partial));
                    card.column = "paused".to_string();
                    break partial;
                }
                Some(SteerAction::Redirect { instruction: fresh }) => {
                    redirects += 1;
                    card.note = Some(append_result(
                        card.note.as_deref(),
                        "operator redirect",
                        &fresh,
                    ));
                    if redirects > MAX_REDIRECTS_PER_DISPATCH {
                        // Exhausted the redirect budget — finalize the last run's
                        // reply to the card's terminal column rather than looping
                        // forever.
                        let last = match &outcome {
                            Ok(outcome) => outcome.reply.clone(),
                            Err(err) => format!("dispatch failed: {err}"),
                        };
                        card.note = Some(append_result(card.note.as_deref(), &responder, &last));
                        let terminal = success_terminal_column(&card);
                        card.column = terminal.to_string();
                        break last;
                    }
                    // Re-run from the original brief plus the (codepoint-capped)
                    // operator instruction.
                    instruction = format!(
                        "{base_instruction}\n\nOperator redirect: {}",
                        cap_redirect(&fresh)
                    );
                    continue;
                }
            }
        };

        card.updated_at_millis = now_millis();
        tasks.upsert(&self.record.id, &card).await?;
        // `guard` drops here → the run leaves the in-flight strip.
        drop(guard);

        // Issue #185: correlate this dispatch's journal trail to its card.
        //
        // Ordering matters. Any MCP failures the turn queued are drained FIRST,
        // tagged with this task, so they land on the task's own timeline. Before
        // this they were left in the queue for whichever operator turn drained
        // next — which both mis-attributed them to an unrelated chat bubble and
        // left the dispatch's timeline silent about the very calls that broke.
        //
        // The steps the drain produces are discarded, matching the rest of
        // `run_task`: a dispatched card has no chat bubble to render them on
        // (they are journaled as `McpCallFailed` events instead).
        //
        // Every write below is **best-effort**: the card was already persisted
        // above, so propagating a journal failure with `?` would abandon the
        // terminal anchor *and* the #151 post-back for a dispatch that has in
        // fact landed — leaving a timeline stuck "still running" for a card the
        // board already shows in its terminal column, and failing the whole cycle over
        // a bookkeeping write. Matches the existing journal-after-persist sites
        // (`chat_and_emit`, `WorkflowCreated`, `TaskSteered`).
        let mut discarded_steps = Vec::new();
        if let Err(err) = self
            .surface_mcp_failures(&mut discarded_steps, Some(&card.id))
            .await
        {
            tracing::warn!(
                task_id = %card.id,
                error = %err,
                "[task] failed to journal dispatch MCP failures; continuing"
            );
        }

        if let Some(events) = self.deps.events.as_ref() {
            // The run's reply, tagged so the per-task timeline can filter it out
            // of the company-scoped journal.
            //
            // `chat_id` is the **card id**, deliberately, not the card's origin
            // thread. `chat_history::owns` routes a reply into a desk's history
            // by matching `chat_id` against the desk id/name, so using the
            // origin here would inject this record into that desk's chat — a
            // behaviour change well outside a read foundation, and a duplicate
            // of the live post-back bubble below. A card id matches no desk, so
            // the record stays exactly what it is: timeline material, reachable
            // only through `task_id`. An empty string would be worse still — it
            // folds into the General desk.
            if let Err(err) = events
                .append(
                    &self.record.id,
                    CompanyEvent::AgentReply {
                        chat_id: card.id.clone(),
                        agent_id: responder.clone(),
                        text: result_text.clone(),
                        steps: Vec::new(),
                        task_id: Some(card.id.clone()),
                    },
                )
                .await
            {
                tracing::warn!(
                    task_id = %card.id,
                    error = %err,
                    "[task] failed to journal dispatch reply; continuing"
                );
            }
            // The terminal anchor, journaled after the card's landing column is
            // persisted so it always records a completed run. Attempted even if
            // the reply above failed — the anchor is what closes a timeline, so
            // dropping it is strictly worse than dropping the reply.
            if let Err(err) = events
                .append(
                    &self.record.id,
                    CompanyEvent::DeskTaskCompleted {
                        task_id: card.id.clone(),
                        desk: responder.clone(),
                        output: result_text,
                        column: card.column.clone(),
                    },
                )
                .await
            {
                tracing::warn!(
                    task_id = %card.id,
                    error = %err,
                    "[task] failed to journal task completion; continuing"
                );
            }
        }

        // Issue #151 §3.2: answer in the conversation the card was spawned
        // from. Only a card that remembers an origin posts back — one created
        // straight on the board, or written before `origin_chat_id` existed,
        // has no thread to answer in and behaves exactly as before.
        //
        // The bubble is attributed to the responder (matching a delegated desk
        // reply) and carries the card's landing column, so the operator reads
        // one line and knows both what came back and where the card went. Steps
        // are deliberately empty: a dispatched card discards them into the note.
        let Some(origin) = card.origin_chat_id.clone() else {
            return Ok(None);
        };
        Ok(Some(OutboundMessage {
            channel: responder,
            text: task_postback_text(&card),
            reply_to: Some(crate::ports::types::ReplyTo { chat_id: origin }),
            steps: Vec::new(),
        }))
    }

    /// Resolves which roster agent runs a task: its `assignee` when that names a
    /// roster member, else the brain's default responder.
    fn task_responder(&self, assignee: &str) -> String {
        if !assignee.is_empty() && self.record.manifest.agents.iter().any(|a| a.id == assignee) {
            assignee.to_string()
        } else {
            self.responder.clone()
        }
    }

    /// Resolves which agent answers an operator message.
    ///
    /// Resolution order, and the order matters:
    ///
    /// 1. a **desk** (the `chat` field naming a group chat with a lead member) is
    ///    answered by that desk's lead — unchanged;
    /// 2. a **roster teammate id** is answered by that teammate directly, which
    ///    is what makes a per-agent DM thread possible (issue #151 §3.3);
    /// 3. everything else — the "General" desk, an unknown id, an unaddressed
    ///    message — goes to the orchestrator, as before.
    ///
    /// Desks are tried first so a desk whose id happens to match an agent id
    /// keeps routing as a desk; the DM case only ever claims ids that resolve to
    /// no desk at all. Without step 2 a DM thread would silently reach the
    /// orchestrator instead of the teammate the operator opened — the console
    /// would look like it were addressing an agent while talking to someone
    /// else.
    fn responder_for(&self, chat: Option<&str>) -> String {
        let Some(chat) = chat else {
            return self.responder.clone();
        };
        if let Some(lead) = self.desk_lead(chat) {
            return lead;
        }
        if self.record.is_roster_agent(chat) {
            return chat.to_string();
        }
        self.responder.clone()
    }

    /// The lead member of a desk: the first member of the matching group chat
    /// (by id, or by case-insensitive name) that is a real roster teammate.
    /// `None` when no desk matches or none of its members are on the roster.
    ///
    /// Membership is the desk's **effective** roster — the manifest members
    /// unioned with operator-added overlay members (issue #72) — resolved through
    /// the same [`CompanyRecord::effective_desk_members`] the REST `list_desks`
    /// handler uses, so the two cannot drift. A roster teammate is a manifest
    /// agent or a team-overlay teammate, so an overlay-added lead is reachable on
    /// a desk the manifest left empty.
    fn desk_lead(&self, desk: &str) -> Option<String> {
        // Resolve the desk key (id or case-insensitive name) against both the
        // manifest desks and the operator-created overlay desks, so a
        // runtime-created desk routes exactly like a blueprint one.
        let desk_id = self.record.resolve_desk_id(desk)?;
        self.record
            .effective_desk_members(&desk_id)
            .into_iter()
            .find(|m| self.record.is_roster_agent(m))
    }

    /// Drains the MCP failure queue **onto the operator bubble's step timeline**
    /// as error steps (the Activity-trace re-skin of the error-hardening cell's
    /// original fallback bubble), and journals a scrubbed
    /// [`CompanyEvent::McpCallFailed`] audit event per failure when the event log
    /// is wired.
    ///
    /// One surface, one renderer, one scrub discipline: a silently-failed MCP
    /// call shows up as a red step in the same timeline as every other tool call
    /// instead of a separate warning bubble. Every string was already scrubbed at
    /// the source (`OcMcpCallTool`), so `scrubbed_message` is safe to show and to
    /// persist.
    ///
    /// `task_id` is the dispatched card the failing turn belonged to, when the
    /// drain runs inside a [`CompanyEvent::TaskDispatched`] cycle (issue #185).
    /// It is stamped onto each journaled failure so a task's broken tool calls
    /// can be filtered out of the company-scoped journal onto its own timeline;
    /// a chat turn passes `None` and journals exactly as before.
    async fn surface_mcp_failures(
        &self,
        steps: &mut Vec<TurnStep>,
        task_id: Option<&str>,
    ) -> Result<()> {
        for failure in self.deps.mcp_failures.drain() {
            steps.push(TurnStep {
                kind: TurnStepKind::Note,
                status: TurnStepStatus::Error,
                label: format!("MCP: {} unavailable", failure.server),
                detail: Some(failure.scrubbed_message.clone()),
                elapsed_ms: None,
            });
            if let Some(events) = self.deps.events.as_ref() {
                events
                    .append(
                        &self.record.id,
                        CompanyEvent::McpCallFailed {
                            task_id: task_id.map(str::to_string),
                            server: failure.server,
                            tool: failure.tool,
                            status: failure.status,
                            message: failure.scrubbed_message,
                        },
                    )
                    .await?;
            }
        }
        Ok(())
    }

    /// Executes one drained delegation from the orchestrator's turn.
    ///
    /// `spawn_task` opens a backlog card through the same
    /// [`TaskStore::upsert`](crate::ports::TaskStore) path the console uses and
    /// surfaces nothing extra (a missing task store is a silent no-op).
    /// `delegate_to_desk` runs a single turn on the desk's lead member and
    /// **returns its reply for the orchestrator to relay** (a [`DeskReply`]) —
    /// the CEO-relay hand-back: instead of a disconnected sibling bubble the
    /// teammate's answer feeds a second orchestrator turn so the CEO comes back
    /// with it in one coherent conversation. An unknown desk (no roster-backed
    /// lead) or a cancelled run yields nothing to relay. No sub-agent
    /// re-delegation in v1: desk members carry no delegation tools, so their
    /// turns queue nothing.
    async fn run_delegation(
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
                let Some(tasks) = self.deps.tasks.as_ref() else {
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
                    // Issue #151 §3.2: remember which conversation asked for
                    // this, so the completion can answer there instead of only
                    // landing in the note.
                    origin_chat_id: chat_id.map(str::to_string),
                    // No parent (#185). `run_delegation` is only reached from an
                    // orchestrator *chat* turn: `run_task` never drains the
                    // delegation queue, and a dispatched card's responder is a
                    // desk member, which carries no delegation tools ("no
                    // sub-agent re-delegation in v1", above). So no task is ever
                    // in scope here to be the parent. Lineage is written through
                    // the task API's `parentTaskId` instead; when task turns do
                    // gain delegation tools, this is the site that stamps it.
                    parent_task_id: None,
                };
                tasks.upsert(&self.record.id, &card).await?;
                Ok(DelegationOutcome::default())
            }
            Delegation::DelegateToDesk { desk, instruction } => {
                let Some(member) = self.desk_lead(&desk) else {
                    return Ok(DelegationOutcome::default());
                };
                // Register the delegated turn so an operator can CANCEL it
                // mid-flight (cancel-only in v1 — pause/redirect are rejected at
                // the route). RAII guard deregisters on every exit path.
                let guard = self.deps.steer.register(
                    &self.record.id,
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
                    .pool
                    .run_steered(
                        &self.record.id,
                        &member,
                        &instruction,
                        &self.deps,
                        &control,
                        chat_id,
                    )
                    .await?;
                // A cancel issued mid-flight discards the delegated reply —
                // nothing is relayed.
                if matches!(control.take(), Some(SteerAction::Cancel)) {
                    return Ok(DelegationOutcome::default());
                }
                // Hand the teammate's answer back to the caller to RELAY through
                // a second orchestrator turn (the CEO-relay hand-back). Their
                // steps ride along and get folded onto the relayed operator
                // bubble so the teammate's activity stays visible.
                Ok(DelegationOutcome {
                    bubble: None,
                    desk_reply: Some(DeskReply {
                        member,
                        reply: outcome.reply,
                        steps: outcome.steps,
                    }),
                })
            }
        }
    }
}

/// The prompt for the CEO-relay hand-back turn: the operator's original message
/// plus each teammate's reply, framed so the orchestrator relays the answer back
/// as its own single, coherent response and does not delegate again.
fn build_relay_prompt(original: &str, desk_replies: &[(String, String)]) -> String {
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

/// The turn instruction for a dispatched card: its title, plus its note when it
/// carries one, framed as a work item to act on.
fn task_instruction(card: &TaskRecord) -> String {
    match card.note.as_deref().filter(|n| !n.is_empty()) {
        Some(note) => format!("Task: {}\n\n{}", card.title, note),
        None => format!("Task: {}", card.title),
    }
}

/// Where a dispatched card lands once its run succeeds (issue #171).
///
/// `in_review` is a naming convention, not a mechanism: nothing consumes it.
/// `task_enters_in_progress` only edge-fires a dispatch when a card enters
/// `in_progress`, so an `in_review` card triggers no further cycle and the only
/// runtime write of `done` is the operator's manual drag on the board.
///
/// That is fine for a card an operator made themselves — they are the reviewer,
/// and the card is sitting in front of them. It strands a card stamped with
/// `origin_chat_id`: that card came from `spawn_task` during an agent-to-agent
/// handoff, its result was already posted back into the originating thread, and
/// no operator is watching the board for it. So a card that remembers an origin
/// completes to `done`; a board-created card still parks in `in_review`.
fn success_terminal_column(card: &TaskRecord) -> &'static str {
    if card.origin_chat_id.is_some() {
        DONE
    } else {
        IN_REVIEW
    }
}

/// The post-back line for a finished card (issue #151 §3.2): what the card was,
/// where it landed, and the reply itself.
///
/// Reads the *result* out of the card rather than taking the raw turn reply, so
/// a cancelled or paused run posts the same thing the board shows instead of
/// claiming an answer it does not have.
fn task_postback_text(card: &TaskRecord) -> String {
    let status = match card.column.as_str() {
        DONE => "is done",
        IN_REVIEW => "is ready for review",
        "paused" => "is paused",
        "backlog" => "went back to the backlog",
        other => other,
    };
    match card.note.as_deref().filter(|n| !n.trim().is_empty()) {
        Some(note) => format!("\"{}\" {status}.\n\n{note}", card.title),
        None => format!("\"{}\" {status}.", card.title),
    }
}

/// Appends a responder-attributed result block to a card's note, preserving any
/// prior note above it. Slice 1 has no first-class `TaskRecord.result` field, so
/// the outcome lives in the note.
fn append_result(prev: Option<&str>, responder: &str, body: &str) -> String {
    let block = format!("[{responder}] {body}");
    match prev.filter(|p| !p.is_empty()) {
        Some(p) => format!("{p}\n\n{block}"),
        None => block,
    }
}

#[async_trait]
impl Brain for HarnessBrain {
    async fn run_cycle(&self, req: CycleRequest, _host: &dyn CycleHost) -> Result<CycleResult> {
        // Idempotent — builds the roster on the first cycle, a no-op after.
        self.pool.ensure(&self.record, &self.deps).await?;

        let mut channel_responses = Vec::new();
        for event in &req.events {
            match event {
                CompanyEvent::OperatorMessage { text, chat, .. } => {
                    // Route to the addressed desk's lead, else the orchestrator.
                    let responder = self.responder_for(chat.as_deref());
                    // The chat/desk thread this turn answers — the same id the
                    // reply is journaled under (`AgentReply.chat_id`). Passed into
                    // the pool so the live turn-stream frames carry it and the
                    // console routes them to this thread; a delegated desk reply
                    // in this cycle rides the same operator thread, so it gets the
                    // same id.
                    let chat_id = chat.as_deref();
                    // Clear stale delegations + MCP failures so nothing leaks from
                    // a prior turn, run the turn (metered through `deps`), then
                    // drain whatever the orchestrator queued (capped; discarded
                    // past the cap).
                    self.deps.delegations.clear();
                    self.deps.mcp_failures.clear();
                    let outcome = self
                        .pool
                        .run(&self.record.id, &responder, text, &self.deps, chat_id)
                        .await?;
                    // The orchestrator's own steps ride on the operator bubble;
                    // its reply is the operator-facing text UNLESS a synchronous
                    // desk delegation runs, in which case the relay turn's reply
                    // replaces it (below).
                    let mut operator_steps = outcome.steps;
                    let mut operator_reply = outcome.reply;
                    // Run whatever the orchestrator queued. A `spawn_task` opens a
                    // card silently; a `delegate_to_desk` runs the desk lead and
                    // hands its answer back to RELAY (the CEO-relay hand-back)
                    // rather than surfacing as a disconnected sibling bubble. Any
                    // future delegation that surfaces its own bubble lands in
                    // `delegated`.
                    let mut delegated = Vec::new();
                    let mut desk_replies: Vec<(String, String)> = Vec::new();
                    for delegation in self
                        .deps
                        .delegations
                        .drain(orchestrator::MAX_DELEGATIONS_PER_TURN)
                    {
                        let out = self.run_delegation(delegation, chat_id).await?;
                        if let Some(bubble) = out.bubble {
                            delegated.push(bubble);
                        }
                        if let Some(desk) = out.desk_reply {
                            // Fold the teammate's activity onto the operator
                            // timeline, then remember the answer to relay.
                            operator_steps.extend(desk.steps);
                            desk_replies.push((desk.member, desk.reply));
                        }
                    }
                    // CEO-relay hand-back: when a synchronous desk delegation
                    // answered, run exactly ONE more orchestrator turn whose
                    // prompt is the original message plus the teammate reply,
                    // and surface THAT as the operator bubble — so the CEO comes
                    // back with the answer in one coherent conversation. The
                    // relay turn must not re-delegate: its prompt is relay-only,
                    // and as a safety net the delegation queue is cleared before
                    // it and drained-and-discarded after, so anything it tries
                    // to queue is dropped (bounding cost to one extra turn, no
                    // re-delegation loop). No delegation → the single first turn
                    // stays exactly as before.
                    if !desk_replies.is_empty() {
                        let relay_prompt = build_relay_prompt(text, &desk_replies);
                        self.deps.delegations.clear();
                        let relay = self
                            .pool
                            .run(
                                &self.record.id,
                                &responder,
                                &relay_prompt,
                                &self.deps,
                                chat_id,
                            )
                            .await?;
                        // Discard anything the relay turn queued — it can only
                        // relay, never re-delegate.
                        let _ = self
                            .deps
                            .delegations
                            .drain(orchestrator::MAX_DELEGATIONS_PER_TURN);
                        operator_reply = relay.reply;
                        operator_steps.extend(relay.steps);
                    }
                    // Re-skin any MCP tool-call failures (from the orchestrator
                    // turn, a delegated desk turn, or the relay turn) as error
                    // steps on the operator bubble — one surface, one renderer.
                    self.surface_mcp_failures(&mut operator_steps, None).await?;
                    channel_responses.push(OutboundMessage {
                        channel: "operator".to_string(),
                        text: operator_reply,
                        reply_to: None,
                        steps: operator_steps,
                    });
                    channel_responses.extend(delegated);
                }
                CompanyEvent::TaskDispatched { task_id } => {
                    if let Some(message) = self.run_task(task_id).await? {
                        channel_responses.push(message);
                    }
                }
                _ => {}
            }
        }
        // The runtime requires at least one channel response per cycle.
        if channel_responses.is_empty() {
            channel_responses.push(OutboundMessage {
                channel: "operator".to_string(),
                text: "Acknowledged.".to_string(),
                steps: Vec::new(),
                reply_to: None,
            });
        }

        let trace = CompressedTrace::now(
            req.cycle_id.clone(),
            format!("harness cycle handled {} event(s)", req.events.len()),
        );

        // No `ledger_deltas` / `token_usage` here on purpose: `HarnessPool::run`
        // is the single cost-accounting site (it writes the ledger entry and the
        // usage sample through `deps`), so surfacing the same spend again would
        // double-count it.
        Ok(CycleResult {
            channel_responses,
            new_traces: vec![trace],
            ledger_deltas: Vec::new(),
            token_usage: TokenUsage::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tinyagents::harness::message::Message;
    use tinyagents::harness::model::{ChatModel, ModelRequest, ModelResponse};

    use crate::harness::provider::{HarnessModel, MockProvider};
    use crate::ports::brain::CycleHost;
    use crate::ports::types::{
        CompanyId, ContextOp, ContextOpResult, Effect, EffectDisposition, ToolCall, ToolResult,
    };
    use crate::store::{FsCompanyStore, FsContextStore, FsOps};

    /// A `CycleHost` that auto-executes anything the brain asks for; the harness
    /// brain v1 makes no host calls, so it stays inert.
    #[derive(Default)]
    struct NoopHost;

    #[async_trait]
    impl CycleHost for NoopHost {
        async fn call_tool(&self, _call: ToolCall) -> Result<ToolResult> {
            Ok(ToolResult {
                ok: true,
                output: serde_json::Value::Null,
            })
        }
        async fn context_op(&self, _op: ContextOp) -> Result<ContextOpResult> {
            Ok(ContextOpResult::Text(String::new()))
        }
        async fn emit_effect(&self, _effect: Effect) -> Result<EffectDisposition> {
            Ok(EffectDisposition::Executed)
        }
    }

    fn record() -> CompanyRecord {
        let manifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "ceo"
role = "Chief Executive"
description = "Runs Acme."
"#,
        )
        .expect("valid manifest");
        CompanyRecord {
            id: CompanyId::new("acme"),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            template_provenance: None,
        }
    }

    fn brain_over_mock(dir: &std::path::Path) -> HarnessBrain {
        let deps = HarnessDeps {
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            context: Arc::new(FsContextStore::new(dir)),
            store: Arc::new(FsCompanyStore::new(dir)),
            meter: Some(Arc::new(FsOps::new(dir))),
            workspace_root: dir.to_path_buf(),
            model_override: None,
            tasks: None,
            skills: None,
            skills_source_dir: None,
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: orchestrator::DelegationQueue::default(),
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            steer: crate::company::steer::InflightRegistry::default(),
        };
        HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record())
    }

    fn request(events: Vec<CompanyEvent>) -> CycleRequest {
        CycleRequest {
            cycle_id: "cycle-1".to_string(),
            company_id: CompanyId::new("acme"),
            events,
            event_seqs: Vec::new(),
            compressed_history: Vec::new(),
            roster: Vec::new(),
            context_index: Vec::new(),
        }
    }

    #[tokio::test]
    async fn operator_message_gets_an_agent_reply() {
        let dir = tempfile::tempdir().unwrap();
        let brain = brain_over_mock(dir.path());
        let result = brain
            .run_cycle(
                request(vec![CompanyEvent::OperatorMessage {
                    text: "status?".into(),
                    by: None,
                    chat: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        assert_eq!(result.channel_responses.len(), 1);
        assert_eq!(result.channel_responses[0].channel, "operator");
        // The mock provider prefixes the routed message, proving the turn ran
        // through the openhuman agent rather than an echo.
        assert!(
            result.channel_responses[0].text.contains("status?"),
            "{:?}",
            result.channel_responses[0].text
        );
        // The offline mock runs no tools and emits no progress, so the operator
        // bubble carries zero steps — the tell that distinguishes a tool-less
        // (here, memory/echo-style) answer from a tool-backed one.
        assert!(
            result.channel_responses[0].steps.is_empty(),
            "a tool-less turn carries no steps: {:?}",
            result.channel_responses[0].steps
        );
        assert_eq!(result.new_traces.len(), 1);
        // Single cost-accounting site: the cycle result carries no ledger delta.
        assert!(result.ledger_deltas.is_empty());
    }

    #[tokio::test]
    async fn no_events_still_acknowledges() {
        let dir = tempfile::tempdir().unwrap();
        let brain = brain_over_mock(dir.path());
        let result = brain
            .run_cycle(request(Vec::new()), &NoopHost)
            .await
            .expect("cycle runs");
        assert_eq!(result.channel_responses.len(), 1);
        assert_eq!(result.channel_responses[0].text, "Acknowledged.");
    }

    #[test]
    fn responder_defaults_to_first_roster_agent() {
        let dir = tempfile::tempdir().unwrap();
        let brain = brain_over_mock(dir.path());
        assert_eq!(brain.responder, "ceo");
        let brain = brain.with_responder("cfo");
        assert_eq!(brain.responder, "cfo");
    }

    // --- Task dispatch ------------------------------------------------------

    use crate::ports::TaskStore;

    /// A two-agent record so assignee routing has somewhere to route.
    fn record_two() -> CompanyRecord {
        let manifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "ceo"
role = "Chief Executive"
description = "Runs Acme."

[[agent]]
id = "engineer"
role = "Engineer"
description = "Builds it."
"#,
        )
        .expect("valid manifest");
        CompanyRecord {
            id: CompanyId::new("acme"),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            template_provenance: None,
        }
    }

    /// A brain wired to a real task store (shared handle returned for seeding /
    /// asserting), over the offline mock provider.
    fn brain_with_tasks(dir: &std::path::Path) -> (HarnessBrain, Arc<FsOps>) {
        let tasks = Arc::new(FsOps::new(dir));
        let deps = HarnessDeps {
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            context: Arc::new(FsContextStore::new(dir)),
            store: Arc::new(FsCompanyStore::new(dir)),
            meter: Some(Arc::new(FsOps::new(dir))),
            workspace_root: dir.to_path_buf(),
            model_override: None,
            tasks: Some(tasks.clone()),
            skills: None,
            skills_source_dir: None,
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: orchestrator::DelegationQueue::default(),
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            steer: crate::company::steer::InflightRegistry::default(),
        };
        (
            HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record_two()),
            tasks,
        )
    }

    fn card(id: &str, assignee: &str) -> TaskRecord {
        TaskRecord {
            id: id.to_string(),
            title: "Ship the thing".to_string(),
            note: None,
            column: "in_progress".to_string(),
            priority: "high".to_string(),
            assignee: assignee.to_string(),
            updated_at_millis: 0,
            origin_chat_id: None,
            parent_task_id: None,
        }
    }

    // ── Issue #151 §3.2: a finished card answers where it was asked ──────

    /// The post-back names the card, where it landed, and the result — so the
    /// operator reads one bubble instead of going to find the note.
    #[test]
    fn postback_reports_the_card_title_status_and_result() {
        let mut finished = card("t1", "maya");
        finished.column = "in_review".to_string();
        finished.note = Some("[maya] Draft is up.".to_string());

        let text = task_postback_text(&finished);
        assert!(text.contains("Ship the thing"), "{text}");
        assert!(text.contains("is ready for review"), "{text}");
        assert!(text.contains("Draft is up."), "{text}");
    }

    /// A cancelled or paused run must not claim an answer it does not have —
    /// the post-back reads the card, not the raw turn reply.
    #[test]
    fn postback_reflects_the_landing_column_not_a_presumed_success() {
        let mut paused = card("t1", "maya");
        paused.column = "paused".to_string();
        paused.note = Some("[maya] [paused] halfway".to_string());
        assert!(task_postback_text(&paused).contains("is paused"));

        let mut cancelled = card("t1", "maya");
        cancelled.column = "backlog".to_string();
        cancelled.note = Some("[operator] cancelled while in flight".to_string());
        let text = task_postback_text(&cancelled);
        assert!(text.contains("went back to the backlog"), "{text}");
        assert!(!text.contains("ready for review"), "{text}");
    }

    /// A card with no note still posts a readable line rather than a title
    /// followed by a dangling blank block.
    #[test]
    fn postback_without_a_note_is_still_a_complete_sentence() {
        let mut finished = card("t1", "maya");
        finished.column = "in_review".to_string();
        finished.note = None;
        let text = task_postback_text(&finished);
        assert_eq!(text, "\"Ship the thing\" is ready for review.");

        finished.note = Some("   ".to_string());
        assert_eq!(
            task_postback_text(&finished),
            "\"Ship the thing\" is ready for review.",
            "a whitespace-only note must not append an empty block"
        );
    }

    /// The compatibility guarantee: a card with no remembered origin — one made
    /// straight on the board, or written before `origin_chat_id` existed —
    /// posts back nowhere and behaves exactly as it did before.
    #[tokio::test]
    async fn a_card_with_no_origin_posts_back_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        let mut c = card("t-no-origin", "maya");
        c.origin_chat_id = None;
        tasks
            .upsert(&CompanyId::new("acme"), &c)
            .await
            .expect("seed");

        let posted = brain.run_task("t-no-origin").await.expect("run");
        assert!(
            posted.is_none(),
            "a card with no originating thread must not post back"
        );
        // The note is still the durable record.
        assert!(only_card(&tasks).await.note.is_some());
    }

    /// …and one that does remember its origin answers there, attributed to the
    /// responder and threaded with `reply_to`.
    #[tokio::test]
    async fn a_card_with_an_origin_posts_back_to_that_thread() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        let mut c = card("t-origin", "maya");
        c.origin_chat_id = Some("strategy".to_string());
        tasks
            .upsert(&CompanyId::new("acme"), &c)
            .await
            .expect("seed");

        let posted = brain
            .run_task("t-origin")
            .await
            .expect("run")
            .expect("a card with an origin must post back");
        assert_eq!(
            posted.reply_to.as_ref().map(|r| r.chat_id.as_str()),
            Some("strategy")
        );
        assert!(
            !posted.channel.is_empty(),
            "the bubble must be attributed to the responder"
        );
        assert!(posted.text.contains("Ship the thing"), "{}", posted.text);
        // A dispatched card discards its steps into the note.
        assert!(posted.steps.is_empty());
    }

    async fn only_card(tasks: &Arc<FsOps>) -> TaskRecord {
        tasks
            .list(&CompanyId::new("acme"))
            .await
            .expect("list")
            .into_iter()
            .next()
            .expect("one card")
    }

    /// A dispatched **board-created** card (no `origin_chat_id`) runs a turn and
    /// moves to `in_review` — the operator who made it is the reviewer — with
    /// its result folded into the note under the responder that ran it.
    #[tokio::test]
    async fn task_dispatch_runs_and_moves_to_in_review() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        tasks
            .upsert(&CompanyId::new("acme"), &card("t1", ""))
            .await
            .unwrap();

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t1".into(),
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        let moved = only_card(&tasks).await;
        assert_eq!(moved.column, "in_review");
        let note = moved.note.expect("result written to note");
        // Default responder (first roster agent) ran it, and the mock provider
        // echoes the instruction (the card title) back into the reply.
        assert!(note.contains("[ceo]"), "{note:?}");
        assert!(note.contains("Ship the thing"), "{note:?}");
    }

    // ── Issue #171: a delegated handoff reaches `done` on its own ─────────

    /// The regression: a card spawned by a delegating turn (so it carries an
    /// `origin_chat_id`) has no operator watching the board, so leaving it in
    /// `in_review` stranded it forever. It must complete to `done`.
    #[tokio::test]
    async fn dispatched_card_with_an_origin_completes_to_done() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        let mut c = card("t-origin", "maya");
        c.origin_chat_id = Some("strategy".to_string());
        tasks
            .upsert(&CompanyId::new("acme"), &c)
            .await
            .expect("seed");

        let posted = brain
            .run_task("t-origin")
            .await
            .expect("run")
            .expect("a card with an origin posts back");

        let moved = only_card(&tasks).await;
        assert_eq!(
            moved.column, "done",
            "a delegated handoff must reach the terminal column, not park in in_review"
        );
        // The note stays the durable record of what came back.
        assert!(moved.note.expect("note").contains("Ship the thing"));
        // …and the bubble says so rather than asking for a review nobody will do.
        assert!(posted.text.contains("is done"), "{}", posted.text);
        assert!(!posted.text.contains("ready for review"), "{}", posted.text);
    }

    /// The success terminal is chosen by origin, not by outcome: a board-created
    /// card keeps its `in_review` review gate.
    #[test]
    fn success_terminal_column_is_done_only_for_a_card_with_an_origin() {
        let board_card = card("t1", "maya");
        assert_eq!(success_terminal_column(&board_card), "in_review");

        let mut delegated = card("t2", "maya");
        delegated.origin_chat_id = Some("strategy".to_string());
        assert_eq!(success_terminal_column(&delegated), "done");
    }

    /// The redirect-cap finalize branch is the other success terminal, so it has
    /// to make the same choice — otherwise a steered handoff still strands.
    #[tokio::test]
    async fn redirect_cap_finalizes_a_card_with_an_origin_to_done() {
        let dir = tempfile::tempdir().unwrap();
        let redirect = || SteerAction::Redirect {
            instruction: "focus on the API".to_string(),
        };
        let (brain, tasks, _provider) = brain_that_steers_itself(
            dir.path(),
            "t1",
            vec![redirect(), redirect(), redirect(), redirect()],
        );
        let mut c = card("t1", "");
        c.origin_chat_id = Some("strategy".to_string());
        tasks.upsert(&CompanyId::new("acme"), &c).await.unwrap();

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t1".into(),
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        assert_eq!(only_card(&tasks).await.column, "done");
    }

    /// The post-back has to have wording for the new landing column — without it
    /// the fallback arm renders the raw column id into the sentence.
    #[test]
    fn postback_reads_naturally_for_a_done_card() {
        let mut finished = card("t1", "maya");
        finished.column = "done".to_string();
        finished.note = None;
        assert_eq!(task_postback_text(&finished), "\"Ship the thing\" is done.");
    }

    /// An `assignee` that names a roster member routes the turn to that member.
    #[tokio::test]
    async fn task_dispatch_routes_to_assignee() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        tasks
            .upsert(&CompanyId::new("acme"), &card("t1", "engineer"))
            .await
            .unwrap();

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t1".into(),
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        let note = only_card(&tasks).await.note.expect("note");
        assert!(note.contains("[engineer]"), "{note:?}");
    }

    /// An assignee that is not on the roster falls back to the default responder.
    #[tokio::test]
    async fn task_dispatch_unknown_assignee_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        tasks
            .upsert(&CompanyId::new("acme"), &card("t1", "ghost"))
            .await
            .unwrap();

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t1".into(),
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        let note = only_card(&tasks).await.note.expect("note");
        assert!(note.contains("[ceo]"), "{note:?}");
    }

    /// A dispatch for a card that no longer exists is a silent no-op, not an
    /// error.
    #[tokio::test]
    async fn task_dispatch_missing_card_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "nope".into(),
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs without a card");
        assert!(
            tasks
                .list(&CompanyId::new("acme"))
                .await
                .unwrap()
                .is_empty()
        );
    }

    // --- Orchestrator routing + delegation ----------------------------------

    /// A roster with an `orchestrator`-tier agent (not first) and a desk.
    fn record_with_desk() -> CompanyRecord {
        let manifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "ceo"
role = "Chief Executive"
description = "Runs Acme."

[[agent]]
id = "chief"
role = "Chief of Staff"
tier = "orchestrator"
description = "Coordinates the company."

[[agent]]
id = "engineer"
role = "Engineer"
description = "Builds it."

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
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            template_provenance: None,
        }
    }

    /// A brain over `record`, wired to a real task store.
    fn brain_over(dir: &std::path::Path, record: CompanyRecord) -> (HarnessBrain, Arc<FsOps>) {
        let tasks = Arc::new(FsOps::new(dir));
        let deps = HarnessDeps {
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            context: Arc::new(FsContextStore::new(dir)),
            store: Arc::new(FsCompanyStore::new(dir)),
            meter: Some(Arc::new(FsOps::new(dir))),
            workspace_root: dir.to_path_buf(),
            model_override: None,
            tasks: Some(tasks.clone()),
            skills: None,
            skills_source_dir: None,
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: orchestrator::DelegationQueue::default(),
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            steer: crate::company::steer::InflightRegistry::default(),
        };
        (
            HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record),
            tasks,
        )
    }

    /// A brain over the desk-bearing record, wired to a real task store.
    fn brain_with_desk(dir: &std::path::Path) -> (HarnessBrain, Arc<FsOps>) {
        brain_over(dir, record_with_desk())
    }

    /// The default responder is the `orchestrator`-tier agent, even when it is
    /// not first on the roster.
    #[test]
    fn default_responder_is_the_orchestrator() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks) = brain_with_desk(dir.path());
        assert_eq!(brain.responder, "chief");
    }

    /// An addressed desk routes to its lead member (by id or name); anything else
    /// — the "General" desk, an unknown id, or no address — falls to the
    /// orchestrator.
    #[test]
    fn responder_for_routes_desk_to_lead_else_orchestrator() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks) = brain_with_desk(dir.path());
        assert_eq!(brain.responder_for(Some("eng_desk")), "engineer");
        assert_eq!(brain.responder_for(Some("Engineering")), "engineer");
        assert_eq!(brain.responder_for(Some("General")), "chief");
        assert_eq!(brain.responder_for(Some("nope")), "chief");
        assert_eq!(brain.responder_for(None), "chief");
    }

    // ── Issue #151 §3.3: a DM thread reaches the teammate it names ──

    /// A chat id naming a roster teammate answers as that teammate, which is
    /// what a per-agent DM thread is. Before this it fell through to the
    /// orchestrator, so the console would show an agent's thread while someone
    /// else answered in it.
    #[test]
    fn responder_for_routes_a_roster_agent_id_to_that_agent() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks) = brain_with_desk(dir.path());
        assert_eq!(brain.responder_for(Some("engineer")), "engineer");
        assert_eq!(brain.responder_for(Some("chief")), "chief");
    }

    /// Desks still win. A desk id is resolved as a desk even if an agent shares
    /// the name, so no existing thread changes where it lands.
    #[test]
    fn a_desk_still_outranks_an_agent_of_the_same_name() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks) = brain_with_desk(dir.path());
        // `eng_desk` is a desk led by `engineer`; it must resolve through the
        // desk path, not the DM path.
        assert_eq!(brain.responder_for(Some("eng_desk")), "engineer");
        // And an id that is neither still reaches the orchestrator.
        assert_eq!(brain.responder_for(Some("not-a-teammate")), "chief");
    }

    /// An operator-added overlay member is resolved as a desk's lead (issue #72):
    /// on a desk the manifest left empty, the overlay addition becomes the lead,
    /// and an addressed message routes to it. Proves `desk_lead`/`responder_for`
    /// read the effective (manifest ∪ overlay) membership.
    #[test]
    fn overlay_member_resolves_as_desk_lead() {
        let dir = tempfile::tempdir().unwrap();
        // `design` is a manifest desk with no declared members; the operator adds
        // `engineer` to it through the overlay.
        let manifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "chief"
role = "Chief of Staff"
tier = "orchestrator"

[[agent]]
id = "engineer"
role = "Engineer"

[[group_chat]]
id = "design"
name = "Design"
"#,
        )
        .expect("valid manifest");
        let record = CompanyRecord {
            id: CompanyId::new("acme"),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: vec![crate::ports::types::OverlayDeskMember {
                desk_id: "design".to_string(),
                agent_id: "engineer".to_string(),
            }],
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            template_provenance: None,
        };
        let (brain, _tasks) = brain_over(dir.path(), record);
        assert_eq!(brain.desk_lead("design"), Some("engineer".to_string()));
        assert_eq!(brain.responder_for(Some("design")), "engineer");
    }

    /// The operator's desk hierarchy drives the desk lead: a desk with manifest
    /// members `[eng1, eng2]` plus an overlay `cto`, ordered `[cto, eng1, eng2]`,
    /// resolves its lead to `cto` — `desk_lead` reads `effective_desk_members`,
    /// so the reorder flows through with no change to the resolver (issue #131).
    #[test]
    fn desk_order_drives_the_desk_lead() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "eng1"
role = "Engineer One"

[[agent]]
id = "eng2"
role = "Engineer Two"

[[group_chat]]
id = "eng"
name = "Engineering"
members = ["eng1", "eng2"]
"#,
        )
        .expect("valid manifest");
        let record = CompanyRecord {
            id: CompanyId::new("acme"),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: vec![crate::ports::types::OverlayAgent {
                id: "cto".to_string(),
                name: "Cto".to_string(),
                role: "CTO".to_string(),
                description: None,
            }],
            overlay_desk_members: vec![crate::ports::types::OverlayDeskMember {
                desk_id: "eng".to_string(),
                agent_id: "cto".to_string(),
            }],
            overlay_desk_order: vec![crate::ports::types::OverlayDeskOrder {
                desk_id: "eng".to_string(),
                ordered: vec!["cto".to_string(), "eng1".to_string(), "eng2".to_string()],
            }],
            overlay_desks: Vec::new(),
            template_provenance: None,
        };
        let (brain, _tasks) = brain_over(dir.path(), record);
        assert_eq!(brain.desk_lead("eng"), Some("cto".to_string()));
    }

    /// Regression for the builder seeding path (#133): a desk-order change written
    /// to the store must take effect on routing once the brain is rebuilt from the
    /// persisted record. The builder used to construct the brain with an empty
    /// `overlay_desk_order`, so desk chats kept routing to the pre-reorder lead.
    /// Here we persist a record, build a brain from the loaded record (blueprint
    /// lead), then write a new order and rebuild the brain from the reloaded record
    /// — the lead must update, not stay stale.
    #[tokio::test]
    async fn desk_order_change_updates_routing_after_rebuild() {
        use crate::ports::store::CompanyStore;

        let dir = tempfile::tempdir().unwrap();
        let store = FsCompanyStore::new(dir.path());
        let manifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "eng1"
role = "Engineer One"

[[agent]]
id = "eng2"
role = "Engineer Two"

[[group_chat]]
id = "eng"
name = "Engineering"
members = ["eng1", "eng2"]
"#,
        )
        .expect("valid manifest");
        let id = CompanyId::new("acme");
        store
            .save(&CompanyRecord {
                id: id.clone(),
                manifest,
                ledger: Vec::new(),
                lifecycle: "running".to_string(),
                overlay_agents: Vec::new(),
                overlay_desk_members: Vec::new(),
                overlay_desk_order: Vec::new(),
                overlay_desks: Vec::new(),
                template_provenance: None,
            })
            .await
            .unwrap();

        // Brain built from the persisted record before any reorder: blueprint lead.
        let loaded = store.load(&id).await.unwrap().unwrap();
        let (brain, _tasks) = brain_over(dir.path(), loaded);
        assert_eq!(
            brain.desk_lead("eng"),
            Some("eng1".to_string()),
            "blueprint lead before reorder"
        );

        // Operator reorders the desk (as `set_desk_order` does), promoting eng2.
        let mut record = store.load(&id).await.unwrap().unwrap();
        record
            .overlay_desk_order
            .push(crate::ports::types::OverlayDeskOrder {
                desk_id: "eng".to_string(),
                ordered: vec!["eng2".to_string(), "eng1".to_string()],
            });
        store.save(&record).await.unwrap();

        // Rebuild the brain from the reloaded record: routing follows the reorder,
        // no stale lead.
        let reloaded = store.load(&id).await.unwrap().unwrap();
        let (rebuilt, _tasks2) = brain_over(dir.path(), reloaded);
        assert_eq!(
            rebuilt.desk_lead("eng"),
            Some("eng2".to_string()),
            "reorder did not take effect on routing after rebuild"
        );
    }

    /// A `spawn_task` delegation opens a backlog card and surfaces no bubble.
    #[tokio::test]
    async fn spawn_task_delegation_opens_a_backlog_card() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_desk(dir.path());
        let out = brain
            .run_delegation(
                Delegation::SpawnTask {
                    title: "Draft the plan".to_string(),
                    note: Some("by friday".to_string()),
                    assignee: Some("engineer".to_string()),
                },
                None,
            )
            .await
            .expect("delegation runs");
        assert!(
            out.bubble.is_none() && out.desk_reply.is_none(),
            "spawn_task surfaces nothing to relay or bubble"
        );

        let cards = tasks.list(&CompanyId::new("acme")).await.unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].title, "Draft the plan");
        assert_eq!(cards[0].column, "backlog");
        assert_eq!(cards[0].assignee, "engineer");
    }

    /// A `delegate_to_desk` delegation runs the desk lead and hands its reply
    /// back to relay (a `DeskReply` attributed to the lead, no standalone
    /// bubble); an unknown desk yields nothing.
    #[tokio::test]
    async fn delegate_to_desk_delegation_answers_as_the_desk_lead() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks) = brain_with_desk(dir.path());
        // The pool must have the roster before a member turn can run.
        brain
            .pool
            .ensure(&brain.record, &brain.deps)
            .await
            .expect("roster");

        let out = brain
            .run_delegation(
                Delegation::DelegateToDesk {
                    desk: "eng_desk".to_string(),
                    instruction: "ship-marker".to_string(),
                },
                None,
            )
            .await
            .expect("delegation runs");
        // The answer comes back as a DeskReply to relay — not a standalone
        // bubble — attributed to the desk lead, and the mock provider echoes the
        // instruction, proving the member's turn ran.
        assert!(
            out.bubble.is_none(),
            "the desk reply is relayed, not bubbled"
        );
        let desk = out.desk_reply.expect("desk lead replies");
        assert_eq!(desk.member, "engineer");
        assert!(desk.reply.contains("ship-marker"), "{:?}", desk.reply);

        // An unknown desk delegates to nobody.
        let none = brain
            .run_delegation(
                Delegation::DelegateToDesk {
                    desk: "ghost".to_string(),
                    instruction: "hello".to_string(),
                },
                None,
            )
            .await
            .expect("delegation runs");
        assert!(
            none.bubble.is_none() && none.desk_reply.is_none(),
            "an unknown desk yields nothing"
        );
    }

    // --- MCP failure drain --------------------------------------------------

    /// A recorded MCP failure re-skins into an **error step** on the operator
    /// bubble's timeline AND a scrubbed `McpCallFailed` audit event when the
    /// event log is wired (the Activity-trace re-skin of the old warning bubble).
    #[tokio::test]
    async fn mcp_failures_surface_as_error_steps_and_event() {
        use crate::harness::mcp_probe::McpFailure;
        use crate::ports::EventLog;
        use crate::ports::types::EventSeq;
        use crate::store::FsEventLog;

        let dir = tempfile::tempdir().unwrap();
        let events: Arc<dyn EventLog> = Arc::new(FsEventLog::new(dir.path()));
        let failures = crate::harness::mcp_probe::McpFailureQueue::default();
        let deps = HarnessDeps {
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            context: Arc::new(FsContextStore::new(dir.path())),
            store: Arc::new(FsCompanyStore::new(dir.path())),
            meter: None,
            workspace_root: dir.path().to_path_buf(),
            model_override: None,
            tasks: None,
            skills: None,
            skills_source_dir: None,
            mcp_servers: Vec::new(),
            facts: None,
            events: Some(events.clone()),
            delegations: orchestrator::DelegationQueue::default(),
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: failures.clone(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            steer: crate::company::steer::InflightRegistry::default(),
        };
        let brain = HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record());

        // A failure recorded during the turn (its message already scrubbed).
        failures.push(McpFailure {
            server: "browserbase".into(),
            tool: "browse".into(),
            status: "tool_call_rejected".into(),
            hint: None,
            scrubbed_message: "server rejected the call".into(),
        });

        let mut steps: Vec<TurnStep> = Vec::new();
        // `None` — this is the chat-turn drain, which journals no `task_id`
        // (#185). The dispatch drain passes the card id; see `run_task`.
        brain
            .surface_mcp_failures(&mut steps, None)
            .await
            .expect("drain surfaces failures");

        assert_eq!(steps.len(), 1, "one error step");
        assert_eq!(steps[0].kind, TurnStepKind::Note);
        assert_eq!(steps[0].status, TurnStepStatus::Error);
        assert!(
            steps[0].label.contains("browserbase"),
            "{:?}",
            steps[0].label
        );
        assert_eq!(steps[0].detail.as_deref(), Some("server rejected the call"));

        let logged = events
            .read_from(&CompanyId::new("acme"), EventSeq::new(0), usize::MAX)
            .await
            .expect("read events");
        assert!(
            logged.iter().any(|e| matches!(
                &e.event,
                CompanyEvent::McpCallFailed { server, status, .. }
                    if server == "browserbase" && status == "tool_call_rejected"
            )),
            "an McpCallFailed audit event was journaled"
        );
    }

    // --- Steer disposition (issue #111) -------------------------------------

    use crate::company::steer::InflightRegistry;
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;

    /// A model that steers its OWN in-flight run on selected turns (via the
    /// shared registry), so the disposition matrix can be driven deterministically
    /// over an offline turn. It pops one queued action per [`invoke`](ChatModel::invoke)
    /// call and applies it against `key`, then echoes the last user message.
    struct SteeringProvider {
        steer: InflightRegistry,
        company: CompanyId,
        key: String,
        actions: StdMutex<VecDeque<SteerAction>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl ChatModel<()> for SteeringProvider {
        async fn invoke(
            &self,
            _state: &(),
            request: ModelRequest,
        ) -> tinyagents::Result<ModelResponse> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(action) = self.actions.lock().unwrap().pop_front() {
                let key = if self.key.is_empty() {
                    self.steer
                        .list(&self.company)
                        .into_iter()
                        .next()
                        .map(|entry| entry.key)
                        .unwrap_or_default()
                } else {
                    self.key.clone()
                };
                let _ = self.steer.steer(&self.company, &key, action);
            }
            let message = request
                .messages
                .iter()
                .rev()
                .find(|m| matches!(m, Message::User(_)))
                .map(|m| m.text())
                .unwrap_or_default();
            Ok(ModelResponse::assistant(format!("did: {message}")))
        }
    }

    impl HarnessModel for SteeringProvider {
        fn telemetry_provider_id(&self) -> String {
            "steering".to_string()
        }
    }

    /// A brain whose provider steers the dispatched card `key` with `actions`
    /// (one per turn). Returns the brain + its task store so a test can seed the
    /// card and read the disposition back.
    fn brain_that_steers_itself(
        dir: &std::path::Path,
        key: &str,
        actions: Vec<SteerAction>,
    ) -> (HarnessBrain, Arc<FsOps>, Arc<SteeringProvider>) {
        let steer = InflightRegistry::new();
        let tasks = Arc::new(FsOps::new(dir));
        let provider = Arc::new(SteeringProvider {
            steer: steer.clone(),
            company: CompanyId::new("acme"),
            key: key.to_string(),
            actions: StdMutex::new(actions.into_iter().collect()),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let deps = HarnessDeps {
            provider: provider.clone(),
            provider_slug: "steering".to_string(),
            context: Arc::new(FsContextStore::new(dir)),
            store: Arc::new(FsCompanyStore::new(dir)),
            meter: None,
            workspace_root: dir.to_path_buf(),
            model_override: None,
            tasks: Some(tasks.clone()),
            skills: None,
            skills_source_dir: None,
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: orchestrator::DelegationQueue::default(),
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            steer,
        };
        (
            HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record()),
            tasks,
            provider,
        )
    }

    /// Cancel mid-flight → the card returns to `backlog`, the partial reply is
    /// DISCARDED, and only the operator cancellation note lands.
    #[tokio::test]
    async fn steer_cancel_returns_to_backlog_and_discards_partial() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks, _) =
            brain_that_steers_itself(dir.path(), "t1", vec![SteerAction::Cancel]);
        tasks
            .upsert(&CompanyId::new("acme"), &card("t1", ""))
            .await
            .unwrap();

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t1".into(),
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        let moved = only_card(&tasks).await;
        assert_eq!(moved.column, "backlog");
        let note = moved.note.expect("note");
        assert!(note.contains("cancelled while in flight"), "{note:?}");
        // The agent's partial reply must NOT be preserved on a cancel.
        assert!(
            !note.contains("did: "),
            "cancel discards the partial: {note:?}"
        );
    }

    /// Pause mid-flight → the card parks in the new `paused` column and the
    /// partial reply is PRESERVED in the note.
    #[tokio::test]
    async fn steer_pause_parks_in_paused_and_preserves_partial() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks, _) =
            brain_that_steers_itself(dir.path(), "t1", vec![SteerAction::Pause]);
        tasks
            .upsert(&CompanyId::new("acme"), &card("t1", ""))
            .await
            .unwrap();

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t1".into(),
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        let moved = only_card(&tasks).await;
        assert_eq!(moved.column, "paused");
        let note = moved.note.expect("note");
        assert!(note.contains("[paused]"), "{note:?}");
        assert!(
            note.contains("did: "),
            "pause preserves the partial: {note:?}"
        );
    }

    /// Redirect on every turn → the run re-runs in-loop carrying the operator
    /// instruction, and the per-dispatch redirect cap (3) finalizes it to
    /// `in_review` instead of looping forever.
    #[tokio::test]
    async fn steer_redirect_reruns_and_the_cap_finalizes_to_in_review() {
        let dir = tempfile::tempdir().unwrap();
        let redirect = || SteerAction::Redirect {
            instruction: "focus on the API".to_string(),
        };
        // Steer a redirect on the first several turns; the cap should stop it.
        let (brain, tasks, provider) = brain_that_steers_itself(
            dir.path(),
            "t1",
            vec![redirect(), redirect(), redirect(), redirect()],
        );
        tasks
            .upsert(&CompanyId::new("acme"), &card("t1", ""))
            .await
            .unwrap();

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t1".into(),
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        let moved = only_card(&tasks).await;
        // Redirect budget exhausted → finalized, not looping.
        assert_eq!(moved.column, "in_review");
        let note = moved.note.expect("note");
        // The operator instruction was carried into the rerun, and the reruns
        // echoed it back through the "Operator redirect:" preamble.
        assert!(note.contains("focus on the API"), "{note:?}");
        assert!(
            note.contains("Operator redirect:"),
            "the rerun carried the operator instruction: {note:?}"
        );
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            4,
            "one initial turn plus three reruns"
        );
    }

    #[tokio::test]
    async fn steer_cancelled_delegation_returns_no_bubble() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _, _) = brain_that_steers_itself(dir.path(), "", vec![SteerAction::Cancel]);

        let result = brain
            .run_delegation(
                Delegation::DelegateToDesk {
                    desk: "engineering".to_string(),
                    instruction: "investigate".to_string(),
                },
                None,
            )
            .await
            .expect("cancellation is handled");

        assert!(
            result.bubble.is_none() && result.desk_reply.is_none(),
            "cancelled delegation must not bubble or relay"
        );
    }

    // --- CEO-relay hand-back (delegate_to_desk second turn) ------------------

    /// A provider that simulates the orchestrator queuing a `delegate_to_desk`
    /// on its turns: on each invoke it pops the next scripted delegation (if any)
    /// onto the shared queue — exactly what the real tool call does — then echoes
    /// the last user message so a test can read the turn's reply. Sharing the
    /// queue handle with [`HarnessDeps::delegations`] is what lets the brain
    /// drain it after the turn.
    struct DelegatingProvider {
        queue: orchestrator::DelegationQueue,
        pushes: StdMutex<VecDeque<Option<Delegation>>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl ChatModel<()> for DelegatingProvider {
        async fn invoke(
            &self,
            _state: &(),
            request: ModelRequest,
        ) -> tinyagents::Result<ModelResponse> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(Some(delegation)) = self.pushes.lock().unwrap().pop_front() {
                self.queue.push(delegation);
            }
            let message = request
                .messages
                .iter()
                .rev()
                .find(|m| matches!(m, Message::User(_)))
                .map(|m| m.text())
                .unwrap_or_default();
            Ok(ModelResponse::assistant(format!("did: {message}")))
        }
    }

    impl HarnessModel for DelegatingProvider {
        fn telemetry_provider_id(&self) -> String {
            "delegating".to_string()
        }
    }

    /// A brain over the desk-bearing record whose provider is a
    /// [`DelegatingProvider`] scripted to push `pushes[i]` on invoke `i + 1`.
    /// Returns the brain plus the shared provider so a test can read the invoke
    /// count.
    fn brain_that_delegates(
        dir: &std::path::Path,
        pushes: Vec<Option<Delegation>>,
    ) -> (HarnessBrain, Arc<DelegatingProvider>) {
        let queue = orchestrator::DelegationQueue::default();
        let provider = Arc::new(DelegatingProvider {
            queue: queue.clone(),
            pushes: StdMutex::new(pushes.into_iter().collect()),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let deps = HarnessDeps {
            provider: provider.clone(),
            provider_slug: "delegating".to_string(),
            context: Arc::new(FsContextStore::new(dir)),
            store: Arc::new(FsCompanyStore::new(dir)),
            meter: None,
            workspace_root: dir.to_path_buf(),
            model_override: None,
            tasks: Some(Arc::new(FsOps::new(dir))),
            skills: None,
            skills_source_dir: None,
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: queue,
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            steer: crate::company::steer::InflightRegistry::default(),
        };
        (
            HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record_with_desk()),
            provider,
        )
    }

    /// (a) After a `delegate_to_desk`, the operator-facing reply is a SECOND
    /// orchestrator turn that relays the teammate's answer — one coherent
    /// bubble, not a disconnected sibling.
    #[tokio::test]
    async fn delegate_to_desk_relays_the_answer_in_a_second_orchestrator_turn() {
        let dir = tempfile::tempdir().unwrap();
        // Invoke 1 (orchestrator) queues a delegate_to_desk; invoke 2 is the desk
        // lead's turn; invoke 3 is the relay turn (queues nothing).
        let (brain, provider) = brain_that_delegates(
            dir.path(),
            vec![Some(Delegation::DelegateToDesk {
                desk: "eng_desk".to_string(),
                instruction: "diagnose the outage".to_string(),
            })],
        );

        let result = brain
            .run_cycle(
                request(vec![CompanyEvent::OperatorMessage {
                    text: "why is the site down?".into(),
                    by: None,
                    chat: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        // The operator sees ONE bubble — the CEO's relay, not a separate teammate
        // sibling bubble.
        assert_eq!(result.channel_responses.len(), 1);
        let bubble = &result.channel_responses[0];
        assert_eq!(bubble.channel, "operator");
        // Three turns ran: orchestrator → desk lead → exactly one relay turn.
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "orchestrator, desk lead, then exactly one relay turn"
        );
        // The relayed bubble carries the teammate's answer (the desk lead echoed
        // its instruction, and the relay prompt embeds that reply under an
        // `engineer replied:` frame) — proving the operator reply is the SECOND
        // turn relaying the teammate, not the pre-delegation first reply.
        assert!(
            bubble.text.contains("engineer replied:")
                && bubble.text.contains("diagnose the outage"),
            "the relay carries the teammate's answer: {:?}",
            bubble.text
        );
        // …and it is the relay turn, whose prompt framed the hand-back.
        assert!(
            bubble.text.contains("Relay their answer"),
            "the operator bubble is the relay turn: {:?}",
            bubble.text
        );
    }

    /// (b) The relay turn cannot re-delegate: a delegation it queues is
    /// discarded, so no further desk turn or relay runs (cost stays bounded to
    /// one extra turn).
    #[tokio::test]
    async fn the_relay_turn_cannot_re_delegate() {
        let dir = tempfile::tempdir().unwrap();
        // Invoke 1 queues a delegation; invoke 3 (the relay) ALSO tries to queue
        // one — which must be discarded, so no fourth/fifth turn runs.
        let (brain, provider) = brain_that_delegates(
            dir.path(),
            vec![
                Some(Delegation::DelegateToDesk {
                    desk: "eng_desk".to_string(),
                    instruction: "first".to_string(),
                }),
                None, // the desk lead's turn queues nothing
                Some(Delegation::DelegateToDesk {
                    desk: "eng_desk".to_string(),
                    instruction: "second".to_string(),
                }),
            ],
        );

        let result = brain
            .run_cycle(
                request(vec![CompanyEvent::OperatorMessage {
                    text: "handle it".into(),
                    by: None,
                    chat: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        // Exactly three turns: orchestrator, desk lead, relay. The relay's queued
        // delegation was dropped — no fourth (desk-lead) or fifth (relay) turn.
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "the relay turn's delegation is discarded — one extra turn, no loop"
        );
        // The discard actually emptied the queue (not left dirty for next cycle).
        assert_eq!(
            brain.deps.delegations.queued(),
            0,
            "the relay turn's queued delegation was discarded"
        );
        // Still exactly one operator bubble.
        assert_eq!(result.channel_responses.len(), 1);
        assert_eq!(result.channel_responses[0].channel, "operator");
    }

    /// (c) A normal, non-delegating message still produces exactly one turn — the
    /// relay path is entered only when a `delegate_to_desk` actually answered.
    #[tokio::test]
    async fn a_non_delegating_message_runs_exactly_one_turn() {
        let dir = tempfile::tempdir().unwrap();
        // No scripted delegations → the orchestrator answers directly.
        let (brain, provider) = brain_that_delegates(dir.path(), Vec::new());

        let result = brain
            .run_cycle(
                request(vec![CompanyEvent::OperatorMessage {
                    text: "status?".into(),
                    by: None,
                    chat: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "no delegation → a single orchestrator turn, no relay"
        );
        assert_eq!(result.channel_responses.len(), 1);
        assert_eq!(result.channel_responses[0].channel, "operator");
        assert!(
            result.channel_responses[0].text.contains("status?"),
            "{:?}",
            result.channel_responses[0].text
        );
    }
}
