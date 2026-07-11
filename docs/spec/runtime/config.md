# Configuration

## The one-key promise

`TINYHUMANS_API_KEY` is the **only required secret**. It authenticates the
runtime to the TinyHumans backend (api.tinyhumans.ai) and from it derive:

- the hosted Medulla brain (the `/orchestration/v1` surface —
  [integrations/medulla.md](../integrations/medulla.md)),
- access to the model catalog for TinyAgents-backed fallbacks (tiers map to
  SKUs server-side; the runtime never names models),
- observability: TinyAgents' Langfuse exporter can proxy traces through the
  backend's telemetry ingestion using the same credential.

**Credential reality vs contract.** Today the backend authenticates
`/orchestration/v1` with a session JWT (magic-link / OAuth / login-token
exchange); a literal API key does not exist yet. The config slot is therefore
an opaque *TinyHumans credential*: the runtime accepts either a session JWT
(now) or an API key (once the backend ships an API-key path for headless
hosts — a tracked upstream workstream, [roadmap.md](../roadmap.md)). The env
var name `TINYHUMANS_API_KEY` is the stable product contract either way.

Without a credential the runtime still builds, validates manifests, runs
`opencompany check`/`spec`, and serves the inspection routes — matching the
README promise that you can build/inspect/explore keyless. Cycles require the
credential.

## Precedence

```text
env (OPENCOMPANY_*, TINYHUMANS_API_KEY)
  ⟵ ~/.opencompany/config.toml
  ⟵ company manifest
  ⟵ built-in defaults
```

Earlier layers win. `opencompany doctor` prints every effective value, which
layer set it, and what is missing for each optional capability.

## Reference

| Variable | Default | Purpose |
| --- | --- | --- |
| `TINYHUMANS_API_KEY` | — (required for cycles) | TinyHumans credential (JWT or API key) |
| `TINYHUMANS_API_URL` | `https://api.tinyhumans.ai` | Backend base URL |
| `OPENCOMPANY_BIND` | `127.0.0.1:8080` | HTTP bind address |
| `OPENCOMPANY_DATA_DIR` | `~/.opencompany` | Bundle root for fs stores |
| `OPENCOMPANY_BRAIN_MODE` | `hosted` | `hosted` \| `sidecar` (overrides `[brain].mode`) |
| `OPENCOMPANY_OPENHUMAN_URL` | — | Attach to a running `openhuman-core serve` instead of launching |
| `TINYPLACE_API_URL` | `https://api.tiny.place` | tiny.place base (staging/local override) |
| `GITHUB_TOKEN` | — | Only for the feedback→issue flow; without it, feedback is stored locally and a prefilled "file it yourself" link is shown |

## Optional capabilities and their degradation

| Capability | Needs | Without it |
| --- | --- | --- |
| Cycles (the brain) | TinyHumans credential | build/inspect only |
| Tools/channels beyond built-ins | OpenHuman reachable | built-in tools; non-operator channels warn and disable |
| tiny.place presence | `tinyplace` feature + funded wallet for the paid handle claim | company runs privately; going-public prompts for funding |
| Feedback auto-filing | `GITHUB_TOKEN` + consent | local capture + manual prefilled link |
| SQLite / TinyCortex stores | respective features | fs bundle |

tiny.place deliberately needs **no key**: identity is a locally generated
Ed25519 keypair in the company bundle. Paid actions (the handle claim) wait
until the wallet is funded, with a clear operator prompt. Whether TinyHumans
sponsors handle claims via a delegated signer bundled with the account is an
open product question ([company-as-agent/identity.md](../company-as-agent/identity.md)).

## Secrets handling

The TinyHumans credential and all per-company secrets live in the
`SecretStore` (fs default: encrypted at rest, `0600`). Secrets MUST never
appear in logs, cycle traces, exports (bundles exclude `secrets/` unless
`--include-secrets`), or feedback issues
([feedback-loop/privacy.md](../feedback-loop/privacy.md)).
