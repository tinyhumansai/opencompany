// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { WorkflowGraph } from "@/api/workflows";

/**
 * The run input, after it came off the toolbar (issue #1204).
 *
 * The field being gone is the easy half and `workflow-toolbar-layout.test.ts`
 * pins it. This file pins the hard half: that the *capability* survived the
 * move intact. It is a real capability — the host seeds the payload as the
 * trigger node's item and any step bound to `=items` reads it (issue #154) — so
 * a version of this change that tidied the bar and quietly stopped delivering
 * the input would look like a success and would run those workflows on nothing.
 *
 * Four claims, and the third is the one that is easy to get wrong:
 *
 *  1. the dialog behind the split control's second half delivers the draft;
 *  2. the dialog's Test run delivers it too — the toolbar box fed BOTH
 *     dispatches, so a dialog that only ran for real would have dropped the
 *     rehearsal;
 *  3. the toolbar's own Run runs with NO input even when a draft is sitting in
 *     the dialog. The draft is a parameter of `run()`, not state it closes
 *     over; if it were state, moving the box out of sight would have turned a
 *     visible field into an invisible one, which is worse than the bar it was
 *     removed from; and
 *  4. what was asked is echoed back on the run's detail.
 *
 * These are claims about wiring between a control and a POST body, so they can
 * only be made against a rendered view — the same exception
 * `workflow-run-failure.test.ts` takes.
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

// React Flow measures its container on mount and jsdom has no layout; these
// three stubs are what let the view render at all. None is under test.
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

/** Every run POST this view made, in order. */
type Posted = { path: string; body: unknown };

function fakeClient(posts: Posted[]): OpenCompanyClient {
  return {
    scopeFor: (company: string | null) => `/api/v1/${company ?? "company"}`,
    get: async (path: string) => {
      if (path.endsWith("/workflows")) return [{ id: GRAPH.id, name: GRAPH.name }];
      if (path.includes("/workflows/runs")) return { runs: [], hasMore: false };
      return GRAPH;
    },
    post: async (path: string, body: unknown) => {
      posts.push({ path, body });
      return { runId: "r1", output: { text: "done" }, pendingApprovals: [] };
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
let posts: Posted[];

/**
 * The dialog is portaled out of the view's own subtree, so everything inside it
 * is addressed from the document rather than from `container`.
 */
const inDocument = <T extends HTMLElement>(selector: string) =>
  document.querySelector<T>(selector);
const inView = <T extends HTMLElement>(selector: string) =>
  container.querySelector<T>(selector);
const byTestId = (id: string) => inDocument<HTMLElement>(`[data-testid="${id}"]`);

/** The trigger input, wherever it currently lives. */
const requestField = () =>
  inDocument<HTMLInputElement>('input[aria-label="Request for this run"]');

async function click(id: string) {
  const el = byTestId(id);
  if (!el) throw new Error(`no “${id}” on screen`);
  await act(async () => {
    el.click();
  });
}

/** Opens the dialog behind the split control and types `text` into it. */
async function typeIntoDialog(text: string) {
  await click("workflow-run-with-input");
  const field = requestField();
  if (!field) throw new Error("the dialog mounted no request field");
  await act(async () => {
    // React tracks the DOM value node-side, so the setter has to be the
    // prototype's for `onChange` to see a change at all.
    const setter = Object.getOwnPropertyDescriptor(
      HTMLInputElement.prototype,
      "value",
    )?.set;
    setter?.call(field, text);
    field.dispatchEvent(new Event("input", { bubbles: true }));
  });
  return field;
}

async function mount() {
  await act(async () => {
    root.render(
      createElement(WorkflowsView, {
        client: fakeClient(posts),
        company: "acme",
        sub: GRAPH.id,
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
  posts = [];
  vi.clearAllMocks();
});

afterEach(async () => {
  await act(async () => {
    root.unmount();
  });
  container.remove();
});

describe("the run input is off the toolbar but still reachable (#1204)", () => {
  it("mounts no field on the bar, and a second half of Run that opens one", async () => {
    await mount();

    expect(inView('input[aria-label="Request for this run"]')).toBeNull();
    // The affordance that replaces it, named for a screen reader even though it
    // draws as an icon.
    const trigger = byTestId("workflow-run-with-input");
    expect(trigger).not.toBeNull();
    expect(trigger?.textContent).toContain("Run with input");

    expect(byTestId("workflow-run-input-dialog")).toBeNull();
    await click("workflow-run-with-input");
    const dialog = byTestId("workflow-run-input-dialog");
    expect(dialog).not.toBeNull();
    expect(dialog?.textContent).toContain("It is handed to the workflow’s first step.");
    expect(dialog?.textContent).not.toContain("=items");
    expect(requestField()).not.toBeNull();
  });

  it("delivers what was typed as the run's trigger input", async () => {
    await mount();
    await typeIntoDialog("the Q3 board deck");
    await click("workflow-run-input-submit");

    expect(posts).toHaveLength(1);
    expect(posts[0].path).toBe("/api/v1/acme/workflows/digest/run");
    expect(posts[0].body).toEqual({ input: { request: "the Q3 board deck" } });
    // And it closes behind the dispatch — a synchronous run holds the request
    // open for the whole run, and the canvas is what reports on it.
    expect(byTestId("workflow-run-input-dialog")).toBeNull();
  });

  it("trims the draft, so the echo and the payload cannot disagree", async () => {
    await mount();
    await typeIntoDialog("   spaced out   ");
    await click("workflow-run-input-submit");

    expect(posts[0].body).toEqual({ input: { request: "spaced out" } });
  });

  it("runs with no input at all when the field was left empty", async () => {
    await mount();
    await click("workflow-run-with-input");
    await click("workflow-run-input-submit");

    // `{}` is the host's "run with a null input" (src/server/ops/workflows.rs),
    // which is what a schedule-driven run gets. Not `{ request: "" }`.
    expect(posts[0].body).toEqual({ input: {} });
  });

  it("carries the same draft into a test run", async () => {
    // The toolbar box fed Run AND Test run. Proving a graph that reads `=items`
    // against a real input without sending anything is the rehearsal that
    // matters most for exactly the workflows this input exists for.
    await mount();
    await typeIntoDialog("dry subject");
    await click("workflow-run-input-test-run");

    expect(posts[0].body).toEqual({
      input: { request: "dry subject" },
      dry_run: true,
    });
  });

  it("runs on Enter, as the toolbar field did", async () => {
    await mount();
    const field = await typeIntoDialog("typed then entered");
    await act(async () => {
      field.dispatchEvent(
        new KeyboardEvent("keydown", { key: "Enter", bubbles: true }),
      );
    });

    expect(posts).toHaveLength(1);
    expect(posts[0].body).toEqual({ input: { request: "typed then entered" } });
  });

  it("keeps the draft for the next visit, so a re-run is an edit not a retype", async () => {
    await mount();
    await typeIntoDialog("the Q3 board deck");
    await click("workflow-run-input-submit");
    await click("workflow-run-with-input");

    expect(requestField()?.value).toBe("the Q3 board deck");
  });
});

describe("the toolbar's Run never carries a payload nobody can see (#1204)", () => {
  it("posts an empty input even with a draft sitting in the dialog", async () => {
    // The defect this change could have introduced. The field used to be on the
    // bar, in plain sight, wired into `run()`'s closure. Moving it behind a
    // dialog WITHOUT cutting that wire would leave a draft typed once riding
    // along on every later press of Run, with nothing on screen to say so.
    await mount();
    await typeIntoDialog("the Q3 board deck");
    await click("workflow-run-input-cancel");

    await click("workflow-run");
    expect(posts).toHaveLength(1);
    expect(posts[0].body).toEqual({ input: {} });
  });

  it("and neither does the toolbar's Test run", async () => {
    await mount();
    await typeIntoDialog("the Q3 board deck");
    await click("workflow-run-input-cancel");

    await click("workflow-test-run");
    expect(posts[0].body).toEqual({ input: {}, dry_run: true });
  });
});

describe("what was asked is echoed on the run's detail (#154)", () => {
  it("shows the request the shown output came from", async () => {
    await mount();
    await typeIntoDialog("the Q3 board deck");
    await click("workflow-run-input-submit");

    const drawer = byTestId("workflow-run-result");
    expect(drawer).not.toBeNull();
    expect(drawer?.textContent).toContain("Requested:");
    expect(drawer?.textContent).toContain("the Q3 board deck");
  });
});
