// @vitest-environment jsdom

// The add-provider picker offers no CLI-logins group. A CLI login has no key
// and no endpoint, so nothing is ever added for one at company level; a
// teammate binds to a harness on its own Model tab. A labelled group that can
// never be chosen reads as "this is switched off here", which is what this
// asserts stays gone.

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { InferenceStatus } from "@/api/inference";
import { InferenceView } from "@/views/InferenceView";

let container: HTMLDivElement;
let root: Root;

function status(): InferenceStatus {
  return {
    provider: "managed",
    slug: "managed",
    baseUrl: "https://openrouter.ai/api/v1",
    models: {},
    source: "runtime",
    keyConfigured: false,
    cognition: "echo",
    usageMetering: "none",
    restartRequired: false,
    harnessReachable: true,
    canRebuildInPlace: false,
    providers: [],
    routes: {},
    managed: { source: "none", configured: false, baseUrl: "" },
  };
}

function stubClient(): OpenCompanyClient {
  return {
    scopeFor: (company: string | null) =>
      company ? `/api/v1/companies/${company}` : "/api/v1/company",
    get: async (path: string) =>
      path.endsWith("/auth/me")
        ? { user: { id: "u1", email: "admin@acme.test", role: "admin" }, role: "admin" }
        : status(),
  } as unknown as OpenCompanyClient;
}

async function mount() {
  await act(async () => {
    root.render(createElement(InferenceView, { client: stubClient(), company: "acme" }));
  });
  await act(async () => {});
}

function testId(id: string) {
  return document.querySelector(`[data-testid="${id}"]`);
}

async function click(el: Element | null) {
  if (!el) throw new Error("nothing to click");
  await act(async () => {
    (el as HTMLElement).click();
  });
  await act(async () => {});
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
});

describe("the add-provider picker has no CLI-logins group", () => {
  it("offers cloud and local runtimes, and nothing for CLI logins", async () => {
    await mount();
    await click(testId("inference-add-open"));

    const dialog = testId("inference-add-provider");
    expect(dialog).not.toBeNull();
    expect(document.querySelector("#inference-add-cloud")).not.toBeNull();
    expect(document.querySelector("#inference-add-local")).not.toBeNull();

    expect(document.querySelector("#inference-add-cli")).toBeNull();
    expect(dialog?.textContent).not.toContain("CLI logins");
    expect(dialog?.textContent).not.toContain("Not available on this host.");
  });

  it("points at the roster instead, and closes when that link is taken", async () => {
    await mount();
    await click(testId("inference-add-open"));

    const hint = testId("inference-add-harness-hint");
    expect(hint).not.toBeNull();
    expect(hint?.querySelector("a")?.getAttribute("href")).toBe("#/company/agents");

    await click(hint?.querySelector("a") ?? null);
    expect(testId("inference-add-provider")).toBeNull();
  });
});
