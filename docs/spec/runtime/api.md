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
POST   /api/v1/companies/{id}/chat/upload      multipart file → attachment reference (#1682)
GET    /api/v1/companies/{id}/chat/history     one desk's transcript (?desk=<thread>)
POST   /api/v1/companies/{id}/chat/messages/{seq}/reactions
                                               { "emoji": "👍", "on": true } → 204
POST   /api/v1/companies/{id}/chat/review      { "chatId", "taskId", "decision": "approve"|"revise",
                                               "note"? } → ChatReviewReceipt (openhuman feature only)
GET    /api/v1/companies/{id}/desks            the company's desks (group chats)
POST   /api/v1/companies/{id}/desks            create an operator-overlay desk
DELETE .../desks/{deskId}                      delete an operator-created desk
POST   .../desks/{deskId}/members              { "agent_id": "…" } → 204
DELETE .../desks/{deskId}/members/{agentId}    remove an operator-added member
PUT    .../desks/{deskId}/order                { "ordered_member_ids": [...] } → 204
GET    /api/v1/companies/{id}/events?since=SEQ SSE stream of events/effects (work feed)
GET    /api/v1/companies/{id}/approvals        pending approvals
GET    /api/v1/companies/{id}/notifications  unread notifications for the signed-in person
PUT    /api/v1/companies/{id}/notifications  mark notifications read (`{ "ids": [...] }`; empty body or null ids marks all)
POST   /api/v1/companies/{id}/approvals/{aid}  { "verdict": "approve"|"deny", "note": "…",
                                               "detach": false,
                                               // a parked blocker only: which of the four
                                               // things the stopped step should do. Narrows
                                               // `verdict` (retry/amend/skip approve, cancel
                                               // denies) — a pair that disagrees is a 400.
                                               // `blocker_answer` is mandatory and non-blank
                                               // with "amend", refused with the rest.
                                               "blocker_verdict": "retry"|"amend"|"skip"|"cancel",
                                               "blocker_answer": "…" }
POST   /api/v1/companies/{id}/feedback         submit feedback (see feedback-loop/)
GET    /api/v1/companies/{id}/feedback         past reports (no operator words)
GET    /api/v1/companies/{id}/feedback/board   the shared board, one page
                                               ?sort=hot|top|new&type=feature|bug
                                               &status=open|planned|completed
                                               &page=1&limit=20
GET    .../feedback/board/{item}               one board item + its comments
POST   .../feedback/board/{item}/vote          { "value": 1 | -1 | 0 }
POST   .../feedback/board/{item}/comments      { "body": "…" }
GET    /api/v1/companies/{id}/memory/traces    inspect working memory (debug)
GET    .../memory/archives                    traces retained on eviction
                                             (provider-backed engines only; 404
                                             when the engine keeps no archive)
POST   /api/v1/companies/{id}/export           export bundle (tar)
POST   /api/v1/companies/{id}/pause            pause / resume lifecycle transitions
GET    /api/v1/companies/{id}/desks            the company's desks and channels
POST   /api/v1/companies/{id}/desks            create one ({ name, description?, id?,
                                               members?, responder? })
DELETE /api/v1/companies/{id}/desks/{desk}     delete an overlay desk
POST   /api/v1/companies/{id}/desks/{desk}/members         add a member
DELETE /api/v1/companies/{id}/desks/{desk}/members/{agent} remove an overlay member
PUT    /api/v1/companies/{id}/desks/{desk}/order           reorder (hierarchy)
```

Single-company (prosumer) mode aliases everything under `/api/v1/company/...`
with no `{id}`.

`GET …/notifications` returns every unread notification addressed to the
signed-in human, newest first — not just `mention`: `dispatch_failed`,
`approval_expired`, and `workflow_run_*` rows are the same durable, user-facing
feed and are not filtered by kind. Each row includes its subject, title, creation
 time, and optional chat context; `unread` is the returned count. Machine
credentials, which have no person identity, receive `401`. `PUT` accepts an
optional `ids` array and returns the remaining unread count. An omitted or null
`ids` value marks all notifications for that person; an empty array marks none.

The `/feedback/board/...` routes are a **proxy** of the TinyHumans hub's shared
board, spent with this instance's credential so a browser never holds one. An
instance without a credential has no board and every one of them answers
`404 tinyhumans_no_board` — the console hides the surface rather than rendering
an empty board. A vote is the *instance's* vote, since every console on a host
shares its one hub account. See
[feedback-loop/README.md](../feedback-loop/README.md).

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

### The built-in `#general` channel (issue #1743)

Every company has a company-wide line from first boot, and nobody can delete,
rename or restaff it. It is **not** a desk, and the shape follows from that.

A desk has a lead and a hierarchy — `members[0]` is the lead, `PUT
…/desks/{id}/order` is how the hierarchy is set, and `delegate_to_desk` routes
work to whoever leads it. "Everyone" has none of those. So `#general` is
deliberately **absent from `GET …/desks`**, which is what keeps every
desk-shaped surface honest without any of them carrying a special case: the org
chart, the assignee picker and the desk counts all read that one route, so none
of them can offer this channel a lead, a seat, a rename or a delete.

Nothing new is stored, and nothing new is addressable. The host has folded four
spellings — `""`, `main`, `General`, `general` — into one conversation since
issue #65 (`chat_history::is_general_chat`), and an unaddressed `POST …/chat`
has always landed there and been answered by the orchestrator. This channel is
that conversation, made visible in the rail rather than invented beside it.

**Membership is derived, never stored.** "Who is in `#general`" is "every
teammate on the roster", computed on each read. There is no membership record,
so a teammate added a minute ago is a member with no write anywhere and the two
cannot drift; a retired one leaves on the next read for the same reason.
`@everyone` posted here expands to that roster (before #1743 it expanded to
nobody, because the broadcast arm looked for a desk and found none). It stays a
**list, not a fan-out** — one operator message spawns exactly one turn, whatever
it names — so a broadcast here costs the same as any other message.

**Who answers a message that mentions nobody:** the orchestrator, one turn, as
it always has for the company's main line. An `@`-mention overrides that exactly
as it does in a desk channel, and delegation from the answering turn is
unchanged. Deliberately not "every agent sees it": a message that woke the whole
roster would cost one turn per teammate for a line that may be a greeting (cf.
issue #1725), and the conservative default is the one this host already had.

That holds even when a **teammate** is called `main` or `General`. `mint_agent_id`
reserves both, but a manifest can still declare one, and `responder_for` used to
match the roster on the bare key — so that teammate answered every unaddressed
message while `GET …/chat/history?desk=main` returned the *folded General
conversation* rather than its transcript: the responder and the transcript named
different conversations. The fold is a fact about the address, not about who was
addressed, so the bare key is the company's line and the teammate keeps its DM
under `dm:<id>`, which `responder_for` still routes to it.

**Every desk write aimed at it is refused with a reason** — `409` and a sentence,
never a bare `404`, because "this id is reserved" and "no such desk" are
different facts the caller needs to tell apart:

| write | answer |
|---|---|
| `DELETE …/desks/general` (or `main`, any case) | `409` — it is not a desk; there is nothing to delete |
| `POST …/desks/{general}/members` | `409` — membership is derived; there is nothing to write |
| `DELETE …/desks/{general}/members/{agentId}` | `409` — same |
| `PUT …/desks/{general}/order` | `409` — it has no hierarchy to order |
| `POST …/desks` with a general id (given or derived from the name) | `409` — the id is reserved, so no desk can shadow the channel |
| `POST …/desks` with the general **display name** under any id | `409` — same reason: `resolve_desk_id` matches a desk by name too |

There is no `PATCH …/desks/{id}` route on this host, so that table is the
complete desk mutation surface.

The refusals are guarded on **the manifest**, not on "no existing desk". A
company whose blueprint really declares a `[[group_chat]]` with one of those
ids keeps it and keeps every write that has always worked on it — the
reservation replaces the "no such desk" answer and nothing else. Refusing on the
id alone would have taken a desk away from every company that authored one,
which is a migration rather than a feature. No shipped `companies/` manifest
declares one, and new ones are refused at creation.

**An operator-created overlay desk is not grandfathered**, because it is not a
blueprint. `POST …/desks` accepted these ids and this name until this issue, so
an upgraded instance can be carrying one — and exempting it would leave the
channel this section calls permanent staffable, reorderable and deletable after
all. Such a desk is therefore:

- **absent from `GET …/desks`**, so no desk-shaped surface offers it a control
  that the writes above would refuse;
- **refused every write in the table**, with the channel's reason;
- **not resolved by a General key at all.** `CompanyRecord::resolve_desk_id`
  searches the manifest desks first and then the overlay ones, and it declines
  the overlay half when the key asked for is a General spelling. Hiding the desk
  from `GET …/desks` was not enough on its own: `desk_lead` → `responder_for`
  resolves through that function, so such a desk's lead would have answered the
  company-wide line while the console rendered `#general` and named the
  orchestrator. One choke point rather than a guard per caller, so
  `@everyone` (`runtime::mentions`), the responder, and `delegate_to_desk`
  grounding (`delegation_tools::desk_ids`, which omits an id nothing can
  resolve) all follow without their own special case.

  Keyed on the **key**, not on the desk: the same desk still resolves under its
  own non-General id, keeps its members, and still routes there. This narrows
  one question; it does not retire a desk. A desk merely *named* `General` is
  likewise still reachable by its own id.

Nothing is deleted to achieve that. Its transcript was already folded into
`#general` by `is_general_chat`, and that channel's membership is the whole
roster — a superset of whatever the desk held — so the conversation and the
people are both still there under the channel that renders them.

## Desks and channels: the `responder` mode

A desk row carries `responder: "lead" | "auto"` (issue #1835), **omitted when
`lead`** — which is every manifest `[[group_chat]]` (the blueprint syntax has
no such field) and every desk created before the field existed, so old
consoles and old wire shapes are byte-for-byte unchanged.

`"lead"` is the standing model: `members[0]` leads, and an unmentioned message
addressed to the desk is answered by that lead. `"auto"` is a **channel**: no
lead exists — the org chart crowns nobody, the members pane badges nobody, and
`delegate_to_desk` refuses it with a reason — and an unmentioned message's
answerer is picked **per message**, by a single tool-less model call over the
channel's own membership (id, role, description), clamped to that membership.
An `@`-mention outranks the pick everywhere, and wherever selection cannot run
— the default build (the selector compiles under the harness feature), the
small-talk fast path, a failure, a timeout — the answer is the channel's first
roster member: exactly what a lead desk would have answered, so the worst case
of the new mode is the old mode. Selection spend is metered under its own
usage kind (`selectorCall`), charged to the whole-company bucket.


### Chat attachments (issue #1682)

```text
POST   …/chat/upload                          multipart file → { nodeId, name, mime, size }
POST   …/chat                                  { "message": "…", "attachments": ["<nodeId>", …] }
```

Two steps, deliberately not one: the byte-transfer half is decoupled from the
synchronous, turn-running `/chat` POST, so a large upload never blocks the
turn and a turn never blocks on bytes. `/chat/upload` is a **binary-only**
sibling of the workspace's `POST …/workspace` create — a chat attachment is a
file hung on a message, not a document someone maintains, so it always stores
bytes and is served back through the existing hardened
`GET …/workspace/blob/{nodeId}` (no second blob route). It shares
`admit_upload`'s size/quota gate and the workspace's filename sanitizer with
that route, and is subject to the same sibling-name collision rule a
workspace create enforces — two attachments in different messages sharing an
exact filename would collide there, so this route retries once, transparently,
under a name disambiguated from the upload's own id rather than surfacing the
`409`.

`/chat`'s `attachments` field is **node ids only**. The host re-resolves each
id against the sending company's own workspace tree and takes the name / mime
/ size from the store — never the client's claim — the same discipline a
`parent` thread reference gets; an id that resolves to no binary node in this
company is a `400`, on the same terms a bad `parent` is. Server-side, the host
also extracts each attachment's text where the format and size allow it (PDF,
DOCX, PPTX, XLSX, plain text — the same `ingest::extract` pipeline
`POST …/memory/ingest` runs; see [memory.md](../company-brain/memory.md)) and
carries it in the journaled event, capped, so a brain that later reads the
message off the wire has the attachment's actual words rather than only a
node id it has no tool to resolve. An image or a scan with no text layer
carries no extracted text; the reference alone still rides the wire.

### Thread review verdicts (issue #1852)

```text
POST   …/chat/review      { "chatId", "taskId", "decision": "approve"|"revise", "note"? }
                          → ChatReviewReceipt { "taskId", "column": "done"|"in_progress"|"in_review" }
```

Settles the `in_review` dispatch card a chat thread is reviewing — the board
card the thread's settle pill announced, **not** the native-tool approval gate
`POST …/approvals/{aid}` settles. `chatId` is the origin conversation (the
desk/channel id); `taskId` is the specific card the operator clicked, because a
desk can hold more than one card `in_review` at once — the verdict is bound to
that card rather than resolved by picking the desk's most recently updated one.
`approve` finishes the card; `revise` re-runs it, carrying `note` back to the
re-run as the reviewer's instruction, on the same path a thread reply of
feedback already takes.

`column` on the receipt is `done` on approve, `in_progress` on revise — or
`in_review`, unchanged, on a revise whose `note` was blank. An empty note is
nothing to re-run on, so the host leaves the card where it was rather than
dispatching an identical attempt a second time; the console reconciles its
optimistic move against whichever of the three comes back.

Gated behind the `openhuman` feature (`with_review_routes`): the harness that
dispatches cards in the first place is what settles them, so a build without
it never mounts `/chat/review` at all — the request 404s at the router, not
through the JSON error envelope below. Errors on a build that does carry the
route: an unrecognized `decision` string is `400 invalid_request`; a `taskId`
that names no card `in_review` on that desk — a wrong id, or no card in review
at all — is `404 not_found`. The verdict itself is serialized against the same
company-wide task-write lock every other card mutation takes, so two verdicts
racing the same card cannot both resolve it.

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

That read is **paged** (issue #1012): `{ runs, hasMore, nextBeforeSeq }`, where
`nextBeforeSeq` is the cursor to pass back as `?before_seq=` and is omitted once
`hasMore` is `false`. The page is cut by `seq` and only then sorted for display
by `(atMillis, seq)`, so the cursor is the page's *lowest* `seq` rather than its
last row — clients must send back what the host issued rather than deriving it,
and a client talking to a host that omits the field falls back to the old
`runs.at(-1).seq` derivation, never to "no more pages". Why the two keys differ,
and the partition argument that makes paging lossless under a clock regression:
[server/run-history-paging.md](../../modules/server/run-history-paging.md).

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

Moved to [`api-write-plane.md`](api-write-plane.md) — this file was over the repository's 500-line limit. See that page for the full detail.

## Read plane — GraphQL (`/graphql`)

Moved to [`api-graphql.md`](api-graphql.md) — this file was over the repository's 500-line limit. See that page for the full detail.

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

`409` is the most overloaded status here, so **the `code` carries the meaning,
not the status**. Three of its codes are permanent states rather than failures,
and a caller that retries them retries forever:

| `code` | Means | What clears it |
|---|---|---|
| `not_in_build` | the binary was compiled without this surface | a different build |
| `not_configured` | the surface is here; this company has not set it up | an operator setting it, elsewhere |
| `restart_required` | saved config the running runtime booted without | restarting the company |

Everything else on `409` — `conflict`, `lifecycle_conflict` — is an ordinary
conflict a caller clears by retrying or by sending something else. Clients must
branch on `code` and never infer permanence from the status: the console's
`classifyLoadFailure` does exactly this, and read `409` as transient across the
board until it did (issue #2081).

`not_in_build` is `501` on the finance routes and `409` elsewhere. The status
differs; the code does not, which is why the code is the thing to read.

## Platform webhooks (Phase 5)

Platform mode can register outbound webhooks per tenant for
`approval.requested`, `work.completed`, `feedback.created`, and
`budget.exhausted` so hosts can build their own surfaces without polling
SSE. Delivery is at-least-once with signature headers; see
[product/platform.md](../product/platform.md) for the requirements source.
