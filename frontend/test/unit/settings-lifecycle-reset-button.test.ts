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
  it("renders when the client is platform-scoped, the company is live, and onReset is given", async () => {
    await render(clientWith(true), "running", () => {});
    expect(resetButton()).not.toBeUndefined();
  });

  it("is left out when onReset is not given (mirrors the `canCreateCompanies` gate upstream)", async () => {
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

describe("the Reset / Start clean button's click wiring", () => {
  it("calls the onReset callback it was given", async () => {
    const onReset = vi.fn();
    await render(clientWith(true), "running", onReset);
    const btn = resetButton();
    expect(btn).not.toBeUndefined();
    await act(async () => {
      btn?.click();
    });
    expect(onReset).toHaveBeenCalledTimes(1);
  });
});

describe("the Reset / Start clean button while another lifecycle action is in flight", () => {
  it("disables alongside Pause/Resume/Suspend/Archive, not just its own click", async () => {
    const onReset = vi.fn();
    // `hang: true` — the in-flight `pause` call never resolves, so `busy`
    // stays true for the duration of this test, the same window the operator
    // sees between a click and the host's response.
    await render(clientWith(true, true), "running", onReset);
    const pause = [...container.querySelectorAll("button")].find((b) =>
      b.textContent?.includes("Pause"),
    );
    expect(pause, "a running, platform-scoped company must offer Pause").not.toBeUndefined();
    await act(async () => {
      pause?.click();
    });
    const btn = resetButton();
    expect(btn, "Reset stays rendered while another action is busy").not.toBeUndefined();
    expect(btn?.disabled, "Reset must disable with the rest of the row").toBe(true);
  });
});
