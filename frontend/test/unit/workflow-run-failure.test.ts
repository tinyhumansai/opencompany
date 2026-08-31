// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { WorkflowGraph, WorkflowRunOutcome } from "@/api/workflows";
import { ApiError } from "@/api/types";

/**
 * A failed run leaves something on screen (issue #1007).
 *
 * The reported symptom is an absence, and an absence is the one thing the pure
 * helpers next door cannot pin. `classifyRunError` already decides *which*
 * failure a rejected POST was, and it was right — the console then took that
 * verdict and rendered nothing with it. `RunResultPanel` is the only surface
 * that shows run detail and it mounts from a settled body, which the failure
 * path by definition never has, so the console dropped back to its resting
 * state behind a four-second toast.
 *
 * So this file renders the view, the way `task-blocked-card.test.ts` earns its
 * exception to the pure-function rule: the claim under test is about what
 * survives the toast, and the only place that is true is the DOM.
 *
 * Two claims, both from the issue:
 *
 *  1. a rejected run leaves a persistent failure drawer, and opens the History
 *     drawer that holds the journaled row; and
 *  2. dispatching a run clears the previous run's result drawer, so run #1's
 *     nodes and "Requested:" line are never presented as run #2's detail.
 */

const toasts = vi.hoisted(() => ({
  base: vi.fn(),
  success: vi.fn(),
  error: vi.fn(),
  warning: vi.fn(),
  info: vi.fn(),
}));

vi.mock("sonner", () => {
  const toast = Object.assign(toasts.base, {
    success: toasts.success,
    error: toasts.error,
    warning: toasts.warning,
    info: toasts.info,
  });
  return { toast };
});

vi.mock("next-themes", () => ({ useTheme: () => ({ resolvedTheme: "light" }) }));

// The canvas is a React Flow instance, and React Flow measures its container on
// mount. jsdom has no layout and no `ResizeObserver`, so these three stubs are
// what let the view render at all — none of them is under test.
class NoopResizeObserver {
  observe() {}
  unobserve() {}
  disconnect() {}
}
Object.assign(globalThis, {
  ResizeObserver: NoopResizeObserver,
  DOMMatrixReadOnly: class {
    m22 = 1;
  },
});
Object.defineProperties(globalThis.HTMLElement.prototype, {
  offsetHeight: { get: () => 400 },
  offsetWidth: { get: () => 800 },
});

const { WorkflowsView } = await import("@/views/WorkflowsView");
const { RunHistoryPanel } = await import("@/views/workflows/RunHistoryPanel");
const { RunFailurePanel } = await import("@/views/workflows/RunFailurePanel");
const { runFailureFrom } = await import("@/views/workflows/run-failure");
const { formatDuration, runDuration } = await import("@/views/workflows/run-health");

const GRAPH: WorkflowGraph = {
  id: "digest",
  name: "Weekly digest",
  version: null,
  nodes: [
    { id: "start", kind: "trigger", name: "Monday morning" },
    { id: "n_3", kind: "agent", name: "Draft the digest", agent: "writer" },
  ],
  edges: [{ from: "start", to: "n_3" }],
};

/** A client whose run POST is whatever the test hands it. */
function fakeClient(post: () => Promise<unknown>): OpenCompanyClient {
  return {
    scopeFor: (company: string | null) => `/api/v1/${company ?? "company"}`,
    get: async (path: string) => {
      if (path.endsWith("/workflows")) return [{ id: GRAPH.id, name: GRAPH.name }];
      if (path.includes("/workflows/runs")) return { runs: [], hasMore: false };
      return GRAPH;
    },
    post,
    // Issue #1845: the week-1 nudge banner polls this on mount; an empty
    // feed keeps it a no-op for every test in this file, which is not about
    // the nudge.
    notifications: async () => ({ notifications: [], unread: 0 }),
    markNotificationsRead: async () => ({ unread: 0 }),
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

function button(label: string): HTMLButtonElement {
  const found = Array.from(container.querySelectorAll("button")).find(
    (b) => b.textContent?.trim() === label,
  );
  if (!found) throw new Error(`no “${label}” button in:\n${container.innerHTML}`);
  return found as HTMLButtonElement;
}

async function clickRun() {
  await act(async () => {
    button("Run").click();
  });
}

beforeEach(async () => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT =
    true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  vi.clearAllMocks();
});

afterEach(async () => {
  await act(async () => {
    root.unmount();
  });
  container.remove();
});

/**
 * Mount the view **on the workflow's detail page**.
 *
 * `sub` names the graph, which is what puts the canvas and its Run / History
 * controls on screen (issue #1110). It used to be omitted: the view auto-selected
 * the first — and only — row of the list, so a bare mount landed inside the
 * workflow. It lands on the index now, where a per-workflow Run button
 * deliberately does not exist, so the deep link is how these tests reach the
 * surface they are about. Nothing else here changed, and none of these tests is
 * about how the workflow got opened.
 */
async function mount(post: () => Promise<unknown>) {
  await act(async () => {
    root.render(
      createElement(WorkflowsView, {
        client: fakeClient(post),
        company: "acme",
        sub: GRAPH.id,
      }),
    );
  });
}

describe("a rejected run POST leaves run detail on screen", () => {
  it("raises a failure drawer carrying the host's message and code", async () => {
    await mount(async () => {
      throw new ApiError(500, "engine_failed", "the writer agent has no model", true);
    });
    await clickRun();

    const panel = container.querySelector('[data-testid="workflow-run-failure"]');
    expect(panel).not.toBeNull();
    expect(
      container.querySelector('[data-testid="workflow-run-failure-message"]')?.textContent,
    ).toContain("the writer agent has no model");
    // The structured code, shown only because the host's own envelope carried it.
    expect(
      container.querySelector('[data-testid="workflow-run-failure-code"]')?.textContent,
    ).toContain("engine_failed");
    // Still a toast — but it is now the notification, not the record.
    expect(toasts.error).toHaveBeenCalled();
  });

  it("opens the History drawer, where the journaled failure row lands", async () => {
    // Closed by default (`historyOpen`), and before this nothing ever opened it —
    // which is how a run that WAS journaled still reached the operator as nothing.
    await mount(async () => {
      throw new ApiError(500, "engine_failed", "boom", true);
    });
    expect(container.querySelector('[data-testid="workflow-run-history"]')).toBeNull();

    await clickRun();
    expect(container.querySelector('[data-testid="workflow-run-history"]')).not.toBeNull();
  });

  it("also opens it when the connection dropped mid-run", async () => {
    // The other half of the issue: this path's toast points at History ("the
    // outcome lands in History") while History was shut.
    await mount(async () => {
      throw new ApiError(0, "network", "connection lost", false);
    });
    await clickRun();
    expect(container.querySelector('[data-testid="workflow-run-history"]')).not.toBeNull();
  });

  it("acknowledges the dispatch itself, before the host has answered", async () => {
    let release: (() => void) | null = null;
    await mount(
      () =>
        new Promise((_, reject) => {
          release = () => reject(new ApiError(500, "engine_failed", "boom", true));
        }),
    );
    // With the drawer open, the run is a row from the moment it is dispatched
    // rather than from whenever the host journals it — which on the synchronous
    // path is the end of the run, i.e. all of it.
    await act(async () => {
      button("History").click();
    });
    expect(container.querySelectorAll('[data-testid="workflow-run-row"]')).toHaveLength(0);

    await clickRun();
    // The click is answered by name, not only by a button spinner and a canvas.
    expect(toasts.info).toHaveBeenCalledWith(expect.stringContaining("Weekly digest"));
    const rows = container.querySelectorAll('[data-testid="workflow-run-row"]');
    expect(rows).toHaveLength(1);
    expect(rows[0].textContent).toContain("running");
    // Counting from the dispatch, so the wait has a number on it.
    expect(
      container.querySelector('[data-testid="workflow-run-duration"]')?.textContent,
    ).toContain("running for");
    await act(async () => {
      release?.();
    });
  });
});

describe("dispatching a run clears the previous run's detail", () => {
  it("drops the result drawer so a stale run is never shown as the new one's", async () => {
    // Run #1 succeeds and leaves its drawer up; run #2 fails. Before this, that
    // drawer stayed — with run #1's output and "Requested:" line — and was the
    // only run detail on screen, so it read as run #2's.
    let attempt = 0;
    await mount(async () => {
      attempt += 1;
      if (attempt === 1) {
        return { runId: "r1", output: { n_3: "the digest" }, pendingApprovals: [] };
      }
      throw new ApiError(500, "engine_failed", "boom", true);
    });

    await clickRun();
    expect(container.querySelector('[data-testid="workflow-run-result"]')).not.toBeNull();

    await clickRun();
    expect(container.querySelector('[data-testid="workflow-run-result"]')).toBeNull();
    expect(container.querySelector('[data-testid="workflow-run-failure"]')).not.toBeNull();
  });
});

describe("the history row names the node the operator named", () => {
  it("leads with the actionable failure at the node's display name and keeps raw details", async () => {
    // The engine's trail is keyed by node id, and this line printed it verbatim
    // while the run drawer's own timeline — and the canvas, and the overlay
    // banner — all said “Draft the digest”.
    const failed: WorkflowRunOutcome = {
      seq: 7,
      atMillis: Date.now(),
      startedAtMillis: Date.now() - 12_400,
      workflowId: GRAPH.id,
      scheduled: false,
      runId: "r9",
      deliveries: [],
      pendingApprovals: [],
      error:
        "harness error: capability error: agent: the writer agent has no model",
      nodes: [
        { nodeId: "start", status: "ok", elapsedMs: 1 },
        { nodeId: "n_3", status: "error", elapsedMs: 12_000 },
      ],
    };
    await act(async () => {
      root.render(
        createElement(RunHistoryPanel, {
          runs: [failed],
          graph: GRAPH,
          workflowName: GRAPH.name,
          onClose: () => {},
          selectedRunSeq: null,
          onSelectRun: () => {},
        }),
      );
    });
    const row = container.querySelector('[data-testid="workflow-run-row"]');
    expect(row?.textContent).toContain(
      "This run failed at “Draft the digest”: the writer agent has no model",
    );
    expect(row?.textContent).not.toContain("This run failed at “n_3”");
    const details = row?.querySelector("details");
    expect(details?.open).toBe(false);
    expect(details?.textContent).toContain(
      "harness error: capability error: agent: the writer agent has no model",
    );
    // …and says how long it took, which nothing on this surface did before.
    expect(
      container.querySelector('[data-testid="workflow-run-duration"]')?.textContent,
    ).toContain("12.4s");
  });
});

describe("what the failure panel is built from", () => {
  const CTX = {
    startedAtMillis: 1_000,
    atMillis: 3_400,
    request: "",
    dryRun: false,
    sawRunStart: false,
  };

  it("keeps the host's code only when the host's own envelope carried it", () => {
    const fromHost = runFailureFrom(new ApiError(409, "engine_failed", "no", true), CTX);
    expect(fromHost.code).toBe("engine_failed");
    // A code synthesised off a status line names the status, not the fault, and
    // printing it beside the message dresses a proxy timeout up as a verdict.
    const fromProxy = runFailureFrom(new ApiError(502, "bad_gateway", "no", false), CTX);
    expect(fromProxy.code).toBeUndefined();
    expect(fromProxy.fromHost).toBe(false);
  });

  it("still has a sentence for a thrown non-Error, or an Error with no message", () => {
    // The whole point of the panel is that it is never blank — a panel with
    // nothing in it is the vanishing toast again, only quieter.
    expect(runFailureFrom("kaboom", CTX).message).toBe("kaboom");
    expect(runFailureFrom(new Error(""), CTX).message).not.toBe("");
    expect(runFailureFrom(new ApiError(500, "x", "", true), CTX).message).toContain("500");
  });
});

describe("failure panel follow-up controls", () => {
  it("opens the durable row, its failed step, and the existing copilot path", async () => {
    const openHistory = vi.fn();
    const showStep = vi.fn();
    const fixWithCopilot = vi.fn();
    await act(async () => {
      root.render(
        createElement(RunFailurePanel, {
          failure: {
            message: "the writer agent has no model",
            fromHost: true,
            runId: "run-9",
            sawRunStart: true,
            startedAtMillis: 1_000,
            atMillis: 3_400,
            request: "",
            dryRun: false,
          },
          onClose: () => {},
          onOpenHistory: openHistory,
          failedStepName: "Draft the digest",
          onShowFailedStep: showStep,
          onFixWithCopilot: fixWithCopilot,
        }),
      );
    });

    await act(async () => {
      button("Open Run history").click();
      button("Show “Draft the digest”").click();
      button("Fix with copilot").click();
    });

    expect(openHistory).toHaveBeenCalledOnce();
    expect(showStep).toHaveBeenCalledOnce();
    expect(fixWithCopilot).toHaveBeenCalledOnce();
  });
});

describe("failure panel error details", () => {
  it("shows the actionable leaf while preserving the host diagnostic verbatim", async () => {
    const raw =
      "harness error: capability error: http_request: Blocked local/private host: 127.0.0.1";
    await act(async () => {
      root.render(
        createElement(RunFailurePanel, {
          failure: {
            message: raw,
            fromHost: true,
            sawRunStart: false,
            startedAtMillis: 1_000,
            atMillis: 3_400,
            request: "",
            dryRun: false,
          },
          onClose: () => {},
        }),
      );
    });

    expect(
      container.querySelector('[data-testid="workflow-run-failure-message"]')?.textContent,
    ).toBe("Blocked local/private host: 127.0.0.1");
    const details = container.querySelector("details");
    expect(details?.open).toBe(false);
    expect(details?.textContent).toContain(raw);
  });
});

describe("how long a run took", () => {
  it("measures a settled run from its start to its finish", () => {
    const run = {
      startedAtMillis: 1_000,
      atMillis: 13_400,
    } as WorkflowRunOutcome;
    expect(formatDuration(runDuration(run, 99_999)!)).toBe("12.4s");
  });

  it("measures a running run against now, and reports nothing without a start", () => {
    const running = { startedAtMillis: 1_000, atMillis: 1_000, running: true } as WorkflowRunOutcome;
    expect(runDuration(running, 6_000)).toBe(5_000);
    // Two different clocks: a live row read a moment early comes out negative,
    // and "-1s" is worse than no number at all.
    expect(runDuration(running, 0)).toBeNull();
    // A row journaled before #371 records only its finish.
    expect(runDuration({ atMillis: 10 } as WorkflowRunOutcome, 20)).toBeNull();
  });

  it("never renders a sixtieth second", () => {
    expect(formatDuration(840)).toBe("840ms");
    expect(formatDuration(179_800)).toBe("3m 00s");
    expect(formatDuration(187_000)).toBe("3m 07s");
  });

  it("rolls long durations into hours and days", () => {
    expect(formatDuration(3_967_000)).toBe("1h 06m 07s");
    expect(formatDuration((28_741 * 60 + 19) * 1_000)).toBe("19d 23h 01m 19s");
  });
});
