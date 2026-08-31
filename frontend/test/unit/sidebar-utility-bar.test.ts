// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { SidebarUtilityBar } from "@/components/sidebar-controls";
import { SidebarProvider } from "@/components/ui/sidebar";

/**
 * The sidebar's utility bar: Settings, Feedback, Discord and Collapse, as one
 * row of icons under the company switcher.
 *
 * All four used to be full-width rows — Settings in the nav list, Feedback and
 * Discord in the footer, Collapse loose in the header — costing four rows of a
 * column whose other rows are the places an operator actually works. They are
 * utilities: three go somewhere, none is somewhere you stay. OpenHuman's shell
 * already groups the same four this way
 * (`app/src/components/layout/shell/SidebarHeader.tsx`).
 *
 * Rendering rather than pure functions, for the reason
 * `sidebar-collapse-button.test.ts` gives: every one of these is icon-only in
 * BOTH sidebar states, so the accessible NAME is the whole of what a screen
 * reader gets. An `aria-label` is exactly the kind of attribute a styling pass
 * drops without breaking a single render.
 */

let host: HTMLDivElement;
let root: Root | null = null;

/** jsdom ships no `matchMedia`, and `SidebarProvider` reaches for it. */
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

function render(view: "overview" | "settings" | "feedback", onNavigate = () => {}) {
  act(() => {
    root!.render(
      createElement(SidebarProvider, {
        children: createElement(SidebarUtilityBar, { view, onNavigate }),
      }),
    );
  });
}

/** The bar's buttons, by their accessible name. */
function byName(name: string): HTMLElement | null {
  return host.querySelector(`[aria-label="${name}"]`);
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  stubMatchMedia();
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
});

afterEach(() => {
  act(() => root?.unmount());
  root = null;
  host.remove();
});

describe("the sidebar utility bar", () => {
  it("names every control it carries", () => {
    render("overview");

    // The four, in the order they are read. Discord keeps its full label —
    // "Discord" alone would not say that the control leaves the console.
    for (const name of ["Settings", "Feedback", "Join our Discord", "Collapse sidebar"]) {
      expect(byName(name), name).not.toBeNull();
    }
  });

  it("is a named group, so the bar itself is addressable", () => {
    render("overview");

    const group = host.querySelector('[role="group"]');
    expect(group?.getAttribute("aria-label")).toBe("Console utilities");
  });

  it("marks the control whose page is open, and only that one", () => {
    render("settings");

    // `aria-current="page"` is what the nav rows announce for the open page,
    // and these are the same kind of claim made by a smaller control.
    expect(byName("Settings")?.getAttribute("aria-current")).toBe("page");
    // Absent rather than "false": some readers announce `aria-current="false"`.
    expect(byName("Feedback")?.getAttribute("aria-current")).toBeNull();

    render("feedback");
    expect(byName("Feedback")?.getAttribute("aria-current")).toBe("page");
    expect(byName("Settings")?.getAttribute("aria-current")).toBeNull();
  });

  it("navigates on a click", () => {
    const onNavigate = vi.fn();
    render("overview", onNavigate);

    act(() => {
      byName("Settings")?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
    expect(onNavigate).toHaveBeenCalledWith("settings");

    act(() => {
      byName("Feedback")?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
    expect(onNavigate).toHaveBeenCalledWith("feedback");
  });

  it("leaves the console for Discord instead of routing to it", () => {
    render("overview");

    const discord = byName("Join our Discord");
    expect(discord?.tagName).toBe("A");
    expect(discord?.getAttribute("href")).toContain("discord");
    // `noreferrer` with `_blank` — an external tab must not get a handle back
    // onto the console's window.
    expect(discord?.getAttribute("target")).toBe("_blank");
    expect(discord?.getAttribute("rel")).toContain("noreferrer");
  });

  it("keeps the guided tour's Settings anchor", () => {
    render("overview");

    // The tour's "Connect your tools" stop spotlights `nav-settings`. It named
    // the nav row until Settings moved onto this bar; the attribute moved with
    // it, or the stop would anchor on nothing and be skipped silently.
    expect(byName("Settings")?.getAttribute("data-tour")).toBe("nav-settings");
  });
});
