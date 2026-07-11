# The Company Brain

"The core maintains the company brain" means two things: the runtime keeps a
Company's durable cognitive state consistent, and it drives cognition over
that state through the `Brain` port. Cognition itself is
[Medulla](../integrations/medulla.md)'s job; the runtime never reimplements
it.

Supporting docs: [charter.md](charter.md), [approvals.md](approvals.md),
[memory.md](memory.md).

## Definition

```text
CompanyBrain
├── identity   CompanyId (ULID), Ed25519 keypair, tiny.place @handle, Agent Card
├── charter    name, output, mission, human_role, policies (mutable at runtime)
├── roster     teammates: id, role, description, tier hint, tool grants, budgets
├── memory     compressed cycle traces (~20:1 working memory) + task results
├── context    addressable chunks (the RLM environment: put/list/peek/search)
├── world      event log (append-only), ledger, open tasks, approval queue
└── feedback   inbox of feedback items + links to filed GitHub issues
```

Each family maps to a storage port ([runtime/ports.md](../runtime/ports.md)):
identity/charter/roster/world → `CompanyStore` + `EventLog`, memory →
`MemoryStore`, context → `ContextStore`. All of it exports as one bundle
([runtime/lifecycle.md](../runtime/lifecycle.md)).

## The cycle

One `CycleRunner::run(company, events) -> CycleReport`, in Medulla
vocabulary (glossary: Cycle, Pass, Dispatch):

1. **Drain** pending events (batched — a burst of webhooks becomes one
   cycle).
2. **Load** working memory: recent compressed traces, the context index,
   roster and charter.
3. **Think**: `Brain::run_cycle`. The orchestrator tier reviews everything
   and loops refine → delegate → dispatch. Mid-cycle callbacks come back to
   the host: tool calls (grant-checked against the manifest), context ops,
   and effects.
4. **Gate**: every effect that crosses the trust boundary — send an external
   message, spend money, publish, file — passes the
   [`ApprovalGate`](approvals.md). Allowed effects execute; gated ones park
   and the cycle result records "awaiting approval".
5. **Persist**: compressed trace → memory, events and effect outcomes →
   event log, spend/usage → ledger.

Inherited from Medulla: termination by construction (pass ceiling, budget
exhaustion, forced final turn) and at least one channel response per cycle.
Added by the kernel: a wall-clock timeout and a per-cycle budget cap.

## One voice

The Operator chats with **the company**, not with teammates. The brain
compresses inward (everything the roster did becomes working memory) and
compiles outward (one coherent reply per surface, produced by dispatch).
This is a product invariant, not just an implementation detail: exposing
per-agent chat would leak roster mechanics that the
[prosumer language rules](../glossary.md) forbid, and it would break the
single-approval-queue model.

## State ownership boundaries

| State | System of record | Notes |
| --- | --- | --- |
| Charter, roster, ledger, approvals | OpenCompany (`CompanyStore`/`EventLog`) | never delegated |
| Working memory (compressed traces) | `MemoryStore` — fs default, TinyCortex target | hosted Medulla also keeps server-side compressed state per session; the local copy is authoritative for export |
| Context chunks | `ContextStore` | ditto |
| Conversation history with the hosted brain | TinyHumans backend (per-session messages) | mirrored locally via the read surface when needed |
| Channel credentials, tool state | OpenHuman domains | reached through ports, never copied |
| Reputation, payments record | tiny.place ledger (on-chain / directory) | the local ledger journals our view |

The rule: **anything needed to move a company between hosts lives behind the
four storage ports**; everything else is reconstructible or belongs to a
neighbor system.
