# The `.riv` file's structure

Two files were supplied: `mascotprofile.riv` and `mascotprofile.rev`.

**Only `.riv` is real.** It opens with the `RIVE` magic header Rive's runtime
requires. `.rev` has no such header and shares the same embedded string
table (artboard/state-machine/ViewModel names) — it reads as a Rive editor
scratch or autosave artifact, not something any runtime loads. It is not
part of this plan and should not be shipped.

Everything below was read from the binary's embedded string table (the
format stores object names as UTF-8) and cross-checked against a screenshot
of the file open in the Rive editor's Data panel. No runtime had actually
played the file yet at the time this was written.

**Runtime introspection later corrected the object graph below on two
points** — see `open-questions.md` §3 for the full account:

- The file's one real, top-level-loadable artboard is literally named
  `Artboard`, not `MascotProfileAnimations`. `MascotProfileAnimations` is
  one of *three* state machines declared directly on `Artboard`
  (`rive.stateMachineNames`), not the artboard itself. There is no separate
  `Mascot`/`Mascot Instance` nested artboard with its own binding scope —
  `Mascot Instance` is a node inside `Artboard`, and everything below
  (all three state machines, all animations) lives at that one top level.
- The Number input selects between **nine** costume animations, not four:
  `cap`, `headband`, `headphone`, `face mask`, `cardboard mask`, and four
  numbered `glass` variants (`rive.animationNames`), each with its own
  `copy` (reverse/exit) clip. The "hard budget: 4 states" section below is
  wrong — `glass1`-`glass4` is a subset of the costume set, not the whole
  of it.

## Object graph (as originally read from the string table — see correction above)

```
Artboard: MascotProfileAnimations
  └─ Mascot Instance   (nested artboard "Mascot")
       └─ State Machine 1
            Number input: mascotAnimationNumber (default 1)
            Animations: glass1 animation / glass1 animation copy
                        glass2 animation / glass2 animation copy
                        glass3 animation / glass3 animation copy
                        glass4 animation / glass4 animation copy
```

The `copy` suffix on each animation is almost certainly the reverse/exit
clip back toward idle — a standard Rive authoring pattern for a state
machine where a Number input selects one of several named states, each with
an entry transition and a return transition — but this is inferred from
naming, not observed. See open questions.

## The ViewModel (Data Binding)

Confirmed directly from the Rive editor (not just the string table): a
ViewModel named `Mascot` with three bindable properties.

| Property | Type | Default |
|---|---|---|
| `handColor` | Color | `#B4900B` |
| `skinColor` | Color | `#F7D145` |
| `mascotAnimationNumber` (shown truncated as `mascotA…` in the editor) | Number | `1` |

This is Rive's newer Data Binding system, separate from — and in addition
to — the plain State Machine input of the same name found in the string
table. In the React runtime (`@rive-app/react-canvas`) these are set via a
`ViewModelInstance`'s `.color("handColor")` / `.number("mascotA…")` setters,
not the older `useStateMachineInput` hook, which only reaches inputs
declared directly on the state machine rather than a bound ViewModel.

## The hard budget: 4 states (superseded — see the correction above)

This section originally claimed the Number input selects between exactly
four states (`glass1`–`glass4`). Runtime introspection found nine costume
animations total (`open-questions.md` §1), so the real budget is larger than
this section assumed — `glass1`-`glass4` is only the `glass` subset. Kept for
history; do not use this section's "4" for capacity planning.

## Where the asset lives once this ships

Avatars are served as static files under `frontend/public/avatars/` — the
eleven `tiny:` mascots are `blob-<flavour>.webp` there today, and nothing
about them is embedded into the Rust binary. `mascotprofile.riv` should land
the same way, e.g. `frontend/public/avatars/mascot.riv`, loaded client-side
by the Rive runtime. No changes to the embedding/build pipeline
(`docs/spec/runtime/globals.md`'s "embedded at build time" concept is about
`companies/_globals/`, unrelated to avatar assets) are needed for this.
