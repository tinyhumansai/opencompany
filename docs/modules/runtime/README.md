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
