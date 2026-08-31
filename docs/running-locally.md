# Running OpenCompany locally

The [README quickstart](../README.md#quickstart) gets a company up in three
commands. This page is everything past that: building the host from source,
running it under Docker Compose, the feature flags, the desktop preview, and
deploying the same images somewhere real.

- [Before you start](#before-you-start)
- [From source](#from-source)
- [Docker and Compose](#docker-and-compose)
- [Feature flags](#feature-flags)
- [Desktop preview (Tauri)](#desktop-preview-tauri)
- [Joining the tiny.place economy](#joining-the-tinyplace-economy)
- [Deploy targets](#deploy-targets)

## Before you start

OpenCompany is a Rust 2024 crate: one configurable host. Business types are
data, not code, just a `company.toml` manifest plus docs, and the operator
console is a separate Vite app. See
[repository-layout.md](repository-layout.md) for where everything lives.

A **TinyHumans API key** unlocks Medulla, the orchestrator. Without one you can
still build, inspect, and explore every company in
[`companies/`](../companies/); the agents just won't do real work. Live
cognition also needs the `medulla` feature compiled in — the from-source
commands below already build with it.

```sh
export TINYHUMANS_API_KEY="th-..."
```

## From source

```sh
# 1. Pull in the OpenHuman + TinyAgents runtimes
git submodule update --init --recursive

# 2. Build the host (the one configurable backend). `--features medulla`
#    compiles in the hosted Medulla brain that `TINYHUMANS_API_KEY` unlocks;
#    drop it for the small default build.
cargo build --features medulla

# 3. Check a company definition before you launch it
cargo run --bin opencompany -- check companies/agentic_marketing_agency

# 4. Launch that company. Point --company at any folder under companies/
cargo run --features medulla --bin opencompany -- serve --company companies/agentic_marketing_agency
```

The host is one configurable backend; each folder under
[`companies/`](../companies/) is a business definition, not its own program.
Point `--company` at a different folder to run a different business. Adding a
new business is a new folder, not a new program.

## Docker and Compose

One script spins up a company **and** its [operator console](../frontend/) in
development mode. Pass a friendly site name (or any directory name under
`companies/`) and keep the stack attached to the terminal:

```sh
./scripts/launch-demo.sh marketing up     # console → :5173, host API → :8080
# Press Ctrl-C when finished, then destroy its containers and network:
./scripts/launch-demo.sh marketing down
# Or destroy the stack and its persistent data volume:
./scripts/launch-demo.sh marketing down -v
```

The launcher bind-mounts the local checkout. Vite hot-updates frontend edits;
`cargo-watch` rebuilds and restarts the backend when Rust source, Cargo files,
or company definitions change. The first start builds the development images
and dependencies; later launches reuse named Cargo and `node_modules` caches.

Use `./scripts/list-demos.sh` to list friendly names and every available
company. Each company uses a separate Compose project and persistent data
volume. `down` removes its containers and network but keeps that volume;
`down -v` deletes the volume and its data too.

For custom ports, credentials, or feature flags, copy `.env.example` to `.env`
before launching. For production-like images without source mounts or hot
reload, run `OPENCOMPANY_COMPANY=marketing docker compose up --build` directly.

## Feature flags

The default build is deliberately small; deeper capabilities sit behind Cargo
features.

```sh
cargo check --features tiny        # compile against vendored TinyAgents
cargo check --features tinyplace   # tiny.place discovery and A2A surface
```

Preview an OpenHuman launch without starting one:

```sh
cargo run --bin opencompany -- open-human --dry-run -- status
```

## Desktop preview (Tauri)

Calls `cargo tauri` directly with OpenHuman's preflight ported into Rust: CEF
on macOS, `wry` on Linux and Windows.

```sh
cargo run --bin opencompany -- open-human --mode desktop --dry-run
cargo run --bin opencompany -- open-human --mode desktop            # launch
cargo run --bin opencompany -- open-human --mode desktop --release  # bundle
```

## Joining the tiny.place economy

To let companies trade with other agents on tiny.place, build with the
`tinyplace` feature and pass `serve --discoverable` to opt every loaded company
into going public, which means registering a `@handle`, publishing an Agent
Card, and answering inbound A2A `tasks/send` over SIWX + x402.

```sh
cargo run --features tinyplace --bin opencompany -- \
  serve --company companies/agentic_marketing_agency --discoverable
```

[`docs/modules/server/README.md`](modules/server/README.md) has the full
discovery flow and the `TINYPLACE_API_URL` / `OPENCOMPANY_PUBLIC_URL` settings.

## Deploy targets

The same two images deploy anywhere Docker runs:

| Target | Where the spec lives |
| --- | --- |
| DigitalOcean App Platform | [`.do/app.yaml`](../.do/app.yaml) |
| AWS Fargate | [`deploy/aws-ecs-task-definition.json`](../deploy/aws-ecs-task-definition.json) |
| Any Docker host | [`deploy/README.md`](../deploy/README.md) |

Checking a release against a deployed tenant is [`qa/`](../qa/README.md): a
zero-dependency console script and the checklist that goes with it.
