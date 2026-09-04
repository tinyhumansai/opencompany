// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { CompanyFeed } from "@/hooks/use-company";
import type { OpenCompanyClient } from "@/api/client";
import type { CompanyStatus } from "@/api/types";
import { LifecycleControls } from "@/views/SettingsView";

/**
 * The Reset / Start clean button (#1807, SettingsView.tsx `LifecycleControls`)
 * had no render test of its own — tinysweeper flagged the gap (PR comment
 * 3879183959) against the repo's "every behavior change gets a focused test"
 * rule (AGENTS.md "Testing Guidelines"). `LifecycleControls` is exported from
 * `SettingsView.tsx` for exactly this: rendering the full `SettingsView` would
 * also pull in `ExternalHarnesses`/`PolicySettings`/`MemoryEngineCard`, none of
 * which this behavior touches.
 *
 * Covers the three-way gate the button renders behind (`onReset && platform &&
 * !archived`), that a click calls the callback the button was given, and that
 * it disables alongside the other lifecycle buttons while one of them is in
 * flight.
 *
 * Scoped to `LifecycleControls`'s own contract: the id/name the callback
 * closes over is `SettingsView`'s wiring (`onReset={() =>
 * onResetCompany(scoped, feed.status.name)}`), not this component's — it only
 * ever calls `onReset()` with no arguments.
 */

function statusWith(lifecycle: string): CompanyStatus {
  return {
    id: "acme",
    name: "Acme Robotics",
    lifecycle,
    pending_approvals: 0,
  };
}

function feedWith(lifecycle: string): CompanyFeed {
  return {
    status: statusWith(lifecycle),
    approvals: [],
    queue: "ready",
    now: Date.now(),
    refresh: () => Promise.resolve(),
  };
}

/** A client whose lifecycle call never resolves, so a click leaves it `busy`. */
function clientWith(platform: boolean, hang = false): OpenCompanyClient {
  return {
    carriesPlatformBearer: platform,
    lifecycle: () => (hang ? new Promise<CompanyStatus>(() => {}) : Promise.resolve(statusWith("paused"))),
  } as unknown as OpenCompanyClient;
}

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
  vi.restoreAllMocks();
});

function resetButton(): HTMLButtonElement | undefined {
  return [...container.querySelectorAll("button")].find((b) =>
    b.textContent?.includes("Reset / Start clean"),
  );
}

async function render(client: OpenCompanyClient, lifecycle: string, onReset?: () => void) {
  await act(async () => {
    root.render(
      createElement(LifecycleControls, {
        client,
        company: "acme",
        feed: feedWith(lifecycle),
        onReset,
      }),
    );
  });
}

describe("the Reset / Start clean button's render gate", () => {
  it("is left out while the product does not offer company creation", async () => {
    // Reset archives this company and provisions a replacement through the same
    // dialog "New company" opens — it is company creation wearing another
    // label, so it answers the same presentation question the other four
    // triggers do (`offersCompanyCreation`).
    await render(clientWith(true), "running", () => {});
    expect(resetButton()).toBeUndefined();
  });

  it("is left out when onReset is not given (mirrors the `offersCompanyCreation` gate upstream)", async () => {
    await render(clientWith(true), "running", undefined);
    expect(resetButton()).toBeUndefined();
  });

  it("is left out for a non-platform (magic-link) session — same rule as Archive/Suspend", async () => {
    await render(clientWith(false), "running", () => {});
    expect(resetButton()).toBeUndefined();
  });

  it("is left out once the company is already archived — nothing left to reset", async () => {
    await render(clientWith(true), "archived", () => {});
    expect(resetButton()).toBeUndefined();
  });
});

/**
 * Two cases retired with the control's entry point.
 *
 * They covered the button's click wiring — that it calls the `onReset` it was
 * given — and that it disables alongside Pause/Resume/Suspend/Archive while any
 * lifecycle action is in flight, rather than only guarding its own click.
 *
 * Both need the button on screen, and it does not render while the product does
 * not offer company creation (`src/product-scope.ts`). The wiring and the
 * disabled rule are untouched in `LifecycleControls`; turning the flag off
 * restores the control and both cases.
 *
 * Not re-pointed at `LifecycleControls` directly: that would test the component
 * below the gate, leaving these green while the shipped path renders nothing —
 * which is the false pass this branch has produced before.
 */
