---
description: One configurable host, a layered kernel, and swappable ports.
---

# Architecture

OpenCompany is a single configurable host. Companies are data — a manifest plus docs — and the operator console is a separate Vite app. Everything a company needs from the outside world sits behind a Rust trait ("port") and is swappable.

## Layered design

Dependencies point strictly downward. OpenCompany owns the kernel; every neighbor is behind a port.

```
L4  Surfaces        Axum HTTP (operator API, A2A, webhooks), CLI, console
L3  Company Brain   cycle loop, approvals, effect routing, feedback loop
L2  Kernel ports    Brain, CompanyStore, EventLog, MemoryStore, ContextStore,
                    ChannelAdapter, ToolProvider, AgentEconomy, ApprovalGate
L1  Adapters        hosted-medulla | openhuman-rpc | tinyagents | tinycortex |
                    tinyplace | fs (default)
L0  Substrate       api.tinyhumans.ai, openhuman-core, tiny.place, filesystem
```

## Who owns what

| Concern                                                  | Owner                  | OpenCompany's role                               |
| -------------------------------------------------------- | ---------------------- | ------------------------------------------------ |
| Cognition (orchestrate / delegate / dispatch)            | Medulla                | called via the `Brain` port; never reimplemented |
| Model access, billing                                    | TinyHumans backend     | sends tier names + credential; never sees SKUs   |
| Tools, channels, credentials                             | OpenHuman              | consumed via JSON-RPC; gaps go upstream as PRs   |
| In-process LLM sub-work                                  | TinyAgents             | embedded library behind `ToolProvider`           |
| Long-term memory                                         | TinyCortex (candidate) | behind `MemoryStore`; default is file-based      |
| Identity, discovery, payments                            | tiny.place             | behind `AgentEconomy`                            |
| Company definition, brain state, lifecycle, HTTP surface | **OpenCompany**        | owned outright                                   |

The takeaway: OpenCompany reuses Medulla, OpenHuman, TinyAgents, TinyCortex, and tiny.place instead of reimplementing them. Changes those layers need go **upstream as PRs.**

## Crate layout

```
src/app/                Runtime config and shared state
src/company/            Company manifest parsing, validation, and boot
src/ports/              Kernel port traits and shared types
src/store/              File-based CompanyStore/EventLog/Memory/Context/Secrets
src/policy/             Manifest-driven ApprovalGate
src/brain/              Offline EchoBrain (the default cognition seam)
src/feedback/           Feedback items, privacy scrubber, GitHub issue filing
src/runtime/            CompanyRuntime, CycleRunner, cron scheduler, registry
src/server/             Axum HTTP router and handlers
src/server/users/       Human sign-in: magic link, passwords, sessions, invites
src/openhuman/          OpenHuman launcher seams
src/tiny/               TinyAgents/OpenHuman status surface
src/bin/opencompany.rs  CLI entrypoint
companies/              Business definitions (a company.toml + docs each)
frontend/               Company-agnostic operator console (Vite + React)
vendor/openhuman/       OpenHuman git submodule
vendor/tinyagents/      TinyAgents git submodule
```

## Design goals

* Make simple company workflows concise; make complex ones explicit, inspectable, and testable.
* Reuse the neighboring runtimes; keep the default build small and gate deeper integrations behind features.
* **One required credential**; everything else optional and gracefully degrading.
* Keep docs, examples, and public APIs aligned.

For the normative port contracts and the full spec, see [`docs/spec/`](../../docs/spec/) in the repository.
