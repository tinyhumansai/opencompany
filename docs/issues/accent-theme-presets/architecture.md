# Architecture

All paths are relative to the repository root. Line numbers were read on
`upstream/main` at `cb81b500c` (2026-09-25).

## 1. How colour flows today

`frontend/src/index.css` is the only place a colour is declared, in three
layers (header at `index.css:14-33`):

1. **Primitives** on `:root`. The brand ramp is `--brand-50..900` at
   `index.css:55-64`, in oklch, with the canonical hex in a trailing comment.
2. **Semantics** on `:root` (light, `index.css:187-443`) and `.dark`
   (`index.css:445-582`). These point at primitives with `var()`.
3. **Utilities**: `@theme inline` (`index.css:593-713`) mints Tailwind classes
   that reference the semantic variable by name. `inline` is load-bearing: it
   makes the utility emit `var(--x)` instead of baking a value in at build time.
   Verified against a built stylesheet: `.bg-brand-500{background-color:var(--brand-500)}`
   and `.bg-primary{background-color:var(--primary)}`.

`next-themes` toggles `.dark` on `<html>` (`frontend/src/main.tsx:52-58`,
`attribute="class"`). Both `:root` and `.dark` therefore match the **same
element**, `<html>`, and that is the element every semantic token is computed
on.

### What depends on the brand ramp

Found by grepping `--brand-` and `brand-(50|[1-9]00)` across `frontend/src`
and `frontend/pages-sdk`:

| Consumer | Light | Dark | Evidence |
| --- | --- | --- | --- |
| `--primary` | `brand-500` | `brand-400` | `index.css:203`, `:467` |
| `--ring` | `brand-500` | `brand-400` | `index.css:250`, `:497` |
| `--chart-1` | `brand-500` | `brand-400` | `index.css:324`, `:532` |
| `--sidebar-primary` | `brand-500` | `brand-400` | `index.css:334`, `:553` |
| `--sidebar-accent` | `brand-100` | neutral active rung | `index.css:341`, `:555` |
| `--sidebar-accent-foreground` | `brand-700` | `brand-300` | `index.css:343`, `:556` |
| `--sidebar-ring` | `brand-500` | `brand-400` | `index.css:345`, `:558` |
| `--shadow-brand` | mix of `brand-500` | mix of `brand-400` | `index.css:368`, `:574` |
| `--glow-brand-card` | mix of `brand-500` | mix of `brand-400` | `index.css:388-392`, `:577-581` |
| `--kg-accent` (graph "AI agents") | `brand-500` | `brand-400` | `index.css:811`, `:832` |
| `bg-brand-*` utilities | direct | direct | `index.css:639-648` |
| `text-brand-700 dark:text-brand-300` | direct | direct | `frontend/src/views/room/MessageComposer.tsx:869` |
| Styleguide swatches | direct | direct | `frontend/src/views/StyleguideView.tsx:314-323` |

Nothing in `.ts`/`.tsx` hard-codes a brand hex or an `oklch(` literal (grep
for `oklch(|hsl(|rgb(` in `frontend/src` returns nothing). The favicon,
manifest and `<meta name="theme-color">` use the neutral tile `#191919`, not
violet (`frontend/index.html:31`, `frontend/public/site.webmanifest:8-9`), so
they are unaffected.

The `--brand-discord*`, `--brand-chargebee` and `--brand-paypal-*` tokens
(`index.css:403-442`) share the prefix but are third-party colours and must
never be touched by a preset. See `roadblocks.md` R6.

## 2. What a preset overrides

**Decision: a preset overrides the ten ramp primitives `--brand-50` …
`--brand-900`, and nothing else by default.**

Because every consumer in the table above goes through the ramp, overriding
the ramp re-derives all of them in both modes for free: the light/dark choice
of *which step* a role uses (500 vs 400, 700 vs 300) stays in the semantic
layer where it already lives, and the preset only supplies the steps.

The narrower alternative — overriding `--primary`, `--ring`,
`--sidebar-primary` and their foregrounds directly — was rejected for three
reasons:

1. It would have to be written twice per preset (`:root` and `.dark`), and
   every future brand-derived semantic token would need a matching override in
   every preset or silently stay violet.
2. It misses the consumers that use a step directly: `--sidebar-accent`
   (`brand-100`), `--sidebar-accent-foreground` (`brand-700`/`brand-300`), the
   `bg-brand-*` utilities, and `MessageComposer.tsx:869`.
3. It breaks the three-layer contract (`docs/design-system/README.md`): a
   preset would put values in the semantic layer.

### Optional foreground overrides

A ramp alone is not always enough. White on `brand-500` measures 5.00:1 for
violet, but for plausible Slack-style hues it does not clear 4.5:1 (measured
with the formula in `docs/design-system/color.md:11-17`: green `#16A34A` 3.30,
amber `#D97706` 3.19, teal `#0D9488` 3.74). A preset may therefore also set
`--primary-foreground` and `--sidebar-primary-foreground` — but **only in
mode-qualified pairs**, because of the specificity trap in `roadblocks.md` R2:

```css
[data-accent-preset="banana"]:not(.dark) {
    --primary-foreground: var(--ink-light-primary);
    --sidebar-primary-foreground: var(--ink-light-primary);
}
[data-accent-preset="banana"].dark {
    --primary-foreground: var(--surface-dark-bg);
    --sidebar-primary-foreground: var(--surface-dark-bg);
}
```

Values are always `var()` references to existing primitives, never literals —
the same rule `test/unit/shell-chrome-tokens.test.ts:78-87` enforces for the
chrome tokens. The preferred fix for a failing foreground is still a darker
500, not a dark foreground; the override exists for hues (yellow, lime) where
no usable 500 carries white text.

### The default preset

The default preset is **whatever `:root` declares**, and it has no CSS block of
its own. Its id is `"default"`; selecting it *removes* the attribute from
`<html>`. Consequences:

- Choosing the default colour later (deferred — see `README.md` non-goals) is
  an edit to `index.css:55-64` and nothing else.
- A browser with nothing stored, or with an id this build does not know,
  renders exactly what ships today.

## 3. The CSS shape

A new section at the **end** of `index.css`, after the knowledge-graph bridge,
titled `ACCENT PRESETS`, one block per preset:

```css
[data-accent-preset="lagoon"] {
    --brand-50: oklch(…);  /* #…… */
    …
    --brand-900: oklch(…); /* #…… */
}
```

Why `[data-accent-preset="…"]` and not `:root[data-accent-preset="…"]`:

- The attribute selector alone matches **any** element, which is what lets a
  swatch in the picker render its own preset (§10) by carrying the attribute
  itself. `:root[…]` would match only `<html>`.
- Its specificity (0,1,0) ties with `:root`, so it wins on **source order** —
  which is why the section must come after the `:root` primitives block. A
  unit test pins the order (`test-plan.md` U3).

Every value is oklch with the hex in a trailing comment, matching the existing
ramp convention (`index.css:40-45`), and every step must be in sRGB gamut.

## 4. Scope: what moves with a preset and what does not

Slack's theme picker (as of its 2023 redesign, from product knowledge — not
re-verified for this document) recolours the workspace frame, the sidebar and
the selected-item highlight. It does **not** recolour the green presence dot,
the red mention badge, or error states: those are signals, and a signal that
changes colour with personal taste stops being one. This console's own brand
doctrine says the same thing more strictly: "Do not reuse a status hue for
anything that is not that status" (`docs/brand/README.md:129-131`).

| Token family | Moves? | Why |
| --- | --- | --- |
| `--primary`, `--ring`, `--sidebar-primary`, `--sidebar-ring`, `--sidebar-accent(-foreground)`, `--shadow-brand`, `--glow-brand-card`, `bg-brand-*` | **Yes** | This is the interaction hue — the thing a preset is. |
| `--status-*` | **No** | Closed vocabulary. Already independent of brand (`index.css:257-275`), so nothing to do. |
| `--tone-1..5` (identity) | **No** | Hash-assigned identity. Already independent. |
| `--chart-2..5` | **No** | Already independent (`index.css:325-328`). |
| `--chart-1` | **No — pin it** | Today it follows brand. See below. |
| `--kg-accent` (graph "AI agents") | **No — pin it** | Today it follows brand. See below. |
| Neutral surfaces / ink | **No (v1)** | See `open-questions.md` Q4. |
| `--destructive`, `--highlight` | **No** | Semantic, already independent. |

**Why pin chart slot 1 and the graph accent.** Both currently resolve through
the brand ramp. Chart slots 2–5 are cyan, green, amber and pink; the graph's
other node classes are pink/cyan (memory stores), green (ok) and amber (people,
`--kg-warn`) (`index.css:811-838`, `KnowledgeGraph.tsx:56` labels
`var(--accent)` "AI agents"). Any Slack-like preset — Jade, Lagoon,
Clementine, Banana — lands on one of those hues and makes two chart series, or
AI agents and people, the same colour. Constraining preset hues to avoid them
would leave almost nothing to choose from. Pinning is cheaper and also right on
the merits: a chart or a graph legend should read the same on every operator's
screen, including a screen being shared.

Mechanism: add two preset-independent primitives with today's exact values —
`--signature-500` (= current `--brand-500`) and `--signature-400` (= current
`--brand-400`) — and repoint `--chart-1` (`index.css:324`, `:532`) and
`--kg-accent` (`index.css:811`, `:832`) at them. This is a zero-pixel change on
the day it lands, and a preset never touches `--signature-*`. The name is a
placeholder; if the default-colour decision replaces violet, whether these
follow is part of that decision (`open-questions.md` Q3).

## 5. The TypeScript registry

`frontend/src/lib/accent-presets.ts`:

```ts
/** One curated accent preset. Colours live in index.css, never here. */
export interface AccentPreset {
  /** Stable id: the `data-accent-preset` value and the stored value. Never renamed. */
  readonly id: string;
  /** What the picker shows. */
  readonly label: string;
}

export const DEFAULT_ACCENT_PRESET = "default";
export const ACCENT_PRESETS: readonly AccentPreset[] = [
  { id: DEFAULT_ACCENT_PRESET, label: "…" },
  { id: "lagoon", label: "Lagoon" },
  // …
];
```

It carries **no colour values**, so `scripts/ci/assert-design-tokens.sh`
passes it with no exemption (§11). Ids are persisted in users' browsers, so —
like the `TEAM_TONES` keys (`docs/design-system/color.md`, "Legacy slot
names") — an id is never renamed; a retired preset's id falls back to default.

## 6. Applying it before first paint

**Decision: a synchronous call in `main.tsx`, before `mount()`. No inline
script, no new public file.**

```ts
// main.tsx, beside purgeStoredSmtpPasswords() at line 96
applyStoredAccentPreset();
mount();
```

`applyStoredAccentPreset()` reads `oc.appearance.accentPreset` inside
`try/catch`, validates it against `ACCENT_PRESETS`, and sets or removes
`document.documentElement.dataset.accentPreset`.

Why this is flash-free, step by step for a production build:

1. The HTML arrives. The only thing painted before the bundle runs is the
   `#boot` placeholder (`frontend/index.html:116-135`), which uses inline grey
   styles and no accent at all, so it cannot show the wrong one.
2. `index.css` is a render-blocking `<link rel="stylesheet">` in `<head>`
   (verified in a built `dist/index.html`), so the preset blocks are parsed
   before any app pixel.
3. `main.tsx` is a module script (`index.html:137`). Its top-level statements
   run in order; the attribute is set before `mount()` calls `createRoot().render()`.
4. React's first commit therefore already sees the chosen preset.

Why not the approaches that look more obvious:

- **`next-themes`' own trick does not work here.** It renders an inline
  `<script>` through `dangerouslySetInnerHTML`
  (`frontend/node_modules/next-themes/dist/index.mjs`, component `_`), which is
  a server-rendering technique: React deliberately creates client-rendered
  `<script>` elements inert. In this SPA the class is applied from an
  **effect**, which `frontend/src/components/crash-fallback.tsx:21-27` and
  `frontend/index.html:107-111` both already acknowledge. See `roadblocks.md`
  R1 — dark mode has this flash today.
- **An inline `<script>` in `index.html`** is blocked in the desktop build:
  `crates/opencompany-app/tauri.conf.json` sets `"script-src 'self'"` in the
  production CSP. (Tauri may hash bundled inline scripts at build time; that is
  not verified here and should not be relied on.)
- **A static `frontend/public/accent-boot.js`** would be served with
  `public, max-age=31536000, immutable` because it is not HTML
  (`crates/opencompany-core/src/server/routes.rs:78-93`), yet it is not
  content-hashed. A change to it would never reach an existing browser. See
  `roadblocks.md` R3.

The crash fallback (`crash-fallback.tsx:110`) needs nothing: the attribute is
on `<html>` before React exists, so the fallback inherits it.

## 7. Persistence

- **Key:** `oc.appearance.accentPreset`. The `oc.<area>.<name>` convention is
  what the console already uses (`oc.connections.v1`,
  `oc.presence.override`, `oc.workspace.listWidth`,
  `oc.workflows.indexMode`). It cannot collide with `next-themes`' `"theme"`
  key (`storageKey` default in its source; read directly by
  `crash-fallback.tsx:67`).
- **Value:** the preset id string, never colours. An unknown or missing id
  means default.
- **Every access guarded:** `localStorage` throws on read in a browser with
  site data blocked (`crash-fallback.tsx:61-63` makes the same point).
- **Cross-tab:** listen for the `storage` event on that key and re-apply, as
  `next-themes` does for its own key.
- **Scope:** per origin. In the desktop build the webview has one origin for
  every host it connects to, so the choice is per install. In a browser each
  host origin (each hosted tenant, each local port) keeps its own — consistent
  with the Appearance page's "a fact about this browser" framing
  (`AppearanceView.tsx:4-9`, `settings-pages.ts` group `"console"`).

## 8. The backend seam, concretely

There is no per-user **preference** storage in the host today. There is a
per-user **self-edit** seam that fits one:

- `PATCH …/auth/me` (`crates/opencompany-core/src/server/users/routes.rs:82`,
  handler `edit_me` at `:1091`) authorises on *being* the user, with no user id
  in the path, and already takes double-option fields (`EditMe`, `:1069`).
- `UserRecord` (`crates/opencompany-core/src/ports/users.rs:61`) is persisted as
  a JSON blob in all three stores (`store/sqlite.rs:1874` writes `user_json`;
  `store/mongodb.rs:1923` likewise; `store/fs_ops.rs:272`), and optional
  fields already use `#[serde(default, skip_serializing_if = …)]`
  (`ports/users.rs:87`, `:97`). A `preferences: Option<UserPreferences>` field
  therefore needs **no migration** in any backend.

What that seam would need, and why it is not v1: see `roadblocks.md` R9 (the
record is per company, admins can read it, the write is a whole-record
replace, and it refuses during `must_change_password`).

The frontend should be written so the swap is local. `accent-presets.ts`
exports a tiny store — `read(): string`, `write(id)`, `subscribe(cb)` — with a
`localStorage` implementation. A server-backed version would hydrate from
`GET …/auth/me` and write through to the server **and to `localStorage`**:
the boot apply in §6 must be synchronous, and no network read can happen
before first paint, so `localStorage` remains the boot cache under any backend.

## 9. Contrast discipline

Every preset must clear the pairs `docs/design-system/color.md:48-56` already
documents for violet, measured with the WCAG formula at `color.md:11-17`:

| Pair | Bar | Violet today |
| --- | --- | --- |
| `--primary-foreground` on `brand-500` | 4.5 | 5.00 |
| `brand-500` on light canvas `#F7F7FC` | 4.5 | 4.68 |
| `brand-400` on dark canvas `#08090B` | 4.5 | 6.08 |
| `brand-700` on `brand-100` (active nav, light) | 4.5 | 6.29 |
| `brand-300` on dark active rung `#1E1E28` | 4.5 | 7.10 |
| every step in sRGB gamut | — | yes |

(Values re-measured on 2026-09-25 with the `color.md` snippet. Note the
`brand-700` on `brand-100` row: `color.md:55` says 6.91 against the old active
rung; the sidebar now uses `brand-100` and `index.css:340` says 6.29, which
matches this measurement.)

**Automated in v1**, as a node-environment vitest (`test-plan.md` U4). It is
feasible without a dependency: the test reads `index.css` as text — exactly
how `test/unit/shell-chrome-tokens.test.ts:31-59` already does — extracts each
preset block's `oklch(L C H)` values, converts oklch → OKLab → linear sRGB
(two 3×3 matrices and a cube, ~25 lines), checks gamut, and computes the WCAG
ratio. Authoring rule for humans: tune in oklch, keep the ramp's lightness
cadence from `index.css:52-54`, and treat the test as the gate, not a reviewer's
eye.

## 10. The picker

A second `Card` on `AppearanceView.tsx`, below the Theme card, titled "Accent".
A `role="radiogroup"` of swatches, each a button with `role="radio"`,
`aria-checked`, and the preset's label as visible text (colour must not be the
only cue). Each swatch carries `data-accent-preset={id}` on itself and paints
with `bg-brand-500 dark:bg-brand-400`: since those utilities emit
`var(--brand-500)` directly (§1), they resolve against the swatch's own
attribute. **They must not use `bg-primary`** — see `roadblocks.md` R4.

Switching presets must suppress transitions for one frame, as
`disableTransitionOnChange` does for the mode (`main.tsx:56`), or every
`transition-colors` element in the console animates at once. `next-themes`
does not export its helper, so write the ~8-line equivalent.

## 11. CI token gate

`scripts/ci/assert-design-tokens.sh` scans only `frontend/src/**/*.{ts,tsx}`
(`SRC=frontend/src`, `INCLUDES=(--include=*.tsx --include=*.ts)`) for four
patterns: arbitrary `text-[…]` sizes, `fixedLabel(<10)`, raw Tailwind palette
classes, and 6-digit hex `#[0-9a-fA-F]{6}` — with `frontend/src/lib/connections.ts`
the one hex exemption.

- `index.css` is **not scanned**, so the preset blocks (oklch plus hex
  comments) pass as the existing ramp does.
- `accent-presets.ts` carries no hex and no palette class, so it passes with
  **no exemption**. If a later need (say, a native Tauri tint) ever forced hex
  into TypeScript, the exemption would be one more
  `grep -v '^frontend/src/lib/accent-presets.ts:'` line beside the
  `connections.ts` one — but that should be argued, not assumed.
- The gate only matches 6-digit hex; an `oklch(…)` literal in a `.ts` file
  would pass it silently (`roadblocks.md` R7). The registry must not rely on
  that gap.
