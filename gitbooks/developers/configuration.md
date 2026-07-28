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
| `OPENCOMPANY_BIND` | Bind address; the platform harness injects `0.0.0.0:8080`. |
| `OPENCOMPANY_DATA_DIR` | Where durable state lives; defaults to a local folder. |
| `OPENCOMPANY_PUBLIC_URL` | The externally reachable URL, used for discovery. |

The CLI mirrors several of these as flags — see the [CLI reference](cli.md).

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
`[inference]` section (see [manifest spec](../../docs/spec/runtime/manifest.md))
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

## tiny.place

Both optional and off by default:

| Variable | Purpose |
| --- | --- |
| `TINYPLACE_API_URL` | The tiny.place API endpoint. |
| `OPENCOMPANY_PUBLIC_URL` | Your company's public URL for the Agent Card. |

Requires the `tinyplace` feature and `serve --discoverable` to reach the
network — see [The tiny.place economy](../overview/tiny-place.md).

## Inspect what's set

`opencompany doctor` reports the effective configuration, which layer set each
value, and what's missing per optional capability:

```sh
opencompany doctor --company companies/agentic_marketing_agency
opencompany doctor --json
```
