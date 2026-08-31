# Workflow authoring, editing and delivery (issues #262, #259, #228)

This page holds the workflow **route** documentation that outgrew the server
module's README: the cron preview (`#262`), the `PUT`/`DELETE` authoring round
trip (`#259`), and run-time report delivery (`#228`). The pause switch and the
disarm rule are on their own page — [pausing-workflows.md](pausing-workflows.md).

## Reading a trigger's cron back (issue #262)

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

## Editing and removing a workflow (issue #259)

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

**The `ownerDesk` field (issue #1862).** `GET` returns `ownerDesk` (camelCase)
alongside `version`; on disk it is `owner_desk` in the workflow's TOML. `POST`
and `PUT` accept the same field in the body. There is no console control to set
or change it today — the create/edit dialog only carries forward whatever a
previous read hydrated it with — so an API caller is currently the only way to
assign or move one. Because `PUT` replaces the graph wholesale, **`ownerDesk`
must be echoed back exactly like `version`**; omitting it on an edit clears the
desk assignment rather than leaving it untouched.

A stored `ownerDesk` can stop resolving after the desk it names is renamed or
removed. The `GET` path already tolerates that — a saved graph must still load
— and `PUT` now grandfathers it too: a desk that is both unresolvable *and*
unchanged from what was already stored does not block the save, so editing
some other field is never refused by desk drift the console gave the operator
no way to fix. A **newly typed or selected** desk that fails to resolve is
still a validation error.

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
# → { "id": "weekly_digest", …, "ownerDesk": "engineering", "editable": true,
#     "version": "73e8ccc6…" }

# Correct the schedule, conditional on nothing having changed since that read.
# `ownerDesk` is echoed back verbatim from the read above — a PUT that drops
# it clears the desk assignment instead of leaving it alone.
curl -X PUT "$HOST/api/v1/company/workflows/weekly_digest" \
     -H "Authorization: Bearer $TOKEN" \
     -H 'content-type: application/json' -d '{
  "id": "weekly_digest",
  "name": "Weekly digest",
  "ownerDesk": "engineering",
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

**Deliberately not in #259**, each its own follow-up: revision history and
rollback (OpenHuman keeps a bounded snapshot ring in a dedicated table; our
overlay bodies live inside `CompanyRecord`, which is saved whole on every write,
so a ring needs its own store surface) and journal retention (see above).
Enable/disable and disarm-on-edit landed separately, as issue #276 — see
[pausing-workflows.md](pausing-workflows.md).

## Workflow runs and report delivery (issue #228)

A workflow's terminal `output` node may carry a `destination` — `owner`,
`email`, or `channel` — saying where its report goes once the run finishes. It
rides the create body and the read shape under the same key, and the model
type is reused verbatim in both directions (`kind` / `target` are single words,
so there is no camelCase mirror to drift from).

### Paging the run history (issue #1012)

`GET …/workflows/runs` is paged: `?limit=` counts **runs** (not journal rows),
`hasMore` says whether an older page exists, and `nextBeforeSeq` is the cursor
to pass back as `?before_seq=`. The page is cut by `seq` and displayed by
`(atMillis, seq)` — two different keys, because `atMillis` is wall-clock and
cutting on it loses runs when the clock steps backwards. Clients must **not**
derive the cursor. Full contract, the partition argument, and the
version-skew fallback: [run-history-paging.md](run-history-paging.md).

### Destinations that can never deliver are refused at save (issue #981)

Two of delivery's refusals are decided by facts that hold for *every* run of the
graph, so a save that names one is a graph guaranteed to drop its report. Both
are refused with a `400` when the workflow is written, not only when it runs:

| Destination | Refused when | Checked in |
| --- | --- | --- |
| `channel` | the target is not one this **running company** can deliver to — `CompanyRuntime::deliverable_channel_ids()`, which is also what `GET …/workflows/wired-channels` serves the console's picker. Desk channels and enabled OpenHuman-provider manifest channels, plus the always-present `operator` channel (issue #1757) — now a durable, journal-backed destination, so the console offers it like any other rather than excluding it | `validate_draft_against_record` in `src/company/workflow_create.rs` (issue #1191), reading the deliverable set the caller passes in. Both write routes pass it; so does the proposal-apply path. The agent tool surfaces pass `None` (no runtime handle) and skip the rule |
| `email` | this company's `[tools].allow` does not grant `email`, which delivery answers with `Denied` / `EmailNotGranted` before it even looks for a mailbox | `validate_draft_against_record` in `src/company/workflow_create.rs`, beside the `tool_call` grant gate — so the orchestrator's `create_workflow` tool is held to it too |

The `operator` destination above is a delivery-plane fact only — it is unrelated
to how the console *lists* the feed. `GET {scope}/desks` carries zero operator
logic: it is the company's real desks (manifest `[[group_chat]]`s plus
operator-created overlay desks) and nothing else. The Operator feed's identity
— its id (ordinarily `operator`, or the collision-fallback id for the one
grandfathered company shape where a roster teammate already owns that id — see
`CompanyRecord::operator_feed_channel`), name, and description — is served by
its own read-only endpoint, `GET {scope}/operator-channel`, which the console
renders as a pinned row below a divider in the chat rail rather than folding
into the desk list (issue #1757 rework, replacing an earlier synthetic-desk
approach that collided with #1762's `#general`).

The `channel` rule lived on the two write routes until issue #1191, which is
why applying a copilot proposal persisted a graph the editor then refused to
save back — the apply path is a save that never ran it. It now lives in the
shared authoring core beside its `email` sibling, inside the `problems`
accumulator, so the refusal is a `workflow_invalid` naming the node and the
`destination.target` field rather than a bare `invalid_request` with no
breakdown. The deliverable set is still a runtime fact: it is threaded in from
the caller as `Option<&[String]>`, the same way #1046 threads `mail_configured`.

Both are guards, not guarantees. Desks come and go and grants can be revoked, so
a graph valid at save can be invalid at run, and delivery's own refusal stays the
backstop — as it must for seed and legacy graphs, which never pass through the
create path at all. Neither check runs at TOML parse time: a seed template is
parsed with no runtime in hand, and checking there would refuse to boot it on a
company whose desks are not resolved yet.

Deliberately **not** refused at save: a wired mailbox and an established inbound
thread with an `email` recipient. Those are per-run, per-recipient conditions an
author-time check cannot see, and refusing on them would refuse graphs that work.
Arming a *schedule* does check the mailbox lever (issue #1046) — see
`UNDELIVERABLE_SCHEDULE_REFUSAL` — because a scheduled run has no reader to
notice the dropped report.

Delivery itself is **not** a route concern. It runs host-side in the shared
`WorkflowRunner` path (`src/workflows/delivery.rs`) once the engine returns,
because the orchestrator's `run_workflow` tool and the trigger scheduler drive
that same port — and a scheduled run is exactly the case where nobody is
watching the console. An **on-demand** run's response therefore carries
`deliveries`: one row per attempt (`sent` / `skipped` / `denied` / `failed`)
with an operator-readable reason. A delivery failure never fails the run, so
that list is where an operator learns *why* a report did not go out; an
unwired runtime writes a loud `failed` row rather than skipping silently.

### Every run carries a `verdict` (issue #981)

The rows say *why* a report did not go out. **`verdict` says what the run adds
up to** — one word, on both run DTOs, always serialized. See
[run-verdict.md](run-verdict.md) for the word list, the precedence order, and
why an undelivered report is its own reading rather than a failure.

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
  "verdict": "ok",
  "deliveries": [
    { "node": "done", "kind": "owner", "target": "ada@acme.test",
      "status": "sent", "detail": "emailed the company's admin" }
  ]
}
```

### Structured validation errors (issue #1016)

A `POST`/`PUT …/workflows` whose graph fails author-time validation answers
`400 { "code": "workflow_invalid" }` with an **additive** `problems` array — one
entry per fault, each naming the node and config field so the console can
highlight the exact spot. The `error` string stays the joined human sentence, so
older string-only clients are unaffected; only this one code carries `problems`.

```jsonc
{
  "error": "node `greet` has a `config.url` of `not-a-url` that is not a valid URL — …",
  "code": "workflow_invalid",
  "problems": [
    { "node_id": "greet", "field": "config.url", "message": "node `greet` has a `config.url` …" }
  ]
}
```

The author-time config gate (issue #661, extended #1016) now also requires a
`transform` to carry a non-empty `config.set` (a table of expression strings), a
`split_out` to carry `config.path`, and an `http_request` `config.url` to be a
real `http(s)` URL with a host (a bare `not-a-url` / `ftp://…` is refused at save
instead of failing at run). An `output_parser` stays schema-optional (a bare
identity parser is valid) and `merge` stays config-free; a `sub_workflow` whose
`workflow_id` names no saved workflow this company can resolve is refused too.

### A run that stopped for a person (issues #881, #880)

When a tool call inside an agent node's turn is parked for approval, the node
produces no deliverable and its branch stops. The run still answers `200` — a
step waiting on a person is not a failure — and says so structurally:

```jsonc
{
  "output": { "nodes": { "draft": { "items": [ { "json": { "text": "…" } } ] } } },
  "pendingApprovals": ["spec"],
  "verdict": "blocked",
  "deliveries": [],
  "nodes": [ { "nodeId": "spec", "status": "blocked", "elapsedMs": 42000 } ],
  "blockedNodes": [
    { "nodeId": "spec", "tools": ["publish_artifact"], "approvalIds": ["appr-1"] }
  ],
  "approvals": [
    { "nodeId": "spec", "tool": "publish_artifact",
      "outcome": "parked", "approvalId": "appr-1" }
  ]
}
```

`output` carries what the nodes **upstream** of the block produced (issue
#1008). It used to be `null`, so the run drawer showed nothing for a step that
had just written a draft.

The **blocked node itself has no entry**, here or in the durable snapshot behind
`GET …/workflows/runs/{runId}/output`. A node refused inside its model's tool
loop ends its turn by writing prose about being blocked, and filing that prose
as the node's product would re-open, one surface over, exactly the confusion
issue #881 fixed by stopping it reaching the next node. The node's `blocked`
chip and the run's notice are what say what happened.

`blockedNodes` and `approvals` are omitted entirely when empty, so a run that
blocked on nobody is byte-unchanged. Both also ride the `WorkflowRunFinished`
journal event, `GET …/workflows/runs` (where each blocked node's chip is
relabelled from the `error` the engine reported) and the SSE
`workflow_run_finished` frame — one shape on all four surfaces.

`approvals` is a **receipt of what the run parked**, not a live count of what is
outstanding: nothing flips a row when the operator approves. Its `outcome` is
`parked`, `parkFailed` (the store refused the write, or no approvals queue is
wired — **nobody will be asked about that call**) or `discarded` (the turn gated
more calls than the per-turn cap allows; the drain drops the excess, so such a
row carries no `tool`).

**Approving does not continue the run.** An agent node is not re-enterable, so
the operator decides the card and runs the workflow again. See
[`workflow-vocabulary.md`](../../spec/runtime/workflow-vocabulary.md) for why.

### The node a run is executing right now (issue #1010)

`GET …/workflows/runs` folds **both** node brackets, not just the finish. A run
still in flight carries `startedNodes` — the ids it has begun, in start order —
beside the `nodes` rows it has finished:

```jsonc
{
  "runId": "run-live",
  "running": true,
  "startedNodes": ["collect", "draft"],
  "nodes": [ { "nodeId": "collect", "status": "ok", "elapsedMs": 12000 } ]
}
```

Started minus finished is the node executing right now — here, `draft`. Before
this the fold read `WorkflowNodeStarted` (issue #382) nowhere, so the only
per-node facts the history carried were finishes, and every console that learned
about a run from the journal rather than from a live SSE start frame — a reload,
a cron fire, an `EventSource` reconnect, a workflow switch and back — painted the
graph with a hole exactly where the work was happening.

`startedNodes` is a **receipt of what started** and deliberately survives the
finish: an id in it with no matching `nodes` row on a settled run is the node the
run was standing on when it was cancelled or lost, which neither list says on its
own. Readers must therefore pair it with `running` before painting anything as
in flight, or a settled run overlays a spinner nothing can clear. Omitted
entirely when empty, like `nodes`, so a run journaled before #382 is
byte-unchanged.

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
to the globals `default_allow` (which opens with `*`) and `*` covers `email`, so on a
default-configured company the established-thread rule is the gate actually
holding the line. Narrow `[tools].allow` explicitly to close the first one.

Every credential-shaped value written here lands in the `SecretStore`; the
responses expose only non-secret status. The networked seams (DNS, SMTP, OAuth
exchange) are dependency-inverted behind traits carried on `ConnectionsRuntime`
and default to empty (offline) — a surface whose seam is absent returns
`404 {"code":"not_wired"}`, which the console degrades gracefully.

### The files a run produced (issue #1684)

`GET …/workflows/runs/{rid}/artifacts` answers the deliverables one past run
made, for the run inspector's "Files associated" section:

```jsonc
{
  "files": [
    { "taskId": "t_a", "artifactId": "art_a1", "title": "Launch spec",
      "kind": "markdown", "source": "specs/launch.md", "latestVersion": 2,
      "updatedAtMillis": 1717000000000, "workspaceNodeId": "node_9",
      "taskTitle": "Draft the launch" }
  ],
  "truncated": false
}
```

There is no direct "artifacts by run" index — `ArtifactVersion.run_id` is the
task **attempt** id, not the workflow run id. The authoritative link is the
card's `origin_run_id`, the run that **opened** the card, so the route joins
`run_id → cards where origin_run_id == run_id → each card's artifacts`, reading
the two broad list primitives (`TaskStore::list` once, then `ArtifactStore::list`
per matched card) and filtering in memory. It is **metadata only** — never the
artifact body — and each row is enough for the console to deep-link the file into
its card's Artifacts tab at `latestVersion` (and, when `workspaceNodeId` is set,
to `#/workspace/<id>`).

Like `GET …/workflows/runs/{rid}/output` it is a **lazy per-run fetch**, NOT
folded into `GET …/workflows/runs` (that fold is already expensive, and an
inspector opens one run at a time). The one contract difference from `output`:
**a run with no files answers `200 { files: [], truncated: false }`, never
`404`** — a run that opened no cards, or cards that published nothing, is the
common case, not an error. `truncated` flips to `true` only when a run's file
count passes the host's defensive cap (`MAX_RUN_ARTIFACTS`), which the console
labels "newest files shown" rather than presenting an incomplete list as
exhaustive.

Provenance is the **opening** run: `origin_run_id` is stamped once, at card
creation, so a card re-owned by a later run still lists its files under the run
that opened it; a `sub_workflow` child stamps its parent run, so sub-workflow
cards roll up to the parent. A legacy record with no `source` (a pre-#244
auto-captured chat reply) is still returned, with `source` omitted, so the
console labels it rather than the history silently dropping it.

## Authoring a workflow — copilot & task-card proposals (issues #580, #753, #783, #874)

Building a graph from a task card, drafting one from a free-text description,
and grounding either on the tools a company can actually reach have their own
focused page: [workflow-authoring-routes.md](workflow-authoring-routes.md).
