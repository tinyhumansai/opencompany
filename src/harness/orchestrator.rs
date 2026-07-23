//! The company **orchestrator**: the operator↔company chat as a first-class
//! delegating agent.
//!
//! Where the harness brain's default chat responder is just the first roster
//! agent, the orchestrator is the one place the operator asks anything and it
//! answers from whole-company context — grounding replies in the company's
//! durable facts and recent activity, and delegating work it should not do
//! itself. It is the roster agent whose manifest `tier = "orchestrator"`, or the
//! first agent when none is tagged (so a company without an orchestrator behaves
//! exactly as before).
//!
//! It reaches three tools, all wired only onto the orchestrator agent:
//!
//! * [`QueryCompanyTool`] — a read surface over the company's [`FactStore`] and
//!   recent [`EventLog`] history.
//! * [`SpawnTaskTool`] / [`DelegateToDeskTool`] — delegation tools that push a
//!   [`Delegation`] onto a shared [`DelegationQueue`]. They perform no work
//!   themselves; the [`HarnessBrain`](crate::harness::HarnessBrain) drains the
//!   queue after the orchestrator's turn (v1: synchronous, in-cycle, capped at
//!   [`MAX_DELEGATIONS_PER_TURN`], no sub-agent re-delegation).
//!
//! Compiled only under `feature = "openhuman"` (the whole `harness` module is).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};

use openhuman_core::openhuman as oh;

use oh::tools::traits::{PermissionLevel, Tool, ToolResult};

use crate::company::Agent as ManifestAgent;
use crate::ports::events::EventLog;
use crate::ports::facts::FactStore;
use crate::ports::types::{CompanyEvent, CompanyId, EventSeq};

/// The manifest cognition-tier that marks the orchestrator agent.
pub const ORCHESTRATOR_TIER: &str = "orchestrator";

/// Max delegations drained from a single orchestrator turn (v1 cap). Anything an
/// over-eager turn queues beyond this is discarded — delegation is bounded so a
/// runaway turn can't fan out unboundedly.
pub const MAX_DELEGATIONS_PER_TURN: usize = 3;

/// How many recent events [`QueryCompanyTool`] surfaces.
const RECENT_EVENTS: usize = 10;
/// How many facts [`QueryCompanyTool`] surfaces.
const FACT_LIMIT: usize = 20;

/// The `query_company` tool name.
pub const QUERY_COMPANY_TOOL: &str = "query_company";
/// The `spawn_task` tool name.
pub const SPAWN_TASK_TOOL: &str = "spawn_task";
/// The `delegate_to_desk` tool name.
pub const DELEGATE_TO_DESK_TOOL: &str = "delegate_to_desk";

/// The id of the orchestrator agent for a roster: the first agent tagged
/// `tier = "orchestrator"`, else the first roster agent, else `None` (empty
/// roster). The fallback is what keeps a company with no tagged orchestrator
/// answering exactly as it did before this cell.
pub fn orchestrator_id(agents: &[ManifestAgent]) -> Option<String> {
    agents
        .iter()
        .find(|a| a.tier.as_deref() == Some(ORCHESTRATOR_TIER))
        .or_else(|| agents.first())
        .map(|a| a.id.clone())
}

/// Whether `tool` is one of the orchestrator's in-cycle delegation tools.
///
/// These enqueue internal work drained by the harness brain (a task card, or a
/// hand-off to a desk's lead member) rather than reaching an external
/// counterparty, so the [`ApprovalPolicy`](crate::harness::policy::ApprovalPolicy)
/// classifies them as internal — never an external effect to park or deny.
pub fn is_delegation_tool(tool: &str) -> bool {
    tool == SPAWN_TASK_TOOL || tool == DELEGATE_TO_DESK_TOOL
}

/// The orchestrator persona brief, appended to the orchestrator agent's persona.
pub fn orchestrator_brief() -> String {
    " You are also this company's orchestrator: the single point of contact for the operator. \
Answer from whole-company context, and when a request belongs to a specialist desk or should be \
tracked as work, delegate instead of guessing. Use `query_company` to ground answers in the \
company's durable facts and recent activity, `delegate_to_desk` to hand a turn to a desk's lead \
member, and `spawn_task` to open a tracked task card. Delegate only when it genuinely helps — \
otherwise answer directly and concisely."
        .to_string()
}

// ---------------------------------------------------------------------------
// Delegation queue
// ---------------------------------------------------------------------------

/// One unit of work the orchestrator hands off during a turn, drained by the
/// harness brain after the turn completes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Delegation {
    /// Open a tracked task card on the company's board.
    SpawnTask {
        /// The task title.
        title: String,
        /// An optional longer note / brief.
        note: Option<String>,
        /// An optional assignee (a roster/desk id); empty when unassigned.
        assignee: Option<String>,
    },
    /// Hand a turn to a desk's lead member.
    DelegateToDesk {
        /// The desk id or name to delegate to.
        desk: String,
        /// The instruction handed to the desk's lead member.
        instruction: String,
    },
}

/// A shared, in-memory queue the delegation tools push onto and the harness
/// brain drains. Cheap to [`Clone`] (a shared handle); the same underlying
/// queue is seen by the tools captured into the orchestrator agent and by the
/// brain that drains it, because [`HarnessDeps`](crate::harness::HarnessDeps)
/// clones share this handle.
#[derive(Clone, Default)]
pub struct DelegationQueue {
    inner: Arc<Mutex<Vec<Delegation>>>,
}

impl DelegationQueue {
    /// Enqueues a delegation.
    pub fn push(&self, delegation: Delegation) {
        self.inner
            .lock()
            .expect("delegation queue")
            .push(delegation);
    }

    /// Empties the queue (called before an orchestrator turn so stale
    /// delegations from a prior turn never leak into this one).
    pub fn clear(&self) {
        self.inner.lock().expect("delegation queue").clear();
    }

    /// Drains up to `cap` queued delegations (FIFO) and discards the rest, so a
    /// single turn can never fan out past the cap.
    pub fn drain(&self, cap: usize) -> Vec<Delegation> {
        let mut guard = self.inner.lock().expect("delegation queue");
        let take = guard.len().min(cap);
        let drained: Vec<Delegation> = guard.drain(..take).collect();
        guard.clear();
        drained
    }

    /// The number of queued delegations (test/observability).
    #[cfg(test)]
    pub fn queued(&self) -> usize {
        self.inner.lock().expect("delegation queue").len()
    }
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

/// A read surface over the company's durable facts and recent event history, so
/// the orchestrator can ground its answers in whole-company context.
pub struct QueryCompanyTool {
    company: CompanyId,
    facts: Option<Arc<dyn FactStore>>,
    events: Option<Arc<dyn EventLog>>,
}

impl QueryCompanyTool {
    /// Builds the tool over the company's read ports. Either handle may be
    /// `None`; the tool reports whatever surface is wired.
    pub fn new(
        company: CompanyId,
        facts: Option<Arc<dyn FactStore>>,
        events: Option<Arc<dyn EventLog>>,
    ) -> Self {
        Self {
            company,
            facts,
            events,
        }
    }
}

#[async_trait]
impl Tool for QueryCompanyTool {
    fn name(&self) -> &str {
        QUERY_COMPANY_TOOL
    }

    fn description(&self) -> &str {
        "Read the company's durable facts and recent activity to ground an answer in whole-company context. Optionally pass a `query` to filter facts by a case-insensitive substring."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Optional case-insensitive substring to filter facts by."
                }
            },
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    fn supports_markdown(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let query = args.get("query").and_then(Value::as_str).map(str::trim);
        let query = query.filter(|q| !q.is_empty());

        let facts = match &self.facts {
            Some(store) => store
                .list(&self.company, query, None)
                .await
                .unwrap_or_default(),
            None => Vec::new(),
        };

        // Recent events: read the log and keep the tail. Mirrors the GraphQL
        // history resolver's read-then-tail pattern (`read_from(0, MAX)`).
        let mut recent: Vec<String> = match &self.events {
            Some(log) => log
                .read_from(&self.company, EventSeq::new(0), usize::MAX)
                .await
                .unwrap_or_default()
                .iter()
                .rev()
                .take(RECENT_EVENTS)
                .map(|stored| format!("- #{} {}", stored.seq, summarize_event(&stored.event)))
                .collect(),
            None => Vec::new(),
        };
        recent.reverse(); // back to chronological order

        let mut md = String::from("# Company insight\n");
        md.push_str("\n## Facts\n");
        if facts.is_empty() {
            md.push_str("_No durable facts recorded._\n");
        } else {
            for fact in facts.iter().take(FACT_LIMIT) {
                md.push_str(&format!(
                    "- **{}**: {}\n",
                    fact.title.trim(),
                    fact.body.trim()
                ));
            }
        }
        md.push_str("\n## Recent activity\n");
        if recent.is_empty() {
            md.push_str("_No recent activity._\n");
        } else {
            md.push_str(&recent.join("\n"));
            md.push('\n');
        }

        Ok(ToolResult::success_with_markdown(
            json!({
                "facts": facts.len(),
                "recent_events": recent.len(),
            }),
            md,
        ))
    }
}

/// A short, non-sensitive one-line summary of an event for the insight surface.
fn summarize_event(event: &CompanyEvent) -> String {
    match event {
        CompanyEvent::OperatorMessage { .. } => "operator message".to_string(),
        CompanyEvent::AgentReply { agent_id, .. } => format!("reply from {agent_id}"),
        CompanyEvent::TaskDispatched { task_id } => format!("task dispatched: {task_id}"),
        CompanyEvent::ScheduleFired { cron, .. } => format!("schedule fired: {cron}"),
        CompanyEvent::WebhookReceived { channel, .. } => format!("webhook on {channel}"),
        CompanyEvent::A2aTaskReceived { from, .. } => format!("A2A task from {from}"),
        CompanyEvent::ApprovalResolved { verdict, .. } => format!("approval {verdict:?}"),
        CompanyEvent::FeedbackFiled { .. } => "feedback filed".to_string(),
        CompanyEvent::PaymentReceived { amount_usd, .. } => format!("payment ${amount_usd:.2}"),
        CompanyEvent::LifecycleChanged { from, to, .. } => format!("lifecycle {from} → {to}"),
        CompanyEvent::MemoryFactDeleted { .. } => "memory fact deleted".to_string(),
        CompanyEvent::McpCallFailed { server, tool, .. } => {
            format!("MCP call failed: {server}/{tool}")
        }
    }
}

/// A delegation tool that opens a tracked task card. Enqueues a
/// [`Delegation::SpawnTask`]; the harness brain writes the card on drain.
pub struct SpawnTaskTool {
    queue: DelegationQueue,
}

impl SpawnTaskTool {
    /// Builds the tool over the shared delegation queue.
    pub fn new(queue: DelegationQueue) -> Self {
        Self { queue }
    }
}

#[async_trait]
impl Tool for SpawnTaskTool {
    fn name(&self) -> &str {
        SPAWN_TASK_TOOL
    }

    fn description(&self) -> &str {
        "Open a tracked task card on the company's board for work that should be followed up. Provide a `title`, an optional `note` brief, and an optional `assignee` (a desk or teammate id)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "The task title." },
                "note": { "type": "string", "description": "An optional longer brief." },
                "assignee": { "type": "string", "description": "An optional desk/teammate id to own it." }
            },
            "required": ["title"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let title = args
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| anyhow::anyhow!("`title` is required"))?
            .to_string();
        let note = args
            .get("note")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string);
        let assignee = args
            .get("assignee")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(str::to_string);

        self.queue.push(Delegation::SpawnTask {
            title: title.clone(),
            note,
            assignee,
        });
        Ok(ToolResult::success(format!(
            "Queued a task card: \"{title}\". It will be opened on the board this turn."
        )))
    }
}

/// A delegation tool that hands a turn to a desk's lead member. Enqueues a
/// [`Delegation::DelegateToDesk`]; the harness brain runs the desk turn on
/// drain and surfaces its reply as its own chat bubble.
pub struct DelegateToDeskTool {
    queue: DelegationQueue,
}

impl DelegateToDeskTool {
    /// Builds the tool over the shared delegation queue.
    pub fn new(queue: DelegationQueue) -> Self {
        Self { queue }
    }
}

#[async_trait]
impl Tool for DelegateToDeskTool {
    fn name(&self) -> &str {
        DELEGATE_TO_DESK_TOOL
    }

    fn description(&self) -> &str {
        "Hand a turn to a desk's lead member so a specialist answers. Provide the `desk` (its id or name) and the `instruction` to carry out."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "desk": { "type": "string", "description": "The desk id or name to delegate to." },
                "instruction": { "type": "string", "description": "The instruction for the desk's lead member." }
            },
            "required": ["desk", "instruction"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let desk = args
            .get("desk")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .ok_or_else(|| anyhow::anyhow!("`desk` is required"))?
            .to_string();
        let instruction = args
            .get("instruction")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|i| !i.is_empty())
            .ok_or_else(|| anyhow::anyhow!("`instruction` is required"))?
            .to_string();

        self.queue.push(Delegation::DelegateToDesk {
            desk: desk.clone(),
            instruction,
        });
        Ok(ToolResult::success(format!(
            "Delegated to the {desk} desk. Its lead will answer this turn."
        )))
    }
}

/// The orchestrator's delegation tools over a shared queue: `spawn_task` and
/// `delegate_to_desk`. `query_company` is built separately because it needs the
/// read ports, not the queue.
pub fn delegation_tools(queue: &DelegationQueue) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(SpawnTaskTool::new(queue.clone())),
        Box::new(DelegateToDeskTool::new(queue.clone())),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str, tier: Option<&str>) -> ManifestAgent {
        ManifestAgent {
            id: id.to_string(),
            role: "Role".to_string(),
            description: None,
            tier: tier.map(str::to_string),
            tools: Vec::new(),
            budget_usd_daily: None,
        }
    }

    #[test]
    fn orchestrator_id_prefers_the_tagged_agent() {
        let roster = vec![
            agent("ceo", None),
            agent("chief", Some("orchestrator")),
            agent("eng", Some("reasoning")),
        ];
        assert_eq!(orchestrator_id(&roster).as_deref(), Some("chief"));
    }

    #[test]
    fn orchestrator_id_falls_back_to_first_agent() {
        let roster = vec![agent("ceo", None), agent("eng", None)];
        assert_eq!(orchestrator_id(&roster).as_deref(), Some("ceo"));
    }

    #[test]
    fn orchestrator_id_is_none_for_an_empty_roster() {
        assert_eq!(orchestrator_id(&[]), None);
    }

    #[test]
    fn delegation_tool_names_are_classified_internal() {
        assert!(is_delegation_tool(SPAWN_TASK_TOOL));
        assert!(is_delegation_tool(DELEGATE_TO_DESK_TOOL));
        // The read tool is NOT a delegation tool.
        assert!(!is_delegation_tool(QUERY_COMPANY_TOOL));
        assert!(!is_delegation_tool("send_email"));
    }

    #[test]
    fn queue_drains_fifo_up_to_cap_and_discards_the_rest() {
        let queue = DelegationQueue::default();
        for i in 0..5 {
            queue.push(Delegation::SpawnTask {
                title: format!("t{i}"),
                note: None,
                assignee: None,
            });
        }
        assert_eq!(queue.queued(), 5);
        let drained = queue.drain(MAX_DELEGATIONS_PER_TURN);
        assert_eq!(drained.len(), 3);
        // The first three (FIFO) survive; the queue is emptied.
        assert_eq!(
            drained[0],
            Delegation::SpawnTask {
                title: "t0".to_string(),
                note: None,
                assignee: None,
            }
        );
        assert_eq!(queue.queued(), 0);
    }

    #[test]
    fn clear_empties_the_queue() {
        let queue = DelegationQueue::default();
        queue.push(Delegation::DelegateToDesk {
            desk: "strategy".to_string(),
            instruction: "plan".to_string(),
        });
        queue.clear();
        assert_eq!(queue.queued(), 0);
    }

    #[tokio::test]
    async fn spawn_task_tool_enqueues_a_task() {
        let queue = DelegationQueue::default();
        let tool = SpawnTaskTool::new(queue.clone());
        tool.execute(json!({ "title": "Ship it", "note": "soon", "assignee": "eng" }))
            .await
            .expect("execute");
        let drained = queue.drain(MAX_DELEGATIONS_PER_TURN);
        assert_eq!(
            drained,
            vec![Delegation::SpawnTask {
                title: "Ship it".to_string(),
                note: Some("soon".to_string()),
                assignee: Some("eng".to_string()),
            }]
        );
    }

    #[tokio::test]
    async fn spawn_task_tool_requires_a_title() {
        let queue = DelegationQueue::default();
        let tool = SpawnTaskTool::new(queue.clone());
        assert!(tool.execute(json!({ "note": "no title" })).await.is_err());
        assert_eq!(queue.queued(), 0);
    }

    #[tokio::test]
    async fn delegate_to_desk_tool_enqueues_a_hand_off() {
        let queue = DelegationQueue::default();
        let tool = DelegateToDeskTool::new(queue.clone());
        tool.execute(json!({ "desk": "strategy", "instruction": "draft a plan" }))
            .await
            .expect("execute");
        let drained = queue.drain(MAX_DELEGATIONS_PER_TURN);
        assert_eq!(
            drained,
            vec![Delegation::DelegateToDesk {
                desk: "strategy".to_string(),
                instruction: "draft a plan".to_string(),
            }]
        );
    }

    #[tokio::test]
    async fn query_company_tool_reports_no_data_when_unwired() {
        let tool = QueryCompanyTool::new(CompanyId::new("acme"), None, None);
        let result = tool.execute(json!({})).await.expect("execute");
        // The insight surface lives in the markdown; `output()` is the summary.
        let out = result.output_for_llm(true);
        assert!(out.contains("No durable facts recorded"), "{out}");
        assert!(out.contains("No recent activity"), "{out}");
    }
}
