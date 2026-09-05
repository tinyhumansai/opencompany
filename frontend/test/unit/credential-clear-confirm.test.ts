// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";

const api = vi.hoisted(() => ({
  clearBilling: vi.fn(),
  clearHosting: vi.fn(),
  getHosting: vi.fn(),
}));

vi.mock("@/api/billing", () => ({
  clearBilling: api.clearBilling,
  saveBilling: vi.fn(),
}));

vi.mock("@/api/hosting", () => ({
  clearHosting: api.clearHosting,
  getHosting: api.getHosting,
  saveHosting: vi.fn(),
}));

vi.mock("sonner", () => ({
  toast: { error: vi.fn(), info: vi.fn(), success: vi.fn() },
}));

const { ChargebeeForm } = await import("@/views/finance/ChargebeeForm");
const { HostingView } = await import("@/views/HostingView");

let container: HTMLDivElement;
let root: Root;

/**
 * A client that answers `/auth/me` as an admin.
 *
 * The role is stated rather than left to a stub with no `get`: `HostingView`
 * withholds Disconnect from a non-admin, and an unanswerable `/auth/me` reads
 * as non-admin. A fixture that did not say would be asserting a member's view
 * of a confirmation flow only an admin can reach.
 */
function client(): OpenCompanyClient {
  return {
    scopeFor: () => "/api/v1/company",
    get: () =>
      Promise.resolve({ id: "u1", email: "a@b.c", role: "admin", company: "acme" }),
  } as unknown as OpenCompanyClient;
}

function button(label: string): HTMLButtonElement {
  const found = Array.from(document.querySelectorAll("button")).find(
    (candidate) => candidate.textContent?.trim() === label,
  );
  if (!found) throw new Error(`No “${label}” button in ${document.body.innerHTML}`);
  return found as HTMLButtonElement;
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  vi.clearAllMocks();
});

afterEach(async () => {
  await act(async () => root.unmount());
  container.remove();
});

describe("credential clearing confirmation (issue #1471)", () => {
  it("does not clear Chargebee credentials until confirmation", async () => {
    api.clearBilling.mockResolvedValue({});
    await act(async () => {
      root.render(
        createElement(ChargebeeForm, {
          client: client(),
          company: "acme",
          status: {
            site: null,
            webhookUrl: null,
            apiKeyConfigured: true,
            webhookConfigured: true,
            granted: true,
            inBuild: true,
          },
          onStatus: vi.fn(),
        }),
      );
    });

    await act(async () => button("Disconnect").click());
    expect(api.clearBilling).not.toHaveBeenCalled();
    expect(document.body.textContent).toContain("write-only Chargebee API key");

    await act(async () => button("Disconnect Chargebee").click());
    expect(api.clearBilling).toHaveBeenCalledOnce();
  });

  it("does not disconnect hosting until confirmation", async () => {
    api.getHosting.mockResolvedValue({
      provider: "vercel",
      inBuild: true,
      granted: true,
      apiKeyConfigured: true,
    });
    api.clearHosting.mockResolvedValue({
      provider: "vercel",
      inBuild: true,
      granted: true,
      apiKeyConfigured: false,
    });
    const fakeClient = client();
    await act(async () => {
      root.render(createElement(HostingView, { client: fakeClient, company: "acme" }));
      await Promise.resolve();
    });

    await act(async () => button("Disconnect").click());
    expect(api.clearHosting).not.toHaveBeenCalled();
    expect(document.body.textContent).toContain("write-only hosting token");

    await act(async () => button("Disconnect hosting").click());
    expect(api.clearHosting).toHaveBeenCalledWith(fakeClient, "acme");
  });
});
