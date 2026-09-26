# Addendum — auto-tinted canvas and chrome

**Status:** decided, 2026-09-26, extending `theme-system-decision.md`. Adds one
more surface to the accent mechanism; changes nothing else already shipped.

## Decision

The console canvas (`--background`) and window chrome (`--chrome`) pick up
the same hue as whatever accent is currently active — curated preset or
Custom slider — instead of always sitting at a fixed violet-adjacent hue.
**Lightness and chroma are untouched**; only the hue channel substitutes, the
same mechanism the accent ramp and the Custom slider already use.

## Why this is safe (verified against real values, not assumed)

Every low-chroma neutral this touches is already clustered near one hue in
`index.css` today:

| token | current value | note |
|---|---|---|
| `--surface-light-bg` | `oklch(0.9776 0.0066 286.28)` | |
| `--surface-light-chrome` | `oklch(0.9427 0.012 286.17)` | |
| `--surface-dark-bg` | `oklch(0.1395 0.0048 262.8)` | |
| (dark chrome resolves to `--surface-dark-2`) | `oklch(0.1865 0.0044 264.46)` | |

Chroma is tiny (0.0044–0.012) in every case — a hue swap at this chroma is
barely perceptible on its own pixel, which is exactly the point: quiet, not
a wash. Selecting the **default/violet accent reproduces today's exact
values** (hue ≈286° already matches), so this is zero-pixel-change for the
default, same discipline as the Phase 1 `--signature-*` split.

## Why the semantic tokens, not the primitives

`--chrome` aliases `--surface-dark-2` in dark mode, and `--surface-dark-2`
also backs `--muted`. Retinting the *primitive* would silently drag `--muted`
along with it — scope creep the operator didn't ask for. Instead, compute
tinted values directly for the **semantic** tokens `--background` and
`--chrome` (both light and dark) as inline overrides, the same way the
Custom hue slider already overrides `--brand-*` directly rather than
touching a shared primitive. Nothing else that happens to share a primitive
value moves.

## What does NOT change

`--card`, `--popover`, `--muted`, `--border`, `--sidebar`, ink/text colours,
status, identity, and chart tokens — identical scope boundary as the
existing accent system and the declined full-chrome-theming path. This is
strictly "canvas + frame follow the accent hue," nothing wider.

## Mechanism

One function, `computeTintedNeutrals(hue): { backgroundLight, backgroundDark,
chromeLight, chromeDark }`, using the four anchor L/C pairs above with the
hue substituted. Called every time an accent is applied — curated preset or
Custom — alongside the existing `--brand-*` ramp application, setting
`--background`/`--chrome` as additional inline custom properties on `<html>`
(mode-qualified the same way the rest of the semantic layer is, i.e. the
`.dark` value applies under `.dark`). No new storage key — it derives from
whatever hue is already persisted for the active accent.

## Verification for whoever implements this

- Selecting the shipped default reproduces the exact current `--background`/
  `--chrome` computed values (zero-pixel-change), same bar as Phase 1.
- Sweep a range of hues and confirm `--card`, `--popover`, `--muted` never
  move (screenshot diff or `getComputedStyle`, not eyeballing).
- Contrast: `--foreground`-on-`--background` and the muted-ink-on-chrome
  pairs `color.md` already documents must still clear their bars for every
  preset and a swept range of Custom hues — reuse the existing shared
  contrast-evaluator function, extended with these two pairs.
