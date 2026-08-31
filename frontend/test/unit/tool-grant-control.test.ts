// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { HostingView } from "@/views/HostingView";

/**
 * The one-click grant, end to end through a view (issue #1796).
 *
 * `finance-health.test.ts` proves the verdict knows a grant is missing and
 * `hosting-view-branches.test.ts` proves the control renders. Neither proves the
 * thing the issue is actually about: that clicking it **writes the grant** and
 * that the page then stops saying the integration reaches nobody.
 *
 * That gap is exactly the failure mode worth guarding. A control that renders,
 * looks right, and quietly does nothing is strictly worse than the dead-end copy
 * it replaced — the operator would leave believing the integration works,
 * instead of leaving knowing it does not.
 *
 * Driven through `HostingView` rather than the component in isolation, because
 * the refresh is the half that lives in the view: the write returns a grant DTO,
 * and it is the view's own status re-read that has to move the badge.
 */

const HOSTING_UNGRANTED = {
  apiKeyConfigured: true,
  provider: "vercel",
  team: "team_abc",
  granted: false,
  inBuild: true,
  supportedProviders: ["vercel"],
};

/** The `/auth/me` answer for a viewer with `role`. */
function whoami(role: "admin" | "member") {
  return { id: "u1", email: "a@b.c", role, company: "acme", hasPassword: true };
}

const GRANTS = {
  allow: ["*", "hosting"],
  manifestAllow: ["*"],
  added: ["hosting"],
  grantable: ["chargebee", "composio", "hosting", "paypal", "search"],
  takesEffect: "on the next turn",
};

let container: HTMLDivElement;
let root: Root;

function at(testid: string): HTMLElement | null {
  return container.querySelector<HTMLElement>(`[data-testid="${testid}"]`);
}

async function click(el: HTMLElement) {
  await act(async () => {
    el.dispatchEvent(new MouseEvent("click", { bubbles: true }));
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

describe("the connect page's grant control", () => {
  it("writes the grant to the host and re-reads the status", async () => {
    // The status flips on the second read, exactly as the host's would once the
    // grant is stored — so the assertion below is about the view believing the
    // host, not about it optimistically hiding its own warning.
    let statusReads = 0;
    const get = vi.fn((path: string) => {
      if (path.endsWith("/auth/me")) return Promise.resolve(whoami("admin"));
      statusReads += 1;
      return Promise.resolve(
        statusReads === 1 ? HOSTING_UNGRANTED : { ...HOSTING_UNGRANTED, granted: true },
      );
    });
    const put = vi.fn().mockResolvedValue(GRANTS);
    const client = {
      scopeFor: () => "/api/v1/companies/acme",
      get,
      put,
    } as unknown as OpenCompanyClient;

    await act(async () => {
      root.render(createElement(HostingView, { client, company: "acme" }));
    });
    expect(at("hosting-not-granted")).not.toBeNull();

    const action = at("hosting-not-granted-action");
    expect(action).not.toBeNull();
    await click(action!);

    // The write went to the tool-grant route, naming the namespace this page is
    // about — not to the hosting route, which cannot confer a grant.
    expect(put).toHaveBeenCalledWith("/api/v1/companies/acme/tools/grants", {
      namespace: "hosting",
    });
    // And the page re-read its own status rather than assuming.
    expect(statusReads).toBe(2);
    expect(at("hosting-not-granted")).toBeNull();
    expect(at("hosting-connected")).not.toBeNull();
  });

  it("keeps the warning up when the host refuses the grant", async () => {
    // A non-admin, or a host that does not offer this namespace. The one thing
    // that must not happen is the warning disappearing: an operator who is told
    // nothing and sees the alert vanish has been told the integration works.
    let statusReads = 0;
    const get = vi.fn((path: string) => {
      if (path.endsWith("/auth/me")) return Promise.resolve(whoami("admin"));
      statusReads += 1;
      return Promise.resolve(HOSTING_UNGRANTED);
    });
    const put = vi.fn().mockRejectedValue(new Error("host said no"));
    const client = {
      scopeFor: () => "/api/v1/companies/acme",
      get,
      put,
    } as unknown as OpenCompanyClient;

    await act(async () => {
      root.render(createElement(HostingView, { client, company: "acme" }));
    });
    await click(at("hosting-not-granted-action")!);

    expect(put).toHaveBeenCalledTimes(1);
    // No re-read: there is nothing new to read, and pretending otherwise would
    // make a refusal look like a slow success.
    expect(statusReads).toBe(1);
    expect(at("hosting-not-granted")).not.toBeNull();
    expect(at("hosting-not-granted-action")).not.toBeNull();
  });

  it("does not offer a non-admin a control whose write is admin-only", async () => {
    // The prop behind this defaults CLOSED and is required, so a connect
    // surface cannot acquire the control by forgetting to ask who is looking.
    // The explanation still renders — a member needs to know why the
    // integration reaches nobody, and who can change it.
    const get = vi.fn((path: string) =>
      path.endsWith("/auth/me")
        ? Promise.resolve(whoami("member"))
        : Promise.resolve(HOSTING_UNGRANTED),
    );
    const put = vi.fn();
    const client = {
      scopeFor: () => "/api/v1/companies/acme",
      get,
      put,
    } as unknown as OpenCompanyClient;

    await act(async () => {
      root.render(createElement(HostingView, { client, company: "acme" }));
    });

    expect(at("hosting-not-granted")).not.toBeNull();
    expect(at("hosting-not-granted-action")).toBeNull();
    expect(put).not.toHaveBeenCalled();
  });

  it("treats a host with no user plane as a non-admin", async () => {
    // `/auth/me` 404s on a host that ships no user plane, and the resolution
    // must fail closed rather than throwing or defaulting open — an unresolved
    // role is not an admin.
    const get = vi.fn((path: string) =>
      path.endsWith("/auth/me")
        ? Promise.reject(new Error("no user plane"))
        : Promise.resolve(HOSTING_UNGRANTED),
    );
    const client = {
      scopeFor: () => "/api/v1/companies/acme",
      get,
      put: vi.fn(),
    } as unknown as OpenCompanyClient;

    await act(async () => {
      root.render(createElement(HostingView, { client, company: "acme" }));
    });

    expect(at("hosting-view")).not.toBeNull();
    expect(at("hosting-not-granted")).not.toBeNull();
    expect(at("hosting-not-granted-action")).toBeNull();
  });
});
