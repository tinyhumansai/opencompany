//! Shared id, enum, and payload types referenced across more than one port.
//!
//! Types local to a single port live beside that port's trait; everything the
//! kernel threads between ports (ids, events, effects, cycle payloads) lives
//! here. Every type derives `Clone, Debug, Serialize, Deserialize` so it can
//! round-trip through JSONL persistence and the HTTP surface.
//!
//! Field lists are Phase-1-minimal: the port contract in
//! `docs/spec/runtime/ports.md` binds trait and method names, and permits
//! payload fields to evolve within Phase 1.

use std::ops::Range;

use serde::{Deserialize, Serialize};

use crate::company::CompanyManifest;
use crate::ports::ids::{generate_id, now_millis};

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Stable identifier for a company (typically a slug of its name).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CompanyId(String);

impl CompanyId {
    /// Wraps an existing id string (e.g. a manifest-derived slug).
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Mints a fresh process-unique company id.
    pub fn generate() -> Self {
        Self(generate_id())
    }
}

impl From<String> for CompanyId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl AsRef<str> for CompanyId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CompanyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Monotonic sequence number for an event within a company's log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EventSeq(u64);

impl EventSeq {
    /// Wraps a raw sequence value.
    pub fn new(seq: u64) -> Self {
        Self(seq)
    }

    /// The underlying sequence value.
    pub fn value(self) -> u64 {
        self.0
    }
}

impl From<u64> for EventSeq {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl std::fmt::Display for EventSeq {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Identifier for a parked effect awaiting operator approval.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ApprovalId(String);

impl ApprovalId {
    /// Wraps an existing approval id string.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Mints a fresh approval id (called by the gate at park time).
    pub fn generate() -> Self {
        Self(generate_id())
    }
}

impl From<String> for ApprovalId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl AsRef<str> for ApprovalId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ApprovalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Content address of a stored context chunk.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChunkAddr(String);

impl ChunkAddr {
    /// Wraps a content-address string (minted by the context store).
    pub fn new(addr: impl Into<String>) -> Self {
        Self(addr.into())
    }
}

impl From<String> for ChunkAddr {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl AsRef<str> for ChunkAddr {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ChunkAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Opaque per-company secret value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretValue(pub String);

impl SecretValue {
    /// Borrows the underlying secret string.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// Actors and verdicts
// ---------------------------------------------------------------------------

/// Who performed an action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActorKind {
    /// The human operator.
    Operator,
    /// The runtime itself (timers, boot replay).
    System,
    /// An autonomous agent inside the company.
    Agent,
    /// A human collaborator of the company. The user's id lives in
    /// [`Actor::id`].
    ///
    /// Fieldless on purpose: `ActorKind` is `Copy`, and a variant carrying a
    /// `String` would silently take that away from every existing holder.
    User,
}

/// An identified actor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    /// The category of actor.
    pub kind: ActorKind,
    /// A stable id for the actor within its category.
    pub id: String,
}

/// An operator's resolution of a parked approval.
///
/// The HTTP body uses the lowercase strings `"approve"` / `"deny"`. The
/// `edit` path named in `approvals.md` is intentionally absent — the api.md
/// body defines no such verdict in Phase 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Approve and execute the parked effect.
    Approve,
    /// Deny and discard the parked effect.
    Deny,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// An external stimulus fed into a company's cycle loop.
///
/// Serialized internally-tagged under `kind` so each JSONL line is
/// self-describing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum CompanyEvent {
    /// A human sent a chat message.
    OperatorMessage {
        /// The message text.
        text: String,
        /// Who sent it.
        ///
        /// `None` on events journaled before per-user auth existed, and on
        /// sends made with an operator/platform credential, which have no
        /// person behind them. Both are attributed to "operator" on read.
        ///
        /// `#[serde(default)]` is what lets every already-persisted event load;
        /// `skip_serializing_if` is what keeps an unattributed event
        /// serializing byte-for-byte as it did before this field existed, so
        /// export/import and the cross-backend round-trip need no migration.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<Actor>,
        /// The desk / chat thread the message targets (issue #53), so the
        /// orchestrator brain can route an addressed message to that desk's lead
        /// member and journal replies against it. `None` on an unaddressed
        /// message (routed to the orchestrator) and on every event journaled
        /// before this field existed. Like `by`, `skip_serializing_if` keeps a
        /// pre-existing event byte-identical, so no stored record needs
        /// migrating.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chat: Option<String>,
    },
    /// An inbound webhook fired.
    WebhookReceived {
        /// The channel the webhook arrived on.
        channel: String,
        /// The raw webhook body.
        body: serde_json::Value,
    },
    /// A cron schedule fired.
    ScheduleFired {
        /// The cron expression that fired.
        cron: String,
        /// The prompt delivered to the company.
        prompt: String,
    },
    /// An A2A task was received from another agent.
    A2aTaskReceived {
        /// The sending agent's address.
        from: String,
        /// The task payload.
        task: serde_json::Value,
    },
    /// An operator resolved a parked approval.
    ApprovalResolved {
        /// The approval that was resolved.
        approval_id: ApprovalId,
        /// The operator's verdict.
        verdict: Verdict,
        /// Who resolved it.
        by: Actor,
    },
    /// Feedback was filed against the company.
    FeedbackFiled {
        /// Free-form feedback text.
        note: String,
    },
    /// A payment was received.
    PaymentReceived {
        /// The amount received, in USD.
        amount_usd: f64,
        /// A memo describing the payment.
        memo: String,
    },
    /// The company's lifecycle state changed (e.g. `running` → `paused`),
    /// recorded with the acting actor for the audit trail.
    LifecycleChanged {
        /// The previous lifecycle state.
        from: String,
        /// The new lifecycle state.
        to: String,
        /// Who performed the transition.
        by: Actor,
    },
    /// An agent replied in a desk/chat. Journaled by the harness/chat layer so
    /// the GraphQL `Chat.history` resolver (WS2c) can read replies back
    /// alongside the operator messages that prompted them.
    AgentReply {
        /// The desk / group-chat the reply belongs to.
        chat_id: String,
        /// The agent that produced the reply.
        agent_id: String,
        /// The reply text.
        text: String,
        /// The scrubbed processing steps behind this reply — the same
        /// per-bubble [`TurnStep`] timeline the live turn streams and the POST
        /// `/chat` body carries — persisted here so a desk history reload
        /// rehydrates the tool-call timeline, not just the text. Additive:
        /// omitted-when-empty on the wire, so every prior log (and every
        /// non-harness reply, which folds no steps) round-trips byte-identical.
        /// Never carries raw tool arguments, output, or call ids — only the
        /// scrubbed shape (see [`crate::harness::steps`]).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        steps: Vec<TurnStep>,
        /// The board task this reply was produced by, when it came out of a
        /// [`TaskDispatched`](Self::TaskDispatched) cycle rather than a chat
        /// turn (issue #185).
        ///
        /// This is the correlation key the per-task timeline filters on: the
        /// journal is company-scoped, so without it a dispatch's reply cannot
        /// be told apart from every other desk reply in the log.
        ///
        /// `None` for an ordinary chat reply and for every event journaled
        /// before this field existed. Additive in exactly the same way as
        /// [`OperatorMessage`](Self::OperatorMessage)'s `by` / `chat`:
        /// `#[serde(default)]` is what lets an already-persisted log load, and
        /// `skip_serializing_if` is what keeps an untagged reply serializing
        /// byte-for-byte as it did before this field existed, so no stored
        /// record needs migrating.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
    },
    /// The Operator deleted a durable memory fact. Journaled for the audit trail
    /// per the Operator-rights section of `docs/spec/company-brain/memory.md`.
    MemoryFactDeleted {
        /// The id of the deleted fact.
        fact_id: String,
    },
    /// A board task was moved into `in_progress` and dispatched to its assignee
    /// for one agent turn on the embedded runtime. Journaled so the dispatch is
    /// auditable and replayable. Only the `openhuman` `HarnessBrain` acts on it;
    /// the default build's `EchoBrain` ignores it, so the board stays inert
    /// without the harness.
    TaskDispatched {
        /// The id of the dispatched task card.
        task_id: String,
    },
    /// An agent's MCP tool call failed during a turn, journaled by the harness
    /// so the operator has an audit trail of which server/tool broke and why.
    /// The `message` is always **scrubbed** at the source (the
    /// `OcMcpCallTool` → `HarnessBrain` drain path), so this record can never
    /// carry a credential, response body, or URL query string. Additive: old
    /// logs never contain it, and its presence doesn't change how any existing
    /// variant serializes (same `by`/`chat` precedent).
    McpCallFailed {
        /// The MCP server the failing call targeted.
        server: String,
        /// The remote tool the agent tried to call.
        tool: String,
        /// A stable status code (e.g. `credential_required`, `tool_call_rejected`).
        status: String,
        /// A short, scrubbed, operator-facing message.
        message: String,
        /// The board task whose dispatch turn made the failing call, when the
        /// failure happened inside a [`TaskDispatched`](Self::TaskDispatched)
        /// cycle (issue #185). Lets a task's failed tool calls be filtered out
        /// of the company-scoped journal onto its own timeline.
        ///
        /// `None` for a failure raised during a chat turn and for every event
        /// journaled before this field existed. Same additive contract as
        /// [`AgentReply`](Self::AgentReply)'s `task_id`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
    },
    /// A new workflow graph was authored and enabled (issue #112), from either
    /// the console `POST …/workflows` route or the orchestrator's
    /// `create_workflow` tool. Journaled best-effort **after** the graph is
    /// persisted and enabled, so it records a completed create — a journal
    /// failure never rolls the create back. Additive: old logs never carry it,
    /// and its presence doesn't change how any existing variant serializes.
    WorkflowCreated {
        /// The new workflow's id (its `workflows/<id>.toml` stem).
        workflow_id: String,
        /// The new workflow's display name.
        name: String,
        /// Who authored it, when known. `None` when created by a surface that
        /// carries no attributed actor (the current create paths); kept as an
        /// `Option` so a future attributed create needs no migration, mirroring
        /// [`OperatorMessage`](Self::OperatorMessage)'s `by`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<Actor>,
    },
    /// An operator steered an in-flight run — paused, cancelled, or redirected a
    /// dispatched task (or cancelled a delegation) from chat (issue #111).
    /// Journaled best-effort **after** the steer is accepted by the in-flight
    /// registry, so it records an accepted operator control action. Additive: old
    /// logs never carry it, and its presence doesn't change how any existing
    /// variant serializes (same `by` / skip-if-none precedent as
    /// [`WorkflowCreated`](Self::WorkflowCreated)).
    TaskSteered {
        /// The steered run's key — the board task id, or a delegation run id.
        task_id: String,
        /// The action taken, as a stable wire word: `pause` / `cancel` /
        /// `redirect`.
        action: String,
        /// The operator's redirect instruction (codepoint-capped), present only
        /// on a `redirect`. Omitted from the wire otherwise, and — being
        /// operator-authored free text — never projected onto the SSE stream.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instruction: Option<String>,
        /// Who steered, when known. `None` on a surface that carries no attributed
        /// actor; kept `Option` so a future attributed steer needs no migration,
        /// mirroring [`OperatorMessage`](Self::OperatorMessage)'s `by`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<Actor>,
    },
    /// A dispatched board task finished its run (issue #185) — the terminal
    /// anchor a per-task timeline ends on and a lineage rollup counts.
    ///
    /// Journaled by the harness at the end of a
    /// [`TaskDispatched`](Self::TaskDispatched) cycle, **after** the card's
    /// landing column has been persisted, so it always records a completed
    /// run. "Completed" here means *the run stopped*, not *it succeeded*: a
    /// cancelled, paused, or failed dispatch emits one too, and `column`
    /// carries where the card actually landed. Without that, a timeline could
    /// not distinguish "still running" from "finished badly".
    ///
    /// This issue only *adds* the event. #171's done-transition can consume
    /// it; nothing here writes the board column off the back of it.
    ///
    /// Additive: old logs never carry it, and its presence doesn't change how
    /// any existing variant serializes.
    DeskTaskCompleted {
        /// The completed task card's id.
        task_id: String,
        /// The desk / agent that ran it — the resolved responder, not the
        /// card's raw `assignee` (which may name nobody on the roster).
        desk: String,
        /// The run's operator-facing result text.
        ///
        /// This is the agent's own reply (or a short `dispatch failed: …` /
        /// cancellation line), which is the same text already written into the
        /// card's note — never raw tool output, arguments, or call ids.
        output: String,
        /// The board column the card landed in: `in_review` on a normal
        /// finish, `backlog` on a failure or cancellation, `paused` on a
        /// pause. Lets a reader tell a successful run from a stopped one
        /// without re-deriving it from `output`.
        column: String,
    },
}

/// A `CompanyEvent` durably appended to the log with its sequence and time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredEvent {
    /// The event's monotonic sequence number.
    pub seq: EventSeq,
    /// The company the event belongs to.
    pub company: CompanyId,
    /// The event payload.
    pub event: CompanyEvent,
    /// Epoch-millis timestamp the event was appended.
    pub at_millis: u64,
}

// ---------------------------------------------------------------------------
// Effects, groups, and dispositions
// ---------------------------------------------------------------------------

/// The supervised-policy taxonomy an effect falls into.
///
/// Not named binding in `ports.md`, but drives `ApprovalGate` evaluation under
/// the supervised policy mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EffectGroup {
    /// Spending money.
    Spend,
    /// Sending a message to a counterparty.
    Send,
    /// Signing or filing a document.
    Sign,
    /// Publishing externally.
    Publish,
    /// Hiring or contracting.
    Hire,
    /// Touching the company's identity.
    Identity,
    /// Anything not otherwise classified.
    Other,
}

/// A side effect the brain wants to perform, submitted to the approval gate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Effect {
    /// The dotted effect kind, e.g. `payment.send`.
    pub kind: String,
    /// The supervised taxonomy group.
    pub group: EffectGroup,
    /// The USD amount involved, if any.
    pub amount_usd: Option<f64>,
    /// Whether this effect continues an established thread.
    pub established_thread: bool,
    /// Whether the counterparty is being contacted for the first time.
    pub first_time_counterparty: bool,
    /// Effect-specific payload.
    pub payload: serde_json::Value,
}

impl Effect {
    /// The dotted effect kind.
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// The supervised taxonomy group.
    pub fn group(&self) -> EffectGroup {
        self.group
    }

    /// The USD amount involved, if any.
    pub fn amount_usd(&self) -> Option<f64> {
        self.amount_usd
    }

    /// Whether this effect continues an established thread.
    pub fn is_established_thread(&self) -> bool {
        self.established_thread
    }

    /// Whether the counterparty is new.
    pub fn is_first_time_counterparty(&self) -> bool {
        self.first_time_counterparty
    }
}

/// How an emitted effect was dispatched by the gate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum EffectDisposition {
    /// The effect was executed immediately.
    Executed,
    /// The effect was parked and awaits operator approval.
    PendingApproval(ApprovalId),
    /// The effect was denied by policy.
    Denied {
        /// Why the effect was denied.
        reason: String,
    },
}

/// The gate's verdict on an effect, minted without an id.
///
/// Matches the bare `evaluate` return in `ports.md`; the `ApprovalId` for a
/// `RequireApproval` decision is minted separately by `park`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyDecision {
    /// Execute the effect now.
    Allow,
    /// Park the effect for operator approval.
    RequireApproval,
    /// Reject the effect outright.
    Deny,
}

// ---------------------------------------------------------------------------
// Cycle payloads
// ---------------------------------------------------------------------------

/// A compressed summary of one completed cycle, carried as history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompressedTrace {
    /// The cycle this trace summarizes.
    pub cycle_id: String,
    /// A short natural-language summary.
    pub summary: String,
    /// Epoch-millis timestamp the trace was produced.
    pub at_millis: u64,
}

impl CompressedTrace {
    /// Builds a trace stamped with the current time.
    pub fn now(cycle_id: impl Into<String>, summary: impl Into<String>) -> Self {
        Self {
            cycle_id: cycle_id.into(),
            summary: summary.into(),
            at_millis: now_millis(),
        }
    }
}

/// Metadata for a context chunk, returned by `ContextStore::list`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChunkMeta {
    /// The chunk's content address.
    pub addr: ChunkAddr,
    /// The chunk's logical label/prefix key.
    pub label: String,
    /// The chunk's length in bytes.
    pub len: usize,
    /// Epoch-millis when the chunk was stored (`0` for rows written before
    /// backends started stamping, so old data reads as "unknown" rather than
    /// as the epoch).
    ///
    /// This is what lets the Brain's "Last updated" stat move when agents write
    /// memory — they write only through this port, never to the `FactStore`
    /// (see `server::ops::memory::memory_stats`).
    ///
    /// Chunks are append-only and never rewritten, so this only ever moves for
    /// a *new* chunk. Backends differ on a re-`put` of an identical body —
    /// sqlite/mongo dedupe on the content address and keep the first write,
    /// the fs index appends a second line — so read freshness as the max
    /// across chunks rather than assuming one row per body.
    #[serde(default)]
    pub stored_at_millis: u64,
}

/// A single ledger movement.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LedgerEntry {
    /// Epoch-millis timestamp of the entry.
    pub at_millis: u64,
    /// The dotted entry kind, e.g. `inference.spend`.
    pub kind: String,
    /// The signed USD amount.
    pub amount_usd: f64,
    /// A human-readable memo.
    pub memo: String,
}

/// Token **and cost** accounting for a cycle — what the runtime meters onto the
/// Usage surface after the brain returns.
///
/// The cost fields carry no `Eq` (they are `f64`), so this type is `PartialEq`
/// only. Both are `#[serde(default)]` so a peer that predates them still
/// decodes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Input tokens consumed.
    pub input: u64,
    /// Output tokens produced.
    pub output: u64,
    /// Input tokens served from the provider's KV cache.
    #[serde(default)]
    pub cached_input: u64,
    /// Best-available USD cost for the cycle. Zero when the path reports tokens
    /// but bills elsewhere (the managed `/openai/v1` passthrough echoes no USD).
    #[serde(default)]
    pub cost_usd: f64,
}

impl TokenUsage {
    /// Whether the cycle moved no tokens and cost nothing — the guard that keeps
    /// an idle or offline cycle from writing a meaningless usage sample.
    ///
    /// Includes [`Self::cached_input`]: a cache-served pass is real usage even
    /// if a provider ever reported it without fresh input tokens.
    pub fn is_zero(&self) -> bool {
        self.input == 0 && self.output == 0 && self.cached_input == 0 && self.cost_usd == 0.0
    }

    /// Folds another total into this one — how a brain accumulates the usage of
    /// several passes into one cycle total. Token counts saturate rather than
    /// wrap so a bogus peer value can never underflow the meter.
    pub fn fold(&mut self, other: &TokenUsage) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cached_input = self.cached_input.saturating_add(other.cached_input);
        self.cost_usd += other.cost_usd;
    }
}

/// Everything the brain needs to run one cycle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CycleRequest {
    /// Unique id for this cycle.
    pub cycle_id: String,
    /// The company running the cycle.
    pub company_id: CompanyId,
    /// The batch of events driving this cycle.
    pub events: Vec<CompanyEvent>,
    /// The [`EventLog`](crate::ports::EventLog) sequence of each event in
    /// [`Self::events`], positionally aligned. Empty when a caller builds a
    /// request without threading seqs (a brain then falls back to the event's
    /// index); the runtime always populates it so hosted cognition can key its
    /// idempotent `POST /events` on the durable log seq.
    #[serde(default)]
    pub event_seqs: Vec<EventSeq>,
    /// Compressed traces of prior cycles.
    pub compressed_history: Vec<CompressedTrace>,
    /// The company roster (agent ids).
    pub roster: Vec<String>,
    /// The context index available to the brain.
    pub context_index: Vec<ChunkMeta>,
}

/// The brain's output from one cycle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CycleResult {
    /// Messages to emit on channels.
    pub channel_responses: Vec<OutboundMessage>,
    /// New compressed traces to persist.
    pub new_traces: Vec<CompressedTrace>,
    /// Ledger movements produced this cycle.
    pub ledger_deltas: Vec<LedgerEntry>,
    /// Token accounting for the cycle.
    pub token_usage: TokenUsage,
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

/// A brain request to invoke a tool.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// The tool name.
    pub tool: String,
    /// The tool arguments.
    pub args: serde_json::Value,
}

/// The result of a tool invocation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    /// Whether the tool succeeded.
    pub ok: bool,
    /// The tool's structured output.
    pub output: serde_json::Value,
}

/// A tool advertised in a company's catalog.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// The tool name.
    pub name: String,
    /// What the tool does.
    pub description: String,
    /// JSON schema for the tool's arguments.
    pub input_schema: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Context store
// ---------------------------------------------------------------------------

/// A chunk of context to store.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContextChunk {
    /// A logical label/prefix key for the chunk.
    pub label: String,
    /// The chunk body.
    pub body: String,
}

/// A search hit from `ContextStore::search`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChunkHit {
    /// The matching chunk's address.
    pub addr: ChunkAddr,
    /// A snippet of the match.
    pub snippet: String,
    /// A relevance score in `[0, 1]`.
    pub score: f64,
}

/// A context operation the brain issues through `CycleHost::context_op`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ContextOp {
    /// Store a chunk, returning its address.
    Put(ContextChunk),
    /// List chunks under a prefix.
    List {
        /// The prefix to list under.
        prefix: String,
    },
    /// Read a chunk (optionally a byte range) as text.
    Peek {
        /// The chunk to read.
        addr: ChunkAddr,
        /// An optional byte range within the chunk.
        range: Option<Range<usize>>,
    },
    /// Search chunks for a query.
    Search {
        /// The search query.
        query: String,
        /// The maximum number of hits to return.
        limit: usize,
    },
}

/// The result of a `ContextOp`, one variant per operation return type.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ContextOpResult {
    /// A `Put` returned this address.
    Addr(ChunkAddr),
    /// A `List` returned this metadata.
    Metas(Vec<ChunkMeta>),
    /// A `Peek` returned this text.
    Text(String),
    /// A `Search` returned these hits.
    Hits(Vec<ChunkHit>),
}

// ---------------------------------------------------------------------------
// Memory store
// ---------------------------------------------------------------------------

/// The result of a completed background task.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskResult {
    /// The task id.
    pub task_id: String,
    /// Whether the task succeeded.
    pub ok: bool,
    /// The task output.
    pub output: serde_json::Value,
}

/// A policy for evicting stale memory.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum EvictionPolicy {
    /// Keep only the most recent `n` traces.
    KeepRecent {
        /// How many traces to retain.
        n: usize,
    },
    /// Evict traces older than the given epoch-millis.
    OlderThan {
        /// The cutoff timestamp in epoch millis.
        before_millis: u64,
    },
}

// ---------------------------------------------------------------------------
// Channels
// ---------------------------------------------------------------------------

/// A message arriving on a channel.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InboundMessage {
    /// The channel the message arrived on.
    pub channel: String,
    /// The message text.
    pub text: String,
    /// Who sent it.
    pub from: Actor,
}

/// Channel-specific reply addressing for an [`OutboundMessage`].
///
/// Carries the chat/thread a reply must be delivered back to on channels whose
/// messages are addressed to a specific conversation — chiefly Telegram, where
/// the reply has to land in the same `chat.id` the inbound update came from.
/// The operator channel is a single implicit surface and needs none of this.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyTo {
    /// The chat/thread id to deliver back to. Rendered as a string so it stays
    /// channel-agnostic (Telegram's numeric `chat.id`, a future channel's
    /// opaque thread key) without widening the type per channel.
    pub chat_id: String,
}

/// A message the company emits on a channel.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutboundMessage {
    /// The channel to emit on.
    pub channel: String,
    /// The message text.
    pub text: String,
    /// The visible processing steps behind this bubble — the agent's tool calls,
    /// thinking runs, and any surfaced MCP failures — folded and scrubbed from
    /// the turn's progress stream (see [`crate::harness::steps`] under the
    /// `openhuman` feature). Per-bubble ownership: the operator bubble carries the
    /// orchestrator's steps; a delegated desk bubble carries that desk lead's
    /// steps.
    ///
    /// Additive and non-secret: the field is omitted on the wire when empty, so
    /// every prior producer (and every non-harness brain, which emits none)
    /// round-trips byte-identically. Never carries raw tool arguments, tool
    /// output, or call ids — only the scrubbed [`TurnStep`] shape.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<TurnStep>,
    /// Where to deliver the reply, for channels addressed to a specific
    /// chat/thread (Telegram). `None` on the operator channel and on every
    /// message emitted before this field existed; `skip_serializing_if` keeps
    /// such a message byte-identical on the wire, so no stored record migrates
    /// (same `by`/`chat`/`McpCallFailed` additive precedent above).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<ReplyTo>,
}

/// One visible step in an agent turn's processing timeline, surfaced in the
/// operator chat.
///
/// The point of the timeline: a failed tool call becomes visible (instead of a
/// vague acknowledgement), and a memory-served answer — which runs **zero**
/// steps — is distinguishable from a tool-backed one. Folded from the harness
/// progress stream by [`crate::harness::steps`] (compiled under the `openhuman`
/// feature); every field is scrubbed there before it reaches this shape.
///
/// The wire form is additive and camelCase (`elapsedMs`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStep {
    /// What kind of step this is (drives the icon in the UI).
    pub kind: TurnStepKind,
    /// How the step ended.
    pub status: TurnStepStatus,
    /// A short, human label (e.g. "Reading messages", "Thinking"). Derived from
    /// the tool's server-computed `display_label`, else its tool name — never
    /// from tool arguments or output.
    pub label: String,
    /// An optional muted detail: whitelisted, scrubbed enrichment (e.g. an MCP
    /// `server · tool`, a delegated desk, a task title) or a plain-language
    /// failure cause. **Never** raw tool output or arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// How long the step took in milliseconds, when known (tool calls report it;
    /// thinking/note steps do not).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

/// The kind of a [`TurnStep`], driving its icon in the timeline. Serialized in
/// `snake_case` (`tool_call` / `thinking` / `note`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStepKind {
    /// A tool call (a paired started/completed pair, or an unmatched one).
    ToolCall,
    /// A run of the model's reasoning, coalesced to a single "Thinking" step.
    Thinking,
    /// A standalone note — e.g. a surfaced MCP failure or the cap-omission
    /// marker.
    Note,
}

/// How a [`TurnStep`] ended. Serialized in `snake_case` (`ok` / `error` /
/// `running`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStepStatus {
    /// Completed successfully (or an informational step).
    Ok,
    /// Failed — rendered in the destructive tone.
    Error,
    /// Started but no completion was observed by the end of the turn.
    Running,
}

// ---------------------------------------------------------------------------
// Company records
// ---------------------------------------------------------------------------

/// An operator-added teammate that the version-controlled manifest does not
/// know about. Persisted as an overlay on the [`CompanyRecord`] and merged into
/// the roster at read/build time; the `company.toml` is never rewritten.
/// Roster-only in v1 (no harness `Agent` is minted for an overlay teammate).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayAgent {
    /// The teammate's stable id.
    pub id: String,
    /// The teammate's display name.
    pub name: String,
    /// The teammate's role.
    pub role: String,
    /// An optional description of the teammate's mandate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// An operator-added desk membership that the version-controlled manifest does
/// not know about. Persisted as an overlay on the [`CompanyRecord`] and merged
/// into a desk's effective membership at read/resolve time; the `company.toml`
/// is never rewritten. Only additions are modelled — a manifest-declared desk
/// member is part of the blueprint and cannot be removed through the overlay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayDeskMember {
    /// The desk (group-chat) id this addition targets.
    pub desk_id: String,
    /// The teammate id added to the desk. Resolves to a manifest agent or an
    /// [`OverlayAgent`].
    pub agent_id: String,
}

/// Where a company's manifest was seeded from — the source template's stable
/// identity, recorded once at launch and carried across rebuilds.
///
/// A company launched from a template directory (`serve --company
/// companies/<slug>`) records the directory slug as its stable `source_id`; a
/// company provisioned from a raw manifest body (`POST /api/v1/companies`)
/// carries no provenance (`CompanyRecord::template_provenance` stays `None`) —
/// provenance is never fabricated for a manifest that did not come from a
/// template. The blueprint (`company.toml`) is never rewritten: provenance
/// lives only on the record/overlay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateProvenance {
    /// The template's stable identifier — the source directory slug (or a
    /// canonical id where one exists). Stable across rebuilds and restarts.
    pub source_id: String,
    /// The template's version, when the source exposes one. `None` when the
    /// template is unversioned.
    #[serde(default)]
    pub version: Option<String>,
    /// The source directory path the company was launched from, when recorded.
    /// Optional and informational; `source_id` is the durable stable key.
    #[serde(default)]
    pub path: Option<String>,
}

/// An operator-set explicit ordering (hierarchy) for one desk's effective
/// members. Persisted as an overlay on the [`CompanyRecord`]; the version-
/// controlled manifest is never rewritten. Applied inside
/// [`CompanyRecord::effective_desk_members`] as a whole-set permutation: ids in
/// `ordered` come first in the given order (so an overlay-added member can be
/// promoted above manifest members — a whole-set reorder, which a per-member
/// `rank` could not express), and any effective member not listed keeps today's
/// relative order after them. Stale ids in `ordered` that are no longer members
/// are simply ignored. An empty `ordered` (or an absent entry) reproduces the
/// blueprint order exactly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayDeskOrder {
    /// The desk (group-chat) id this ordering targets.
    pub desk_id: String,
    /// The operator-set member id order. Listed ids sort first in this order;
    /// unlisted effective members keep their default relative order after.
    pub ordered: Vec<String>,
}

/// An operator-created desk (group chat) that the version-controlled manifest
/// does not declare. Persisted as an overlay on the [`CompanyRecord`] and merged
/// with the manifest's `[[group_chat]]` desks at read/resolve time; the
/// `company.toml` is never rewritten. This is the desk analogue of
/// [`OverlayAgent`] — the manifest stays authoritative and rebuild-preserved,
/// while runtime-created desks live alongside it and survive rebuilds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayDesk {
    /// The desk id — snake_case, unique across the manifest desks and the other
    /// overlay desks. Doubles as the chat thread id.
    pub id: String,
    /// Human-readable desk name.
    pub name: String,
    /// What the desk is for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The desk's founding member ids, in order; the first is its lead. Each
    /// must resolve to a roster teammate (manifest agent or [`OverlayAgent`]).
    /// Further members can still be added through the desk-member overlay.
    #[serde(default)]
    pub members: Vec<String>,
}

/// The operator overlays persisted as a single JSON blob by the string-column
/// stores (sqlite + mongodb `overlay_json`). The filesystem store keeps the two
/// collections as typed fields on its own `Meta` instead.
///
/// [`Self::parse`] accepts both the current object form and the legacy bare
/// array (`overlay_json` held only `Vec<OverlayAgent>` before desk overlays
/// existed), so existing rows load without a migration.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OverlayBlob {
    /// The operator team overlay.
    #[serde(default)]
    pub agents: Vec<OverlayAgent>,
    /// The operator desk-membership overlay.
    #[serde(default)]
    pub desk_members: Vec<OverlayDeskMember>,
    /// The operator per-desk member-ordering overlay.
    #[serde(default)]
    pub desk_order: Vec<OverlayDeskOrder>,
    /// The operator-created desk overlay. Absent on rows written before desk
    /// creation existed, so `#[serde(default)]` loads them as empty.
    #[serde(default)]
    pub desks: Vec<OverlayDesk>,
    /// The source-template provenance recorded at launch. `None` for companies
    /// provisioned from a raw manifest and for legacy rows written before
    /// provenance existed (the `#[serde(default)]` keeps those rows loading).
    #[serde(default)]
    pub provenance: Option<TemplateProvenance>,
}

impl OverlayBlob {
    /// Builds a blob from a record's overlay collections and provenance.
    pub fn from_record(record: &CompanyRecord) -> Self {
        Self {
            agents: record.overlay_agents.clone(),
            desk_members: record.overlay_desk_members.clone(),
            desk_order: record.overlay_desk_order.clone(),
            desks: record.overlay_desks.clone(),
            provenance: record.template_provenance.clone(),
        }
    }

    /// Parses the persisted blob, accepting both the current object form
    /// (`{"agents":[…],"desk_members":[…],"desks":[…]}`) and the legacy
    /// bare-array form (`[…]`, the pre-desk-overlay value that held only agents).
    /// When the current form parse fails, falls back to the legacy array; if
    /// that also fails the *original* error (from the current-form parse) is
    /// propagated so the caller sees why the object form failed rather than a
    /// misleading "expected sequence" message. New optional keys (`desks`) are
    /// absorbed by `#[serde(default)]`, so no migration is needed.
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        match serde_json::from_str::<OverlayBlob>(json) {
            Ok(blob) => Ok(blob),
            Err(original) => serde_json::from_str::<Vec<OverlayAgent>>(json)
                .map(|agents| Self {
                    agents,
                    desk_members: Vec::new(),
                    desk_order: Vec::new(),
                    desks: Vec::new(),
                    provenance: None,
                })
                .map_err(|_| original),
        }
    }
}

/// A durable company record: charter/roster (manifest) plus ledger and
/// lifecycle state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompanyRecord {
    /// The company id.
    pub id: CompanyId,
    /// The materialized manifest (charter + roster).
    pub manifest: CompanyManifest,
    /// The append-only ledger.
    pub ledger: Vec<LedgerEntry>,
    /// Lifecycle state, e.g. `running`, `paused`, `archived`.
    pub lifecycle: String,
    /// Operator-added teammates not present in the manifest (the team overlay).
    #[serde(default)]
    pub overlay_agents: Vec<OverlayAgent>,
    /// Operator-added desk memberships not present in the manifest (the desk
    /// overlay). Merged into a desk's effective membership at read time.
    #[serde(default)]
    pub overlay_desk_members: Vec<OverlayDeskMember>,
    /// Operator-set per-desk member orderings (the desk-hierarchy overlay).
    /// Applied as a whole-set permutation inside [`Self::effective_desk_members`];
    /// empty = the blueprint order. The manifest is never rewritten.
    #[serde(default)]
    pub overlay_desk_order: Vec<OverlayDeskOrder>,
    /// Operator-created desks not present in the manifest (the desk-creation
    /// overlay). Merged with the manifest's `[[group_chat]]` desks at read time.
    #[serde(default)]
    pub overlay_desks: Vec<OverlayDesk>,
    /// Where this company's manifest was seeded from — the source template's
    /// stable identity, stamped once at launch and carried across rebuilds.
    /// `None` for companies provisioned from a raw manifest body. The
    /// `#[serde(default)]` keeps records written before provenance existed
    /// loading without a migration.
    #[serde(default)]
    pub template_provenance: Option<TemplateProvenance>,
}

impl CompanyRecord {
    /// The effective member ids of a desk: the desk's declared members first
    /// (from the manifest `[[group_chat]]` or, for an operator-created desk, the
    /// [`OverlayDesk`]), then any operator-overlay member additions for that
    /// desk, in order and deduplicated on id — then re-ordered by this desk's
    /// operator-set [`OverlayDeskOrder`] if one exists.
    ///
    /// This is the single source of truth for "who is on a desk", shared by the
    /// REST `list_desks` handler and the harness `desk_lead` resolver so the two
    /// cannot drift. Base ordering: declared order is preserved (manifest or
    /// [`OverlayDesk`]), overlay members are appended in insertion order. Then,
    /// if the operator has set an explicit order for this desk, it is applied as
    /// a whole-set permutation — listed ids come first in the operator's order
    /// (so an overlay member can be promoted to the lead slot), and any effective
    /// member the order does not mention keeps its base relative position after
    /// them. With no order override the base order is returned unchanged, so the
    /// first declared member stays the lead by default.
    pub fn effective_desk_members(&self, desk_id: &str) -> Vec<String> {
        let mut members: Vec<String> = self
            .manifest
            .group_chats
            .iter()
            .find(|c| c.id == desk_id)
            .map(|c| c.members.clone())
            .or_else(|| {
                self.overlay_desks
                    .iter()
                    .find(|d| d.id == desk_id)
                    .map(|d| d.members.clone())
            })
            .unwrap_or_default();
        for add in &self.overlay_desk_members {
            if add.desk_id == desk_id && !members.contains(&add.agent_id) {
                members.push(add.agent_id.clone());
            }
        }
        // Apply the operator-set ordering as a whole-set permutation. Listed ids
        // sort first in the operator's order; unlisted members keep their base
        // relative order after (stable sort). Stale ids no longer members are
        // absent from `members`, so they simply have no effect.
        if let Some(order) = self
            .overlay_desk_order
            .iter()
            .find(|o| o.desk_id == desk_id && !o.ordered.is_empty())
        {
            let mut ranked: Vec<(usize, usize, String)> = members
                .into_iter()
                .enumerate()
                .map(|(base_index, id)| {
                    // Listed ids rank by their position in the operator order;
                    // unlisted ids rank after all listed ids, keyed on their base
                    // index so their relative order is preserved by the sort.
                    let key = match order.ordered.iter().position(|listed| *listed == id) {
                        Some(pos) => pos,
                        None => order.ordered.len() + base_index,
                    };
                    (key, base_index, id)
                })
                .collect();
            ranked.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
            members = ranked.into_iter().map(|(_, _, id)| id).collect();
        }
        members
    }

    /// Resolves a desk key (an id, or a case-insensitive name) to its canonical
    /// id, searching the manifest desks first and then the operator-created
    /// overlay desks. Lets the harness route to overlay desks by the same
    /// id-or-name key it already accepts for manifest desks.
    pub fn resolve_desk_id(&self, key: &str) -> Option<String> {
        self.manifest
            .group_chats
            .iter()
            .find(|c| c.id == key || c.name.eq_ignore_ascii_case(key))
            .map(|c| c.id.clone())
            .or_else(|| {
                self.overlay_desks
                    .iter()
                    .find(|d| d.id == key || d.name.eq_ignore_ascii_case(key))
                    .map(|d| d.id.clone())
            })
    }

    /// Whether a desk with `desk_id` exists in either the manifest or the
    /// operator-created overlay desks.
    pub fn desk_exists(&self, desk_id: &str) -> bool {
        self.manifest.group_chats.iter().any(|c| c.id == desk_id)
            || self.overlay_desks.iter().any(|d| d.id == desk_id)
    }

    /// Whether `agent_id` names a roster teammate — a manifest agent or an
    /// operator-overlay teammate. The desk overlay may only add ids that resolve
    /// here.
    pub fn is_roster_agent(&self, agent_id: &str) -> bool {
        self.manifest.agents.iter().any(|a| a.id == agent_id)
            || self.overlay_agents.iter().any(|a| a.id == agent_id)
    }
}

/// A compact company listing entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompanySummary {
    /// The company id.
    pub id: CompanyId,
    /// The display name.
    pub name: String,
    /// Lifecycle state.
    pub lifecycle: String,
}

// ---------------------------------------------------------------------------
// Agent economy (tiny.place seam)
// ---------------------------------------------------------------------------

/// A company's tiny.place identity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompanyIdentity {
    /// The company id.
    pub company: CompanyId,
    /// The tiny.place `@handle`.
    pub handle: String,
}

/// The registration state of a company on tiny.place.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum RegistrationState {
    /// Not yet registered.
    Unregistered,
    /// Registered under this address.
    Registered {
        /// The registered agent address.
        addr: AgentAddr,
    },
}

/// A published Agent Card advertising a company's skills on tiny.place.
///
/// The three original fields (`handle`, `description`, `skills`) are unchanged;
/// every field added for the A2A wire shape carries `#[serde(default)]` so
/// records written by earlier phases round-trip without loss.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentCard {
    /// The advertised `@handle`.
    pub handle: String,
    /// A short description of the company.
    pub description: String,
    /// The advertised skill ids.
    pub skills: Vec<String>,
    /// Human-readable display name (the company name).
    #[serde(default)]
    pub name: String,
    /// The actor kind; always `"agent"` for a company.
    #[serde(default)]
    pub actor_type: String,
    /// The A2A endpoint, e.g. `https://host/a2a/{handle}`.
    #[serde(default)]
    pub endpoint: String,
    /// Interfaces the endpoint speaks, e.g. `["a2a-jsonrpc"]`.
    #[serde(default)]
    pub supported_interfaces: Vec<String>,
    /// Capability tokens derived from the advertised skills.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Free-form discovery tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Per-skill payment requirements advertised to counterparties.
    #[serde(default)]
    pub payment_requirements: Vec<CardPayment>,
}

/// A single priced skill on an [`AgentCard`], in x402 terms.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CardPayment {
    /// The skill this price applies to.
    pub skill_id: String,
    /// The decimal price string, e.g. `"25.00"`.
    pub price: String,
    /// The settlement asset, e.g. `"USDC"`.
    pub asset: String,
    /// The settlement network, e.g. `"solana"`.
    pub network: String,
}

/// An addressable agent on tiny.place.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentAddr(pub String);

/// A task sent agent-to-agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct A2aTask {
    /// The requested skill id.
    pub skill: String,
    /// The task input.
    pub input: serde_json::Value,
}

/// A handle to a dispatched A2A task.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct A2aTaskHandle(pub String);

/// A payment requirement quoted by a counterparty.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PaymentRequirement {
    /// The counterparty address.
    pub to: AgentAddr,
    /// The amount due, in USD.
    pub amount_usd: f64,
    /// What the payment is for.
    pub memo: String,
}

/// A firm quote a company can pay against.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Quote {
    /// A unique quote id.
    pub quote_id: String,
    /// The counterparty address.
    pub to: AgentAddr,
    /// The quoted amount, in USD.
    pub amount_usd: f64,
}

/// The budget envelope a payment must fit within.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BudgetScope {
    /// The remaining budget for this scope, in USD.
    pub remaining_usd: f64,
    /// A label describing the scope (e.g. an agent id).
    pub label: String,
}

/// A receipt for a completed payment.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PaymentReceipt {
    /// The quote that was paid.
    pub quote_id: String,
    /// The amount paid, in USD.
    pub amount_usd: f64,
    /// Epoch-millis timestamp of the payment.
    pub at_millis: u64,
}

#[cfg(test)]
mod test {
    use super::*;

    fn round_trip<T>(value: &T) -> T
    where
        T: Serialize + for<'de> Deserialize<'de>,
    {
        let json = serde_json::to_string(value).expect("serialize");
        serde_json::from_str(&json).expect("deserialize")
    }

    // ── Issue #174: cycle usage carries cost, and folds ─────────────────────

    /// A cycle with nothing to report writes nothing, and any single non-zero
    /// field makes it real usage — including a token-less charge.
    #[test]
    fn token_usage_is_zero_only_when_every_field_is() {
        assert!(TokenUsage::default().is_zero());
        for usage in [
            TokenUsage {
                input: 1,
                ..TokenUsage::default()
            },
            TokenUsage {
                output: 1,
                ..TokenUsage::default()
            },
            TokenUsage {
                cached_input: 1,
                ..TokenUsage::default()
            },
            TokenUsage {
                cost_usd: 0.0001,
                ..TokenUsage::default()
            },
        ] {
            assert!(!usage.is_zero(), "{usage:?} is real usage");
        }
    }

    /// Several model passes in one cycle accumulate into one total.
    #[test]
    fn token_usage_folds_passes_together() {
        let mut total = TokenUsage::default();
        total.fold(&TokenUsage {
            input: 100,
            output: 20,
            cached_input: 10,
            cost_usd: 0.01,
        });
        total.fold(&TokenUsage {
            input: 50,
            output: 5,
            cached_input: 0,
            cost_usd: 0.02,
        });
        assert_eq!(total.input, 150);
        assert_eq!(total.output, 25);
        assert_eq!(total.cached_input, 10);
        assert!((total.cost_usd - 0.03).abs() < 1e-9);
    }

    /// A bogus peer value must never wrap the meter into a huge or tiny number.
    #[test]
    fn token_usage_fold_saturates_instead_of_overflowing() {
        let mut total = TokenUsage {
            input: u64::MAX,
            output: u64::MAX,
            cached_input: u64::MAX,
            cost_usd: 0.0,
        };
        total.fold(&TokenUsage {
            input: 10,
            output: 10,
            cached_input: 10,
            cost_usd: 0.0,
        });
        assert_eq!(total.input, u64::MAX);
        assert_eq!(total.output, u64::MAX);
        assert_eq!(total.cached_input, u64::MAX);
    }

    /// The cost fields are additive on the wire: a peer that predates them still
    /// decodes, and an all-zero usage still serializes them for a peer that has
    /// them.
    #[test]
    fn token_usage_decodes_a_payload_without_the_cost_fields() {
        let legacy: TokenUsage = serde_json::from_str(r#"{"input":7,"output":3}"#).unwrap();
        assert_eq!(legacy.input, 7);
        assert_eq!(legacy.output, 3);
        assert_eq!(legacy.cached_input, 0);
        assert_eq!(legacy.cost_usd, 0.0);
        assert_eq!(round_trip(&legacy), legacy);
    }

    /// The `TurnStep` wire shape is camelCase with snake_case enum values:
    /// `{kind, status, label, detail?, elapsedMs?}`. Locks the contract the
    /// console `TurnStep` mirror in `frontend/src/api/types.ts` depends on.
    #[test]
    fn turn_step_wire_shape_is_camel_case_with_snake_case_enums() {
        let step = TurnStep {
            kind: TurnStepKind::ToolCall,
            status: TurnStepStatus::Error,
            label: "Searching the web".to_string(),
            detail: Some("brave · search".to_string()),
            elapsed_ms: Some(1234),
        };
        let json = serde_json::to_value(&step).unwrap();
        assert_eq!(json["kind"], "tool_call");
        assert_eq!(json["status"], "error");
        assert_eq!(json["label"], "Searching the web");
        assert_eq!(json["detail"], "brave · search");
        assert_eq!(json["elapsedMs"], 1234);
        assert_eq!(round_trip(&step), step);
    }

    /// A step with no detail/elapsed omits both keys, and every kind/status
    /// value serializes to its documented snake_case token.
    #[test]
    fn turn_step_omits_absent_fields_and_covers_every_variant() {
        let bare = TurnStep {
            kind: TurnStepKind::Thinking,
            status: TurnStepStatus::Ok,
            label: "Thinking".to_string(),
            detail: None,
            elapsed_ms: None,
        };
        let json = serde_json::to_value(&bare).unwrap();
        assert_eq!(json["kind"], "thinking");
        assert_eq!(json["status"], "ok");
        assert!(json.get("detail").is_none(), "absent detail is omitted");
        assert!(json.get("elapsedMs").is_none(), "absent elapsed is omitted");

        assert_eq!(serde_json::to_value(TurnStepKind::Note).unwrap(), "note");
        assert_eq!(
            serde_json::to_value(TurnStepStatus::Running).unwrap(),
            "running"
        );
    }

    /// `OutboundMessage.steps` is additive: an empty timeline is omitted from
    /// the wire entirely (so every prior producer round-trips byte-identically),
    /// and a legacy `{channel, text}` payload still loads with an empty `steps`.
    #[test]
    fn outbound_message_steps_are_additive_and_omitted_when_empty() {
        let no_steps = OutboundMessage {
            channel: "operator".to_string(),
            text: "hi".to_string(),
            steps: Vec::new(),
            reply_to: None,
        };
        let json = serde_json::to_string(&no_steps).unwrap();
        assert_eq!(json, r#"{"channel":"operator","text":"hi"}"#);

        let legacy: OutboundMessage =
            serde_json::from_str(r#"{"channel":"operator","text":"hi"}"#).unwrap();
        assert!(legacy.steps.is_empty());

        let with_steps = OutboundMessage {
            channel: "operator".to_string(),
            text: "done".to_string(),
            steps: vec![TurnStep {
                kind: TurnStepKind::Note,
                status: TurnStepStatus::Error,
                label: "MCP: brave unavailable".to_string(),
                detail: Some("server rejected the call".to_string()),
                elapsed_ms: None,
            }],
            reply_to: None,
        };
        assert_eq!(round_trip(&with_steps), with_steps);
    }

    /// `AgentReply.steps` is additive the same way: a reply journaled before
    /// the field existed loads with an empty timeline, and a tool-less reply
    /// omits the key so its on-disk form is byte-identical to the legacy log.
    #[test]
    fn agent_reply_steps_are_additive_and_omitted_when_empty() {
        let legacy: CompanyEvent = serde_json::from_str(
            r#"{"kind":"AgentReply","chat_id":"main","agent_id":"ceo","text":"hi"}"#,
        )
        .expect("a pre-steps AgentReply still loads");
        match &legacy {
            CompanyEvent::AgentReply { steps, .. } => assert!(steps.is_empty()),
            other => panic!("expected AgentReply, got {other:?}"),
        }

        // A tool-less reply serializes without the `steps` key.
        let tool_less = CompanyEvent::AgentReply {
            task_id: None,
            chat_id: "main".to_string(),
            agent_id: "ceo".to_string(),
            text: "hi".to_string(),
            steps: Vec::new(),
        };
        let json = serde_json::to_value(&tool_less).unwrap();
        assert!(json.get("steps").is_none());

        // A reply with a timeline round-trips it.
        let with_steps = CompanyEvent::AgentReply {
            task_id: None,
            chat_id: "main".to_string(),
            agent_id: "ceo".to_string(),
            text: "done".to_string(),
            steps: vec![TurnStep {
                kind: TurnStepKind::ToolCall,
                status: TurnStepStatus::Ok,
                label: "Reading messages".to_string(),
                detail: None,
                elapsed_ms: Some(12),
            }],
        };
        let back: CompanyEvent =
            serde_json::from_str(&serde_json::to_string(&with_steps).unwrap()).unwrap();
        assert_eq!(back, with_steps);
    }

    /// #185: the `task_id` correlation key is additive in both directions —
    /// an event journaled before it existed still loads, and an untagged event
    /// still serializes byte-for-byte as it did before the field was added.
    ///
    /// That second half is the migration-free guarantee: every already-persisted
    /// `AgentReply` / `McpCallFailed` in every company's log must round-trip
    /// unchanged, or the cross-backend export/import comparison breaks.
    #[test]
    fn task_id_correlation_is_additive_and_omitted_when_absent() {
        let legacy: CompanyEvent = serde_json::from_str(
            r#"{"kind":"AgentReply","chat_id":"main","agent_id":"ceo","text":"hi"}"#,
        )
        .expect("a pre-task_id AgentReply still loads");
        match &legacy {
            CompanyEvent::AgentReply { task_id, .. } => assert!(task_id.is_none()),
            other => panic!("expected AgentReply, got {other:?}"),
        }

        // An untagged reply keeps the legacy wire shape exactly.
        let untagged = CompanyEvent::AgentReply {
            chat_id: "main".to_string(),
            agent_id: "ceo".to_string(),
            text: "hi".to_string(),
            steps: Vec::new(),
            task_id: None,
        };
        assert_eq!(
            serde_json::to_string(&untagged).unwrap(),
            r#"{"kind":"AgentReply","chat_id":"main","agent_id":"ceo","text":"hi"}"#
        );

        // A dispatch-produced reply carries the key and round-trips.
        let tagged = CompanyEvent::AgentReply {
            chat_id: "t-1".to_string(),
            agent_id: "ceo".to_string(),
            text: "done".to_string(),
            steps: Vec::new(),
            task_id: Some("t-1".to_string()),
        };
        let back: CompanyEvent =
            serde_json::from_str(&serde_json::to_string(&tagged).unwrap()).unwrap();
        assert_eq!(back, tagged);

        // Same contract on the failure event.
        let legacy_mcp: CompanyEvent = serde_json::from_str(
            r#"{"kind":"McpCallFailed","server":"gh","tool":"issues","status":"credential_required","message":"needs auth"}"#,
        )
        .expect("a pre-task_id McpCallFailed still loads");
        match &legacy_mcp {
            CompanyEvent::McpCallFailed { task_id, .. } => assert!(task_id.is_none()),
            other => panic!("expected McpCallFailed, got {other:?}"),
        }
    }

    /// #185: the dispatch terminal round-trips, and reports where the card
    /// landed so a stopped run is distinguishable from a successful one.
    #[test]
    fn desk_task_completed_round_trips() {
        let done = CompanyEvent::DeskTaskCompleted {
            task_id: "t-1".to_string(),
            desk: "ceo".to_string(),
            output: "shipped".to_string(),
            column: "in_review".to_string(),
        };
        let json = serde_json::to_string(&done).unwrap();
        assert!(json.contains(r#""kind":"DeskTaskCompleted""#));
        let back: CompanyEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, done);
    }

    #[test]
    fn an_operator_message_journaled_before_attribution_still_loads() {
        // Exactly what is already on disk in every existing company's event
        // log. If this ever fails, the change needs a migration.
        let legacy = r#"{"kind":"OperatorMessage","text":"hi"}"#;
        let event: CompanyEvent = serde_json::from_str(legacy).unwrap();
        assert_eq!(
            event,
            CompanyEvent::OperatorMessage {
                text: "hi".into(),
                by: None,
                chat: None,
            }
        );
    }

    #[test]
    fn an_unattributed_message_serializes_exactly_as_it_did_before() {
        // `skip_serializing_if` keeps the old bytes. This is what lets
        // export/import and the fs/sqlite/mongo round-trip stay green without
        // touching a single stored record.
        let event = CompanyEvent::OperatorMessage {
            text: "hi".into(),
            by: None,
            chat: None,
        };
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"kind":"OperatorMessage","text":"hi"}"#
        );
    }

    #[test]
    fn an_attributed_message_round_trips_with_its_actor() {
        let event = CompanyEvent::OperatorMessage {
            text: "hi".into(),
            by: Some(Actor {
                kind: ActorKind::User,
                id: "u1".into(),
            }),
            chat: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["by"]["kind"], "user");
        assert_eq!(json["by"]["id"], "u1");
        assert_eq!(serde_json::from_value::<CompanyEvent>(json).unwrap(), event);
    }

    #[test]
    fn actor_kind_is_still_copy() {
        // A `String`-carrying variant would have taken this away from every
        // existing holder, which is why the User id lives on `Actor` instead.
        fn assert_copy(kind: ActorKind) -> (ActorKind, ActorKind) {
            (kind, kind)
        }
        let (a, b) = assert_copy(ActorKind::User);
        assert_eq!(a, b);
    }

    #[test]
    fn company_event_variants_round_trip_tagged() {
        let events = vec![
            CompanyEvent::OperatorMessage {
                text: "hi".into(),
                by: None,
                chat: None,
            },
            CompanyEvent::WebhookReceived {
                channel: "email".into(),
                body: serde_json::json!({"subject": "hello"}),
            },
            CompanyEvent::ScheduleFired {
                cron: "0 9 * * *".into(),
                prompt: "daily standup".into(),
            },
            CompanyEvent::A2aTaskReceived {
                from: "@peer".into(),
                task: serde_json::json!({"skill": "seo.audit"}),
            },
            CompanyEvent::ApprovalResolved {
                approval_id: ApprovalId::new("a1"),
                verdict: Verdict::Approve,
                by: Actor {
                    kind: ActorKind::Operator,
                    id: "owner".into(),
                },
            },
            CompanyEvent::FeedbackFiled {
                note: "too slow".into(),
            },
            CompanyEvent::PaymentReceived {
                amount_usd: 25.0,
                memo: "invoice #1".into(),
            },
        ];
        for event in &events {
            assert_eq!(&round_trip(event), event);
        }

        // The tag field is emitted under `kind`.
        let json = serde_json::to_value(&events[0]).unwrap();
        assert_eq!(json["kind"], "OperatorMessage");
        assert_eq!(json["text"], "hi");
    }

    #[test]
    fn mcp_call_failed_round_trips_and_is_byte_stable() {
        let event = CompanyEvent::McpCallFailed {
            task_id: None,
            server: "browserbase".into(),
            tool: "browse".into(),
            status: "tool_call_rejected".into(),
            message: "server rejected the call".into(),
        };
        assert_eq!(round_trip(&event), event);
        // The tag is emitted under `kind`, and the field set is fixed — a byte
        // guard so a later field addition is a deliberate, tested change.
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"kind":"McpCallFailed","server":"browserbase","tool":"browse","status":"tool_call_rejected","message":"server rejected the call"}"#
        );
    }

    #[test]
    fn task_steered_round_trips_and_omits_empty_fields() {
        // A plain pause: no `instruction`, no `by` — both must be OMITTED from
        // the wire (skip_serializing_if), so old logs stay byte-stable.
        let pause = CompanyEvent::TaskSteered {
            task_id: "t1".into(),
            action: "pause".into(),
            instruction: None,
            by: None,
        };
        assert_eq!(round_trip(&pause), pause);
        assert_eq!(
            serde_json::to_string(&pause).unwrap(),
            r#"{"kind":"TaskSteered","task_id":"t1","action":"pause"}"#
        );

        // A redirect carries its (capped) instruction; still no actor.
        let redirect = CompanyEvent::TaskSteered {
            task_id: "t1".into(),
            action: "redirect".into(),
            instruction: Some("focus on the API".into()),
            by: None,
        };
        assert_eq!(round_trip(&redirect), redirect);
        assert_eq!(
            serde_json::to_string(&redirect).unwrap(),
            r#"{"kind":"TaskSteered","task_id":"t1","action":"redirect","instruction":"focus on the API"}"#
        );
    }

    #[test]
    fn verdict_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&Verdict::Approve).unwrap(),
            "\"approve\""
        );
        assert_eq!(serde_json::to_string(&Verdict::Deny).unwrap(), "\"deny\"");
        assert_eq!(
            serde_json::from_str::<Verdict>("\"approve\"").unwrap(),
            Verdict::Approve
        );
    }

    #[test]
    fn effect_round_trips_and_accessors_read_fields() {
        let effect = Effect {
            kind: "payment.send".into(),
            group: EffectGroup::Spend,
            amount_usd: Some(42.5),
            established_thread: true,
            first_time_counterparty: false,
            payload: serde_json::json!({"to": "@vendor"}),
        };
        let back = round_trip(&effect);
        assert_eq!(back, effect);
        assert_eq!(effect.kind(), "payment.send");
        assert_eq!(effect.group(), EffectGroup::Spend);
        assert_eq!(effect.amount_usd(), Some(42.5));
        assert!(effect.is_established_thread());
        assert!(!effect.is_first_time_counterparty());
    }

    #[test]
    fn effect_disposition_round_trips() {
        for disp in [
            EffectDisposition::Executed,
            EffectDisposition::PendingApproval(ApprovalId::new("x")),
            EffectDisposition::Denied {
                reason: "over cap".into(),
            },
        ] {
            assert_eq!(round_trip(&disp), disp);
        }
    }

    #[test]
    fn policy_decision_round_trips() {
        for dec in [
            PolicyDecision::Allow,
            PolicyDecision::RequireApproval,
            PolicyDecision::Deny,
        ] {
            assert_eq!(round_trip(&dec), dec);
        }
    }

    #[test]
    fn event_seq_orders_numerically() {
        assert!(EventSeq::new(1) < EventSeq::new(2));
        assert_eq!(EventSeq::new(7).value(), 7);
    }

    #[test]
    fn agent_card_round_trips_with_extended_fields() {
        let card = AgentCard {
            handle: "acme".into(),
            description: "We audit SEO.".into(),
            skills: vec!["seo.audit".into()],
            name: "Acme SEO".into(),
            actor_type: "agent".into(),
            endpoint: "https://host/a2a/acme".into(),
            supported_interfaces: vec!["a2a-jsonrpc".into()],
            capabilities: vec!["seo.audit".into()],
            tags: vec!["seo.audit".into()],
            payment_requirements: vec![CardPayment {
                skill_id: "seo.audit".into(),
                price: "25.00".into(),
                asset: "USDC".into(),
                network: "solana".into(),
            }],
        };
        assert_eq!(round_trip(&card), card);
    }

    fn desk_record(toml_src: &str, overlay: Vec<OverlayDeskMember>) -> CompanyRecord {
        CompanyRecord {
            id: CompanyId::new("acme"),
            manifest: toml::from_str(toml_src).expect("parse manifest"),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: overlay,
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            template_provenance: None,
        }
    }

    /// Like [`desk_record`] but with an explicit per-desk order overlay, for the
    /// desk-hierarchy tests.
    fn desk_record_ordered(
        toml_src: &str,
        overlay: Vec<OverlayDeskMember>,
        order: Vec<OverlayDeskOrder>,
    ) -> CompanyRecord {
        let mut record = desk_record(toml_src, overlay);
        record.overlay_desk_order = order;
        record
    }

    /// The effective membership is the manifest members first, then overlay
    /// additions in insertion order, deduplicated — the shared rule the REST
    /// list and the harness desk-lead resolver both read.
    #[test]
    fn effective_desk_members_unions_manifest_and_overlay_deduped() {
        let manifest = "[company]\nname = \"Acme\"\n\
             [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n\
             [[agent]]\nid = \"eng\"\nrole = \"Engineer\"\n\
             [[group_chat]]\nid = \"studio\"\nname = \"Studio\"\nmembers = [\"ceo\"]\n";
        let record = desk_record(
            manifest,
            vec![
                OverlayDeskMember {
                    desk_id: "studio".into(),
                    agent_id: "eng".into(),
                },
                // A duplicate of a manifest member is not added twice.
                OverlayDeskMember {
                    desk_id: "studio".into(),
                    agent_id: "ceo".into(),
                },
                // An addition for a different desk is ignored here.
                OverlayDeskMember {
                    desk_id: "other".into(),
                    agent_id: "eng".into(),
                },
            ],
        );
        assert_eq!(
            record.effective_desk_members("studio"),
            vec!["ceo".to_string(), "eng".to_string()]
        );
        // An unknown desk with only an overlay addition still resolves it.
        assert_eq!(
            record.effective_desk_members("other"),
            vec!["eng".to_string()]
        );
    }

    /// A three-member manifest desk whose order override permutes the members.
    const HIERARCHY_MANIFEST: &str = "[company]\nname = \"Acme\"\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n\
         [[agent]]\nid = \"eng\"\nrole = \"Engineer\"\n\
         [[agent]]\nid = \"des\"\nrole = \"Designer\"\n\
         [[group_chat]]\nid = \"studio\"\nname = \"Studio\"\nmembers = [\"ceo\", \"eng\", \"des\"]\n";

    fn order(desk: &str, ids: &[&str]) -> Vec<OverlayDeskOrder> {
        vec![OverlayDeskOrder {
            desk_id: desk.into(),
            ordered: ids.iter().map(|s| s.to_string()).collect(),
        }]
    }

    /// A full permutation reorders the manifest members exactly as given.
    #[test]
    fn desk_order_reorders_manifest_members() {
        let record = desk_record_ordered(
            HIERARCHY_MANIFEST,
            Vec::new(),
            order("studio", &["des", "ceo", "eng"]),
        );
        assert_eq!(
            record.effective_desk_members("studio"),
            vec!["des".to_string(), "ceo".to_string(), "eng".to_string()]
        );
    }

    /// The override can promote an overlay-added member above manifest members —
    /// the whole-set permutation a per-member rank could not express.
    #[test]
    fn desk_order_promotes_overlay_member_to_lead() {
        let record = desk_record_ordered(
            HIERARCHY_MANIFEST,
            vec![OverlayDeskMember {
                desk_id: "studio".into(),
                agent_id: "cto".into(),
            }],
            order("studio", &["cto", "ceo", "eng", "des"]),
        );
        let members = record.effective_desk_members("studio");
        assert_eq!(members[0], "cto");
        assert_eq!(
            members,
            vec![
                "cto".to_string(),
                "ceo".to_string(),
                "eng".to_string(),
                "des".to_string()
            ]
        );
    }

    /// An absent or empty override reproduces the base order byte-for-byte.
    #[test]
    fn desk_order_absent_or_empty_keeps_base_order() {
        let base = desk_record(HIERARCHY_MANIFEST, Vec::new());
        let base_members = base.effective_desk_members("studio");
        assert_eq!(base_members, vec!["ceo", "eng", "des"]);

        // An explicit empty override for the desk is a no-op too.
        let empty = desk_record_ordered(HIERARCHY_MANIFEST, Vec::new(), order("studio", &[]));
        assert_eq!(empty.effective_desk_members("studio"), base_members);
    }

    /// Ids in the override that are no longer desk members are ignored.
    #[test]
    fn desk_order_ignores_stale_ids() {
        let record = desk_record_ordered(
            HIERARCHY_MANIFEST,
            Vec::new(),
            order("studio", &["ghost", "des", "ceo", "eng"]),
        );
        // `ghost` is not a member, so it contributes nothing; the rest apply.
        assert_eq!(
            record.effective_desk_members("studio"),
            vec!["des".to_string(), "ceo".to_string(), "eng".to_string()]
        );
    }

    /// A subset override lists its ids first, then the unlisted members keep
    /// their base relative order after.
    #[test]
    fn desk_order_subset_is_listed_first_then_default() {
        let record = desk_record_ordered(HIERARCHY_MANIFEST, Vec::new(), order("studio", &["des"]));
        // `des` promoted first; `ceo`, `eng` keep their base order behind it.
        assert_eq!(
            record.effective_desk_members("studio"),
            vec!["des".to_string(), "ceo".to_string(), "eng".to_string()]
        );
    }

    /// The persisted overlay blob round-trips the desk-order collection.
    #[test]
    fn overlay_blob_round_trips_desk_order() {
        let record = desk_record_ordered(
            HIERARCHY_MANIFEST,
            Vec::new(),
            order("studio", &["des", "ceo", "eng"]),
        );
        let blob = OverlayBlob::from_record(&record);
        let json = serde_json::to_string(&blob).expect("serialize blob");
        let parsed = OverlayBlob::parse(&json).expect("parse blob");
        assert_eq!(parsed.desk_order, record.overlay_desk_order);
    }

    /// An object-form blob written before `desk_order` existed still parses, and
    /// the legacy bare-array form still parses — both with an empty order.
    #[test]
    fn overlay_blob_parses_without_desk_order_key() {
        // Object form missing the `desk_order` key (pre-#131 rows).
        let object = r#"{"agents":[{"id":"a","name":"A","role":"r"}],"desk_members":[{"desk_id":"d","agent_id":"a"}]}"#;
        let blob = OverlayBlob::parse(object).expect("object without desk_order");
        assert_eq!(blob.desk_members.len(), 1);
        assert!(blob.desk_order.is_empty());

        // Legacy bare `Vec<OverlayAgent>` form.
        let legacy = r#"[{"id":"a","name":"A","role":"r"}]"#;
        let blob = OverlayBlob::parse(legacy).expect("legacy array");
        assert!(blob.desk_order.is_empty());
    }

    /// `is_roster_agent` accepts both manifest agents and overlay teammates, and
    /// rejects an unknown id — the validation the desk-add route relies on.
    #[test]
    fn is_roster_agent_covers_manifest_and_overlay() {
        let manifest = "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n";
        let mut record = desk_record(manifest, Vec::new());
        record.overlay_agents.push(OverlayAgent {
            id: "nova".into(),
            name: "Nova".into(),
            role: "Growth".into(),
            description: None,
        });
        assert!(record.is_roster_agent("ceo"));
        assert!(record.is_roster_agent("nova"));
        assert!(!record.is_roster_agent("ghost"));
    }

    /// The persisted overlay blob reads both the current object form and the
    /// legacy bare-`overlay_agents`-array form, so existing sqlite/mongo rows
    /// load without a migration.
    #[test]
    fn overlay_blob_parses_object_and_legacy_array() {
        let object = r#"{"agents":[{"id":"a","name":"A","role":"r"}],"desk_members":[{"desk_id":"d","agent_id":"a"}]}"#;
        let blob = OverlayBlob::parse(object).expect("object");
        assert_eq!(blob.agents.len(), 1);
        assert_eq!(blob.desk_members.len(), 1);
        // Issue #85: an object written before provenance existed omits the key;
        // `#[serde(default)]` loads it as `None` (zero-migration back-compat).
        assert!(blob.provenance.is_none());

        // Legacy: overlay_json used to hold a bare Vec<OverlayAgent>.
        let legacy = r#"[{"id":"a","name":"A","role":"r"}]"#;
        let blob = OverlayBlob::parse(legacy).expect("legacy array");
        assert_eq!(blob.agents.len(), 1);
        assert!(blob.desk_members.is_empty());
        assert!(blob.provenance.is_none());

        // The empty-array default persisted by fresh schema.
        let blob = OverlayBlob::parse("[]").expect("empty array");
        assert!(blob.agents.is_empty());
        assert!(blob.desk_members.is_empty());
        assert!(blob.provenance.is_none());
        assert!(blob.desks.is_empty());

        // A pre-desk-creation object row (no `desks` key) loads with an empty
        // desk overlay — no migration needed.
        let pre_desks = r#"{"agents":[],"desk_members":[]}"#;
        let blob = OverlayBlob::parse(pre_desks).expect("pre-desks object");
        assert!(blob.desks.is_empty());
    }

    /// Issue #85: a record's template provenance round-trips through the
    /// `OverlayBlob` the sqlite/mongodb stores persist, and a blob carrying
    /// provenance re-parses with it intact.
    #[test]
    fn overlay_blob_carries_template_provenance() {
        let mut record = desk_record("[company]\nname = \"Acme\"\n", Vec::new());
        record.template_provenance = Some(TemplateProvenance {
            source_id: "agentic_law_firm".to_string(),
            version: Some("2.0.0".to_string()),
            path: Some("companies/agentic_law_firm".to_string()),
        });
        let json = serde_json::to_string(&OverlayBlob::from_record(&record)).expect("serialize");
        let blob = OverlayBlob::parse(&json).expect("reparse");
        assert_eq!(blob.provenance, record.template_provenance);
    }

    /// An operator-created overlay desk resolves through the same
    /// `effective_desk_members` / `resolve_desk_id` / `desk_exists` helpers the
    /// manifest desks use, so the REST list and the harness desk-lead resolver
    /// treat it identically. Member additions still layer on top.
    #[test]
    fn overlay_desk_resolves_like_a_manifest_desk() {
        let manifest = "[company]\nname = \"Acme\"\n\
             [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n\
             [[agent]]\nid = \"eng\"\nrole = \"Engineer\"\n";
        let mut record = desk_record(manifest, Vec::new());
        record.overlay_desks.push(OverlayDesk {
            id: "growth".into(),
            name: "Growth".into(),
            description: None,
            members: vec!["eng".into()],
        });
        // Resolves by id and by case-insensitive name.
        assert_eq!(record.resolve_desk_id("growth").as_deref(), Some("growth"));
        assert_eq!(record.resolve_desk_id("GROWTH").as_deref(), Some("growth"));
        assert!(record.desk_exists("growth"));
        // Founding member is the lead; a later overlay addition appends.
        assert_eq!(
            record.effective_desk_members("growth"),
            vec!["eng".to_string()]
        );
        record.overlay_desk_members.push(OverlayDeskMember {
            desk_id: "growth".into(),
            agent_id: "ceo".into(),
        });
        assert_eq!(
            record.effective_desk_members("growth"),
            vec!["eng".to_string(), "ceo".to_string()]
        );
    }

    /// The overlay blob round-trips operator-created desks through its persisted
    /// JSON form, so a created desk survives a store save/load cycle.
    #[test]
    fn overlay_blob_round_trips_desks() {
        let with_desks = r#"{"agents":[],"desk_members":[],"desks":[{"id":"growth","name":"Growth","members":["eng"]}]}"#;
        let blob = OverlayBlob::parse(with_desks).expect("object with desks");
        assert_eq!(blob.desks.len(), 1);
        assert_eq!(blob.desks[0].id, "growth");
        assert_eq!(blob.desks[0].members, vec!["eng".to_string()]);
        // Re-serialize and re-parse — the desk survives the round trip.
        let json = serde_json::to_string(&blob).expect("serialize");
        let again = OverlayBlob::parse(&json).expect("reparse");
        assert_eq!(again.desks, blob.desks);
    }

    #[test]
    fn legacy_agent_card_json_deserializes_with_defaults() {
        // A card written by an earlier phase carried only three fields; the new
        // `#[serde(default)]` fields must fill in without error.
        let json = r#"{"handle":"acme","description":"d","skills":["a"]}"#;
        let card: AgentCard = serde_json::from_str(json).expect("deserialize legacy card");
        assert_eq!(card.handle, "acme");
        assert!(card.name.is_empty());
        assert!(card.payment_requirements.is_empty());
        assert!(card.supported_interfaces.is_empty());
    }
}
