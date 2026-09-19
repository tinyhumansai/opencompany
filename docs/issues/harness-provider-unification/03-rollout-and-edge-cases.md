# Rollout and edge cases

## Desktop-only gating — already solved, do not touch

Covered in `01-what-already-works.md` §4. `can_run_local_acp()` already
gates whether `GET {scope}/harnesses` returns any `detected: true` rows, so
a hosted/server build's agent picker already shows zero CLI options today,
correctly, with no code change needed. The one rule for implementation: any
new UI added under this brief must derive "should I show a CLI-harness
readiness affordance at all" from the harness list this endpoint already
returns (an empty list of `detected` entries means don't show one), never
from a second, independently-computed check — the existing code comment on
`can_run_local_acp` names the exact bug two independently-stated copies of
this predicate caused before (issue reference in the comment itself,
`app/types.rs:795-808`).

## Readiness-check cadence

The probe (`acp/discovery.rs`'s `confirm()`) costs roughly 2 seconds per
harness — a real subprocess spawn plus a JSON-RPC round trip. Once this runs
in two places (the agent editor's picker, and the detail view reached from
it), a naive implementation could double-spawn if both are visited in quick
succession, or if two agent editors are opened for two different agents
bound to the same harness.

**Recommendation: match the standalone page's existing behavior — re-probe
per page load, no persistent cache — but de-duplicate concurrent calls for
the same harness id within one page's lifetime.** A simple in-memory map of
`harness id → in-flight promise`, cleared once that probe resolves, is
enough: it costs nothing when only one caller ever asks, and it prevents two
simultaneous callers (the picker rendering, and a "Manage" link opening the
detail view a moment later) from spawning the adapter twice for the same
answer. This is a small frontend addition, not a new backend mechanism, and
it is not blocking for a first version — flagging it because the standalone
page today doesn't need to worry about two callers at once, and the new
surfaces do.

Explicitly not recommended: a cross-page-load cache (e.g. in `localStorage`
or app state). That would reintroduce exactly the "second source of truth
that could disagree with the CLI actually being there" problem the
standalone page's whole design avoids (`docs/spec/runtime/
external-harnesses-ui.md`, already read this session — "there is no state in
between for a button to move it through").

## A pinned model the harness no longer advertises — already handled

Checked, holds: a stale-model edge case (an operator's pin outlives an
adapter update, or the model list hasn't loaded yet) is already handled by
`AgentDetailView.tsx`'s `unlistedModel` (line ~1893-1894) — a pin absent
from the live-fetched list is still offered rather than silently dropped.
No new work needed; see `01-what-already-works.md` §5 for the full citation.

## Multiple agents bound to the same harness

Already correctly handled server-side — `agents_on()` (`lanes.rs:178`)
already collects every agent (manifest and overlay) bound to a given harness
id, and `agent_models_on()` (line 221) already carries each one's individual
model override into the same lane's factory build. No backend change needed
for this case; it's already exercised by any company with more than one
agent on the same `[[harness]]` today. The only new thing the "Bound
teammates" list in the detail view needs is a way to read this from the UI
— see `02-the-real-gap-and-fix.md`'s two options for that.

## When a harness stops being ready after an agent is already bound

**Decision: purely live-checked, no proactive state needed for a first
version.** This matches the existing design's own philosophy exactly — the
standalone page never caches readiness, and a turn against an unready
harness already fails cleanly via `lanes.rs`'s `unavailable` mechanism
(`01-what-already-works.md` §3) with a specific reason, not a generic error.
Building a proactive "this agent's harness went offline" badge on the agent
list would require either polling (reintroducing the exact per-provider
polling cost the standalone page's own doc explicitly rejected — "one would
cost a request per provider per interval," said of a different but
structurally identical feature in `docs/modules/mcp.md`'s reasoning about
health polling) or a push mechanism nothing in this app currently has for
harness state specifically. Not worth building for a first version; note as
a possible follow-up only if real usage shows operators are confused by a
turn failing with no warning beforehand.

## Turn-time failure surfacing — unverified, needs checking at
## implementation time

`lanes.rs` already produces a specific, legible reason string when a bound
harness can't run a turn (`01-what-already-works.md` §3). **This brief did
not trace where that reason string actually surfaces to an operator** —
whether the console's existing turn-failure UI already renders arbitrary
reason text from an `unavailable` lane, or whether it currently shows a
generic "this agent couldn't respond" message that swallows the specific
reason. This is the one piece of real uncertainty left in this brief.
Whoever implements should trace from `HarnessBrain::run_turn`'s handling of
a non-empty `unavailable` list (referenced in `lanes.rs`'s module doc but not
read in full during this pass) through to whatever the console renders for a
failed turn, and confirm the reason string reaches the operator legibly —
building new plumbing for this only if it doesn't already.

## Rollout order

1. **Extend `AgentDetailView.tsx`'s picker with live readiness** (Gap 1,
   steps 1–4 of the fix). Additive — the picker still works exactly as
   before for every non-`acp` harness and for an `acp` harness on a build
   that already shows it; this only adds a badge and an inline Install
   action. Blast radius: every company that opens an agent's Model tab on a
   desktop build with at least one detected CLI harness; zero effect on
   hosted builds (nothing renders, per the desktop-only gating above) or
   companies with no detected harnesses.
2. **Add the "Manage" detail view and fold the standalone Harnesses page
   into it**, including confirming and updating whatever routes to the old
   top-level Settings entry (flagged as unconfirmed in
   `02-the-real-gap-and-fix.md` — trace the actual route registration before
   removing it). Depends on step 1 existing so there's somewhere to link
   from.
3. **Add the "Bound teammates" read** — pick option (a) or (b) from Gap 1's
   two choices based on what the existing agents-list DTO already carries.
   Independent of steps 1–2's shipping order, but needed before the detail
   view in step 2 can show that section meaningfully — can ship as a
   follow-up commit in the same PR rather than blocking step 2's merge.
4. **Retire the CLI-logins add-provider group** (Gap 2). Fully independent
   of steps 1–3 — can land in the same PR or a separate one, in either
   order. Purely a removal plus two comment updates; no migration, no data
   to move, since nothing was ever persisted for this category.
5. **Add the LLM/Provider page's "Local harnesses" section** (Gap 1b).
   Depends on step 2 existing (it opens the same detail view) and step 3
   (its row count needs the same "N agents bound" read). Purely additive
   read-only UI — no persistence, no new backend writes. Can land in the
   same PR as steps 2–3 or as a small follow-up; the only firm ordering
   constraint is landing after there's a detail view for its rows to open.
6. **Trace and, if needed, fix turn-time failure surfacing.** Do this last,
   after confirming via step 1's live readiness whether operators are
   actually hitting the "bound but not ready" case in practice, since it's
   the one item in this brief without a confirmed current behavior to build
   on.

No step requires a data migration — nothing this brief touches has ever
been persisted differently before (Gap 1/1b add read-only UI layers over
existing data; Gap 2 removes a UI path that never wrote anything).
