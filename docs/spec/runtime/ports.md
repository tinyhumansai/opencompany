# Port Contracts

Normative trait sketches for the kernel's seams. Signatures are Rust 2024
(`async fn` in traits), all returning the crate `Result<T>` from
`src/error.rs`. Names are binding; exact field lists on the payload types may
evolve during Phase 1 without a spec change, methods may not.

Ports live one-per-file under `src/ports/`.

## Brain

The cognition seam. The kernel never reimplements the cycle; it hands events
to a `Brain` and services the brain's callbacks through a `CycleHost`.

```rust
// src/ports/brain.rs
pub trait Brain: Send + Sync {
    async fn run_cycle(&self, req: CycleRequest, host: &dyn CycleHost)
        -> Result<CycleResult>;

    /// How this brain does cognition and where its usage is metered.
    /// Defaults to an injected brain: per-cycle metering, unknown provider.
    fn cognition(&self) -> Cognition { Cognition::default() }
}

/// Metering + diagnosis descriptor for a cognition path (issue #174).
pub struct Cognition {
    /// `harness` | `hosted` | `sidecar` | `echo` | `custom`.
    pub path: &'static str,
    /// Provider slug the path's cycle usage is metered under.
    pub provider: &'static str,
    pub metering: UsageMetering,
}

pub enum UsageMetering {
    /// The path meters each agent turn itself (the openhuman harness) and MUST
    /// report a zero `CycleResult::token_usage`, or its spend is double-counted.
    PerTurn,
    /// `CycleRunner` meters whatever the cycle reports (hosted Medulla reads it
    /// off the `orch:usage` frame).
    PerCycle,
    /// No model runs on this path (the echo brain) — a zero Usage reading is the
    /// truth, not a missing hook.
    None,
}
```

`CycleRunner` **enforces** both non-`PerCycle` arms: a path that declares
`PerTurn` or `None` and then reports non-zero cycle usage is warned about and
dropped, never metered. Only `PerCycle` reaches the meter.

```rust
/// Callbacks the brain makes into the host mid-cycle.
pub trait CycleHost: Send + Sync {
    async fn call_tool(&self, call: ToolCall) -> Result<ToolResult>;
    async fn context_op(&self, op: ContextOp) -> Result<ContextOpResult>;
    async fn emit_effect(&self, effect: Effect) -> Result<EffectDisposition>;
    /// Parks an already-decided effect for approval, without re-evaluating it.
    async fn park_effect(&self, effect: Effect) -> Result<ApprovalId>;
}

pub enum EffectDisposition {
    Executed,
    PendingApproval(ApprovalId),
    Denied { reason: String },
}
```

`emit_effect` submits an effect for a **decision** — the gate evaluates it and
executes, parks, or denies it. `park_effect` is for a brain that hosts its own
policy layer and has **already** decided: the harness brain's openhuman
`ApprovalPolicy` blocks a gated tool call inside the agent turn, and the
projected call is parked as-is so the operator can see and resolve it. Passing
it back through `emit_effect` would re-decide it against the coarser
`ApprovalGate` taxonomy and quietly drop it (issue #172).

`CycleRequest` carries `{cycle_id, company_id, events, compressed_history,
roster, context_index}`; `CycleResult` carries channel responses, new
compressed traces, ledger deltas, and `token_usage` — tokens **and** cost, which
`CycleRunner` meters onto the `UsageMeter` + ledger for every path that is not
`PerTurn`-metered (issue #174), so hosted/sidecar cognition is accounted for and
not only the openhuman harness. Implementations:
`HostedMedullaBrain` (default — see
[integrations/medulla.md](../integrations/medulla.md)), `StubBrain`
(single TinyAgents call, offline tests), `SidecarBrain` (feature `sidecar`),
and a far-future `NativeBrain` (TinyAgents graph port; interface only).

Kernel-side backstops regardless of implementation: a wall-clock timeout and
a per-cycle budget cap. Medulla's own guarantees (termination by
construction, ≥1 response per cycle) are inherited, not re-verified.

## CompanyStore

Durable company records: charter, roster, ledger, approval queue.

The record also carries the **operator overlays** — teammates, desk members,
desk order, operator-created desks, and (issue #168) `overlay_workflows`: the
workflow graph bodies authored at runtime through the console's create dialog or
the orchestrator's `create_workflow` tool. These are persisted here rather than
written into `companies/<name>/workflows/<id>.toml` because the company source
tree is the version-controlled seed and, in hosted mode, a read-only crate mount
(writing there failed every hosted tenant with `EROFS`). Every reader unions the
two sources — `load_workflow_union` / `list_workflows_union` in
`src/company/workflow_file.rs` — with the committed seed file winning on an id
collision, matching the manifest-first convention the desk resolvers use. The
workflow scheduler (issue #169) reads through the same union, which is what
makes a schedule on a console-created workflow survive a restart.

**A boot rebuild is not a plain re-seed** (issue #208). `RuntimeBuilder::build`
persists the freshly-parsed seed manifest with exactly one field merged:
`[workflows].enabled` becomes the seed's ids plus every surviving
`overlay_workflows` id not already among them. `create_workflow` writes the graph
body and the enabled id in one save, so overlay presence *is* the enablement
invariant — deriving from the bodies carries a runtime enablement forward and
re-heals records an earlier rebuild wiped, with no migration. Ids dropped from
the seed, and enabled ids with no surviving body, do not survive.

Every other manifest field is **seed-authoritative**; for `[tools]` and
`[policy]` that is a security property, not a convention — a record-wins merge
would let a runtime grant or a relaxed approval mode outlive the operator
revoking it in version control. Runtime additions that must persist get their own
overlay field instead (`overlay_agents`, `overlay_desks`, the `SecretStore` for
console MCP credentials).

```rust
// src/ports/store.rs
pub trait CompanyStore: Send + Sync {
    async fn load(&self, id: &CompanyId) -> Result<Option<CompanyRecord>>;
    async fn save(&self, record: &CompanyRecord) -> Result<()>;
    async fn list(&self) -> Result<Vec<CompanySummary>>;
    async fn append_ledger(&self, id: &CompanyId, entry: LedgerEntry) -> Result<()>;
}
```

## EventLog

Append-only, replayable. Boot replays the tail to rebuild in-flight state.

```rust
// src/ports/events.rs
pub trait EventLog: Send + Sync {
    async fn append(&self, id: &CompanyId, event: CompanyEvent) -> Result<EventSeq>;
    async fn read_from(&self, id: &CompanyId, seq: EventSeq, limit: usize)
        -> Result<Vec<StoredEvent>>;
    fn subscribe(&self, id: &CompanyId) -> BoxStream<'static, StoredEvent>;
}
```

`CompanyEvent` variants: `OperatorMessage`, `WebhookReceived`,
`ScheduleFired`, `A2aTaskReceived`, `ApprovalResolved`, `FeedbackFiled`,
`PaymentReceived`, `LifecycleChanged`, `AgentReply`, `MemoryFactDeleted`,
`TaskDispatched`, `McpCallFailed`, `WorkflowCreated` (a new saved workflow
graph was authored + enabled via the console `POST …/workflows` route or the
orchestrator's `create_workflow` tool; journaled best-effort after persist),
`TaskSteered` (an operator paused, cancelled, or redirected an in-flight task
or delegation), `DeskTaskCompleted` (a dispatched board task finished its run —
the terminal anchor a per-task timeline ends on; "completed" means the run
stopped, not that it succeeded, and `column` carries where the card landed).

### Per-task event correlation (issue #185)

The journal is company-scoped, so the events a dispatch *produces* cannot be
filtered back to their task by shape alone. `AgentReply` and `McpCallFailed`
therefore carry an optional `task_id`, stamped by the harness when the
producing turn ran inside a `TaskDispatched` cycle and absent for an ordinary
chat turn. Together with the `TaskDispatched` / `DeskTaskCompleted` anchors,
that is what `GET …/tasks/{task_id}` filters on to assemble a task's timeline.

Both fields are additive — `#[serde(default, skip_serializing_if = …)]` — so
every already-persisted event loads unchanged and an untagged event serializes
byte-for-byte as it did before the field existed. No stored log needs
migrating, and the cross-backend export/import round-trip is unaffected.

`TaskRecord` gains `parent_task_id` on the same contract, recording the
task-to-task edge that `origin_chat_id` (a *conversation*, shared by every
sibling spawned in that thread, and absent entirely on a board-native card)
cannot express. It is the parent half of the Task Detail screen's lineage.

`OutboundMessage` gains `task_id` on the same contract (issue #246): the card a
chat turn **opened**, so the console can say a card exists instead of leaving an
operator to notice it on the board. It is journaled onto that turn's
`AgentReply.task_id`, which widens that field's meaning from "the dispatch that
produced this reply" to "the card this reply is about" — a card-creating reply
now also appears on that card's timeline, which is the lineage an operator
wants and costs no schema change. A turn that opens several cards reports the
**first**: the journal field is a single optional id, and widening it would
break the byte-identical round-trip, so the claim is incomplete but never wrong.
Both `chat/history` surfaces (REST and GraphQL) project it from the shared
`MessageView`, so the chip survives a transcript reload on either.

## MemoryStore

The equivalent of Medulla's `CyclePersistence`; TinyCortex is the target
backend ([integrations/tinycortex.md](../integrations/tinycortex.md)).

```rust
// src/ports/memory.rs
pub trait MemoryStore: Send + Sync {
    async fn save_trace(&self, id: &CompanyId, trace: CompressedTrace) -> Result<()>;
    async fn recent_traces(&self, id: &CompanyId, limit: usize)
        -> Result<Vec<CompressedTrace>>;
    async fn save_task_result(&self, id: &CompanyId, result: TaskResult) -> Result<()>;
    async fn evict(&self, id: &CompanyId, policy: EvictionPolicy) -> Result<u64>;
}
```

## ContextStore

The RLM environment: addressable chunks the brain queries lazily. Mirrors
Medulla's `ContextStore` port.

```rust
// src/ports/context.rs
pub trait ContextStore: Send + Sync {
    async fn put(&self, id: &CompanyId, chunk: ContextChunk) -> Result<ChunkAddr>;
    async fn list(&self, id: &CompanyId, prefix: &str) -> Result<Vec<ChunkMeta>>;
    async fn peek(&self, id: &CompanyId, addr: &ChunkAddr, range: Option<Range<usize>>)
        -> Result<String>;
    async fn search(&self, id: &CompanyId, query: &str, limit: usize)
        -> Result<Vec<ChunkHit>>;
}
```

## ChannelAdapter

Inbound/outbound conversation surfaces. The built-in `"operator"` channel is
always present; others (email, tinyplace-dm, …) usually delegate to OpenHuman.

```rust
// src/ports/channel.rs
pub trait ChannelAdapter: Send + Sync {
    fn channel_id(&self) -> &str; // "operator", "email", "tinyplace-dm", ...
    fn inbound(&self) -> BoxStream<'static, InboundMessage>;
    async fn send(&self, msg: OutboundMessage) -> Result<()>;
}
```

### OutboundMessage steps (activity trace)

An `OutboundMessage` carries an additive, scrubbed `steps: Vec<TurnStep>` — the
visible processing behind that bubble (tool calls, thinking runs, surfaced MCP
failures), folded from the harness turn's progress stream:

```rust
// src/ports/types.rs
pub struct OutboundMessage {
    pub channel: String,
    pub text: String,
    // Omitted on the wire when empty (serde skip_serializing_if).
    pub steps: Vec<TurnStep>,
}

pub struct TurnStep {
    pub kind: TurnStepKind,      // tool_call | thinking | note
    pub status: TurnStepStatus,  // ok | error | running
    pub label: String,           // display_label, else the tool name
    pub detail: Option<String>,  // whitelisted enrichment, or a scrubbed cause
    pub elapsed_ms: Option<u64>,
}
```

Per-bubble ownership: the operator bubble carries the orchestrator's steps; a
delegated desk bubble carries that desk lead's steps. **Zero steps is
meaningful** — a memory-served or tool-less answer runs none, which is how the
console distinguishes it from a tool-backed one, and how a silently-failed MCP
call becomes visible (surfaced as an `error` step on the operator bubble rather
than a vague acknowledgement).

Security: `steps` never carry raw tool arguments, tool output, or call ids —
only a safe label, a whitelisted/scrubbed detail, and an elapsed time. They are
never written to the memory store (`memory_loop::outcome_chunk` stays
text-only), so a step detail can never be retrieved and re-injected into a later
turn. The fold + scrub lives in `src/harness/steps.rs` (compiled under the
`openhuman` feature). Non-harness brains (echo, medulla) emit no steps.

## ToolProvider

Tool catalog + invocation, scoped per company. Backed by OpenHuman JSON-RPC
by default, TinyAgents built-ins as fallback.

```rust
// src/ports/tools.rs
pub trait ToolProvider: Send + Sync {
    async fn catalog(&self, company: &CompanyId) -> Result<Vec<ToolSpec>>;
    async fn invoke(&self, company: &CompanyId, call: ToolCall) -> Result<ToolResult>;
}
```

Tool grants come from the manifest (`[tools].allow`, per-agent `tools`);
`invoke` MUST reject calls outside the grant before any side effect.

## AgentEconomy

The tiny.place seam ([integrations/tinyplace.md](../integrations/tinyplace.md)).

```rust
// src/ports/economy.rs
pub trait AgentEconomy: Send + Sync {
    async fn ensure_registered(&self, identity: &CompanyIdentity)
        -> Result<RegistrationState>;
    async fn publish_card(&self, identity: &CompanyIdentity, card: &AgentCard)
        -> Result<()>;
    async fn send_a2a_task(&self, to: &AgentAddr, task: A2aTask)
        -> Result<A2aTaskHandle>;
    async fn quote(&self, requirement: &PaymentRequirement) -> Result<Quote>;
    async fn pay(&self, quote: &Quote, budget: &BudgetScope) -> Result<PaymentReceipt>;
}
```

`pay` MUST fail if the `BudgetScope` (derived from `[budget]` and delegated
signer caps) would be exceeded; the ledger records every receipt.

## ApprovalGate

Policy evaluation and the approval queue
([company-brain/approvals.md](../company-brain/approvals.md)).

```rust
// src/ports/approvals.rs
pub trait ApprovalGate: Send + Sync {
    async fn evaluate(&self, company: &CompanyId, effect: &Effect)
        -> Result<PolicyDecision>; // Allow | RequireApproval | Deny
    async fn park(&self, company: &CompanyId, effect: Effect) -> Result<ApprovalId>;
    async fn resolve(&self, id: &ApprovalId, verdict: Verdict, by: Actor)
        -> Result<Option<Effect>>;
}
```

## SecretStore

Per-company secrets (channel credentials, GitHub token). Company A's secrets
MUST be invisible to company B.

```rust
pub trait SecretStore: Send + Sync {
    async fn get(&self, company: &CompanyId, key: &str) -> Result<Option<SecretValue>>;
    async fn set(&self, company: &CompanyId, key: &str, value: SecretValue) -> Result<()>;
}
```

## Console-surface stores (WS3)

Seven additional ports back the operator console's durable surfaces. They follow
the same one-trait-per-file convention (`src/ports/{tasks,workspace,facts,
usage,skills_state,inbox,runs}.rs`), key everything on `CompanyId`, return the
crate `Result<T>`, and are covered by the conformance suite
([storage.md](storage.md)). Their fs/sqlite/mongodb backends live alongside the
five core ports.

### TaskStore

The Kanban task board (`src/ports/tasks.rs`).

```rust
pub trait TaskStore: Send + Sync {
    async fn list(&self, company: &CompanyId) -> Result<Vec<TaskRecord>>;
    async fn upsert(&self, company: &CompanyId, task: &TaskRecord) -> Result<()>;
    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool>;
}
```

`TaskRecord` carries `{id, title, note, column, priority, assignee,
updated_at}`.

`column` ∈ `todo|planning|in_progress|paused|in_review|done` — the
`BOARD_COLUMNS` constant in `src/ports/tasks.rs`, which is the one authority the
REST write boundary, the dispatch edge and the harness lifecycle seam all read,
and which the console mirrors in the same order. (`paused` arrived with
steering, issue #111; this line used to omit it.) Entering `in_progress` is what
dispatches the card; nothing dispatches out of `done`.

`todo` is the one **not started** column, and the board's one manual-entry
column: the console's `+` button lives there alone and `POST …/tasks` defaults
to it (issue #206), so an operator cannot create a card straight into
`in_progress` or a terminal column. The transcript's "Add to board" action
(issue #246) relies on exactly that default: it omits `column` so the *server*
decides where a chat-created card lands, which is what keeps the human drag into
`in_progress` the only thing that spends an agent turn.

**The collapsed `backlog` pool (issue #301, epic #183 §3).** `todo` used to be
one of two not-started columns: `backlog` was the unqueued pool *and* where the
lifecycle returned work needing another pass (a failed dispatch, a cancellation,
an orchestrator `revise` verdict). #206 split them deliberately, to record *why*
a card had not started — never picked up vs bounced back. #301 reverses that:
the distinction is **provenance, not position**, and every return path already
stamps its reason onto the card's note (`review_note`'s "reviewed: needs another
pass — …", the dispatch error text, `[operator] cancelled while in flight`),
which the board renders on the card. So a task that cannot proceed goes **back
to To-do with the reason on the card**, never into a stuck state of its own.

Nothing about that is silent for stored data: `backlog` is no longer a board
column, so a card persisted under it would fail `is_board_column` and vanish
from the board — the exact silent disappearance #205 exists to prevent.
`TaskRecord::column` therefore deserializes through a normalizer that rewrites
the legacy `backlog` literal to `todo`. Every backend funnels through it (sqlite
and mongodb store the record as a `task_json` string, the fs bundle as a JSON
array), so one seam heals every stored board lazily on read and the next upsert
persists the new literal. Reads heal; **writes do not** — the REST DTOs
deserialize `column` as a plain string and validate it separately, so a client
still sending `backlog` gets a `400` naming the valid set.

`planning` sits between intake and dispatch: the card is being turned into a
plan. It is **accepted but inert** — nothing writes it automatically yet.
Epic #183 §4's auto-advance owns it and is blocked on #242/#243; the vocabulary lands
first so §4's code can write the column through a boundary that already accepts
it, rather than having #242-dependent code write a column the host rejects. An
operator may drag a card into it manually and nothing happens, which is correct:
`planning` is not the dispatch edge.

`assignee` names a **roster teammate id, a desk, or nobody** (`""`), resolved by
`crate::runtime::assignee` against the full roster — manifest agents, operator
overlay teammates, and desks (by id or case-insensitive name). The write plane
rejects anything else with a `400` and stores the canonical key rather than what
was typed; dispatch refuses a card whose assignee no longer resolves, returning
it to `todo` with the reason on the note, and writes the agent that actually
worked the card back onto `assignee` so the board names the doer (issue #205).
That write-back covers an unassigned card and a card assigned to a teammate; a
card assigned to a **desk** keeps the desk id. A desk assignment records who the
card belongs to, and dispatch only chooses which member runs the current turn, so
writing the lead back would erase the desk from the board the first time the card
ran — the member that did the work is named on the note instead.

### ArtifactStore

Versioned task outputs and the human-edit diff (`src/ports/artifacts.rs`,
issue #187) — what the Task Detail **Artifacts** tab renders.

```rust
pub trait ArtifactStore: Send + Sync {
    async fn list(&self, company: &CompanyId, task_id: Option<&str>)
        -> Result<Vec<ArtifactRecord>>;
    async fn get(&self, company: &CompanyId, id: &str) -> Result<Option<ArtifactRecord>>;
    async fn upsert(&self, company: &CompanyId, artifact: &ArtifactRecord) -> Result<()>;
    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool>;
}
```

`ArtifactRecord` carries `{id, task_id, title, kind, versions, created_at,
updated_at}`; `ArtifactKind` ∈ `text|markdown|image|file`. Each
`ArtifactVersion` carries `{version, body, author, author_id, created_at,
step_seq?, note?}`; `ArtifactAuthor` ∈ `agent|operator`.

**Versions are append-only.** An operator's pre-approval edit is recorded as a
*new version by a different author*, never as a mutation of the agent's — which
is what makes `human_edit_diff()` ("the agent wrote X, the operator shipped Y")
answerable at any later point, and why no route rewrites a stored version.
Editing in place would destroy the single highest-signal quality datum the
product can produce: sustained high `churn` on an agent's artifacts means its
instructions need work.

Independent of the per-task timeline (#185). A version may cross-reference the
step that produced it via the optional `step_seq`, but this port never reads the
event journal, so an artifact stands on its own.

Backends must uphold `store::conformance::assert_artifact_store`, which asserts
the full ordered version history survives a round-trip — a backend that stored
only the latest body would otherwise pass a naive check while silently
destroying the diff.

### WorkspaceStore

The Obsidian-style note tree (`src/ports/workspace.rs`), seeded from the
company's `workspace/**` on first use.

```rust
pub trait WorkspaceStore: Send + Sync {
    async fn tree(&self, company: &CompanyId) -> Result<Vec<WorkspaceNode>>;
    async fn read(&self, company: &CompanyId, id: &str)
        -> Result<Option<(WorkspaceNode, String)>>;
    async fn write(&self, company: &CompanyId, id: &str, content: &str)
        -> Result<WorkspaceNode>;
    async fn create(&self, /* parent, name, kind, content */) -> Result<WorkspaceNode>;
    async fn rename_move(&self, /* id, new_name, new_parent */) -> Result<WorkspaceNode>;
    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool>;
    async fn is_empty(&self, company: &CompanyId) -> Result<bool>;
}
```

Nodes are folders or files (`NodeKind`); `[[wikilink]]` backlinks are derived
at read time by the GraphQL layer.

### FactStore

The operator's durable, hand-curated Memory view — distinct from the two
cognition-facing memory ports (see
[company-brain/memory.md](../company-brain/memory.md)).

```rust
pub trait FactStore: Send + Sync {
    async fn list(&self, company: &CompanyId, /* query, kind, page */)
        -> Result<Vec<FactRecord>>;
    async fn upsert(&self, company: &CompanyId, fact: &FactRecord) -> Result<()>;
    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool>;
}
```

`FactRecord` carries `{id, kind, title, body, source, updated_at}`; `FactKind`
∈ `fact|preference|person|project|reference`.

### UsageMeter

Durable per-company usage accounting (`src/ports/usage.rs`); the WS5
usage/finances projections read it.

```rust
pub trait UsageMeter: Send + Sync {
    async fn record(&self, company: &CompanyId, sample: &UsageSample) -> Result<()>;
    async fn query(&self, company: &CompanyId, since_millis: u64)
        -> Result<Vec<UsageSample>>;
}
```

`UsageSample` records one metered event (`SampleKind::Inference` tokens or
`SampleKind::OauthCall`). **Writers** — three, and they do **not** share failure
semantics:

| Writer | Called | On write failure |
| --- | --- | --- |
| `metering::inference::record_inference_usage` (always compiled) | per cycle by `CycleRunner`, for every cognition path that is not `PerTurn`-metered | logs and swallows — returns `()`, so the cycle still succeeds |
| `metering::oauth::record_oauth_call` | per connected-tool call | logs and swallows — returns `()` |
| `harness::cost::record_turn_cost` | per turn by the openhuman harness's cost hook | **propagates** — returns `Result<()>` and `HarnessPool::run_inner` applies `?`, so a ledger or meter failure fails the turn |

The per-cycle and OAuth paths hold "accounting never fails the work it accounts
for"; the per-turn harness path deliberately does not, because it writes the
`inference.spend` ledger entry in the same call and a silently dropped ledger
write is a money bug. **Retention:** backends evict samples older than
**90 days** (`RETENTION_DAYS`, the console's maximum `D90` window) on write,
anchored to the newest observed sample for deterministic eviction. Samples are
non-secret accounting rows; money still resolves from the ledger and `[budget]`.

### SkillStateStore

Per-company installed-skill state overlay (`src/ports/skills_state.rs`) —
enable/disable and provenance on top of the read-only `skills/` directory.

```rust
pub trait SkillStateStore: Send + Sync {
    async fn list(&self, company: &CompanyId) -> Result<Vec<SkillState>>;
    async fn set(&self, company: &CompanyId, state: &SkillState) -> Result<()>;
    async fn remove(&self, company: &CompanyId, slug: &str) -> Result<bool>;
}
```

`SkillState` carries the slug, `enabled`, and a `SkillSource`
(`company|registry|custom`).

### InboxStore

Per-teammate email inboxes and their messages (`src/ports/inbox.rs`).

```rust
pub trait InboxStore: Send + Sync {
    async fn inboxes(&self, company: &CompanyId) -> Result<Vec<InboxMeta>>;
    async fn set_enabled(&self, company: &CompanyId, key: &str, meta: &InboxMeta)
        -> Result<()>;
    async fn messages(&self, company: &CompanyId, /* key, page */)
        -> Result<Vec<EmailRecord>>;
    async fn append(&self, company: &CompanyId, msg: &EmailRecord) -> Result<()>;
    async fn mark_read(&self, /* company, key, ids */) -> Result<u64>;
}
```

Real send/receive depends on the domain/SMTP transport and the HMAC-signed
inbound ingest webhook ([api.md](api.md)); the store itself is transport-blind.

### RunStore

One attempt at a task, and its trace (`src/ports/runs.rs`). A `RunRecord`
carries the task and agent it belongs to, its 1-based `attempt` ordinal, a
status, the cost it accrued, and — on failure — why. A `RunStepRecord` is one
entry of its trace, keyed `(run_id, step_seq)` on a run-scoped dense counter
rather than an `EventSeq`.

```rust
pub trait RunStore: Send + Sync {
    // storage verbs
    async fn create_run(&self, company: &CompanyId, spec: NewRun) -> Result<RunRecord>;
    async fn get_run(&self, company: &CompanyId, id: &str) -> Result<Option<RunRecord>>;
    async fn put_run(&self, company: &CompanyId, run: &RunRecord) -> Result<()>;
    async fn list_runs(&self, company: &CompanyId, filter: &RunFilter)
        -> Result<Vec<RunRecord>>;
    async fn append_run_step(&self, company: &CompanyId, step: &RunStepRecord) -> Result<()>;
    async fn list_run_steps(&self, company: &CompanyId, run_id: &str)
        -> Result<Vec<RunStepRecord>>;

    // transitions — provided methods; legality is enforced here, not per backend
    async fn begin_run(&self, /* company, id, trigger_event_seq */) -> Result<RunRecord>;
    async fn finish_run(&self, /* company, id, outcome */) -> Result<RunRecord>;
}
```

**The transitions are the API; the storage verbs are the seam.** `create_run`
mints `Pending` and allocates the attempt ordinal, `begin_run` moves
`Pending → Running`, and `finish_run` settles a run into a parked or terminal
status. Legality lives in the provided methods, so no backend can re-derive the
state machine and drift from the others. `put_run` writes a row verbatim with no
check at all — it exists because Rust cannot hide a trait method from a `dyn`
caller, and it is documented as the backend seam rather than something to call.

`RunStatus` is `pending · running · waiting_approval · paused · succeeded ·
failed · cancelled`. The two parked statuses are separated by **who unblocks
them**: `waiting_approval` means a *person* must act; `paused` means anything
else must — a dependency, a rate limit, a missing credential, a retry, an
operator steer. Defining the split by who resolves it, rather than by cause,
keeps it correct as new blocking reasons appear. `waiting_approval` is
re-enterable: approval grants are single-use and argument-exact, so a run that
could only stop once would force approvals to be batched into one prompt.

**Boot reaping.** `reap_orphaned_runs` runs at startup, before dispatch and the
scheduler spawn, and settles every `pending`/`running` row as `failed` with an
orphan reason. This is a proof rather than a timeout heuristic, resting on three
invariants held elsewhere in the runtime: cycles are process-local
`tokio::spawn`s, exactly one process may write a given company's journal, and
cycles serialise on a per-company mutex. Any active row present at boot is
therefore necessarily dead. The two parked statuses are never reaped — parked is
not orphaned.

The fs backend stores runs in `runs.jsonl` (last-write-wins per id) and steps in
`run-steps.jsonl` (a true append, folded per `(run_id, step_seq)` on read).
Deliberately not one file per run: that would make a run id a path component,
and a store must never let an id it did not mint address the filesystem.

#### Who writes a run, and when

A run wraps a cycle; it never replaces one. Four writers, in order:

1. **`CompanyRuntime::dispatch_task`** — the single choke point every dispatch
   passes through — mints the `Pending` row *before* the cycle is spawned, and
   puts its id on the `TaskDispatched` event so the journal is self-describing.
   If the row cannot be written the dispatch proceeds anyway with
   `run_id: None`: record-keeping never fails the work it records.
2. **`CycleRunner::run_locked`** calls `begin_run` right after the event's
   append yields its seq — the serial lock is held and the seq now exists, so
   the row can name the exact log line that drove it. After the brain returns
   (`Ok` *or* `Err`) a **terminality backstop** settles any row still claiming
   to be live, so a brain that ignores `TaskDispatched` or errors out cannot
   strand one. Only a panic escapes it, which is the boot reaper's job.
3. **`HarnessBrain::run_task`** does the rich settle: the `TaskRunEnd` the steer
   loop yielded maps to a `RunStatus` (`lifecycle::run_status_for`), the folded
   cost and step count ride along, and a failure carries its reason. It returns
   before the backstop runs, so the rich settle always wins.
4. **The trace sink** (`harness::run_trace::RunTraceSink`) writes each step
   **during** the turn, from the collector task that already drains the harness
   progress stream. A tool call's start persists as `running` and its completion
   re-writes the same `step_seq` finalized — which is why killing the host
   mid-run leaves the prefix behind instead of nothing. The await lives in the
   collector, never the model loop, so a slow store slows only trace
   persistence. One sink spans every turn of the attempt (redirect re-runs, and
   a delegate's turn), so ordinals stay dense and cost folds across all of them.

The explicit price is write amplification: one row per step plus roughly three
status writes per run, against one event before. Affordable because cycles
serialise per company.

**Review vs paused at the settle.** A run that otherwise succeeded while parking
at least one approval finishes `waiting_approval`, not `succeeded` — a person
must act. A failed, cancelled or paused run keeps the reason it stopped;
relabelling it "waiting on you" would hide that reason.

#### Correlation fields elsewhere

Four additive `Option<String>` fields point back at a run. All are
`#[serde(default, skip_serializing_if = "Option::is_none")]`, so **no migration
and no backfill**: a record written before them loads with `None`, and an
untagged one serializes byte-identically to how it did before.

| Carrier | Meaning when set |
|---|---|
| `CompanyEvent::TaskDispatched.run_id` | the attempt this dispatch opened |
| `Effect.run_id` | the attempt whose turn parked this approval — stamped at the dispatch boundary, never in `ApprovalPolicy::effect_for` (a policy is per-agent and outlives runs), so a chat-parked effect stays `None` |
| `ArtifactVersion.run_id` | the attempt that wrote *this revision* — per version, so a card dispatched twice keeps both links |
| `UsageSample.run_id` | the attempt a turn's tokens were spent under; attribution only, no ledger semantics change |

Old `RunRecord`s are never synthesised from historical `AgentReply` events:
fabricating identity for attempts nobody recorded would be worse than a
pre-existing card honestly showing zero of them.

#### Reading runs back

Three surfaces, all in `src/server/ops/runs.rs`, under both scope forms:

| Route | Answers |
|---|---|
| `GET …/runs?task=&status=&limit=` | the company's attempts, newest first |
| `GET …/runs/{run_id}` | one attempt plus its full persisted step trace |
| `GET …/tasks/{task_id}` → `runs[]` | the card's attempts, additive on the task detail read |

Each hands its predicates to `RunStore::list_runs` as a `RunFilter`. **No route
here folds the journal** — that is the whole reason a run is state rather than
an event, and the sibling `GET …/workflows/runs` (which does fold, and says so)
is the cost being avoided. `?status=` takes a comma-separated list and refuses
an unknown word with a `400`, because a typo'd filter answering `[]` is
indistinguishable from "nothing matched".

Three things the wire shape refuses to imply, each a state the write path really
produces:

- **`phase`** (`active` · `parked` · `terminal`), projected from
  `RunStatus::phase`, is how a reader decides liveness. `finishedAtMillis` is
  absent for a *parked* run exactly as for a running one — a parked run can
  resume — so inferring liveness from the timestamp renders an attempt waiting
  on a person as running forever.
- **`stepCount` / `stepCountCapped`.** The count is the high-water ordinal
  persisted, capped at `run_trace::MAX_RUN_STEPS`, and written on the settle —
  so it reads `0` throughout a live run and stops meaning "steps the agent
  took" once capped. `usage` settles alongside it and is provisional until then.
- **A step's `status`** (`ok` · `error` · `running`) rides beside its `kind`. A
  host killed mid-tool-call leaves that call recorded `running` — the point of
  an incremental trace — meaning in-flight-when-the-trace-stopped, never failed.

Steps project into the console's existing `TimelineEntry` contract (`seq` /
`atMillis` / `kind` / `label` / `detail`, plus `status` and `elapsedMs`), so
`kind` widens additively to include `tool_call` · `thinking` · `note` and the
grouped-timeline renderer is reused rather than reinvented. `usage` is re-cased
to camelCase by a local DTO — `TokenUsage` carries no `rename_all` because its
field names are the decode contract for already-journaled events.

Run detail is **refresh-on-read**: steps persist incrementally, so re-reading a
live attempt shows the progress since. Streaming would widen the harness turn
stream for something a re-read already answers.

## Assembly

```rust
// src/company/runtime.rs
pub struct CompanyRuntime {
    brain: Arc<dyn Brain>,
    store: Arc<dyn CompanyStore>,
    events: Arc<dyn EventLog>,
    memory: Arc<dyn MemoryStore>,
    context: Arc<dyn ContextStore>,
    tools: Arc<dyn ToolProvider>,
    channels: Vec<Arc<dyn ChannelAdapter>>,
    economy: Option<Arc<dyn AgentEconomy>>,
    approvals: Arc<dyn ApprovalGate>,
}
```

Built by a `RuntimeBuilder` with fs/hosted defaults; a platform operator
swaps any port. `AppState` holds a `CompanyRegistry` mapping `CompanyId` →
running `CompanyRuntime`, serving both the single-company prosumer case and
the multi-tenant platform case with the same type.

### UserStore, SessionStore, LoginCodeStore

The company's human collaborators and their credentials
(`src/ports/{users,sessions,login_codes}.rs`). Full design in
[users.md](users.md).

```rust
#[async_trait]
pub trait UserStore: Send + Sync {
    async fn list_users(&self, company: &CompanyId) -> Result<Vec<UserRecord>>;
    async fn get_user(&self, company: &CompanyId, id: &str) -> Result<Option<UserRecord>>;
    async fn find_user_by_email(&self, company: &CompanyId, email: &str)
        -> Result<Option<UserRecord>>;
    async fn upsert_user(&self, company: &CompanyId, user: &UserRecord) -> Result<()>;
    async fn delete_user(&self, company: &CompanyId, id: &str) -> Result<bool>;

    async fn list_invites(&self, company: &CompanyId) -> Result<Vec<InviteRecord>>;
    async fn find_invite_by_email(&self, company: &CompanyId, email: &str)
        -> Result<Option<InviteRecord>>;
    async fn upsert_invite(&self, company: &CompanyId, invite: &InviteRecord) -> Result<()>;
    async fn delete_invite(&self, company: &CompanyId, id: &str) -> Result<bool>;
}

#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn create(&self, company: &CompanyId, session: &SessionRecord) -> Result<()>;
    async fn find_by_token_hash(&self, company: &CompanyId, token_hash: &str)
        -> Result<Option<SessionRecord>>;
    async fn list_for_user(&self, company: &CompanyId, user_id: &str)
        -> Result<Vec<SessionRecord>>;
    async fn touch(&self, company: &CompanyId, id: &str, at_millis: u64) -> Result<()>;
    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool>;
    async fn delete_for_user(&self, company: &CompanyId, user_id: &str) -> Result<u64>;
    async fn purge_expired(&self, company: &CompanyId, now_millis: u64) -> Result<u64>;
}

#[async_trait]
pub trait LoginCodeStore: Send + Sync {
    async fn create(&self, company: &CompanyId, code: &LoginCodeRecord) -> Result<()>;
    /// Atomically redeems a code. Returns the record only if THIS call consumed
    /// it; every later call returns `None`.
    async fn consume(&self, company: &CompanyId, code_hash: &str, now_millis: u64)
        -> Result<Option<LoginCodeRecord>>;
    async fn delete_for_email(&self, company: &CompanyId, email: &str) -> Result<u64>;
    async fn purge_expired(&self, company: &CompanyId, now_millis: u64) -> Result<u64>;
}
```

Normative requirements beyond the usual per-company isolation:

- `email` is unique within a company, for users and invites independently.
  Lookups by email and by token hash are on request-path hot loops and MUST be
  indexed, not scanned.
- Email lookup is **exact**. Stores never normalize on the caller's behalf, so
  a caller that skips `normalize_email` misses rather than silently matching an
  address it did not ask for.
- `LoginCodeStore::consume` MUST make its check-and-mark a **single atomic
  step**. It is the only place single-use is enforced; a read-then-write in a
  handler would be a check-time/use-time gap.
- `token_hash` and `code_hash` hold hashes only. Never store, log, or return a
  plaintext secret.

## Default implementations

| Port | Default (`src/store/fs.rs` unless noted) | Alternates |
| --- | --- | --- |
| `Brain` | `HostedMedullaBrain` (`src/brain/hosted.rs`) | stub, sidecar, native |
| `CompanyStore`, `EventLog` | fs bundle (TOML + JSONL) | sqlite, operator-supplied |
| `MemoryStore`, `ContextStore` | fs (JSONL + content-addressed blobs) | tinycortex, operator-supplied |
| `ToolProvider` | OpenHuman RPC, built-ins fallback | TinyAgents-native |
| `ChannelAdapter` | built-in operator chat | OpenHuman channels |
| `AgentEconomy` | none (companies work offline) | tinyplace |
| `ApprovalGate` | manifest `[policy]` evaluator | OpenHuman policy hook |
| `SecretStore` | fs (encrypted at rest) | OS keychain, operator-supplied |
| `TaskStore`, `WorkspaceStore`, `FactStore`, `UsageMeter`, `SkillStateStore`, `InboxStore` | fs bundle | sqlite, mongodb |
| `UserStore`, `SessionStore`, `LoginCodeStore` | fs bundle | sqlite, mongodb |
