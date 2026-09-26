# Addendum 2 — fix `--accent`/`--sidebar-accent` to actually follow the preset

**Status:** decided, 2026-09-26, extending `theme-system-decision.md` and
`theme-system-decision-addendum.md`. This is a bug fix on the original
shipped scope, not new scope — `--accent` was always intended to be
"brand-tinted" (its own file comment says so) but was wired to a fixed
primitive instead of the brand ramp, so it silently never moved.

## The bug

```
--accent: var(--surface-light-active);       /* oklch(0.942 0.0256 293.15) */
--accent-foreground: var(--ink-light-primary);
...
.dark { --accent: var(--surface-dark-active); /* oklch(0.2396 0.019 284.87) */ }
```

`--surface-light-active`/`--surface-dark-active` are fixed primitives,
independent of `--brand-*`. `--accent` backs the hover/rest tint under menu
rows, list items and nav (`bg-accent`, ~40 call sites) — including the
command palette's selected-row highlight, which is what surfaced this: it
stays violet-ish regardless of which accent preset is active, while
`--primary`/`--ring`/`--sidebar-primary` correctly follow it. Same bug in
dark mode's `--sidebar-accent` (`var(--surface-dark-active)`, not
brand-derived) — light mode's `--sidebar-accent` is fine, it already uses
`var(--brand-100)`.

## Fix

Same hue-substitution mechanism as the background/chrome addendum, two more
anchor pairs, same safety argument (chroma is low — 0.0256/0.019 — L/C
untouched, only hue substitutes):

- `--accent` (light anchor: L=0.942 C=0.0256; dark anchor: L=0.2396 C=0.019)
- `--sidebar-accent`, dark only (same dark anchor as `--accent` above — it's
  the same primitive today, so reuse the computed value rather than a
  second computation)

Light mode's `--sidebar-accent` already correctly follows (`--brand-100`) —
leave it untouched.

## What does NOT change

Same boundary as every prior step: `--card`, `--popover`, `--muted`,
`--border`, ink/text, status, identity, chart. `--accent-foreground` also
stays untouched (`--ink-*-primary` — neutral text on the tinted hover, by
original design, so hover text never strobes a color as you move the mouse).

## Verification

- Default reproduces today's exact `--accent`/dark `--sidebar-accent` values
  — zero-pixel-change, same bar as every prior step.
- Sweep presets + custom hues, confirm via `getComputedStyle` that `--accent`
  and dark `--sidebar-accent` shift together with `--primary`, while
  `--card`/`--popover`/`--muted`/`--accent-foreground` stay fixed.
- Real screenshot: open the command palette (⌘K) under at least 2 different
  accents and confirm the selected-row highlight matches the active accent,
  not violet.
