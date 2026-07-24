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

/// Token accounting for a cycle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Input tokens consumed.
    pub input: u64,
    /// Output tokens produced.
    pub output: u64,
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
}

impl OverlayBlob {
    /// Builds a blob from a record's two overlay collections.
    pub fn from_record(record: &CompanyRecord) -> Self {
        Self {
            agents: record.overlay_agents.clone(),
            desk_members: record.overlay_desk_members.clone(),
        }
    }

    /// Parses the persisted blob, accepting both the current object form
    /// (`{"agents":[…],"desk_members":[…]}`) and the legacy bare-array form
    /// (`[…]`, the pre-desk-overlay value that held only agents).
    /// When the current form parse fails, falls back to the legacy array; if
    /// that also fails the *original* error (from the current-form parse) is
    /// propagated so the caller sees why the object form failed rather than a
    /// misleading "expected sequence" message.
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        match serde_json::from_str::<OverlayBlob>(json) {
            Ok(blob) => Ok(blob),
            Err(original) => serde_json::from_str::<Vec<OverlayAgent>>(json)
                .map(|agents| Self {
                    agents,
                    desk_members: Vec::new(),
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
}

impl CompanyRecord {
    /// The effective member ids of a desk: the manifest's declared members
    /// first, then any operator-overlay additions for that desk, in order and
    /// deduplicated on id.
    ///
    /// This is the single source of truth for "who is on a desk", shared by the
    /// REST `list_desks` handler and the harness `desk_lead` resolver so the two
    /// cannot drift. Ordering rule: manifest order is preserved, overlay members
    /// are appended in insertion order — so the manifest's first member stays the
    /// lead, and an overlay lead only applies to a desk the manifest left empty.
    pub fn effective_desk_members(&self, desk_id: &str) -> Vec<String> {
        let mut members: Vec<String> = self
            .manifest
            .group_chats
            .iter()
            .find(|c| c.id == desk_id)
            .map(|c| c.members.clone())
            .unwrap_or_default();
        for add in &self.overlay_desk_members {
            if add.desk_id == desk_id && !members.contains(&add.agent_id) {
                members.push(add.agent_id.clone());
            }
        }
        members
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
        }
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

        // Legacy: overlay_json used to hold a bare Vec<OverlayAgent>.
        let legacy = r#"[{"id":"a","name":"A","role":"r"}]"#;
        let blob = OverlayBlob::parse(legacy).expect("legacy array");
        assert_eq!(blob.agents.len(), 1);
        assert!(blob.desk_members.is_empty());

        // The empty-array default persisted by fresh schema.
        let blob = OverlayBlob::parse("[]").expect("empty array");
        assert!(blob.agents.is_empty());
        assert!(blob.desk_members.is_empty());
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
