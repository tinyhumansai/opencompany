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
POST   /api/v1/companies/{id}/approvals/{aid}  { "verdict": "approve"|"deny", "note": "…",
                                               "detach": false }
POST   /api/v1/companies/{id}/feedback         submit feedback (see feedback-loop/)
GET    /api/v1/companies/{id}/feedback         past reports (no operator words)
GET    /api/v1/companies/{id}/memory/traces    inspect working memory (debug)
POST   /api/v1/companies/{id}/export           export bundle (tar)
POST   /api/v1/companies/{id}/pause            pause / resume lifecycle transitions
```

Single-company (prosumer) mode aliases everything under `/api/v1/company/...`
with no `{id}`.

`detach` on the approval resolve chooses what the response waits for. Omitted
(or `false`) it holds the response open for the agent's follow-up turn and
answers with that cycle's messages — the long-standing contract, unchanged.
Set, it answers `200 { "recorded": true, "alreadyResolved": bool }` as soon as
the verdict is durable and the grant minted, and the continuation arrives on the
`agent_reply` event-stream frame instead. `alreadyResolved` is a success: a
second resolve of the same approval is an idempotent no-op that mints no second
grant.

Either way the resolve survives a dropped connection — the follow-up cycle runs
on its own task, so it is no longer cancelled when a client or a reverse proxy
gives up mid-turn. `detach` removes the *wait*; it is not what provides the
drop-safety. See
[company-brain/approvals.md](../company-brain/approvals.md#settling-the-verdict-is-not-running-the-follow-up).

### Running and stopping a workflow (issue #383)

```text
POST   …/workflows/{wid}/run                 { "input": {…}, "detach": false }
POST   …/workflows/runs/{runId}/cancel       stop a run that is still walking its graph
```

`detach` is the same idea as the approval one and reads the same way. Omitted
(or `false`) the response is the settled run — `{ output, pendingApprovals,
deliveries, runId }`, byte-unchanged, plus `cancelled: true` **only** when the
run was stopped while the request was still open. A synchronous run is
cancellable like any other: its id is registered before the first node runs and
the console learns it from the `workflow_run_started` frame, so a cancel can
land mid-request. Without that flag the resulting `output: null` with no
approvals and no deliveries would be indistinguishable from a run that
legitimately produced nothing. Set, the host answers **`202 Accepted`**
with `{ "runId": "…", "detached": true }` before the engine walks a node; the
run is then followed through the `workflow_run_started` / `workflow_node_finished`
/ `workflow_run_finished` frames it already keys by that `runId`, and read back
from `GET …/workflows/runs`, whose fold reports `running: true` until it settles.

**Clients must discriminate on the response shape, not on what they sent.** A
host predating this ignores the unknown `detach` field and answers the full
synchronous `200`, so `output` present means "already settled" and `detached`
present means "watch the stream". Both directions are compatible: an older
client never sends the field and is unaffected.

Either way the run survives a dropped connection. It runs on its own task, so a
closed tab or a proxy giving up no longer cancels it mid-graph — before this it
did, and because a run journals a start first, the abandoned run then folded as
`running: true` until the next host restart swept it.

`…/runs/{runId}/cancel` answers `200 { "cancelling": true }` when the run is
live and `404` when the run is unknown **or has already settled** — one answer,
because they mean the same thing to the caller: there is nothing to stop. It is
behind the same `ScopedCompany` guard as every other route here, so any operator
of the company may stop any of its runs.

`cancelling`, not `cancelled`: the route fires a signal and returns. The run
settles a moment later with a `WorkflowRunFinished` carrying `cancelled: true`
and **no error** — a stop somebody asked for is not a failure, and a reader that
only checks `error` would render it as a clean success.

**Stopping is not finishing.** The executing node is dropped mid-await rather
than allowed to complete, so an external side effect it had started may be
half-done — the same class of outcome as the host being killed, only
operator-initiated. Nodes that already completed keep their journal rows, and
approvals earlier nodes parked stay valid in the queue: they are journal-backed
and independent of the run, so they can still be approved or denied afterwards.
No minted grant is revoked. See
[events.md](events.md#stopping-a-run-issue-383).

## Console write plane (`src/server/ops/`)

The console's writes are a REST router family under `src/server/ops/`, each
route registered under **both** scope forms (`…/companies/{id}/…` and the
`…/company/…` prosumer alias) by the `scoped` helper. These are the mutations,
plus **two deliberate read exceptions**: the two inbox `GET`s at the end of the
block below, and the two workspace `GET`s (#177). Every other console read goes
through GraphQL (see the read plane below). Anything a build doesn't serve
`404`s — the console treats that as "not wired yet".

```text
POST   …/tasks                              create a task card (`originChatId` records the thread it came from, #246)
PATCH  …/tasks/{taskId}                      edit / move a task
DELETE …/tasks/{taskId}                      delete a task
GET    …/tasks/{taskId}/export               the task's record as a readable HTML document (#352)
POST   …/tasks/{taskId}/discussion           post a message to the card's thread (#335)
POST   …/memory                             add a memory fact
DELETE …/memory/{factId}                     delete a memory fact
GET    …/workspace                          the whole tree (metadata; no bodies)
GET    …/workspace/file/{nodeId}             one file: content + inbound backlinks
POST   …/workspace                          create a folder/file (or upload)
PUT    …/workspace/file/{nodeId}             write file content
PATCH  …/workspace/{nodeId}                  rename / move
DELETE …/workspace/{nodeId}                  delete a node
POST   …/skills                             add a custom skill
GET    …/skills/registry                     browse the shared skill library
POST   …/skills/{slug}/install              install a registry/company skill
POST   …/skills/{slug}/uninstall            uninstall a skill
PUT    …/skills/{slug}                       enable / disable a skill
POST   …/team                               add an operator-overlay teammate
DELETE …/team/{agentId}                      remove an overlay teammate
PUT    …/team/{agentId}/inbox                toggle a teammate's inbox
PUT    …/team/{agentId}/budget               set / change / remove a daily cap
DELETE …/team/{agentId}/budget               reset the cap to the manifest default
POST   …/inboxes/{key}/read                  mark inbox messages read
POST   …/inboxes/ingest                     HMAC-signed inbound email → inbox
GET    …/inboxes                            list inboxes + unread counts
GET    …/inboxes/{key}/messages              one teammate's mail (store order)
```

The two inbox `GET`s are the read exception, and they are **REST twins of the
`Company.inboxes` GraphQL resolver**: the operator console ships no GraphQL
client, so without them the Inbox view had no reachable per-agent read at all
and fell back to a client-side fixture (issue #173). They read the same
`InboxStore` both inbound paths — the ingest webhook and the IMAP poller — file
into, and `GET …/team` tags each teammate with `inboxEnabled` so the Team toggle
reflects that store too. Messages come back in append order; the console sorts
them newest-first. The GraphQL resolver stays the canonical read for any client
that does speak GraphQL — these routes duplicate it, they do not replace it.

The two workspace `GET`s are the same story one issue later (#177): the console
had no reachable workspace read either, so the Workspace tab persisted to
`localStorage` and the operator and the agents looked at two different trees —
a note written by an agent through its `workspace_*` tools (#237) was invisible
to the operator, and vice versa. They are REST twins of `Company.workspaceTree`
/ `workspaceFile`, differing only in timestamp shape (epoch millis, matching
every other console read, rather than ISO-8601 strings). The backlink scan is
literally shared code (`company::workspace_links`), so the two surfaces cannot
report different backlinks for the same note. The tree read carries metadata
only — bodies are fetched per file, so a navigation read does not grow with the
size of the workspace. Reading a folder id as a file is a `404`, never an empty
note.

Team writes are an **operator overlay** persisted through the store, merged
into the manifest roster at read time — the version-controlled `company.toml`
is never rewritten. In v1 overlay teammates are **roster-only**: they appear in
the roster and get an inbox, but no harness `Agent` is built for them yet.

The two **budget** routes (issue #343) are how a teammate's `budget_usd_daily`
becomes changeable without a redeploy. Both are **admin-only** — a member gets
`403` and an unauthenticated caller `401` — and both stamp who set the cap and
when, surfaced as `budgetSetBy` / `budgetSetAtMillis` on the roster row. A
stored cap wins over the manifest, and the change is enforced on the teammate's
**next dispatch**: the harness fingerprints the override set alongside its other
freshness axes, so the roster is rebuilt before the next turn rather than at the
next process start.

`PUT` takes `{"budgetUsdDaily": <number|null>}` and the three cases stay apart
on the wire, which is the point of the route:

| body | effect |
|---|---|
| `{"budgetUsdDaily": 5}` | cap at $5/day |
| `{"budgetUsdDaily": 0}` | cap at nothing — a real cap, not "uncapped" |
| `{"budgetUsdDaily": null}` | remove the cap, beating a manifest cap |
| `{}` | **`422`** — an omitted key is never read as "remove the cap" |

A negative or non-finite amount is `400`; an unknown teammate is `404`.
`DELETE` drops the override so the manifest default applies again — distinct
from `PUT null`, and not expressible by it. `POST …/team` also accepts an
optional `budgetUsdDaily`, so a console-created teammate can be given a cap at
creation; only that form of the add requires an admin.

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

### The OAuth callback always redirects

`/api/v1/oauth/callback` is reached by a **browser navigation**, so anything it
returns as a body becomes the page the operator is left on. It therefore never
answers with JSON. Every outcome redirects to the console's Connections view:

- success → `…/connections?connected=<provider>`
- failure → `…/connections?connect_error=<code>[&provider=<provider>]`

`<code>` is one of a closed set — `denied`, `invalid_request`, `invalid_state`,
`unknown_company`, `provider_disabled`, `exchange_failed`, `store_failed` — that
the console maps to operator-facing copy. The provider's own error text is
logged host-side but never forwarded: it is attacker-influenced and must not
ride in a URL that lands in browser history and access logs. `provider` is
appended only when a signature-verified `state` supplies it, so the arms that
fire before verification omit it.

### Provider catalog vs. configured providers

The console's Connections view offers 11 provider tiles; `well_known()` in
`server::ops::connections` carries built-in authorize/token URLs for three
families only (`slack`, `google`/`gmail`, `github`). Every other tile needs
`OPENCOMPANY_OAUTH_<P>_AUTHORIZE_URL` / `_TOKEN_URL` alongside its `_ID` /
`_SECRET`, or it is simply not enabled on that host.

This gap is **known and safe**: an unconfigured tile fails at `start` with a
`400 provider '<p>' is not enabled on this host`, the console shows a toast, and
the browser never navigates — so there is no broken redirect to come back from.
Closing the gap (shipping more well-known URLs, or hiding unconfigured tiles) is
separate work.

## Read plane — GraphQL (`/graphql`)

Every console **read** is served by a single async-graphql query surface at
`POST /graphql` (with a `GET /graphql` GraphiQL explorer in development) — the
sole exceptions being the two inbox `GET`s and the two workspace `GET`s above,
which exist because the console ships no GraphQL client and those two views need
a reachable read (issues #173 and #177 respectively). The schema is
query-only — REST otherwise owns writes — and is **built once at startup** and
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
- **`/chat` thread addressing is a load-bearing contract, not just routing.**
  The body's `chat` field names a desk; three behaviours follow from it, and
  the console's per-workflow copilot (issue #303) is built entirely on them,
  with no route of its own:
  1. An **unknown** thread id falls through to the orchestrator — the brain
     tries desk-lead, then roster agent, then its own responder.
  2. Replies are journaled against that thread, and the desk filter
     (`server::chat_history::owns`) matches the id **exactly**; the General
     catch-all applies only when General is the desk being *read*. So an
     addressed thread is isolated from the team's chat in both directions.
  3. `GET /chat/history?desk=<thread>` therefore replays exactly that thread.

  The copilot addresses `workflow-copilot:<workflowId>` (a `:` cannot occur in
  a manifest desk id, so it can never collide with a real desk, and it does not
  appear in `GET …/desks`). Making unknown thread ids a `404`, or loosening
  `owns` to match on prefix, would break that surface — see
  [`frontend/src/api/workflow-copilot.ts`](../../../frontend/src/api/workflow-copilot.ts).

  **Thread addressing isolates transcripts. It does not scope authority.**
  These are two different things and only the first is enforced here. The
  thread id decides who answers and where the exchange is journaled; it does
  **not** narrow the orchestrator's context or its tool grants, which stay
  company-wide for every `/chat` turn whatever thread it names. A caller that
  needs the responder confined to one subject has to constrain it in the
  prompt — which is advisory — or build a genuinely scoped agent, which this
  seam is not.

  That is a scoping property, not an authorization one: `/chat` is already
  authenticated and company-scoped, so an operator addressing a workflow
  thread gains nothing they could not get by opening the Chat tab or calling
  the workflow routes directly. The copilot therefore adds no privilege; what
  it adds is a transcript that stays out of the team's chat.

  Two more consequences worth knowing before reusing the seam. A chat turn
  runs the **whole** company cycle, so an actionable message also opens a
  board card via `company::task_intent`. And an unconfigured company answers
  `200` with the echo brain's `"You said: …"` rather than an error, so a caller
  that needs a real answer must check `cognition` from `GET {scope}/inference`
  — there is no status code to catch.
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
