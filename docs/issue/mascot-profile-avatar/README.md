# Animated mascot avatar — plan

Planning only. No code in this PR. A tracking issue follows once this is reviewed.

## What this is

An operator supplied a Rive file (`mascotprofile.riv`) exported from Figma's
Rive plugin: an animated character ("the mascot") whose hands and skin can be
recolored and which plays one of four animation states on demand. The ask is
to offer it as an alternate, editable face for a teammate — alongside the
eleven static `tiny:` mascots and an uploaded image — that reacts to hover and
(eventually) to the agent actively replying.

This directory is the deep-dive that preceded any code: what the `.riv` file
actually contains, how it plugs into the console's closed avatar-reference
grammar, which of the ~30 places an avatar renders should get a live canvas
versus keep the existing static tile, where the animation states should be
driven from, and what still needs the real Rive runtime to answer before
implementation starts.

## Documents

- [`rive-parameters.md`](rive-parameters.md) — the `.riv` file's structure
  (artboard, state machine, ViewModel) and the hard 4-state budget.
- [`avatar-grammar.md`](avatar-grammar.md) — the host + console changes to
  the closed `tiny:`/`blob:` avatar-reference grammar, and the test mirrors
  a third form needs.
- [`rendering-strategy.md`](rendering-strategy.md) — the call-site audit
  (which of the ~30 avatar render sites get a live canvas) and the
  bundle/code-splitting plan.
- [`state-mapping.md`](state-mapping.md) — where idle/hover/replying signals
  come from, and what's in scope for v1 versus v1.5+.
- [`open-questions.md`](open-questions.md) — the empirical unknowns that
  need the actual Rive runtime, as an implementation checklist.

## Recommended approach, in one paragraph

Add a third closed avatar-reference form, `mascot:animated`, that behaves
like the existing `tiny:` mascots everywhere except the two hero surfaces
that render it live: the agent profile sheet and the avatar picker. Every
other of the ~30 places a `TeammateAvatar` renders — facepiles, the org
chart, thread rows, mention pickers — keeps drawing the existing tone tile
automatically, because a `mascot:` reference resolves to no static image and
`TeammateAvatar` already falls back to the tile when there's nothing to draw.
Ship one fixed colorway (the file's own defaults) and no live "replying"
reactivity in v1; both are real but separable follow-ups. Load the Rive
runtime and the ~1.8&nbsp;MB `.riv` asset lazily, the same way the console
already isolates `recharts` and `@xyflow/react` from the main bundle.
