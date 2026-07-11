# Memory

What a company remembers, where it lives, and the Operator's rights over it.

## What is remembered

| Kind | Written by | Retention |
| --- | --- | --- |
| Compressed cycle traces (~20:1 working memory) | every cycle | rolling window feeds cycles; older traces retained until evicted |
| Task results (delegated work products) | cycles | durable |
| Context chunks (documents, research, transcripts the brain filed) | cycles, imports | durable, content-addressed |
| Customers, engagements, decisions, outcomes | cycles (as structured task results / context) | durable |
| Feedback items and their issue links | feedback flow | durable |

Conversation history with the hosted brain also exists server-side per
session ([integrations/medulla.md](../integrations/medulla.md)); the local
stores remain authoritative for export and migration.

## Port boundary

Memory is two ports, not a database
([runtime/ports.md](../runtime/ports.md)):

- **`MemoryStore`** — traces and task results; the shape of Medulla's
  `CyclePersistence` (`save_trace`, `recent_traces`, `save_task_result`,
  `evict`).
- **`ContextStore`** — the RLM environment (`put`/`list`/`peek`/`search`)
  the brain queries lazily instead of stuffing context windows.

**TinyCortex is the intended backend for both**
([integrations/tinycortex.md](../integrations/tinycortex.md)) but is a
choice, not a dependency: the fs default preserves the one-key promise, and
DB-agnosticism applies to memory exactly as to every other store.

## Compounding

Each cycle loads recent traces, so decisions and outcomes bias future work —
this is the mechanism behind "memory compounds" in the
[vision](../vision/README.md). Eviction (`evict` with an `EvictionPolicy`)
keeps the working window bounded; evicted traces are archived, not deleted,
until retention policy or the Operator says otherwise.

## Operator rights (normative)

- **Inspect**: `GET /api/v1/companies/{id}/memory/traces` and the exported
  bundle expose everything remembered, human-readably.
- **Delete**: the Operator MAY delete any memory item or context chunk;
  deletion propagates to the backing store and is journaled (that a deletion
  happened is auditable; the content is gone).
- **Redact**: customer-content redaction requests are honored across traces
  and chunks — required for the privacy stance in
  [feedback-loop/privacy.md](../feedback-loop/privacy.md).
- **Export**: memory travels with the bundle; no store may hold memory
  hostage ([runtime/lifecycle.md](../runtime/lifecycle.md), export).
