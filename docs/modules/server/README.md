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
| `tasks` | `POST …/tasks`, `PATCH`/`DELETE …/tasks/{id}`, `GET …/tasks/{id}` (the Task Detail read, #185) |
| `task_export` | `GET …/tasks/{id}/export` (the task's record as a document, #352) |
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

### Exporting a task's record (issue #352)

`GET …/tasks/{taskId}/export` answers **one self-contained HTML file** carrying
what the Task Detail screen shows: the header and the worked/waiting split, the
whole ordered timeline with its details expanded, the text of every artifact
revision and the human-edit diff, and the neighbouring cards. `Content-Type:
text/html`, `Content-Disposition: attachment`, so `curl -OJ` lands a named file
and the console's Export button downloads one.

**Why HTML rather than Markdown or PDF.** The bar in epic #184 is "a
non-technical person can read it unaided", which already rules out the JSON the
screen fetches. HTML opens by double-click on any machine with no reader
installed and nothing to explain; it keeps the **proportional waiting bands**
(#305) that are the entire point of the worked/waiting work — a four-hour wait
must not look like a four-second one, which is exactly what a text format
flattens; and the reader's own browser prints it to the PDF a client asks for by
name. It also costs **no new dependency**: the document is `format!` plus
inlined CSS, no templating engine, no PDF crate, no headless browser. Markdown
was rejected because it loses the proportions and, opened in a text editor by
the non-technical reader this exists for, shows raw `##` and `|`. PDF was
rejected because generating one means a new rendering stack for an artifact the
browser already produces from this file.

**Why the server renders it.** One implementation serves the console button and
an automated caller alike, so an audit or scheduled export needs no second
renderer. A client-side document would live only inside the React view — the
same place the record is stuck today.

**Redaction is structural, not procedural.** The handler renders
`tasks::assemble_detail` and `artifacts::artifacts_for_task` — the *same values*
`GET …/tasks/{id}` and the Artifacts tab return. It never reads the event log
itself, so there is no second path whose scrubbing could drift from the
console's; `detail` text is scrubbed at source before either caller sees it.
Everything interpolated is HTML-escaped, so a card titled `<script>…</script>`
renders as text in a file that will be opened in a browser and forwarded on.

**Exporting is a pure read** — no journal entry, no column change, no state on
the task. A test compares the board rows and the journal length across the call,
because an audit export that modifies what it audits is worse than none.

**One figure, computed once.** The worked/waiting split (#305) is computed
host-side in `TaskDurations` and carried on `TaskDetail`, so the console and the
exported record read the same numbers instead of deriving them separately. It
used to be a hand-maintained mirror between `task_export.rs` and
`frontend/src/views/TaskDetailView.tsx`: they agreed, but nothing failed if they
stopped, and the failure mode is an exported record disagreeing with the screen
about how long a person was waited on. `TaskDurations` carries the totals plus
`workedLive` / `waitingLive` and `asOfMillis`; a caller wanting a ticking figure
adds `now - asOfMillis` to the live half, which is exact because every closed
span already ended before `asOfMillis`. The waiting *band height* stays mirrored
— it is presentation, and a drifted pixel curve misleads nobody about a fact.

**No sign-offs section, until #333 lands.** #352 deferred it explicitly: until an
approval carries a task id, the only sign-offs attributable to a card are the
resolutions that fell inside its run window, which is a correlation rather than a
link. An imprecise attribution is least acceptable in the one artifact that goes
to a client or an auditor, so the document omits the section rather than printing
it with a caveat. When #333 (PR #349) merges, `TaskDetail` gains an `approvals[]`
keyed by a real id and the section lands reading that — including the pending
sign-offs that have no resolution event and so cannot appear at all today.

**Bounded output.** The document is one `String` in one response, and it prints
every revision of every artifact, so a long editing history scales it. Each
revision body is capped (`MAX_BODY_CHARS`) and each human-edit diff is capped
(`MAX_DIFF_LINES`); both cuts are announced in the document, because a reader
must never be left believing they hold the whole text when they do not.

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
