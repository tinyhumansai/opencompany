# HTTP API

The Axum surface the runtime exposes. Existing routes (`GET /healthz`, `GET
/spec`, `GET /tiny`) are kept unchanged. Routes are grouped by audience;
handlers live as focused groups under `src/server/`, never in the binary.

## Operator API

Auth: a human's session cookie ([users.md](users.md)), or a platform-issued
token in platform mode (see below). There is no unauthenticated path and no
operator token — see [config.md](config.md#authentication).

Provisioning and suspension require the `platform` scope, which no session can
ever hold.

```text
GET    /api/v1/companies                       list running companies
POST   /api/v1/companies                       boot from an uploaded manifest (platform)
GET    /api/v1/companies/{id}                  status: charter, roster, budget burn,
                                               lifecycle state, tiny.place state
POST   /api/v1/companies/{id}/chat             operator message → event; SSE reply stream
GET    /api/v1/companies/{id}/events?since=SEQ SSE stream of events/effects (work feed)
GET    /api/v1/companies/{id}/approvals        pending approvals
POST   /api/v1/companies/{id}/approvals/{aid}  { "verdict": "approve"|"deny", "note": "…" }
POST   /api/v1/companies/{id}/feedback         file feedback (see feedback-loop/)
GET    /api/v1/companies/{id}/memory/traces    inspect working memory (debug)
POST   /api/v1/companies/{id}/export           export bundle (tar)
POST   /api/v1/companies/{id}/pause            pause / resume lifecycle transitions
```

Single-company (prosumer) mode aliases everything under `/api/v1/company/...`
with no `{id}`.

## Console write plane (`src/server/ops/`)

The console's writes are a REST router family under `src/server/ops/`, each
route registered under **both** scope forms (`…/companies/{id}/…` and the
`…/company/…` prosumer alias) by the `scoped` helper. All reads for these
surfaces go through GraphQL (below); these are the mutations only. Anything a
build doesn't serve `404`s — the console treats that as "not wired yet".

```text
POST   …/tasks                              create a task card
PATCH  …/tasks/{taskId}                      edit / move a task
DELETE …/tasks/{taskId}                      delete a task
POST   …/memory                             add a memory fact
DELETE …/memory/{factId}                     delete a memory fact
POST   …/workspace                          create a folder/file (or upload)
PUT    …/workspace/file/{nodeId}             write file content
PATCH  …/workspace/{nodeId}                  rename / move
DELETE …/workspace/{nodeId}                  delete a node
POST   …/skills                             add a custom skill
POST   …/skills/{slug}/install              install a registry/company skill
POST   …/skills/{slug}/uninstall            uninstall a skill
PUT    …/skills/{slug}                       enable / disable a skill
POST   …/team                               add an operator-overlay teammate
DELETE …/team/{agentId}                      remove an overlay teammate
PUT    …/team/{agentId}/inbox                toggle a teammate's inbox
POST   …/inboxes/{key}/read                  mark inbox messages read
POST   …/inboxes/ingest                     HMAC-signed inbound email → inbox
```

Team writes are an **operator overlay** persisted through the store, merged
into the manifest roster at read time — the version-controlled `company.toml`
is never rewritten. In v1 overlay teammates are **roster-only**: they appear in
the roster and get an inbox, but no harness `Agent` is built for them yet.

### Credential-bearing surfaces (feature-gated)

These write secrets to the `SecretStore` and expose only non-secret status.
The networked half of each (DNS lookup, SMTP send, OAuth token exchange) is
dependency-inverted behind a trait; when the relevant seam is absent the write
route `404`s with `{"code":"not_wired"}`.

```text
PUT    …/domain                             set the custom domain
POST   …/domain/verify                       server-side DNS check
PUT    …/smtp                               store SMTP credentials (secret store)
POST   …/smtp/test                           send a test email
POST   …/connections/{provider}/start        begin OAuth (returns authorize URL)   [feature: oauth]
POST   …/connections/{provider}/disconnect   drop stored OAuth tokens               [feature: oauth]
GET    /api/v1/oauth/callback                OAuth redirect target (unscoped; state carries the company)  [feature: oauth]
```

## Read plane — GraphQL (`/graphql`)

Every console **read** is served by a single async-graphql query surface at
`POST /graphql` (with a `GET /graphql` GraphiQL explorer in development). The
schema is query-only — REST owns writes — and is **built once at startup** and
stored on `AppState`; each request injects its resolved `GqlAuth` principal.

The schema is rooted at a **`Company` aggregation object** so a view fetches
everything it needs in one round trip; the only top-level queries are
`companies`, `company(id)` (the sole company when `id` is omitted in
single-company mode), and `skillRegistry` (the unscoped shared library). Under
`Company` hang `team`, `chats`/`chat(id)`, `inboxes`, `tasks`, `skills`,
`workspaceTree`/`workspaceFile(id)`, `memory`, `workflows`/`workflow(id)`,
`usage`, `finances`, `connections`, `domain`, and `smtp`. The authoritative
contract is the SDL snapshot at
[`src/server/graphql/schema.graphql`](../../../src/server/graphql/schema.graphql)
(`graphql::sdl()` regenerates it). Mutations and subscriptions are out of
scope; SSE (`/chat` streaming, the `/events` work feed) is not yet wired.

- **`/chat`** enqueues an `OperatorMessage` event and streams the resulting
  cycle's channel responses over SSE. One conversational surface, one voice:
  the operator talks to the company, not to individual teammates.
- **`/events`** is the work feed's backend: each frame is a plain-language
  rendering of an event or executed effect plus the raw payload for
  programmatic consumers. Resumable via `since` (event sequence number).

## Agent-facing (tiny.place-compatible)

Enabled per company by `[place].discoverable`; served only with the
`tinyplace` feature.

```text
POST   /a2a/{handle}                        A2A JSON-RPC (tasks/send …), SIWX-verified
GET    /a2a/{handle}/skill.md               capability discovery doc
GET    /.well-known/agent-card.json         single-company mode
GET    /companies/{handle}/.well-known/agent-card.json   platform mode
```

- Inbound requests carry tiny.place per-action signatures
  (`Authorization: tiny.place <agentId>:<signature>:<timestamp>`); the
  runtime verifies via the `tinyplace` SDK before anything reaches the brain.
- **x402-priced skills**: if the requested skill has a price on the Agent
  Card, the route responds `402 Payment Required` with the x402 challenge;
  on resubmission the payment is verified through
  `AgentEconomy`/the facilitator, receipted to the ledger, and the task
  enters the event queue as `A2aTaskReceived`.
- Untrusted counterparty text is prompt-guard sanitized before it reaches the
  brain (mirroring tiny.place's own promptguard practice).

## Inbound integrations

```text
POST   /hooks/{companyId}/{channel}         webhooks → CompanyEvent
```

HMAC-verified per channel secret from the `SecretStore`; unverifiable
payloads are dropped with a 401 and never become events.

## Auth model

| Caller | Mechanism |
| --- | --- |
| Prosumer operator (local) | Operator token minted at first run, stored in the OS keychain / config dir; the desktop UI holds it. |
| Platform | Platform-issued JWT per tenant; `POST /api/v1/companies` and suspend/archive require a platform-scope claim. |
| Peer agents (A2A) | tiny.place SIWX signatures + optional x402 payment; no accounts. |
| Webhook senders | Per-channel HMAC secrets. |

The runtime's own upstream credential (`TINYHUMANS_API_KEY` / JWT) is never
accepted inbound; it is outbound-only ([config.md](config.md)).

## Errors

JSON error envelope `{ "error": string, "code": string }` with stable `code`
values; 4xx for caller mistakes, 402 reserved for x402 challenges, 409 for
lifecycle-state conflicts (e.g. chatting with an archived company).

## Platform webhooks (Phase 5)

Platform mode can register outbound webhooks per tenant for
`approval.requested`, `work.completed`, `feedback.created`, and
`budget.exhausted` so hosts can build their own surfaces without polling
SSE. Delivery is at-least-once with signature headers; see
[product/platform.md](../product/platform.md) for the requirements source.
