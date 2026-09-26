# Rollout plan

Phased so every phase is one small, reviewable PR that leaves `main` shippable
and changes nothing visible until the picker lands. Sized for a
brb-architectobot plan → brb-codecrusher implementation per phase. The tests
each phase adds are specified in [`test-plan.md`](test-plan.md) by id.

Before Phase 1: the operator answers `open-questions.md` Q2 (is a personal
interaction hue acceptable to the brand at all) and Q3 (pin chart 1 and the
graph accent). Everything else can be decided in flight.

---

## Phase 1 — Decouple what must not move (zero-pixel change)

Goal: after this, the brand ramp is consumed only by things that *should*
follow a preset.

1. In `frontend/src/index.css`, add primitives `--signature-500` and
   `--signature-400` beside the ramp (`index.css:55-64`), with **the exact
   current values** of `--brand-500` / `--brand-400` and a comment saying why
   they exist and that presets never override them.
2. Repoint `--chart-1` (`index.css:324`, `:532`) and `--kg-accent`
   (`index.css:811`, `:832`) at them.
3. Fix the stale contrast comment at `index.css:464-466` (4.70 → 5.00, per
   `roadblocks.md` R13).
4. Update `docs/design-system/color.md` "Charts" and "The knowledge graph"
   sections: slot 1 is a fixed violet, not "brand".

Tests: U1. Verification: `npm run typecheck`, `typecheck:unit`,
`typecheck:e2e`, `npm test`, `scripts/ci/assert-design-tokens.sh`; open
`#/styleguide` and the Overview graph in light and dark and confirm, with
screenshots, that nothing changed.

## Phase 2 — Preset plumbing, no UI

1. Add the `ACCENT PRESETS` section at the **end** of `index.css`
   (`architecture.md` §3), with **one** real preset to prove the mechanism.
   (The full curated set is Phase 4, once names and hues are decided.)
2. Add `frontend/src/lib/accent-presets.ts` (its tests go in
   `frontend/test/unit/`, the frontend's convention — the `_tests.rs` sibling
   rule in `CLAUDE.md` is for Rust):
   - `AccentPreset`, `ACCENT_PRESETS`, `DEFAULT_ACCENT_PRESET`;
   - `ACCENT_PRESET_STORAGE_KEY = "oc.appearance.accentPreset"`;
   - `readStoredAccentPreset()`, `applyAccentPreset(id)`,
     `applyStoredAccentPreset()`, `setAccentPreset(id)` (write + apply +
     one-frame transition suppression);
   - a `subscribe` for the `storage` event, and a `useAccentPreset()` hook on
     `useSyncExternalStore`.
   Every `localStorage` access in `try/catch`. Unknown ids resolve to default
   and **remove** the attribute.
3. In `frontend/src/main.tsx`, call `applyStoredAccentPreset()` immediately
   before `mount()` (`main.tsx:98`), with a comment citing `roadblocks.md` R1
   and R3 for why it is here and not in an inline or public script.

Tests: U2, U3, U5, E1. At this point the preset is reachable only by setting
the storage key by hand — which is exactly what E1 does.

## Phase 3 — The picker

1. `frontend/src/components/accent-preset-picker.tsx`: a `role="radiogroup"`
   of labelled swatches (`architecture.md` §10). Swatches paint with
   `bg-brand-500 dark:bg-brand-400` under their own `data-accent-preset`,
   **never** `bg-primary` (`roadblocks.md` R4). Keyboard: arrow keys move and
   select, as a native radio group does.
2. `frontend/src/views/settings/AppearanceView.tsx`: a second `Card` titled
   "Accent", **below** Theme (`roadblocks.md` R15). Update the file header
   comment, which currently describes a one-card page.
3. Update `settings-pages.ts` `hint` for `appearance` (currently "Light, dark,
   or follow the system") to mention the accent.

Tests: U6, E2, E3, E4. Verification: all three typecheck gates, `npm test`,
the token script, and the picker seen running in a real browser, in light and
dark, with screenshots — at the real preset count, not with one preset.

## Phase 4 — The curated set, the contrast gate, and the docs

1. Author the curated presets (target 6–8; names and hues are
   `open-questions.md` Q5) in oklch, following the ramp cadence rule at
   `index.css:52-54`. Add them to `ACCENT_PRESETS`.
2. Land the contrast test U4. Every preset must pass it; a preset that cannot
   pass with a white foreground gets the mode-qualified foreground override
   (`architecture.md` §2) or is dropped.
3. `StyleguideView.tsx`: show every preset's ramp so a reviewer sees them side
   by side, and reword the "Brand violet is the only hue" copy
   (`StyleguideView.tsx:351`, `:442`, `:545`) per the Q2 answer.
4. Docs, each kept under 500 lines:
   - `docs/design-system/color.md`: an "Accent presets" section — the rule
     (ramp-only, mode-qualified foregrounds), a per-preset contrast table, and
     how to add one. `color.md` is 381 lines today; if the tables push it past
     500, put them in a new `docs/design-system/accent-presets.md` and link it.
   - `docs/design-system/README.md`: the three-layer contract gains a line on
     presets overriding layer 1 only.
   - `docs/brand/README.md`: per the Q2 answer.

Tests: U4, E5 (visual, optional).

## Phase 5 — Future work (not v1; each its own issue)

- **Server-backed preference** via `PATCH …/auth/me`, after resolving the four
  constraints in `roadblocks.md` R9. `localStorage` stays the boot cache.
- **Pre-mount dark-mode apply**, fixing the existing flash (`roadblocks.md` R1,
  `open-questions.md` Q8) — cheap once Phase 2 exists.
- **Agent-authored pages** receive mode and preset over the existing
  `postMessage` bridge (`frontend/pages-sdk/client.ts`).
- **Custom accent** (free hex): needs a runtime ramp generator and a runtime
  contrast check against the same pairs as U4.
- **Vision-assistive presets**, which would have to move status colours too.
- **Tighten the token gate** to catch `oklch(` in `.ts`/`.tsx`
  (`roadblocks.md` R7).
- **`openpanel-init.js` caching** (`roadblocks.md` R3, adjacent finding).
- **Native desktop tint** only if the shell ever draws its own title bar again
  (`window-chrome.tsx:91`).

## Effort, roughly

| Phase | Size | Risk |
| --- | --- | --- |
| 1 | ~20 lines CSS + docs | Low; zero-pixel by construction |
| 2 | ~120 lines TS + ~15 CSS + tests | Medium; the FOUC ordering is the subtle part |
| 3 | ~150 lines TSX + tests | Low–medium; a11y of the radio group |
| 4 | ~15 lines CSS per preset + contrast test + docs | Medium; tuning hues to pass is design work |
