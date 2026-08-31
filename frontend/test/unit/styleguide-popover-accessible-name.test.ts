// @vitest-environment jsdom
//
// Codex review on #1821: the styleguide's own Popover example is the
// canonical pattern operators and future contributors copy. If the example
// itself omits `PopoverTitle`, Base UI still opens its popup as
// `role="dialog"` but never wires an `aria-labelledby` (`Popover.Popup` only
// reads the id back from `Popover.Title`'s generated one) — so the
// "recommended" pattern would demonstrate an unnamed dialog to a
// screen-reader user, and anyone copying it reproduces exactly the gap the
// run-status legend popover was just fixed for
// (`workflow-run-status-legend.test.ts`).

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { StyleguideView } from "@/views/StyleguideView";

(globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT =
  true;

let container: HTMLDivElement;
let root: Root;

/**
 * See `styleguide-back-link.test.ts` — the styleguide's component-section
 * preview reaches for `window.matchMedia` unguarded via `useIsMobile`, and
 * jsdom ships none.
 */
function stubMatchMedia() {
  Object.defineProperty(window, "matchMedia", {
    configurable: true,
    writable: true,
    value: (query: string) => ({
      matches: false,
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
  stubMatchMedia();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("the styleguide's Popover example names itself for assistive technology", () => {
  it("labels the dialog via aria-labelledby pointing at the heading", async () => {
    act(() => root.render(createElement(StyleguideView)));

    const trigger = Array.from(
      container.querySelectorAll<HTMLButtonElement>("button"),
    ).find((button) => button.textContent?.includes("Click or hover me"));
    expect(trigger).not.toBeUndefined();

    await act(async () => {
      trigger!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });

    const dialog = document.body.querySelector('[role="dialog"]');
    expect(dialog).not.toBeNull();

    const labelledBy = dialog?.getAttribute("aria-labelledby");
    expect(labelledBy).toBeTruthy();

    const heading = labelledBy ? document.getElementById(labelledBy) : null;
    expect(heading).not.toBeNull();
    expect(heading?.textContent).toBe("Popover");
  });
});
