---
description: One required key; everything else optional and gracefully degrading.
---

# Configuration

OpenCompany is configured through environment variables and manifest tables.
The governing rule: **one required credential, everything else optional.**

## The one-key promise

`TINYHUMANS_API_KEY` is the only required credential. It unlocks
[Medulla](../overview/medulla.md), the hosted orchestrator. No flow may
hard-require a database, a funded wallet, a GitHub token, or an OpenHuman
install — a violation is a release blocker.

```sh
export TINYHUMANS_API_KEY="th-..."
```

Without it, the host still builds, inspects, and validates every company; only
live cognition is gated.

## Host settings

| Variable | Purpose |
| --- | --- |
| `OPENCOMPANY_COMPANY` | The company to load (used by container images). |
| `OPENCOMPANY_BIND` | Bind address; the platform harness injects `0.0.0.0:8080`. See [bind precedence](#bind-precedence). |
| `OPENCOMPANY_DATA_DIR` | Where durable state lives; defaults to a local folder. Set it to run two hosts side by side without them sharing one company store. The `--home` flag overrides it for company bundles only — the shared workspace directories still follow this variable, so isolating two hosts with `--home` alone only half-works. |
| `OPENCOMPANY_PUBLIC_URL` | The externally reachable URL (inbound webhooks, sign-in links). |

The CLI mirrors several of these as flags — see the [CLI reference](cli.md).

### Agent workspace checkpoints

Automatic Git history for each agent's private filesystem workspace is opt-in
in the data directory's `config.toml`:

```toml
[workspace]
git_enabled = true
```

OpenCompany creates a baseline commit, then checkpoints changes after tool
calls, including shell commands. Calls that change nothing create no commit.
The Git object database is stored beside the working tree rather than among the
agent's files. The default is `false`.

### Bind precedence

Where a flag and a variable name the same thing, the flag wins. For the
listener address:

```text
serve --bind  ⟵  OPENCOMPANY_BIND  ⟵  config.toml `bind`  ⟵  127.0.0.1:8080
```

A blank `OPENCOMPANY_BIND` counts as unset and falls through. `serve` prints
the address and the layer that supplied it on startup — `listening on
0.0.0.0:8080 (from OPENCOMPANY_BIND)` — and an address that cannot be bound
aborts boot naming that address rather than quietly reverting to the default.

The default is loopback; a wildcard bind happens only when a flag, variable,
or config entry asks for one. Full details in the
[configuration spec](../../spec/runtime/config.md).

## Inference: managed or bring-your-own (BYOK)

By default agents think with the managed TinyHumans brain, keyed by
`TINYHUMANS_API_KEY`. The managed default is also tunable by env:

| Variable | Purpose |
| --- | --- |
| `OPENCOMPANY_INFERENCE_KEY` | Per-tenant override for `TINYHUMANS_API_KEY`. |
| `OPENCOMPANY_INFERENCE_URL` | Managed base URL override. |
| `OPENCOMPANY_INFERENCE_MODEL` | Pins the **whole roster** to one workload; unset keeps each agent's tier. |

A company can instead **bring its own key** (issue #56) — OpenRouter, any
OpenAI-compatible endpoint, or a local Ollama server — via a manifest
`[inference]` section (see [manifest spec](../../spec/runtime/manifest.md))
or, live from the operator console under **Connections → Inference**. The
outbound key is **write-only**: it is stored server-side and never returned in
any status/API response. Precedence is **console override > manifest
`[inference]` > managed default**, and a switch takes effect on the agents'
next turn with no restart. A BYOK-only tenant needs no `TINYHUMANS_API_KEY` at
all.

## Storage backends

Storage is DB-agnostic behind ports. The default is **file-based** (a folder —
nothing to provision). The MongoDB backend is opt-in:

| Variable | Purpose |
| --- | --- |
| `OPENCOMPANY_STORAGE` | Backend selector, e.g. `mongodb`. Unset = file-based. |
| `OPENCOMPANY_MONGODB_URI` | Connection string (tenant-scoped credentials in hosted mode). |
| `OPENCOMPANY_MONGODB_DB` | Database name. |
| `OPENCOMPANY_TENANT_ID` | Shared-single-DB mode only; namespaces company ids. |

See [Deployment](deployment.md) for how the hosted platform injects these.

## Channels: Telegram

Setup is a bot token and nothing else. Create a bot with @BotFather and paste
its token under **Connections → Channels**; the host collects messages by
long-polling Telegram (`getUpdates`), which dials *out*, so it works on
localhost, behind NAT, and on any self-hosted box with no public URL. Needs the
`telegram` feature for the outbound transport.

| Variable | Purpose |
| --- | --- |
| `OPENCOMPANY_TELEGRAM_POLL_SECONDS` | Long-poll hold and idle back-off; default `30`. |

An inbound **webhook** is an optional fast-path for a publicly reachable host:
set `OPENCOMPANY_PUBLIC_URL` to a public **https** URL and the console offers a
webhook URL plus a secret to register with. Without one there is nothing to
offer — Telegram cannot deliver to a private address — so the console shows no
webhook at all, rather than a URL that silently never fires. Polling stands down
on its own while a webhook is registered.

## Inspect what's set

`opencompany doctor` reports the effective configuration, which layer set each
value, and what's missing per optional capability:

```sh
opencompany doctor --company companies/marketing_agency
opencompany doctor --json
```
