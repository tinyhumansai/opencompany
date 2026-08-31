// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { WorkflowGraph, WorkflowRunOutcome } from "@/api/workflows";

/**
 * The node a run is executing RIGHT NOW, on a console that did not watch it
 * start (issue #1010).
 *
 * Three separate holes met in one symptom — a graph that looks dead while it is
 * working:
 *
 *  1. the history fold carried only the *finish* bracket, so `statesFromRun`
 *     could never produce `"running"` for a console reading the journal;
 *  2. `liveRanRef` grew forever while the frame window evicted, so the
 *     history seed was withheld from a fold that had nothing left to fold —
 *     switching workflow away and back mid-run blanked the canvas; and
 *  3. the window was never emptied on a company switch, and the folds match on
 *     `workflowId`/`runId` alone, so one company's run painted another's
 *     identically-named workflow.
 *
 * The first two are asserted here. The third is a three-line effect on the
 * shell's own state and is covered by argument rather than by a test — see the
 * PR description.
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

// The canvas is a React Flow instance and React Flow measures its container on
// mount. jsdom has no layout and no `ResizeObserver`, so these stubs are what
// let the view render at all — none of them is under test. Same three the
// `task-blocked-card` / `workflow-run-failure` suites install, for the same
// reason.
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
const { statesFromRun, windowHasRunStart } = await import("@/views/workflows/graph");
type Ev = import("@/hooks/use-events").CompanyStreamEvent;

const GRAPH: WorkflowGraph = {
  id: "digest",
  name: "Weekly digest",
  // Editable-graph token is irrelevant to this fixture's fold/paint behavior;
  // `version` just has to be present now that it is required (issue #1013).
  version: null,
  nodes: [
    { id: "start", kind: "trigger", name: "Monday morning" },
    { id: "collect", kind: "agent", name: "Collect", agent: "analyst" },
    { id: "draft", kind: "agent", name: "Draft the digest", agent: "writer" },
  ],
  edges: [
    { from: "start", to: "collect" },
    { from: "collect", to: "draft" },
  ],
};

/** A run in flight: `collect` finished, `draft` started and has not. */
const RUNNING_ROW: WorkflowRunOutcome = {
  seq: 7,
  atMillis: 1_000,
  startedAtMillis: 900,
  workflowId: "digest",
  scheduled: true,
  runId: "run-live",
  deliveries: [],
  pendingApprovals: [],
  running: true,
  startedNodes: ["collect", "draft"],
  nodes: [{ nodeId: "collect", status: "ok", elapsedMs: 12 }],
};

// ── The fold's new half ──────────────────────────────────────────────────────

describe("statesFromRun reads the started bracket", () => {
  it("marks a started-but-unfinished node running", () => {
    // The issue in one assertion: before `startedNodes` existed on the wire,
    // `draft` had no entry at all and the graph showed a hole where the work
    // was happening.
    expect(statesFromRun(RUNNING_ROW)).toEqual({
      collect: "ok",
      draft: "running",
    });
  });

  it("lets the finish win over the start for the same node", () => {
    // The two lists are ordered independently — one by start, one by finish —
    // so the reading must not depend on how they interleave.
    const states = statesFromRun({
      ...RUNNING_ROW,
      startedNodes: ["draft", "collect"],
      nodes: [
        { nodeId: "collect", status: "ok", elapsedMs: 12 },
        { nodeId: "draft", status: "error", elapsedMs: 3 },
      ],
    });
    expect(states).toEqual({ collect: "ok", draft: "error" });
  });

  it("never paints a SETTLED run's unfinished node as running", () => {
    // The receipt outlives the run on purpose — `draft` is the node this run
    // was standing on when it was cancelled, and nothing else records that. But
    // a settled run has nothing executing, so an overlay built from it must not
    // show a spinner no frame can ever clear. This is `settle()`'s rule, on the
    // one surface that has no fold to apply it.
    const states = statesFromRun({
      ...RUNNING_ROW,
      running: false,
      error: "cancelled",
    });
    expect(states).toEqual({ collect: "ok" });
    expect(states.draft).toBeUndefined();
  });

  it("reads a host that sends no started trail exactly as before", () => {
    // A run journaled before #382, or a host predating #1010: absent means "no
    // start trail", never "nothing started".
    const { startedNodes: _dropped, ...old } = RUNNING_ROW;
    expect(statesFromRun(old)).toEqual({ collect: "ok" });
  });
});

// ── The seed's guard ─────────────────────────────────────────────────────────

const startFrame = (runId: string): Ev => ({
  type: "workflow_run_started",
  seq: 1,
  atMillis: 1,
  workflowId: "digest",
  runId,
  scheduled: true,
  startedBy: "schedule",
});
const nodeStartedFrame = (runId: string, nodeId: string): Ev => ({
  type: "workflow_node_started",
  seq: 2,
  atMillis: 2,
  workflowId: "digest",
  runId,
  nodeId,
});

describe("windowHasRunStart asks the window, not a growing set", () => {
  it("is true only for a start frame of that very run", () => {
    expect(windowHasRunStart([startFrame("run-live")], "run-live")).toBe(true);
    expect(windowHasRunStart([startFrame("other")], "run-live")).toBe(false);
    expect(windowHasRunStart([], "run-live")).toBe(false);
  });

  it("is false when only node frames survived the eviction", () => {
    // The case the old ref got wrong. `foldLiveRun` folds from the window ONLY
    // when it finds the run's start; with node frames alone it needs the
    // history seed, and `foldFromHistory` then applies those node frames on top
    // — strictly better than either half alone.
    expect(
      windowHasRunStart(
        [nodeStartedFrame("run-live", "collect"), nodeStartedFrame("run-live", "draft")],
        "run-live",
      ),
    ).toBe(false);
  });
});

// ── The lifetime, on the mounted view ────────────────────────────────────────

function fakeClient(): OpenCompanyClient {
  return {
    scopeFor: (company: string | null) => `/api/v1/${company ?? "company"}`,
    get: async (path: string) => {
      if (path.endsWith("/workflows")) return [{ id: GRAPH.id, name: GRAPH.name }];
      if (path.includes("/workflows/runs")) return { runs: [RUNNING_ROW], hasMore: false };
      return GRAPH;
    },
    post: async () => ({}),
    // Issue #1845: the week-1 nudge banner polls this on mount; an empty
    // feed keeps it a no-op for every test in this file, which is not about
    // the nudge.
    notifications: async () => ({ notifications: [], unread: 0 }),
    markNotificationsRead: async () => ({ unread: 0 }),
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

/** The canvas node for `id`, read through the attribute the node paints its
 * run state onto. */
function runStateOf(id: string): string | null {
  const nodes = Array.from(container.querySelectorAll("[data-run-state]"));
  for (const node of nodes) {
    if (node.closest(`[data-id="${id}"]`)) return node.getAttribute("data-run-state");
  }
  return null;
}

/**
 * Render the view **on the workflow's detail page**, where the canvas is.
 *
 * `sub` names the graph (issue #1110). It used to be omitted: the view
 * auto-selected the first — and only — row of the list, so a bare render landed
 * on the canvas. `#/workflows` is the index now and has no canvas at all, so
 * the deep link is how these tests reach one. The switch-away-and-back case
 * still exercises exactly what it did: the clear effect keys on `company`, and
 * that is the argument being changed.
 */
async function render(runEvents: Ev[], company = "acme") {
  await act(async () => {
    root.render(
      createElement(WorkflowsView, {
        client: fakeClient(),
        company,
        sub: GRAPH.id,
        runEvents,
      }),
    );
  });
}

beforeEach(() => {
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

describe("a console joining a run mid-flight keeps painting it", () => {
  it("shows the executing node from the history alone", async () => {
    // No frames at all — a reload, or a cron fire nobody's tab was open for.
    // The whole run used to be blank; now the journal names both brackets.
    await render([]);
    expect(runStateOf("collect")).toBe("ok");
    expect(runStateOf("draft")).toBe("running");
  });

  it("keeps painting it after a switch away and back evicts the start frame", async () => {
    // **The regression, in the order that produced it.**
    //
    // Watching live records the run in `liveRanRef` — a set that only ever
    // grew — and in `adoptedFromHistoryRef`. The switch cleared the SECOND ref
    // only. So on the way back both clauses of the old guard held, the seed
    // was withheld as "already watched live", and if the rolling 300-frame
    // window had meanwhile evicted the run's start there was nothing left to
    // fold from: `foldLiveRun` returned null and the canvas blanked on a run
    // that was still going. The two refs answer the same question from
    // opposite sides, so they must have the same lifetime.
    await render([startFrame("run-live"), nodeStartedFrame("run-live", "draft")]);
    expect(runStateOf("draft")).toBe("running");

    // Away — the clear effect runs on `company` exactly as it does on
    // `selectedId`, and drops the adoption.
    await render([startFrame("run-live"), nodeStartedFrame("run-live", "draft")], "beta");

    // …and back, to a window that has since lost the start frame. The run is
    // still in flight and the host still says so.
    await render([nodeStartedFrame("run-live", "draft")]);
    expect(runStateOf("draft")).toBe("running");
    expect(runStateOf("collect")).toBe("ok");
  });
});
