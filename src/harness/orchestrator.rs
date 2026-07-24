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
//! It reaches five tools, all wired only onto the orchestrator agent:
//!
//! * [`QueryCompanyTool`] — a read surface over the company's [`FactStore`] and
//!   recent [`EventLog`] history.
//! * [`SpawnTaskTool`] / [`DelegateToDeskTool`] — delegation tools that push a
//!   [`Delegation`] onto a shared [`DelegationQueue`]. They perform no work
//!   themselves; the [`HarnessBrain`](crate::harness::HarnessBrain) drains the
//!   queue after the orchestrator's turn (v1: synchronous, in-cycle, capped at
//!   [`MAX_DELEGATIONS_PER_TURN`], no sub-agent re-delegation).
//! * [`RunWorkflowTool`] — executes one of the company's saved workflow graphs
//!   by id via the [`WorkflowRunner`] port (issue #67). It loads the graph from
//!   the company source directory (the same loader the REST run route uses) and
//!   invokes the runner reached through a shared [`WorkflowRunnerHandle`], so a
//!   task waiting on a workflow can actually be run to completion. Unlike the
//!   delegation tools it runs the graph inline and returns a concise summary of
//!   the run rather than enqueuing deferred work.
//! * [`AddAgentTool`] (issue #71) — writes a new [`OverlayAgent`] through the
//!   same store path the console `POST .../team` route uses, so the
//!   orchestrator can bring on a teammate mid-chat.
//!
//! Compiled only under `feature = "openhuman"` (the whole `harness` module is).

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use crate::ports::store::company_write_lock;

use async_trait::async_trait;
use serde_json::{Value, json};

use openhuman_core::openhuman as oh;

use oh::tools::traits::{PermissionLevel, Tool, ToolResult};

use crate::company::{Agent as ManifestAgent, WorkflowFile, load_company_workflows};
use crate::error::OpenCompanyError;
use crate::ports::events::EventLog;
use crate::ports::facts::FactStore;
use crate::ports::types::{CompanyEvent, CompanyId, EventSeq, OverlayAgent};
use crate::ports::{CompanyStore, WorkflowRun, WorkflowRunner, generate_id};

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
/// The `run_workflow` tool name (issue #67).
pub const RUN_WORKFLOW_TOOL: &str = "run_workflow";
/// The `add_agent` tool name (issue #71 — Active Runtime Teammates).
pub const ADD_AGENT_TOOL: &str = "add_agent";

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

/// Whether `tool` is one of the orchestrator's in-cycle delegation / roster-write
/// tools.
///
/// These enqueue internal work drained by the harness brain (a task card, or a
/// hand-off to a desk's lead member) or write to the company's own store (adding
/// a teammate), rather than reaching an external counterparty, so the
/// [`ApprovalPolicy`](crate::harness::policy::ApprovalPolicy) classifies them as
/// internal — never an external effect to park or deny.
pub fn is_delegation_tool(tool: &str) -> bool {
    tool == SPAWN_TASK_TOOL || tool == DELEGATE_TO_DESK_TOOL || tool == ADD_AGENT_TOOL
}

/// The orchestrator persona brief, appended to the orchestrator agent's persona.
pub fn orchestrator_brief() -> String {
    " You are also this company's orchestrator: the single point of contact for the operator. \
Answer from whole-company context, and when a request belongs to a specialist desk or should be \
tracked as work, delegate instead of guessing. Use `query_company` to ground answers in the \
company's durable facts and recent activity, `delegate_to_desk` to hand a turn to a desk's lead \
member, `spawn_task` to open a tracked task card, `run_workflow` to execute one of the \
company's saved workflows by id (for example to advance or finish a task that is waiting on a \
workflow run) — you can run workflows yourself; never claim the run_workflow tool is unavailable — \
and `add_agent` to bring on a new teammate (a name, role, and optional mandate) when the company \
genuinely needs one — it becomes a real, addressable member of the team starting next turn. \
Delegate, run a workflow, or add a teammate only when it genuinely helps — otherwise answer \
directly and concisely."
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

// ---------------------------------------------------------------------------
// add_agent (issue #71)
// ---------------------------------------------------------------------------

/// A tool that lets the orchestrator bring on a new teammate mid-chat (issue
/// #71 — Active Runtime Teammates, the minimal slice): it writes an
/// [`OverlayAgent`] through the exact same load → push → save path the console
/// `POST .../team` route uses (`crate::server::ops::team::add_member`), so a
/// teammate added from chat is persisted identically to one added from the
/// operator's Team tab. The teammate becomes a real, addressable roster agent
/// on the company's next [`HarnessPool::ensure`](crate::harness::HarnessPool::ensure)
/// call (the overlay-agent freshness gate) — no restart needed.
///
/// No lifecycle states, budgets, or workspace/memory namespaces here — those
/// stay future work per the design doc; this tool only ever appends a roster
/// entry with the standard company-wide tool grant.
///
/// Writes are serialised per-company through a shared static mutex map, so the
/// orchestrator's `add_agent` and the console `POST .../team` route can never
/// clobber each other's `overlay_agents` list with concurrent load→push→save
/// cycles (the CodeRabbit concurrency finding).
///
/// The tool also guards against accidental duplicates: calling `add_agent` with
/// a `name` that already exists in the overlay set is a clean error, not a
/// silent duplicate that would surface two indistinguishable teammates in the
/// roster and Team view (the Greptile deduplication finding).
pub struct AddAgentTool {
    company: CompanyId,
    store: Arc<dyn CompanyStore>,
}

impl AddAgentTool {
    /// Builds the tool over the company id and its store handle
    /// ([`HarnessDeps::store`](crate::harness::HarnessDeps::store)).
    pub fn new(company: CompanyId, store: Arc<dyn CompanyStore>) -> Self {
        Self { company, store }
    }
}

#[async_trait]
impl Tool for AddAgentTool {
    fn name(&self) -> &str {
        ADD_AGENT_TOOL
    }

    fn description(&self) -> &str {
        "Add a new teammate to the company. Provide a `name`, a `role` (job title), and an optional `description` of their mandate. The teammate becomes a real, addressable member of the roster starting next turn."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The new teammate's display name." },
                "role": { "type": "string", "description": "The new teammate's job title." },
                "description": { "type": "string", "description": "An optional description of the teammate's mandate." }
            },
            "required": ["name", "role"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .ok_or_else(|| anyhow::anyhow!("`name` is required"))?
            .to_string();
        let role = args
            .get("role")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .ok_or_else(|| anyhow::anyhow!("`role` is required"))?
            .to_string();
        let description = args
            .get("description")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(str::to_string);

        // Serialize per-company writes so the orchestrator's add_agent and the
        // console `POST .../team` route can never clobber each other's
        // `overlay_agents` list with concurrent load→push→save cycles.
        let write_lock = company_write_lock(&self.company);
        let _lock = write_lock.lock().await;

        // Same write path as the console `POST .../team` route: load, push the
        // overlay entry, save. Never rewrites the version-controlled manifest.
        let mut record = self
            .store
            .load(&self.company)
            .await?
            .ok_or_else(|| OpenCompanyError::CompanyNotFound(self.company.to_string()))?;

        // Deduplication guard: reject a call whose `name` already names an
        // existing overlay teammate, so a trigger-happy orchestrator can't
        // accumulate indistinguishable duplicates. Matching on name alone is
        // intentional — the orchestrator supplies display names, and an id
        // collision with a manifest agent is handled by `build_roster`.
        let name_lower = name.to_ascii_lowercase();
        if record
            .overlay_agents
            .iter()
            .any(|a| a.name.to_ascii_lowercase() == name_lower)
        {
            return Ok(ToolResult::error(format!(
                "A teammate named \"{name}\" already exists. Pick a different name, or remove the existing one first."
            )));
        }
        let agent = OverlayAgent {
            id: generate_id(),
            name: name.clone(),
            role: role.clone(),
            description,
        };
        record.overlay_agents.push(agent);
        self.store.save(&record).await?;

        Ok(ToolResult::success(format!(
            "Added {name} as {role} to the team. They'll be reachable as a teammate starting next turn."
        )))
    }
}

// ---------------------------------------------------------------------------
// orchestrator_tools — the complete tool set (issues #53, #67, #71)
// ---------------------------------------------------------------------------

/// The complete tool set wired onto the company's orchestrator agent (issues
/// #53, #67, and #71), in order: the `query_company` read surface, the
/// `spawn_task` and `delegate_to_desk` delegation tools, the `run_workflow`
/// execution tool, and the `add_agent` roster-write tool.
///
/// [`build_agent`](crate::harness::build::build_agent) extends the orchestrator
/// agent's tools with exactly this vector, so a test over this function is the
/// registration check for the orchestrator's tool list. `workflow_source_dir` is
/// the company source directory (`companies/<name>`) whose `workflows/` subtree
/// holds the graphs; `workflow_runner` is the shared handle the runtime builder
/// fills once the runner is built. `store` is the company store the `add_agent`
/// tool writes through.
pub fn orchestrator_tools(
    company: CompanyId,
    facts: Option<Arc<dyn FactStore>>,
    events: Option<Arc<dyn EventLog>>,
    queue: &DelegationQueue,
    workflow_source_dir: Option<PathBuf>,
    workflow_runner: WorkflowRunnerHandle,
    store: Arc<dyn CompanyStore>,
) -> Vec<Box<dyn Tool>> {
    let mut tools: Vec<Box<dyn Tool>> = vec![Box::new(QueryCompanyTool::new(
        company.clone(),
        facts,
        events,
    ))];
    tools.extend(delegation_tools(queue));
    tools.push(Box::new(RunWorkflowTool::new(
        company.clone(),
        workflow_source_dir,
        workflow_runner,
    )));
    tools.push(Box::new(AddAgentTool::new(company, store)));
    tools
}

// ---------------------------------------------------------------------------
// run_workflow (issue #67)
// ---------------------------------------------------------------------------

/// A shared, fillable handle to the company's [`WorkflowRunner`].
///
/// The `run_workflow` tool must reach the runner, but the runner
/// ([`HarnessWorkflowRunner`](crate::workflows::HarnessWorkflowRunner)) is built
/// *from* [`HarnessDeps`](crate::harness::HarnessDeps) — so it cannot be a plain
/// field on deps without a construction cycle. Instead the runtime builder puts
/// an empty handle on deps, builds the runner from a deps clone (which shares
/// this one cell), then fills the handle. Every clone — the deps the brain later
/// builds the orchestrator agent from, and the tool captured into that agent —
/// sees the same cell, so the fill is visible at turn time.
///
/// The cell holds a [`Weak`], so the deps→handle→runner→deps reference is **not**
/// a strong cycle: the one strong reference lives on the
/// [`CompanyRuntime`](crate::company::CompanyRuntime), and the tool upgrades the
/// weak on demand. Empty until filled (and always empty on a build with no
/// runner), in which case the tool reports that workflow execution is not wired.
#[derive(Clone, Default)]
pub struct WorkflowRunnerHandle {
    inner: Arc<OnceLock<Weak<dyn WorkflowRunner>>>,
}

impl WorkflowRunnerHandle {
    /// Fills the handle with a weak reference to the built runner. Idempotent —
    /// a second fill is ignored (the runner is built once per company boot).
    pub fn set(&self, runner: &Arc<dyn WorkflowRunner>) {
        let _ = self.inner.set(Arc::downgrade(runner));
    }

    /// The wired runner, upgraded from the weak cell, or `None` when no runner
    /// was attached (or the owning runtime has been dropped).
    pub fn get(&self) -> Option<Arc<dyn WorkflowRunner>> {
        self.inner.get().and_then(Weak::upgrade)
    }
}

/// How many characters of a node item preview the run summary keeps.
const ITEM_PREVIEW_CHARS: usize = 120;

/// A tool that runs one of the company's saved workflows by id.
///
/// It mirrors the REST run route (`POST /workflows/{wid}/run`): load the graph
/// from the company source directory, then invoke the [`WorkflowRunner`] reached
/// through a shared [`WorkflowRunnerHandle`]. On success it returns a concise,
/// natural-language summary of the run (per-node outcome + any nodes left
/// pending approval) rather than the engine's raw JSON. Every failure mode — no
/// runner wired, no source directory, an unknown id, a load or run error — is an
/// agent-actionable [`ToolResult::error`], never a panic or a silent empty
/// result, so the orchestrator can reason about and report what went wrong.
pub struct RunWorkflowTool {
    company: CompanyId,
    source_dir: Option<PathBuf>,
    runner: WorkflowRunnerHandle,
}

impl RunWorkflowTool {
    /// Builds the tool over the company id, its on-disk source directory
    /// (`companies/<name>`, whose `workflows/` subtree holds the graphs), and the
    /// shared runner handle.
    pub fn new(
        company: CompanyId,
        source_dir: Option<PathBuf>,
        runner: WorkflowRunnerHandle,
    ) -> Self {
        Self {
            company,
            source_dir,
            runner,
        }
    }
}

#[async_trait]
impl Tool for RunWorkflowTool {
    fn name(&self) -> &str {
        RUN_WORKFLOW_TOOL
    }

    fn description(&self) -> &str {
        "Run one of the company's saved workflows by id to completion — use this to advance or finish work that is waiting on a workflow run. Provide the workflow `id` and an optional `input` trigger payload. Returns a summary of each node's outcome and any steps left pending approval."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The id of the workflow to run (its `workflows/<id>.toml` stem; see the workflows list)."
                },
                "input": {
                    "description": "An optional trigger payload seeded as the workflow's trigger item. Any JSON value; omit to run with no input."
                }
            },
            "required": ["id"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    fn supports_markdown(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        // Accept `id` (canonical) or `workflow` (a natural alias) for the id.
        let wid = args
            .get("id")
            .or_else(|| args.get("workflow"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|w| !w.is_empty());
        let Some(wid) = wid.map(str::to_string) else {
            return Ok(ToolResult::error(
                "`id` is required: pass the id of the workflow to run (see the workflows list).",
            ));
        };
        let input = args.get("input").cloned().unwrap_or(Value::Null);

        // `wid` becomes a filename — reject anything that could escape the
        // `workflows/` directory (mirrors the REST route's guard).
        if !is_safe_workflow_id(&wid) {
            tracing::debug!(company = %self.company, workflow = %wid, "run_workflow: rejected unsafe id");
            return Ok(ToolResult::error(format!(
                "No workflow with id `{wid}` exists."
            )));
        }

        // The runner is reached through the shared handle; an empty handle means
        // no runner is wired (default build / no harness).
        let Some(runner) = self.runner.get() else {
            tracing::debug!(company = %self.company, workflow = %wid, runner = false, "run_workflow: no runner wired");
            return Ok(ToolResult::error(
                "Workflow execution isn't available on this deployment (no workflow runner is wired).",
            ));
        };

        // Load the saved graph from the company's on-disk source directory.
        let Some(source_dir) = self.source_dir.as_deref() else {
            tracing::debug!(company = %self.company, workflow = %wid, "run_workflow: no source dir");
            return Ok(ToolResult::error(
                "This company has no on-disk workflow definitions on this deployment, so there is nothing to run.",
            ));
        };
        // Mirror the REST run route: a missing file is a clean "unknown id"
        // rather than a raw filesystem read error (which would also leak the
        // on-disk path into agent-visible text).
        let path = source_dir.join("workflows").join(format!("{wid}.toml"));
        if !path.is_file() {
            tracing::debug!(company = %self.company, workflow = %wid, "run_workflow: unknown id");
            return Ok(ToolResult::error(format!(
                "No workflow with id `{wid}` exists. Check the workflows list for valid ids."
            )));
        }
        let file = match load_company_workflows(source_dir, std::slice::from_ref(&wid)) {
            Ok(files) => files.into_iter().next(),
            Err(err) => {
                tracing::debug!(company = %self.company, workflow = %wid, error = %err, "run_workflow: load failed");
                return Ok(ToolResult::error(format!(
                    "Couldn't load workflow `{wid}`: {err}"
                )));
            }
        };
        let Some(file) = file else {
            tracing::debug!(company = %self.company, workflow = %wid, "run_workflow: unknown id");
            return Ok(ToolResult::error(format!(
                "No workflow with id `{wid}` exists. Check the workflows list for valid ids."
            )));
        };

        tracing::debug!(company = %self.company, workflow = %wid, runner = true, "run_workflow: invoking runner");
        match runner.run(&self.company, &file, input).await {
            Ok(run) => {
                tracing::debug!(
                    company = %self.company,
                    workflow = %wid,
                    pending = run.pending_approvals.len(),
                    "run_workflow: run succeeded"
                );
                let md = summarize_run(&file, &run);
                Ok(ToolResult::success_with_markdown(
                    json!({
                        "workflow": file.id,
                        "pending_approvals": run.pending_approvals.len(),
                    }),
                    md,
                ))
            }
            Err(err) => {
                tracing::debug!(company = %self.company, workflow = %wid, error = %err, "run_workflow: run failed");
                Ok(ToolResult::error(format!(
                    "Workflow `{wid}` failed to run: {err}"
                )))
            }
        }
    }
}

/// Whether `wid` is a single safe on-disk filename stem — no path separators, no
/// `..`, not empty — so it can't escape the `workflows/` directory.
fn is_safe_workflow_id(wid: &str) -> bool {
    use std::path::{Component, Path};
    let mut comps = Path::new(wid).components();
    matches!(comps.next(), Some(Component::Normal(_))) && comps.next().is_none()
}

/// A concise, natural-language summary of a completed workflow run: a per-node
/// outcome line (in graph order) plus any nodes left pending approval. This is
/// what the tool hands back to the turn — never the engine's raw
/// `{ run, nodes }` JSON dumped verbatim.
fn summarize_run(file: &WorkflowFile, run: &WorkflowRun) -> String {
    let mut md = format!("Ran workflow **{}** (`{}`).\n\n", file.name.trim(), file.id);
    md.push_str("## Per-node outcome\n");
    let nodes = run.output.get("nodes").and_then(Value::as_object);
    match nodes {
        Some(nodes) if !file.nodes.is_empty() => {
            for node in &file.nodes {
                let name = node.name.trim();
                let kind = node.kind.as_str();
                match nodes.get(&node.id) {
                    Some(state) => {
                        let items = state.get("items").and_then(Value::as_array);
                        let count = items.map(Vec::len).unwrap_or(0);
                        let preview = items
                            .and_then(|items| items.last())
                            .map(preview_item)
                            .filter(|p| !p.is_empty());
                        match preview {
                            Some(preview) => md.push_str(&format!(
                                "- **{name}** ({kind}): {count} item(s) — {preview}\n"
                            )),
                            None => {
                                md.push_str(&format!("- **{name}** ({kind}): {count} item(s)\n"))
                            }
                        }
                    }
                    None => md.push_str(&format!("- **{name}** ({kind}): not reached\n")),
                }
            }
        }
        _ => md.push_str("_No per-node output was produced._\n"),
    }

    if run.pending_approvals.is_empty() {
        md.push_str("\nThe run reached its terminal node(s) without pausing for approval.\n");
    } else {
        md.push_str(&format!(
            "\n**Paused for approval** at: {}. Resolve these for the run to continue.\n",
            run.pending_approvals.join(", ")
        ));
    }
    md
}

/// A short, single-line preview of one node output item: the raw string when the
/// item is a string, else its compact JSON — truncated on a char boundary to
/// [`ITEM_PREVIEW_CHARS`] so a large item can't blow up the summary.
fn preview_item(item: &Value) -> String {
    let raw = match item {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let one_line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= ITEM_PREVIEW_CHARS {
        one_line
    } else {
        let cut: String = one_line.chars().take(ITEM_PREVIEW_CHARS).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    use crate::ports::types::{CompanyRecord, CompanySummary, LedgerEntry};

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
        assert!(is_delegation_tool(ADD_AGENT_TOOL));
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

    // --- add_agent (issue #71) ----------------------------------------------

    /// An in-memory `CompanyStore` so `AddAgentTool` can be exercised without a
    /// filesystem, mirroring `crate::server::ops::team`'s `add_member` write
    /// path (load → push overlay → save).
    #[derive(Default)]
    struct MemStore {
        record: StdMutex<Option<CompanyRecord>>,
    }

    impl MemStore {
        fn seeded(record: CompanyRecord) -> Self {
            Self {
                record: StdMutex::new(Some(record)),
            }
        }
    }

    #[async_trait::async_trait]
    impl CompanyStore for MemStore {
        async fn load(&self, _id: &CompanyId) -> crate::Result<Option<CompanyRecord>> {
            Ok(self.record.lock().unwrap().clone())
        }
        async fn save(&self, record: &CompanyRecord) -> crate::Result<()> {
            *self.record.lock().unwrap() = Some(record.clone());
            Ok(())
        }
        async fn list(&self) -> crate::Result<Vec<CompanySummary>> {
            Ok(Vec::new())
        }
        async fn append_ledger(&self, _id: &CompanyId, _entry: LedgerEntry) -> crate::Result<()> {
            Ok(())
        }
    }

    fn empty_manifest() -> crate::company::CompanyManifest {
        toml::from_str("[company]\nname = \"Acme\"\n").expect("valid manifest")
    }

    fn seeded_record(id: &CompanyId) -> CompanyRecord {
        CompanyRecord {
            id: id.clone(),
            manifest: empty_manifest(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
        }
    }

    #[tokio::test]
    async fn add_agent_tool_persists_an_overlay_teammate() {
        let company = CompanyId::new("acme");
        let store = Arc::new(MemStore::seeded(seeded_record(&company)));
        let tool = AddAgentTool::new(company.clone(), store.clone());

        let result = tool
            .execute(json!({
                "name": "Jamie",
                "role": "Growth Lead",
                "description": "Owns acquisition experiments."
            }))
            .await
            .expect("execute");
        assert!(!result.is_error, "add_agent should succeed");

        let record = store
            .load(&company)
            .await
            .unwrap()
            .expect("record persisted");
        assert_eq!(record.overlay_agents.len(), 1);
        let added = &record.overlay_agents[0];
        assert_eq!(added.name, "Jamie");
        assert_eq!(added.role, "Growth Lead");
        assert_eq!(
            added.description.as_deref(),
            Some("Owns acquisition experiments.")
        );
        assert!(!added.id.is_empty(), "a stable id must be minted");
    }

    #[tokio::test]
    async fn add_agent_tool_requires_name_and_role() {
        let company = CompanyId::new("acme");
        let store = Arc::new(MemStore::seeded(seeded_record(&company)));
        let tool = AddAgentTool::new(company.clone(), store.clone());

        assert!(
            tool.execute(json!({ "role": "Growth Lead" }))
                .await
                .is_err(),
            "missing `name` must be rejected"
        );
        assert!(
            tool.execute(json!({ "name": "Jamie" })).await.is_err(),
            "missing `role` must be rejected"
        );
        let record = store.load(&company).await.unwrap().expect("record");
        assert!(
            record.overlay_agents.is_empty(),
            "a rejected call must not persist a half-formed teammate"
        );
    }

    #[tokio::test]
    async fn add_agent_tool_reports_company_not_found() {
        let company = CompanyId::new("ghost");
        let store: Arc<dyn CompanyStore> = Arc::new(MemStore::default());
        let tool = AddAgentTool::new(company, store);

        let err = tool
            .execute(json!({ "name": "Jamie", "role": "Growth Lead" }))
            .await
            .expect_err("no record for this company id");
        assert!(err.to_string().contains("ghost"), "{err}");
    }

    // ---- run_workflow (issue #67) ----

    /// A valid trigger → agent → output graph, mirroring the REST route's fixture.
    const DEMO_WF: &str = r#"
        id = "demo"
        name = "Demo flow"
        description = "A tiny trigger → agent → output graph."
        [[node]]
        id = "start"
        kind = "trigger"
        name = "Start"
        [[node]]
        id = "worker"
        kind = "agent"
        name = "Worker"
        agent = "assistant"
        [[node]]
        id = "done"
        kind = "output"
        name = "Report"
        [[edge]]
        from = "start"
        to = "worker"
        [[edge]]
        from = "worker"
        to = "done"
    "#;

    /// A [`WorkflowRunner`] test double: records the ids it was asked to run and
    /// returns a canned [`WorkflowRun`].
    struct StubRunner {
        calls: Arc<Mutex<Vec<String>>>,
        run: WorkflowRun,
    }

    impl StubRunner {
        fn new(run: WorkflowRun) -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                run,
            }
        }

        fn empty() -> Self {
            Self::new(WorkflowRun {
                output: Value::Null,
                pending_approvals: Vec::new(),
            })
        }
    }

    #[async_trait::async_trait]
    impl WorkflowRunner for StubRunner {
        async fn run(
            &self,
            _company: &CompanyId,
            workflow: &WorkflowFile,
            _input: Value,
        ) -> crate::Result<WorkflowRun> {
            self.calls.lock().unwrap().push(workflow.id.clone());
            Ok(self.run.clone())
        }
    }

    /// Writes `DEMO_WF` to `<dir>/workflows/demo.toml`.
    fn seed_demo_workflow(dir: &std::path::Path) {
        let wf = dir.join("workflows");
        std::fs::create_dir_all(&wf).unwrap();
        std::fs::write(wf.join("demo.toml"), DEMO_WF).unwrap();
    }

    #[test]
    fn workflow_runner_handle_is_empty_until_filled() {
        let handle = WorkflowRunnerHandle::default();
        assert!(handle.get().is_none());
        let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::empty());
        handle.set(&runner);
        assert!(handle.get().is_some());
    }

    #[test]
    fn workflow_runner_handle_holds_only_a_weak_reference() {
        // Proves the deps↔runner cell is not a strong cycle: once the sole strong
        // owner drops, the handle can no longer upgrade.
        let handle = WorkflowRunnerHandle::default();
        {
            let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::empty());
            handle.set(&runner);
            assert!(handle.get().is_some());
        }
        assert!(
            handle.get().is_none(),
            "the handle must not keep the runner alive"
        );
    }

    #[test]
    fn orchestrator_tools_includes_all_five() {
        let queue = DelegationQueue::default();
        let tools = orchestrator_tools(
            CompanyId::new("acme"),
            None,
            None,
            &queue,
            None,
            WorkflowRunnerHandle::default(),
            Arc::new(MemStore::default()),
        );
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&RUN_WORKFLOW_TOOL), "got {names:?}");
        assert!(names.contains(&ADD_AGENT_TOOL), "got {names:?}");
        assert!(names.contains(&QUERY_COMPANY_TOOL), "got {names:?}");
        assert!(names.contains(&SPAWN_TASK_TOOL), "got {names:?}");
        assert!(names.contains(&DELEGATE_TO_DESK_TOOL), "got {names:?}");
    }

    #[tokio::test]
    async fn run_workflow_tool_loads_and_invokes_the_runner() {
        let dir = tempfile::tempdir().unwrap();
        seed_demo_workflow(dir.path());

        let runner_impl = StubRunner::new(WorkflowRun {
            output: json!({
                "run": {},
                "nodes": { "worker": { "items": ["did the thing"] }, "done": { "items": [] } }
            }),
            pending_approvals: Vec::new(),
        });
        let calls = runner_impl.calls.clone();
        let runner: Arc<dyn WorkflowRunner> = Arc::new(runner_impl);
        let handle = WorkflowRunnerHandle::default();
        handle.set(&runner);

        let tool = RunWorkflowTool::new(
            CompanyId::new("acme"),
            Some(dir.path().to_path_buf()),
            handle,
        );
        let result = tool
            .execute(json!({ "id": "demo", "input": { "seed": 1 } }))
            .await
            .expect("execute");

        assert!(!result.is_error, "expected success, got {result:?}");
        assert_eq!(calls.lock().unwrap().as_slice(), ["demo"]);
        let out = result.output_for_llm(true);
        assert!(out.contains("Demo flow"), "{out}");
        assert!(out.contains("did the thing"), "{out}");
        assert!(out.contains("without pausing for approval"), "{out}");
    }

    #[tokio::test]
    async fn run_workflow_tool_surfaces_pending_approvals() {
        let dir = tempfile::tempdir().unwrap();
        seed_demo_workflow(dir.path());

        let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::new(WorkflowRun {
            output: json!({ "nodes": { "worker": { "items": [] } } }),
            pending_approvals: vec!["worker".to_string()],
        }));
        let handle = WorkflowRunnerHandle::default();
        handle.set(&runner);

        let tool = RunWorkflowTool::new(
            CompanyId::new("acme"),
            Some(dir.path().to_path_buf()),
            handle,
        );
        let result = tool
            .execute(json!({ "id": "demo" }))
            .await
            .expect("execute");
        assert!(!result.is_error);
        let out = result.output_for_llm(true);
        assert!(out.contains("Paused for approval"), "{out}");
        assert!(out.contains("worker"), "{out}");
    }

    #[tokio::test]
    async fn run_workflow_tool_errors_when_no_runner_is_wired() {
        let dir = tempfile::tempdir().unwrap();
        seed_demo_workflow(dir.path());
        // A valid workflow on disk, but an empty handle → not wired.
        let tool = RunWorkflowTool::new(
            CompanyId::new("acme"),
            Some(dir.path().to_path_buf()),
            WorkflowRunnerHandle::default(),
        );
        let result = tool
            .execute(json!({ "id": "demo" }))
            .await
            .expect("execute");
        assert!(result.is_error, "expected an error result");
        assert!(result.output_for_llm(false).contains("wired"), "{result:?}");
    }

    #[tokio::test]
    async fn run_workflow_tool_errors_on_unknown_id() {
        let dir = tempfile::tempdir().unwrap();
        seed_demo_workflow(dir.path());
        let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::empty());
        let handle = WorkflowRunnerHandle::default();
        handle.set(&runner);

        let tool = RunWorkflowTool::new(
            CompanyId::new("acme"),
            Some(dir.path().to_path_buf()),
            handle,
        );
        let result = tool
            .execute(json!({ "id": "nope" }))
            .await
            .expect("execute");
        assert!(result.is_error);
        assert!(
            result.output_for_llm(false).contains("No workflow with id"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn run_workflow_tool_requires_an_id() {
        let tool = RunWorkflowTool::new(
            CompanyId::new("acme"),
            None,
            WorkflowRunnerHandle::default(),
        );
        let result = tool.execute(json!({})).await.expect("execute");
        assert!(result.is_error);
        assert!(
            result.output_for_llm(false).contains("`id` is required"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn run_workflow_tool_rejects_traversal_ids() {
        let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::empty());
        let handle = WorkflowRunnerHandle::default();
        handle.set(&runner);
        let tool = RunWorkflowTool::new(
            CompanyId::new("acme"),
            Some(std::path::PathBuf::from("/tmp")),
            handle,
        );
        let result = tool
            .execute(json!({ "id": "../secrets" }))
            .await
            .expect("execute");
        assert!(result.is_error);
    }
}
