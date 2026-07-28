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
| `--home <DIR>` | OpenCompany home holding company bundles. Default `$HOME/.opencompany/companies`. |
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
bundle directory. Both accept `--home <DIR>`.

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
