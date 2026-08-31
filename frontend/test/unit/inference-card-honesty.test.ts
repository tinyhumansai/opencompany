// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { InferenceStatus } from "@/api/inference";
import { InferenceSection } from "@/views/connections/InferenceSection";

/**
 * The Inference card must not say two things at once (issues #1736, #1737).
 *
 * Both defects are the same defect: the card knew a fact about the host or
 * about the stored configuration and rendered something that contradicted it.
 * It offered a "Restart now" button on hosts where the route behind it can only
 * fail, and it rendered a Provider select that was a constant while the header
 * beside it rendered whatever the host actually holds.
 */

let container: HTMLDivElement;
let root: Root;

/** A status with everything nailed down; each test varies only what it is about. */
function status(over: Partial<InferenceStatus> = {}): InferenceStatus {
  return {
    provider: "managed",
    slug: "managed",
    baseUrl: "https://openrouter.ai/api/v1",
    models: {},
    defaultTierModels: {},
    source: "runtime",
    keyConfigured: true,
    cognition: "echo",
    usageMetering: "none",
    restartRequired: true,
    harnessReachable: true,
    canRebuildInPlace: true,
    ...over,
  };
}

/**
 * A client stub answering `GET …/inference` from a queue, so a test can stage
 * what the host holds before a save and what it holds after one.
 */
function stubClient(replies: InferenceStatus[], mutation?: InferenceStatus) {
  let reads = 0;
  const read = () => replies[Math.min(reads++, replies.length - 1)];
  const settled = () => mutation ?? replies[replies.length - 1];
  return {
    scopeFor: (company: string | null) =>
      company ? `/api/v1/companies/${company}` : "/api/v1/company",
    get: async (path: string) => (path.endsWith("/inference/models") ? [] : read()),
    put: async () => ({ status: settled(), note: "" }),
    del: async () => ({ status: settled(), note: "" }),
    post: async () => ({ status: settled(), note: "" }),
  } as unknown as OpenCompanyClient;
}

async function mount(client: OpenCompanyClient, canManage = true) {
  await act(async () => {
    root.render(createElement(InferenceSection, { client, company: "acme", canManage }));
  });
}

function testId(id: string) {
  return container.querySelector(`[data-testid="${id}"]`);
}

/**
 * What the Provider select currently reads, as the operator sees it. The
 * trigger renders its own chevron glyph into the same text node, so strip
 * anything that is not part of a provider label.
 */
function providerSelect(): string {
  const text = container.querySelector("#inference-provider")?.textContent ?? "";
  return text.replace(/[^\w\s()-]/g, "").trim();
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

describe("the restart notice offers an action only where one exists (issue #1736)", () => {
  it("offers Restart now when the host can rebuild a runtime in place", async () => {
    await mount(stubClient([status({ canRebuildInPlace: true })]));

    expect(testId("inference-restart-required")).not.toBeNull();
    expect(testId("inference-restart-now")).not.toBeNull();
    expect(testId("inference-restart-manual")).toBeNull();
  });

  it("withholds the button and names the real remedy when the host cannot", async () => {
    // `POST …/inference/restart` needs a `RuntimeRebuilder` wired into the
    // host. Where none is, it fails unconditionally with "this host cannot
    // rebuild a company runtime in place" — so a button here is a control whose
    // only possible outcome is a toast the operator can do nothing about.
    await mount(stubClient([status({ canRebuildInPlace: false })]));

    expect(testId("inference-restart-required")).not.toBeNull();
    expect(testId("inference-restart-now")).toBeNull();

    const manual = testId("inference-restart-manual");
    expect(manual).not.toBeNull();
    // The remedy, in both spellings the host could mean — the capability comes
    // from the host, which does not know which shell it is packaged in.
    expect(manual?.textContent).toContain("quit and reopen the app");
    expect(manual?.textContent).toContain("restart the server process");
  });

  it("keeps the notice itself either way — the restart is still required", async () => {
    await mount(stubClient([status({ canRebuildInPlace: false })]), false);
    expect(testId("inference-restart-required")?.textContent).toContain("Restart required.");
  });
});

describe("the Provider select shows the provider the host holds (issue #1737)", () => {
  it("opens on the stored provider rather than a hardcoded default", async () => {
    // The select was `useState("managed")` with nothing ever writing it back, so
    // it read "Managed (TinyHumans)" whatever was stored — including after a
    // full process restart, which just re-runs the same initializer. The header
    // beside it renders the host's provider, so the two disagreed on one card.
    await mount(stubClient([status({ provider: "openrouter" })]));
    expect(providerSelect()).toBe("OpenRouter");
  });

  it("follows the host to a provider it normalized on the way in", async () => {
    // `managed` is a legacy alias the host resolves to `openrouter`. The value
    // the select shows has to be the value the host came back with, or an
    // operator sees one vendor named in the header and another in the select
    // while their key is stored against exactly one of them.
    await mount(stubClient([status({ provider: "openai_compatible" })]));
    expect(providerSelect()).toBe("Custom (OpenAI-compatible)");
  });

  it("rehydrates after a save rather than snapping back to the default", async () => {
    // The reported sequence: save under one provider, and the select goes on
    // reading "Managed (TinyHumans)" while the header reads the saved one.
    const client = stubClient(
      [status({ provider: "managed" }), status({ provider: "openrouter" })],
      status({ provider: "openrouter" }),
    );
    await mount(client);
    expect(providerSelect()).toBe("Managed (TinyHumans)");

    await act(async () => {
      (testId("inference-save") as HTMLButtonElement).click();
    });
    await act(async () => {});

    expect(providerSelect()).toBe("OpenRouter");
  });

  it("names the key that belongs in the field, for the provider it is stored against", async () => {
    // The managed brain is OpenRouter with the platform paying, so a key set
    // here is sent to OpenRouter. This line asked for a TinyHumans key — true
    // when `managed` was a provider of its own, and never updated when it
    // stopped being one. It is what the reported 401 actually was.
    await mount(stubClient([status({ provider: "managed" })]));
    expect(testId("inference-key-note")?.textContent).toContain("an OpenRouter key");
  });
});
