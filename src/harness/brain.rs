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
/// appended; past this cap the run is finalized to `in_review` so a redirect
/// storm can't loop forever.
const MAX_REDIRECTS_PER_DISPATCH: u32 = 3;
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
    /// onto the board — moved to `in_review` on success, back to `backlog` with
    /// the error noted on failure. A missing task store or a card that has since
    /// vanished is a silent no-op.
    async fn run_task(&self, task_id: &str) -> Result<()> {
        let Some(tasks) = self.deps.tasks.as_ref() else {
            return Ok(());
        };
        let Some(mut card) = tasks
            .list(&self.record.id)
            .await?
            .into_iter()
            .find(|t| t.id == task_id)
        else {
            return Ok(());
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

        loop {
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
                            card.note = Some(append_result(
                                card.note.as_deref(),
                                &responder,
                                &outcome.reply,
                            ));
                            card.column = "in_review".to_string();
                        }
                        Err(err) => {
                            card.note = Some(append_result(
                                card.note.as_deref(),
                                &responder,
                                &format!("dispatch failed: {err}"),
                            ));
                            card.column = "backlog".to_string();
                        }
                    }
                    break;
                }
                Some(SteerAction::Cancel) => {
                    // Partial work is DISCARDED — only a cancellation note lands,
                    // and the card returns to `backlog`.
                    card.note = Some(append_result(
                        card.note.as_deref(),
                        "operator",
                        "cancelled while in flight",
                    ));
                    card.column = "backlog".to_string();
                    break;
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
                    break;
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
                        // reply to `in_review` rather than looping forever.
                        let last = match &outcome {
                            Ok(outcome) => outcome.reply.clone(),
                            Err(err) => format!("dispatch failed: {err}"),
                        };
                        card.note = Some(append_result(card.note.as_deref(), &responder, &last));
                        card.column = "in_review".to_string();
                        break;
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
        }

        card.updated_at_millis = now_millis();
        tasks.upsert(&self.record.id, &card).await?;
        // `guard` drops here → the run leaves the in-flight strip.
        drop(guard);
        Ok(())
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

    /// Resolves which agent answers an operator message. A message addressed to a
    /// desk (its `chat` field naming a group chat with a lead member) is answered
    /// by that desk's lead; everything else — including the "General" desk and
    /// unaddressed messages — goes to the orchestrator (the default responder).
    fn responder_for(&self, chat: Option<&str>) -> String {
        match chat.and_then(|desk| self.desk_lead(desk)) {
            Some(member) => member,
            None => self.responder.clone(),
        }
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
    async fn surface_mcp_failures(&self, steps: &mut Vec<TurnStep>) -> Result<()> {
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
    /// returns its reply as its own chat bubble — `channel = <member id>`, the
    /// distinct-bubble path the console already renders. An unknown desk (no
    /// roster-backed lead) is a silent no-op. No sub-agent re-delegation in v1:
    /// desk members carry no delegation tools, so their turns queue nothing.
    async fn run_delegation(
        &self,
        delegation: Delegation,
        chat_id: Option<&str>,
    ) -> Result<Option<OutboundMessage>> {
        match delegation {
            Delegation::SpawnTask {
                title,
                note,
                assignee,
            } => {
                let Some(tasks) = self.deps.tasks.as_ref() else {
                    return Ok(None);
                };
                let card = TaskRecord {
                    id: generate_id(),
                    title,
                    note,
                    column: "backlog".to_string(),
                    priority: "medium".to_string(),
                    assignee: assignee.unwrap_or_default(),
                    updated_at_millis: now_millis(),
                };
                tasks.upsert(&self.record.id, &card).await?;
                Ok(None)
            }
            Delegation::DelegateToDesk { desk, instruction } => {
                let Some(member) = self.desk_lead(&desk) else {
                    return Ok(None);
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
                // A cancel issued mid-flight discards the delegated reply — no
                // bubble surfaces.
                if matches!(control.take(), Some(SteerAction::Cancel)) {
                    return Ok(None);
                }
                // The desk lead's own steps ride on its distinct bubble.
                Ok(Some(OutboundMessage {
                    channel: member,
                    text: outcome.reply,
                    reply_to: None,
                    steps: outcome.steps,
                }))
            }
        }
    }
}

/// The turn instruction for a dispatched card: its title, plus its note when it
/// carries one, framed as a work item to act on.
fn task_instruction(card: &TaskRecord) -> String {
    match card.note.as_deref().filter(|n| !n.is_empty()) {
        Some(note) => format!("Task: {}\n\n{}", card.title, note),
        None => format!("Task: {}", card.title),
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
                    // The orchestrator's own steps ride on the operator bubble.
                    let mut operator_steps = outcome.steps;
                    // Run whatever the orchestrator queued; each delegated desk
                    // bubble carries its own lead's steps. Collect them first so
                    // the operator bubble can still be finalized before it is
                    // pushed (any MCP failure a delegated turn recorded lands on
                    // the operator timeline).
                    let mut delegated = Vec::new();
                    for delegation in self
                        .deps
                        .delegations
                        .drain(orchestrator::MAX_DELEGATIONS_PER_TURN)
                    {
                        if let Some(message) = self.run_delegation(delegation, chat_id).await? {
                            delegated.push(message);
                        }
                    }
                    // Re-skin any MCP tool-call failures (from the orchestrator
                    // turn or a delegated desk turn) as error steps on the
                    // operator bubble — one surface, one renderer.
                    self.surface_mcp_failures(&mut operator_steps).await?;
                    channel_responses.push(OutboundMessage {
                        channel: "operator".to_string(),
                        text: outcome.reply,
                        reply_to: None,
                        steps: operator_steps,
                    });
                    channel_responses.extend(delegated);
                }
                CompanyEvent::TaskDispatched { task_id } => {
                    self.run_task(task_id).await?;
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
            overlay_desks: Vec::new(),
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
            overlay_desks: Vec::new(),
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
        }
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

    /// A dispatched task runs a turn and moves to `in_review`, its result folded
    /// into the note under the responder that ran it.
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
            overlay_desks: Vec::new(),
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
            overlay_desks: Vec::new(),
        };
        let (brain, _tasks) = brain_over(dir.path(), record);
        assert_eq!(brain.desk_lead("design"), Some("engineer".to_string()));
        assert_eq!(brain.responder_for(Some("design")), "engineer");
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
        assert!(out.is_none(), "spawn_task surfaces no chat bubble");

        let cards = tasks.list(&CompanyId::new("acme")).await.unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].title, "Draft the plan");
        assert_eq!(cards[0].column, "backlog");
        assert_eq!(cards[0].assignee, "engineer");
    }

    /// A `delegate_to_desk` delegation runs the desk lead and surfaces its reply
    /// as its own bubble (`channel = <member id>`); an unknown desk is a no-op.
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
            .expect("delegation runs")
            .expect("desk lead replies");
        // The reply is its own bubble attributed to the desk lead, and the mock
        // provider echoes the instruction, proving the member's turn ran.
        assert_eq!(out.channel, "engineer");
        assert!(out.text.contains("ship-marker"), "{:?}", out.text);

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
        assert!(none.is_none(), "an unknown desk is a silent no-op");
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
        brain
            .surface_mcp_failures(&mut steps)
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

        assert!(result.is_none(), "cancelled delegation must not bubble");
    }
}
