# Harness/provider unification: surface readiness, retire the dead stub

Implementation brief for issue #2394. Read this file first, then the other
three in order. Written so a fresh engineer or Claude session with none of
the conversation this came from can pick it up and execute correctly.

## The headline finding: this is much smaller than it first looked

The originating question was "how do we let an operator pick Claude Code /
Codex as a teammate's model source, the way Claude's own Connectors settings
page lets you pick a connector." Two earlier passes at scoping this assumed
real backend work was needed — a "delegated credential bridge," a new
inference-config mechanism, a persisted company-level "provider" row. **All
of that was wrong.** Reading the actual code found that per-agent harness
binding, including binding to a bare CLI id nothing declares, is **already
fully built, already correct, and already shipped**:

- `Agent.harness: Option<String>` (`crates/opencompany-core/src/company/types.rs:702`)
  already lets a teammate bind to any harness by id.
- `harness/lanes.rs`'s `referenced_implicit_locals` (line 257) already
  synthesizes a lane on demand for a bare `claude`/`codex` binding nothing
  declares in `company.toml`.
- The same file's `unavailable` mechanism already fails a turn cleanly with a
  specific reason when a harness can't run — never a silent fallback to a
  different model (see `01-what-already-works.md` for the exact chain).
- `frontend/src/views/team/AgentDetailView.tsx`'s Harness & Model editor
  (issue #1245's harness-picker follow-up) already lets an operator pick a
  detected CLI harness for a teammate, today, in the running app.

See `01-what-already-works.md` for the full, cited chain — it's worth reading
in full before touching anything, because it changes what "the fix" even is.

**The actual gap is narrow**, covered in `02-the-real-gap-and-fix.md`:
1. That picker's option label shows only an id/kind (`harnessOptionLabel`,
   `frontend/src/lib/agent.ts:408`) — no live readiness. An operator can pick
   "claude" with no idea whether it's installed or signed in.
2. The LLM/Provider page's separate "CLI logins" group
   (`Category::Cli` in `company/inference/catalogue.rs`) is dead
   (`CLI_LOGINS_REACHABLE = false`, `frontend/src/inference/connect.ts:197`;
   a hardcoded refusal in `server/ops/inference/providers.rs:1083-1093`) —
   and, per this research, it isn't just unfinished, it's **the wrong shape**:
   there is nothing that should ever be "added" or persisted at the company
   level for a CLI login. It was very likely a superseded attempt at solving
   a problem issue #1245's picker already solves correctly.

## What the fix actually is

Put live readiness into the picker that already exists, and retire the dead
one rather than finishing it. Concretely: reuse the exact same readiness
plumbing the standalone External Harnesses settings page already uses
(`crates/opencompany-app/src/acp/discovery.rs` — no credential files read, a
live JSON-RPC probe, nothing cached) inline in
`AgentDetailView.tsx`'s harness picker, and fold the standalone Harnesses
settings page into being the detail view reached from there — a "Manage"
link next to the picker, not a separate top-level Settings section. Full
detail, edge cases, and the rollout order: `02-the-real-gap-and-fix.md` and
`03-rollout-and-edge-cases.md`.

## Corrected flow (ASCII)

An earlier session pass sketched a 9-panel flow assuming a company-level
"Add provider → CLI logins → fill a form → Add" sequence, mirroring how
Cloud/Local providers work. **That flow is wrong and is not reproduced
here** — there is no "Add" step, because nothing is added. What actually
changes is entirely inside the agent editor, plus a link out to a detail
view. This is the corrected flow:

**Step 0 — discoverability: the LLM/Provider page, so this doesn't only
exist inside a specific agent's editor (Gap 1b, `02-the-real-gap-and-fix.md`):**

```
┌──────────────────────────────────────────────────────────────────────┐
│  Model & Providers                                        [+ Add]    │
├──────────────────────────────────────────────────────────────────────┤
│  Connected                                                            │
│  ─────────────────────────────────────────────────────────────────   │
│  OpenRouter          Cloud            ✓ connected              ›     │
│  Managed (TinyHumans) Cloud            ✓ connected              ›     │
│                                                                        │
│  Local harnesses                                                      │
│  ─────────────────────────────────────────────────────────────────   │
│  claude (ACP)         ✓ ready          2 agents bound          ›     │
│  codex (ACP)          ⚠ not installed  0 agents bound          ›     │
└──────────────────────────────────────────────────────────────────────┘
```

"Connected" is the real, existing section heading (`ProvidersTab.tsx`'s
`<Card>`/`<h3>` rendering `ProviderList`) — "Local harnesses" is a new
sibling section in the same style, purely read-only: `[+ Add]` never opens
anything for it, and its rows read "N agents bound," never "connected,"
because nothing company-wide is. Clicking a row opens the *same* detail view
Step 2 below shows — this is a second entry point into one destination, not
a second feature.

**Step 1 — an agent's Model tab, today, structurally close to what's already
shipped, but the harness option now carries live readiness once expanded:**

```
┌──────────────────────────────────────────────────────────────────────┐
│  ← research-agent          Model                                     │
├──────────────────────────────────────────────────────────────────────┤
│  Harness & model                                                      │
│  claude (ACP) → claude-opus-5              ✓ ready         [Edit]    │
└──────────────────────────────────────────────────────────────────────┘

  ↓ pressing Edit opens the existing editor in place — a harness Select
    (readiness now shown per option) and, only when the drafted harness is
    `acp`, a model Select beneath it, live-fetched for that harness:

┌──────────────────────────────────────────────────────────────────────┐
│  Harness   [ claude — claude (ACP)  ✓ ready              ▾ ]  [Manage]│
│  Model     [ claude-opus-5                                ▾ ]        │
│                                              [ Cancel ]   [ Save ]    │
└──────────────────────────────────────────────────────────────────────┘
```

This closed/open two-state shape (a summary row, an `[Edit]` pencil, a form
that appears in place) is the real structure of `HarnessAndModel`
(`AgentDetailView.tsx`) today — not a flat list of always-visible options.
Readiness (`✓ ready` / `⚠ not installed` / `⏳ checking` / `⚠ not signed in`)
on the harness `<Select>`'s options comes from the same live probe the
standalone page already runs — no new backend call shape, just a second
frontend caller of the existing Tauri `oc_acp_harnesses`/
`oc_acp_confirm_harness` commands. The model `<Select>` and its live refresh
already exist and need no new work at all — see `01-what-already-works.md`
§5's expanded detail. Pressing Save persists through the **existing** `PATCH`
this editor already sends — `edits.harness` and, when a model is pinned,
`edits.model` — nothing new to persist.

**Step 2 — pressing "Manage" opens the detail view (the old standalone
External Harnesses page's content, now reached from here instead of a
top-level Settings entry):**

```
┌──────────────────────────────────────────────────────────────────────┐
│  ← Back to research-agent    claude (ACP)         [Reinstall]        │
├──────────────────────────────────────────────────────────────────────┤
│  ✓ Ready — signed in, checked just now              [ Check again ]  │
│                                                                        │
│  Adapter          claude-agent-acp v0.70.0 (this app's own copy)     │
│  CLI              found on PATH — v2.1.276                           │
│  Models detected  claude-opus-5, claude-sonnet-5                     │
│                                                                        │
│  Bound teammates                                                      │
│  ─────────────────────────────────────────────────────────────────   │
│  research-agent    → runs its full turn through this harness [Edit]  │
│  support-triage     → runs its full turn through this harness [Edit] │
│                                                                        │
│  To bind another teammate to this harness, open that teammate's       │
│  Model tab.                                                           │
└──────────────────────────────────────────────────────────────────────┘
```

Reached from two places — Step 0's "Local harnesses" row, and Step 1's
`[Manage]` link — the same live-computed content either way, never two
sources of truth pretending to agree. Each `[Edit]` link is a shortcut back
to that teammate's own Model tab (Step 1), not a new editing surface here.

**Step 3 — not installed, shown inline wherever a harness row appears
(the closed summary row, the open Select's option, or Step 0's "Local
harnesses" row) — no separate modal needed:**

```
│  codex (ACP)                          ⚠ not installed    [Install]   │
```

Pressing `Install` runs the existing adapter-install flow in place; the row
updates to `⏳ checking` then `✓ ready` on success, matching how the
standalone page already behaves.

**Step 4 — turn-time failure (already built, not new — see
`01-what-already-works.md` for the exact mechanism):**

```
┌──────────────────────────────────────────────────────────────────────┐
│  research-agent                                    ⚠ harness offline │
├──────────────────────────────────────────────────────────────────────┤
│  research-agent can't take a turn right now. It's bound to `claude`, │
│  which needs this machine's desktop app open and Claude Code signed  │
│  in — neither is true at the moment.                                  │
│                                                                        │
│              [ Open harness settings ]                                │
└──────────────────────────────────────────────────────────────────────┘
```

This message's *mechanism* (a clean failure naming the reason, never a
silent fallback) is already real — `lanes.rs`'s `unavailable` and
`resolve_acp_engine`'s error mapping already produce exactly this shape of
error today. What's new is only making sure whatever surfaces a failed turn
to the operator renders this reason legibly — see
`03-rollout-and-edge-cases.md` for the specific check needed.

**What's retired, not built:** the "CLI logins" group in the add-provider
dropdown on the LLM/Provider page. See `02-the-real-gap-and-fix.md` for the
reasoning and the exact code to remove/change.

## Files in this brief

- `01-what-already-works.md` — the cited chain proving per-agent harness
  binding already works end to end; read this before proposing any backend
  change, most of what you'd reach for already exists
- `02-the-real-gap-and-fix.md` — the two real gaps, the fix for each, and why
  the CLI-logins provider group should be retired rather than finished
- `03-rollout-and-edge-cases.md` — desktop-only gating (already solved),
  readiness-check cadence, multi-agent "Bound teammates," turn-time failure
  surfacing, and the rollout order
