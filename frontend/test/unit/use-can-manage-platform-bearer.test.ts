// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ApiError } from "@/api/types";
import type { OpenCompanyClient } from "@/api/client";
import { HostingView } from "@/views/HostingView";

/**
 * `AdminScopedCompany` (`scope.rs`) admits the platform bearer directly, with
 * no session behind it — but `resolve_principal` prefers a session over the
 * bearer whenever one resolves. `useCanManage` has to land on the same
 * answer: grant the bearer-only case, and still refuse a member whose session
 * resolves, even on a client that also carries a bearer. A read that merely
 * failed to resolve a session — a timeout, a 5xx — is a third case: it must
 * not be treated as proof there is none.
 */

const HOSTING = {
  apiKeyConfigured: true,
  provider: "vercel",
  team: "team_abc",
  granted: true,
  inBuild: true,
  supportedProviders: ["vercel"],
};

function clientAs(opts: {
  platformBearer: boolean;
  me: "admin" | "member" | "none" | "unreachable";
}): OpenCompanyClient {
  return {
    scopeFor: () => "/api/v1/companies/acme",
    carriesPlatformBearer: opts.platformBearer,
    get: (path: string) => {
      if (path.endsWith("/auth/me")) {
        if (opts.me === "none") {
          return Promise.reject(new ApiError(401, "unauthorized", "not signed in", true));
        }
        if (opts.me === "unreachable") {
          return Promise.reject(new ApiError(0, "network_error", "cannot reach the company host"));
        }
        return Promise.resolve({
          id: "u1",
          email: "a@b.c",
          role: opts.me,
          company: "acme",
          hasPassword: true,
        });
      }
      return Promise.resolve(HOSTING);
    },
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

async function show(element: React.ReactElement) {
  await act(async () => {
    root.render(element);
  });
}

function at(testid: string): HTMLElement | null {
  return container.querySelector<HTMLElement>(`[data-testid="${testid}"]`);
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

describe("useCanManage, platform bearer vs member session", () => {
  it("offers the platform bearer the whole form when there is no /auth/me user", async () => {
    const client = clientAs({ platformBearer: true, me: "none" });
    await show(createElement(HostingView, { client, company: "acme" }));

    expect(at("hosting-read-only")).toBeNull();
    expect(at("hosting-api-key")).not.toBeNull();
    expect(at("hosting-save")).not.toBeNull();
  });

  it("still refuses a member session, even on a client that also carries a platform bearer", async () => {
    const client = clientAs({ platformBearer: true, me: "member" });
    await show(createElement(HostingView, { client, company: "acme" }));

    expect(at("hosting-api-key")).toBeNull();
    expect(at("hosting-save")).toBeNull();
    expect(at("hosting-read-only")).not.toBeNull();
  });

  it("fails closed with no bearer and no session", async () => {
    const client = clientAs({ platformBearer: false, me: "none" });
    await show(createElement(HostingView, { client, company: "acme" }));

    expect(at("hosting-api-key")).toBeNull();
    expect(at("hosting-save")).toBeNull();
    expect(at("hosting-read-only")).not.toBeNull();
  });

  it("fails closed on a platform bearer when /auth/me merely couldn't be reached", async () => {
    const client = clientAs({ platformBearer: true, me: "unreachable" });
    await show(createElement(HostingView, { client, company: "acme" }));

    expect(at("hosting-api-key")).toBeNull();
    expect(at("hosting-save")).toBeNull();
    expect(at("hosting-read-only")).not.toBeNull();
  });
});
