// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { WorkflowGraph, WorkflowRunOutcome, WorkflowRunsPage } from "@/api/workflows";

/**
 * A "Load older" page that answers a question nobody is asking any more must be
 * dropped, not appended (issue #1012 follow-up).
 *
 * `loadOlderRuns` awaits a page and then unconditionally does
 * `setRuns(prev => [...prev, ...older])`. It has no staleness guard at all —
 * not even the workflow id — and a `useCallback` has no cleanup, so nothing
 * invalidates a request already in flight. Two ways that lands the wrong rows:
 *
 * * **Across companies.** `create_company_workflow` checks workflow-id
 *   uniqueness only *within* the requesting company (`src/company/workflow_create.rs`),
 *   so two companies genuinely share an id — a seed workflow shipped identically
 *   to both is the ordinary case. An older page started against company A and
 *   held in flight while the operator switches to B appends A's runs onto B's
 *   history, and A's cursor overwrites B's pagination state.
 * * **Within one company.** The first-page effect replaces `runs` wholesale on
 *   a refresh (a run finished, the 2s poll ticked, the client was swapped).
 *   Neither the company nor the workflow id changes, so identity fields cannot
 *   see it — the second case below fails against a guard built from those alone.
 *
 * These render the view, the way `workflow-run-failure.test.ts` earns its
 * exception to the pure-function rule: the claim is about what ends up in the
 * DOM after two overlapping fetches race, which no pure helper can pin.
 */

vi.mock("sonner", () => {
  const noop = vi.fn();
  const toast = Object.assign(noop, { success: noop, error: noop, warning: noop, info: noop });
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

/** The id both companies' workflow happen to share — e.g. an identical seed. */
const WF_ID = "shared-wf";

const GRAPH: WorkflowGraph = {
  id: WF_ID,
  name: "Shared workflow",
  version: null,
  nodes: [{ id: "start", kind: "trigger", name: "Start" }],
  edges: [],
};

function run(seq: number, runId: string): WorkflowRunOutcome {
  return {
    seq,
    atMillis: seq * 1_000,
    workflowId: WF_ID,
    scheduled: false,
    runId,
    deliveries: [],
    pendingApprovals: [],
  };
}

/** A resolver the test controls, so a fetch can be held open across renders. */
function deferred<T>(): { promise: Promise<T>; resolve: (value: T) => void } {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((res) => {
    resolve = res;
  });
  return { promise, resolve };
}

const EMPTY: WorkflowRunsPage = { runs: [], hasMore: false };

/**
 * A client whose run-history reads are scripted per company slug.
 *
 * `first` answers the newest-page fetch (no `before_seq`); `older` is the single
 * held-open "Load older" response for `acme`, which the test resolves when it
 * chooses. Everything else answers the inert shape the view needs to render.
 */
function makeClient(script: {
  first: Record<string, WorkflowRunsPage>;
  older?: Promise<WorkflowRunsPage>;
}): OpenCompanyClient {
  return {
    scopeFor: (company: string | null) => `/api/v1/${company ?? "company"}`,
    get: async (path: string) => {
      if (path.endsWith("/workflows")) return [{ id: WF_ID, name: GRAPH.name }];
      if (path.includes("/workflows/tool-slugs")) return { slugs: [], unwired: [] };
      if (path.includes("/workflows/wired-channels")) return { channels: [] };
      if (path.includes("/workflows/runs")) {
        const url = new URL(path, "http://test");
        const slug = path.split("/")[3];
        const workflow = url.searchParams.get("workflow");
        const beforeSeq = url.searchParams.get("before_seq");
        // The company-wide index fetch is inert here — the view stays on the
        // detail page throughout.
        if (workflow !== WF_ID) return EMPTY;
        if (beforeSeq !== null) {
          return slug === "acme" && script.older ? script.older : EMPTY;
        }
        return script.first[slug] ?? EMPTY;
      }
      if (/\/workflows\/[^/?]+$/.test(path)) return GRAPH;
      return null;
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

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
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

function rows(): NodeListOf<Element> {
  return container.querySelectorAll('[data-testid="workflow-run-row"]');
}

function loadOlderButton(): HTMLButtonElement | null {
  return container.querySelector('[data-testid="workflow-run-load-older"]');
}

async function openHistory() {
  await act(async () => {
    (container.querySelector('[data-testid="workflow-history-toggle"]') as HTMLButtonElement).click();
  });
}

describe("run history pagination races", () => {
  it("drops a superseded company's stale older page instead of appending it", async () => {
    const older = deferred<WorkflowRunsPage>();
    const client = makeClient({
      first: {
        acme: { runs: [run(20, "acme-r2"), run(10, "acme-r1")], hasMore: true, nextBeforeSeq: 10 },
        beta: { runs: [run(5, "beta-r1")], hasMore: false },
      },
      older: older.promise,
    });

    await act(async () => {
      root.render(createElement(WorkflowsView, { client, company: "acme", sub: WF_ID }));
    });

    await openHistory();
    expect(rows()).toHaveLength(2);
    expect(loadOlderButton()).not.toBeNull();

    // Fire "Load older" for acme. Nothing resolves it until the test does.
    await act(async () => {
      loadOlderButton()!.click();
    });

    // The operator switches to `beta`, which happens to have a workflow of the
    // SAME id already selected (`sub` unchanged — `WorkflowsView` never resets
    // `selectedId` on a company prop change, and the deep-link-apply effect
    // will not reselect an id it already applied). Beta's own first page
    // answers immediately: one run, nothing older.
    await act(async () => {
      root.render(createElement(WorkflowsView, { client, company: "beta", sub: WF_ID }));
    });
    expect(rows()).toHaveLength(1);
    expect(loadOlderButton()).toBeNull();

    // Now let acme's stale older-page answer land.
    await act(async () => {
      older.resolve({ runs: [run(1, "acme-older")], hasMore: false });
    });

    // Pre-fix: appended unconditionally — 1 + 1 = 2 rows, one of them acme's,
    // and the stale response's cursor would overwrite beta's pagination state
    // too. Post-fix: the company guard rejects it and beta's page is untouched.
    expect(rows()).toHaveLength(1);
  });

  it("drops an older page superseded by a refresh of the SAME company's history", async () => {
    const older = deferred<WorkflowRunsPage>();
    const before = makeClient({
      first: {
        acme: { runs: [run(20, "acme-r2"), run(10, "acme-r1")], hasMore: true, nextBeforeSeq: 10 },
      },
      older: older.promise,
    });

    await act(async () => {
      root.render(createElement(WorkflowsView, { client: before, company: "acme", sub: WF_ID }));
    });

    await openHistory();
    expect(rows()).toHaveLength(2);

    await act(async () => {
      loadOlderButton()!.click();
    });

    // The history refreshes while that page is in flight — same company, same
    // workflow, so nothing about the request's IDENTITY has changed. Modelled
    // by swapping the client, which is one of the first-page effect's own
    // dependencies, exactly as a `runsTick`/`runEventTick` bump would be.
    // The fresh page says the history is complete.
    const after = makeClient({
      first: {
        acme: { runs: [run(20, "acme-r2"), run(10, "acme-r1")], hasMore: false },
      },
    });
    await act(async () => {
      root.render(createElement(WorkflowsView, { client: after, company: "acme", sub: WF_ID }));
    });
    expect(rows()).toHaveLength(2);
    expect(loadOlderButton()).toBeNull();

    await act(async () => {
      older.resolve({ runs: [run(1, "acme-older")], hasMore: false });
    });

    // A guard built from company + workflow alone passes here: both still
    // match. Only the generation counter can tell that the list this answer
    // was for has already been replaced.
    expect(rows()).toHaveLength(2);
  });
});
