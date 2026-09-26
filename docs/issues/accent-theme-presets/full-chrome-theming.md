# Full chrome theming — the corrected scope

Written 2026-09-26 after the operator reviewed the shipped accent-preset
feature (issue #2493, PR #2494) against real Slack screenshots and said "I
wanted the exact [same]" — pointing specifically at Slack's gradient themes
("Funky Fresh", "Jazz Club", "Electric Fusion"), not the flat "Single color"
row. In every screenshot the **entire sidebar surface** is a gradient wash,
not just the interaction hue. This supersedes the "recolouring the neutral
surfaces… stay as they are in v1" non-goal in `README.md` and the "No (v1)"
row for neutral surfaces in `architecture.md` §4.

This is a scope correction on already-shipped work, not a bug fix. Read
`architecture.md`, `roadblocks.md`, and `README.md` first — this document
assumes that context and only states what's different or new.

## 1. What actually has to change

**Reusable as-is:** the mechanism in `architecture.md` §5–7 — the
`accent-presets.ts` registry shape (id/label, no colours), the
`localStorage["oc.appearance.accentPreset"]` persistence, the synchronous
pre-mount apply in `main.tsx`, the `[data-accent-preset="…"]` attribute-on-`<html>`
approach, and the CI-gate compliance story (§11) all carry over unchanged. A
gradient preset is still "an id maps to a CSS block"; nothing about *how* the
choice is stored or applied before first paint needs to change.

**Not reusable — needs rework:**

1. **The preset override set.** Today a preset overrides ten `--brand-N`
   primitives (§2). A chrome-themed preset additionally needs to override
   whatever `--sidebar` resolves to — and `--sidebar` is not independently
   overridable today (see §2 below). This is new architecture, not an
   extension of the existing one.
2. **The picker's swatch rendering.** The shipped swatches
   (`architecture.md` §10) are `bg-brand-500 dark:bg-brand-400` — a single flat
   circle. A gradient preset's swatch must render the *gradient*, so the
   preview is honest about what selecting it does. This means either a new
   `previewGradient?: string` (a CSS `background-image` value, itself
   `var()`-composed, never a literal — see §11 R7 on the CI gate's blind spot
   for non-hex color functions in `.ts`) or computing the swatch's background
   from the same CSS block the theme itself uses, via a second
   `data-accent-preset` attribute on the swatch node the way flat swatches
   already do (`roadblocks.md` R4) — the gradient case still resolves
   correctly under that mechanism, since `background-image: linear-gradient(var(--x), var(--y))`
   set inside `[data-accent-preset="x"]` resolves against the swatch's own
   attribute exactly as `--brand-500` does today. **This part is cheap** — it
   is the same mechanism, wired to one more property.
3. **The contrast test's assumptions.** `test-plan.md` U4 (and
   `architecture.md` §9) checks foreground-on-`brand-500` and
   foreground-on-canvas pairs. A gradient sidebar background is not one
   colour — contrast must be checked against **both gradient endpoints**, or
   the darker/lighter stop fails silently while the other passes. This is a
   test-design change, not just more test cases.
4. **The "pin chart-1 and kg-accent" reasoning still holds, unchanged** — it
   was never about the sidebar background, it was about the interaction hue
   colliding with chart/graph hues (`architecture.md` §4, `roadblocks.md`
   R10). Nothing here reopens that decision.

## 2. The actual blast radius (counted, not estimated)

Read live on this branch, 2026-09-26.

`--sidebar` is declared as an alias, not an independent value:

```
frontend/src/index.css:332   --sidebar: var(--surface-light-1);
frontend/src/index.css:551   --sidebar: var(--surface-dark-1);
```

`--surface-light-1` / `--surface-dark-1` is **the same primitive** `--card`
and `--popover` resolve to:

```
frontend/src/index.css:196   --card: var(--surface-light-1);
frontend/src/index.css:198   --popover: var(--surface-light-1);
frontend/src/index.css:332   --sidebar: var(--surface-light-1);
frontend/src/index.css:452   --card: var(--surface-dark-1);
frontend/src/index.css:551   --sidebar: var(--surface-dark-1);
```

Class-usage counts across `frontend/src/**/*.tsx`:

| Utility | Occurrences |
| --- | --- |
| `bg-sidebar` | 38 |
| `bg-card` | 70 |
| `bg-popover` | 16 |

**This is the single most important finding in this document.** Overriding
`--sidebar` inside a preset block today, with no other change, would also
recolor every card and popover in the console — 124 class-usage sites across
the app, not one component. This is not a hypothetical edge case; it is the
literal current wiring. A preset cannot touch `--sidebar` until it has its own
primitive, the same way `--chart-1` could not move until it got
`--signature-500/400` (`architecture.md` §4) — except this decoupling is
larger, because `--chart-1` had exactly two consumers and `--sidebar` shares
its value with two other semantic roles used across most of the UI.

The outer app frame is a **separate** token, `--chrome`
(`index.css:246` `var(--surface-light-chrome)`, `:503` `var(--surface-dark-2)`),
consumed by the shell wrapper (`frontend/src/components/ui/sidebar.tsx:175`,
`bg-chrome has-data-[variant=inset]:bg-sidebar`). Slack's gradient in the
screenshots covers the sidebar column specifically, not the message-pane
background, which maps reasonably well onto "theme `--sidebar`, leave
`--chrome` alone" — but confirm this against the actual screenshots before
building, since the third screenshot's gradient reads slightly into the frame
edge too.

**Required decoupling, before any gradient preset can safely exist:**

1. Give the sidebar its own primitive — e.g. `--sidebar-surface-1` — with
   today's exact `--surface-light-1` / `--surface-dark-1` values, and repoint
   `--sidebar` (only) at it. Zero-pixel change on landing, same pattern as
   Phase 1 of the shipped work.
2. Re-verify `--card` and `--popover` still resolve correctly afterward (they
   should be untouched, since only `--sidebar`'s alias target moves).
3. Only then can a preset override `--sidebar-surface-1` — or, for a gradient,
   set a new `--sidebar-background-image` consumed by the sidebar's own
   `background-image` rule — without recoloring cards and popovers.

## 3. Contrast re-verification burden

Not combinatorial, but not free either. Today's contrast table
(`architecture.md` §9, sourced from `docs/design-system/color.md:48-56`) has 5
pairs, all against the *interaction* hue. A themed sidebar adds, per preset:

- `--sidebar-foreground` (text) against the new sidebar background/gradient —
  1 pair, both gradient stops if gradient.
- `--sidebar-accent-foreground` (active nav row text) against
  `--sidebar-accent` — already brand-derived today (`brand-100` /
  `--surface-dark-active`, `architecture.md` §1 table) and would need its own
  look at whether it still reads against a now-colored sidebar backdrop.
- `--sidebar-border` — not a contrast pair in the WCAG text sense, but a
  visibility check (a border invisible against its own gradient reads as a
  layout bug, not a contrast failure a text-contrast test would catch).

Rough count: **2 real WCAG pairs × 2 stops (if gradient) × N presets**, so for
8 presets that is up to 32 checks, not 5. This fits the same node-vitest
approach already built for U4 (`architecture.md` §9) — it needs two more pair
definitions and a loop over gradient stops, not a new testing strategy. Not
free, but not a redesign either.

## 4. Gradient mechanism — recommendation

Use `background-image`, not `background-color`, on the sidebar's own root
element, driven by two new theme-independent-name, preset-dependent tokens:

```css
[data-accent-preset="funky-fresh"]:not(.dark) {
    --sidebar-gradient-from: oklch(…);  /* #… */
    --sidebar-gradient-to: oklch(…);    /* #… */
}
[data-accent-preset="funky-fresh"].dark {
    --sidebar-gradient-from: oklch(…);
    --sidebar-gradient-to: oklch(…);
}
```

and in the semantic layer (mode-independent, since the preset already
supplies mode-qualified stops):

```css
:root {
    --sidebar-background-image: linear-gradient(180deg, var(--sidebar-gradient-from), var(--sidebar-gradient-to));
}
```

with the default preset's stops both equal to `--sidebar-surface-1` (§2), so
`linear-gradient(x, y, y)` renders as a flat fill and the default case needs no
special-casing in the component. The sidebar root
(`frontend/src/components/ui/sidebar.tsx:206`) changes from
`bg-sidebar` to consuming `--sidebar-background-image` via a `@theme inline`
utility (e.g. `bg-[image:var(--sidebar-background-image)]` or a named utility
class), following the exact "`@theme inline` makes the utility emit `var(--x)`"
pattern already verified in `architecture.md` §1.

This reuses the existing three-layer discipline exactly — primitives (the two
gradient stops) → semantics (the composed gradient) → utility — rather than
inventing a second styling mechanism alongside it. The swatch preview
(§1.2) reads `--sidebar-background-image` the same way a flat swatch reads
`--brand-500` today, so one rendering path covers both flat and gradient
presets: a "flat" preset is simply a gradient with identical stops.

## 5. Tauri desktop consistency — an honest opinion, not a deferral

`crates/opencompany-app/tauri.conf.json` sets `"decorations": true` with no
custom title bar (`README.md` non-goal citation,
`frontend/src/components/window-chrome.tsx:91` `SHELL_DRAWS_ITS_OWN_TITLE_BAR
= false`), and this document does not reopen that — native title-bar tinting
stays out of scope, correctly.

But the visual-consistency question is real and worth stating plainly rather
than waving off: on macOS, an untinted native title bar sits directly above
the in-webview `window-title-bar.tsx` custom title row, which itself sits
above a now-gradient-washed sidebar. Today, with a flat neutral sidebar, the
native chrome and the in-app chrome already read as two different grays and
this is apparently acceptable. Under a vivid two-stop gradient (Jazz Club's
blue-to-red, for instance), the seam between "OS-drawn, always-neutral" and
"app-drawn, vividly colored" will be more visually obvious than it is today,
not because anything new breaks, but because the contrast between the two
surfaces increases. This is a real product judgment call (is that seam
acceptable, the way it evidently is for the flat case), not an engineering
blocker — flag it for the operator to look at once a real gradient preset
exists, rather than assuming either answer.

## 6. Rollout, corrected

Phases 1–4 in `rollout-plan.md` are unaffected and already shipped (or, per
the plan, phased in — check current state before resuming). This is
additional work, not a redo:

### Phase A — Decouple the sidebar surface (zero-pixel, same pattern as the shipped Phase 1)

1. Add `--sidebar-surface-1` (light) / dark equivalent with today's exact
   `--surface-light-1` / `--surface-dark-1` values.
2. Repoint `--sidebar` (`index.css:332`, `:551`) at it. `--card` and
   `--popover` are untouched — verify with the same "diff the computed style"
   technique already used to verify Phase 1 was zero-pixel.
3. Add `--sidebar-background-image` at the semantic layer, defaulting to a
   flat fill of the new primitive (§4).
4. Update `frontend/src/components/ui/sidebar.tsx:206` (and any other
   `bg-sidebar` consumer that means "the sidebar surface" rather than "match
   the sidebar's color," per the 38 occurrences in §2 — most are almost
   certainly fine as `bg-sidebar` still, since that utility can keep emitting
   `var(--sidebar)` for anything that isn't the sidebar's own root panel;
   confirm which of the 38 actually need to move to the gradient image vs.
   stay a flat color read from the (now-decoupled) `--sidebar`).

### Phase B — One real gradient preset, no picker changes

Prove the mechanism (§4) with one preset, the same "prove it before scaling"
approach the shipped Phase 2 used.

### Phase C — Swatch rendering + contrast test extension

Picker shows the gradient truthfully (§1.2); contrast test covers both stops
(§3).

### Phase D — The curated gradient set + docs

Author the full set matching what the operator actually wants from the
reference screenshots; update `docs/design-system/color.md` and
`docs/brand/README.md` (whose "the one hue the product owns" language,
`roadblocks.md` R5, already needed revisiting for flat presets and needs it
more so for a full chrome recolor).

## 7. Cheap vs. large — the operator's actual decision

**Cheap corrections** (hours, same mechanism as what shipped):
- The gradient rendering approach itself (§4) — it's the existing three-layer
  pattern with one more token pair.
- The swatch preview change (§1.2).

**Genuinely larger** (the real redesign):
- The sidebar/card/popover decoupling (§2) — required before anything else
  here can start, and touches a token three other semantic roles depend on.
- The contrast test extension (§3) — up to ~6x today's check count.
- The brand-doctrine rewrite this forces (§6 Phase D) — "violet is the one hue
  the product owns" was already strained by flat presets; a full chrome
  recolor makes it require an actual copy/positioning decision, not just a
  reword.

Net: this is real, additional work — roughly on the same order as the shipped
feature, not a small follow-up — concentrated almost entirely in Phase A's
decoupling and the doctrine question in Phase D, not in the gradient mechanism
itself (which is cheap). The operator should decide whether the "exact" match
to Slack is worth that, or whether a smaller correction (e.g. flat single-color
presets that also recolor the sidebar via the same Phase A decoupling, without
gradients) gets most of the visual win in `full-chrome-theming.md` §4's
"cheap" column without landing in Phase D's harder doctrine question.
