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
    /// Which model the path's cycle usage is metered against, folded onto the
    /// closed `ModelSlug` vocabulary (issue #1749). `None` when the path cannot
    /// name one: an injected brain, `echo` (runs no model), `sidecar` (the
    /// host's `InferenceClient` picks it), and `hosted` (Medulla picks it
    /// upstream and the `orch:usage` frame does not carry it). The `harness`
    /// path names its model **per turn** instead, off the live provider.
    pub model: Option<ModelSlug>,
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

### What the console is told: `cognition` (issue #1735)

`Cognition` is a *metering and diagnosis* descriptor with an open set of path
labels, and it is deliberately not `Serialize`. What a console surface actually
asks is narrower and closed — **can a teammate answer me, and if not, is that
mine to fix?** — so the host derives that answer and reports it, rather than
publishing the path vocabulary and letting every view restate the rule.

```rust
// src/server/cognition.rs
pub enum CognitionState {
    Configured, Unconfigured, RestartRequired, Unavailable, Undetermined,
}

pub enum InferenceResolution { Resolved, Nothing, Unreadable }

pub fn cognition_state(
    path: &str,
    harness_reachable: bool,
    resolution: InferenceResolution,
) -> CognitionState;
```

| state | what is true | remedy |
|---|---|---|
| `configured` | a path that runs a real model is live | — |
| `unconfigured` | a harness is attached, the config reads clean, nothing is set | Settings → Inference, **in-app** |
| `restart-required` | a provider resolves, but the runtime predates it | the restart, **not** another provider choice |
| `unavailable` | no agent harness is reachable on this host | a different build or host wiring — say so plainly |
| `undetermined` | a harness is reachable, but the host could not *read* the config | **none that can be named** |

Not a fifth `*_in_build` boolean. Cognition is two facts at once — a harness is
reachable, *and* a model resolved at runtime — and only the second is actionable
without a new binary. One flag collapses them and sends the operator who needs a
settings page off looking for a build.

**The states are named for their remedy, not their mechanism.** Two mechanisms
land on `unavailable`: the `openhuman` feature is not compiled in, or it is and
the embedder built its runtimes without `app::harness::attach` — the shipped
desktop-shell bug that module exists to end. The operator can act on neither, so
splitting them would offer a distinction they cannot use; folding either into
`unconfigured` would offer a settings page that cannot help.

`restart-required` and `undetermined` exist for the same reason, and both were
added because collapsing them into `unconfigured` produced a false sentence.
Brain selection happens once, in `RuntimeBuilder::build`, so a company
configured *after* boot keeps the echo brain until its runtime is rebuilt —
telling that operator "this company has no model configured" sends them back to
the page they have just come from to redo work they did correctly. The remedy is
the restart `ops::inference` already reports as `restartRequired` (issue #266).
The banner links to the card that owns it and stops there: whether a restart can
be *performed* in place is that card's fact (`can_rebuild_in_place`, issue
#1736), and promising the button from chat would be the switch that does nothing
all over again.

`undetermined` is the state whose remedy **cannot be named**. A config the host could not read is no
evidence that saving one would help — the #266 doctrine — which is why
`ops::inference`'s `runner_gap_for` degrades a resolve error to
`RunnerGap::NotWired` rather than `InferenceRequired`, and why its
`unreadable_inference_config_is_not_restartable` regression exists. Cognition
must not make, on the same runtime, the promise that route declines to make. The
banner for it therefore carries no settings link and does not borrow the harness
wording either, since a harness *is* attached.

All three outcomes of the config read are carried, because all three mean
something different to the operator. It is consulted only on the degraded path —
a company whose brain is not the echo brain, or that has no harness at all, pays
neither the manifest load nor the secret-store resolve, because neither can
change the answer.

That is why the second input is **harness reachability, not the Cargo feature**.
`cfg!(feature = "openhuman")` says the harness was compiled in; it does not say
this company's runtime was ever handed a pool. `cognition_state` therefore takes
`ops::inference::harness_reachable`, and alongside it
`ops::inference::inference_resolution` — the same predicates
`restart_pending` and `runner_gap_for` gate their restart and
configure-inference advice on (issues #266, #514) — so the chat banner and the
Inference card cannot disagree about whether Settings → Inference is a remedy or
a dead end for one company.

Derived on every read from the brain the runtime is holding
(`runtime.cognition().path`) plus that reachability check, so it cannot
drift from reality. Reported on `GET …/capabilities` beside `mediaInBuild`,
`searchInBuild`, `publishInBuild` and `mcpInBuild` — the per-company answer to
"what can this company actually do" — as `cognition`.

The console consumes it in chat (issue #1734): on anything but `configured` the
transcript carries a banner naming the cause and, where one exists, the remedy,
and every company-side row is marked as a placeholder rather than presented as
the teammate's own words. `ChatMessage` carries no provenance, so a company-level
state is the only shape that answer has — see `MessageRow`'s `cognition` prop for
why marking beats suppressing. The same state reaches `ThreadPanel`, because a
reply read inside a thread is the same false attribution as one read in the
channel, and the marker's tooltip names the cause it was given rather than
restating `unconfigured`'s remedy for both.

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

`CycleRequest` carries `{cycle_id, company_id, events, event_seqs}`. It also
carried `compressed_history`, `roster` and `context_index` until issue #1175:
no `Brain` read any of the three, and populating the first two cost a
`recent_traces` read plus an unbounded `ContextStore::list` scan on every
cycle. A cycle therefore carries **no working memory** — see
[company-brain/memory.md](../company-brain/memory.md) for what does the
compounding instead. `CycleResult` carries channel responses, new
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

That sentence was not fully retired by #411, and saying so matters more than
the tidier telling: #411 fixed the *classified* failures, and everything the
classifier had no arm for still landed on `Unknown` and rendered as the same
catch-all. The workspace tools were the whole family in that gap — issue #887
found `workspace_read` writing five distinct, actionable sentences for its five
failure exits and the operator being shown "Something went wrong with this
action." for all of them, which is why the underlying fault in a live turn
could not be diagnosed at all. #887 closed that fall-through by widening
`INTRINSIC_TOOLS` to the workspace family.

Membership in that list is an audit, not a one-line edit. A tool qualifies when
its message is OC-authored copy **and** every one of its failure exits is free
of host paths and raw store errors — `OpenCompanyError::StoreIo` renders an
absolute host path, and what lands in `result` is shown on the console *and*
written into the persisted trace. #887 therefore sanitised every workspace exit
first and added the names second; the reverse order would have published the
host's filesystem layout into every agent turn.
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
