// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { WindowControlsInset, WindowDragBar } from "@/components/window-chrome";

/**
 * The desktop window's own chrome, and — mostly — its absence.
 *
 * `tauri.conf.json` runs the main window with `titleBarStyle: "Overlay"`, so
 * macOS stops drawing a title bar and floats the traffic lights over the web
 * content. Two things have to be put back by hand: a band that opts back into
 * dragging, and reserved space so the lights are not sitting on the company
 * switcher.
 *
 * Both are conditional on a runtime this suite is not, which is the whole point
 * of testing them: a band that renders in a browser is a 28px strip across the
 * top of every page that silently swallows clicks, with nothing on screen to
 * explain it. The guard is one `if` and exactly the kind that gets "simplified".
 */

let host: HTMLDivElement;
let root: Root | null = null;

/** Present the runtime as the Tauri desktop, on the given platform. */
function asDesktop(platform: string) {
  (window as unknown as Record<string, unknown>).__TAURI__ = {};
  Object.defineProperty(navigator, "platform", {
    configurable: true,
    value: platform,
  });
}

function render(node: Parameters<Root["render"]>[0]) {
  act(() => root!.render(node));
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
});

afterEach(() => {
  act(() => root?.unmount());
  root = null;
  host.remove();
  delete (window as unknown as Record<string, unknown>).__TAURI__;
});

describe("the window drag band", () => {
  it("renders nothing in a browser", () => {
    // No `__TAURI__`: there is no window to drag, and a band here would only
    // eat the top of every page.
    render(createElement(WindowDragBar));
    expect(host.querySelector("[data-tauri-drag-region]")).toBeNull();
  });

  it("renders nothing on a desktop that keeps its native title bar", () => {
    // `titleBarStyle: "Overlay"` is a macOS-only style — Windows and Linux draw
    // their real title bar, so reserving a band would waste 28px for nothing.
    asDesktop("Win32");
    render(createElement(WindowDragBar));
    expect(host.querySelector("[data-tauri-drag-region]")).toBeNull();
  });

  it("draws a draggable, non-announced band on the macOS desktop", () => {
    asDesktop("MacIntel");
    render(createElement(WindowDragBar));

    const bar = host.querySelector("[data-tauri-drag-region]");
    expect(bar).not.toBeNull();
    // Window chrome, not content: there is nothing here to read.
    expect(bar?.getAttribute("aria-hidden")).toBe("true");
    // Positioned over the top of its container rather than reserving a row, so
    // it adds no inherited inset to the page below it.
    expect(bar?.className).toContain("absolute");
    expect(bar?.className).toContain("top-0");
  });
});

describe("the traffic-light inset", () => {
  it("reserves nothing where the lights do not float", () => {
    render(createElement(WindowControlsInset));
    expect(host.querySelector("[data-tauri-drag-region]")).toBeNull();

    asDesktop("Linux x86_64");
    render(createElement(WindowControlsInset));
    expect(host.querySelector("[data-tauri-drag-region]")).toBeNull();
  });

  it("reserves a draggable strip at the top of the sidebar on macOS", () => {
    asDesktop("MacIntel");
    render(createElement(WindowControlsInset));

    const inset = host.querySelector("[data-tauri-drag-region]") as HTMLElement | null;
    expect(inset).not.toBeNull();
    // In flow, unlike the band: this one's job is to take up the space the
    // lights are drawn in, so the switcher below it starts clear of them.
    expect(inset?.className).not.toContain("absolute");
    expect(inset?.className).toContain("flex-none");
    expect(inset?.style.height).toBe("28px");
  });
});
