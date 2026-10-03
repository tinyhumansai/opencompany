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
- [Deploy targets](#deploy-targets)

## Before you start

OpenCompany is a Rust 2024 Cargo workspace: one configurable host
(`crates/opencompany-core`) plus the desktop and terminal shells that embed
it. Business types are data, not code, just a `company.toml` manifest plus
docs, and the operator console is a separate Vite app. See
[repository-layout.md](repository-layout.md) for where everything lives.
Commands that name a feature or a target name the package too
(`-p opencompany-core`); the bare `cargo build`/`test` lines cover every member.

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
cargo build -p opencompany-core --features medulla

# 3. Check a company definition before you launch it
cargo run --bin opencompany -- check companies/marketing_agency

# 4. Launch that company. Point --company at any folder under companies/
cargo run -p opencompany-core --features medulla --bin opencompany -- serve --company companies/marketing_agency
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

The launcher works with Docker Compose and with Podman's Docker-compatible CLI
plus Compose provider (for example, the `podman-docker` and `podman-compose`
packages). The rootless Podman warning that `/` is not a shared mount is
informational for these repository bind mounts.

The launcher bind-mounts the local checkout. Vite hot-updates frontend edits;
`cargo-watch` rebuilds and restarts the backend when Rust source, Cargo files,
or company definitions change. The first start builds the development images
and dependencies; later launches reuse named Cargo and `node_modules` caches.

The demo does not ship a default username or password. Open
<http://localhost:5173> after the first launch: a company nobody has joined yet
asks the first visitor to choose the admin login and a password, and signs
them in (`docs/spec/runtime/users.md`, "First admin"). Do that before exposing
the port to anyone else — the offer closes the moment the first account exists.

To create the admin from the shell instead — a scripted deploy, or a host you
will not be first to open — the helper securely prompts for a password, boots
and stops the backend as needed, then writes the credential, once per fresh
data volume:

```sh
# From the repository root:
./scripts/init-demo-admin.sh marketing you@example.com
./scripts/launch-demo.sh marketing up
```

Then sign in as `you@example.com` with the password you entered. Use the same
demo name in both commands: `marketing` above can be replaced by any name
printed by `./scripts/list-demos.sh`.

The helper's Docker-free regression test checks the Compose files, selected
company, and password command without starting containers:

```sh
./scripts/test-init-demo-admin.sh
```

If you run `down -v`, that account is deleted with the rest of the persistent
demo data and must be initialized again. A plain `down` keeps the account.

Use `./scripts/list-demos.sh` to list friendly names and every available
company. Each company uses a separate Compose project and persistent data
volume. `down` removes its containers and network but keeps that volume;
`down -v` deletes the volume and its data too.

For custom ports, credentials, or feature flags, copy `deploy/.env.example` to `deploy/.env`
before launching. For production-like images without source mounts or hot
reload, run `OPENCOMPANY_COMPANY=marketing docker compose -f deploy/docker-compose.yml up --build` directly.

## Feature flags

The default build is deliberately small; deeper capabilities sit behind Cargo
features.

```sh
cargo check -p opencompany-core --features tiny        # compile against vendored TinyAgents
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

## Deploy targets

The same two images deploy anywhere Docker runs:

| Target | Where the spec lives |
| --- | --- |
| DigitalOcean App Platform | [`.do/app.yaml`](../.do/app.yaml) |
| AWS Fargate | [`deploy/aws-ecs-task-definition.json`](../deploy/aws-ecs-task-definition.json) |
| Any Docker host | [`deploy/README.md`](../deploy/README.md) |

Checking a release against a deployed tenant is [`scripts/qa/`](../scripts/qa/README.md): a
zero-dependency console script and the checklist that goes with it.
