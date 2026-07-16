# 04 — WS4: openhuman as a Library (the Harness)

## Scope

Replace the out-of-process OpenHuman seam (`src/openhuman/{launcher,rpc,
tools,channel}.rs`, JSON-RPC behind feature `openhuman-rpc`) with **direct
library embedding** of `vendor/openhuman` (`openhuman_core`): one openhuman
`Agent` per manifest `[[agent]]`, with memory, inference provider, tools,
skills, and approval policy injected through `AgentBuilder`.

This supersedes the JSON-RPC integration described in
[`docs/spec/integrations/openhuman.md`](../spec/integrations/openhuman.md) —
the "library-crate split" that doc lists as an upstream candidate is realized
by linking `openhuman_core` directly. WS9 updates that doc and the roadmap
(including the "TinyAgents is the harness" non-goal) to match.

## Design

### Cargo strategy

```toml
openhuman_core = { package = "openhuman", path = "vendor/openhuman",
                   optional = true, default-features = false }

[features]
openhuman = ["dep:openhuman_core"]   # NEW: library harness
openhuman-rpc = ["dep:reqwest"]      # legacy; kept one release, then deleted
```

`src/harness/` compiles only under `feature = "openhuman"`. The default build
stays offline and echo-brained — the spec's one-key promise holds. Legacy
`src/openhuman/` modules survive one release behind `openhuman-rpc`; the
reusable tool/channel definitions in `tools.rs`/`channel.rs` are ported into
`harness/build.rs` before the module is retired.

### Module layout

```
src/harness/
  mod.rs        # HarnessPool: CompanyId -> Vec<Arc<CompanyAgent>>
  build.rs      # manifest [[agent]] -> AgentBuilder
  provider.rs   # inference Provider (hosted Medulla only)
  memory.rs     # OcMemory: openhuman Memory trait over opencompany ports
  policy.rs     # tool_policy -> ApprovalGate bridge
  cost.rs       # TurnCost -> LedgerEntry + UsageMeter
```

### Core types

```rust
pub struct CompanyAgent {
    pub agent_id: String,
    pub role: String,
    agent: tokio::sync::Mutex<openhuman_core::agent::harness::session::Agent>,
}
pub struct HarnessPool { agents: RwLock<HashMap<CompanyId, Vec<Arc<CompanyAgent>>>> }
impl HarnessPool {
    pub async fn ensure(&self, company: &CompanyRecord, deps: &HarnessDeps) -> Result<()>;
    pub async fn run(&self, company: &CompanyId, agent_id: &str, message: &str) -> Result<String>;
}
```

`build_agent` (in `build.rs`) wires each manifest agent:

```rust
AgentBuilder::default()
    .provider(hosted_provider(&config)?)          // provider.rs
    .memory(Arc::new(OcMemory::new(company_id, agent_id, deps.memory, deps.context)))
    .workflows(enabled_skills)                    // parsed SKILL.md bodies via
                                                  // openhuman::skills::ops_parse
    .tools(company_tools(manifest, agent))        // manifest.tools ∩ agent.tools
    .tool_policy(ApprovalPolicy::new(deps.approvals, agent.budget_usd_daily))
    .workspace_dir(company_home.join("workspace"))
    .model_name(model_for_tier(&agent.tier))
    .post_turn_hooks(vec![cost_hook])             // cost.rs
    .build()
```

### Provider (`provider.rs`)

openhuman's `inference::provider::Provider` is a trait, so opencompany brings
its own implementation speaking to the **hosted Medulla brain / TinyHumans
API** — consistent with the spec non-goal "not a model host": no local-LLM or
BYO-model path ships. A `MockProvider` exists for tests only. `BrainMode`
continues to select hosted vs echo; the sidecar mode is subsumed by the
embedded harness.

### Memory adapter (`memory.rs`)

`OcMemory` implements `openhuman_core::memory::traits::Memory` over the
existing `MemoryStore` + `ContextStore` ports, namespacing entries
`{company}/{agent}`. When the `tinycortex` backend is active, delegate
directly — openhuman's memory value types are re-exported from the tinycortex
crate, so translation is near-zero. `recall_relevant_by_vector` degrades to
FTS/substring recall on the fs backend (best effort, never errors). This is
exactly the "memory pluggable and configurable" requirement: the backend is
chosen by `OPENCOMPANY_STORAGE`, not by openhuman.

Operator rights from
[`company-brain/memory.md`](../spec/company-brain/memory.md) flow through
unchanged: everything `OcMemory` stores is inspectable/deletable/exportable
via the opencompany ports it writes to.

### Approval bridge (`policy.rs`)

The `tool_policy` closure returns `ToolPolicyDecision::require_approval` for
effects matching manifest policy — `[policy].mode` uses the same three words
as OpenHuman's security tiers (readonly/supervised/full) by design, so the
mapping is 1:1, plus `always_approve` kinds and per-agent
`budget_usd_daily` / `auto_approve_under_usd` thresholds. Parked decisions go
through the existing `ApprovalGate` port and journal, so the console's
approvals surface and `resolve_approval` flow work unchanged; approve/deny
resumes the suspended tool call inside the agent turn.

### Cost hook (`cost.rs`)

A post-turn hook reads openhuman's `TurnCost`
(`UsageInfo { input_tokens, output_tokens, cached_input_tokens }`,
`total_usd()`) and writes both:

1. `CompanyStore::append_ledger(LedgerEntry { kind: "inference.spend",
   amount_usd, memo: agent_id })` — feeds Finances;
2. `UsageMeter::record(UsageSample { agent, provider, tokens…, cost_usd })` —
   feeds Usage (WS5).

OAuth-connected tool invocations record `SampleKind::OauthCall` samples with
the provider name — that populates the console's calls-by-provider chart.

### Multi-agent & desk routing

openhuman is single-agent; **group-chat routing stays opencompany's job**.
The ops `chat` handler (WS3) resolves the target desk's `members`, picks the
addressed agent (mention-parse; else the first member as desk lead), calls
`HarnessPool::run`, and journals both directions to the `EventLog` — the
operator message as today, the reply as a new variant:

```rust
CompanyEvent::AgentReply { chat_id: String, agent_id: String, text: String }
```

which is what the GraphQL `Chat.history` resolver reads back. Fan-out (every
member responds) is a later policy flag; v1 is single-responder.

`RuntimeBuilder` grows `.with_harness(Arc<HarnessPool>)`;
`CompanyRuntime::run_cycle` routes agent turns through the pool when the
feature is on, echo brain otherwise.

## Subtasks (commit-sized; serial within this workstream)

1. `feat(harness): openhuman feature + path dep, compile-only scaffold`
2. `feat(harness): hosted provider + MockProvider`
3. `feat(harness): OcMemory adapter over MemoryStore/ContextStore`
4. `feat(harness): approval bridge (tool_policy -> ApprovalGate)`
5. `feat(harness): cost hook (ledger + UsageMeter)`
6. `feat(runtime): HarnessPool wiring, AgentReply event, desk routing in chat`
7. `chore(openhuman): deprecate openhuman-rpc modules` (one release later)

## Dependencies

None to start (day one, parallel with WS1). Consumes WS1 skill parsing (step
build), WS3's `UsageMeter` port (step 5). Feeds WS5 and WS2c (chat history).

## Tests & exit criteria

Unit: memory-adapter isolation + namespacing; cost mapping (zero-usage → no
entry). Feature (under `--features openhuman`): full chat cycle with
`MockProvider` → reply + ledger delta + usage sample; gated tool → parked →
resolve → resumed turn. Exit: default build (no features) is byte-for-byte
unaffected; e2e chat round-trip green per
[09-verification.md](09-verification.md).
