# Test plan

Specification only — no tests are written in this PR. Modelled on the two
existing theme tests:

- `frontend/test/unit/shell-chrome-tokens.test.ts` — reads `index.css` as text
  in the node environment, brace-matches a block, and asserts declarations.
  Guards the **shape** of the token layer.
- `frontend/test/e2e/theme-toggle-visible.spec.ts` — pins the stored theme with
  `page.addInitScript` **before** the app boots, so the first paint is the one
  under test, then asserts geometry and classes in a real browser.

Unit tests run under `vitest` with `environment: "node"` and
`include: ["test/unit/**/*.test.ts"]` (`frontend/vitest.config.ts:48-52`); a
file that needs a DOM opts in with a `@vitest-environment jsdom` docblock.

## Unit

### U1 — `chart-1` and `kg-accent` no longer follow the ramp (Phase 1)

In `shell-chrome-tokens.test.ts` style, extract `:root` and `.dark` and assert
`--chart-1` and `--kg-accent` resolve to `var(--signature-500)` /
`var(--signature-400)`, and that no preset block declares `--signature-*`.
Note the `.dark { --kg-* }` bridge is a **second** `.dark` block
(`index.css:831`); the `block()` helper returns the first, so the `--kg-accent`
assertion needs a helper that finds every `.dark {` block, or a selector
anchored on the comment above it.

### U2 — registry integrity (Phase 2) · `accent-presets-registry.test.ts`

- ids are unique, non-empty, and match `/^[a-z][a-z0-9-]*$/` (safe inside an
  attribute selector without escaping);
- exactly one entry is `DEFAULT_ACCENT_PRESET`, and it has **no** CSS block;
- every non-default id has exactly one `[data-accent-preset="<id>"] {` block in
  `index.css`, and every such block has a registry entry (no orphans either
  way);
- the registry source contains no colour literal — no `#hex`, `oklch(`,
  `rgb(`, `hsl(` — covering the token-gate gap in `roadblocks.md` R7.

### U3 — preset block shape (Phase 2)

For every preset block:

- declares **all ten** `--brand-(50|100|…|900)` and nothing else in the
  unqualified block (`roadblocks.md` R2) — matched as
  `--brand-(50|[1-9]00)\b` so `--brand-discord` et al. are not swept in (R6);
- each value is an `oklch(L C H)` literal with a hex trailing comment
  (`index.css:40-45` convention);
- any `-foreground` override appears only under `:not(.dark)` / `.dark`
  qualified selectors, in pairs, and its values are `var(--…)` references;
- the preset section starts **after** the last `:root {` and `.dark {` block
  (source order is what makes it win, `architecture.md` §3; and it keeps
  `block()`-style lookups honest, `roadblocks.md` R14).

### U4 — contrast gate (Phase 4) · `accent-presets-contrast.test.ts`

For every preset **and for the default ramp in `:root`**:

1. Parse the ten oklch values; convert oklch → OKLab → linear sRGB (Björn
   Ottosson's published matrices), fail if any channel is outside [0, 1]
   beyond a 1e-4 tolerance (gamut).
2. Compute WCAG 2.1 ratios with the formula in `docs/design-system/color.md:11-17`
   and assert ≥ 4.5:1 for each pair in `architecture.md` §9. Foreground and
   ground values come from the primitives the semantic tokens name, resolved
   through the preset's mode-qualified override when there is one.
3. Report the measured table on failure, so the author sees which pair and by
   how much.

Do **not** assert `brand-500` on `--accent` or `--chrome`: the default already
fails those (4.20 / 4.22, `roadblocks.md` R12). Record that as a known gap in
the test's doc comment and file it separately.

### U5 — storage and apply (Phase 2) · `accent-presets-storage.test.ts`, jsdom

- nothing stored → attribute absent;
- stored known id → `document.documentElement.dataset.accentPreset === id`;
- stored unknown id (a retired preset) → attribute absent, no throw;
- `localStorage.getItem` throwing → attribute absent, no throw (the
  `crash-fallback.tsx:61-63` case);
- selecting default **removes** the attribute rather than setting `"default"`;
- a `storage` event for the key re-applies; one for `"theme"` does not;
- writes go to `oc.appearance.accentPreset` and never touch `"theme"`.

### U6 — accessible names (Phase 3)

Add a case to `frontend/test/unit/accessibility-control-names.test.ts`, which
asserts on component source text: the picker renders `role="radiogroup"` with
an `aria-label`, and each swatch is `role="radio"` with `aria-checked` and its
label as visible text. E3 covers the same contract in a real browser via
`getByRole("radio", { name })`.

## End-to-end (Playwright)

All in a new `frontend/test/e2e/accent-preset.spec.ts`, reusing the tour-skip
`addInitScript` from `theme-toggle-visible.spec.ts:39-46`, and run for both
`"light"` and `"dark"` by pinning `"theme"` the same way that spec does
(`:57-65`).

### E1 — no flash from default to chosen (Phase 2)

The load-bearing test. With the key pinned via `addInitScript`, install —
also via `addInitScript`, so it exists before any app code — a
`MutationObserver` on `#root` that records
`document.documentElement.dataset.accentPreset` the first time `#root` gains a
child that is not `#boot`. Assert the recorded value equals the pinned id.
This fails if the apply ever moves into an effect.

### E2 — resolved colours (Phase 3)

After load, read `getComputedStyle(document.documentElement)` for
`--brand-500`, and the computed `background-color` of a real primary button,
and assert they match the preset's declared value — in both modes. Also assert
a `--status-running` dot and a chart-1 mark (Usage view or the styleguide) did
**not** change versus the default: the scope rule, tested.

### E3 — picker round trip (Phase 3)

Open `/#/settings/appearance`, choose a preset by its visible label, assert the
`<html>` attribute and the stored key, reload, assert both survive. Choose the
default, assert the attribute is gone. Assert the Theme control still works
afterwards (the two are independent).

### E4 — swatch shows its own colour (Phase 3)

With the page on preset A, assert swatch B's computed background equals B's
`--brand-500` (light) / `--brand-400` (dark), not A's. Catches the
`bg-primary` mistake in `roadblocks.md` R4.

### E5 — visual (optional, Phase 4)

If the `e2e:visual` lane (`frontend/package.json`, `PW_VISUAL=1`) is used,
snapshot `#/styleguide` once per preset per mode.

## CI selection

Per `CLAUDE.md`, a new target is not covered until a CI job selects it.
Confirm the Console E2E lanes pick up `test/e2e/accent-preset.spec.ts` by glob
rather than by list before counting it as coverage, and confirm the unit files
match `test/unit/**/*.test.ts`.
