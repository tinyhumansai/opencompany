# Runtime

The runtime is the part of OpenCompany that is owned outright: the kernel
that keeps each [Company](../glossary.md)'s brain state durable, runs cycles
against the [Brain](../integrations/medulla.md), gates effects through
approvals, and serves the HTTP surface. Everything that touches a neighbor
system sits behind a port trait.

Supporting docs:

- [ports.md](ports.md) — the port trait contracts (normative)
- [manifest.md](manifest.md) — `company.toml` schema
- [lifecycle.md](lifecycle.md) — company state machine and durability
- [api.md](api.md) — HTTP routes and auth
- [config.md](config.md) — configuration and the one-key story
- [users.md](users.md) — human collaborators: magic-link/password sign-in,
  sessions, invites, and chat attribution

## Responsibilities

The kernel owns:

- **Manifest parsing and validation** with prosumer-friendly errors.
- **The cycle loop**: normalize stimuli into events, batch them per company,
  invoke the `Brain`, service its callbacks (tools, context ops), route its
  effects through the `ApprovalGate`, persist the results.
- **Durability**: append-only event log, replay on boot, checkpointed
  drain on shutdown, tar export/import of the whole company bundle.
- **Multi-company hosting**: a registry of running `CompanyRuntime`s with
  per-company isolation (one serial cycle queue each; companies run
  concurrently).
- **The HTTP surface**: operator API, agent-facing A2A endpoint, webhooks.

The kernel explicitly does **not** own cognition (Medulla), model routing
(TinyHumans backend), tool implementations (OpenHuman / TinyAgents), memory
internals (TinyCortex or any store), or the agent economy (tiny.place).

## Crate layout (target)

Today's modules (`src/app`, `src/server`, `src/openhuman`, `src/tiny` — see
[docs/modules/](../../modules/)) remain; the spec adds:

```text
src/ports/      one file per port trait (brain, store, events, memory,
                context, channel, tools, economy, approvals, secrets)
src/company/    manifest.rs, runtime.rs (CompanyRuntime, RuntimeBuilder),
                cycle.rs (CycleRunner), registry.rs (CompanyRegistry)
src/brain/      hosted.rs (HostedMedullaBrain), stub.rs, sidecar.rs (gated)
src/economy/    tinyplace adapter, card generation, signer management
src/store/      fs (default), sqlite (gated)
src/feedback/   capture, scrubber, github filing
```

`AppState` grows a `CompanyRegistry`; `src/error.rs` grows variants
(`Manifest`, `Store`, `Brain`, `Economy`, `PolicyDenied`, `Http`) so every
port returns the crate `Result<T>`.

## Feature flags

| Feature | Adds |
| --- | --- |
| *(default)* | kernel, fs store, hosted brain client, operator API |
| `tiny` | TinyAgents embedding (existing flag; used by stub brain and local workers) |
| `sqlite` | SQLite store implementations |
| `tinycortex` | TinyCortex `MemoryStore`/`ContextStore` adapters |
| `tinyplace` | tiny.place economy adapter and A2A routes |
| `sidecar` | Node sidecar brain for self-hosters |

The default build MUST stay small and compile offline; every feature degrades
to a stub or a clear "not enabled" error, never a panic.

## DB-agnosticism

No storage engine appears in the kernel. The four storage ports
(`CompanyStore`, `EventLog`, `MemoryStore`, `ContextStore`) each ship a
file-based default (a human-inspectable bundle under
`~/.opencompany/companies/<slug>/` — see [lifecycle.md](lifecycle.md)), and a
platform operator implements the same traits over Postgres, S3, or anything
else. Export is defined as "read everything through the ports"; import is the
inverse — so migration between backends is total by construction.
