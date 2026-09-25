# The two real gaps, and the fix for each

## Gap 1: the working picker shows no readiness

Covered in full in `01-what-already-works.md` §6. The fix:

**Reuse the standalone External Harnesses page's exact readiness plumbing,
inline in `AgentDetailView.tsx`'s Harness & Model editor.** Concretely:

1. Import `HarnessRow`, `joinHarnesses`, `withReadiness`, `isChecking`,
   `isUsableHere`, `readinessNote` from `frontend/src/lib/harnesses.ts` —
   already exported, already used by `external-harnesses.tsx`.
2. In the agent editor, once `harnesses` (from `client.listHarnesses`) is
   loaded, also call the same Tauri survey/confirm pair
   (`acpHarnesses`/`confirmAcpHarness` from
   `frontend/src/api/transport/desktop`, already imported by
   `external-harnesses.tsx`) to get live readiness, and join them exactly as
   `joinHarnesses()` already does.
3. Extend `harnessOptionLabel` (or add a sibling render, since the `<select>`
   option text and a richer row UI have different constraints) to show the
   readiness badge next to each `acp`-kind entry — `✓ ready` / `⏳ checking` /
   `⚠ not installed` / `⚠ not signed in`, matching the vocabulary the
   standalone page already uses (`readinessNote`).
4. For a `⚠ not installed` row, surface the existing `Install` action inline
   (same `installAcpHarness` Tauri call the standalone page already makes) —
   no new install mechanism, just a second call site for the existing one.
5. A "Manage" link next to the picker opens a detail view carrying the
   *same* content the standalone External Harnesses page renders today
   (adapter version, CLI version, detected models, re-check button) — see
   Gap 1's route change below.

**Route change:** the standalone page currently lives at its own top-level
Settings destination. Per the decision already made for this brief (folding
it into a detail view, not keeping it as its own section), that route should
redirect to — or the component should be reachable only via — a per-harness
detail view opened from the agent editor's "Manage" link. Find the exact
current route registration (search the settings router for
`external-harnesses` or the component's import) before removing anything;
this brief did not trace that specific route wiring and it needs confirming
at implementation time, not assumed.

**Gap 1b: folding the standalone page away loses a real, currently-findable
entry point.** Today's standalone External Harnesses page sits in Settings —
reachable directly, without first picking an agent. Folding it entirely into
being "the thing you reach from an agent's Manage link" (as this brief
originally scoped it) is a real discoverability regression: an operator
asking "is Claude Code even usable here" has no way to find out without
opening a specific agent's Model tab first.

**Fix: the LLM/Provider page keeps a second, sibling read-only section**,
alongside the existing "Connected" providers list (the real heading, verified
in `frontend/src/inference/ProvidersTab.tsx` — it renders a `<Card>` with an
`<h3>` reading "Connected", followed by `ProviderList`; the new section
follows the same `<Card>`/`<h3>` convention, its own header reading "Local
harnesses"). This is a **second entry point into the same detail view**
Gap 1's "Manage" link already opens — not a new destination, not new
persisted state. It's populated purely by reads already planned elsewhere in
this brief: `GET {scope}/harnesses` for the detected/`acp` rows (already
gated correctly by `can_run_local_acp`, point 4 in `01-what-already-works.md`),
the same live readiness probe Gap 1 wires into the agent picker, and the same
"N agents bound" read the "Bound teammates" list below already needs.

Two things this section must NOT do, to stay consistent with Gap 2's
conclusion: its rows are never "Add"-able (the page's `[+ Add]` button
continues to only open Cloud/Local-runtime forms, since CLI logins have no
add flow — Gap 2 below), and its status column must read "N agents bound,"
never "connected" — nothing is connected company-wide, some number of agents
individually picked it, and the wording has to say that plainly rather than
borrow language that implies a persisted, shared connection.

**New read needed, not yet built:** the detail view's "Bound teammates" list
(shown in the README's Step 2 mockup) needs "which agents currently have
`harness == this id`" for the *whole company*, not just the one agent whose
editor you opened it from. `agents_on()` (`lanes.rs:178`) already computes
exactly this, server-side, but it's private to that module and used only
during lane-building, not exposed via an endpoint. Two options: (a) add a
small read endpoint that reuses `agents_on`'s logic (or promotes it to
`pub(crate)` and calls it from a new handler), or (b) compute it client-side
from the existing company agents-list endpoint by filtering on `.harness`,
which needs no backend change at all if that endpoint already returns each
agent's `harness` field (check `AgentDetailDto`/whatever the roster-list DTO
is before deciding — likely the cheaper path if it already carries this
field, since `AgentDetailDto` already includes `harness` per
`AgentDetailView.tsx`'s own type import).

## Gap 2: the CLI-logins provider group is dead, and finishing it is the
wrong move

`company/inference/catalogue.rs`'s `Category::Cli` and its UI copy
(`GROUP_CLI = "CLI logins"`, `HELPER_CLI` — "You don't need to type
anything... another tool already holds the credential") describe a
company-level **add-a-provider** action. But per Gap 1's findings, there is
nothing to add:

- No credential is collected (the copy says so itself).
- No endpoint is collected.
- Per the External Harnesses page's own design philosophy (already read
  this session, `docs/spec/runtime/external-harnesses-ui.md`): *"There is
  deliberately no 'connect' action... a stored 'connected' flag would be a
  second source of truth that could disagree with the CLI actually being
  there."*

So a company-level "Add" for a CLI login would have nothing to persist
except possibly a bare marker record ("this company intends to use CLI
logins") — and nothing downstream reads such a marker today; `lanes.rs`
resolves purely from each *agent's* `harness` field, never from anything at
the company/provider-list level. Building that marker would be a second
source of truth with no consumer, the exact anti-pattern the harness page
was written to avoid.

**Recommendation: retire this group from the add-provider flow rather than
finishing it.**

Concretely:

- Remove/stop rendering `GROUP_CLI` in the add-provider dropdown
  (`frontend/src/inference/connect.ts` and wherever the dropdown component
  reads `SETUP_INFERENCE_OPTIONS`/the provider-kind groups from). This is a
  frontend-only change — `CLI_LOGINS_REACHABLE = false` can simply stay
  `false` permanently (rename or re-comment it as "this category has no
  add-provider flow by design" rather than "not reachable yet", so a future
  reader doesn't mistake it for still-pending work).
- Leave the backend refusal at `server/ops/inference/providers.rs:1083-1093`
  in place, unchanged — it's still doing its job (a defensive 400 if
  something does try to POST a CLI-kind add-provider request), just update
  its comment to say this is permanent by design, not a placeholder for a
  future desktop-delegated-credential mechanism (the comment there currently
  implies a future fix is expected; per this brief's finding, it is not).
- `Category::Cli`, `CLI_LOGINS` (the table of known CLI-login kinds), and
  `ProviderRef::ClaudeCode` can stay in `catalogue.rs`/`resolve.rs` as-is —
  they're harmless, and `ProviderRef::ClaudeCode` genuinely still parses from
  the routing grammar (worth leaving alone rather than a wider removal this
  brief hasn't scoped).
- No change needed to `server/ops/inference/providers.rs:2679`'s
  `has_category(providers, Category::Cli)` check (the routing-validation
  guard preventing a route from naming a CLI-category provider that can
  never exist) — it stays correct and load-bearing exactly as-is.

This is a **smaller** change than "finish the dead feature" — a UI removal
plus two comment updates, not new persistence, new validation, or a new
credential model.
