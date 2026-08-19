# OpenCompany design system

The implementation contract for the console's visual layer. The *why* lives in
[`docs/brand/README.md`](../brand/README.md); this directory is the *what*.

| Document | Covers |
| --- | --- |
| [`color.md`](color.md) | Every colour token, its role, and its measured contrast |
| [`typography.md`](typography.md) | The type scale, mono policy, and the 192-site migration list |
| [`components.md`](components.md) | Anatomy and required states for each shipped primitive |

**Source of truth:** `frontend/src/index.css`. These documents describe it; if
they ever disagree, the stylesheet wins and the document is a bug.

**Living reference:** run the console and open `#/styleguide`. It renders every
token by reading the variables at runtime, so it cannot drift from the
stylesheet — and it needs no host, company, or sign-in.

---

## The one rule

**Components may only use layer 3.**

The stylesheet is three layers, and nothing skips one:

```
1. PRIMITIVES   --brand-500, --surface-dark-2, --ink-light-hint, --green-mark
                Raw ramps. Theme-independent. Never referenced by a component.
                ↓
2. SEMANTICS    --primary, --border, --status-running
                What a colour means here. Light in :root, dark in .dark.
                ↓
3. UTILITIES    bg-primary, text-status-done-text, shadow-lg
                Tailwind classes. This is all a component may touch.
```

A component that reaches past layer 3 — into a ramp, or into an arbitrary
value like `text-[11px]` or `bg-[#5865f2]` — has made a decision the system
cannot see, cannot theme, and cannot change later. That is the entire failure
mode this structure exists to prevent.

**When the token you need does not exist, add it to layer 2.** Do not
approximate with a near-miss and do not inline a raw value. Naming the need is
the work.

---

## Anti-patterns, and what to do instead

| Instead of | Use | Why |
| --- | --- | --- |
| `text-[11px]` | `text-2xs` | 11px is a real rung of the scale; it now has a name |
| `text-[9px]` | `text-3xs` (10px) | Below 10px is illegible, not dense |
| `bg-[#5865f2]` | a named brand token | A raw hex cannot theme |
| `text-green-600` (Tailwind palette) | `text-status-done-text` | Palette colours carry no meaning and are untuned for this canvas |
| `shadow-lg` on a resting card | `border` + surface lightness | Elevation means "floats above the page" |
| A new accent hue | the existing violet | There is one accent |
| `text-status-done` for a *label* | `text-status-done-text` | Mark weights measure 3:1 — enough for a dot, not for words |

---

## Changing a token

1. **Change it in `index.css`**, in the semantic layer. Primitives change only
   when the brand itself changes.
2. **Check both themes at `#/styleguide`.** Dark is not a filter over light;
   several tokens are independently tuned.
3. **Re-measure contrast if it is a text or status colour.** The ratios quoted
   in [`color.md`](color.md) are measured, not estimated, and a change makes
   them stale. The helper used to produce them is described there.
4. **Update the affected document in this directory.**

Because every component reads layer 2, a correct token change propagates
everywhere at once — that is the payoff for the indirection.

---

## The Figma library

**File:** [OpenCompany Design System](https://www.figma.com/design/bUj8Ofz2EQL6Y8DU06zbDR)

Built from these tokens rather than eyeballed. Every swatch, specimen and
component is **bound to a variable**, so changing a variable's value redraws
the documentation pages that show it — a foundations page painted with hex
literals is a screenshot that lies the moment the system moves.

Binding is what keeps the file coherent internally. Keeping it in step with
*this repo* is a separate, manual job — see below.

| Collection | Variables | Contents |
| --- | --- | --- |
| `Primitives` | 39 | Brand ramp, neutral ramp, status hues. Scopes are `[]` — hidden from every picker, so designers must pick a semantic. |
| `Color · Light` | 32 | Semantic roles, each an **alias** to a primitive. No raw hex. |
| `Color · Dark` | 32 | Same names, independently tuned values. |
| `Scale` | 22 | Radius, spacing, and the font-size ramp. |

Plus **17 text styles** (`Body/`, `Label/`, `Heading/`, `Mono/`) with font size
bound to the `Scale` variables, and **12 effect styles** (`Elevation/` and
`Elevation Dark/`).

Every variable carries an explicit scope and a **WEB code syntax** naming the
real CSS variable — `color/status/running` reports as `var(--status-running)`
— so Dev Mode round-trips to this codebase rather than to an invented name.

### Keeping it in step — by hand, for now

**The codebase is the source of truth; Figma follows.** When a token changes
in `index.css`, update the matching variable's value in the Figma file — the
aliases and bound styles propagate the rest. Never fix a drift by editing a
swatch's fill: that puts a literal back into a file whose whole point is that
it has none.

There is no generator in this repo. One was written and removed before merge,
so the sync is manual and will drift the way manual syncs do. Two things make
that survivable:

- The variables are **aliased**, not copied. Changing a primitive moves every
  semantic that points at it, so a palette change is a handful of edits rather
  than 125.
- Every variable carries the real CSS variable name as its code syntax, so a
  drift is visible in Dev Mode rather than silent.

If the sync becomes a burden, the durable fix is a Figma development plugin
driving the Plugin API — it has no rate limit and no seat gate, unlike the
hosted MCP server, which allows **6 tool calls per month** on a Starter plan
with a View seat.

### Plan constraints worth knowing

The file was built on a Figma **Starter** plan, which forced three departures
from the standard structure. None is a design decision; all are reversible on
Professional:

1. **One mode per collection.** Light/Dark would normally be two modes of a
   single `Color` collection with a toggle. Instead they are two parallel
   collections sharing one set of names. Merging them into modes is the first
   thing to do after an upgrade.
2. **Three pages maximum.** The convention is one page per component; here the
   file is Cover / Foundations / Components, with sections carrying the
   separation.
3. **MCP call limit — 6 per month** on a View seat, which is what stopped the
   build. Button and Status Badge are in the file; Input, Badge, Alert, Avatar,
   Tab and Card are specified in [`components.md`](components.md) and not yet
   drawn. Drawing them needs either an upgraded seat or a plugin.

---

## Known debt

Catalogued rather than hidden.

| Debt | Sites | Status |
| --- | --- | --- |
| Arbitrary font sizes | 192 | **Cleared** — [`typography.md`](typography.md#migration) |
| Font sizes below 10px | 15 | **Cleared** — [`typography.md`](typography.md#sizes-below-the-scale) |
| Hardcoded hex colours | 26 | **Cleared** — [`color.md`](color.md#hardcoded-colour-debt) |
| Tailwind palette used for state | 87 | **Cleared** — [`color.md`](color.md#status) |
| Identity colours colliding with status | 6 maps | **Cleared** — [`color.md`](color.md#identity-tones) |
| Contrast failures in the running app | 3 | **Cleared** — see [Auditing](#auditing-the-running-app) |
| Geist Mono not installed | — | **Cleared** — [`typography.md`](typography.md#the-mono-face) |
| No vector logo asset | — | Open — [`../brand/README.md`](../brand/README.md#6-logo--marks) |
| Figma kept in step by hand | — | Open — [Keeping it in step](#keeping-it-in-step--by-hand-for-now) |
| Figma library covers 8 components | — | Open — [The Figma library](#the-figma-library) |

## Auditing the running app

Two Playwright tools measure contrast against the real console, because
reading tokens cannot catch what composition does to them. Both need a host
running (see `frontend/test/e2e/host.sh`) and take `SP=<dir>` to save
screenshots:

```sh
cd frontend
node test/tools/contrast-audit.mjs              # 9 views x 2 themes, at rest
node test/tools/contrast-audit-interactive.mjs  # dialogs, dropdowns, hover, focus
```

They found three failures a static reading of the tokens could not:

| Where | Was | Cause |
| --- | --- | --- |
| Sidebar Discord row, dark | 3.04:1 | `opacity-60` applied to a mid-tone hue |
| Sidebar Discord row, light | 4.20:1 | a fill colour used as text |
| Settings subtitle, light | 4.48:1 | muted text on the brand-tinted accent |

The pattern in all three is **composition**: every token passed on its own,
and the pairing failed. That is the class of bug this tooling exists for.

### Two ways these tools lied before they worked

Both worth knowing, because both reported a confident pass:

1. **An rgb-only colour regex.** `getComputedStyle` returns `oklch(...)` for
   every token here, so nearly every element was skipped and backgrounds fell
   back to white. The audit measured 4 nodes and called it clean.
2. **`page.goto()` to a hash-only URL is a same-document navigation.** The
   page never reloaded, `next-themes` never re-read the theme, and the "dark"
   pass was light mode a second time.

Hence both tools print how many nodes they measured. A vacuous run has to be
visible as one, or it reads as success.

### The two checks that keep it cleared

Both should return nothing but `connections.ts` (third-party brand colours,
which are correct as literals):

```sh
cd frontend
# No arbitrary values.
grep -rn 'text-\[' src --include="*.tsx" --include="*.ts"
# No raw palette colours, no raw hex.
grep -rn '\(text\|bg\|border\|ring\|fill\|stroke\)-\(emerald\|rose\|amber\|sky\|red\|green\|blue\|yellow\|violet\|indigo\|teal\|cyan\|slate\)-[0-9]' src --include="*.tsx" --include="*.ts"
grep -rn '#[0-9a-fA-F]\{6\}' src --include="*.tsx" --include="*.ts"
```

Worth a CI job. Debt of this kind returns one convenient exception at a time,
and a grep is cheaper than the argument.
