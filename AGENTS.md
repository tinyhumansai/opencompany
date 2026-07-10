# Repository Guidelines

## Project Structure & Module Organization

OpenCompany is a Rust 2024 crate rooted at `Cargo.toml`. Rust source lives
under `src/`. Public module surfaces live in source module directories:

- `src/app/`: runtime configuration and shared Axum state
- `src/server/`: Axum router and HTTP handlers
- `src/openhuman/`: launcher and integration seams for the vendored OpenHuman checkout
- `src/tiny/`: optional TinyAgents crate feature/status surface

The command-line entrypoint lives in `src/bin/opencompany.rs`. Runnable examples
live in `examples/`. Design notes and module specifications live in `docs/`,
with `docs/spec/README.md` as the top-level architecture reference and
`docs/modules/` holding per-surface design docs.
Vendored runtime sources live under `vendor/` as Git submodules:

- `vendor/openhuman/`
- `vendor/tinyagents/`

Prefer small modules with focused responsibilities. Keep core type definitions
in a dedicated `types.rs` file and package-local tests in the module file or a
dedicated `test.rs` file when they grow.

## Build, Test, and Development Commands

- `cargo fmt --all -- --check`: verify Rust formatting without changing files.
- `cargo fmt`: format Rust source files.
- `cargo clippy --all-targets -- -D warnings`: run lint checks.
- `cargo build --all-targets`: compile library, binary, tests, and examples.
- `cargo test`: run the full test suite.
- `cargo run --bin opencompany`: run the CLI.
- `cargo run --bin opencompany -- serve`: run the Axum HTTP server on `127.0.0.1:8080`.
- `git submodule update --init --recursive`: initialize OpenHuman and TinyAgents.
- `cargo check --features tiny`: compile against vendored TinyAgents.

Run commands from the repository root unless a future workspace layout changes
the module location.

## Coding Style & Naming Conventions

Use standard `rustfmt` output and Rust 2024 idioms. Module and file names should
be `snake_case`; public types should be `PascalCase`; functions, methods,
fields, and local variables should be `snake_case`. Return `Result<T>` using
the crate error type from `src/error.rs`.

## Testing Guidelines

Add focused tests with every behavior change. Keep tests near the module they
exercise unless they verify cross-module behavior, in which case place them in
the consuming module or a future `tests/` directory.

Maintain at least 80% coverage for meaningful library behavior. Document any
intentionally untested edge case in the PR description.

## Documentation Expectations

Keep `README.md`, `docs/spec/README.md`, and module docs in `docs/modules/`
aligned with code changes. Prefer concrete examples over vague descriptions,
especially for Axum routes, OpenHuman launcher behavior, and `tiny*` feature
integration.

Keep every Markdown file, including this one, at 500 lines or fewer. When a
topic grows past that limit, split it into focused files and link them from the
module's `README.md`.

## Commit & Pull Request Guidelines

Use concise, imperative commit subjects. Keep the first line specific to the
change and avoid bundling unrelated work.

Pull requests should include a short summary, the commands run locally, and any
API or behavior changes. Include updated examples or docs when public APIs,
architecture, or expected usage changes.
