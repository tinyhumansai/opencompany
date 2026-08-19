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

Delivery itself is **not** a route concern. It runs host-side in the shared
`WorkflowRunner` path (`src/workflows/delivery.rs`) once the engine returns,
because the orchestrator's `run_workflow` tool and the trigger scheduler drive
that same port — and a scheduled run is exactly the case where nobody is
watching the console. An **on-demand** run's response therefore carries
`deliveries`: one row per attempt (`sent` / `skipped` / `denied` / `failed`)
with an operator-readable reason. A delivery failure never fails the run, so on
that run the list is where an operator learns a report did not go out; an
unwired runtime writes a loud `failed` row rather than skipping silently.

A **scheduled** run is journaled too (issue #228): the same
`WorkflowRunFinished` record a manual run writes, with its `deliveries` rows,
folded into `GET …/workflows/runs` — so a failed scheduled delivery is as
operator-readable as a manual one. The scheduler's stdout log still exists, but
it is the platform team's diagnostic, not the operator surface: the run rows
carry the full `detail`, while the log never carries a field that could bear an
address (issue #248).

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

### A run that stopped for a person (issues #881, #880)

When a tool call inside an agent node's turn is parked for approval, the node
produces no deliverable and its branch stops. The run still answers `200` — a
step waiting on a person is not a failure — and says so structurally:

```jsonc
{
  "output": null,
  "pendingApprovals": ["spec"],
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
to `["*", "media", "composio"]` and `*` covers `email`, so on a
default-configured company the established-thread rule is the gate actually
holding the line. Narrow `[tools].allow` explicitly to close the first one.

Every credential-shaped value written here lands in the `SecretStore`; the
responses expose only non-secret status. The networked seams (DNS, SMTP, OAuth
exchange) are dependency-inverted behind traits carried on `ConnectionsRuntime`
and default to empty (offline) — a surface whose seam is absent returns
`404 {"code":"not_wired"}`, which the console degrades gracefully.
## Building a workflow from a task card (issue #580)

A board card marked `deliverable: "workflow"` does not dispatch to a teammate
when it enters In Progress — it builds a *reusable workflow* instead. The builder
pass (`src/harness/workflow_build.rs`) proposes a graph and lands the card **In
Review** with a `TaskWorkflowProposal`; the graph does not exist yet. Two task
routes finish the loop:

- `POST …/tasks/{id}/workflow-proposal/apply` rebuilds a `RawWorkflow` from the
  **stored** proposal `ops` (host authority — the browser's copy is never
  trusted) and runs it through the **same** `create_company_workflow` core this
  page's `POST …/workflows` uses, so a proposed graph passes exactly the checks a
  hand-authored one does — including #276's create-disarm for a scheduled graph.
  On success the card links to the created workflow (issue #339) and moves to
  Done; a refused create (roster drift, a name taken since) keeps the card In
  Review with the reason and returns a 400.
- `POST …/tasks/{id}/workflow-proposal/reject` clears the proposal and returns
  the card to To-do.

The full contract — the deliverable choice, the builder pass, and the
review-before-creation gate — is [workflow-build.md](../../spec/runtime/workflow-build.md).

## Drafting a workflow from a description (issue #753)

`POST …/workflows/draft-from-description` is the New-workflow dialog's copilot: it
turns a sentence into a graph the create form loads, so an operator can start
from a description instead of a blank form. It is the same engine as the #580
card builder (`draft_workflow_from_description` in `src/harness/workflow_build.rs`)
with the board card removed — the company evidence, the one tool-less model call,
and the host's authority over the id, the display name, the approval gating and
the node-kind vocabulary are identical. The one extra it grounds the model in is
the company's **effective tool slugs** (`workflow_effective_tool_slugs`), because
a typed description is far likelier to want a `tool_call` step than a card is.

**It never persists.** The draft is validated exactly as `POST …/workflows`
would (`courtesy_validate_draft`), handed back, and hydrated into the create
form; the operator reviews and edits it there and presses Create, which is still
the only call that saves a graph. So a bad draft costs a review, not a rollback,
and the review-before-creation discipline the card builder keeps is preserved
without a board card.

Like the cron preview, it answers **200 in both model-answer cases** — a drafted
graph, or an honest "this is better done once" — keyed by `automatable`:

```bash
curl -X POST "$HOST/api/v1/company/workflows/draft-from-description" \
     -H "Authorization: Bearer $TOKEN" \
     -H 'content-type: application/json' \
     -d '{"description":"Every Monday, have the writer draft the weekly digest and email the team."}'
```

```jsonc
{ "automatable": true,
  "summary": "Draft and email the weekly digest every Monday",
  "workflow": { "id": "weekly-digest", "name": "Weekly digest", "nodes": [ … ], "edges": [ … ] } }
```

```jsonc
{ "automatable": false,
  "reason": "this is better done once than built into a workflow: it names a one-time cleanup" }
```

An empty description is a `400`. A build with no embedded brain classifies the
gap the way the run route does — `not_wired` (404), `restart_required` or
`inference_required` (409) — so the console points the operator at the same next
step (a restart, or configuring inference in Settings) rather than a bare
failure. The spend is metered like a card pass, under a freshly minted id and a
`workflow:copilot` sentinel agent; there is no `RunStore` row, because a
synchronous request is not a card's attempt at its own work.

## Which tools a proposal may name (issues #783, #874)

`GET …/workflows/tool-slugs` is the browser-side copilot's tool grounding — the
`CopilotPanel` reads it once and inlines the answer in the message it composes,
so a proposed `tool_call` names a real slug instead of an invented one.

```jsonc
{ "slugs": ["shell", "read_workspace_state"],
  "unwired": [ { "slug": "web_search",
                 "reason": "searchBackendNotConfigured",
                 "detail": "granted, but no managed search backend is configured on this deployment; …" } ] }
```

`slugs` is the **effective** set — `workflow_effective_tool_slugs`: the catalogue,
the company's `[tools].allow`, and this deployment's wiring all agreeing. It is
the same set the in-process create/fix copilot grounds on, so the two surfaces
cannot drift.

`unwired` is the granted-but-unwired remainder, with the reason from the same
`WorkflowToolWiring` the run-time gate reads — `searchBackendNotConfigured` or
`capabilityTierFiltered`, matching the two sentences `refusal_for` produces at
run time. Reporting it, rather than dropping it, is what lets a reader tell "this
company is not allowed that tool" (absent from both lists) from "allowed, but
nobody configured the provider here".

That distinction is issue #874. The route used to answer the wider **grant-only**
set, so on a deployment with no search credential a granted `web_search` was
offered, the copilot authored a node on it, and the run failed at the first node
with `tool_call 'web_search' is not available in company workflows`.

Two deliberate non-changes:

- **Create/save validation stays permissive.** `validate_tool_call_node` still
  checks grants alone, so authoring a graph now and wiring the provider later
  remains legal. This route narrows what a caller is *told is available*, not
  what the host will *accept*.
- **Unknowable wiring is not "unwired".** With no harness deps attached the
  deployment cannot be asked, so `slugs` falls back to the grant-only set and
  `unwired` is empty — the pre-#874 answer. Claiming every granted tool is broken
  would be the worse failure. A default build (no `openhuman` feature) wires no
  `tool_call` grants at all and answers two empty lists rather than a 404, so the
  copilot grounds on "no tools" instead of being unable to tell.

A host predating #874 sends no `unwired` key; the client defaults it to `[]`,
which reads identically to a fully wired deployment.
