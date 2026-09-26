// @vitest-environment jsdom
//
// `MascotAvatar`'s own contract, mocked at the `@rive-app/react-canvas`
// boundary rather than rendered for real: jsdom has no WebGL/canvas support
// (`Not implemented: HTMLCanvasElement's getContext()`), so the actual Rive
// artboard — whether hover visibly swaps the mascot's cap for headphones —
// can only be watched in a real browser (see
// `docs/issue/mascot-profile-avatar/open-questions.md` §3 for how that was
// verified). What a unit test *can* pin, and this one does: which
// `mascotAnimationNumber` value each `mode`/`state`/`costume` combination
// writes, that `prefers-reduced-motion` and `mode="static"` both hold the
// costume baseline regardless of `state`, and that the colors are set once
// the ViewModel instance is bound.

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const rive = vi.hoisted(() => ({
  setNumber: vi.fn(),
  setHandRgb: vi.fn(),
  setSkinRgb: vi.fn(),
}));

vi.mock("@rive-app/react-canvas", () => ({
  useRive: () => ({ rive: {}, RiveComponent: () => createElement("canvas") }),
  useViewModel: () => ({}),
  useViewModelInstance: () => ({}),
  useViewModelInstanceNumber: () => ({ value: 1, setValue: rive.setNumber }),
  useViewModelInstanceColor: (name: string) => ({
    setRgb: name === "handColor" ? rive.setHandRgb : rive.setSkinRgb,
  }),
}));

const { MascotAvatar } = await import("@/components/mascot-avatar");

let container: HTMLDivElement;
let root: Root;
let reduced = false;

/**
 * Same stub as `team-add-agent-dialog.test.ts` / `cov-chat-members-add-remove.test.ts`:
 * jsdom ships no `matchMedia`, and `MascotAvatar`'s own
 * `usePrefersReducedMotion` reaches for it unguarded. `reduced` is mutable
 * so a single test can flip it between renders.
 */
function stubMatchMedia() {
  Object.defineProperty(window, "matchMedia", {
    configurable: true,
    writable: true,
    value: (query: string) => ({
      get matches() {
        return reduced;
      },
      media: query,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
      onchange: null,
    }),
  });
}

beforeEach(() => {
  reduced = false;
  stubMatchMedia();
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  vi.clearAllMocks();
});

afterEach(async () => {
  await act(async () => {
    root.unmount();
  });
  container.remove();
});

// eslint-disable-next-line @typescript-eslint/no-explicit-any
function render(props: Record<string, any>) {
  act(() => {
    root.render(createElement(MascotAvatar, props));
  });
}

describe("MascotAvatar", () => {
  it("writes 1 (cap) for idle, 2 for hover, 3 for replying — the file's own default costume", () => {
    render({ state: "idle" });
    expect(rive.setNumber).toHaveBeenLastCalledWith(1);
    render({ state: "hover" });
    expect(rive.setNumber).toHaveBeenLastCalledWith(2);
    render({ state: "replying" });
    expect(rive.setNumber).toHaveBeenLastCalledWith(3);
  });

  it("defaults to animated mode, idle state, cap costume when no props are given", () => {
    render({});
    expect(rive.setNumber).toHaveBeenLastCalledWith(1);
  });

  it("holds the costume baseline under prefers-reduced-motion, regardless of the requested state", () => {
    reduced = true;
    render({ state: "hover" });
    expect(rive.setNumber).toHaveBeenLastCalledWith(1);
  });

  it("sets the mascot's default skin/hand colors once bound", () => {
    render({ state: "idle" });
    expect(rive.setHandRgb).toHaveBeenCalledWith(0xb4, 0x90, 0x0b);
    expect(rive.setSkinRgb).toHaveBeenCalledWith(0xf7, 0xd1, 0x45);
  });

  it("renders hidden from assistive tech — the mascot is decorative, the teammate's name carries the meaning", () => {
    render({ state: "idle" });
    const wrapper = container.querySelector("[aria-hidden]");
    expect(wrapper).not.toBeNull();
  });

  it("a chosen costume becomes the idle baseline, by its mascotAnimationNumber", () => {
    render({ state: "idle", costume: "glass2" });
    expect(rive.setNumber).toHaveBeenLastCalledWith(8);
  });

  it("an unrecognised costume falls back to the default (cap, 1)", () => {
    render({ state: "idle", costume: "not-a-real-costume" });
    expect(rive.setNumber).toHaveBeenLastCalledWith(1);
  });

  it("in animated mode, hover/replying stay the fixed reactive numbers even with a costume chosen — a chosen costume is the idle baseline, not a hover override", () => {
    render({ state: "idle", costume: "glass2" });
    expect(rive.setNumber).toHaveBeenLastCalledWith(8);
    render({ state: "hover", costume: "glass2" });
    expect(rive.setNumber).toHaveBeenLastCalledWith(2);
    render({ state: "idle", costume: "glass2" });
    expect(rive.setNumber).toHaveBeenLastCalledWith(8);
  });

  it("static mode ignores state entirely and holds the costume baseline", () => {
    render({ mode: "static", state: "idle", costume: "glass2" });
    expect(rive.setNumber).toHaveBeenLastCalledWith(8);
    render({ mode: "static", state: "hover", costume: "glass2" });
    expect(rive.setNumber).toHaveBeenLastCalledWith(8);
    render({ mode: "static", state: "replying", costume: "glass2" });
    expect(rive.setNumber).toHaveBeenLastCalledWith(8);
  });

  it("writes a chosen skin/hand color pair", () => {
    render({ state: "idle", skinColor: "mint", handColor: "teal" });
    expect(rive.setSkinRgb).toHaveBeenLastCalledWith(0xa8, 0xe6, 0xc1);
    expect(rive.setHandRgb).toHaveBeenLastCalledWith(0x2f, 0x8f, 0x86);
  });

  it("falls back to the default colors for an unrecognised id", () => {
    render({ state: "idle", skinColor: "nonexistent", handColor: "nonexistent" });
    expect(rive.setHandRgb).toHaveBeenLastCalledWith(0xb4, 0x90, 0x0b);
    expect(rive.setSkinRgb).toHaveBeenLastCalledWith(0xf7, 0xd1, 0x45);
  });
});
