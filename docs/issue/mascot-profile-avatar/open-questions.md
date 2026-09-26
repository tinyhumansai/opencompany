# Open questions — now checked against the real Rive runtime

Everything below was originally produced by static analysis: reading the
`.riv` file's embedded string table, a screenshot of its Data panel, and the
console's own source, with nothing actually loaded or played. That gap is
now closed — `MascotAvatar` (`frontend/src/components/mascot-avatar.tsx`)
is implemented, wired into both hero surfaces (the agent profile sheet, the
avatar picker), and was driven live in a browser against
`companies/design_studio`, including the hover transition that was
previously unconfirmed (see §3). What follows is what was actually observed,
not what was assumed.

## 1. Which number is which state, visually — resolved, all nine costumes screenshotted

**Update, 2026-09-26 (the mode/costume/color slice):** the "what do `4`-`9`+
look like" question below was closed by actually cycling
`mascotAnimationNumber` 1–13 against a running instance of the file
(`@rive-app/canvas`, driven from a standalone harness page, no jsdom/mocking)
and screenshotting each landing frame, cross-checked a second time by reading
the state machine's `StateChange` event data for the transition clip name
each number plays. Two things the pre-runtime string-table read got wrong,
now corrected in `crates/opencompany-core/src/company/mascot.rs`
(`MASCOT_COSTUMES`):

- **`face mask` never rendered.** No number 1–13 produced anything resembling
  a face-covering mask. What the string table missed entirely: a tenth,
  unlisted costume — a keffiyeh-style headdress, internally named `habibi` in
  the animation clip names (`rive.animationNames` includes `habibi`,
  `habibi copy`, but no plan or doc before this one named it) — which does
  render, at `mascotAnimationNumber = 6`.
- **Number `4` is not a stable costume.** It transitions via the same `"cap
  dance"` clip name number `1` does, but settles on a visibly different
  resting frame (a shorter, plainer beanie vs. `1`'s low-pulled ribbed one
  with the mascot's eyes showing through) depending on which number the
  state machine was previously on — confirmed by isolating the transition
  (jumping directly from a fresh page load's default straight to `4`, and
  separately via the sequential 1→2→3→4 path) and getting different resting
  frames both times. Not addressable as a costume a picker can promise a
  consistent look for, so `MASCOT_COSTUMES` excludes it.

The confirmed, stable mapping (`mascotAnimationNumber` → costume, `id` in
`MASCOT_COSTUMES`): `1` cap, `2` headphones, `3` headband, `5` glass1 (round
goggles), `6` habibi (keffiyeh), `7` cardboard_mask, `8` glass2 (round
glasses), `9` glass3 (cat-eye sunglasses), `10` glass4 (rectangle
sunglasses) — nine total. `11` was caught mid-gesture (a wave) rather than at
rest even after a long settle wait; `12`/`13` echoed `10`'s own resting
frame rather than introducing a new one; `14` renders the artboard shrunk/off
-model — evidence there is no addressable tenth costume among the higher
numbers either the string table nor the sequential walk found.

Superseded by the above but kept for history: `mascotAnimationNumber = 1`
renders the mascot wearing its cap (idle); `= 2` swaps it to headphones
(hover) — both still true, just no longer the *only* two confirmed numbers.
`replying = 3` (headband) is wired the same way `hover` is, but whether it
reads as a *meaningful* "replying" cue rather than an arbitrary third number
remains unconfirmed and is unchanged by this update — animated mode keeps
`hover`/`replying` as these two fixed numbers regardless of a teammate's
chosen costume (see `mascot-avatar.tsx`'s module docs for why), so this
question is about their own visual, not about what a chosen costume does to
them.

## 2. What the "copy" clips actually do — still unconfirmed, no longer blocked

Now testable (§3 is fixed) but not yet tested: nobody has watched a
transition play in reverse (mascot → idle) to confirm the `copy`-suffixed
clips are the return/exit transitions their naming implies. Low priority —
the forward transition (the part hover actually needs) is confirmed working.

## 3. ViewModel binding path — resolved: wrong state machine, not a nested-artboard problem

The original diagnosis (a nested artboard whose ViewModel binding doesn't
inherit from its parent) was investigated and ruled out, then replaced with
the actual cause once the runtime was introspected directly instead of
inferred from the string table:

- `useRive({ artboard: "Mascot" })` — trying to load the suspected nested
  artboard directly, sidestepping any nesting/inheritance issue entirely —
  throws `"Invalid artboard name or no default artboard"`, the same error
  `MascotProfileAnimations` throws when passed as an `artboard` name. There
  is no separately loadable `Mascot` artboard; `Mascot Instance` is a node
  inside the file's one real artboard (`Artboard`), not a nested artboard
  with its own binding scope.
- Instrumenting the component to log `rive.stateMachineNames` and
  `rive.animationNames` directly (rather than trusting the string table or
  an editor screenshot) showed `Artboard` carries **three** state machines —
  `MascotProfileAnimations`, `animtionStatemachin`, `State Machine 1` — and
  **all 21 animations**, including every `glass1`-`glass4` variant, `cap`,
  `headband`, `headphone`, `face mask`, `cardboard mask`, and their
  dance/jump variants. Nothing about this artboard is nested; everything
  relevant lives at the top level this component already loads.
- The earlier pass loaded `State Machine 1` — the machine the Rive editor's
  Data panel shows `mascotAnimationNumber` bound under — and confirmed the
  ViewModel write round-tripped through its own getter, but the rendered
  artboard never moved (canvas-pixel sampling, byte-identical across
  states, across four configurations, with and without `autoBind: true`).
  Switching the loaded state machine to **`MascotProfileAnimations`**
  instead — same ViewModel writes, unchanged — **fixed it**: hover now
  visibly swaps the mascot's cap for headphones, confirmed by canvas-pixel
  diffing and a screenshot, on both hero surfaces, in both light and dark
  mode. `mascot-avatar.tsx` now loads `MascotProfileAnimations`.
  `State Machine 1` apparently can still bind and read the same ViewModel
  instance (the write never errored or warned) — it just doesn't act on it
  visually. Unconfirmed what `State Machine 1` and `animtionStatemachin` are
  actually for; possibly leftover/unused, possibly gating something outside
  the costume layer. Not investigated further since it isn't blocking.
- **Classic (non-data-bound) state-machine inputs do not exist on this
  file.** `rive.stateMachineInputs(name)` was checked for all three state
  machines and returned an empty array for each, including the one
  currently instanced — `mascotAnimationNumber` exists *only* as a
  ViewModel Data Binding property, not additionally as a classic SMI input
  as `rive-parameters.md` originally guessed from the string table alone.
  `Rive.setNumberStateAtPath()` (the classic path-addressed input setter,
  a plausible-looking way to reach a nested state machine's input by name)
  was tried and correctly failed with "Could not access an input with
  name... at path" — there is no such classic input to find, at any path.
  The ViewModel Data Binding hooks were the right tool the whole time; the
  bug was only ever which state machine was loaded.
- **A footgun found along the way, worth recording so nobody repeats it:**
  calling `rive.stateMachineInputs()` (classic SMI introspection) on a
  `rive` instance that also has the ViewModel Data Binding hooks
  (`useViewModel`/`useViewModelInstance`) active crashed the Rive WASM
  runtime outright (`RuntimeError: null function`, inside
  `ViewModel.defaultInstance`) — the same failure mode as mixing the
  `useStateMachineInput` hook with ViewModel hooks, previously found and
  documented here. It reproduces even from a read-only introspection call,
  not just a write. Diagnosing which state machine has which classic inputs
  therefore has to happen in a build of the component with the ViewModel
  hooks temporarily removed entirely, never alongside them.

**Net effect:** hover works end-to-end and is visible — mouse events → React
state → `MascotState` prop → ViewModel write → the loaded state machine
advancing the artboard. Confirmed on both hero surfaces, both themes, by
canvas-pixel sampling and screenshots, not just a round-tripped getter.

## 4. The Number input's real valid range and behavior — partially confirmed

`1` and `2` are confirmed to render distinct, correct costumes (§1). `3`
("replying") is written the same way but its visual is unconfirmed.
Auto-loop/auto-return behavior between costumes (whether leaving `hover`
plays a `copy` reverse clip back to idle, or jump-cuts) was not directly
observed — see §2.

## 5. Actual bundle cost — partially measured

The shipped `.riv` file itself is confirmed at 1,763,803 bytes
(`frontend/public/avatars/mascot-animated.riv`, ~1.68&nbsp;MiB uncompressed;
matches the plan's ~1.8&nbsp;MB estimate closely). The gzipped JS chunk cost
of `@rive-app/react-canvas` plus its WASM runtime, once actually
code-split via the `lazy()` boundary this component is meant to be the
seam for, was not measured in this pass — still open.
