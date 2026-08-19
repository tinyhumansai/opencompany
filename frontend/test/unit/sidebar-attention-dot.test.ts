// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { SidebarMenuBadge, SidebarMenuDot } from "@/components/ui/sidebar";

/**
 * The sidebar's attention signal survives collapse (issue #1018).
 *
 * The sidebar had exactly one attention signal — a count on Approvals — and
 * `SidebarMenuBadge` carries `group-data-[collapsible=icon]:hidden`, so it is
 * `display: none` the moment the rail collapses to icons. That rule is correct
 * on its own terms: a two-digit count does not fit a 32px rail, which is why
 * upstream hides it. The defect is that the count was the *only* signal, so
 * hiding it hid the fact that anything was waiting at all.
 *
 * **A collapsed rail showing nothing is indistinguishable from all-clear**, and
 * that is the failure: not a missing number, a missing answer to "does anything
 * need me?".
 *
 * # The same rule as the select popup's fade (issue #975)
 *
 * Both are controls that hid information without saying they were hiding
 * anything, and both are answered the same way rather than two ways: **the
 * signal must survive the constrained form, and must not depend on reading a
 * number.** There the clipped row is allowed to show through instead of being
 * painted over; here a mark replaces a count that cannot fit.
 *
 * # What this pins, and what it cannot
 *
 * jsdom applies no Tailwind, so `display` cannot be computed here — the visible
 * proof is a real Chromium measurement across both states, recorded in the PR.
 * What is pinned is the rule: the dot's visibility must be the exact **mirror**
 * of the badge's, so precisely one of the two shows at any width. A "fix" that
 * un-hid the badge instead (the thing the issue explicitly warns against) or
 * dropped the mirror fails here.
 */

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

function dot(label = "19 approvals need you") {
  act(() => {
    root.render(createElement(SidebarMenuDot, { label }));
  });
  return container.querySelector('[data-slot="sidebar-menu-dot"]');
}

describe("the sidebar's collapsed attention dot", () => {
  it("shows only once the rail has collapsed", () => {
    const cls = dot()?.className ?? "";
    expect(cls).toMatch(/(^|\s)hidden(\s|$)/);
    expect(cls).toContain("group-data-[collapsible=icon]:block");
  });

  it("is the exact mirror of the badge, so the two never both show", () => {
    // The badge hides when collapsed; the dot shows only then. If these ever
    // stop being opposites the rail either says nothing again or says it twice.
    act(() => {
      root.render(createElement(SidebarMenuBadge, null, "19"));
    });
    const badgeCls =
      container.querySelector('[data-slot="sidebar-menu-badge"]')?.className ?? "";
    expect(badgeCls).toContain("group-data-[collapsible=icon]:hidden");
    expect(dot()?.className ?? "").toContain("group-data-[collapsible=icon]:block");
  });

  it("names what is waiting, so the signal is not colour alone", () => {
    // A bare coloured span is invisible to a screen reader, and colour is not a
    // channel everyone receives. The label carries what the badge carried.
    const d = dot();
    expect(d?.getAttribute("role")).toBe("img");
    expect(d?.getAttribute("aria-label")).toBe("19 approvals need you");
    // `title` also gives the collapsed rail a hover explanation it did not have.
    expect(d?.getAttribute("title")).toBe("19 approvals need you");
  });

  it("does not render the count itself", () => {
    // The whole point: a number is what could not survive the rail. If the
    // digits come back, this has become the badge again and will be hidden with
    // it.
    expect(dot()?.textContent).toBe("");
  });

  it("still renders", () => {
    // Guards the guard: every assertion above passes against nothing.
    expect(dot()).not.toBeNull();
  });
});
