// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { DeptLite } from "@/views/overview/kg/KnowledgeDetail";
import { KnowledgeGraphFullscreen } from "@/views/overview/kg/KnowledgeGraphFullscreen";

/**
 * Issue #1664: the mobile detail sheet covered the graph legend completely.
 *
 * At or below 820px the detail panel stops being a 300px right rail and becomes
 * a full-width bottom sheet. The legend was `z-10` at the bottom-left and the
 * sheet is `z-30` anchored to the same edge, so with any node selected the
 * whole legend disappeared — all seven kind labels and the workflow-placement
 * caveat that #1318 exists to make reachable without a hover.
 *
 * jsdom does no layout and evaluates no media query, so this pins the class
 * contract; `test/e2e/overview-responsive-chrome.spec.ts` measures the actual
 * boxes in a browser at both sides of the breakpoint.
 */

let host: HTMLElement;
let root: Root;

const DESKS: DeptLite[] = [
  { deptId: "desk:eng", teamId: "team:desk:eng", name: "Engineering", tagline: "", color: "var(--accent)" },
  { deptId: "desk:gtm", teamId: "team:desk:gtm", name: "Go-to-Market", tagline: "", color: "var(--ok)" },
];

function render(detail: boolean) {
  act(() => {
    root.render(
      createElement(KnowledgeGraphFullscreen, {
        deptList: DESKS,
        currentTeamId: DESKS[0].teamId,
        currentDept: DESKS[0],
        toolWiki: null,
        extraDetail: detail ? createElement("div", { "data-testid": "card" }, "a card") : undefined,
        legendSlot: createElement("div", null, "Notes Human AI teammate Tool Workflow Stage SOP task"),
        onNavDept: () => {},
        onBack: () => {},
        children: createElement("svg"),
      }),
    );
  });
}

function legend(): HTMLElement {
  const el = host.querySelector('[data-testid="kg-legend"]');
  expect(el, "the shell must render the legend it is given").not.toBeNull();
  return el as HTMLElement;
}

function sheet(): HTMLElement {
  const el = host.querySelector("aside");
  expect(el, "an open card must render the detail panel").not.toBeNull();
  return el as HTMLElement;
}

function deskSelector(): HTMLElement {
  const el = host.querySelector('[data-testid="kg-desk-selector"]');
  expect(el, "the shell must render the desk selector").not.toBeNull();
  return el as HTMLElement;
}

function paddle(direction: "Previous" | "Next"): HTMLElement {
  const el = host.querySelector(`[aria-label="${direction} desk"]`);
  expect(el).not.toBeNull();
  return el as HTMLElement;
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
});

afterEach(() => {
  act(() => root.unmount());
  host.remove();
});

describe("the graph legend and the detail panel (#1664)", () => {
  it("sits above the panel rather than under it", () => {
    render(true);
    // The panel is `z-30`. Anything sharing an edge with it and expecting to be
    // read has to be above that, which is the level the status slot and the
    // paddles were already given for the same reason (#1307).
    expect(sheet().className).toContain("z-30");
    expect(legend().className).toContain("z-40");
    expect(legend().className).not.toContain("z-10");
  });

  it("lifts clear of the bottom sheet at or below 820px", () => {
    render(true);
    const classes = legend().className;
    expect(classes).toContain("max-[820px]:bottom-[calc(55%+0.5rem)]");
    // …and cannot grow past the band that leaves: at its real cap the legend is
    // seven wrapped kinds plus an expanded caveat, and the desk selector is
    // directly above.
    //
    // Two terms, not one (review of PR #1752). 26% of the card is the share the
    // legend may take; `calc(45%-6rem)` is what is genuinely left once the desk
    // selector has stood in the band above it, and on a short card that is the
    // smaller of the two. A flat 26% put the legend 16px over the desk selector
    // at 700x400 — measured, in
    // `test/e2e/overview-responsive-chrome.spec.ts`, which is where the numbers
    // behind both terms live.
    expect(classes).toContain("max-[820px]:max-h-[min(26%,calc(45%-6rem))]");
    expect(classes).toContain("max-[820px]:overflow-y-auto");
  });

  it("keeps out of the paddles' columns rather than only above them", () => {
    // Review of PR #1752. Separating the legend from the paddles vertically
    // holds only while the band is tall enough to stack them, and the band is a
    // percentage of a card that is shorter than the window: measured at 700x600
    // the card is 522px, the sheet takes 287 and the legend's cap 136, leaving
    // 21px between the desk selector and the legend for an 80px paddle. There
    // is no percentage that fits. So they are separated on the axis that has
    // room — the paddles hug the edges, and the legend starts inside the left
    // one and stops short of the right one.
    render(true);
    const classes = legend().className;
    expect(classes).toContain("max-[820px]:left-16");
    expect(classes).toContain("max-[820px]:max-w-[calc(100%-8rem)]");
    // The paddles narrow below 640px, and the legend follows them in.
    expect(classes).toContain("max-[639px]:left-12");
    expect(classes).toContain("max-[639px]:max-w-[calc(100%-6rem)]");
    // And `left` had to leave the shared class list for the same reason
    // `bottom` did: a `sm:left-5` there outranks `max-[820px]:left-16`.
    expect(classes).not.toContain("sm:left-");
  });

  it("names the same fraction the sheet is capped at, so the two cannot drift", () => {
    render(true);
    const cap = /max-h-\[(\d+)%\]/.exec(sheet().className);
    const lift = /bottom-\[calc\((\d+)%\+/.exec(legend().className);
    expect(cap, "the sheet must carry a percentage cap").not.toBeNull();
    expect(lift, "the legend must lift by that same percentage").not.toBeNull();
    expect(lift![1]).toBe(cap![1]);
  });

  it("caps the sheet against the graph card rather than the viewport", () => {
    // The panel is absolutely positioned inside the graph surface, which sits on
    // the console's inset card and is shorter than the window. `62vh` measured
    // 68% of that card at 430x932, so the band every other offset was derived
    // from never existed. Same correction `Overview.tsx` made when the graph
    // claimed `h-svh` inside the card and cropped its own legend.
    render(true);
    expect(sheet().className).not.toContain("vh]");
    expect(paddle("Next").className).not.toContain("vh]");
  });

  it("steps the desk selector out of the paddles' column while a card is open", () => {
    // Review of PR #1752. With a card open at or below 820px the paddles leave
    // mid-height and rise into the band the selector already occupies, and they
    // are `z-40` over its `z-20` — so an overlap covers the first desk chip and
    // takes its clicks. Measured at 700x400 the Previous-desk paddle spanned
    // y 28..108 against the selector's 33..83, and at 700x600 y 45..125 against
    // the same 33..83.
    //
    // Column, not vertical offset: how far down the paddle would have to go
    // depends on how many lines the chips wrap to, which depends on the
    // company. The same rule the legend follows.
    render(true);
    expect(deskSelector().className).toContain("max-[820px]:left-16");
    expect(deskSelector().className).toContain("max-[639px]:left-12");
  });

  it("gives the desk selector the whole corner back when the card closes", () => {
    // With no panel the paddles are at mid-height and nowhere near it.
    render(true);
    render(false);
    expect(deskSelector().className).not.toContain("max-[820px]:left-16");
    expect(deskSelector().className).toContain("left-5");
  });

  it("keeps the paddles out of the strip the legend now takes", () => {
    render(true);
    for (const direction of ["Previous", "Next"] as const) {
      expect(paddle(direction).className).toContain("max-[820px]:top-[17%]");
    }
  });

  it("stops short of the right rail above 820px instead of running under it", () => {
    // Above the breakpoint the panel is the 300px rail and the legend keeps its
    // corner — but at 900x800 it measured 280px *under* that rail. While it was
    // `z-10` that read as clipped; at `z-40` it would read as covering the card.
    render(true);
    expect(legend().className).toContain("max-w-[calc(100%-21rem)]");
  });

  it("uses no `sm:` variant while a card is open", () => {
    // Load-bearing, not stylistic. Tailwind emits `sm:` (min-width 640) after
    // `max-[820px]`, so a `sm:bottom-5` in this list wins at 700px and the lift
    // above silently does not happen — which is what the first attempt did.
    render(true);
    expect(legend().className).not.toMatch(/\bsm:bottom-/);
    expect(legend().className).not.toMatch(/\bsm:max-w-/);
    // `sm:left-` joined them in the review of PR #1752: it lived in the shared
    // class list, outranked `max-[820px]:left-16` at 800px, and kept the legend
    // sitting under the Previous-desk paddle.
    expect(legend().className).not.toMatch(/\bsm:left-/);
  });

  it("returns to the bottom corner, full width, when the card closes", () => {
    render(true);
    render(false);
    const classes = legend().className;
    expect(classes).toContain("bottom-3");
    expect(classes).toContain("sm:bottom-5");
    expect(classes).toContain("sm:max-w-[calc(100%-2.5rem)]");
    expect(classes).not.toContain("max-w-[calc(100%-21rem)]");
    expect(classes).not.toContain("max-[820px]:bottom-[calc(55%+0.5rem)]");
    expect(classes).not.toContain("overflow-y-auto");
  });
});
