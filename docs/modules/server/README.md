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
| `tasks` | `POST …/tasks`, `GET …/tasks`, `GET …/tasks/{id}` (the Task Detail read, #185), `PATCH`/`DELETE …/tasks/{id}`, `GET …/tasks/inflight`, `POST …/tasks/{id}/steer` (#111), `POST …/tasks/{id}/discussion` (#335) |
| `memory` | `POST …/memory`, `DELETE …/memory/{id}` (journals `MemoryFactDeleted`) |
| `workspace` | `GET …/workspace`, `GET …/workspace/file/{id}`, `POST …/workspace`, `PUT …/workspace/file/{id}`, `PATCH`/`DELETE …/workspace/{id}` (the two `GET`s are REST twins of the GraphQL reads — the console has no GraphQL client, #177) |
| `skills` | `POST …/skills`, `GET …/skills/registry`, `POST …/skills/{slug}/install\|uninstall`, `PUT …/skills/{slug}` |
| `team` | `POST …/team`, `DELETE …/team/{id}`, `PUT …/team/{id}/inbox` (overlay; roster-only in v1) |
| `mail` | `POST …/inboxes/{key}/read` |
| `inbox` | `POST …/inboxes/ingest` (HMAC-signed inbound email) |
| `domain` | `PUT …/domain`, `POST …/domain/verify` |
| `smtp` | `PUT …/smtp`, `POST …/smtp/test` |
| `connections` (feature `oauth`) | `POST …/connections/{provider}/start\|disconnect`, `GET /api/v1/oauth/callback` |
| `workflows` | `POST …/workflows`, `GET …/workflows`, `GET …/workflows/runs`, `POST …/workflows/cron/preview`, `GET …/workflows/{wid}`, `PUT …/workflows/{wid}`, `DELETE …/workflows/{wid}`, `POST …/workflows/{wid}/run` |

### The per-task discussion (issue #335)

The Task Detail screen's Discussion tab shipped as an honest stub with no
backend. This is what was decided when it got one, written down here rather
than left to be inferred from the schema.

**A task discussion is its own thread, not a filtered view of company chat.**
The alternative — reuse the chat store and filter it to a card — is cheaper and
was rejected: chat is addressed to a *desk* and is a conversation with the
company, while a discussion is addressed to a *card* and is a note about one
piece of work. Filtering one into the other would mean every card's thread is a
slice of a stream whose messages were written for a different audience, and a
card would stop being the record of its own work the moment somebody replied in
the desk instead.

**One store, two projections.** The thread is *not* a second message store: a
post is journaled as `CompanyEvent::TaskDiscussionPosted`, in the same
company `EventLog` the timeline is folded from, and `GET …/tasks/{task_id}`
serves both out of **one** traversal (`fold_task_journal`). That is what keeps
the two notions of "something happened on this task" from drifting — they are
the same log read two ways.

They stay two *arrays* rather than one merged list because they answer different
questions: the timeline is the record of what the **company** did on the card
(dispatch, replies, failed tool calls, approvals, completion), and the
discussion is what **people** said about it. Merged, an operator's aside would
sit between a dispatch and its completion and read as part of the run.

**What the shared log costs, stated plainly.** The read rides a cost that was
already there — the detail folded the whole journal per request before this
existed. The *write* does not: it points a new, human-paced, unbounded writer at
the log every other projection folds over, with no retention or compaction for
it anywhere in the tree. That is not hypothetical — it is the conclusion the
runtime already reached and acted on: `src/ports/runs.rs` (#342) says outright
that "folding the whole journal per read is what makes the existing workflow-run
list expensive", #242 answered it by giving runs a store of their own instead of
more events, and #357 now writes a `RunRecord` per attempt on `main`. This goes
the other way knowingly: one small event per post with no per-step fan-out, and
a paged read so thread length no longer sets response size. But it is a shared
log with an extra writer on it, and the moment a thread needs history, search or
retention it wants the same treatment runs got.

**The read is paged.** `GET …/tasks/{taskId}` answers with the newest
`DISCUSSION_PAGE` (50) messages plus `discussionHasMore`, and
`?discussionBefore=<seq>` walks backwards — the `first` + `before_seq` shape
`chat_history::history_for_desk` already uses. Without it the whole thread is
re-sent on every 4s poll, per browser, forever. The fold drops out-of-window
posts as it reads them, so a long thread is traversed but never resident. The
timeline is *not* paged: it is bounded by what the company did on one card.

**Operator-only in v1; agents do not participate.** Posting journals a message
and nothing else: no cycle runs, no turn is dispatched, and no agent reads the
thread back. The projections enforce it rather than merely documenting it — the
sidecar wire body (`wire_event`) and the orchestrator's insight line
(`summarize_event`) both name the *card* and deliberately omit the *text*, so
the tab cannot become an unannounced prompt surface. Nor does a post hold a slot
in the orchestrator's ten-event activity tail: they fold there into one
"N discussion posts" line, because a card's afternoon of back-and-forth would
otherwise evict every dispatch, reply and approval from the only view the
orchestrator has of the company — "agents do not participate" has to hold for
the *slot* as well as the *text*.

Agent participation is a product decision to take with first-class runs (#242):
a discussion anchored to a task and one anchored to a run are different things.
That deferral was cleaner when this branch was cut than it is now — #342 landed
`RunStore` and #357 landed the write path, so an attempt *is* a first-class
record on `main` and the question is answerable rather than blocked. Still
deferred, but on scope, not on missing prior art.

**Ordering, editing, deletion, references.** Oldest-first by journal sequence,
which is also the console's render key. The journal is append-only, so v1 has no
edit and no delete — what was said stays said; a retraction would need a
tombstone event and is not one. That is a real gap: the log is what export/import
ships, and a discussion is exactly where somebody pastes the API key they are
blocked on. Tracked as **#358**, a redaction path for journaled human prose. A
message is plain text: it cannot yet reference an artifact or an approval, left
until there is a link target more durable than a row's position.

**Attribution.** A post carries the signed-in user as `by`, resolved to a roster
label on read (a display name, or an email's local part). A user no longer on
the roster reads as `someone`; a post made with a machine credential reads as
`operator`. A user id and an email address never reach the wire — a thread is
read by every member of the company.

The write is `POST …/tasks/{taskId}/discussion` with `{text}`. Empty or
whitespace-only text is a `400` (there is no delete, so a blank row would be
permanent noise), an unknown card is a `404`, and over-long text is truncated to
`MAX_DISCUSSION_CHARS` rather than refused. The `201` echoes the journaled row —
read back at its own `seq`, not re-stamped — so the console renders the post at
once under the key the next poll returns it under. Reads ride the detail's 4s
poll, which is what makes another operator's post appear without a reload.

### Reading a trigger's cron back (issue #262)

`POST …/workflows/cron/preview` answers what a 5-field expression means and when
it next fires. It exists because a schedule's *dangerous* failure is the one
that validates: `0 9 * * *` and `9 0 * * *` are both valid and nine hours apart,
and the dialect is always UTC — so an author in IST who wants a 9am report
writes `0 9 * * *` and gets one at 14:30 local. No validation can catch either
mistake, because neither expression is wrong.

```bash
curl -X POST "$HOST/api/v1/company/workflows/cron/preview" \
     -H "Authorization: Bearer $TOKEN" \
     -H 'content-type: application/json' -d '{"expr":"0 9 * * MON"}'
```

```jsonc
{ "description": "Every Mon at 09:00 UTC",
  "next": [1786007000000, 1786611800000, 1787216600000] }
```

`description` is `null` for a shape the humaniser declines to paraphrase (a
restricted month or day-of-month, say) — the fire times still state the schedule
exactly, so a `null` description is a designed answer rather than a failure.
`next` is epoch millis, which is what lets the console render each fire time in
UTC *and* in the viewer's own zone from one number that cannot disagree with
itself.

**A malformed expression is a 200**, carrying the parser's message:

```jsonc
{ "error": "cron `every day` needs 5 fields (minute hour day month weekday), found 2" }
```

That is deliberate. The console previews while the author is still typing, so a
half-written expression is the normal live state, not an exception — and the
console's HTTP client throws on any non-2xx, so a 400 per keystroke would force
`try`/`catch` as ordinary control flow. The rejection that matters is unchanged:
`POST …/workflows` still validates the schedule and refuses to save a bad one.

Optional `"after": <epoch millis>` pins the instant the fire times are counted
from; it defaults to now and exists so tests need not assert against a moving
clock.

### Editing and removing a workflow (issue #259)

A saved workflow used to be write-once: a typo'd cron or a node pointed at the
wrong teammate was permanent, and the only recovery was to author a second
workflow and leave the broken one firing. `PUT` replaces a graph wholesale and
`DELETE` removes it.

**Only overlay-backed graphs are writable.** A workflow defined by a file in the
company source tree (`companies/<name>/workflows/<wid>.toml`), or enabled by
name with no saved graph at all, answers `409` — it is not squeamishness about
touching disk, it is the reader's rules restated. `load_workflow_union` gives a
seed file precedence on an id collision, so an edit stored behind one would
never be served; and `merge_enabled_workflows` re-derives `[workflows].enabled`
from seed ids at boot, so a "delete" of a seed-backed workflow would come back
on the next restart. The same invariant is what makes an overlay delete durable.

The read routes project that predicate as `editable`, so the console can grey
its buttons out instead of surfacing a 409 after the click.

**The version token.** `GET …/workflows/{wid}` returns an opaque `version` for
an editable graph. Echo it back — in the `PUT` body as `expectedVersion`, or on
the `DELETE` as `?expectedVersion=` — and the write is refused with `409` if the
graph moved in between, so one console cannot silently overwrite another's edit.
The comparison happens under the same per-company write lock as the save, so it
is a real guard rather than a check-then-act race. **Omitting it is an
unconditional write**, which keeps `curl` usable without a read-modify-write
dance; the console always sends it. Never parse the token — the contract is
"echo back what the read returned", which is what lets the algorithm change
without a client migration.

**The id may not change.** A `PUT` whose body `id` differs from `{wid}` is a
`400`. The id keys the union read path, the scheduler and every journalled run,
so a rename would silently orphan all three. A rename is a create plus a delete.

**Past runs are orphaned, not reaped.** A deleted workflow's
`WorkflowRunFinished` entries stay in the company journal and keep coming back
from `GET …/workflows/runs`. The journal is append-only and shared with chat and
audit, so there is no per-workflow table to cascade; and what a workflow *did*
stays true after the workflow is gone. Retention is a separate design.

**No scheduler change is involved.** `WorkflowScheduler::tick` re-reads the
company record and re-derives the schedule set from the overlay union every
minute, so the tick *is* a continuous reconcile: a deleted workflow stops firing
on the next tick and an edited cron takes effect on it, with no restart and no
unbind call. There is no persisted registration to drift — which is why this
needs no equivalent of OpenHuman's `reconcile_schedule_triggers_on_boot`, whose
job is re-syncing cron rows that live in a second durable store.

```bash
# Read the graph and its concurrency token.
curl -s "$HOST/api/v1/company/workflows/weekly_digest" \
     -H "Authorization: Bearer $TOKEN"
# → { "id": "weekly_digest", …, "editable": true, "version": "73e8ccc6…" }

# Correct the schedule, conditional on nothing having changed since that read.
curl -X PUT "$HOST/api/v1/company/workflows/weekly_digest" \
     -H "Authorization: Bearer $TOKEN" \
     -H 'content-type: application/json' -d '{
  "id": "weekly_digest",
  "name": "Weekly digest",
  "nodes": [ { "id": "start", "kind": "trigger", "name": "Monday 10:00",
               "schedule": "0 10 * * MON" },
             { "id": "done", "kind": "output", "name": "Owner summary",
               "destination": { "kind": "owner" } } ],
  "edges": [ { "from": "start", "to": "done" } ],
  "expectedVersion": "73e8ccc6…"
}'
# → 200 with the stored graph and a FRESH version; or 409 if it moved.

# Remove it. 204 on success; past runs stay readable.
curl -X DELETE "$HOST/api/v1/company/workflows/weekly_digest?expectedVersion=a60663c5…" \
     -H "Authorization: Bearer $TOKEN"
```

**In the console.** The Workflows view offers Edit and Delete side by side, both
greyed out with the host's own explanation when `editable` is `false` — an
explicit `false` only, since a host predating this sends no such field and
`undefined` must not read as a refusal. Edit reopens the create dialog hydrated
from the saved graph, with the **id read-only** (a rename is a `400`, so
offering the field would be a trap) and `version` sent as `expectedVersion`. A
`409` leaves the dialog open with the host's message and raises the same
persistent conflict banner Delete uses, whose Reload re-reads the graph and its
token; nothing is retried without one.

**An edit does not disarm a schedule.** OpenHuman's `flows_update` forces
`enabled = false` when an edit turns a manual trigger into an automatic one, so
a new schedule cannot go live unreviewed. This host has no equivalent and
deliberately gains none here: `WorkflowScheduler::tick` does not gate on
`[workflows].enabled` at all, so writing `false` would stop nothing, and
`adding_a_schedule_by_edit_arms_the_workflow_on_the_next_tick` pins that. An
edit is therefore exactly as live as a create, which already persists a running
schedule the same way. Reversing this means first making the scheduler honour
`enabled`, which is the enable/disable follow-up below.

**Deliberately not in #259**, each its own follow-up: revision history and
rollback (OpenHuman keeps a bounded snapshot ring in a dedicated table; our
overlay bodies live inside `CompanyRecord`, which is saved whole on every write,
so a ring needs its own store surface); journal retention (see above); and
enable/disable without deleting, which means first reversing this scheduler's
deliberate decision not to gate on `[workflows].enabled`.

### Workflow runs and report delivery

A workflow's terminal `output` node may carry a `destination` — `owner`,
`email`, or `channel` — saying where its report goes once the run finishes. It
rides the create body and the read shape under the same key, and the model
type is reused verbatim in both directions (`kind` / `target` are single words,
so there is no camelCase mirror to drift from).

Delivery itself is **not** a route concern. It runs host-side in the shared
`WorkflowRunner` path (`src/workflows/delivery.rs`) once the engine returns,
because the orchestrator's `run_workflow` tool and the trigger scheduler drive
that same port — and a scheduled run is exactly the case where nobody is
watching the console. An **on-demand** run's response therefore carries
`deliveries`: one row per attempt (`sent` / `skipped` / `denied` / `failed`)
with an operator-readable reason. A delivery failure never fails the run, so on
that run the list is where an operator learns a report did not go out; an
unwired runtime writes a loud `failed` row rather than skipping silently.

A **scheduled** run is not persisted, so its delivery outcomes are not surfaced
yet. The scheduler logs each undelivered report and drops the run value — see
`src/runtime/workflow_scheduler.rs`. That makes a failed scheduled delivery
diagnosable in the host's stdout, which is not the same as operator-visible.
Surfacing those outcomes is issue #228; the durable record it needs is the
first-class `Run` tracked by issue #242.

That distinction decides what the scheduler's log line may say. Every row
carries two reasons: `detail`, the free text the run response and the console
render, and `reason`, a closed set (`DeliveryReason`). Only `reason` is logged.
On the transport-failure arms `detail` interpolates the transport's own reply,
and a mail transport quotes the mailbox it refused — so `detail` on host stdout
would put a recipient's address on a platform surface. `reason` says what class
of thing failed and has no field that could carry the address (issue #248).

Authoring a destination and reading the result back:

Both routes go through `ScopedCompany`, so both need an operator credential —
`$TOKEN` below is the bearer token the `Authorization` header is parsed from.

```bash
# Create a graph whose output node reports to the company's admins.
curl -X POST "$HOST/api/v1/company/workflows" \
     -H "Authorization: Bearer $TOKEN" \
     -H 'content-type: application/json' -d '{
  "id": "weekly_digest",
  "name": "Weekly digest",
  "nodes": [
    { "id": "start", "kind": "trigger", "name": "Monday 09:00", "schedule": "0 9 * * MON" },
    { "id": "write", "kind": "agent",  "name": "Draft it", "agent": "chief_of_staff" },
    { "id": "done",  "kind": "output", "name": "Owner summary",
      "destination": { "kind": "owner" } }
  ],
  "edges": [ { "from": "start", "to": "write" }, { "from": "write", "to": "done" } ]
}'

# Run it now. `deliveries` says what happened to the report.
curl -X POST "$HOST/api/v1/company/workflows/weekly_digest/run" \
     -H "Authorization: Bearer $TOKEN" \
     -H 'content-type: application/json' -d '{"input":{"request":"last week"}}'
```

```jsonc
{
  "output": { "nodes": { "done": { "items": [ { "json": { "text": "…" } } ] } } },
  "pendingApprovals": [],
  "deliveries": [
    { "node": "done", "kind": "owner", "target": "ada@acme.test",
      "status": "sent", "detail": "emailed the company's admin" }
  ]
}
```

Swap `{ "kind": "owner" }` for `{ "kind": "email", "target": "ada@example.com" }`
and a recipient who has never written in comes back as
`"status": "skipped"` with the reason, having sent nothing:

```jsonc
{ "node": "done", "kind": "email", "target": "ada@example.com",
  "status": "skipped",
  "detail": "this recipient has never written to the company, so a workflow may not open the conversation — send once from the inbox first" }
```

The gating is fail-closed and differs per kind. `owner` resolves server-side to
the company's active admins (the graph names nobody) and falls back to the
`operator` channel. `channel` must name an adapter the deployment already
wired. `email` is the only kind that can address an outsider, and it needs
**both** an `email` grant in the manifest's `[tools].allow` **and** an
established inbound thread from that address — the same rule the agent send
path applies; a cold recipient is skipped and reported, never mailed. Note the
grant half is satisfied by default: since #230 an unset `[tools].allow` defaults
to `["*", "media", "composio"]` and `*` covers `email`, so on a
default-configured company the established-thread rule is the gate actually
holding the line. Narrow `[tools].allow` explicitly to close the first one.

Every credential-shaped value written here lands in the `SecretStore`; the
responses expose only non-secret status. The networked seams (DNS, SMTP, OAuth
exchange) are dependency-inverted behind traits carried on `ConnectionsRuntime`
and default to empty (offline) — a surface whose seam is absent returns
`404 {"code":"not_wired"}`, which the console degrades gracefully.

### Connections: hosted versus the self-hosted hatch

`ops::connections` (feature `oauth`) runs OAuth with **this host's own provider
application** — a client id/secret an operator registered themselves and handed
to the process as `OPENCOMPANY_OAUTH_<PROVIDER>_ID` / `_SECRET`. That is the
only way a standalone checkout can complete a handshake, and it is supported for
exactly that reason. It is a hatch, not a deployment mode — the same framing
`ops::composio` uses for its BYO token. A hosted tenant is injected no
`OPENCOMPANY_OAUTH_*` variable at all, so on that host `provider_config`
resolves nothing and a local Connect can only fail.

The read plane says which it is. `ops::connections_read::connect_route` answers
one question per provider — *can a Connect click possibly succeed here, and by
which route?* — as a `credentialSource` tier, stored-wins:

| Tier | When | Console |
| --- | --- | --- |
| `static` | a token is already stored for this provider (BYO override), **or** this host registered its own provider app *and* has a state signing secret (the hatch) | Connect button, as today |
| `attested` | no stored token, and the pod carries a platform-**projected** identity (`TINYHUMANS_TOKEN_FILE` naming a file that exists) | "Managed by the platform", no local Connect |
| `none` | neither | read-only "not available on this host" |

**The hatch also needs `OPENCOMPANY_OAUTH_STATE_SECRET`** (issue #318). The
`state` nonce binds an in-flight authorization to one company, provider and
expiry, and the callback verifies it before exchanging the code — it is the
flow's CSRF defence. That signing key used to fall back to a literal baked into
this repository, which made the value public, identical across every
unconfigured deployment, and constructible rather than obtainable: verifying it
proved only that it was well-formed. There is now **no default**. A host with a
registered provider application but no secret reports `none` rather than
offering a button whose check is void, `start` refuses with a message naming the
variable, and the process logs the misconfiguration once — a tile has no room to
name a variable, and an operator reads logs. Whitespace-only counts as unset, so
an empty shell expansion gets the closed door rather than a secret of `" "`.

`attested` deliberately requires the projected-file tier, not
`TinyhumansTokenSource::from_env` as a whole: that resolver also accepts a
long-lived `TINYHUMANS_API_KEY`, which a self-hoster commonly sets to buy
inference. Accepting it here would tell such an operator their working Connect
button is platform-managed and take it away. Both the REST route and the GraphQL
`Company.connections` resolver project the field through the same
`connect_route_from_env`, so the two read shapes cannot drift.

**Provider mapping to the platform backend.** Its registered OAuth providers are
`notion`, `google`, `gmail`, `github`, `twitter`, `discord` and `instagram`. Two
consequences for the console catalog: `gmail` is a registered provider *name* but
not a separate provider application — it is Google's app requested with the Gmail
skill scopes, so a Gmail connect and a Google connect share one grant (which is
why the backend merges scopes incrementally rather than replacing them). And
there is **no Slack provider** at all (the backend's only Slack credential is an
internal alerting bot), so Slack has no hosted route except Composio, which runs
its own OAuth.

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

## Inbound channel webhooks (`hooks.rs`)

`POST /hooks/{company}/telegram` is the **optional hosted fast-path** for
Telegram inbound, not the default. Telegram can only deliver to it from the
public internet, so it is surfaced only when `OPENCOMPANY_PUBLIC_URL` is a
public **https** URL (`AppConfig::public_webhook_base_url`); otherwise
`GET …/channels/telegram` reports `webhookUrl: null` and
`POST …/channels/telegram/webhook` is refused with `400`, rather than handing an
operator a `http://127.0.0.1:<port>/hooks/…` URL that can never receive a
delivery. Everywhere else — local and most self-hosted deployments — inbound
arrives over `getUpdates` long-polling
([`runtime::telegram_poller`](../runtime/README.md#background-listeners)) and a
bot token is the whole setup: no webhook secret, no `setWebhook`, no public URL.
