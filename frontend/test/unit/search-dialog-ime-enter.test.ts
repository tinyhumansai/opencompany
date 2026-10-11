// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { SearchResult } from "@/search/types";

/**
 * Search is the most IME-heavy input in the console: a CJK operator types the
 * whole query through a composition and confirms it with Enter. That
 * confirming Enter still carries `isComposing` on the keydown, and the dialog
 * used to read it as "activate the highlighted result" — closing the palette
 * and navigating away mid-composition. The dialog must leave the key alone
 * while a composition is being confirmed and activate only on a settled Enter.
 */

const RESULT: SearchResult = {
  kind: "channel",
  id: "channel-general",
  title: "#general",
  titleMatches: [],
  href: "#room/general",
  action: "Open #general",
  score: 1,
};

vi.mock("@/search/useSearch", () => ({
  useSearch: () => ({
    groups: [{ kind: "channel", label: "Channels", results: [RESULT] }],
    loading: false,
  }),
}));

const { SearchDialog } = await import("@/search/SearchDialog");

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  window.location.hash = "";
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  window.location.hash = "";
});

function renderDialog(onOpenChange: (open: boolean) => void) {
  act(() => {
    root.render(
      createElement(SearchDialog, {
        client: {} as OpenCompanyClient,
        company: "acme",
        open: true,
        onOpenChange,
      }),
    );
  });
  // The dialog renders through a portal, so it lives on body, not in container.
  return document.body.querySelector<HTMLInputElement>('[data-testid="search-input"]')!;
}

// Setting `.value` directly only moves the DOM node; React's value tracker
// then sees no change and swallows the input event. Go through the prototype
// setter so the controlled input actually receives the edit.
function setValue(target: HTMLInputElement, value: string) {
  Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(target, value);
  act(() => {
    target.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

function enter(target: HTMLInputElement, isComposing: boolean) {
  act(() => {
    target.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true, isComposing }),
    );
  });
}

describe("SearchDialog and IME composition", () => {
  it("does not activate a result while a composition is being confirmed", () => {
    const onOpenChange = vi.fn();
    const input = renderDialog(onOpenChange);
    setValue(input, "かな");
    enter(input, true);
    expect(onOpenChange).not.toHaveBeenCalled();
    expect(window.location.hash).toBe("");
  });

  it("does not activate on Safari's completing Enter (compositionend, then a plain Enter)", () => {
    // WebKit bug 165004: the Enter that commits the candidate arrives after
    // compositionend, isComposing already false. Only the burst marks it.
    const onOpenChange = vi.fn();
    const input = renderDialog(onOpenChange);
    setValue(input, "かな");
    act(() => {
      input.dispatchEvent(new Event("compositionend", { bubbles: true }));
    });
    enter(input, false);
    expect(onOpenChange).not.toHaveBeenCalled();
    expect(window.location.hash).toBe("");
  });

  it("still activates the highlighted result on a settled Enter", () => {
    const onOpenChange = vi.fn();
    const input = renderDialog(onOpenChange);
    setValue(input, "general");
    enter(input, false);
    expect(onOpenChange).toHaveBeenCalledWith(false);
    expect(window.location.hash).toBe("#room/general");
  });
});
