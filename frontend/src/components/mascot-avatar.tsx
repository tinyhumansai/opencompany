// The animated Rive mascot: an alternate teammate face, live at exactly the
// hero surfaces that mount it (the agent profile sheet, the agent detail
// page's header, the avatar picker). See `docs/issue/mascot-profile-avatar/`
// for the deep-dive this was planned from.
//
// Deliberately its own component rather than a `TeammateAvatar` variant: a
// `mascot:` reference resolves to no static image (`staticAvatarSrc` in
// `lib/avatar.ts`), so every other surface keeps drawing the tone tile with
// zero changes, and only a caller that explicitly wants the live canvas reaches
// for this.

import { useEffect, useState } from "react";
import {
  useRive,
  useViewModel,
  useViewModelInstance,
  useViewModelInstanceColor,
  useViewModelInstanceNumber,
} from "@rive-app/react-canvas";

import {
  hexToRgb,
  mascotCostumeNumber,
  mascotHandColorHex,
  mascotSkinColorHex,
  mascotSrc,
  type MascotCostume,
  type MascotHandColor,
  type MascotMode,
  type MascotSkinColor,
} from "@/lib/avatar";
import { cn } from "@/lib/utils";

export type MascotState = "idle" | "hover" | "replying";

/**
 * The `mascotAnimationNumber` for `hover`/`replying`, kept exactly as the
 * mechanism already built it (confirmed live: `2` swaps the mascot to
 * headphones) rather than reinvented for a chosen costume. A teammate's
 * `idle` baseline is its chosen costume ({@link mascotCostumeNumber}); the
 * reactive states stay these two fixed numbers regardless of that choice —
 * see the module docs on {@link MascotAvatar} for why.
 */
const REACTIVE_NUMBERS: Record<"hover" | "replying", number> = {
  hover: 2,
  replying: 3,
};

interface Props {
  /**
   * Whether the canvas plays at all. Defaults to `"animated"`, the file's own
   * default and what every `mascot:animated` wearer already rendered.
   * `"static"` freezes on the chosen costume's resting frame: no autoplay, and
   * {@link Props.state} is ignored entirely — a static mascot does not react
   * to hover or "replying", so callers should not wire those handlers up for
   * it either (this only guards the canvas itself).
   */
  mode?: MascotMode | string;
  /**
   * Which of the file's states to play. Defaults to idle. Ignored in
   * `"static"` mode. In `"animated"` mode, `hover`/`replying` are the fixed
   * {@link REACTIVE_NUMBERS} already built — they do not swap to a different
   * costume than the one chosen; only `idle` lands on it.
   */
  state?: MascotState;
  /**
   * The chosen costume id (`MASCOT_COSTUMES` in `lib/avatar.ts`), or
   * `undefined` for the file's own default (`"cap"`). Applies in both display
   * modes: the resting frame in `"static"`, the `idle` baseline in
   * `"animated"`. An unrecognised id falls back to the default rather than
   * crashing the Rive ViewModel write.
   */
  costume?: MascotCostume | string;
  /** The chosen skin (body) color id, or `undefined` for the file's own default. */
  skinColor?: MascotSkinColor | string;
  /** The chosen hand/accent color id, or `undefined` for the file's own default. */
  handColor?: MascotHandColor | string;
  className?: string;
  "data-testid"?: string;
  /**
   * Fires once the canvas has actually been given its chosen costume and
   * colors — the same `ready` gate that controls this component's own
   * opacity (see the module docs above it). `MascotWarmer`
   * (`teammate-avatar.tsx`) is the caller: it is the signal that the canvas
   * now holds a real, correctly-costumed frame worth capturing with
   * `canvas.toDataURL()`, rather than the file's un-costumed default one.
   */
  onReady?: () => void;
}

/**
 * `prefers-reduced-motion: reduce` holds the mascot on its idle frame.
 *
 * The same accessibility carve-out an animated GIF avatar already has to
 * consider (`docs/spec/runtime/avatars.md`) — "a moving face is more
 * recognisable" assumes the viewer can tolerate motion, which reduced-motion
 * says they can't. Duplicated locally rather than shared: three other
 * components in this codebase (`WorkingIndicator`, `ChatLiveReceipt`,
 * `RevealSelectedNode`) already each keep their own copy of this exact hook
 * rather than a shared one, so this follows the established convention.
 */
function usePrefersReducedMotion(): boolean {
  const [reduced, setReduced] = useState(false);
  useEffect(() => {
    const mql = window.matchMedia?.("(prefers-reduced-motion: reduce)");
    if (!mql) return;
    setReduced(mql.matches);
    const onChange = () => setReduced(mql.matches);
    if (typeof mql.addEventListener === "function") {
      mql.addEventListener("change", onChange);
      return () => mql.removeEventListener("change", onChange);
    }
    mql.addListener(onChange);
    return () => mql.removeListener(onChange);
  }, []);
  return reduced;
}

/**
 * The live animated mascot. Mount this instead of `TeammateAvatar` only at
 * the small number of hero surfaces that want it live — see
 * `docs/issue/mascot-profile-avatar/rendering-strategy.md` for which those
 * are and why every other avatar surface must not mount this.
 *
 * Callers should `lazy()`-load this module (mirroring the
 * `lazy(() => import(...).then((m) => ({ default: m.X })))` convention this
 * codebase already uses for `recharts`/`@xyflow/react`/`react-joyride`) rather
 * than importing `@rive-app/react-canvas` directly — this file is the
 * code-split boundary.
 */
export function MascotAvatar({
  mode = "animated",
  state = "idle",
  costume,
  skinColor,
  handColor,
  className,
  "data-testid": testId,
  onReady,
}: Props) {
  const reducedMotion = usePrefersReducedMotion();
  const isStatic = mode === "static";
  const { rive, RiveComponent } = useRive({
    src: mascotSrc("animated"),
    // The file has one *loadable* artboard, literally named "Artboard" —
    // `useRive({ artboard: "Mascot" })` throws "Invalid artboard name or no
    // default artboard", so `Mascot Instance` (seen in the object graph) is
    // a node inside `Artboard`, not a separately loadable artboard; there is
    // no nested artboard to route around. This omits `artboard` and lets the
    // runtime use its default.
    //
    // `Artboard` carries three state machines (`rive.stateMachineNames`):
    // `MascotProfileAnimations`, `animtionStatemachin`, and `State Machine
    // 1`. The Rive editor's Data panel shows `mascotAnimationNumber` bound
    // under `State Machine 1`, which is what an earlier pass loaded here —
    // the ViewModel write round-tripped through its own getter, but the
    // rendered artboard never moved (canvas-pixel sampling, byte-identical
    // across states). `MascotProfileAnimations` is the one that actually
    // drives the costume swap: loading *this* one instead, with the same
    // ViewModel writes below unchanged, visibly swaps the mascot's cap for
    // headphones on hover (confirmed both by pixel sampling and a
    // screenshot). Both machines can apparently read the same bound
    // ViewModel instance; only one of them acts on it. See
    // `docs/issue/mascot-profile-avatar/open-questions.md` §3.
    //
    // `autoBind: true` lets the runtime perform its own default
    // ViewModel-instance binding at load time, ahead of this component's own
    // manual `useViewModel`/`useViewModelInstance` calls below.
    stateMachine: "MascotProfileAnimations",
    autoBind: true,
    // `autoplay: false` does not mean "paint one frame and stop" — it means
    // the Rive runtime never starts its render loop at all, so the canvas
    // never paints *anything*, including the ViewModel-driven costume/color
    // writes below (confirmed live: a static mascot rendered fully
    // transparent, not a frozen cap). A static mascot's costume never
    // transitions (the number this component writes for it never changes),
    // so keeping the loop running costs nothing extra to look at — "static"
    // is enforced by never changing `state`/the animation number and never
    // wiring hover up (see this component's own module docs), not by
    // stopping the runtime.
    autoplay: !reducedMotion,
  });

  const viewModel = useViewModel(rive, { useDefault: true });
  const vmi = useViewModelInstance(viewModel, { useDefault: true, rive });

  const { setValue: setAnimationNumber } = useViewModelInstanceNumber(
    "mascotAnimationNumber",
    vmi,
  );
  const { setRgb: setHandColor } = useViewModelInstanceColor("handColor", vmi);
  const { setRgb: setSkinColor } = useViewModelInstanceColor("skinColor", vmi);

  // Whether the canvas has actually been given its chosen costume and colors
  // at least once. `vmi` becoming truthy only means the file loaded and the
  // ViewModel bound — the canvas itself keeps painting the file's own default
  // (uncostumed) frame until the effects below write to it, which is a tick
  // later. Gating visibility on this instead of on `vmi`/`rive` directly is
  // what prevents a mount from ever showing the wrong costume, even for one
  // frame, and — combined with staying transparent until then — is what lets
  // a caller layer this over its own placeholder (`AvatarTile`'s tone tile,
  // or a caller's own `Skeleton`) without that placeholder being replaced by
  // a flash of the un-costumed default first.
  const [ready, setReady] = useState(false);

  // The chosen colors, or the file's own defaults when unset or
  // unrecognised. Set whenever either changes — not just once — so a picker
  // preview updates live as an operator tries different swatches before
  // saving. Applies identically in both display modes.
  useEffect(() => {
    if (!vmi) return;
    setHandColor(...hexToRgb(mascotHandColorHex(handColor)));
    setSkinColor(...hexToRgb(mascotSkinColorHex(skinColor)));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [vmi, handColor, skinColor]);

  // The costume baseline, and — in animated mode only — the hover/replying
  // swap already built. A static mascot ignores `state` entirely: it freezes
  // on the chosen costume and never re-fires this effect for a state change,
  // because reduced-motion and static both resolve to the same baseline
  // number regardless of what `state` says.
  useEffect(() => {
    if (!vmi) return;
    const baseline = mascotCostumeNumber(costume);
    const number =
      isStatic || reducedMotion || state === "idle"
        ? baseline
        : REACTIVE_NUMBERS[state];
    setAnimationNumber(number);
    setReady(true);
    if (!onReady) return;
    // The ViewModel write above is not the paint: Rive applies it on its own
    // render loop's next tick, and `canvas.toDataURL()` (what `onReady` exists
    // for — see this component's own `Props.onReady` doc) needs a frame that
    // has actually been drawn with it, not merely requested. One
    // `requestAnimationFrame` is when the browser is about to paint; a second
    // is the first opportunity to run *after* that paint has happened.
    let raf2 = 0;
    const raf1 = requestAnimationFrame(() => {
      raf2 = requestAnimationFrame(() => onReady());
    });
    return () => {
      cancelAnimationFrame(raf1);
      cancelAnimationFrame(raf2);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [vmi, isStatic, state, reducedMotion, costume]);

  return (
    <div
      className={cn("overflow-hidden rounded-xl", className)}
      data-testid={testId}
      aria-hidden
    >
      {/* The `.riv` file is ~1.7 MB and can take a couple of seconds to fetch
          and instance even after this component's own chunk has loaded, and
          an untouched canvas is fully transparent — painting nothing at all
          is what read as "broken" (issue found live 2026-09-26). Opacity
          rather than an unmount keeps `RiveComponent` mounted (and its
          `autoplay` loop warm) throughout, so there is no second mount cost
          once the file lands; only its visibility changes. Fully transparent
          until `ready` means whatever a caller has placed behind this
          (`AvatarTile`'s initials tile, or a caller's own `Skeleton`) is what
          shows during the gap, instead of this component inventing its own
          placeholder that would paint over — and hide — that one. */}
      <RiveComponent
        className={cn(
          // `RiveComponent` renders its own wrapper around the actual
          // `<canvas>` rather than spreading straight onto it (confirmed by
          // inspecting the mounted DOM: `className` here lands on that
          // wrapper, not the canvas). That wrapper has no size of its own —
          // it is sized by its content — and the canvas in turn sizes itself
          // to *its* parent (`shouldResizeCanvasToContainer`'s default),
          // which is this very wrapper. With neither given an explicit size,
          // that is a circular "auto" on both ends and the canvas resolves
          // to a real width but a **zero height** (confirmed live: `<canvas
          // width="56" height="0">`) — painting nothing, silently, no error
          // anywhere. `size-full` breaks the circle: it is a definite size
          // (100% of the outer `div` above, which this component's own
          // `className` prop already sizes, one way or another, at every
          // call site). Found live 2026-09-26 against the agent detail
          // page's hero avatar, after the mascot had rendered correctly
          // everywhere it was tested up to this point.
          "size-full",
          "transition-opacity duration-150",
          ready ? "opacity-100" : "opacity-0",
        )}
      />
    </div>
  );
}
