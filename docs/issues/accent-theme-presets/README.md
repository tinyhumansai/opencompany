# Accent theme presets — planning set

**Status:** planning only, 2026-09-25. No feature code. No GitHub issue has
been filed yet; the operator files one after reviewing this set.

The console's Appearance settings page grows a second control: a picker of
curated, named accent presets, in the spirit of Slack's "Themes" tab. It sits
beside the existing light / dark / system control and is independent of it: a
preset is chosen once and holds in both modes.

This directory lives at `docs/issues/<slug>/` (plural) rather than the
`docs/issue/<slug>/` the brief suggested, because `docs/issues/` already exists
in the repo (`docs/issues/local-runtime-setup/`) and a second, near-identical
parent directory would split planning docs across two places.

## The files

| File | What it answers |
| --- | --- |
| [`architecture.md`](architecture.md) | Exactly which custom properties a preset overrides, where the preset data lives, how the choice is applied before first paint, how it persists, how it passes the CI token gate, and the backend seam for later. |
| [`roadblocks.md`](roadblocks.md) | Every risk found by reading the code, each with `file:line` evidence. Read this before implementing. |
| [`rollout-plan.md`](rollout-plan.md) | Phased implementation order, sized for a brb-architectobot / brb-codecrusher run. |
| [`test-plan.md`](test-plan.md) | Unit and e2e tests to add, modelled on the existing theme tests. |
| [`open-questions.md`](open-questions.md) | What needs an operator or brand decision before (or during) implementation. |
| [`full-chrome-theming.md`](full-chrome-theming.md) | Research into full sidebar/gradient theming, requested 2026-09-26. **Declined** — see `theme-system-decision.md`. Kept for its blast-radius evidence. |
| [`font-picker.md`](font-picker.md) | Research into a real typeface picker, requested 2026-09-26. **Declined** — see `theme-system-decision.md`. Kept for its bundled-fonts/doctrine evidence. |
| [`theme-system-decision.md`](theme-system-decision.md) | **The final call, 2026-09-26.** Declines full-chrome theming and the font picker; ships the accent-preset system as-is plus a new constrained hue-customize option. Implement against this file. |

## Problem

The console has exactly one accent: brand violet `#7153F0`, declared as the
`--brand-50..900` ramp in `frontend/src/index.css:55-64` and bound to
`--primary`, `--ring` and the sidebar tokens. An operator can switch light and
dark (`frontend/src/components/theme-toggle.tsx`) but cannot change the hue the
interface spends on interaction. The Appearance page was built expecting this:
its header comment names "an accent" as the kind of second control the page
exists to hold (`frontend/src/views/settings/AppearanceView.tsx:13-15`).

## Terminology — read this first

"Accent" is already taken three times in this codebase, none of them meaning
what this feature means:

- `--accent` / `bg-accent` is shadcn's **hover and rest tint** under menu rows,
  bound to the neutral `--surface-*-active` rung, not to brand
  (`frontend/src/index.css:227-233`, `:482`).
- `--sidebar-accent` is the **active nav row** background (`index.css:341`).
- Inside the knowledge graph, `.oc-kg { --accent: var(--kg-accent) }` is the
  graph's own name for brand (`index.css:876`).
- Tailwind also has an `accent-*` utility family (the CSS `accent-color`
  property), which `scripts/ci/assert-design-tokens.sh` lists in its `PROPS`.

So in code this feature is always the **accent preset**: the DOM attribute is
`data-accent-preset`, the storage key `oc.appearance.accentPreset`, the module
`accent-presets.ts`. No new CSS custom property may be named `--accent-*`. The
user-facing label can still say "Accent".

## Goals

1. A curated set of named presets (target 6–8), each a complete, hand-tuned
   replacement for the brand ramp, selectable from Settings → Appearance.
2. The chosen preset is on screen from the **first** paint of the app, in both
   modes, with no flash from the default accent to the chosen one.
3. Persisted per browser (per desktop install), like the theme mode, with the
   storage behind a small interface so a server-backed store can be slotted in
   later without touching the picker or the CSS.
4. Every preset meets the same measured contrast bars the current brand meets
   (`docs/design-system/color.md:48-56`), enforced by a unit test rather than
   by review.
5. Zero change to the CI token gate: preset colours live in `index.css`, where
   every other colour already lives; the TypeScript side carries ids and labels
   only.

## Non-goals for v1

- **Choosing the default preset's colour.** Explicitly deferred to a separate
  brand / colour-psychology decision. The architecture makes the default
  "whatever `:root` declares", so that decision is a one-block edit later and
  blocks nothing here (see `architecture.md` §2).
- **A free-form custom colour picker.** A user-supplied hex cannot be
  contrast-vetted ahead of time and would need a runtime ramp generator. Future
  work; noted in `open-questions.md`.
- **Recolouring status, identity tones, or chart series 2–5.** Those are closed
  vocabularies with fixed meanings (`docs/brand/README.md:117-131`). See
  `architecture.md` §4 for the full scope ruling, including chart slot 1 and the
  knowledge graph.
- **Recolouring the neutral surfaces.** The greys carry ~286° of violet at very
  low chroma (`docs/brand/README.md:110-115`). They stay as they are in v1; see
  `open-questions.md` Q4.
- **Native desktop chrome.** The Tauri window uses the OS-drawn, decorated title
  bar (`crates/opencompany-app/tauri.conf.json` `"decorations": true`, no
  `titleBarStyle`; `frontend/src/components/window-chrome.tsx:91`
  `SHELL_DRAWS_ITS_OWN_TITLE_BAR = false`). The console cannot tint it, and it
  does not follow dark mode today either. Out of scope.
- **The terminal client.** `crates/opencompany-tui/src/ui.rs` uses ANSI named
  colours (`Color::Yellow`, `Color::Green`, …) and has no brand hue to replace.
- **Agent-authored pages.** They render in a `sandbox="allow-scripts"` iframe
  with no `allow-same-origin` (`frontend/src/views/PagesView.tsx:290`), so they
  cannot read the console's storage, and no theme reaches them today — not even
  dark mode. They keep the default ramp via `frontend/pages-sdk/index.css`.
  Future work.
- **Vision-assistive presets** (Slack ships tritanopia and
  protanopia/deuteranopia themes). Worth doing, but they need the status palette
  to move too, which is the opposite of this feature's scope rule. Future work.

## Decisions in one screen

| Question | Decision | Where argued |
| --- | --- | --- |
| What does a preset override? | The ten `--brand-{50..900}` primitives, plus optional mode-qualified `-foreground` overrides. Nothing else. | `architecture.md` §2 |
| Where are the colours? | A new section at the end of `frontend/src/index.css`, one `[data-accent-preset="<id>"]` block per preset. | `architecture.md` §2–3 |
| Where are the ids and labels? | `frontend/src/lib/accent-presets.ts`, with no colour literals. Needs no CI exemption. | `architecture.md` §5 |
| How is it applied without a flash? | A synchronous `applyStoredAccentPreset()` call in `main.tsx` before `mount()`. No inline script. | `architecture.md` §6 |
| Storage key? | `oc.appearance.accentPreset` in `localStorage`. | `architecture.md` §7 |
| Charts and status? | Status and chart slots 2–5 fixed (already are). Chart slot 1 and the graph's AI-agent colour **pinned** to a new fixed primitive, so they stop following brand. | `architecture.md` §4 |
| Backend later? | Real seam exists: `PATCH …/auth/me` + `UserRecord`, stored as JSON. `localStorage` stays the boot cache either way. | `architecture.md` §8 |
| Contrast? | A node-environment vitest that parses each preset's oklch values and asserts the documented pairs. In v1. | `architecture.md` §9, `test-plan.md` |
