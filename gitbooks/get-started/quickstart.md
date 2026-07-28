---
description: From zero to a running company in a few commands.
---

# Quickstart

You can explore every company template with nothing but the code. To let the
agents actually run, you add one key — your company's brain subscription.

## 1. Get a key (unlocks the brain)

Grab a **TinyHumans API key** at [tinyhumans.ai](https://tinyhumans.ai). It
unlocks [Medulla](../overview/medulla.md), the orchestrator that runs your
company.

```sh
export TINYHUMANS_API_KEY="th-..."
```

Without a key you can still build, inspect, and explore every template. Add the
key when you're ready to put Medulla in the driver's seat.

## 2. Pull in the runtimes

```sh
git submodule update --init --recursive
```

## 3. Check a company before you launch it

Pick any template and validate its definition in plain language:

```sh
cargo run --bin opencompany -- check companies/agentic_marketing_agency
```

## 4. Launch it

```sh
cargo run --bin opencompany -- serve --company companies/agentic_marketing_agency
```

The host comes up on `127.0.0.1:8080`. Point `--company` at any other folder
under `companies/` to run a different business — same host, different company.

## Prefer a console + hot reload?

One script spins up a company **and** its operator console in development mode:

```sh
./scripts/launch-demo.sh marketing up     # console → :5173, host API → :8080
./scripts/launch-demo.sh marketing down   # stop and clean up
```

Run `./scripts/list-demos.sh` to see every available company and its short
alias.

## What next

- [Your first company](your-first-company.md) — the operator journey, install
  to earnings.
- [Pricing & your key](pricing.md) — how the brain subscription works.
- [Developer docs](../developers/README.md) — build, extend, and deploy the
  runtime.
