# Open questions

Genuinely undecided items that need an operator, brand, or design call. Each
has a recommendation so it can be answered with a yes or no. **Q2 and Q3
block Phase 1**; the rest can be answered in flight.

---

## Q1. The default preset's colour — deferred by design

Explicitly out of scope for this work, pending brand and colour-psychology
input. The architecture keeps it independent: the default is whatever `:root`
declares (`architecture.md` §2), so the eventual decision is an edit to
`frontend/src/index.css:55-64` plus the contrast table, and changes nothing in
the picker, the registry or the storage.

One sub-question does touch this plan: **if the default stops being violet, is
the current violet kept as a named preset** (so existing operators can choose to
keep what they have)? Recommendation: yes, and name it now so the id is stable.

## Q2. Does the brand accept a personal interaction hue? (blocks Phase 1)

`index.css:47-50` and `docs/brand/README.md:92` define violet as "the one hue
the product owns … interactive or ours". Presets make it a per-operator choice.
Slack's precedent says this is normal for a workspace tool; the brand doc says
the hue *is* the product.

Recommendation: accept it, and reword the doctrine from "violet is the hue" to
"the brand ramp is the interaction hue; violet is its default". The logo tile is
neutral `#191919`, so the mark is unaffected. Answering this also decides the
copy in `StyleguideView.tsx:351`, `:442`, `:545` and `UsageView.tsx:63`.

## Q3. Pin chart slot 1 and the graph's AI-agent colour? (blocks Phase 1)

Both follow the brand today and would collide with status and chart hues under
most Slack-style presets (`roadblocks.md` R10).

Recommendation: **pin** both to a fixed primitive (`architecture.md` §4). The
alternative — letting them follow and forbidding preset hues near cyan, green,
amber, red and pink — leaves roughly violet, indigo and blue, which is not a
picker worth building.

Follow-up for the Q1 decision: when the default changes, do the pinned
`--signature-*` values change with it, or stay violet forever as the "data"
colour? No need to answer now.

## Q4. Should presets tint the neutrals?

Surfaces and ink carry ~286° at low chroma; the hover tint `--accent`
(`#ECE9FC`) and the dark active rung are the visible cases (`roadblocks.md`
R8). Slack themes do recolour the sidebar frame.

Recommendation for v1: **no**. Overriding `--surface-dark-active` reaches dark
inputs, borders and secondary buttons (`index.css:469`, `:496`, `:504`), and
each preset would need its own measured neutral ladder. Revisit if a green or
orange preset looks wrong in review — the likely minimal step is a per-preset
override of only the semantic `--accent` (hover tint) in a mode-qualified pair.

## Q5. Which presets, how many, and what names?

Recommendation: 6–8, spread around the hue circle, each tuned to pass the
contrast gate. Slack uses evocative names (Aubergine, Clementine, Banana,
Jade, Lagoon, Barbra, Gray, Mood Indigo); copying them outright is not
advisable. Candidates by hue family: violet (the current brand), indigo, blue,
teal, green, amber/orange, rose, graphite. Yellow is the hardest to make pass
(`roadblocks.md` R11) and could be left out of v1.

Needs: a designer or the operator to choose hues and names; the contrast test
to accept them.

## Q6. Is the contrast gate automated in v1?

Recommendation: **yes** (`test-plan.md` U4). It is a node-environment vitest
with ~25 lines of colour math and no new dependency; the existing
`shell-chrome-tokens.test.ts` already parses `index.css` the same way. What it
cannot judge is whether a preset *looks* good — that remains review with
screenshots in both modes.

Decide alongside it: whether to also file the pre-existing 4.20 / 4.22 gap for
`brand-500` on `--accent` and `--chrome` (`roadblocks.md` R12), which the gate
deliberately does not assert.

## Q7. Per browser, or per person?

v1 is per origin via `localStorage` — per desktop install, per hosted tenant in
a browser. That matches the Appearance page's own framing
(`AppearanceView.tsx:4-9`). A person on two laptops sets it twice.

The server seam (`PATCH …/auth/me`) is per **company**, not per person across
companies, and has the constraints in `roadblocks.md` R9. Recommendation: stay
per browser; revisit only if operators ask for sync, and then decide whether
the unit is "person in this company" (fits the existing seam) or "person
everywhere" (needs a seam that does not exist — the host has no cross-company
identity).

## Q8. Fix the existing dark-mode flash in the same change?

`next-themes` applies `.dark` from an effect in this SPA, so an operator whose
explicit choice differs from the OS sees one wrong-mode frame
(`roadblocks.md` R1). The Phase 2 pre-mount function could apply `.dark` too,
for almost nothing.

Recommendation: yes, but as its own small PR immediately after Phase 2, so the
accent change and a behaviour change to dark mode are reviewed separately.

## Q9. Does the preset reach pre-company surfaces?

Because it is applied on `<html>` from `localStorage`, it will also colour
Login and the company picker, where `ThemeToggle` also appears
(`theme-toggle-visible.spec.ts:31-34` notes those call sites). The picker
itself lives only on Appearance. Recommendation: accept — consistent with how
the mode already behaves.

## Q10. Future scope to schedule, not decide now

- A free-form custom accent (hex input) — needs a runtime ramp generator and a
  runtime contrast check.
- Vision-assistive presets that move status colours.
- Propagating mode and preset into agent-authored pages over the pages
  `postMessage` bridge; those pages get neither today.
- Tightening `scripts/ci/assert-design-tokens.sh` to catch `oklch(` in
  TypeScript (`roadblocks.md` R7).
- The adjacent `openpanel-init.js` immutable-caching finding
  (`roadblocks.md` R3).
