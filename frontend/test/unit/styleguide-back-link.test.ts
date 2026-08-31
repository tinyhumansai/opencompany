// @vitest-environment jsdom
//
// "Back to console" has to return the operator to the console they left, and on
// a desktop holding several hosts that is a scope, not just a page (issue
// #1358). `App` remounts `Console` on the way back, so a bare `#/overview`
// would leave `useHostRoute` to initialize from an absent parameter and land on
// whichever host the bootstrap fallback picks — a silent host switch.

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { StyleguideView } from "@/views/StyleguideView";

(globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT =
  true;

let container: HTMLDivElement;
let root: Root;

/**
 * The styleguide's component section renders a `SidebarProvider` preview,
 * which reaches for `window.matchMedia` unguarded in `useIsMobile`. jsdom
 * ships no `matchMedia`, so without a stub the whole view fails to mount —
 * same stub `sidebar-collapse-button.test.ts` and `working-indicator.test.ts`
 * install. The back-link assertions do not care about the mobile flag; the
 * stub reports "not matching" (the desktop case, where the sidebar is the
 * inline column rather than a sheet).
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
  window.history.replaceState(null, "", "#/");
});

/** Renders the styleguide and returns its header link's destination. */
function backHref(): string | null {
  act(() => root.render(createElement(StyleguideView)));
  const link = container.querySelector<HTMLAnchorElement>(
    '[data-testid="styleguide-header"] a',
  );
  expect(link).not.toBeNull();
  return link!.getAttribute("href");
}

/**
 * The styleguide header's title row — the flex row `PageHeader` puts the
 * heading, the theme toggle and the back link on (issue #1763).
 *
 * Found by walking from the `h1` rather than by position, so the assertions
 * below stay about *this row* if the component ever gains another wrapper.
 */
function headerRow(): HTMLElement {
  act(() => root.render(createElement(StyleguideView)));
  const heading = container.querySelector<HTMLElement>(
    '[data-testid="styleguide-header"] h1',
  );
  expect(heading).not.toBeNull();
  return heading!.parentElement!;
}

describe("the styleguide back link", () => {
  it("carries the host scope the styleguide was opened with", () => {
    window.history.replaceState(null, "", "#/styleguide?host=c-2");
    expect(backHref()).toBe("#/overview?host=c-2");
  });

  it("names no host when the address names none", () => {
    window.history.replaceState(null, "", "#/styleguide");
    expect(backHref()).toBe("#/overview");
  });
});

/**
 * jsdom lays nothing out, so this pins the properties that decide the outcome
 * rather than the geometry: the title block sets its min-content width from an
 * unbreakable path (`docs/design-system/`) and the controls do not shrink, so a
 * non-wrapping row pushes "Back to console" past the right edge of a 320px
 * viewport instead of stacking it under the heading.
 */
describe("the styleguide header on a narrow viewport", () => {
  it("wraps its controls instead of overflowing", () => {
    const row = headerRow();
    expect(row.className).toContain("flex-wrap");
    expect(row.className).not.toContain("flex-nowrap");
  });

  it("lets the title shrink below its min-content width", () => {
    const heading = headerRow().querySelector<HTMLElement>("h1");
    expect(heading?.className).toContain("min-w-0");
    // The row itself has to allow it too: a flex item defaults to
    // `min-width: auto`, so `min-w-0` on the heading alone is overridden by the
    // row's own floor.
    expect(headerRow().className).toContain("min-w-0");
  });
});
