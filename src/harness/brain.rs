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
use crate::harness::orchestrator::{self, Delegation};
use crate::harness::{HarnessDeps, HarnessPool};
use crate::ports::brain::{Brain, CycleHost};
use crate::ports::types::{
    CompanyEvent, CompanyRecord, CompressedTrace, CycleRequest, CycleResult, OutboundMessage,
    TokenUsage,
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
        let instruction = task_instruction(&card);
        match self
            .pool
            .run(&self.record.id, &responder, &instruction, &self.deps)
            .await
        {
            Ok(reply) => {
                card.note = Some(append_result(card.note.as_deref(), &responder, &reply));
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
        card.updated_at_millis = now_millis();
        tasks.upsert(&self.record.id, &card).await?;
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
    /// (by id, or by case-insensitive name) that is a real roster agent. `None`
    /// when no desk matches or none of its members are on the roster.
    fn desk_lead(&self, desk: &str) -> Option<String> {
        let chat = self
            .record
            .manifest
            .group_chats
            .iter()
            .find(|c| c.id == desk || c.name.eq_ignore_ascii_case(desk))?;
        chat.members
            .iter()
            .find(|m| self.record.manifest.agents.iter().any(|a| &a.id == *m))
            .cloned()
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
    async fn run_delegation(&self, delegation: Delegation) -> Result<Option<OutboundMessage>> {
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
                let reply = self
                    .pool
                    .run(&self.record.id, &member, &instruction, &self.deps)
                    .await?;
                Ok(Some(OutboundMessage {
                    channel: member,
                    text: reply,
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

/// Desk-routes an operator message to the roster member it addresses.
///
/// A leading `@mention` matching a roster member's id or role (case- and
/// separator-insensitive — `@Creative-Director`, `@creative_director`, and the
/// role "Creative Director" all match) routes to that member, and the mention is
/// stripped from the message. Anything else — no mention, or a mention that
/// names no roster member — falls back to `responder` with the message intact.
///
/// Returns `(agent_id, message)` ready for [`HarnessPool::run`].
fn resolve_address(agents: &[ManifestAgent], responder: &str, text: &str) -> (String, String) {
    if let Some(rest) = text.trim_start().strip_prefix('@') {
        let (mention, remainder) = match rest.split_once(char::is_whitespace) {
            Some((m, r)) => (m, r.trim_start()),
            None => (rest, ""),
        };
        let target = normalize_mention(mention);
        if !target.is_empty()
            && let Some(agent) = agents.iter().find(|a| {
                normalize_mention(&a.id) == target || normalize_mention(&a.role) == target
            })
        {
            // Strip the mention; keep the original text if nothing else remains
            // (a bare "@creative_director" still addresses that agent).
            let message = if remainder.is_empty() {
                text.to_string()
            } else {
                remainder.to_string()
            };
            return (agent.id.clone(), message);
        }
    }
    (responder.to_string(), text.to_string())
}

/// Normalizes a mention / agent id / role for matching: lowercased, ASCII
/// alphanumerics only, so separators (`-`, `_`, spaces) are ignored.
fn normalize_mention(s: &str) -> String {
    s.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
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
                    // Clear stale delegations so nothing leaks from a prior turn,
                    // run the turn (metered through `deps`), then drain whatever
                    // the orchestrator queued (capped; discarded past the cap).
                    self.deps.delegations.clear();
                    let reply = self
                        .pool
                        .run(&self.record.id, &responder, text, &self.deps)
                        .await?;
                    channel_responses.push(OutboundMessage {
                        channel: "operator".to_string(),
                        text: reply,
                    });
                    for delegation in self
                        .deps
                        .delegations
                        .drain(orchestrator::MAX_DELEGATIONS_PER_TURN)
                    {
                        if let Some(message) = self.run_delegation(delegation).await? {
                            channel_responses.push(message);
                        }
                    }
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

    use crate::harness::provider::MockProvider;
    use crate::ports::brain::CycleHost;
    use crate::ports::types::{
        CompanyId, ContextOp, ContextOpResult, Effect, EffectDisposition, ToolCall, ToolResult,
    };
    use crate::store::{FsCompanyStore, FsContextStore, FsOps};

    fn agent(id: &str, role: &str) -> ManifestAgent {
        ManifestAgent {
            id: id.to_string(),
            role: role.to_string(),
            description: None,
            tier: None,
            tools: Vec::new(),
            budget_usd_daily: None,
        }
    }

    #[test]
    fn resolve_address_routes_mentions_and_falls_back() {
        let roster = [
            agent("creative_director", "Creative Director"),
            agent("ceo", "Chief Executive"),
        ];

        // @id — separator/case-insensitive, mention stripped from the message.
        let (who, msg) = resolve_address(&roster, "ceo", "@Creative-Director draft the copy");
        assert_eq!(who, "creative_director");
        assert_eq!(msg, "draft the copy");

        // @role — "Creative Director" normalizes to "creativedirector".
        let (who, _) = resolve_address(&roster, "ceo", "@creativedirector hi");
        assert_eq!(who, "creative_director");

        // No mention → default responder, message untouched.
        let (who, msg) = resolve_address(&roster, "ceo", "just do it");
        assert_eq!(who, "ceo");
        assert_eq!(msg, "just do it");

        // Unknown mention → default responder, message left intact (not stripped).
        let (who, msg) = resolve_address(&roster, "ceo", "@nobody hello");
        assert_eq!(who, "ceo");
        assert_eq!(msg, "@nobody hello");

        // Bare mention with no remainder → routes, keeps the original text.
        let (who, msg) = resolve_address(&roster, "ceo", "@creative_director");
        assert_eq!(who, "creative_director");
        assert_eq!(msg, "@creative_director");
    }

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
        }
    }

    /// A brain over the desk-bearing record, wired to a real task store.
    fn brain_with_desk(dir: &std::path::Path) -> (HarnessBrain, Arc<FsOps>) {
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
        };
        (
            HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record_with_desk()),
            tasks,
        )
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

    /// A `spawn_task` delegation opens a backlog card and surfaces no bubble.
    #[tokio::test]
    async fn spawn_task_delegation_opens_a_backlog_card() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_desk(dir.path());
        let out = brain
            .run_delegation(Delegation::SpawnTask {
                title: "Draft the plan".to_string(),
                note: Some("by friday".to_string()),
                assignee: Some("engineer".to_string()),
            })
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
            .run_delegation(Delegation::DelegateToDesk {
                desk: "eng_desk".to_string(),
                instruction: "ship-marker".to_string(),
            })
            .await
            .expect("delegation runs")
            .expect("desk lead replies");
        // The reply is its own bubble attributed to the desk lead, and the mock
        // provider echoes the instruction, proving the member's turn ran.
        assert_eq!(out.channel, "engineer");
        assert!(out.text.contains("ship-marker"), "{:?}", out.text);

        // An unknown desk delegates to nobody.
        let none = brain
            .run_delegation(Delegation::DelegateToDesk {
                desk: "ghost".to_string(),
                instruction: "hello".to_string(),
            })
            .await
            .expect("delegation runs");
        assert!(none.is_none(), "an unknown desk is a silent no-op");
    }
}
