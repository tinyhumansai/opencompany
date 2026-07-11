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
}

/// Callbacks the brain makes into the host mid-cycle.
pub trait CycleHost: Send + Sync {
    async fn call_tool(&self, call: ToolCall) -> Result<ToolResult>;
    async fn context_op(&self, op: ContextOp) -> Result<ContextOpResult>;
    async fn emit_effect(&self, effect: Effect) -> Result<EffectDisposition>;
}

pub enum EffectDisposition {
    Executed,
    PendingApproval(ApprovalId),
    Denied { reason: String },
}
```

`CycleRequest` carries `{cycle_id, company_id, events, compressed_history,
roster, context_index}`; `CycleResult` carries channel responses, new
compressed traces, ledger deltas, and token usage. Implementations:
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
`PaymentReceived`.

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
