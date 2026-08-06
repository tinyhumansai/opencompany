# Port Contracts

Normative trait sketches for the kernel's seams. Signatures are Rust 2024
(`async fn` in traits), all returning the crate `Result<T>` from
`src/error.rs`. Names are binding; exact field lists on the payload types may
evolve during Phase 1 without a spec change, methods may not.

Ports live one-per-file under `src/ports/`.

This file is the **index**: the assembly that holds the ports together, the
default implementation of each, and the map below. The trait contracts
themselves live in five focused files, split out (issue #427) once this one had
grown past the repo's 500-line cap for a Markdown file — the same reason the
event vocabulary those traits carry moved to [`events.md`](events.md)
(issue #371).

## The ports

| Port | Contract | Seam |
| --- | --- | --- |
| `Brain`, `CycleHost` | [ports-cognition.md](ports-cognition.md#brain) | cognition: the cycle the kernel never reimplements |
| `ChannelAdapter` | [ports-cognition.md](ports-cognition.md#channeladapter) | inbound/outbound conversation surfaces, and the `TurnStep` activity trace on a bubble |
| `CompanyStore` | [ports-state.md](ports-state.md#companystore) | charter, roster, ledger, approval queue, operator overlays |
| `EventLog` | [ports-state.md](ports-state.md#eventlog) | the append-only journal (its vocabulary: [events.md](events.md)) |
| `MemoryStore` | [ports-state.md](ports-state.md#memorystore) | compressed cycle traces and task results |
| `ContextStore` | [ports-state.md](ports-state.md#contextstore) | the RLM environment the brain queries lazily |
| `SecretStore` | [ports-state.md](ports-state.md#secretstore) | per-company credentials |
| `UserStore`, `SessionStore`, `LoginCodeStore` | [ports-state.md](ports-state.md#userstore-sessionstore-logincodestore) | human collaborators and their credentials ([users.md](users.md)) |
| `ToolProvider` | [ports-effects.md](ports-effects.md#toolprovider) | tool catalog + invocation, grant-checked |
| `AgentEconomy` | [ports-effects.md](ports-effects.md#agenteconomy) | the tiny.place seam |
| `ApprovalGate` | [ports-effects.md](ports-effects.md#approvalgate) | policy evaluation and the approval queue |
| `TaskStore` | [ports-console.md](ports-console.md#taskstore) | the Kanban board |
| `ArtifactStore` | [ports-console.md](ports-console.md#artifactstore) | versioned task outputs and the human-edit diff |
| `WorkspaceStore` | [ports-console.md](ports-console.md#workspacestore) | the note tree |
| `FactStore` | [ports-console.md](ports-console.md#factstore) | the operator's curated Memory view |
| `UsageMeter` | [ports-console.md](ports-console.md#usagemeter) | durable per-company usage accounting |
| `SkillStateStore` | [ports-console.md](ports-console.md#skillstatestore) | installed-skill state overlay |
| `InboxStore` | [ports-console.md](ports-console.md#inboxstore) | per-teammate email inboxes |
| `RunStore` | [ports-runs.md](ports-runs.md#runstore) | one attempt at a task, and its trace |

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
| `TaskStore`, `WorkspaceStore`, `FactStore`, `UsageMeter`, `SkillStateStore`, `InboxStore` | fs bundle | sqlite, mongodb |
| `UserStore`, `SessionStore`, `LoginCodeStore` | fs bundle | sqlite, mongodb |
