// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { WorkflowStep } from "@/onboarding/WorkflowStep";

/**
 * CodeRabbit review, PR #2046: `listWorkflowRuns` and `listWorkflows` used to
 * share one `await Promise.allSettled([...])`, so the mount effect published
 * NEITHER result to state until BOTH had settled. The run history is the
 * step's load-bearing half — the founder is looking at this card wondering
 * whether their run counted — and a workflow-name lookup that is merely slow
 * (not failed) held that answer hostage on "Checking your runs…" for no
 * reason: the label it supplies only replaces a fallback (`name()` already
 * falls back to the raw `workflowId`).
 *
 * This never showed up in `onboarding-gate-workflow-progress.test.ts` (a pure
 * function over already-resolved rows) — it is specifically about which of
 * two REQUESTS the render waits on, which only mounting the component and
 * controlling each promise independently can observe.
 */

let container: HTMLDivElement;
let root: Root;

function fakeClient(
  runs: () => Promise<{ runs: unknown[]; hasMore: boolean }>,
  workflows: () => Promise<unknown[]>,
): OpenCompanyClient {
  return {
    scopeFor: () => "/api/v1/company",
    get: async (path: string) => {
      if (path.includes("/workflows/runs")) return runs();
      if (path.includes("/workflows")) return workflows();
      throw new Error(`unexpected path: ${path}`);
    },
  } as unknown as OpenCompanyClient;
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

describe("WorkflowStep publishes each read as it settles", () => {
  it("renders the run's progress while the workflow-name lookup is still in flight", async () => {
    let resolveNames!: (names: unknown[]) => void;
    const namesPromise = new Promise<unknown[]>((resolve) => {
      resolveNames = resolve;
    });
    const client = fakeClient(
      async () => ({
        runs: [
          {
            seq: 1,
            atMillis: 1_700_000_000_000,
            workflowId: "research-request",
            scheduled: false,
            deliveries: [],
            pendingApprovals: [],
            verdict: "ok",
          },
        ],
        hasMore: false,
      }),
      () => namesPromise, // never resolves during this test — the slow read
    );

    await act(async () => {
      root.render(
        createElement(WorkflowStep, {
          client,
          company: null,
          onOpenWorkflows: () => {},
          onOpenApprovals: () => {},
        }),
      );
      await Promise.resolve();
      await Promise.resolve();
    });

    // The runs read settled; the workflow-names read is still pending. Before
    // the fix, `Promise.allSettled` kept the founder on "Checking your runs…"
    // regardless. `gate-workflow-step` (the always-rendered wrapper) is
    // present either way, so assert on the loader vs. the result directly.
    expect(
      container.querySelector('[data-testid="gate-workflow-succeeded"]'),
      "the run's own answer must not wait on the still-pending name lookup",
    ).toBeTruthy();
    expect(container.textContent).not.toContain("Checking your runs");

    // Let the slow read land too, so the effect's cleanup does not warn about
    // a promise this test left dangling.
    await act(async () => {
      resolveNames([]);
      await Promise.resolve();
    });
  });
});
