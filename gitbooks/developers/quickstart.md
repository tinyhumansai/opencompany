---
description: Toolchain, submodules, and the core build/test/run commands.
---

# Build & run locally

## Prerequisites

- A recent stable Rust toolchain (the crate targets **Rust 2024**).
- `git` with submodule support.
- Docker (optional — only for the console demo launcher and container builds).

## Get the code and its runtimes

```sh
git clone https://github.com/tinyhumansai/opencompany.git
cd opencompany
git submodule update --init --recursive
```

The submodules pull in the vendored **OpenHuman** and **TinyAgents** runtimes
under `vendor/`.

## Everyday commands

Run these from the repository root:

```sh
cargo fmt --all -- --check          # verify formatting
cargo clippy --all-targets -- -D warnings   # lint
cargo build --all-targets           # compile lib, bin, tests, examples
cargo test                          # full test suite
```

## Run the host

```sh
cargo run --bin opencompany                       # the CLI
cargo run --bin opencompany -- serve              # HTTP server on 127.0.0.1:8080
cargo run --bin opencompany -- serve --company companies/agentic_marketing_agency
```

## Feature flags

The default build is small; deeper integrations are feature-gated.

```sh
cargo check --features tiny        # compile against vendored TinyAgents
```

Building with the `tinyplace` feature and passing `serve --discoverable` opts
loaded companies into the [tiny.place](../overview/tiny-place.md) economy — see
[Deployment](deployment.md).

## Explore the runtimes without side effects

```sh
cargo run --bin opencompany -- open-human --dry-run -- status
```

Next: how it all fits together in [Architecture](architecture.md).
