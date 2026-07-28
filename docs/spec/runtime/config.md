# Configuration

## The one-key promise

`TINYHUMANS_API_KEY` is the **only required secret**. It authenticates the
runtime to the TinyHumans backend (api.tinyhumans.ai) and from it derive:

- the hosted Medulla brain (the `/orchestration/v1` surface —
  [integrations/medulla.md](../integrations/medulla.md)),
- access to the model catalog for TinyAgents-backed fallbacks (tiers map to
  SKUs server-side; the runtime never names models),
- observability: TinyAgents' Langfuse exporter can proxy traces through the
  backend's telemetry ingestion using the same credential,
- feedback forwarding: an instance holding a credential sends operator reports
  to the backend hub, recorded on behalf of the credential's **owner**, instead
  of filing its own GitHub issues
  ([feedback-loop](../feedback-loop/README.md)).

**Credential reality vs contract.** Today the backend authenticates
`/orchestration/v1` with a session JWT (magic-link / OAuth / login-token
exchange); a literal API key does not exist yet. The config slot is therefore
an opaque *TinyHumans credential*: the runtime accepts either a session JWT
(now) or an API key (once the backend ships an API-key path for headless
hosts — a tracked upstream workstream, [roadmap.md](../roadmap.md)). The env
var name `TINYHUMANS_API_KEY` is the stable product contract either way.

Because a forwarded report is attributed to whoever the credential resolves to,
the same pass-through works for either credential form: the backend identifies
the owner, and the runtime never needs to know who that is.

Without a credential the runtime still builds, validates manifests, runs
`opencompany check`/`spec`, and serves the inspection routes — matching the
README promise that you can build/inspect/explore keyless. Cycles require the
credential; feedback falls back to the local GitHub/manual-link path.

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
| `OPENCOMPANY_INFERENCE_KEY` | `TINYHUMANS_API_KEY` | Harness-brain credential (`openhuman` feature). Per-tenant override of the platform key |
| `OPENCOMPANY_INFERENCE_URL` | `https://api.tinyhumans.ai/openai/v1` | Harness-brain OpenAI-compatible endpoint (`openhuman` feature) |
| `OPENCOMPANY_INFERENCE_MODEL` | `chat-v1` | Roster-wide default model/tier for the harness brain (`openhuman` feature) |
| `TINYPLACE_API_URL` | `https://api.tiny.place` | tiny.place base (staging/local override) |
| `GITHUB_TOKEN` | — | Only for the feedback→issue flow; without it, feedback is stored locally and a prefilled "file it yourself" link is shown |
| `OPENCOMPANY_MAIL_PROVIDER` | `smtp` when any `OPENCOMPANY_MAIL_*` is set | Host-level outbound mail transport. Supported: `smtp` |
| `OPENCOMPANY_MAIL_HOST` | — | SMTP submission host. Setting it opts the host into platform mail |
| `OPENCOMPANY_MAIL_FROM_EMAIL` | — (required with `_HOST`) | Envelope `From` for platform mail |
| `OPENCOMPANY_MAIL_PORT` | `587` | Submission port |
| `OPENCOMPANY_MAIL_SECURITY` | `starttls` | `none` \| `starttls` \| `ssl` |
| `OPENCOMPANY_MAIL_USERNAME` / `_PASSWORD` | — | SMTP auth. Redacted from `Debug` and never logged |
| `OPENCOMPANY_MAIL_FROM_NAME` | — | Display name on the `From` header |
| `OPENCOMPANY_CORS_ORIGINS` | — (CORS off) | Comma-separated exact origins allowed to send the session cookie cross-origin, e.g. `http://localhost:5173`. `*` is refused: a wildcard is illegal with credentials |

### Outbound mail

Two credential scopes, deliberately separate:

- **Host-level** (`OPENCOMPANY_MAIL_*`, above): the *platform's* mail identity,
  used for mail sent on the platform's behalf — login links most of all. One
  provider per host.
- **Per-company** (the company's `SecretStore`, written by `PUT …/smtp`): a
  company's *own* outbound identity, used by the test send and per-teammate
  mail. A tenant never receives the host-level credential.

Both go through the same provider-agnostic `MailSender` seam
(`src/server/ops/mailer.rs`). Credentials are a provider-tagged enum, so adding
a transport is a variant plus a sender behind its own feature — the default
build still links no network crates. A **partial** `OPENCOMPANY_MAIL_*`
configuration fails the boot rather than silently disabling mail.

**AWS SES** needs no separate provider: point `OPENCOMPANY_MAIL_HOST` at
`email-smtp.<region>.amazonaws.com` with SES SMTP credentials. A native SES API
transport is only worth adding for what the SMTP interface cannot express
(configuration sets, per-message tags, richer send errors).

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

## Authentication

There are exactly two principals, and **no unauthenticated path**:

| Principal | Credential | Reaches |
| --- | --- | --- |
| **Platform** | a platform/tenant bearer, when `platform_auth` is configured | the companies its tenant owns; provisioning and suspension need the `platform` scope |
| **User** | a human's session cookie ([users.md](users.md)) | their own company only |

`/healthz` is the sole exception — the manager's wake-on-request proxy blocks
on it, so it must answer before anyone could authenticate.

Without `platform_auth`, there is no machine credential at all: humans are the
whole story, and HTTP provisioning is unavailable by construction (load
companies with `serve --company <dir>`). A company with no admin in its
manifest's `[users]` cannot be reached until one is listed — that is the
bootstrap, and it is deliberate.

### What changed, and why it mattered

There used to be a third principal — `Dev`: with no `operator_token` set, every
operator route allowed **every** request. And `operator_token` was **dead
configuration**: no env var, flag, or config key set it, and
`bin/opencompany.rs` never populated it. Only tests did. So `Dev` was the only
reachable state, and every deployment served chat, tasks, secrets, and
provisioning to anyone who could reach the port.

The token is gone rather than made settable, and the routes now require a real
principal. `?token=` does nothing.

### Network exposure

This is no longer the only thing isolating a company, but it still matters:

- **Hosted mode**: the manager injects `OPENCOMPANY_BIND=0.0.0.0:8080`. Binding
  `0.0.0.0` is *mandatory* in a container — it must accept traffic from its
  network — so the bind address is not evidence of exposure; port publishing
  is. The container is additionally reachable only through the manager's proxy.
- **Self-hosting**: bind loopback, or put TLS in front. `Secure` is set on the
  session cookie whenever `public_url` is https, and a login code is never
  echoed in a response unless the bind is loopback-only.

## Secrets handling

The TinyHumans credential and all per-company secrets live in the
`SecretStore` (fs default: encrypted at rest, `0600`). Secrets MUST never
appear in logs, cycle traces, exports (bundles exclude `secrets/` unless
`--include-secrets`), or feedback issues
([feedback-loop/privacy.md](../feedback-loop/privacy.md)).
