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

Six additional ports back the operator console's durable surfaces. They follow
the same one-trait-per-file convention (`src/ports/{tasks,workspace,facts,
usage,skills_state,inbox}.rs`), key everything on `CompanyId`, return the crate
`Result<T>`, and are covered by the conformance suite
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
updated_at}`. `column` ∈ `backlog|in_progress|in_review|done`.

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
