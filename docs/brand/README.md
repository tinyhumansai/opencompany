# OpenCompany brand guideline

The written source of truth for how OpenCompany looks and sounds. It is
deliberately short. Everything measurable — exact values, contrast ratios,
component states — lives in [`docs/design-system/`](../design-system/README.md),
and the values themselves live in `frontend/src/index.css`. This file is about
*why*, so that a decision the tokens do not cover can still be made correctly.

Renderable reference: run the console and open `#/styleguide`.

---

## 1. What we are

**OpenCompany is the operating layer for a company with a headcount of one.**

One person brings capital, taste, and the decisions that matter. A roster of
agents does every function, around the clock. The product is the runtime that
makes that a company rather than a pile of scripts.

Three things follow from that, and they drive every visual choice below.

**It is an instrument, not a destination.** Operators do not visit OpenCompany
to enjoy it. They come to see what their company did overnight, approve two
things, and leave. Every pixel is measured against that: does it help someone
decide faster? Decoration that does not is removed.

**It is dense on purpose.** A company generates more state than a person can
hold. The console shows a lot at once — that is the job, not a flaw to be
designed away with whitespace. The system's answer to density is hierarchy and
restraint, not fewer facts.

**The stakes are real.** These agents spend money, send email, and sign things.
Approval, failure, and cost must be unmistakable. This is why status colour is
a closed, protected vocabulary and never decorative.

### What we are not

Not a chatbot. Not a dashboard template. Not an AI product that signals "AI"
with gradients, glows, and purple sparkles. The intelligence is evident from
what the company *does*; the interface's job is to stay out of the way and be
legible at 7am.

---

## 2. Voice

**Plain, specific, and on the operator's side of the screen.**

Name things by what the operator controls, never by how the runtime is built.
They manage *approvals*, not `ApprovalSummary` records; they run a *workflow*,
not a DAG execution.

| Do | Don't |
| --- | --- |
| "Two runs are waiting on your approval." | "You have 2 pending approval requests!" |
| "The host rejected the credential. Reconnect and try again." | "Something went wrong 😞" |
| "Publish" → toast: "Published" | "Submit" → toast: "Success" |
| "No workflows yet. Create one to get started." | "Nothing to see here!" |
| "acme-marketing" | "Company #4832" |

**Rules that settle most arguments:**

- **Active voice, and the button says what happens.** A control named
  *Publish* produces *Published*. An action keeps its name through the whole
  flow — that consistency is how people learn their way around.
- **Sentence case everywhere.** Headings, buttons, labels, menu items. Title
  Case is not used anywhere in the product.
- **Errors explain and instruct; they never apologise.** Say what happened and
  what to do next. Never blame the operator, never be vague, and never leak
  the underlying error text (it is often attacker-influenced — see the
  `connectErrorMessage` precedent in `app-shell.tsx`).
- **Empty states are invitations.** State what will appear here and give the
  one action that fills it.
- **No filler and no cheerleading.** No "simply", "just", "seamlessly",
  "powerful", "revolutionary". No exclamation marks in UI copy.
- **Each element does one job.** A label labels. An example demonstrates.
  Nothing quietly does double duty.

**On agents:** refer to them by their role — "the recruiter", "the finance
desk" — not as "AI" or "the bot". They are staff. Use *they*, never *it*, and
never assume gender for a named agent or a person.

---

## 3. Colour

Full values, ramps and measured contrast: [`design-system/color.md`](../design-system/color.md).

### The one hue we own

**Violet `#7153F0`.** It means *interactive or ours*: buttons, links, focus
rings, the active nav row, the leading chart series, the logo. Nothing else.

A cool-warm hybrid, deliberately carrying more warmth than a generic indigo
such as `#6366F1`.

The discipline that makes it work is what the brand hue is **forbidden** from
doing:

- It is never a status. A run is not violet.
- It is not a background wash. There are no violet gradients, no glows, no
  tinted hero panels.
- It is not used to make something look important. Hierarchy comes from size,
  weight and position first.

Because ~95% of the console is neutral surface, one saturated hue used
sparingly is louder than five used freely.

### Neutrals carry the brand

The greys are not grey. They hold ~286° of the brand hue at very low chroma
(0.001–0.03). It is barely nameable in isolation and unmistakable in aggregate:
it is what stops the console reading as a stock template. Pure neutral grey —
what this product shipped before — looks unfinished beside a saturated accent.

### Status is a closed vocabulary

Five states, five colours, identical everywhere a run appears:

| State | Meaning | Hue |
| --- | --- | --- |
| **Idle** | Nothing scheduled | Neutral |
| **Running** | Working now | Cyan |
| **Needs approval** | Blocked on a human | Amber |
| **Done** | Finished cleanly | Green |
| **Failed** | Finished badly | Red |

Do not add a sixth. Do not reuse a status hue for anything that is not that
status. The instant a green dot can mean two things, the operator has to read
the label — and the colour has stopped doing its job.

Each status ships three weights (`-mark`, `-text`, `-soft`) because one value
cannot both fill a 6px dot and set legible 11px text. Use the named token; do
not derive your own.

### Identity is not status

A desk, a teammate, a skill category, a memory kind — these need a colour to
tell them apart, and it must not be a status colour. They use the **identity
palette** (`--tone-1` … `--tone-5`), which deliberately holds no amber, green
or red.

This is not hypothetical tidiness. Identity used to be drawn from the same
palette as status, so a desk was tinted the exact green that means "done" and
a category the red that means "failed".

Where the two palettes do come close — identity violet against the brand
violet, identity blue against running cyan — **form separates them**:

| | Shape |
| --- | --- |
| **Identity** | A tile with initials |
| **Status** | A pill or dot with a label |

They never take the same shape. When you build something that shows both at
once, that is the rule to check first.

### Never

- Colour as the *only* carrier of meaning. Always pair with an icon, a label,
  or position — roughly 1 in 12 men cannot separate the red/green pair.
- Raw hex in a component. If the system has no token for it, add one.
- A second accent hue. There is one.

---

## 4. Typography

**Geist Variable** for everything the operator reads. Three weights only —
Normal, Medium, Semibold. Bold is not in the system.

**Mono for values that change in place.** Run ids, durations, token counts,
timestamps. The reason is mechanical, not stylistic: a proportional digit
changes width as it ticks, so a live counter makes the row jitter. Prose is
never mono.

The scale runs 10 / 11 / 12 / 14 / 16 / 18 / 20 / 24px. It starts lower than
most products' because this console is genuinely dense — 11px and 10px are
real, load-bearing rungs here, not exceptions.

**Below 10px is not a size, it is a bug.** Fifteen sites currently sit under
it; they are listed for repair in
[`design-system/typography.md`](../design-system/typography.md).

---

## 5. Form

**Radius 10px** at `lg`, deriving the rest of the scale. Rounded enough to feel
like software rather than a spreadsheet; not so round it reads as a toy. The
knowledge graph is the one deliberate exception, at 2–6px, so diagram chrome
stays visually distinct from console chrome.

**Elevation is earned.** A shadow means *this floats above the page* — dialogs,
popovers, dragged cards, command palettes. It is not a decoration for a resting
card; those separate by border and surface lightness alone. Shadows are tinted
with the neutral hue, never pure black, which goes muddy over a tinted surface.

**Borders do most of the separating.** One hairline at `--border`. Two adjacent
borders is one border too many.

**Motion is functional.** Three durations (120 / 180 / 260ms), two curves.
Motion shows where something came from or that the system heard you. It never
merely delights. Nothing loops, nothing bounces, nothing pulses except a live
"running" indicator. `prefers-reduced-motion` is honoured globally.

---

## 6. Logo & marks

The wordmark is **OpenCompany**, set in Geist Semibold, sentence-cased with a
capital C — never `Open Company`, `OPENCOMPANY`, or `opencompany` in prose
(lowercase is correct only as an identifier: the crate, the CLI, `#7153f0`).

**Usage:**

- Clear space on all sides ≥ the cap height of the "O".
- Minimum width 88px; below that use the mark alone.
- On colour, use the white lockup. Never violet-on-colour.
- Never re-set it in another face, stretch it, add a gradient, outline it, or
  apply a shadow.

> **Status:** the repository has no vector logo asset — only a raster in the
> GitBook assets and `src-tauri/icons/icon.png`. An SVG wordmark and mark, plus
> a favicon set, are outstanding. Until they exist, treat the type-set wordmark
> above as the interim lockup.

---

## 7. Applying it

**Before adding a colour, a size, or a shadow, check the token exists.** If it
does not, the question is "should the system name this?" — not "what hex is
close?". One-off values are how design systems die; this codebase already
carries 192 of them, catalogued for repair in the design-system docs.

**The check that catches most mistakes:** open `#/styleguide` in both themes.
If what you built does not look like it belongs on that page, it does not
belong in the product.

---

## See also

- [`design-system/README.md`](../design-system/README.md) — the implementation contract
- [`design-system/color.md`](../design-system/color.md) — every value, with measured contrast
- [`design-system/typography.md`](../design-system/typography.md) — the scale and the migration list
- [`design-system/components.md`](../design-system/components.md) — anatomy and states per primitive
- `frontend/src/index.css` — the tokens themselves
