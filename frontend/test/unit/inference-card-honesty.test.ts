// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { InferenceStatus } from "@/api/inference";
import { InferenceView } from "@/views/InferenceView";

/**
 * The Inference card must not say two things at once (issues #1736, #1737).
 *
 * Both defects were the same defect: the card knew a fact about the host or
 * about the stored configuration and rendered something that contradicted it.
 *
 * #1736 — the "Restart now" button offered on hosts where the route behind it
 * can only fail — still has a control to hold it to, and these tests follow it
 * onto the page that replaced the card.
 *
 * #1737's half was about a **Provider select that no longer exists**: the page
 * showed one provider chosen from a constant list, and that control is what the
 * provider list replaces. Its tests went with it rather than being re-pointed at
 * a control that answers a different question — a test kept alive by rewriting
 * what it asserts is not the same test.
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
    // The catalog route answers with an object naming the endpoint that was
    // read, not a bare array (`InferenceModelCatalog`). These tests assert
    // nothing about the picker, so the stub answers the host's *unreadable
    // catalog* reply — a 200 carrying `error`, with no `tierVocabulary`,
    // because "we could not ask" is not the same fact as `"unknown"`.
    //
    // Deliberately not `{models: [], tierVocabulary: "unknown"}`: the host
    // cannot produce that pairing. `list_models` only sets a vocabulary in its
    // success arm, and `catalog_models` treats an empty catalog as a failure,
    // so an empty list always arrives with `error` set and no vocabulary. A
    // double that answered a shape the host cannot emit would let these tests
    // pass on behaviour nothing real can reach.
    get: async (path: string) =>
      // The page resolves the viewer's role for itself now, so the stub has to
      // answer `/auth/me`. An unresolved role fails closed by design, and a
      // read-only page would hide the very control these tests are about.
      path.endsWith("/auth/me")
        ? { user: { id: "u1", email: "admin@acme.test", role: "admin" }, role: "admin" }
        : path.endsWith("/inference/routes")
        ? { routes: {}, mode: "managed", orphaned: [] }
        : path.endsWith("/inference/models")
        ? {
            baseUrl: "https://openrouter.ai/api/v1",
            models: [],
            tierDefaults: {},
            error:
              "Could not list models from https://openrouter.ai/api/v1: connection refused. " +
              "Enter model ids directly.",
          }
        : read(),
    put: async () => ({ status: settled(), note: "" }),
    del: async () => ({ status: settled(), note: "" }),
    post: async () => ({ status: settled(), note: "" }),
  } as unknown as OpenCompanyClient;
}

async function mount(client: OpenCompanyClient, canManage = true) {
  await act(async () => {
    root.render(createElement(InferenceView, { client, company: "acme" }));
    void canManage;
  });
}

function testId(id: string) {
  return container.querySelector(`[data-testid="${id}"]`);
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
