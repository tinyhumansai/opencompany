// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { ApiError } from "@/api/types";
import type {
  WorkflowFixFromRun,
  WorkflowGraph,
  WorkflowRunOutcome,
  WorkflowRunsPage,
} from "@/api/workflows";

/**
 * Issue #1704: what a workflow or company switch has to leave behind.
 *
 * `WorkflowsView` has one cleanup effect on `[selectedId, company]`, and every
 * fix in its history has been the same shape — a piece of per-workflow state
 * that was not listed there, rendering under the NEXT workflow as though it
 * belonged to it (`result` and `runRefusal` in #528, `runFailure` in #1007,
 * `adoptedFromHistoryRef` in #863, `liveRanRef` in #1010, the run-input draft
 * in #1204). These are the four still missing, and two of them are worse than
 * merely stale:
 *
 *  * **`fixingRunSeq` / `fixReason` are keyed by run `seq`.** `seq` is a
 *    journal position allocated per COMPANY, not per workflow, so a leftover
 *    value does not go unread — it lands on whichever run of the newly selected
 *    workflow happens to share that number. And because `RunHistoryPanel`
 *    disables EVERY row's Fix button while `fixingRunSeq` is set (one fix at a
 *    time), a leaked one takes the affordance away from a workflow no fix was
 *    ever requested for.
 *
 *  * **Clearing them is only half the fix.** The switch happens while the fix
 *    request is still in flight, so the clear runs first and the reply writes
 *    the state straight back. The second test below is the one that pins that:
 *    it fails against a cleanup effect that clears both fields and nothing else.
 *
 *  * **`conflict`** is the last of the persistent banners to outlive a switch.
 *    A successful graph read clears it — but a graph read that FAILS does not,
 *    which is exactly when the operator is left staring at it.
 *
 *  * **`error`** renders outside the `detailOpen` gate, so a graph-load failure
 *    follows the operator all the way back to the index.
 *
 * These render the view, the way `workflow-run-failure.test.ts` and
 * `workflow-history-cross-company-race.test.ts` earn their exception to the
 * pure-function rule: the claim is about what is in the DOM after a switch,
 * which no pure helper can pin.
 */

vi.mock("sonner", () => {
  const noop = vi.fn();
  const toast = Object.assign(noop, {
    success: noop,
    error: noop,
    warning: noop,
    info: noop,
    message: noop,
  });
  return { toast };
});

vi.mock("next-themes", () => ({ useTheme: () => ({ resolvedTheme: "light" }) }));

// React Flow measures its container on mount; jsdom has no layout and no
// `ResizeObserver`, so these stubs are what let the view render at all. None is
// under test. (Same three as `workflow-history-cross-company-race.test.ts`.)
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

const WF_A = "wf-a";
const WF_B = "wf-b";

/** The seq both workflows' failed runs share — the collision the bug needs. */
const SHARED_SEQ = 20;

function graph(id: string, name: string): WorkflowGraph {
  return {
    id,
    name,
    version: "v1",
    nodes: [{ id: "start", kind: "trigger", name: "Start" }],
    edges: [],
  };
}

const GRAPHS: Record<string, WorkflowGraph> = {
  [WF_A]: graph(WF_A, "Workflow A"),
  [WF_B]: graph(WF_B, "Workflow B"),
};

/** A failed run — `error` is what puts the Fix affordance on the row.
 *
 * `nodes` names the node the failure traces to (matching each graph's own
 * `start` trigger, the only node either defines). Codex review on #1821
 * (eighth pass) made the Fix affordance conditional on `failedNodeOf(run)`
 * finding one — a run whose `error` names no node (a host restart, an
 * uncompilable graph) offers no fix the copilot could make, so the button no
 * longer renders for it. This fixture is about the button leaking across a
 * workflow switch, not about that distinction, so it keeps the ordinary,
 * node-attributed shape that earns the button in the first place. */
function failedRun(workflowId: string, seq: number): WorkflowRunOutcome {
  return {
    seq,
    atMillis: seq * 1_000,
    workflowId,
    scheduled: false,
    runId: `${workflowId}-r${seq}`,
    deliveries: [],
    pendingApprovals: [],
    error: `${workflowId} blew up`,
    nodes: [{ nodeId: "start", status: "error", elapsedMs: 1 }],
  };
}

const EMPTY: WorkflowRunsPage = { runs: [], hasMore: false };

/** A resolver the test controls, so a fetch can be held open across renders. */
function deferred<T>(): { promise: Promise<T>; resolve: (value: T) => void } {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((res) => {
    resolve = res;
  });
  return { promise, resolve };
}

function makeClient(script: {
  /** Held-open answer to `POST …/fix-from-run`, if the test drives one. */
  fix?: Promise<WorkflowFixFromRun>;
  /**
   * Held-open answers to SUCCESSIVE `POST …/fix-from-run` calls, in order —
   * what a retry of the same run needs, since the two requests have to be
   * settled independently and out of order.
   */
  fixes?: Promise<WorkflowFixFromRun>[];
  /** Workflow ids whose graph read rejects, so nothing can clear state for us. */
  graphFails?: string[];
  /** Rejection for `DELETE …/workflows/{id}`, if the test drives one. */
  del?: () => Promise<never>;
  /**
   * Reject the workflow-LIST read from this call onwards (1-based), so the
   * first read can land the detail view and a later REFRESH can fail under it —
   * which is the shape the finding is about: a company-wide failure arriving
   * while one workflow is open.
   */
  listFailsAfter?: number;
  /** Narrow that failure to these companies, so another company's list is fine. */
  listFailsFor?: string[];
  /** Hold one company's list read open, so a test can look at the gap it leaves. */
  holdList?: { company: string; gate: Promise<null> };
}): OpenCompanyClient {
  let listReads = 0;
  return {
    scopeFor: (company: string | null) => `/api/v1/${company ?? "company"}`,
    get: async (path: string) => {
      if (path.endsWith("/workflows")) {
        listReads += 1;
        const forCompany = path.split("/")[3] ?? "";
        if (script.holdList && forCompany === script.holdList.company) {
          await script.holdList.gate;
        }
        const late = script.listFailsAfter === undefined || listReads > script.listFailsAfter;
        const named = script.listFailsFor === undefined || script.listFailsFor.includes(forCompany);
        if (script.listFailsAfter !== undefined && late && named) {
          throw new Error("could not load workflows");
        }
        return [
          { id: WF_A, name: GRAPHS[WF_A].name },
          { id: WF_B, name: GRAPHS[WF_B].name },
        ];
      }
      if (path.includes("/workflows/tool-slugs")) return { slugs: [], unwired: [] };
      if (path.includes("/workflows/wired-channels")) return { channels: [] };
      if (path.includes("/workflows/runs")) {
        const url = new URL(path, "http://test");
        const workflow = url.searchParams.get("workflow");
        // The company-wide index fetch is inert here — the view stays on a
        // detail page throughout.
        if (workflow !== WF_A && workflow !== WF_B) return EMPTY;
        return { runs: [failedRun(workflow, SHARED_SEQ)], hasMore: false };
      }
      const detail = /\/workflows\/([^/?]+)$/.exec(path);
      if (detail) {
        const id = detail[1];
        if (script.graphFails?.includes(id)) {
          throw new Error(`could not load ${id}`);
        }
        return GRAPHS[id] ?? null;
      }
      return null;
    },
    post: async (path: string) => {
      if (path.includes("/fix-from-run")) {
        if (script.fixes?.length) return script.fixes.shift()!;
        if (script.fix) return script.fix;
      }
      return {};
    },
    del: async () => {
      if (script.del) return script.del();
      return undefined;
    },
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

/** The dialogs are portaled out of the view's own subtree. */
function inDocument<T extends Element>(testId: string): T | null {
  return document.querySelector<T>(`[data-testid="${testId}"]`);
}

function inView<T extends Element>(testId: string): T | null {
  return container.querySelector<T>(`[data-testid="${testId}"]`);
}

async function show(
  client: OpenCompanyClient,
  company: string,
  sub: string | null,
  listEventTick = 0,
) {
  await act(async () => {
    root.render(createElement(WorkflowsView, { client, company, sub, listEventTick }));
  });
}

async function click(el: Element | null) {
  if (!el) throw new Error("nothing to click");
  await act(async () => {
    (el as HTMLButtonElement).click();
  });
}

async function openHistory() {
  await click(inView("workflow-history-toggle"));
}

function fixButton(): HTMLButtonElement | null {
  return inView<HTMLButtonElement>("workflow-run-fix-with-copilot");
}

describe("WorkflowsView leaves per-workflow state behind on a switch", () => {
  it("clears an in-flight copilot fix when the workflow changes", async () => {
    const fix = deferred<WorkflowFixFromRun>();
    const client = makeClient({ fix: fix.promise });

    await show(client, "acme", WF_A);
    await openHistory();
    await click(fixButton());

    // Workflow A's row is now the one being fixed.
    expect(fixButton()?.textContent).toContain("Fixing…");
    expect(fixButton()?.disabled).toBe(true);

    // The operator switches to workflow B, whose own failed run happens to
    // carry the same journal `seq`.
    await show(client, "acme", WF_B);

    // Pre-fix: `fixingRunSeq` is still 20, so B's unrelated run renders as
    // mid-fix and its Fix button is disabled by a request nobody made for it.
    expect(fixButton()?.textContent).toContain("Fix with copilot");
    expect(fixButton()?.disabled).toBe(false);

    // Let the abandoned request land so the test does not leave it dangling.
    await act(async () => {
      fix.resolve({ automatable: false, reason: "the trigger is misconfigured." });
    });
  });

  it("clears an in-flight copilot fix when the company changes", async () => {
    const fix = deferred<WorkflowFixFromRun>();
    const client = makeClient({ fix: fix.promise });

    await show(client, "acme", WF_A);
    await openHistory();
    await click(fixButton());
    expect(fixButton()?.disabled).toBe(true);

    // Same workflow id in another company — ids are unique only within a
    // company, so an identically seeded workflow is the ordinary case.
    await show(client, "beta", WF_A);

    expect(fixButton()?.textContent).toContain("Fix with copilot");
    expect(fixButton()?.disabled).toBe(false);

    await act(async () => {
      fix.resolve({ automatable: false, reason: "the trigger is misconfigured." });
    });
  });

  it("never lands the verdict for the workflow left behind on the new one", async () => {
    const fix = deferred<WorkflowFixFromRun>();
    const client = makeClient({ fix: fix.promise });

    await show(client, "acme", WF_A);
    await openHistory();
    await click(fixButton());

    // Switch away FIRST — the reply is still in flight, which is the whole
    // point: clearing `fixReason` on the switch cannot help if the request
    // that arrives afterwards writes it straight back.
    await show(client, "acme", WF_B);

    await act(async () => {
      fix.resolve({ automatable: false, reason: "the trigger is misconfigured." });
    });

    // Pre-fix: "The copilot couldn't fix this…" appears under workflow B's
    // run, about a failure of workflow A's.
    expect(inView("workflow-run-fix-not-automatable")).toBeNull();
    expect(container.textContent).not.toContain("the trigger is misconfigured.");
  });

  it("rejects a fix from before a switch even after the operator comes back", async () => {
    // The round trip the identity guard cannot see (review of PR #1744).
    //
    // Switching away is what re-enables Fix — that is this PR's own cleanup —
    // so returning to the same workflow and retrying the same failed run is a
    // path the fix opens rather than a contrived one. Both requests then name
    // the same workflow, the same company and the same `seq`, and the FIRST to
    // land is the stale one.
    const stale = deferred<WorkflowFixFromRun>();
    const retry = deferred<WorkflowFixFromRun>();
    const client = makeClient({ fixes: [stale.promise, retry.promise] });

    await show(client, "acme", WF_A);
    await openHistory();
    await click(fixButton());
    expect(fixButton()?.disabled).toBe(true);

    // Away and back — the switch to B empties the spinner slot, which is what
    // lets the operator press Fix on the very same row again.
    await show(client, "acme", WF_B);
    await show(client, "acme", WF_A);
    expect(fixButton()?.disabled).toBe(false);
    await click(fixButton());
    expect(fixButton()?.textContent).toContain("Fixing…");

    // The abandoned first request answers.
    await act(async () => {
      stale.resolve({
        automatable: false,
        reason: "a verdict from before the operator switched away.",
      });
    });

    // Pre-fix: that verdict renders under the run the operator is still
    // waiting on, and its `finally` clears the retry's spinner — the button
    // re-enables while a request nobody can see is still running.
    expect(container.textContent).not.toContain("a verdict from before the operator switched away.");
    expect(fixButton()?.textContent).toContain("Fixing…");
    expect(fixButton()?.disabled).toBe(true);

    // The retry is still the one that owns the row.
    await act(async () => {
      retry.resolve({ automatable: false, reason: "the trigger is misconfigured." });
    });
    expect(container.textContent).toContain("the trigger is misconfigured.");
    expect(fixButton()?.disabled).toBe(false);
  });

  it("does not carry a version-conflict banner onto the next workflow", async () => {
    const conflict = new ApiError(409, "conflict", "This workflow changed since you loaded it.");
    // Workflow B's graph read fails, so nothing incidentally clears the banner:
    // only a successful read does, and that is precisely the case where an
    // operator would never see the leak.
    const client = makeClient({
      graphFails: [WF_B],
      del: () => Promise.reject(conflict),
    });

    await show(client, "acme", WF_A);
    await click(inView("workflow-delete"));
    await click(inDocument("workflow-delete-confirm"));
    expect(inView("workflow-conflict")).not.toBeNull();

    await show(client, "acme", WF_B);

    // Pre-fix: a banner claiming B's graph is stale, offering a Reload that
    // re-reads B — a false statement with a remedy for something else.
    expect(inView("workflow-conflict")).toBeNull();
  });

  it("does not carry a graph-load error back to the index", async () => {
    const client = makeClient({ graphFails: [WF_A] });

    await show(client, "acme", WF_A);
    expect(inView("workflow-graph-error")).not.toBeNull();

    await click(inView("workflow-back-to-index"));

    // Pre-fix: "could not load the workflow graph" sits over an index that
    // loaded perfectly, about a workflow nobody is looking at.
    expect(inView("workflow-graph-error")).toBeNull();
  });

  it("keeps a workflow-LIST failure visible when the operator returns to the index", async () => {
    // Review of PR #1744. `error` was one slot for two unrelated failures, and
    // clearing it on every selection change threw the company-wide one away at
    // the exact moment the operator went back to the list it is about: a stale
    // list with nothing on screen saying so.
    const client = makeClient({ listFailsAfter: 1 });

    // The first list read lands the detail view; the host then says the list
    // changed (issue #384's `listEventTick`) and the refresh fails under it.
    await show(client, "acme", WF_A);
    await show(client, "acme", WF_A, 1);
    expect(inView("workflow-list-error")?.textContent).toContain("could not load workflows");

    await click(inView("workflow-back-to-index"));

    // Pre-fix: the selection change cleared the single shared `error` slot, so
    // the index rendered a stale list with nothing saying the refresh failed.
    expect(inView("workflow-list-error")?.textContent).toContain("could not load workflows");
  });

  it("does not carry a workflow-LIST failure onto the next company", async () => {
    // The other half of the same rule: the list read is keyed on the company,
    // so its failure has to end at a company change. Splitting the two slots
    // must not lose the axis the shared slot got right.
    const client = makeClient({ listFailsAfter: 1, listFailsFor: ["acme"] });

    await show(client, "acme", WF_A);
    await show(client, "acme", WF_A, 1);
    expect(inView("workflow-list-error")).not.toBeNull();

    // Beta's own list loads perfectly, and must not inherit Acme's warning.
    await show(client, "beta", WF_A, 1);

    expect(inView("workflow-list-error")).toBeNull();
  });

  it("drops the list failure the moment the company changes, not when the next list answers", async () => {
    // The company axis is the one the shared `error` slot got RIGHT, and
    // splitting it must not lose it. A successful read for the next company
    // would clear this eventually — but "eventually" is a whole round trip
    // during which Acme's "could not load workflows" sits over Beta's loading
    // list, which is the same false claim this PR is about everywhere else.
    const gate = deferred<null>();
    const client = makeClient({
      listFailsAfter: 1,
      listFailsFor: ["acme"],
      holdList: { company: "beta", gate: gate.promise },
    });

    await show(client, "acme", WF_A);
    await show(client, "acme", WF_A, 1);
    expect(inView("workflow-list-error")).not.toBeNull();

    // Beta's list read is still in flight.
    await show(client, "beta", WF_A, 1);
    expect(inView("workflow-list-error")).toBeNull();

    await act(async () => {
      gate.resolve(null);
    });
    expect(inView("workflow-list-error")).toBeNull();
  });
});
