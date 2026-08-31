// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { WorkflowGraph, WorkflowSummary } from "@/api/workflows";

/**
 * A deep link to a workflow authored elsewhere resolves, and stops toasting a
 * false "no workflow" (issue #1045).
 *
 * The reported symptom is an absence and a wrong toast — neither of which the
 * pure helpers can pin — so this file renders the view, the way
 * `workflow-run-failure.test.ts` earns its exception to the pure-function rule.
 *
 * The deep-link resolver had two coupled defects:
 *
 *  (a) it fired the "no workflow" toast against whatever the picker happened to
 *      hold the instant the id was first seen, without re-reading the list — so
 *      a link followed to a just-authored graph, whose create frame had not yet
 *      reached this tab, got a false toast; and
 *  (b) it recorded the id as "applied" BEFORE the presence check, so an absent
 *      id was frozen — a later refresh that finally carried the graph re-ran the
 *      effect and early-returned instead of selecting it, so the false toast
 *      stood forever and the canvas never resolved.
 *
 * The fix re-reads the list before deciding, and marks an id applied only once
 * it has resolved (selected, or confirmed missing after the fresh read).
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

// React Flow measures its container on mount; jsdom has no layout and no
// `ResizeObserver`, so these stubs are what let the view render at all. None is
// under test. (Same three as `workflow-run-failure.test.ts`.)
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

const PROBE = "qa_probe_wf3";
const NO_WORKFLOW = "This company has no workflow";

/** Seven ordinary workflows — the list a link's target is NOT yet part of. */
const STALE: WorkflowSummary[] = Array.from({ length: 7 }, (_, i) => ({
  id: `wf${i + 1}`,
  name: `Workflow ${i + 1}`,
}));
/** The same list once the graph authored elsewhere has landed in it. */
const FULL: WorkflowSummary[] = [...STALE, { id: PROBE, name: "QA probe" }];

function graphFor(id: string): WorkflowGraph {
  return {
    id,
    name: id,
    version: null,
    nodes: [{ id: "start", kind: "trigger", name: "Start" }],
    edges: [],
  };
}

/**
 * A client whose `GET …/workflows` answer is supplied by the test, and which
 * records every `getWorkflow` id so a test can assert a selection actually
 * landed (the selection drives the graph fetch).
 */
function makeClient(
  listFor: () => Promise<WorkflowSummary[]>,
): { client: OpenCompanyClient; graphGets: string[] } {
  const graphGets: string[] = [];
  const client = {
    scopeFor: (company: string | null) => `/api/v1/${company ?? "company"}`,
    get: async (path: string) => {
      if (path.endsWith("/workflows")) return listFor();
      if (path.includes("/workflows/tool-slugs")) return { slugs: [], unwired: [] };
      if (path.includes("/workflows/wired-channels")) return { channels: [] };
      if (path.includes("/workflows/runs")) return { runs: [], hasMore: false };
      const m = path.match(/\/workflows\/([^/?]+)$/);
      if (m) {
        const id = decodeURIComponent(m[1]);
        graphGets.push(id);
        return graphFor(id);
      }
      return null;
    },
    post: async () => ({}),
    // Issue #1845: the week-1 nudge banner polls this on mount; an empty
    // feed keeps it a no-op for every test in this file, which is not about
    // the nudge.
    notifications: async () => ({ notifications: [], unread: 0 }),
    markNotificationsRead: async () => ({ unread: 0 }),
  } as unknown as OpenCompanyClient;
  return { client, graphGets };
}

let container: HTMLDivElement;
let root: Root;

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

function toastedNoWorkflow(): boolean {
  return toasts.error.mock.calls.some((args) =>
    String(args[0] ?? "").includes(NO_WORKFLOW),
  );
}

describe("a deep link to a workflow authored elsewhere resolves", () => {
  it(
    "re-reads the list and selects the id instead of toasting (decisive)",
    async () => {
      // First read (the picker's own load) predates the graph; the re-read the
      // resolver now does before deciding carries it. On main the resolver never
      // re-reads: it toasts against the stale list and freezes the id.
      let call = 0;
      const lists = [STALE, FULL];
      const { client, graphGets } = makeClient(() =>
        Promise.resolve(lists[Math.min(call++, lists.length - 1)]),
      );

      await act(async () => {
        root.render(
          createElement(WorkflowsView, { client, company: "acme", sub: PROBE }),
        );
      });

      // The selection landed on the requested graph — the fetch that only fires
      // for the selected workflow was asked for it.
      expect(graphGets).toContain(PROBE);
      // …and no false "no workflow" toast was raised.
      expect(toastedNoWorkflow()).toBe(false);
    },
  );

  it(
    "does not freeze an absent id: a later list refresh still selects it (defect b)",
    async () => {
      // The re-read is held pending, so resolution can only come from the tick
      // that refreshes the picker into carrying the id. That path is exactly the
      // one defect (b) broke: on main the id is marked applied on the absent
      // pass, so this refresh re-runs the effect and early-returns.
      let phase: "mount" | "live" = "mount";
      let mountCall = 0;
      let tail: WorkflowSummary[] = STALE;
      const { client, graphGets } = makeClient(() => {
        if (phase === "mount") {
          if (mountCall++ === 0) return Promise.resolve(STALE);
          // Hang the resolver's re-read so only the tick can resolve the id.
          return new Promise<WorkflowSummary[]>(() => {});
        }
        return Promise.resolve(tail);
      });

      await act(async () => {
        root.render(
          createElement(WorkflowsView, {
            client,
            company: "acme",
            sub: PROBE,
            listEventTick: 0,
          }),
        );
      });
      // Not selected yet: the picker still holds the stale list.
      expect(graphGets).not.toContain(PROBE);

      // The host's create frame lands: the shell bumps the tick and the picker
      // re-reads into a list that now carries the graph.
      phase = "live";
      tail = FULL;
      await act(async () => {
        root.render(
          createElement(WorkflowsView, {
            client,
            company: "acme",
            sub: PROBE,
            listEventTick: 1,
          }),
        );
      });

      expect(graphGets).toContain(PROBE);
      expect(toastedNoWorkflow()).toBe(false);
    },
  );

  it(
    "still toasts exactly once for an id absent from both reads (no over-suppression)",
    async () => {
      // The genuinely-missing case: the link names a workflow this company does
      // not have and never gets. Re-reading must not swallow the report — the
      // operator followed a dead link and has to be told, once.
      const { client, graphGets } = makeClient(() => Promise.resolve(STALE));

      await act(async () => {
        root.render(
          createElement(WorkflowsView, { client, company: "acme", sub: PROBE }),
        );
      });

      expect(graphGets).not.toContain(PROBE);
      const noWorkflowToasts = toasts.error.mock.calls.filter((args) =>
        String(args[0] ?? "").includes(NO_WORKFLOW),
      );
      expect(noWorkflowToasts).toHaveLength(1);
    },
  );
});
