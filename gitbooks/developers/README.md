---
description: Build on, extend, and deploy the OpenCompany runtime.
---

# Developer overview

The marketing pages describe what OpenCompany does. This section is for the
people who build on it: the Rust crate, the CLI, the company manifest format,
and how to deploy the host.

OpenCompany is a **Rust 2024 crate** — one configurable host. Business types
are data (a manifest plus docs), not code, and the operator console is a
separate Vite/React app.

## Who this is for

| Persona | What they touch |
| --- | --- |
| **Platform operator** | The crate, the provisioning API, storage ports, webhooks — embedding or hosting fleets of companies |
| **Template author** | `company.toml` manifests under `companies/` — no Rust required |
| **Runtime contributor** | The kernel, ports, adapters, and HTTP surface |

## The one invariant

The only mandatory external dependency is the **TinyHumans API key.** Storage
is DB-agnostic behind ports, tiny.place is opt-in, and every integration
degrades gracefully. Keep it that way — a violation of the one-key promise is a
release blocker.

## Map of this section

| Page | What's in it |
| --- | --- |
| [Build & run locally](quickstart.md) | Toolchain, submodules, build/test/run commands |
| [Architecture](architecture.md) | The layered design, crate layout, and ports |
| [CLI reference](cli.md) | `opencompany` subcommands |
| [Authoring companies](companies.md) | The `company.toml` manifest and template rules |
| [Deployment](deployment.md) | Docker, cloud targets, and the hosted platform harness |
| [Configuration](configuration.md) | Environment variables and the one-key story |

## Deeper reference

The in-repo specification under [`docs/spec/`](https://github.com/tinyhumansai/opencompany/tree/main/docs/spec)
is the authoritative architecture reference, and
[`docs/modules/`](https://github.com/tinyhumansai/opencompany/tree/main/docs/modules)
documents the code as it exists today. When the spec and the module docs
disagree, the spec wins for new work.
