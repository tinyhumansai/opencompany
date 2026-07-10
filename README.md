<h1 align="center">OpenCompany</h1>

<p align="center">
  A Rust/Axum host for company-aware agent systems on OpenHuman and TinyHumans modules.
</p>

OpenCompany is a Rust crate for starting a company operating layer on top of
OpenHuman and the sibling TinyHumans Rust modules. It uses Axum for the HTTP
surface, vendors OpenHuman and TinyAgents as Git submodules, and keeps
TinyAgents behind a feature flag so the default scaffold stays fast.

## Package Surfaces

- `app`: runtime config and shared state.
- `server`: Axum router and HTTP handlers.
- `openhuman`: launcher seams for the vendored OpenHuman checkout.
- `tiny`: compile-time status for the vendored TinyAgents crate.

## Quick Start

```sh
git submodule update --init --recursive
cargo test
cargo run --bin opencompany
cargo run --bin opencompany -- serve --bind 127.0.0.1:8080
cargo run --bin opencompany -- spec --openhuman-root vendor/openhuman
```

Compile against vendored TinyAgents:

```sh
cargo check --features tiny
```

Preview the OpenHuman launch command without starting it:

```sh
cargo run --bin opencompany -- open-human --dry-run -- status
```

## Repository Layout

```text
src/app/                Runtime config and shared state
src/server/             Axum HTTP router
src/openhuman/          OpenHuman launcher seams
src/tiny/               TinyAgents/OpenHuman status
src/bin/opencompany.rs  CLI entrypoint
docs/spec/              Top-level architecture specification
docs/modules/           Per-package design docs
examples/               Runnable examples
vendor/openhuman/       OpenHuman git submodule
vendor/tinyagents/      TinyAgents git submodule
```

## License

OpenCompany is licensed under the GNU General Public License v3. See
[LICENSE](LICENSE).

See [docs/spec/README.md](docs/spec/README.md) for the architecture reference.
