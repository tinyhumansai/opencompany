# Repository layout

OpenCompany is a Rust 2024 crate: one configurable host. Business types are
data, not code, just a `company.toml` manifest plus docs, and the operator
console is a separate Vite app.

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
src/openhuman/          OpenHuman launcher seams
src/tiny/               TinyAgents/OpenHuman status surface
src/bin/opencompany.rs  CLI entrypoint
companies/              19 business definitions (a company.toml + docs each)
frontend/               Company-agnostic operator console (Vite + React)
docs/spec/              Architecture reference
docs/modules/           Per-package design docs
qa/                     Release checks against a deployed tenant
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

## Where to go next

- [`docs/spec/README.md`](spec/README.md): the architecture reference
- [`docs/modules/`](modules/): per-package design docs
- [`companies/README.md`](../companies/README.md): the full company catalog
- [`docs/running-locally.md`](running-locally.md): builds, Docker, deploys
