# Cognition and channel ports

The two seams a cycle runs across: the `Brain` that does the thinking, and the
`ChannelAdapter` that carries the conversation in and out. Part of the port
contracts indexed by [ports.md](ports.md).

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
`CycleRunner` meters onto the [`UsageMeter`](ports-console.md#usagemeter) +
ledger for every path that is not
`PerTurn`-metered (issue #174), so hosted/sidecar cognition is accounted for and
not only the openhuman harness. Implementations:
`HostedMedullaBrain` (default — see
[integrations/medulla.md](../integrations/medulla.md)), `StubBrain`
(single TinyAgents call, offline tests), `SidecarBrain` (feature `sidecar`),
and a far-future `NativeBrain` (TinyAgents graph port; interface only).

Kernel-side backstops regardless of implementation: a wall-clock timeout and
a per-cycle budget cap. Medulla's own guarantees (termination by
construction, ≥1 response per cycle) are inherited, not re-verified.

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
    pub kind: TurnStepKind,           // tool_call | thinking | note
    pub status: TurnStepStatus,       // ok | error | running | awaiting_approval
    pub label: String,                // display_label, else the tool name
    pub detail: Option<String>,       // WHAT IT WAS DOING — redacted arguments
    pub result: Option<String>,       // WHAT CAME BACK — a summary, or a cause
    pub failure: Option<TurnStepFailure>,  // WHY IT STOPPED — typed
    pub truncated: bool,              // the result was cut before it was read
    pub elapsed_ms: Option<u64>,
}
```

Per-bubble ownership: the operator bubble carries the orchestrator's steps; a
delegated desk bubble carries that desk lead's steps. **Zero steps is
meaningful** — a memory-served or tool-less answer runs none, which is how the
console distinguishes it from a tool-backed one, and how a silently-failed MCP
call becomes visible (surfaced as an `error` step on the operator bubble rather
than a vague acknowledgement).

A step answers three questions (issue #411). Before it did, a failure was the
single sentence "Something went wrong with this action.", and a call *blocked
pending approval*, an *unauthorized* response and a *truncated* result all
rendered identically:

- **`detail` — what it was doing.** The call's arguments, so two reads of two
  different files stop looking alike.
- **`result` — what came back.** An intrinsic OpenCompany tool's own message; a
  shape (`"12 items"`, `"4.2k characters"`) for anything else; a
  plain-language cause on a failure.
- **`failure` — why it stopped**, as a typed `TurnStepFailure`
  (`unauthorized` · `timeout` · `declined` · `blocked_by_policy` ·
  `missing_permission` · `missing_app` · `unavailable` · `failed`), projected
  from OpenHuman's `ToolFailureClass` in one exhaustive match. The console
  renders a known state; it never pattern-matches the prose in `result`.

Two statuses are **not** failures and must not be counted as such
(`TurnStepStatus::is_failure` is the one place that question is answered):
`running` (in flight when the trace stopped) and `awaiting_approval` (the
approval gate parked the call — it is waiting on a person). `truncated` marks a
result the harness cut (issue #410): a *success* whose answer is incomplete,
which no status word can express.

Security: `steps` never carry raw tool **output** or call ids. Arguments reach
`detail` only through `runtime::approval_display::redact` — the same host-side
redactor an approval card uses (issue #372), reused rather than re-derived, so
one denylist governs both surfaces. That is safe because a gated call's
arguments *are* the parked effect's payload the card already shows; it also
means #372's documented limit applies here too (the denylist matches on keys, so
a credential hidden in free text under a benign key is shown on both surfaces by
the same rule). A remote tool's output never contributes content — only a shape.
Steps are never written to the memory store (`memory_loop::outcome_chunk` stays
text-only), so a step detail can never be retrieved and re-injected into a later
turn. The fold + scrub lives in `src/harness/steps.rs` (compiled under the
`openhuman` feature). Non-harness brains (echo, medulla) emit no steps.

The same `TurnStep` rows are persisted per attempt by the
[run trace](ports-runs.md) (issue #242)
and rendered on **both** surfaces — the chat bubble and the task Attempts tab —
from one projection, so the two cannot tell an operator different stories.
