# Font picker — planning

**Status:** planning only, 2026-09-26. No code. Written after the accent-preset
feature (issue #2493, PR #2494) shipped without it — the operator's original
reference (Slack's Preferences → Appearance) has three controls: Font, Color
Mode, and a theme/accent picker. Color Mode already existed; the accent picker
is issue #2493; this is the font picker, scoped out of #2493 and never
separately planned. This doc is that planning pass, matching the depth of
[`architecture.md`](architecture.md) / [`roadblocks.md`](roadblocks.md) in this
same directory.

## The headline finding

**A real typeface-swap picker — the thing Slack's "Lato (Default)" dropdown
actually is — is not a config toggle here. It requires sourcing, licensing,
and bundling entirely new font families that do not exist in this codebase
today, and it reopens a stated design doctrine the same way the accent
picker reopened the "one hue" doctrine.** It is not infeasible, but it is a
materially bigger and riskier lift than the accent preset work was, for a
structural reason the color case didn't have: a color override is free and
synchronous (a CSS custom property), while a font override is a network
fetch, and this SPA has no way to know the stored preference before the HTML
is already parsing.

Both paths — do it properly, or do a narrower thing instead — are laid out
below with what each actually costs. This is not hedging; it's two real
options because the evidence doesn't point at one cleanly the way "override
only the ten brand primitives" did for accent.

## 1. What's bundled today (verified, not assumed)

Exactly two font families exist in this codebase, both self-hosted via npm,
both variable fonts:

```
frontend/package.json:
  "@fontsource-variable/geist": "^5.2.9"
  "@fontsource-variable/geist-mono": "^5.3.0"
```

```css
/* frontend/src/index.css:594-600 */
--font-sans: "Geist Variable", ui-sans-serif, system-ui, sans-serif;
--font-heading: var(--font-sans);
--font-mono: "Geist Mono Variable", ui-monospace, "SF Mono", "Menlo",
  monospace;
```

`ui-sans-serif, system-ui, sans-serif` is a **fallback stack for while Geist
loads or fails to load**, not a second selectable typeface — nothing in the
codebase lets a user actually choose it. `frontend/index.html:121` sets
`font-family: system-ui, -apple-system, 'Segoe UI', Roboto, sans-serif` only
on the pre-hydration `#boot` placeholder, for the same reason (something has
to render before the real font arrives).

Installed size: `@fontsource-variable/geist` is 224K on disk, `-geist-mono` is
228K — variable fonts covering the full weight axis in one file. A third
family of comparable quality is realistically **another 150–250K**, shipped
to every operator regardless of whether they ever pick it.

## 2. The doctrine this collides with

`docs/brand/README.md` states the typography rule right next to the color
rule the accent picker already overrode:

```
docs/brand/README.md:162   "A second accent hue. There is one."
docs/brand/README.md:168-177
  ## 4. Typography
  **Geist Variable** for everything the operator reads. Three weights only —
  Normal, Medium, Semibold. Bold is not in the system.
```

`docs/design-system/typography.md:1-4` opens the same way — "**Geist
Variable** for everything the operator reads" — and its closing section is
titled **Migration: Complete**, describing 192 sites that were hunted down and
collapsed onto one scale and one face. A user-selectable font doesn't just add
an option next to that; it reopens a inconsistency that migration was written
specifically to close, the same shape of reversal the accent picker made for
"there is one hue" — except the accent case had `open-questions.md` Q2 asked
and answered by the operator before implementation. This one wasn't asked.

This is not a reason to refuse the feature — the operator overrode the color
doctrine deliberately and it shipped fine. It's the reason this needs the same
explicit go-ahead the color case got, rather than being implemented quietly.

## 3. The FOUC/FOIT problem is harder than the accent case, not the same

The accent preset's no-flash mechanism (`frontend/src/main.tsx`, committed
pattern) is:

```
// Synchronous, and before `mount()`, so the chosen accent preset is already on
// `<html>` for React's first commit
applyStoredAccentPreset();
mount();
```

This works for color because setting a CSS custom property is **instant** —
the moment the attribute lands on `<html>`, every `var(--brand-*)` consumer
repaints with the new value, no network involved.

A font is not instant. Swapping `--font-sans` to a second family only looks
right once that family's `.woff2` has actually downloaded and parsed. Doing
the swap synchronously pre-mount (same pattern, trivial to write) makes the
**decision** happen before first paint, but the **visible face** still won't
change until the file arrives — a flash from Geist to the chosen font
(FOUT), or invisible text while it loads if `font-display: block` is used
(FOIT). The accent picker has nothing analogous to this: there is no
"waiting for the color to download."

The standard fix is `<link rel="preload" as="font" href="...">` in `<head>`,
resolved **before** any JS runs. That's exactly the information this SPA
doesn't have at HTML-parse time — the stored preference lives in
`localStorage`, readable only once a script executes, and this app has no
server render to inject a preload hint conditionally (`crash-fallback.tsx`'s
whole reason for existing is that this SPA has nothing between "HTML arrives"
and "React runs"). A universal preload of every bundled font avoids the flash
but pays the download cost for fonts nobody selected — cheap at two families,
real money at four or five.

This is solvable (preload every bundled family unconditionally, accept the
extra requests; or accept one visible reflow on a cold load, same as any
web font does today) — it just doesn't have a clean answer the way the color
case did, and needs picking rather than assuming.

## 4. CSP and desktop constraint (same shape as accent, quick to confirm)

```
crates/opencompany-app/tauri.conf.json:25-26
  "font-src 'self' data:"
```

Same constraint the accent work hit for scripts: no Google Fonts CDN, no
runtime font loading from a remote origin. Any new face must be self-hosted
the same way Geist is — an npm-installed `@fontsource` package (or manually
vendored `.woff2` files) built into the bundle. This rules out the
lowest-effort version of a font picker (just pointing `<link>` at
`fonts.googleapis.com`) outright; it was never on the table here.

## 5. CI: the token gate doesn't cover this, and should before it ships

`scripts/ci/assert-design-tokens.sh` polices font **size** —

```
scripts/ci/assert-design-tokens.sh:8    "Arbitrary font sizes"
scripts/ci/assert-design-tokens.sh:64   arbitrary font sizes
scripts/ci/assert-design-tokens.sh:77   SVG font sizes below the floor
```

— but has no rule about font **family**. Nothing today stops a component from
writing `style={{ fontFamily: "Geist Variable" }}` directly instead of
`var(--font-sans)`, and such a component would silently stop following a
font-picker choice. This isn't a new problem the picker introduces, but a
font picker is the first feature where it would actually produce a visible,
reportable bug (a card that doesn't change font when everything else does).
Worth a grep-based CI rule in the same family as the color gate before this
ships, not a blocker to planning it.

## 6. UI placement — a real ordering decision, not a detail

The shipped Appearance page (`frontend/src/views/settings/AppearanceView.tsx`,
current header comment on `origin/feat/2493-accent-signature-tokens`) pins
Theme first deliberately:

> Theme stays first — the theme e2e spec
> (`test/e2e/theme-toggle-visible.spec.ts`) measures it sitting below the fold
> at a short viewport, and putting Accent above it would change what that spec
> is testing (`roadblocks.md` R15).

Slack's own reference puts **Font first**, above Color Mode. Matching that
order here means either accepting the ordering change and updating the
short-viewport e2e spec's expectations, or deliberately deviating from the
reference and putting Font third (after Theme and Accent) to avoid touching
that test. Both are legitimate; picking silently is what already caused the
scope-gap complaint this doc exists to prevent. **This needs the operator's
call, not an implementer's.**

## 7. Persistence (no open question here)

Follows the existing convention with no new design needed: `accent-presets.ts`
uses `oc.appearance.accentPreset`; a font choice would be
`oc.appearance.fontFamily`, read/applied the same way, no key collision, no
new mechanism.

## 8. Two real paths forward

**Path A — a genuine typeface picker.** Source and bundle 2–4 additional
variable font families (self-hosted, `font-src 'self'`-compliant, licensed for
this use), each ~150–250K, each QA'd across the existing three-weight system
(does a candidate font even *have* clean Normal/Medium/Semibold cuts that read
consistently at the console's unusually low `text-3xs`/`text-2xs` rungs — 10px
and 11px are real, load-bearing sizes here per `typography.md`, and not every
font holds up that small). Solve the preload/FOUC question explicitly (likely:
preload all bundled families, accept the extra request cost). Update
`docs/design-system/typography.md` and `docs/brand/README.md` to describe the
doctrine change the same way `accent-presets.md`/`color.md` describe the hue
one. Real cost: font selection, licensing, and small-size legibility QA — not
engineering mechanism, which is straightforward and mirrors the accent
picker's pre-mount pattern exactly.

**Path B — a narrower control that doesn't reopen the typeface doctrine.**
Expose something real but smaller: a density/comfort toggle (e.g. adjusting
line-height or the active rung of the existing scale) rather than a face swap.
This stays inside "one typeface, one scale," ships with zero new bundled
assets, has no FOUC problem at all (pure CSS, like accent), and doesn't need a
brand-doctrine conversation. It is a materially different feature from what
Slack's Font dropdown does, so it should be named as an alternative, not
sold as the same thing under a different label.

**Recommendation:** if the goal is literally "match Slack's Font dropdown,"
that's Path A, and it's worth doing properly rather than rushed — this is not
a same-PR add-on the way the missing Gray accent preset was. If the goal is
"the console should feel more configurable/premium" more generally, Path B
gets most of that feeling for a fraction of the cost and risk. Left for the
operator to choose; not decided here.

## Open questions (operator input needed before Path A starts)

- **Does the brand doctrine change accept multiple typefaces at all**, the
  same way it accepted multiple accent hues? (Mirrors #2493's Q2.)
- **Which fonts, how many, and licensed how** — this is a real content
  decision, not an engineering one.
- **Font ordering on the Appearance page** — first (matching Slack) or last
  (avoiding the existing e2e viewport assumption)?
- **Preload-all vs. accept-a-flash** for the FOIT/FOUT problem in §3.
- If Path B is chosen instead: what should "premium/configurable" actually
  mean here, concretely, since it would no longer be a font picker.
