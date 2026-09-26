# Theme system — the final design decision

**Status:** decided, 2026-09-26. Supersedes the "build it" path in
`full-chrome-theming.md` and `font-picker.md` — both are kept for their
research (the blast-radius and doctrine evidence is real and worth keeping),
but their proposed builds are declined below, with reasons. This document is
the one to implement against.

## The call, in one paragraph

OC's console is a **shared company surface** — every operator on a company
sees the same console — not a private, per-person space the way a Slack
sidebar is. Everything in `index.css` is built around one discipline: violet
is reserved for interaction and identity, neutrals carry a deliberate faint
brand undertone, and every contrast pair is measured. The right theme system
for this product is: **keep personalization to the one thing that's genuinely
the operator's own reaction to the interface — the interaction hue** — via
curated presets (shipped) plus a constrained customize option (this doc),
and leave every shared, brand-identity, and accessibility-load-bearing
surface exactly as measured.

## Declined: full gradient/chrome sidebar theming

Evidence, not just cost:

- `content-surface.tsx`'s own doc comment: the sidebar column **paints no
  fill of its own** — the window `--chrome` surface shows through it and the
  content-card margin as one continuous surface, *specifically because*
  issue #1178 removed per-pane coloring to kill a 1px seam ("tinting each
  pane separately would land them on different values and put back the
  seam the layout exists to remove"). A Slack-style colored/gradient sidebar
  panel does not extend this architecture — it reverses an already-shipped
  fix.
- `--sidebar` and `--card`/`--popover` share one primitive today (124
  combined call sites) — decoupling them is a real, non-trivial precondition
  the `full-chrome-theming.md` doc already identified.
- Slack's gradient themes earn their keep as a personalization/delight
  feature for an individual's own private workspace. OC's sidebar is shared
  company chrome. The feature's actual value in its source product doesn't
  transfer.

**Not doing this.**

## Declined: a real typeface picker

Evidence: only two font families exist in the entire codebase (Geist
Variable, Geist Mono, self-hosted), and `docs/brand/README.md` states "Geist
Variable for everything" as deliberate doctrine — the same weight as the
"one hue" rule the accent picker already got explicit permission to
override. Nobody has made the equivalent call for typography, and doing so
means bundling 2-4 new licensed font families and solving a real FOUC/preload
problem this CSP-locked SPA has no existing pattern for. This is chasing
Slack feature-parity, not a real OC need.

**Not doing this.**

## Shipping: the accent-preset system as-is (already done)

PR #2494 — 9 curated presets (Violet, Indigo, Blue, Teal, Green, Amber, Rose,
Graphite, Default) overriding only `--brand-{50..900}`. This already covers
every surface that should be personalizable: `--primary`, `--ring`,
`--sidebar-primary`, the active-nav-row ink (`--sidebar-accent-foreground`),
`--shadow-brand`, `--glow-brand-card`, and "you"-message inline code tinting
in `markdown.tsx`. Confirmed by survey: buttons, links, and the one
brand-ink surface (active nav row) are the *only* interactive elements that
carry brand hue — everything else (status, identity, charts 2-5, chart-1/kg
via the Phase-1 `--signature-*` pin, third-party logo colors) is correctly
untouched. No changes needed here.

## New: a constrained "Customize" tab

This is the actual answer to "and also have the customize option" — scoped
so it cannot produce an inaccessible or off-brand result, unlike a raw hex
input would.

### Mechanism

The existing brand ramp is not ten independent colours — it is one
lightness/chroma cadence (fixed per step) with a single rotating hue
(`285.51°` for the shipped default). Every curated preset already IS a hue
rotation of that same cadence (confirmed: `accent-presets.ts` header comment
— "Graphite is the deliberate exception: chroma zero at every step" implies
every other preset keeps the cadence and varies hue/chroma together, tuned
per-hue for contrast).

**Customize exposes exactly one control: a hue value (0-360°).** Given a
hue, the ramp is generated in the browser from the same fixed per-step
lightness cadence and a conservative chroma curve already proven safe across
the 8 non-Graphite presets (interpolate/clamp within the range those 8
presets already span, rather than inventing new chroma math) — so a custom
hue can't produce a combination nobody has verified the *shape* of, only a
new angle on a shape that's already known to work.

- **No new dependency.** OKLCH is native CSS; this is a hue-channel
  substitution over an already-authored lightness/chroma table, not general
  colour science. `culori`/`chroma-js`/etc. are not needed.
- **Live contrast check, not a static test.** Port the exact assertions
  `accent-presets-contrast.test.ts` already runs (the same pairs
  `docs/design-system/color.md` documents) into a small runtime function
  shared by both the test and the picker. If a hue fails, the customize UI
  says so and refuses to apply it — the same bar every curated preset
  already clears, enforced live instead of only at commit time.
- **Applied via inline style, not a CSS block.** Curated presets stay
  `[data-accent-preset="id"]` blocks in `index.css` (nothing changes there).
  A custom hue is user-generated data, not an authored asset, so it's set as
  inline custom properties on `<html>` (the 10 `--brand-*` values, computed),
  under a distinct `data-accent-preset="custom"` marker so the existing
  swatch/picker code's assumptions about "one of N known ids" still hold —
  `custom` just also carries an inline style payload alongside it.
- **Persistence:** the existing `oc.appearance.accentPreset` key stores
  `"custom"`; a second key, `oc.appearance.customHue`, stores the hue value.
  `applyStoredAccentPreset()` (already applied pre-mount, no flash) reads
  both when the stored id is `"custom"`.
- **UI:** a "Custom" swatch as a 10th tile in the existing picker grid,
  opening a single hue slider (not a full colour picker — saturation/
  lightness stay locked to the proven cadence) with a live preview swatch
  and the same radiogroup/keyboard semantics the other 9 tiles already have.

### What Customize does NOT do

- No saturation/lightness control — only hue. This is what keeps every
  possible output inside the already-verified shape.
- No effect on the sidebar/chrome/card surfaces, status colours, identity
  tones, or charts — identical scope boundary to the curated presets.
- No raw hex escaping into `.tsx` — the hue is a number, the ramp is
  computed, `scripts/ci/assert-design-tokens.sh` needs no new exemption.

## Also worth doing cheaply, not requested but noted for the record

`--radius` is a single global token everything else derives from via
`calc()`. A coarse "Shape: Sharp / Default / Rounded" control would be a
near-zero-risk, purely geometric third dimension (no contrast implications
at all) — flagged here for a future ask, **not** being built now since it
wasn't requested; scope stays to the customize-hue feature above.

## Implementation notes for whoever builds this

- Land on the same branch/PR #2494 (per operator's standing "one PR" rule).
- Reuse `accent-presets.ts`'s existing `AccentPreset`/storage/apply/
  `useSyncExternalStore` machinery — this is additive to it, not a rewrite.
- The contrast-check function must be a single shared implementation used by
  both `accent-presets-contrast.test.ts` (extend it to fuzz a range of hues,
  not just the 9 fixed ones) and the live UI check — write it once in
  `frontend/src/lib/`, imported by both.
- Full multi-round Playwright verification before calling this done: every
  curated preset AND a swept range of custom hues (not just one sample),
  across both themes, keyboard and mouse, persistence across reload, and
  confirm the live contrast refusal actually refuses a hue chosen to fail
  (don't just confirm it accepts good ones).
