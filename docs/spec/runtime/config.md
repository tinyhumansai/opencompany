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

## Where the credential comes from

The runtime does not hold a credential — it *obtains* one, per request, from a
two-tier source (`company::credentials`). Highest precedence first:

| Tier | Env | Who sets it | Shape |
| --- | --- | --- | --- |
| projected file | `TINYHUMANS_TOKEN_FILE` | the hosting platform | a short-lived, audience-bound token in a file the platform **rewrites in place** (600-second expiry, so roughly every 8 minutes) |
| static | `TINYHUMANS_API_KEY` | you | a long-lived key held for the life of the process |

A hosted tenant gets the projected file and stores no secret at all: the platform
mounts it read-only at `/var/run/secrets/tinyhumans.ai/token` and the env var
carries that **path** — never a token value. The tier is selected only when the
path exists, so a leftover variable under a runtime that mounts nothing (docker)
falls through to the static tier instead of failing every request. The file is
re-read as it rotates: a read is cached for 80% of the token's remaining TTL,
capped at 60 seconds, and a token whose `exp` cannot be read (or has already
passed) is not cached at all. A `401` from the backend drops the cached read, so
the next request goes straight back to the file. Expiry is parsed out of the
JWT **without verifying the signature** — the runtime needs the date, and the
backend is the party that verifies the token.

The static tier is what `docker compose` uses, and it is the only credential path
available if you run this repo standalone. Standalone/self-hosted operation is
**not supported** this milestone: it is an escape hatch, not a deployment mode.

`opencompany doctor` prints the active tier as `credential_source`
(`attested` / `static` / `none`) alongside the token-file path. Neither the
report nor any API response ever carries the token.

## Precedence

```text
env (OPENCOMPANY_*, TINYHUMANS_TOKEN_FILE, TINYHUMANS_API_KEY)
  ⟵ ~/.opencompany/config.toml
  ⟵ company manifest
  ⟵ built-in defaults
```

Earlier layers win. `opencompany doctor` prints every effective value, which
layer set it, and what is missing for each optional capability.

This is also why the [first-run setup flow](setup.md) reports a layer with every
field it offers. That flow writes `config.toml` — the *second* layer — so on a
host where the platform injects `OPENCOMPANY_*`, an edit to one of those keys
would be saved and then outranked at the next boot. It renders such a field
read-only and refuses the write, rather than reporting a success that changes
nothing.

### The bind address

`serve` takes a `--bind` flag, so its listener address has one extra layer on
top of the chain above:

```text
--bind
  ⟵ OPENCOMPANY_BIND
  ⟵ ~/.opencompany/config.toml  (bind = "…")
  ⟵ 127.0.0.1:8080
```

A blank `OPENCOMPANY_BIND` counts as unset and falls through, as everywhere
else in the chain. `serve` prints the address and the layer that chose it on
startup — `listening on 0.0.0.0:8080 (from OPENCOMPANY_BIND)` — so a
configured address that disagrees with the one in use is visible immediately
rather than only when something fails to reach the host. An address that
cannot be bound aborts boot naming that address; there is no silent fallback
to the default.

The default is loopback. A wildcard bind is reached only by an explicit flag,
variable, or config entry — see [network exposure](#network-exposure) for what
that does and does not imply.

## Reference

| Variable | Default | Purpose |
| --- | --- | --- |
| `TINYHUMANS_TOKEN_FILE` | — | Platform-projected, audience-bound token file; rotates in place and outranks `TINYHUMANS_API_KEY` |
| `TINYHUMANS_API_KEY` | — (required for cycles when no token file) | Static TinyHumans credential (JWT or API key) |
| `TINYHUMANS_API_URL` | `https://api.tinyhumans.ai` | Backend base URL |
| `OPENCOMPANY_BIND` | `127.0.0.1:8080` | HTTP bind address. Outranked by `serve --bind`, outranks `config.toml`'s `bind` — see [the bind address](#the-bind-address) |
| `OPENCOMPANY_DATA_DIR` | `~/.opencompany` (workspace and bundle home alike; bundles at `companies/<slug>`) | The instance data root: both the workspace layout and the company-bundle home. `--home` outranks it for the bundle home **only** — the workspace (`memory/`, `store/`, `files/`, `logs/`, `tmp/`) still resolves under this variable, so `--home` alone does not move a whole instance. The only knob that isolates two hosts from each other — see [the workspace layout](workspace-layout.md#choosing-the-root-srcstorepathsrs) |
| `OPENCOMPANY_BRAIN_MODE` | `hosted` | `hosted` \| `sidecar` (overrides `[brain].mode`) |
| `OPENCOMPANY_AUTH_MODE` | — (each company's `[users].mode`, itself defaulting to `email`) | `email` \| `wallet` \| `none` — how humans sign in, for **every** company this host serves. Set by a platform that must guarantee a mode across tenants (the desktop app does not set it today — see [sign-in modes](auth-modes.md)). An unparseable value aborts boot; `none` on a routable bind is refused wherever a runtime is registered — at boot, at `POST /api/v1/companies`, and by the desktop app's own loader. See [sign-in modes](auth-modes.md) |
| `OPENCOMPANY_OPENHUMAN_URL` | — | Attach to a running `openhuman-core serve` instead of launching |
| `OPENCOMPANY_INFERENCE_KEY` | `TINYHUMANS_API_KEY` | Harness-brain credential (`openhuman` feature). Deploy-time default only — a company key set through the console (`PUT …/inference`) outranks both names |
| `OPENCOMPANY_INFERENCE_URL` | `https://api.tinyhumans.ai/openai/v1` | Harness-brain OpenAI-compatible endpoint (`openhuman` feature) |
| `OPENCOMPANY_INFERENCE_MODEL` | `chat-v1` | Roster-wide default model/tier for the harness brain (`openhuman` feature) |
| `OPENCOMPANY_CONTEXT_WINDOW` | `240000` | Context window the managed inference profile advertises, in tokens (`openhuman` feature). Compression and deterministic trimming engage at 90% of it; set it to a smaller model's advertised window (with an estimation margin) or `off`/`0` to restore unbounded intra-turn history — see [harness history protection](providers.md#history-protection) |
| `TINYPLACE_API_URL` | `https://api.tiny.place` | tiny.place base (staging/local override) |
| `GITHUB_TOKEN` | — | Only for the feedback→issue flow; without it, feedback is stored locally and a prefilled "file it yourself" link is shown |
| `OPENCOMPANY_MAIL_PROVIDER` | `smtp` when any `OPENCOMPANY_MAIL_*` is set | Host-level outbound mail transport. Supported: `smtp` |
| `OPENCOMPANY_MAIL_HOST` | — | SMTP submission host. Setting it opts the host into platform mail |
| `OPENCOMPANY_MAIL_FROM_EMAIL` | — (required with `_HOST`) | Envelope `From` for platform mail |
| `OPENCOMPANY_MAIL_PORT` | `587` | Submission port |
| `OPENCOMPANY_MAIL_SECURITY` | `starttls` | `none` \| `starttls` \| `ssl` |
| `OPENCOMPANY_MAIL_USERNAME` / `_PASSWORD` | — | SMTP auth. Redacted from `Debug` and never logged |
| `OPENCOMPANY_MAIL_FROM_NAME` | — | Display name on the `From` header |
| `OPENCOMPANY_SHUTDOWN_GRACE_SECONDS` | `25` | How long a SIGTERM/SIGINT waits for in-flight turns before exiting anyway. `0` exits as soon as the companies are quiesced. Must stay at least 2s below the pod's `terminationGracePeriodSeconds` (the 2s is the connection-grace overhead at the end of the drain) — see [shutdown](lifecycle.md#shutdown) |
| `OPENCOMPANY_CORS_ORIGINS` | — (CORS off) | Comma-separated exact origins allowed to make credentialed cross-origin requests, e.g. `http://localhost:5173` for a Vite dev server or `https://app.example.com` for a [hub console](hub-console.md). `*` is refused: a wildcard is illegal with credentials |
| `OPENCOMPANY_PLATFORM_TOKEN` | — (no machine credential) | The shared platform secret. A bearer equal to it is the `tenant:platform` principal, with the `platform` scope |
| `OPENCOMPANY_PLATFORM_JWT_SECRET` | — (signed tenant tokens not accepted) | HS256 secret that signs tenant-scoped machine tokens. No shipped literal and no fallback: unset means the path does not exist. Set on a build without `platform-jwt` (in the default feature set) and the host **refuses to boot** |

### The `[memory]` section

The memory engine is the one host-level choice the **console** can write, and
it is the ordinary `env ⟵ config.toml` precedence rather than an exception to
it:

```toml
[memory]
backend = "remote"          # store | embedded | remote | null
driver  = "supermemory"     # supermemory | mem0 | cognee | namespace
url     = "https://api.supermemory.ai"
api_key = "sk-…"
```

`OPENCOMPANY_MEMORY` **set at all** — whatever it names — makes this section
inert and the console read-only, because a hosted tenant's control plane
injects those variables and a picker that wrote a file the next boot ignores
would be the silently-ignored-configuration failure the setup flow refuses for
the same reason. Unset, this section is what boot binds, and
`PUT …/memory/engine` is what writes it — probing the engine first, then
rebinding it live so the choice does not wait for a restart. See
[the memory engine](memory-engine.md#choosing-an-engine-from-the-console).

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

## Default MCP servers

A packaged Open Company can ship a set of MCP tool servers already registered
and **enabled**, so a fresh install has working tools with no user setup
(issue #527). They are declared in this instance's `config.toml` as
`[[default_mcp_server]]` entries:

```toml
# MCP servers every company on this install gets, enabled, with no user action.
#
# PLACEHOLDER — intentionally empty. The shipped list is a product decision
# (issue #527 assigns it to Steven / eng) and is deliberately NOT compiled into
# the binary, so settling it is an edit to this file and a restart, never a
# release. Uncomment and fill in:
#
# [[default_mcp_server]]
# name = "deepwiki"
# endpoint = "https://mcp.deepwiki.com/mcp"
# description = "Documentation and Q&A for public GitHub repositories."
# # optional: allowed_tools / disallowed_tools / timeout_secs / enabled
```

**An empty or absent list means "ship no defaults"** — never "fall back to a
built-in set". There is no compiled-in list to fall back to, which is what makes
the file the single source of truth.

### What may be a default, and what may not

A default is handed to every agent on the install unprompted, so it has to be
safe unattended. Each entry is checked at boot by `normalize_default_servers`;
an entry that fails is **dropped with a logged reason** rather than aborting the
boot, because a typo in a packaged file should not stop an install from
starting. The rules:

| Rule | Why |
| --- | --- |
| `http(s)://` endpoint, no stdio `command` | The hosted-v1 transport boundary — the same `validate_one` check every other declaration path uses |
| No credential in the endpoint (`user:pass@host`, or `?token=` / `?apiKey=` / `?access_token=` — percent-encoded key spellings included) | The entry ships to every company; a secret here is a secret everywhere |
| No `auth_secret` | A default must not depend on a credential. A server needing auth is declared in that company's `company.toml`, or added from the console, where the token is stored per company |
| Unique `name` | Two rows claiming one slug would let merge order decide which wins. The first is kept |

A credential-carrying entry is **refused, not scrubbed**: scrubbing would ship a
server whose auth silently no longer works, which fails at an agent's first tool
call instead of here, where somebody is looking.

### How a default merges with a company's own servers

Three layers, lowest precedence first — **default → manifest → runtime**:

- A company's `[[mcp_server]]` in `company.toml` **shadows** a default of the
  same name: declaring it is saying something specific about it.
- A console edit (the runtime index) overrides either, and its enable/disable
  and tool lists win — but the declaration keeps the **lower** layer's source
  badge. That is how an operator turns a shipped default off: the console writes
  an override, which persists.
- A default therefore **cannot be deleted** from the console, only disabled —
  the same guard a manifest server already has, and for the same reason. Its
  declaration lives in this file, so deleting the row would not remove it; the
  next resolution would merge it straight back.

The console shows which layer a server came from (`default`, `manifest`, or
`runtime`), so a shipped default is never presented as something the operator
added.

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

#### How a platform bearer is authenticated

Exactly two shapes, selected by the two environment variables above:

- **Shared platform secret** (`OPENCOMPANY_PLATFORM_TOKEN`) — an exact match.
  Authenticated by knowledge of the secret, the same pattern the control plane
  uses for its own admin token. Grants the single `tenant:platform` principal.
- **Signed tenant token** (`OPENCOMPANY_PLATFORM_JWT_SECRET`) — HS256 over the
  `tenant` / `scopes` / `companies` claims, `exp` honored when present. This is
  how a tenant-scoped machine token is issued.

Both may be set; a bearer accepted by either is accepted. Boot prints the active
mode (`platform auth: shared-secret | jwt | both`) and never the secret.

No state fails open. No credential means every machine route answers `401`; an
unauthenticated bearer of any shape is `401`; a wrong secret is `401`; and a
signing secret on a build compiled without `platform-jwt` aborts boot with a
message naming the variable and the missing feature rather than degrading to a
weaker check.

Two limits worth naming. The signing is **symmetric**, so a workload that can
verify a token could also mint one — acceptable while the trust boundary is a
single workload; asymmetric keys via the control plane's issuer are the
follow-up. And a signed token carrying **no `exp` never expires**, because the
verifier clears its required-claims set; tightening that is a separate policy
decision.

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
