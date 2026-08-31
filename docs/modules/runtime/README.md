# Runtime Module

The runtime module assembles the kernel and drives it. `CompanyRuntime` is the
port bundle from [`docs/spec/runtime/ports.md`](../../spec/runtime/ports.md)
(brain, stores, tools, channels, approvals), built by `RuntimeBuilder` with
file-based defaults. `CycleRunner` implements the serial-per-company cycle from
[`docs/spec/runtime/lifecycle.md`](../../spec/runtime/lifecycle.md):
drain → load → think (`Brain::run_cycle`) → gate (`ApprovalGate`) → persist.

Effects are journaled before execution and marked after, so replay never
re-fires a completed effect (at-most-once). `CompanyRegistry` maps `CompanyId`
to a running runtime, serving both the single-company and multi-tenant cases
with one type. Approval resolution schedules a follow-up cycle so the brain
learns the verdict.

`cron.rs` + `scheduler.rs` implement the cron scheduler: each manifest
`[[schedule]]` 5-field expression is matched against an injectable clock, and a
`ScheduleFired` event is enqueued into the company's serial cycle queue when
due. The clock is injectable so schedule firing is tested deterministically
without wall-clock waits.

`maintenance.rs` is the third minute loop, and it is deliberately not a cron at
all: `MaintenanceTicker` retires overdue approvals, unredeemed grants and stale
fire claims for **every** registered company, once a minute (issue #971). One
process-wide task over `CompanyRegistry`, shaped like `workflow_scheduler.rs`
and for the same reason — a company can be registered after boot.

That shape *is* the fix. Maintenance used to ride `scheduler.rs`'s loop, which
is only spawned for a company whose manifest declares a `[[schedule]]` — so a
company with none never swept anything, at any age, and a tenant driven entirely
by workflow schedules accumulated approvals for days. Anything that must happen
for every company hangs off the registry, never off a per-company spawn.

Two things differ from the schedulers beside it. A **paused** company is still
swept: this finishes work rather than starting it, and a paused company's queue
is the one most likely to be full of requests nobody will answer. And the sweep
is **capped per tick, oldest first**, so the first pass after a shortened
deadline drains a backlog over a few minutes instead of bursting. Enforcement is
not here — the gate re-checks the deadline under the lock that removes a parked
entry, so an overdue approval default-denies on the operator's click whether or
not this ever ran; what this adds is that the queue empties and the badge goes
back to describing current state. `CompanyScheduler::tick_maintenance` remains
as a thin delegate over the same `sweep_company`, so the two callers cannot
drift.

`workflow_scheduler.rs` drives the *other* kind of cron: the `schedule` a saved
workflow graph's `trigger` node carries (issue #169). Same `CronExpr` matcher,
same injectable `Clock`, same minute-boundary loop — but **one process-wide
task**, not one per company, because workflow schedules are runtime data
(creating a workflow in the console adds a cron with no reboot, and a hosted
tenant can be registered after boot). Each tick re-reads `CompanyRegistry`,
skips companies that aren't `running` or have no `WorkflowRunner` wired, and
enumerates graphs through the seed ∪ overlay union
(`list_workflows_union`) so console-created workflows — which exist only as
record overlays — are scheduled too. A matching minute fires the workflow on its
own tokio task (a scheduled run is a *new* causal chain at `WORKFLOW_DEPTH` 0,
exactly like an operator clicking Run) with an in-flight guard per
`(company, workflow)` so a slow run never overlaps itself — held as an RAII
guard, so a run that panics releases its slot on the unwind instead of retiring
that schedule for the life of the process. Missed runs are skipped, never caught
up. All cron times are UTC, and a graph may carry at most one scheduled trigger
(validated in `workflow_file.rs`).

**When a company has schedules but no runner**, the scheduler says so once. This
is the default build's inert seam, but it is *also* what a configured build looks
like when its inference source fails to resolve at boot — in which case a saved
schedule silently never fires and looks identical to a working one. The warning
is latched per company rather than emitted per tick (once a minute forever would
be ~1440 lines a day per tenant, burying the signal it raises), and re-armed on
either state change: a runner appearing, or the company's scheduled-workflow
count dropping to zero — so a schedule saved later onto a still-unwired company
is reported rather than swallowed by the latch. A company with no scheduled
workflows and no runner is not misconfigured and stays silent.

`workflow_spawn.rs` owns the two things every entry point that *starts* a
workflow run owes it: minting the run id through the `RunSupervisor` (so the id
the console correlates SSE frames on is also the address a cancel is sent to),
and journalling the outcome through `record_run_finished` on **both** arms with
the `RunGuard` held across the write. The console run route and the
approved-gate resume arm both go through `WorkflowSpawn`, so they cannot drift.
It holds four cloned handles rather than an `Arc<CompanyRuntime>`, which is what
lets a caller with only a `&CompanyRuntime` start a run. The cron scheduler
above keeps its own spawn body — it wraps the same two steps in a schedule claim
and a per-delivery log sweep that only make sense for a fire nobody is watching.

`workflow_resume.rs` is what approving a paused `requires_approval` node
actually does (issue #395). The engine **settles** a paused run rather than
suspending it, so there is nothing to resume: the module reads the workflow id,
node id and trigger input off the parked `workflow.approve` effect, unions the
gate id into the input's `approvals` array, and starts a fresh supervised run.
That makes it restart-durable for free — the parked card is self-contained, so
journal replay is all a continuation needs — at the documented cost that
upstream nodes re-execute. See
[`docs/spec/company-brain/approvals.md`](../../spec/company-brain/approvals.md).

A blocked *agent node's* gated tool call (issue #1816/#1825) is a different
shape: the parked effect carries no workflow lineage, so
`workflow_resume.rs::spawn_blocked_node_continuation` reads the run id and
trigger input back out of the dedicated `BlockedNodeStashed` journal record
instead, once the last decision on that turn lands. `CompanyRuntime`'s
`reconcile_stranded_blocked_nodes`, run once at boot from `builder.rs`, is that
path's own restart recovery: it re-dispatches any stash the journal shows as
decided but never spawned (or spawned but never retired), and retires a stash
that resolved with nothing approved — the two ways a crash can land between the
decision and this module's write. See [journal.md](../../spec/runtime/journal.md)
for the record kinds involved.

## Background listeners

One per-company background loop sits beside the scheduler, spawned in `serve`
and stopped by the same shutdown `Notify`:

- `mailbox_poller.rs` — the IMAP mailbox poll (feature `imap`), on a fixed
  interval (`OPENCOMPANY_MAIL_POLL_SECONDS`, default `60`).

A Telegram `getUpdates` poller and its `/hooks/{company}/telegram` webhook
fast-path used to sit here. Both are gone with the channel itself: one messaging
vendor's inbound path, its bot token, its webhook secret and its two mutually
exclusive delivery modes were a standing surface to keep working for a channel
that was never the product. Email (IMAP/SMTP) and the console remain the ways in.

## Harness pool (`src/harness/`, feature `openhuman`)

`src/harness/` embeds `openhuman_core` as a library (see
[`docs/modules/openhuman/README.md`](../openhuman/README.md)). `HarnessPool`
builds one openhuman `Agent` per manifest `[[agent]]` through `AgentBuilder`
(`build.rs`), wiring memory (`memory.rs`, an openhuman `Memory` over the
`ContextStore`), the hosted-Medulla inference provider (`provider.rs`, with a
`MockProvider` for tests), and the approval policy (`policy.rs`, `[policy].mode`
→ openhuman `ToolPolicy`). The default build links none of it.

`HarnessPool::run` maps a completed turn's cost (`cost.rs`, `TurnCost` →
ledger + `UsageMeter`). **Partial:** openhuman exposes turn usage only through
a `pub(crate)` accessor, so until the upstream public accessor
(tinyhumansai/openhuman#4940) lands, `run` records a **zero-usage** turn; the
mapping itself is complete and tested. Group-chat/desk routing is single-
responder in v1 — the full desk-resolving `chat` handler and approval resume
live in the WS3 chat handler, not the harness.

## Metering (`src/metering/`)

`src/metering/` holds pure, I/O-free projections that back the Usage and
Finances views: `bucket_usage` folds `UsageSample`s into the daily token
series, tokens-by-teammate, calls-by-provider, and totals over a 7/30/90-day
range; `finances_from` projects the ledger + `[budget]` + optional wallet
balance into balance, budget-vs-spend, revenue, spend-by-category, and the
transaction journal. `roster_display_names` resolves teammate ids to prosumer
display names (manifest role, overridden by operator-overlay name). The
async-graphql wrappers live in `server::graphql`, not here.

## Store seeding

The workspace store seeds a new company from its `companies/<name>/workspace/**`
template on first use (`WorkspaceStore::is_empty` gates the seed); skills read
the company's `skills/<id>/SKILL.md` plus the repo-level shared registry.

Boot also scaffolds the reserved system roots `agents/` and `artifacts/`
(issues #551, #552), via
`company::workspace_scaffold::ensure_workspace_scaffold`. That call is gated on
"this is not a rebuild" and on **nothing else** — deliberately not on
`seed_dir`, since a provisioned tenant and the desktop build have no company
bundle to seed from and their workspace needs the same shape; deliberately not
on `is_empty`, since that gate exists to make operator deletions stick against
re-seeding, and an existing company only ever picks the root up on a later
boot; and deliberately not on the roster, since the root is part of what a
workspace is. It is idempotent, so it costs one tree read per boot.

The roots are created **empty** (`artifacts/` and `secrets/` each carry one
explanatory note). `agents/<agent-id>/`, `artifacts/<agent-id>/` and
`desks/<desk-id>/` are minted on demand by `ensure_agent_folder` /
`ensure_artifact_folder` / `ensure_desk_folder`, at the moment that agent or desk
first produces something — a folder per roster member
would fill the tree with empty directories for teammates who have done nothing.
The minters find-or-create the root they need, so they double as the repair
path if boot's fail-soft create ever misses. There is deliberately no
roster-rebuild seam: `HarnessPool::ensure` writes nothing to the workspace,
because a member folder is no longer a function of the roster.

`desks/` is **not** scaffolded (issue #645). It was until nothing turned out to
write into it: `ensure_desk_folder` still has no callers (#552's publish path
is the intended first producer), so every company carried an empty root
promising a feature it does not yet have. Because the minter already creates an
absent root on its way down, dropping it from `SYSTEM_ROOTS` was enough —
`desks/` now appears whole, root and member folder together, the first time a
desk actually produces something. Existing companies keep whatever `desks/`
they already have: the scaffold resolves only the names in `SYSTEM_ROOTS`, so
an unmanaged root is never inspected, deduplicated, warned about or removed.

The builder threads that same `WorkspaceStore` handle onto `HarnessDeps`
(`workspace`), so agents read and write the shared note tree through the tools
in `harness::workspace_tools` (issues #237, #551) rather than being blind to it.
Boot also provisions lowercase `secrets/README.md`; agent workspace tools omit
that whole subtree while console and operator APIs keep ordinary access. It is
for operator-only workspace notes, not credentials consumed by providers or
tools, which remain in the dedicated secret/connection stores.
One handle, three writers — console REST, GraphQL, and a granted agent — so an
operator edit is what the next turn reads, with no rebuild, and an agent's note
is in the tab the operator is already looking at. Each write records its author
(issue #326). `None` fails closed: no workspace tools are wired.
