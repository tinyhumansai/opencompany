// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { WorkflowStep } from "@/onboarding/WorkflowStep";

/**
 * Codex review, PR #2046, across several rounds converging on one shape of
 * finding: `WorkflowStep` needs to keep re-reading run history for as long
 * as it is mounted, not just once. `client`/`company` never change just
 * because a run finishes, an approval gets decided elsewhere, or a NEW run
 * starts from another tab — and the activation poll cannot unmount this step
 * for a company that has not (yet) activated. The settled design polls
 * UNCONDITIONALLY (mirroring `useActivationGate`), guarded against overlap by
 * an in-flight check rather than by which `progress.kind` is currently
 * showing — narrower "only poll while running/waiting" guards kept missing a
 * kind one round at a time.
 */

const runningRun = {
  seq: 1,
  atMillis: 1_700_000_000_000,
  workflowId: "research-request",
  scheduled: false,
  deliveries: [],
  pendingApprovals: [],
  running: true,
  verdict: "running" as const,
};

const succeededRun = { ...runningRun, verdict: "ok" as const, running: false };

const awaitingApprovalRun = {
  ...runningRun,
  running: false,
  verdict: "awaiting-approval" as const,
  pendingApprovals: ["ap-1"],
};

let container: HTMLDivElement;
let root: Root;

function fakeClient(getRuns: () => Promise<{ runs: unknown[]; hasMore: boolean }>): OpenCompanyClient {
  return {
    scopeFor: () => "/api/v1/company",
    get: async (path: string) => {
      if (path.includes("/workflows/runs")) return getRuns();
      if (path.includes("/workflows")) return [];
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
  vi.useRealTimers();
});

describe("WorkflowStep's continuous run-history poll", () => {
  it("picks up the run settling without an unmount/reload", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    let calls = 0;
    const client = fakeClient(async () => {
      calls += 1;
      // First read (the mount effect): the run is still going. Every read
      // after that (the poll): it has finished.
      return { runs: [calls === 1 ? runningRun : succeededRun], hasMore: false };
    });

    await act(async () => {
      root.render(
        createElement(WorkflowStep, {
          client,
          company: null,
          onOpenWorkflows: () => {},
          onOpenApprovals: () => {},
        }),
      );
      await vi.advanceTimersByTimeAsync(0);
    });

    expect(
      container.querySelector('[data-testid="gate-workflow-running"]'),
      "the initial read must render the running state",
    ).toBeTruthy();
    expect(calls).toBe(1);

    // Advance past one poll tick without unmounting or changing props.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5000);
    });

    expect(calls, "the running state must have triggered a re-poll").toBeGreaterThan(1);
    expect(
      container.querySelector('[data-testid="gate-workflow-succeeded"]'),
      "the card must pick up the run settling without an unmount or reload",
    ).toBeTruthy();
    expect(container.querySelector('[data-testid="gate-workflow-running"]')).toBeNull();
  });

  it("does not start an overlapping poll while a request is still in flight", async () => {
    // Codex review, PR #2046: a host consistently slower than the poll
    // interval used to have EVERY response arrive already stale — each tick
    // incremented the request counter before the previous one could land, so
    // nothing was ever "the latest" by the time it resolved, and requests
    // piled up unboundedly while the card sat frozen. The fix is to never
    // start a new request while one is still outstanding, which this proves
    // directly: two poll intervals pass with the first request still
    // unresolved, and only one request has been issued.
    vi.useFakeTimers({ shouldAdvanceTime: true });
    let calls = 0;
    let resolveSecond!: (v: { runs: unknown[]; hasMore: boolean }) => void;
    const client = fakeClient(() => {
      calls += 1;
      if (calls === 1) return Promise.resolve({ runs: [runningRun], hasMore: false });
      return new Promise((resolve) => {
        resolveSecond = resolve;
      });
    });

    await act(async () => {
      root.render(
        createElement(WorkflowStep, {
          client,
          company: null,
          onOpenWorkflows: () => {},
          onOpenApprovals: () => {},
        }),
      );
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(calls).toBe(1);

    // First poll tick issues the second (slow) request.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5000);
    });
    expect(calls).toBe(2);

    // A second poll interval passes with that request still unresolved — no
    // third request must be issued while it is outstanding.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5000);
    });
    expect(calls, "a poll tick must not overlap a still-outstanding request").toBe(2);

    // Once it finally resolves, the next tick is free to issue another.
    await act(async () => {
      resolveSecond({ runs: [succeededRun], hasMore: false });
      await Promise.resolve();
    });
    expect(
      container.querySelector('[data-testid="gate-workflow-succeeded"]'),
      "the slow request's own response must still be applied, not discarded",
    ).toBeTruthy();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(5000);
    });
    expect(calls, "polling resumes once the outstanding request clears").toBe(3);
  });

  it("does not fire a poll tick while the initial mount read is still in flight", async () => {
    // The mount read and the poll share the same in-flight guard, so a slow
    // initial read (not merely a slow poll response) must also hold off the
    // very first tick rather than racing it.
    vi.useFakeTimers({ shouldAdvanceTime: true });
    let calls = 0;
    let resolveInitial!: (v: { runs: unknown[]; hasMore: boolean }) => void;
    const initial = new Promise<{ runs: unknown[]; hasMore: boolean }>((resolve) => {
      resolveInitial = resolve;
    });
    const client = fakeClient(() => {
      calls += 1;
      if (calls === 1) return initial;
      return Promise.resolve({ runs: [succeededRun], hasMore: false });
    });

    await act(async () => {
      root.render(
        createElement(WorkflowStep, {
          client,
          company: null,
          onOpenWorkflows: () => {},
          onOpenApprovals: () => {},
        }),
      );
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(calls).toBe(1);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(5000);
    });
    expect(
      calls,
      "a poll tick must not start while the initial read is still outstanding",
    ).toBe(1);

    await act(async () => {
      resolveInitial({ runs: [runningRun], hasMore: false });
      await Promise.resolve();
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5000);
    });
    expect(calls, "polling resumes once the initial read clears").toBe(2);
  });

  it("retries after the initial read fails, instead of staying stuck", async () => {
    // tinysweeper critique, PR #2046: the poll effect used to arm only on
    // `progress.kind === "running"`. A FAILED initial read leaves `progress`
    // at `null` forever, so that guard never opened and the "Couldn't read
    // this company's run history" message was permanently stuck — a
    // transient failure had no way back to a live card without a reload.
    vi.useFakeTimers({ shouldAdvanceTime: true });
    let calls = 0;
    const client = fakeClient(async () => {
      calls += 1;
      if (calls === 1) throw new Error("network blip");
      return { runs: [succeededRun], hasMore: false };
    });

    await act(async () => {
      root.render(
        createElement(WorkflowStep, {
          client,
          company: null,
          onOpenWorkflows: () => {},
          onOpenApprovals: () => {},
        }),
      );
      await vi.advanceTimersByTimeAsync(0);
    });

    expect(calls).toBe(1);
    expect(
      container.textContent,
      "the initial failure must render the couldn't-read message",
    ).toContain("run history just now");

    // Advance past one poll tick — the retry this fix adds.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5000);
    });

    expect(calls, "a failed initial read must be retried").toBeGreaterThan(1);
    expect(
      container.textContent,
      "a successful retry must replace the stuck failure message",
    ).not.toContain("run history just now");
    expect(
      container.querySelector('[data-testid="gate-workflow-succeeded"]'),
      "the retry's success must render normally",
    ).toBeTruthy();
  });

  it("re-polls a run that is waiting on an approval, not only a running one", async () => {
    // Codex review, PR #2046: an approval can be decided (or the queue can
    // strand it) from another tab entirely, so a card showing
    // "waiting-on-you" needs the same self-healing poll a "running" card
    // does, not just the narrower "running" case the first fix covered.
    vi.useFakeTimers({ shouldAdvanceTime: true });
    let calls = 0;
    const client = fakeClient(async () => {
      calls += 1;
      return { runs: [calls === 1 ? awaitingApprovalRun : succeededRun], hasMore: false };
    });

    await act(async () => {
      root.render(
        createElement(WorkflowStep, {
          client,
          company: null,
          onOpenWorkflows: () => {},
          onOpenApprovals: () => {},
        }),
      );
      await vi.advanceTimersByTimeAsync(0);
    });

    expect(calls).toBe(1);
    expect(container.querySelector('[data-testid="gate-workflow-waiting"]')).toBeTruthy();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(5000);
    });

    expect(calls, "a waiting-on-you card must also be re-polled").toBeGreaterThan(1);
    expect(
      container.querySelector('[data-testid="gate-workflow-succeeded"]'),
      "the card must pick up the approval having been decided elsewhere",
    ).toBeTruthy();
  });

  it("keeps polling a run that has already settled, to catch a newer run starting elsewhere", async () => {
    // Codex review, PR #2046: a founder can start a fresh run from another
    // tab while this card is still showing an older run's terminal outcome
    // (`none`, `did-not-finish`, `needs-rerun`, or even `succeeded`, briefly,
    // before the parent gate catches up and unmounts this step). Polling
    // used to stop once `progress.kind` left `running`/`waiting-on-you`;
    // it now runs unconditionally for as long as the step is mounted.
    vi.useFakeTimers({ shouldAdvanceTime: true });
    let calls = 0;
    const client = fakeClient(async () => {
      calls += 1;
      // The first read finds nothing; a later poll picks up a run that
      // started after this card mounted.
      return { runs: calls === 1 ? [] : [runningRun], hasMore: false };
    });

    await act(async () => {
      root.render(
        createElement(WorkflowStep, {
          client,
          company: null,
          onOpenWorkflows: () => {},
          onOpenApprovals: () => {},
        }),
      );
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(calls).toBe(1);
    expect(container.querySelector('[data-testid="gate-workflow-none"]')).toBeTruthy();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(5000);
    });

    expect(calls, "a settled ('none') card must still be re-polled").toBeGreaterThan(1);
    expect(
      container.querySelector('[data-testid="gate-workflow-running"]'),
      "a run started elsewhere must become visible without an unmount",
    ).toBeTruthy();
  });
});
