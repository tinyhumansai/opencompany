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
GET    /api/v1/companies/{id}/chat/history     one desk's transcript (?desk=<thread>)
POST   /api/v1/companies/{id}/chat/messages/{seq}/reactions
                                               { "emoji": "👍", "on": true } → 204
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

**A company bounds how many runs it will execute at once** (issue #401). Every
run — this manual route, a cron fire, an approved gate's continuation, and one
an orchestrator agent starts — counts against `[workflows].max_in_flight_runs`
(default 8; see `manifest.md`). A run over the ceiling is **refused, never
queued**: the route answers **`429 Too Many Requests`** with the standard
`{ "error", "code": "workflow_run_limit" }` envelope and **no `runId`**, because
nothing started. Both `detach` modes refuse identically — the check precedes the
detach/sync branch, so a rejected run journals no `WorkflowRunStarted`. The
message names the three levers: wait for a run to finish, stop one via
`…/workflows/runs/{runId}/cancel`, or raise the manifest cap. A slot frees the
moment a run settles (including on cancel or panic), so a refused run succeeds on
the next attempt once the company is back under its ceiling.

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
[workflow-events.md](workflow-events.md#stopping-a-run-issue-383).

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
GET    …/workspace/search?q=…                which notes mention a phrase (#607)
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
GET    …/team/{agentId}                      one agent in full (tier, tools, desks)
PATCH  …/team/{agentId}                      edit an overlay teammate
DELETE …/team/{agentId}                      remove an overlay teammate
PUT    …/team/{agentId}/inbox                toggle a teammate's inbox
PUT    …/team/{agentId}/budget               set / change / remove a daily cap
DELETE …/team/{agentId}/budget               reset the cap to the manifest default
GET    …/policy                              the autonomy tier + always-ask list
PUT    …/policy                              set the tier and/or the always-ask list
DELETE …/policy                              reset the policy to the manifest's
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

`GET …/workspace/search?q=…` (#607) answers which notes mention a phrase, so
discovery costs one call rather than a listing plus one read per candidate.
Matching is a plain **case-insensitive substring** over node names and text
bodies — no tokenising, no stemming, no ranking — which is the only definition
that answers identically on all three storage backends, and it is defined once
in `company::workspace_search`, shared with the GraphQL `Company.workspaceSearch`
resolver and the agent `workspace_search` tool. Optional `prefix` scopes to a
subtree; optional `limit` pages the answer (default 20, hard cap 50). A hit
carries the node, its logical `path`, whether the `name` or the `content`
matched, and — for a content match — a short `excerpt`. `total` reports every
match, so a capped page says it is one. An empty `q` is a `400`, not "match
everything": that is the tree read above, and answering it here would turn a
cleared search box into a full-tree fetch. `limit=0` is a `400` for the same
reason — it is never read as "no limit". A **binary** node matches on its name
only: a text read of a payload is empty by the port's definition, and its bytes
are never scanned or excerpted.

Both workspace `GET`s — and the `POST` / `PATCH` node bodies — carry
`createdBy` and `updatedBy` (#326), each `{"kind":"seed"|"operator"|"agent",
"id"?}` with `id` present exactly when `kind` is `agent`. `createdBy` is fixed
at creation; `updatedBy` follows content writes only, so an operator rename does
not repaint an agent's authorship. The console renders the creator as a badge
and the last writer only when the two differ. Both fields are always serialized,
and a node predating the field reads back as `operator`. The `PUT` write route
stamps `operator`; agent writes stamp `agent{id}` from the agent's roster id,
which is fixed at agent-build time and never taken from tool arguments. Agents
reach the same tree through `workspace_list` / `workspace_search` /
`workspace_read` / `workspace_create` / `workspace_write`, and a created note has its default home
in the reserved `Agents/<agent-id>/` folder (#551) — a convention the persona
brief steers toward, not a boundary the routes enforce. `workspace_rename` and
`workspace_delete` (#671) are the exception: those two *are* bounded to
`Agents/<agent-id>/`, checked on the resolved node so an `id` argument refuses
exactly as its path would. Neither restamps authorship; a delete leaves any
artifact version that pointed at the node with a dangling `workspaceNodeId`,
which is the same state the `DELETE` route above produces and is read-guarded
before reuse. Boot scaffolds the
`Agents/` root empty; an individual `Agents/<agent-id>/` is minted the first
time that agent writes into it, and the `Desks/` root is minted whole the first
time a desk produces something (#645) — so a tree read on a fresh company shows
exactly one root and no member folders.

Team writes are an **operator overlay** persisted through the store, merged
into the manifest roster at read time — the version-controlled `company.toml`
is never rewritten. Overlay teammates are addressable: since issue #71 the
harness builds a real agent for each one, with no cognition tier and never the
orchestrator.

Since issue #619 an overlay teammate carries its own `tools` list — the
overlay's answer to a manifest `[[agent]].tools` line, and the thing that was
missing when the only readable answer for every teammate was "everything the
company allows". It obeys the same rule a manifest line does: **empty means the
company's standard grant** (#264), so a teammate written before the field
existed reads back exactly as it did. What changed is that a non-empty value is
now writable at all.

`POST …/team` and the orchestrator's `add_agent` tool both **derive the roster
id from the display name** (issue #686): "Dana Designer" becomes
`dana_designer`, in the same snake_case grammar the manifest validator enforces
on a hand-authored `[[agent]].id`. They used to mint an opaque
`{millis}-{counter}` id, which #570/#552/#607 render as a workspace folder and
in search-hit paths — so half a company's tree read as
`Agents/019fad5ada20-000000000003/` beside `Agents/backend_engineer/`.

- **Collisions suffix, they do not refuse.** A slug already held by a manifest
  agent, another teammate, a desk id or name, or a reserved word (`operator`,
  `Agents`, `Desks`) becomes `<slug>_2`, `_3`, … Duplicate display names have
  always been accepted here, and an unsuffixed collision with a *manifest* id is
  worse than a refusal: the roster build skips it, so the teammate would persist
  and never materialise.
- **Minted once, never re-minted.** `PATCH …/team/{agentId}` renames a teammate
  and leaves the id alone; a name-keyed id would orphan its workspace folder,
  budget row, desk memberships and inbox on every correction.
- **Removal frees the slug**, so re-adding the same name takes the id back and
  **adopts the old `Agents/<slug>/` folder** — the intended remedy for a typo'd
  name, and not a way to get a clean slate.

Teammates carrying generated ids are **not migrated**: rewriting them would
rewrite the `WorkspaceOrigin` stamps issue #326 keeps honest, and every path
into their folders. They keep working, reachable by display name through
`crate::runtime::assignee`.

`GET …/team/{agentId}` is the **agent detail** read (issue #264). `GET …/team`
answers "who is on the roster"; this answers "what is this agent", and before it
existed neither the console nor any other client could reach an agent's tier,
its tool grants or its desk membership — the roster row carried none of them, so
checking what a company actually grants an agent was not possible from outside
the process.

The `tools` object is the reason the route earns its keep. It carries three
lists, because only the third is the answer:

| field | meaning |
|---|---|
| `requested` | the agent's own globs — a manifest `[[agent]].tools` line, or an overlay teammate's `tools` list (#619). **Empty means the company's standard grant**, not "no tools" |
| `companyAllow` | the `[tools].allow` ceiling the request is intersected with |
| `effective` | what the agent actually holds |

`effective` is computed by the same `agent_effective_grants` the harness calls
when it builds the agent, so the readout cannot drift from what is enforced.
`isOrchestrator` is likewise resolved by the roster rule (a `tier =
"orchestrator"` agent, else the first declared) rather than read off `tier`, so
a company that tags nobody still names its orchestrator.

`PATCH …/team/{agentId}` edits an **overlay** teammate's `name`, `role`,
`description` and `tools`. It is a patch: an omitted key is left alone, and
`"description": null` clears it — the two must stay apart or every partial save
would erase an agent's instructions. `tools` needs no such distinction: an
omitted key leaves the scope alone and `"tools": []` is the deliberate way back
to the company's standard grant, so `null` never has to mean a third thing. A
blank glob inside the list is a `400` — `""` matches nothing an operator meant,
and storing it would read as a scope that grants nothing while looking like a
scope that was set. Globs are stored verbatim, exactly like a manifest line: the
`[tools].allow` ceiling is applied at read time, so a glob the company does not
cover surfaces as asked-for-but-not-granted rather than vanishing on save.

A blank `name`/`role` is `400`, an unknown teammate `404`,
and a **manifest** teammate is `409`: its fields live in the version-controlled
`company.toml`, and the console does not rewrite the blueprint. The one thing
that *is* changeable on such a teammate is its daily budget, and that works
because #343 modelled it as an override rather than as a rewrite. Every detail
response carries an `editable` list naming the fields this route will accept, so
a client renders read-only from the host's answer instead of re-deriving the
rule. `tier` stays read-only for both kinds: there is no override layer for it,
and adding one is a policy decision rather than a form field. `tools` was
read-only for the same reason until #619 made that policy decision for the
overlay half — a manifest teammate's `tools` line still lives in the blueprint
and is still `409`.

The orchestrator's `add_agent` tool writes the same field, under one added rule:
**a minted teammate is never wider than the agent that minted it** (#619).
Omitting `tools` copies the minter's own line — so an unscoped minter still
mints an unscoped teammate, which keeps tracking `[tools].allow` instead of
freezing a copy of it. Passing `tools` narrows the request against what the
minter actually holds, and a request that narrows to nothing is a clean tool
error rather than a stored empty list, because an empty list means *inherit
everything* — silently storing one would turn a deliberate narrowing into the
widest grant in the company. Every mint is logged with the minter, the teammate
and the resolved scope: `add_agent` is `Reach::Nothing` and never asks, so the
log is the only place the grant is observable at all.

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

The three **policy** routes (issue #562) are the company-scoped twin of the
budget pair. Before them the autonomy tier lived only in `[policy].mode` and
nothing in the console read or wrote it, so an operator drowning in approval
cards could change it only by redeploying an edited `company.toml` — or, on a
hosted tenant with a read-only manifest snapshot, not at all.

`GET` returns the tier and always-ask list **in force**, what the manifest would
restore, whether an override is set and by whom, and the selectable tiers with
the host's own description of each (`POLICY_MODES` narrowed to tiers the console
has text for, so it never offers one the host would downgrade). `PUT` takes `mode`
and `alwaysApprove`, both optional and independent — `{"mode": "auto"}`
leaves the list alone, `{"alwaysApprove": []}` clears it (a real state, not a
reset), `{"mode": null}` stops overriding the tier, and `{}` is a **`422`**
because a body that sets nothing is never stored. An unknown `mode` is `422`
too, not accepted-and-downgraded, or the console would show a tier the gate was
not running. Both writes are admin-only and attributed. `DELETE` restores the
manifest's `[policy]` — its own verb, since a `PUT` of the manifest's current
values would pin them. The change takes effect on the company's **next turn**
(`ApprovalPolicy` is built per roster build, and this override is fingerprinted
alongside the other freshness axes). It survives a rebuild unless the seed's
`[policy]` itself changed: version control wins when it speaks, so tightening
`company.toml` clears a looser tier set here, and a redeploy that changed
nothing does not.

### Credential-bearing surfaces (feature-gated)

These write secrets to the `SecretStore` and expose only non-secret status.
The networked half of each (DNS lookup, SMTP send, OAuth token exchange) is
dependency-inverted behind a trait; when the relevant seam is absent the write
route `404`s with `{"code":"not_wired"}`.

```text
GET    …/credential                         whether the company has its own key + which tier it presents
PUT    …/credential                         set / rotate / clear the company's TinyHumans key  [admin]
PUT    …/domain                             set the custom domain
POST   …/domain/verify                       server-side DNS check
PUT    …/smtp                               store SMTP credentials (secret store)
POST   …/smtp/test                           send a test email
POST   …/connections/{provider}/start        begin OAuth (returns authorize URL)   [feature: oauth]
POST   …/connections/{provider}/disconnect   drop stored OAuth tokens               [feature: oauth]
GET    /api/v1/oauth/callback                OAuth redirect target (unscoped; state carries the company)  [feature: oauth]
```

`…/credential` is the company's **one** TinyHumans key, presented by every
surface wired to it (**Composio today**) — see
[`credentials.md`](credentials.md) for the resolution order, the rotation
guarantee, and which surfaces are deliberately outside it.

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

- **A message has a durable id, and things can refer to it (issue #364).**
  `POST /chat` answers with `messageId` — the sequence position the operator's
  own message was journaled under — and stamps the same on each reply bubble.
  Two things name a message by that id:

  - The `parent` field on the `/chat` body makes the send a **thread reply**.
    It is journaled onto both the `OperatorMessage` and the replies it draws,
    so the whole exchange comes back under the same row on the next read. A
    `parent` that is not a message id is a `400`, never a silently-flattened
    thread — a reply that quietly lands in the channel reads as a lost reply.
  - `POST /chat/messages/{seq}/reactions` sets or clears **one person's** one
    reaction. `on` is explicit rather than a toggle, which is what makes a
    retry or a double tap idempotent. The target must be a chat message —
    anything else is a `404`, so the log can never hold a reaction no reader
    could render — and the emoji is bounded and refused if it carries control
    characters. Authorized through the same gate a send passes: reacting is
    writing into a transcript, so it can be neither easier nor harder than
    saying something in it.

  Both project through the shared `MessageView`, so REST and GraphQL cannot
  disagree about the shape of a thread or who reacted. Reactions are
  deliberately absent from the `/events` stream — see [events.md](events.md).

  The copilot addresses `workflow-copilot:<workflowId>` (a `:` cannot occur in
  a manifest desk id, so it can never collide with a real desk, and it does not
  appear in `GET …/desks`). Making unknown thread ids a `404`, or loosening
  `owns` to match on prefix, would break that surface — see
  [`frontend/src/api/workflow-copilot.ts`](../../../frontend/src/api/workflow-copilot.ts).

  **Thread addressing isolates transcripts. For every thread but one, it does
  not scope authority.** The thread id decides who answers and where the
  exchange is journaled; for an ordinary thread it does **not** narrow the
  responder's context or tool grants, which stay company-wide however the turn
  is addressed.

  **The copilot thread is the exception (issue #416).** A `chat` id matching
  `workflow-copilot:<workflowId>` ([`company::copilot`](../../../src/company/copilot.rs))
  makes the turn **confined**, host-side, in two places that hold independently:

  - the harness runs it on an ephemeral agent with **no tools, no company
    memory and no delegation** ([`harness::confine`](../../../src/harness/confine.rs)),
    and skips the retrieve→inject step and the memory writeback, so the turn
    answers from the message it was sent and leaves nothing behind. Every tool
    call is denied by the host with a reason, so an empty toolbelt is a
    boundary rather than an absence;
  - the `/chat` handler does not open a board card from a copilot message, so a
    question phrased as a request cannot leave work on the board. That half is
    in the default build, not behind `openhuman`.

  Confinement narrows one **turn**; it is not an authorization check and must
  not be read as one. `/chat` is already authenticated and company-scoped, so
  an operator addressing a workflow thread gains nothing they could not get by
  opening the Chat tab or calling the workflow routes directly. What the
  copilot adds is a transcript that stays out of the team's chat and an answer
  drawn from one workflow rather than from everything the company knows.

  **A copilot answer may carry a proposed edit (issue #415), and that adds no
  route and no capability.** The proposal is a fenced block in the reply text —
  a list of node/edge operations — which the *console* turns into a candidate
  graph and applies through `PUT …/workflows/{wid}` with `expectedVersion`, the
  same write the canvas editor performs, after the operator has read the diff
  and pressed Apply. The confined turn still calls nothing: it emits text, and
  a person decides. So the host needs no notion of a proposal, and a proposal
  cannot produce a graph the editor could not have produced — including the
  `409` a graph that moved underneath it earns.

  Two more consequences worth knowing before reusing the seam. A chat turn
  runs the **whole** company cycle, so every message is first classified by
  `company::task_intent::triage_message` (#267) into `Track` (an instruction —
  the route opens a `todo` card), `Answer` (a question or read — no card), or
  `Chatter` (greetings, and anything ambiguous — no card). `Answer` is also the
  only class that *gates*: the harness narrows the issue-#453 delegation claim
  to answering for that turn, so the model's own `spawn_task` / `assign_task` /
  `review_task` fail at the tool boundary with the do-not-retry refusal.
  `delegate_to_desk` is deliberately **not** refused — it is how a question the
  orchestrator cannot answer alone reaches a desk that can — so it runs the
  desk lead and relays their reply, and only its board card stands down.
  `query_company` / `run_workflow` / `read_run_output` run inline and are
  untouched throughout. The turn loses the ability to *write*, never the
  ability to answer. Ambiguity falls to `Chatter`, which neither cards nor gates: a
  missed card costs one follow-up message, a spurious card pollutes the board
  permanently. The gate is harness-only — `HostedMedullaBrain` has no
  delegation stack to gate (#176) — while the triage itself is compiled into
  every build and fronts both brains. The card half is suppressed wholesale on
  a copilot thread (#416), precisely because the seam is being reused for a
  conversation that is not a request to the company. And an
  unconfigured company answers
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
