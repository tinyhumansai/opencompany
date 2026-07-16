# Server Module

The server module owns the Axum HTTP surface. The base routes are:

- `GET /healthz`
- `GET /spec`
- `GET /tiny`

Operator chat and approvals live under `/api/v1/...` (see `server::operator`),
and feedback under `server::feedback`. Add future API routes as focused handler
groups rather than wiring behavior directly in the binary entrypoint.

## Read plane — `server::graphql`

The console's reads are one async-graphql query surface (`POST /graphql`, plus
a `GET /graphql` GraphiQL explorer). The schema is **built once at startup**
(`build_schema`) and stored on `AppState`; each request injects its resolved
`GqlAuth` principal via request data. It is query-only — REST owns writes.

The module is split one file per surface: `mod.rs` (the `Company`-rooted
`QueryRoot` — `companies`, `company(id)`, `skillRegistry`), `auth.rs`
(`GqlAuth`, claim resolution + `visible_companies`), `company.rs` (the
aggregation object every view fetches through), `pagination.rs`, and one
resolver file per view (`tasks`, `workspace`, `memory_facts`, `skills`,
`inbox`, `workflows`, `usage`, `finances`, `connections`). `schema.graphql` is
the checked-in SDL snapshot (the read contract); `graphql::sdl()` regenerates
it and a snapshot test guards drift.

## Write plane — `server::ops`

Console writes are the `server::ops` router family. Each route is registered
under **both** scope forms — `…/companies/{id}/…` and the `…/company/…`
prosumer alias — by the `scoped` helper; the `ScopedCompany` extractor resolves
the target runtime and enforces authorization per form (platform-or-operator +
address check for `{id}`, operator + `sole()` for the alias).

| Surface (`ops::*`) | Routes |
|---|---|
| `tasks` | `POST …/tasks`, `PATCH`/`DELETE …/tasks/{id}` |
| `memory` | `POST …/memory`, `DELETE …/memory/{id}` (journals `MemoryFactDeleted`) |
| `workspace` | `POST …/workspace`, `PUT …/workspace/file/{id}`, `PATCH`/`DELETE …/workspace/{id}` |
| `skills` | `POST …/skills`, `POST …/skills/{slug}/install\|uninstall`, `PUT …/skills/{slug}` |
| `team` | `POST …/team`, `DELETE …/team/{id}`, `PUT …/team/{id}/inbox` (overlay; roster-only in v1) |
| `mail` | `POST …/inboxes/{key}/read` |
| `inbox` | `POST …/inboxes/ingest` (HMAC-signed inbound email) |
| `domain` | `PUT …/domain`, `POST …/domain/verify` |
| `smtp` | `PUT …/smtp`, `POST …/smtp/test` |
| `connections` (feature `oauth`) | `POST …/connections/{provider}/start\|disconnect`, `GET /api/v1/oauth/callback` |

Every credential-shaped value written here lands in the `SecretStore`; the
responses expose only non-secret status. The networked seams (DNS, SMTP, OAuth
exchange) are dependency-inverted behind traits carried on `ConnectionsRuntime`
and default to empty (offline) — a surface whose seam is absent returns
`404 {"code":"not_wired"}`, which the console degrades gracefully.

## tiny.place A2A inbound + discovery (`tinyplace` feature)

Behind the `tinyplace` feature the server mounts the agent-to-agent surface
(`server::a2a`). With the feature off, none of these routes exist and the
default build links no crypto.

| Route | Purpose |
| --- | --- |
| `POST /a2a/{handle}` | JSON-RPC `tasks/send` from a counterparty agent |
| `GET  /a2a/{handle}` | the company's Agent Card (directory record) |
| `GET  /a2a/{handle}/skill.md` | human/agent-readable priced-skill catalog |
| `GET  /.well-known/agent-card.json` | the sole company's card (prosumer) |
| `GET  /companies/{handle}/.well-known/agent-card.json` | a named company's card |

`POST /a2a/{handle}` enforces the trust boundary in a fixed order before any
work reaches cognition:

1. Resolve a **discoverable** company (`[place].discoverable = true` with a
   matching `[company].handle`); a miss is `404`.
2. Verify the SIWX `Authorization` header (skew window + single-use replay
   protection via a host-global nonce cache). A bad/missing header is `401`.
3. For a skill priced above `0.00`, require a valid x402 authorization; without
   one the response is a `402` challenge naming the amount and the company's
   own tiny.place address.
4. Sanitize the counterparty payload (a minimal promptguard pass — control
   characters are stripped) before it becomes an `A2aTaskReceived` event and
   drives exactly one cycle. Paying customers run under the same approval gates
   as any other stimulus.

An unreachable tiny.place backend maps to `503`; any other transport failure is
`502`.

## Enable discovery for all companies

Every company declares its own discoverability in its manifest:

```toml
[company]
name = "Acme SEO"
handle = "acme"

[place]
discoverable = true
skills = [{ id = "seo.audit", price_usd = "25.00", description = "Full audit" }]
```

To opt **every** loaded company into going public regardless of its manifest,
pass `serve --discoverable`. It marks each company discoverable and synthesizes
a `@handle` (a slug of the company name) when one is missing, so Agent Card
generation and validation succeed:

```bash
cargo run --features tinyplace --bin opencompany -- \
  serve --discoverable \
  --company companies/agentic_law_firm \
  --company companies/agentic_marketing_agency
```

At boot each discoverable company runs the going-public flow (lifecycle step 3):
load-or-generate the Ed25519 keypair, `ensure_registered`, then publish the
Agent Card — all best-effort. An unreachable tiny.place degrades the company to
"private" with a warning and never blocks or fails boot.

Relevant configuration:

- `TINYPLACE_API_URL` — tiny.place economy base URL (default
  `https://api.tiny.place`).
- `OPENCOMPANY_PUBLIC_URL` — public host base embedded in published Agent Card
  endpoints. When unset, the endpoint falls back to `http://{bind}`.
