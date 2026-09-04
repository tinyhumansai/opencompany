// @vitest-environment jsdom
//
// Bug B-006: the gate renders instead of the router outlet, so an `<a href="#/…">`
// inside it changed `location.hash`, re-rendered the same checklist, and read to
// the founder as a link that does nothing. "Decide in Approvals" was the one
// that got found; rewriting only that anchor would have left the next one to be
// discovered the same way. The gate therefore refuses to swallow ANY in-app hash
// link, wherever inside it the link came from — which is what these cover.

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { ActivationStatus } from "@/api/activation";
import type { OpenCompanyClient } from "@/api/client";
import { OnboardingGate } from "@/onboarding/OnboardingGate";

let container: HTMLDivElement;
let root: Root;

const status: ActivationStatus = {
  nameConfirmed: false,
  integrationConnected: true,
  workflowRunSucceeded: true,
  isActivated: false,
};

// The gate's name step needs no network; the other two are not opened here.
const client = {
  scopeFor: () => "/api/v1/companies/acme",
  get: () => Promise.resolve({ runs: [] }),
} as unknown as OpenCompanyClient;

function renderGate(onLeave: (route: string) => void) {
  act(() =>
    root.render(
      createElement(OnboardingGate, {
        client,
        company: "acme",
        status,
        currentName: "Acme",
        onRefresh: () => {},
        onSkip: () => {},
        onLeave,
        onWaiveStep: () => {},
      }),
    ),
  );
}

/** Drops an anchor inside the gate, the way any embedded view would. */
function plantAnchor(href: string, attrs: Record<string, string> = {}) {
  const host = container.querySelector('[data-testid="gate-step-name"]');
  const a = document.createElement("a");
  a.setAttribute("href", href);
  for (const [k, v] of Object.entries(attrs)) a.setAttribute(k, v);
  a.textContent = "decide in Approvals";
  host!.appendChild(a);
  return a;
}

function click(el: Element, init: MouseEventInit = {}) {
  const ev = new MouseEvent("click", { bubbles: true, cancelable: true, button: 0, ...init });
  act(() => {
    el.dispatchEvent(ev);
  });
  return ev;
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.restoreAllMocks();
});

describe("the gate never swallows an in-app link", () => {
  it("sends a hash link out through onLeave instead of letting it go nowhere", () => {
    const onLeave = vi.fn();
    renderGate(onLeave);
    const ev = click(plantAnchor("#/approvals"));
    expect(onLeave).toHaveBeenCalledWith("#/approvals");
    // Prevented, so the browser never performs the navigation the gate would
    // have rendered straight through.
    expect(ev.defaultPrevented).toBe(true);
  });

  it("covers any route, not just the one that was reported", () => {
    const onLeave = vi.fn();
    renderGate(onLeave);
    click(plantAnchor("#/observatory"));
    expect(onLeave).toHaveBeenCalledWith("#/observatory");
  });

  it("catches a click on something nested inside the anchor", () => {
    const onLeave = vi.fn();
    renderGate(onLeave);
    const a = plantAnchor("#/approvals");
    const span = document.createElement("span");
    a.appendChild(span);
    click(span);
    expect(onLeave).toHaveBeenCalledWith("#/approvals");
  });

  it("leaves an external link alone", () => {
    const onLeave = vi.fn();
    renderGate(onLeave);
    const ev = click(plantAnchor("https://example.com/docs"));
    expect(onLeave).not.toHaveBeenCalled();
    expect(ev.defaultPrevented).toBe(false);
  });

  it("leaves a new-tab click alone — the gate is not in the way there", () => {
    const onLeave = vi.fn();
    renderGate(onLeave);
    const ev = click(plantAnchor("#/approvals"), { metaKey: true });
    expect(onLeave).not.toHaveBeenCalled();
    expect(ev.defaultPrevented).toBe(false);
  });

  it("leaves a target=_blank link alone", () => {
    const onLeave = vi.fn();
    renderGate(onLeave);
    click(plantAnchor("#/approvals", { target: "_blank" }));
    expect(onLeave).not.toHaveBeenCalled();
  });

  it("does not hijack the gate's own buttons", () => {
    const onLeave = vi.fn();
    renderGate(onLeave);
    const skip = container.querySelector('[data-testid="gate-skip"]')!;
    click(skip);
    expect(onLeave).not.toHaveBeenCalled();
  });

  it("does not forward a hash whose first segment names no console route", () => {
    // CodeRabbit review, PR #2046: nothing this gate renders today puts an
    // `<a>` in its own tree, so this has no live trigger yet — but the
    // handler's own doc promises to catch a link "wherever it came from",
    // which the bare `href?.startsWith("#")` check alone did not keep: any
    // hash at all was forwarded to `onLeave`, whether or not it named a
    // route this console actually serves.
    const onLeave = vi.fn();
    renderGate(onLeave);
    const ev = click(plantAnchor("#/not-a-real-view"));
    expect(onLeave, "an unrecognized route must not reach onLeave").not.toHaveBeenCalled();
    // Left alone rather than intercepted — the same "browser's own default
    // is correct" treatment a modified click or an external link already
    // gets. `onLeave`'s side effects (standing the gate down, marking it
    // skipped) are reserved for a route this console actually validated.
    expect(ev.defaultPrevented).toBe(false);
  });

  it("still validates a route that carries a sub-page", () => {
    const onLeave = vi.fn();
    renderGate(onLeave);
    click(plantAnchor("#/connections/apps"));
    expect(onLeave).toHaveBeenCalledWith("#/connections/apps");
  });
});
