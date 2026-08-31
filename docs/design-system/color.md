# Colour

Every colour token, what it is for, and what it measures. Values are declared
in `frontend/src/index.css` in oklch; the hex on each row is the canonical
value for anything outside CSS (Figma, a slide, a favicon).

**Contrast figures are measured**, using WCAG 2.1 relative luminance against
the light canvas `#F7F7FC` or the dark canvas `#08090B`. They are not
estimates. Re-measure after any change:

```js
// hex → linear → relative luminance → ratio
const lin = c => (c/=255, c <= 0.04045 ? c/12.92 : ((c+0.055)/1.055) ** 2.4);
const lum = h => { const [r,g,b] = h.match(/\w\w/g).map(x => lin(parseInt(x,16)));
                   return 0.2126*r + 0.7152*g + 0.0722*b; };
const ratio = (a,b) => { const [x,y] = [lum(a),lum(b)].sort((p,q)=>q-p);
                         return (x+0.05)/(y+0.05); };
```

Targets: **4.5:1** for text, **3:1** for UI marks and large text.

---

## Brand ramp

Violet. The only hue the product owns. `--brand-*`, addressable as
`bg-brand-500` etc., though components should prefer the semantic names below.

| Token | Hex | Role |
| --- | --- | --- |
| `--brand-50` | `#F0ECFD` | Tint backgrounds |
| `--brand-100` | `#E4DDFC` | Tint backgrounds, borders on brand surfaces |
| `--brand-200` | `#CEC1FA` | Disabled brand fills |
| `--brand-300` | `#B09DF8` | Dark-mode active nav ink |
| `--brand-400` | `#937BF6` | **Dark-mode accent** — links, focus, primary |
| `--brand-500` | `#7153F0` | **The brand.** Light-mode accent and all filled brand buttons |
| `--brand-600` | `#6247D7` | Pressed state on brand fills |
| `--brand-700` | `#5038B2` | Light-mode active nav ink |
| `--brand-800` | `#3C2A89` | High-contrast ink on tint |
| `--brand-900` | `#2B1E62` | Reserved |

500 is the value the brand guide names. Every other step holds this ramp's
original lightness cadence and hue drift, with chroma scaled so 500 lands
exactly on it; every step is inside the sRGB gamut.

The ramp is theme-independent — 500 is 500 in both themes. What changes is
*which step the accent role points at*: 500 in light, 400 in dark, because 500
is too dense to read as ink on near-black.

| Pair | Ratio | Verdict |
| --- | --- | --- |
| white on `brand-500` | 5.00:1 | AA — filled buttons, both themes |
| `brand-500` on light canvas | 4.68:1 | AA — links |
| `brand-400` on dark canvas | 6.08:1 | AA — links, dark |
| `brand-700` on light active row | 6.91:1 | AA — active nav row |
| `brand-300` on dark active row | 7.10:1 | AA — active nav row, dark |

---

## Neutrals

Cool-tinted at ~286°, chroma 0.001–0.03. See
[`../brand/README.md`](../brand/README.md#neutrals-carry-the-brand) for why they
are not pure grey.

There is no single grey ramp. The brand guide draws light and dark as two
independent ladders whose rungs carry the same *roles* rather than the same
lightnesses, and the roles are what the semantic layer binds to — so the rung
is the primitive and the theme picks a set.

### Surface rungs

| Rung | Light | Dark | Role |
| --- | --- | --- | --- |
| `bg` | `#F7F7FC` | `#08090B` | Main content canvas |
| `1` | `#FFFFFF` | `#0C0D0F` | Sidebar · cards · panels |
| `2` | `#F1F1F8` | `#121315` | Panel stroke · date badge |
| `3` | `#E8E8F2` | `#131317` | Icon circles |
| `4` | `#DADAE5` | `#161719` | Card stroke · dividers |
| `active` | `#ECE9FC` | `#1E1E28` | The nav row you are standing on |

Declared as `--surface-light-*` and `--surface-dark-*`. Every value is the
brand guide's, unchanged.

### Ink levels

The five-level text hierarchy, `--ink-light-*` / `--ink-dark-*`, surfaced to
components as `text-ink-*`. Ratios are worst case across the three grounds text
actually sits on — `bg`, rung `1`, and `active`. Rungs 2–4 are strokes and icon
circles, not text grounds.

| Level | Light | Dark | Role |
| --- | --- | --- | --- |
| `primary` | `#0A0A14` 16.53:1 | `#FFFFFF` 16.52:1 | Active labels · channel headers |
| `secondary` | `#43435A` 8.05:1 | `#AEAEBB` 7.53:1 | Nav items · section labels · names |
| `tertiary` | `#4A535A` 6.59:1 | `#99A2AC` 6.38:1 | Descriptions · body text |
| `hint` | `#5A5E65` 5.47:1 | `#92929E` 5.37:1 | Subtitles · empty-state prompts |
| `muted` | `#6A6973` 4.54:1 | `#858590` 4.53:1 | Member counts · metadata |

`primary` and light `secondary` are the guide's values. The rest are the
guide's hue and chroma at a corrected lightness: six of the ten levels as drawn
do not clear 4.5:1 against the surfaces the guide itself assigns them — dark
`tertiary`, marked *body text*, measured 2.91:1, and light `muted` measured
1.45:1.

Lifting each failing level by the smallest amount that clears the floor
collapses the ramp — all five land on 4.5:1 and become one colour. The ramp is
redistributed instead: the weakest level sits on the floor and the rest step up
in even increments of lightness, the same perceptual-uniformity argument the
brand ramp is built on. Hierarchy is preserved by *role*; the guide draws dark
`hint` brighter than dark `secondary`, which its own usage labels contradict.

Dark `hint` (`#8E9286`) and dark `muted` (`#62665C`) also move hue. They are
drawn at ~122° — green — while every other neutral in the guide sits near 285°,
and the guide's own subtitle reads "All cool-toned greys with purple
undertone".

---

## Semantic surfaces

Layer 2. These are what components use.

| Token | Light | Dark | Role |
| --- | --- | --- | --- |
| `--background` | rung `bg` | rung `bg` | The canvas |
| `--card` | rung `1` | rung `1` | Resting panels |
| `--popover` | rung `1` | rung `3` | Floating surfaces |
| `--muted` | rung `2` | rung `2` | Recessed fills, code, skeletons |
| `--secondary` | rung `3` | rung `active` | Secondary button fill |
| `--surface-icon` | rung `3` | rung `3` | Ground behind an icon circle |
| `--accent` | rung `active` | rung `active` | Hover/rest tint under rows |
| `--sidebar` | rung `1` | rung `1` | Mobile nav sheet, standalone switcher |
| `--sidebar-accent` | `brand-100` | rung `active` | Active nav row background |
| `--chrome` | `#EBEBF4` | rung `2` | The window chrome |
| `--chrome-border` | rung `4` | rung `active` | Where the content card meets it |
| `--border` | rung `4` | rung `4` | The hairline |
| `--input` | `#85858F` | rung `active` | Field borders, stronger rules |
| `--ring` | `brand-500` | `brand-400` | Focus |

Light surfaces climb *toward* the viewer with lightness (canvas `#F7F7FC` →
card white), and dark does the same (`#08090B` → `#0C0D0F` → `#161719`). A card
lifts off the page before any shadow is applied.

Dark borders were translucent white, so one value could read against the
canvas, the card and the popover at once. The guide names the stroke outright —
rung 4, "card stroke / dividers" — so they are opaque now, and the ladder keeps
them legible: every ground a border lands on sits clear of rung 4 in lightness.
That constraint is why the dark popover is rung 3 rather than rung 4 — a
surface painted the same rung as the stroke renders its own edge invisible,
which is what the translucent value used to prevent.

### The chrome, and why it is not a rung (issue #1178)

The console's shell is two layers. `--chrome` is the window frame: the surface
the sidebar column stands on and the margin the routed page's card floats in.
It is painted exactly **once**, on the shell root, and both regions are that one
surface showing through — the sidebar paints no fill of its own, and there is no
divider between the panes. Tinting each pane separately lands them on different
values and re-draws the seam the layout exists to remove.

It is deliberately not a rung of the surface ladder. Those six are *content*
surfaces — the canvas a page is drawn on and the panels stacked over it — and
this one sits behind all of them. Light needs a value the ladder does not
carry: darker than the canvas by enough to read as a layer, light enough that
`--ink-muted` still clears 4.5:1, since the sidebar's faintest labels sit on it.
Rung 2 separates less than the white sidebar it replaced; rung 3 drops muted ink
to 4.44:1. `#EBEBF4` is between them — **1.11:1** against the card, **4.57:1**
for muted ink.

Dark takes rung 2 and inverts the direction. The canvas is already the darkest
value in the theme, so "further back" cannot mean darker: `#030405` against
`#08090B` measures 1.03:1, which is not a layer, it is a rendering artefact.
`#121315` gives **1.07:1**, and the card's `--chrome-border` hairline (1.13:1
against the chrome, 1.21:1 against the card) and the dark `shadow-sm` inset
highlight carry the rest.

**`--sidebar-accent` moved for the same reason.** The `active` rung was tuned to
read on a white sidebar; against `--chrome` it measures 1.02:1 and the selected
nav row simply vanished. `brand-100` is the same tint one step deeper — 1.10:1
on the chrome, with `--sidebar-accent-foreground` at 6.29:1 on it. `--sidebar`
itself still names rung 1, and two surfaces still paint it: the mobile nav
sheet, which is an overlay dragged over the page rather than a pane of the
shell, and the standalone host switcher, which draws its own card on a console
that has no shell at all.

**Anything that cuts a hole in the chrome must ask for the chrome.** A `ring-2`
around a status dot is a cut-out of the ground behind it, not a decoration. The
two in the shell — `SidebarMenuDot`'s attention dot on the collapsed rail and
the host switcher's status dot — take `ring-chrome`, because that is what is
actually behind them now. `ring-sidebar` there paints a halo.

**`--accent-foreground` stays neutral.** 40 call sites pair `bg-accent` with
`text-accent-foreground`; brand text on every hover would make the console
strobe. The single place brand ink is spent is `--sidebar-accent-foreground` —
the nav row you are standing on.

### Text on surface

| Token | Light | Dark | Role |
| --- | --- | --- | --- |
| `--foreground` | 18.44:1 | 19.92:1 | Primary reading text |
| `--muted-foreground` | 5.07:1 | 5.46:1 | Secondary, metadata, captions |
| `--primary` | 4.58:1 | 5.98:1 | Links, emphasis |
| `--destructive` | 4.67:1 | 6.73:1 | Error text and marks |

`--muted-foreground` also measures 4.55:1 on `--muted`, so caption text on a
recessed fill still passes.

---

## Status

A closed vocabulary of five. Each ships three weights, because one value cannot
both fill a 6px dot and set legible 11px text.

- **`-mark`** — dots, bars, chart fills, icon glyphs. Target 3:1.
- **`-text`** — words. Target 4.5:1. Materially darker in light mode.
- **`-soft`** — the tinted background a badge sits on.

| State | Light mark | Light text | Dark (both) |
| --- | --- | --- | --- |
| **idle** | `#8C8C9E` 3.22:1 | `#6E6E80` 4.87:1 | `#9797A8` 6.80:1 |
| **running** | `#008DD0` 3.08:1 | `#0A6E9C` 5.50:1 | `#38BDF8` 9.12:1 |
| **blocked** | `#BF7200` 3.14:1 | `#A16207` 4.80:1 | `#FFC53D` 12.38:1 |
| **done** | `#009A49` 3.08:1 | `#0A7D3E` 5.10:1 | `#35C77F` 8.95:1 |
| **failed** | `#E5484D` 3.29:1 | `#C62A2F` 5.43:1 | `#FF6369` 6.73:1 |

The light marks clear 3:1 against every surface where they render, including
the muted and active fills. The light text weights remain darker, so a compact
label keeps the 4.5:1 text target without making the corresponding dot or bar
unnecessarily heavy.

In dark mode `-mark` and `-text` intentionally collapse to the same bright
value: on near-black it clears 4.5:1 on its own, and a separate text weight
would only be dimmer.

**Never rely on colour alone.** Roughly 1 in 12 men cannot separate the
red/green pair. Every status must also carry an icon, a label, or a position.

---

## Identity tones

A categorical palette for **who**, not what state: the tile behind a desk's
initials, a teammate's avatar, a thread's tint, a skill's category, a memory's
kind. Assigned by hash, so a name keeps its colour across reloads and carries
no meaning beyond "not the other one".

| Token | Mark | Light text | Dark text |
| --- | --- | --- | --- |
| `--tone-1` violet | `#8B5CF6` | `#6D28D9` 6.93:1 | `#C4B5FD` 10.58:1 |
| `--tone-2` blue | `#3B82F6` | `#1D4ED8` 6.54:1 | `#93C5FD` 10.83:1 |
| `--tone-3` teal | `#14B8A6` | `#0F766E` 5.34:1 | `#5EEAD4` 13.20:1 |
| `--tone-4` fuchsia | `#D946EF` | `#A21CAF` 6.17:1 | `#F0ABFC` 11.10:1 |
| `--tone-5` slate | `#64748B` | `#475569` 7.39:1 | `#CBD5E1` 13.16:1 |

**No amber, no green, no red.** That is the whole design of this palette.
Identity used to be drawn from the same Tailwind colours as status, which put
the collision the brand doc warns about directly into the product: a desk
keyed `emerald` wore the exact green that means "done", a skill filed under
Finance wore the red that means "failed", and every task-outcome memory looked
like a failed one.

Five rather than eight, because five hues clear of the status vocabulary is
what the hue circle has room for once brand and five states are spoken for. A
hash over five still distributes well.

**Where the hues do come close — identity violet against the brand, blue
against running cyan — form separates them:**

The brand moving from indigo to violet tightened this. `--tone-1` sits at 292.7°
and `--brand-500` now sits at 285.5°, roughly 7° apart where they were 14°
before. Form still separates them, and the two never take the same shape, but
`--tone-1` is the first thing to retune if identity and interaction start
reading as the same colour.

| | Shape | Carries |
| --- | --- | --- |
| **Identity** | A tile with initials | Who |
| **Status** | A pill or dot with a label | What state |

They never take the same shape. That rule is what makes the remaining hue
proximity safe, and it is the first thing to check when adding a component
that shows both at once.

### Legacy slot names

`TEAM_TONES` and the thread `TONES` map are keyed `sky`, `violet`, `amber`,
`emerald`, `rose`, `cyan`, `indigo`, `teal`. Those keys are **persisted
against desks and members and arrive from the host**, so they cannot be
renamed — they name a slot, not a colour. A desk keyed `amber` resolves to
`--tone-5` (slate), and that is correct.

## Charts

| Slot | Light | Dark |
| --- | --- | --- |
| `--chart-1` | `#7153F0` violet | `#937BF6` |
| `--chart-2` | `#008DD0` cyan | `#38BDF8` |
| `--chart-3` | `#009A49` green | `#35C77F` |
| `--chart-4` | `#BF7200` amber | `#FFC53D` |
| `--chart-5` | `#E93D82` pink | `#FF6BA6` |

Brand leads slot 1; the sequence then walks the hue circle so neighbouring
series never collide. The ordering is chosen so the *two-series* case — by far
the most common — gets violet and cyan, the pair that survives the most common
colour-vision deficiencies.

Chart colours are marks, not text. Axis labels and legends use
`--muted-foreground`, never the series colour.

---

## The knowledge graph

The Overview graph was ported with its own palette vocabulary (`--kg-*`,
plus unprefixed names inside `.oc-kg`). Rather than rename ~2000 lines, those
names are re-pointed at the semantic layer in `index.css`.

Two consequences: the graph now themes for free — it previously carried its own
hardcoded light/dark hex pairs — and the mapping is strictly one-way. Nothing
outside `.oc-kg` may use those names.

`--kg-brain-1` / `--kg-brain-2` stay deliberately outside the status
vocabulary: they identify *which store* a node came from, and colouring them
with status hues would imply a health they do not carry.

---

## Hardcoded colour debt

**Cleared.** Every colour in `src/` now resolves through a token. Verify with:

```sh
cd frontend
grep -rn '\(text\|bg\|border\|ring\|fill\|stroke\)-\(emerald\|rose\|amber\|sky\|red\|green\|blue\|yellow\|violet\|indigo\|teal\|cyan\|slate\)-[0-9]' src --include="*.tsx" --include="*.ts"
grep -rn '#[0-9a-fA-F]\{6\}' src --include="*.tsx" --include="*.ts"
```

The first returns nothing. The second returns only `src/lib/connections.ts`.

### The one file that keeps its hexes

`connections.ts` holds eleven third-party provider brand colours — Gmail's
red, Slack's aubergine, GitHub's near-black. They are correct as literals:
they identify *someone else*, and a themed approximation of Slack's purple
would be wrong in both themes. They are data about a third party, not a design
decision this system gets to make, and the field says so.

Discord's blurple is the same category but appears in markup rather than data,
so it is named: `--brand-discord`, `--brand-discord-hover`,
`--brand-discord-on-dark`. The token name is what stops a future cleanup
"fixing" it into the palette.

`--brand-chargebee` (`#ff3300`) joins it for the same reason — the provider
mark on Finance → Invoicing. One token where Discord needs three: as a mark
rather than a label it only has to clear 3:1, which it does on both grounds
(5.36:1 dark, 3.67:1 light), so neither theme needs its own step.

PayPal's monogram on Finance → Wallet needs five, because the mark is two
overlapping P's and their crossing is its own colour: `--brand-paypal-back`,
`--brand-paypal-front`, `--brand-paypal-overlap`, and themed
`--brand-paypal-back-on-dark` / `--brand-paypal-front-on-dark` /
`--brand-paypal-overlap-on-dark` pairs. Both P
colours move between themes to preserve contrast: the back P uses `#002991` in
light mode and `#008cff` in dark mode, while the front P uses `#0070ba` in light
mode and `#60cdff` in dark mode. The back P's dark value clears 3:1 at 5.74:1;
its light value would measure only 1.61:1 on the dark card. The front P is the
mirror case on the light ground: its light value clears 3:1 at 5.22:1 on the
white card, where its dark value would measure only 1.80:1. The crossing uses
`#0066a8` in light mode so it remains darker than `#0070ba` and preserves the
mark's depth cue; it clears 3:1 at 6.06:1. In dark mode it uses `#008cff`, the
back P's value, retaining the existing treatment there. A two-tone mark
also cannot take `currentColor`, so these are referenced as
`fill-(--brand-paypal-*)` on the paths rather than inherited from the call site.

Anything drawn on top of a provider colour must not assume a light or dark
ground — they span `#0F0F0F` to `#EA4335`.
