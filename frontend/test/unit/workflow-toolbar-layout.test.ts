// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { WorkflowGraph, WorkflowSummary } from "@/api/workflows";

/**
 * Issue #1135: the workflow detail toolbar carried index-level controls, on
 * three ragged rows.
 *
 * Every assertion here is about *what is on screen and how it is grouped*,
 * which is a fact about rendered output and about nothing else — no pure helper
 * can hold it, so this file renders the view, the way
 * `workflow-index-first.test.ts` earns the same exception.
 *
 * The four decisions the issue asked for are the four blocks below: the
 * workflow picker is gone from the detail view, `New workflow` belongs to the
 * index and is that screen's primary, the detail screen's one primary is `Run`,
 * and the toolbar is two rows with the description on its own line and `Delete`
 * set apart at the end.
 *
 * The primary assertions read the `bg-primary` fill off the rendered class
 * string on purpose. "Exactly one filled button per screen" is the issue's
 * third complaint stated exactly, and the fill is the only place that fact
 * exists — a `variant` prop is not observable once `cva` has flattened it.
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
// `ResizeObserver`. None of these three is under test.
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

const ROWS: WorkflowSummary[] = [
  {
    id: "alpha",
    name: "Daily Agent Harness Tweets",
    description:
      "Every day at 9am, searches for trending tweets matching 'Agent harness' and drafts a reply for review.",
  },
  { id: "beta", name: "Beta report" },
];

function graphFor(id: string): WorkflowGraph {
  const row = ROWS.find((r) => r.id === id);
  return {
    id,
    name: row?.name ?? id,
    version: "v1",
    nodes: [
      // Scheduled, so the Pause/Resume control mounts — it is one of the four
      // the issue names in the secondary group.
      { id: "start", kind: "trigger", name: "Start", schedule: "0 9 * * *" },
      { id: "done", kind: "output", name: "Done" },
    ],
    edges: [{ from: "start", to: "done" }],
  };
}

function makeClient(rows: WorkflowSummary[] = ROWS) {
  const client = {
    scopeFor: (company: string | null) => `/api/v1/${company ?? "company"}`,
    get: async (path: string) => {
      if (path.endsWith("/workflows")) return rows;
      if (path.includes("/workflows/tool-slugs")) return { slugs: [], unwired: [] };
      if (path.includes("/workflows/wired-channels")) return { channels: [] };
      if (path.includes("/workflows/runs")) return { runs: [], hasMore: false };
      const m = path.match(/\/workflows\/([^/?]+)$/);
      if (m) return graphFor(decodeURIComponent(m[1]));
      return null;
    },
    post: async () => ({}),
    del: async () => undefined,
    // Issue #1845: the week-1 nudge banner polls this on mount; an empty
    // feed keeps it a no-op for every test in this file, which is not about
    // the nudge.
    notifications: async () => ({ notifications: [], unread: 0 }),
    markNotificationsRead: async () => ({ unread: 0 }),
  } as unknown as OpenCompanyClient;
  return client;
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
  window.location.hash = "";
});

/** Mount at a hash, with `sub` matching it the way the router would hand it over. */
async function mountAt(hash: string, client: OpenCompanyClient, company = "acme") {
  window.location.hash = hash;
  const sub = hash.replace(/^#\/?/, "").split("?")[0].split("/").filter(Boolean)[1] ?? null;
  await act(async () => {
    root.render(createElement(WorkflowsView, { client, company, sub }));
  });
}

const buttons = () => Array.from(container.querySelectorAll<HTMLButtonElement>("button"));
const named = (label: string) =>
  buttons().find((b) => b.textContent?.trim() === label) ?? null;
/** Every button drawn with the primary fill — the screen's declared main action. */
const filled = () =>
  buttons()
    .filter((b) => b.className.split(/\s+/).includes("bg-primary"))
    .map((b) => b.textContent?.trim() ?? "");
const testId = (id: string) => container.querySelector<HTMLElement>(`[data-testid="${id}"]`);
const idsWithin = (id: string) =>
  Array.from(testId(id)?.querySelectorAll<HTMLElement>("[data-testid]") ?? []).map(
    (el) => el.dataset.testid,
  );

describe("the detail toolbar carries only this workflow's controls (#1135)", () => {
  it("drops the workflow picker — the name is the heading and the index is the switch", async () => {
    await mountAt("#/workflows/alpha", makeClient());

    expect(testId("workflow-detail-name")?.textContent).toBe("Daily Agent Harness Tweets");
    // The `<Select>` that used to sit under the heading, offering the workflow
    // you are already inside. Nothing on the detail screen is a combobox now.
    expect(container.querySelector('[role="combobox"]')).toBeNull();
    // The way back is a back link, not a dropdown.
    expect(testId("workflow-back-to-index")).not.toBeNull();
  });

  it("leaves `New workflow` to the index, where the list it makes one for lives", async () => {
    const client = makeClient();
    await mountAt("#/workflows/alpha", client);
    expect(named("New workflow")).toBeNull();
    expect(testId("workflow-create")).toBeNull();

    await act(async () => {
      testId("workflow-back-to-index")?.click();
    });
    expect(testId("workflow-create")).not.toBeNull();
  });
});

describe("one filled button per screen (#1135)", () => {
  it("makes Run the detail screen's primary, and nothing else", async () => {
    await mountAt("#/workflows/alpha", makeClient());
    // Issue #1204 made Run a split control, so the fill is worn by two
    // elements. That is not a second primary: they are the two halves of one
    // button, which the next assertion is what actually proves — the pair is
    // accepted only because both sit inside `workflow-run-split`. A filled
    // button anywhere ELSE on the detail screen still fails here, which is the
    // property #1135 asked for.
    expect(filled()).toEqual(["Run", "Run with input…"]);
    const split = testId("workflow-run-split");
    expect(split).not.toBeNull();
    for (const button of buttons().filter((b) =>
      b.className.split(/\s+/).includes("bg-primary"),
    )) {
      expect(split?.contains(button)).toBe(true);
    }
  });

  it("makes New workflow the index's primary, and nothing else", async () => {
    await mountAt("#/workflows", makeClient());
    expect(filled()).toEqual(["New workflow"]);
  });
});

describe("the detail toolbar is two rows (#1135)", () => {
  it("puts identity and state on row 1, with the description on its own line", async () => {
    await mountAt("#/workflows/alpha", makeClient());

    const identity = testId("workflow-detail-identity");
    expect(identity).not.toBeNull();
    expect(idsWithin("workflow-detail-identity")).toEqual([
      "workflow-back-to-index",
      "workflow-detail-name",
      "workflow-detail-description",
    ]);

    // "Its own line" is the fix for the description losing a width fight with
    // the chips, so it must be a sibling of the row they sit in, not a member
    // of it — a `<p>` inside that row would truncate exactly as before.
    const description = testId("workflow-detail-description");
    expect(description?.textContent).toContain("Every day at 9am");
    expect(description?.parentElement).toBe(identity);
    expect(description?.previousElementSibling?.contains(testId("workflow-detail-name"))).toBe(
      true,
    );
  });

  it("puts the actions on row 2, run intent first and Delete last", async () => {
    await mountAt("#/workflows/alpha", makeClient());

    const order = idsWithin("workflow-detail-actions");
    expect(order).toEqual([
      "workflow-run-split",
      "workflow-run",
      "workflow-run-with-input",
      "workflow-test-run",
      "workflow-copilot-toggle",
      "workflow-history-toggle",
      "workflow-toggle-enabled",
      "workflow-edit",
      "workflow-delete",
    ]);

    // Issue #1204: the free-text field is OFF the bar. This used to assert that
    // the field and Run shared a parent, which is the arrangement the issue was
    // filed against — the widest control on a row of nine, in the way of the
    // one thing an operator comes here to press. The stronger statement is that
    // the toolbar mounts no such field at all, which is false for the old
    // layout and for any half-measure that merely narrowed the box.
    expect(
      container.querySelector('input[aria-label="Request for this run"]'),
    ).toBeNull();

    // Grouped, not one undifferentiated strip. Asserted as what each group
    // EXCLUDES as well as what it holds: "the two halves share a parent" is
    // also true of the flat row this issue replaced, since everything shared
    // one parent there.
    const split = testId("workflow-run-split");
    expect(split?.contains(testId("workflow-run"))).toBe(true);
    expect(split?.contains(testId("workflow-run-with-input"))).toBe(true);
    expect(split?.contains(testId("workflow-test-run"))).toBe(false);
    const runGroup = split?.parentElement;
    expect(runGroup?.contains(testId("workflow-test-run"))).toBe(false);

    const utility = testId("workflow-delete")?.closest("div");
    expect(utility?.contains(testId("workflow-edit"))).toBe(true);
    expect(utility?.contains(testId("workflow-history-toggle"))).toBe(false);
    // And the destructive action is not another outline button one gap from
    // Edit: it carries the destructive tone.
    expect(testId("workflow-delete")?.className).toContain("text-destructive");
  });
});
