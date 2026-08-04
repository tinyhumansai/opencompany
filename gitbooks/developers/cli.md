---
description: The opencompany binary and its subcommands.
---

# CLI reference

The `opencompany` binary is the entrypoint for running and inspecting
companies. Invoke it with `cargo run --bin opencompany -- <command>` from a
checkout, or as `opencompany <command>` from an installed build.

```sh
opencompany <command> [options]
```

## `serve`

Run the Axum HTTP host.

```sh
opencompany serve --company companies/agentic_marketing_agency
```

| Flag | Purpose |
| --- | --- |
| `--bind <ADDR>` | Address to bind. Default `127.0.0.1:8080`. |
| `--company <DIR>` | A company to load at boot — a manifest file or a directory containing one. **Repeatable** for multi-company hosting. |
| `--home <DIR>` | OpenCompany home holding company bundles (`<home>/companies/<slug>`). Falls back to `OPENCOMPANY_DATA_DIR`, then to `$HOME/.opencompany`. |
| `--discoverable` | Opt every loaded company into going public on [tiny.place](../overview/tiny-place.md), regardless of each manifest's `[place].discoverable`. Needs the `tinyplace` feature to reach the network. |
| `--openhuman_root <PATH>` | Optional OpenHuman checkout path to report in `/spec`. |

## `check`

Validate a company manifest and print its effective configuration in plain
language.

```sh
opencompany check companies/agentic_marketing_agency
```

Takes a manifest file or a directory containing `company.toml` / `agents.toml`
(defaults to the current directory).

## `doctor`

Report the effective runtime configuration, which layer set each value, and
what's missing per optional capability.

```sh
opencompany doctor --company companies/agentic_marketing_agency
opencompany doctor --json
```

## `spec`

Print a JSON runtime specification. Accepts `--openhuman_root <PATH>`.

## `export` / `import`

Move a company's full state (through the storage ports) between homes.

```sh
opencompany export <company-slug> --out ./backup
opencompany import ./backup
```

`export` excludes `secrets/` and `keys/` unless `--include-secrets` is passed.
With `--features export` the output is a single `.tar`; otherwise an unpacked
bundle directory. Both accept `--home <DIR>`, and both fall back to
`OPENCOMPANY_DATA_DIR` the same way `serve` does.

## Where state lives

Every subcommand that touches company bundles resolves one root, highest
precedence first:

1. `--home <DIR>` — an explicit flag outranks both entries below.
2. `OPENCOMPANY_DATA_DIR` — the instance data root. Set it to run two hosts side
   by side; without isolation they share one company store and each one's
   teammates and desks show up in the other.
3. `$HOME/.opencompany` — the default when neither is set.

Bundles hang off `<home>/companies/<slug>` in all three cases. Installs created
before the default dropped a redundant `companies` leaf are nested one level
deeper; `serve`, `export`, and `import` move them up on first launch and print
what moved. A same-named company already at the destination is skipped with both
paths named, never merged.

`OPENCOMPANY_HOME` is not supported. It never was, so setting it used to do
nothing silently; `serve`, `export`, and `import` now abort and point at
`OPENCOMPANY_DATA_DIR`. The check runs before `--home`, so passing the flag does
not silence it.

## `open-human`

Launch a sibling OpenHuman checkout through cargo.

```sh
opencompany open-human --dry-run -- status
```

| Flag | Purpose |
| --- | --- |
| `--root <PATH>` | OpenHuman checkout path. Default `vendor/openhuman`. |
| `--mode <core\|desktop>` | Launch target. Default `core`. |
| `--dry-run` | Print the cargo command without executing it. |
| `-- <args>` | Arguments passed through to the OpenHuman binary. |
