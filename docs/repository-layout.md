# Repository layout

OpenCompany is a Rust 2024 Cargo workspace: one configurable host, plus the
shells that embed it. Business types are data, not code, just a `company.toml`
manifest plus docs, and the operator console is a separate Vite app.

## Crate layout

```text
Cargo.toml                  Virtual workspace root: members, shared deps, [patch]
crates/opencompany-core/    The host: package `opencompany-core`, library crate
                            `opencompany`, binary `opencompany`, with its `src/`,
                            `tests/`, `benches/`, `examples/` and `build.rs`.
crates/opencompany-app/     The Tauri desktop shell (its own workspace + lock)
crates/opencompany-tui/     The terminal client: the host, embedded, in ratatui
```

Every `src/...` path below and in the rest of `docs/` is short for
`crates/opencompany-core/src/...`. What the crate embeds and reads at build
and test time — `companies/`, `frontend/`, `vendor/` —
stays at the repository root, which is `../..` from `CARGO_MANIFEST_DIR`.

## Host source tree

```text
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
src/harness/            Execution engines: the embedded OpenHuman runtime and agents (feature `openhuman`)
src/hive/               Hive desks: completion episodes, speech over MCP, Jev routing, referral
src/tiny/               TinyAgents/OpenHuman status surface
src/globals/            The global baseline: agents, workflows, skills, starting tool belt
src/ledger/             Dynamic ledgers: declared record shapes and the append-only fold
src/crypto/             Offline Ed25519 verification (wallet sign-in, runner handshake)
src/ingest/             Turning dropped files and links into memory
src/metering/           Usage and finances metering projections
src/harness/            Harness engines agents run their turns on
src/runner/             Machines executing host work on the operator's hardware
src/workflows/          Running company workflows on the tinyflows engine
src/chargebee/          Chargebee billing integration (backend service + agent tools)
src/paypal/             PayPal wallet and transaction visibility
src/bin/opencompany.rs  CLI entrypoint
companies/              22 business definitions (a company.toml + docs each)
frontend/               Company-agnostic operator console (Vite + React)
docs/gitbooks/          The published GitBook: overview, get started, developer guide
docs/spec/              Architecture reference
docs/modules/           Per-package design docs
scripts/qa/             Release checks against a deployed tenant
vendor/openhuman/       OpenHuman git submodule
vendor/openhuman/vendor/tinyagents/
                        TinyAgents inherited from OpenHuman
```

## Package surfaces

| Package | Owns |
| --- | --- |
| `app` | Runtime config and shared state |
| `company` | Manifest parsing, validation, and boot |
| `ports` | Kernel trait seams and shared types |
| `store` | File-based default stores |
| `policy` | The manifest-driven approval gate |
| `brain` | The offline cognition seam |
| `runtime` | Company runtime and cycle loop |
| `server` | The Axum router |
| `openhuman` | Launcher seams |
| `tiny` | Vendored TinyAgents status |
| `companies/_globals` | The global baseline every company starts from |
| `ledger` | Dynamic ledgers and the append-only fold |
| `crypto` | Offline Ed25519 signature verification |
| `ingest` | Dropped files and links into memory |
| `metering` | Usage and finances metering projections |
| `harness` | Engines agents run their turns on |
| `runner` | Machines executing host work on operator hardware |
| `workflows` | Running company workflows on the tinyflows engine |
| `chargebee` | Chargebee billing integration |
| `paypal` | PayPal wallet and transaction visibility |

## Where to go next

- [`docs/spec/README.md`](spec/README.md): the architecture reference
- [`docs/modules/`](modules/): per-package design docs
- [`companies/README.md`](../companies/README.md): the full company catalog
- [`docs/running-locally.md`](running-locally.md): builds, Docker, deploys
