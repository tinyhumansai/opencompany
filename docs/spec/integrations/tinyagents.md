# TinyAgents

TinyAgents (vendored at `vendor/tinyagents`, crate `tinyagents = "1.8"`,
Rust 2024) is the recursive language-model harness: durable typed state
graphs (checkpoints, interrupts, `Send` fan-out, subgraphs), a
provider-neutral model harness (typed tools, middleware, structured output,
streaming, usage/cost), a capability registry, the `.rag`/`.ragsh` surfaces,
and an RLM runtime. It is embedded as a library behind the existing `tiny`
feature — it is **not** a server.

## What OpenCompany uses it for

- **`StubBrain`** — the Phase 1 offline brain: a single harness call so the
  whole kernel pipeline is testable without the hosted backend (the mock
  provider makes this work in CI with no network).
- **Local worker execution** — when the brain delegates sub-work that should
  run host-side (long tools, local file work), workers run as TinyAgents
  sub-agents under `ToolProvider`, with usage/cost rolled into the ledger.
- **Built-in tools** — the fallback `ToolProvider` when OpenHuman is absent.
- **`SidecarBrain` inference proxy** — the sidecar's `InferenceClient` calls
  back into the host, which fulfils it through the TinyAgents harness.
- **Observability** — the embedded Langfuse exporter, proxied through the
  TinyHumans backend with the same credential
  ([runtime/config.md](../runtime/config.md)).
- **`NativeBrain` (far future)** — a graph port of Medulla's `runCycle`;
  interface reserved, no commitment
  ([medulla.md](medulla.md)).

## Roster → execution mapping (internal detail)

A manifest `[[agent]]` entry does **not** become a standing process. The
Roster is charter data the brain reasons over; when a cycle delegates,
host-side workers are ephemeral TinyAgents sub-agents parameterized by the
teammate's role/description/grants, with recursion depth and budget caps
enforced by the harness (`SubAgentDepth`, usage rollup). This mapping is
never surfaced to prosumers ([glossary](../glossary.md), translation table).

## Registry use

The TinyAgents registry gives names to capabilities (models/tools/agents/
graphs) inside a company runtime; the kernel registers the granted tool
catalog and worker archetypes there so `.ragsh` debugging and future `.rag`
blueprints see the same catalog the brain does.

## Boundary

TinyAgents never talks to the network on its own in OpenCompany: model
access goes through the TinyHumans credential; provider keys beyond that are
out of scope ([roadmap.md](../roadmap.md), non-goals — not a model host).
