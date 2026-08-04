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

## Background listeners

Two per-company background loops sit beside the scheduler, both spawned in
`serve` and stopped by the same shutdown `Notify`:

- `mailbox_poller.rs` — the IMAP mailbox poll (feature `imap`), on a fixed
  interval (`OPENCOMPANY_MAIL_POLL_SECONDS`, default `60`).
- `telegram_poller.rs` — Telegram `getUpdates` long-polling (feature
  `telegram`), the inbound path that **needs no public URL**. It dials out to
  `api.telegram.org`, so it works on localhost, behind NAT, and on any
  self-hosted box — where Telegram's servers can never reach an inbound
  `/hooks/{company}/telegram` route. Setup is the bot token alone; the loop
  idles until one is stored and picks up a token pasted into the console on its
  next tick, with no restart. Long-poll hold and idle back-off are
  `OPENCOMPANY_TELEGRAM_POLL_SECONDS` (default `30`).

The webhook route (`server::hooks`) stays as an optional hosted fast-path, and
is offered only when `OPENCOMPANY_PUBLIC_URL` is a public **https** URL. The two
paths never both consume an update: Telegram refuses `getUpdates` while a
webhook is registered, so the poller checks `getWebhookInfo` first and stands by
on a publicly reachable host — while on a host with no public URL a registered
webhook can only be a dead endpoint, so it clears it and takes inbound back.
Both paths run the same turn and share `telegram::deliver_replies`, so which one
delivered an update is invisible downstream.

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

The builder threads that same `WorkspaceStore` handle onto `HarnessDeps`
(`workspace`), so agents read the operator's note tree through the tools in
`harness::workspace_tools` (issue #237) rather than being blind to it. One
handle, three writers — console REST, GraphQL, and a granted agent — so an
operator edit is what the next turn reads, with no rebuild. `None` fails
closed: no workspace tools are wired.
