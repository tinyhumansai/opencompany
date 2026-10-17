// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { ApiError } from "@/api/types";
import { PeopleView } from "@/views/PeopleView";

/**
 * FINDING, not a passing pin: `PeopleView`'s own `isAdmin` gate
 * (`me?.role === "admin"`) fails closed the same way whether `/auth/me`
 * answered "member" or never answered at all — both leave `me` null-ish and
 * land on the `!isAdmin` branch. That branch is safe (no control is
 * offered either way) but it is not honest: a genuine read failure — a
 * dropped connection, a 500 — renders the identical "Only an admin can
 * manage people" notice a real member would see, rather than the page's own
 * `error` alert a few lines below it, which this failure mode can never
 * reach because the `!isAdmin` return fires first. Skipped because fixing it
 * is a `frontend/src` change, out of scope here; the safe half (no control
 * leaks) already holds and is asserted below.
 */

function clientAs(get: (path: string) => Promise<unknown>): OpenCompanyClient {
  return {
    scopeFor: () => "/api/v1/companies/acme",
    get,
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

async function show(element: React.ReactElement) {
  await act(async () => {
    root.render(element);
  });
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

describe("PeopleView, /auth/me merely failing to answer (not a real member)", () => {
  it("still offers no control — the safe half of fail-closed holds even on a network failure", async () => {
    const get = vi.fn((path: string) => {
      if (path.endsWith("/auth/me")) {
        return Promise.reject(new ApiError(0, "network_error", "cannot reach the company host"));
      }
      return Promise.reject(new Error(`unexpected GET ${path}`));
    });
    const client = clientAs(get);
    await show(createElement(PeopleView, { client, company: "acme" }));
    await act(async () => {});

    expect(container.textContent).not.toContain("Invite");
    expect(container.querySelector("[aria-label^='Manage']")).toBeNull();
    // The safe outcome must come from an actual failed /auth/me read, not
    // from PeopleView never asking in the first place — either would leave
    // the screen looking identical above.
    expect(get).toHaveBeenCalledWith(expect.stringMatching(/\/auth\/me$/));
  });

  // Fixed in `PeopleView`: the fail-closed branch renders the page's `error`
  // alert when the read failed and the member notice only when a member
  // actually answered, so a dropped connection no longer reads as "you lack
  // access".
  it("says the read failed, not that the viewer lacks access", async () => {
    const client = clientAs((path) => {
      if (path.endsWith("/auth/me")) {
        return Promise.reject(new ApiError(0, "network_error", "cannot reach the company host"));
      }
      return Promise.reject(new Error(`unexpected GET ${path}`));
    });
    await show(createElement(PeopleView, { client, company: "acme" }));
    await act(async () => {});

    expect(container.textContent).toContain("cannot reach the company host");
    expect(container.textContent).not.toContain("Only an admin can manage people");
  });
});
