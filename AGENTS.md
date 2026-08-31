# Repository Guidelines

## Project Structure & Module Organization

OpenCompany is a Rust 2024 crate rooted at `Cargo.toml`. Rust source lives
under `src/`. Public module surfaces live in source module directories:

- `src/app/`: runtime configuration and shared Axum state
- `src/server/`: Axum router and HTTP handlers
- `src/ledger/`: dynamic ledgers — declared record shapes, the append-only fold,
  and the `derived/` folder they render into (`docs/spec/runtime/ledgers.md`)
- `src/globals/`: the global baseline — the agents, workflows, skills and
  starting tool belt every company gets whichever vertical it started from,
  authored in `globals/` and embedded at build time
  (`docs/spec/runtime/globals.md`)
- `src/openhuman/`: launcher and integration seams for the vendored OpenHuman checkout
- `src/tiny/`: optional TinyAgents crate feature/status surface

The command-line entrypoint lives in `src/bin/opencompany.rs`. Business types
are data-only definitions under `companies/` (a `company.toml` manifest plus a
`README.md` — not Cargo crates), loaded at runtime via `opencompany serve
--company companies/<name>`. What every company has regardless of which of
those it started from is authored beside them in `globals/`. The operator
console is a Vite/React app under `frontend/`. Design notes and module specifications live in `docs/`, with
`docs/spec/README.md` as the top-level architecture reference and
`docs/modules/` holding per-surface design docs.
The vendored runtime source is the `vendor/openhuman/` Git submodule. TinyAgents
is inherited from OpenHuman at `vendor/openhuman/vendor/tinyagents/`.

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
- `./scripts/dump-prompt.sh --company companies/<name>`: print the system prompt each agent in that bundle is built with (`docs/spec/runtime/agents.md`).
- `git submodule update --init vendor/openhuman`: initialize OpenHuman.
- `scripts/ci/init-vendored-submodules.sh`: initialize its vendored crates.
- `cargo check --features tiny`: compile against OpenHuman's TinyAgents pin.

Run commands from the repository root unless a future workspace layout changes
the module location.

`rust-toolchain.toml` pins an **explicit** Rust version (issue #1298), and
every `dtolnay/rust-toolchain` call site in `.github/workflows/` passes that
same version. Do not change either back to `stable`. Both said `stable` until
rustc 1.98.0 shipped on 2026-08-18 with a newly-enforced
`clippy::result_large_err`, at which point every open PR in the repo went red
at once — including PRs touching no Rust — while everyone still on 1.97.x
passed `cargo clippy` locally and could not reproduce it. A pin turns the next
stable release into one reviewable bump PR instead.

To bump: edit `rust-toolchain.toml`, then run
`scripts/ci/assert-toolchain-pin.sh`, which fails and names any workflow call
site still on the old version. That script runs in the `rust` job, so a
half-finished bump is caught rather than shipped. Because the pin is in
`rust-toolchain.toml`, a plain `cargo` in this checkout already uses it — you
do not need `cargo +<version>`, and if your `rustup` lacks the toolchain it
will fetch it.

`.cargo/config.toml` sets `RUST_MIN_STACK = 8388608` for every cargo-invoked
process (issue #895). Do not drop it. The gated suite exceeds libtest's 2 MiB
default thread stack, and when it does the failure is a `SIGABRT` that aborts
the **whole test binary** — so every test after it is skipped, and the symptom
reads as "I broke the harness" rather than "this needs a bigger stack".

8 MiB is 2.7x the measured floor: the whole gated suite aborts at 2 MiB and
passes 4182/4182 at 3 MiB (aarch64-darwin, default parallelism). The margin
covers x86-64 CI frame layout, higher CI parallelism, and growth. The depth is
cumulative `async fn` composition in the vendored OpenHuman turn chain (~117 KiB
for its largest single future, against ~32 KiB for the largest OpenCompany-owned
one), so it is not bounded from this repo — `Box::pin` at our own seam was tried
and moved it by nothing. Full evidence is in `.cargo/config.toml`.

If you change the value, say what you measured; an unexplained ceiling is the
ratchet #895 exists to complain about. Export `RUST_MIN_STACK` yourself to
override it — the file does not use `force`.

## Coding Style & Naming Conventions

Use standard `rustfmt` output and Rust 2024 idioms. Module and file names should
be `snake_case`; public types should be `PascalCase`; functions, methods,
fields, and local variables should be `snake_case`. Return `Result<T>` using
the crate error type from `src/error.rs`.

## Testing Guidelines

Add focused tests with every behavior change. Keep tests near the module they
exercise unless they verify cross-module behavior, in which case place them in
the consuming module or in `tests/` as an integration target.

A new file under `tests/` is not covered until a CI job both selects it and
enables the features its crate-level `cfg` needs (issue #475). A target missing
either builds, runs and reports zero without failing anything. The gated `Rust
(openhuman, tinycortex)` job runs `--tests` and then asserts a non-zero count
per target via `scripts/ci/assert-integration-targets-run.sh`; if your target
needs a feature set no lane builds, add the lane and run that script there too
rather than loosening the `cfg`.

A feature-gated test has the same problem one level up (issue #770). Cargo
features are additive and every CI lane pins an explicit feature set, so a test
behind `#[cfg(feature = "x")]` is compiled by `Check (--all-features)` and
executed by nothing unless some lane enables `x` — and nothing reports the
silence. Every feature therefore needs a row in `scripts/ci/feature-lanes.txt`
saying which lane runs its tests (`tested`/`partial`) or why none does
(`compile-only`, with a reason). `scripts/ci/assert-feature-lanes.sh` fails on an
unclassified feature, and fails a `compile-only` row that turns out to have a
gated test. When you add the lane, run it through
`scripts/ci/run-scoped-suite.sh`, which asserts a non-zero count — a filter that
selects nothing exits 0.

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

## Running under the platform harness (hosted mode)

This repo is also the tenant workload of the OpenCompany hosting platform:
the `opencompany-manager` control plane (the superproject at
`tinyhumansai/opencompany-microservices`, where this repo is the
`opencompany/` submodule) builds this crate into a per-tenant container and
injects its environment. When developing hosted behavior, know the seams:

- The manager injects `OPENCOMPANY_COMPANY`, `OPENCOMPANY_BIND=0.0.0.0:8080`,
  `OPENCOMPANY_DATA_DIR=/data`, and `OPENCOMPANY_PUBLIC_URL` into every
  tenant container. `OPENCOMPANY_DATA_DIR` is the instance data root for
  the workspace layout, the company-bundle home, **and** the embedded OpenHuman
  runtime's own root: `serve` derives `<data-dir>/openhuman` and exports it as
  `OPENHUMAN_WORKSPACE`, because the vendored runtime otherwise defaults its
  durable agent journal into `$HOME` — the read-only root filesystem in a tenant
  (issue #446). An unwritable journal root aborts boot; see
  `docs/spec/runtime/storage.md`. `docker/entrypoint.sh`
  additionally forwards it as `--home "$OPENCOMPANY_DATA_DIR"`, which resolves
  identically (the flag outranks the variable). Locally it is the only knob that
  isolates two `serve` processes from each other — see
  `docs/spec/runtime/storage.md`. It also injects `OPENCOMPANY_ADMIN_EMAIL`, the
  address that provisioned the instance: a standing admin invite equivalent to a
  manifest `[users].admins` entry, without which a provisioned company (whose
  manifest names nobody) has nobody eligible to sign in — see
  `docs/spec/runtime/users.md`. Plus — when database-per-tenant storage is enabled —
  `OPENCOMPANY_STORAGE=mongodb`, `OPENCOMPANY_MONGODB_URI` (credentials
  scoped to that tenant's database only), and `OPENCOMPANY_MONGODB_DB`.
- In the alternative **shared-single-DB** mode (all tenants on one logical
  MongoDB), the manager also injects `OPENCOMPANY_TENANT_ID=<tenant-slug>`.
  The workload then namespaces company ids with `<tenant>--` and records
  `owners` rows so tenants stay apart in the shared database. Isolation is
  application-layer only in this mode — a compromised container can reach
  every tenant's documents; db-per-tenant stays the security default. See
  `docs/spec/runtime/storage.md`. Unset (the default) is a full no-op.
- The manager should also inject `OPENCOMPANY_DEPLOYMENT=hosted-tenant` and,
  when product analytics is on, `OPENCOMPANY_ANALYTICS_TOKEN`. Neither is
  required to boot: an instance that says nothing is treated as **self-hosted**
  and reports nothing, which is the safe direction and the documented default
  (`docs/spec/runtime/analytics.md`). `OPENCOMPANY_TENANT_ID` alone also implies
  a hosted tenant, so shared-single-DB tenants are covered without the new
  variable; db-per-tenant tenants need it.
- Storage backend selection and the MongoDB backend are documented in
  `docs/spec/runtime/storage.md`; the port traits it implements are the
  entire persistence contract (`docs/spec/runtime/ports.md`).
- The container must serve `/healthz` on `:8080` quickly — the manager's
  wake-on-request proxy blocks on it and gives up after its startup timeout.
- Run the full platform locally by following
  `docs/local-development.md` in the superproject.

## Commit & Pull Request Guidelines

Use concise, imperative commit subjects. Keep the first line specific to the
change and avoid bundling unrelated work.

Keep commits small and concise. Commit each coherent, validated slice on its
own rather than batching many changes together, and keep the message short and
focused on that one change.

Pull requests should include a short summary, the commands run locally, and any
API or behavior changes. Include updated examples or docs when public APIs,
architecture, or expected usage changes.
