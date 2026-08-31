// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { WorkflowGraph, WorkflowSummary } from "@/api/workflows";

/**
 * Issue #1209: the Workflows index says when a schedule is off.
 *
 * `#276`'s disarm rule switches off every workflow created with a schedule, so
 * a company's scheduled workflows arrive paused. The index rendered them
 * identically to the ones that do fire — 8 of 18 rows in the reproduction — and
 * the create toast said only "Workflow created.". Both are facts about rendered
 * output, so this renders the view the way `workflow-index-first.test.ts` does.
 *
 * The `undefined` case is the one worth pinning hardest: `enabled` is optional
 * on the wire, and a host predating #276 sends no field. Badging that would put
 * "Paused" on every workflow of every older host.
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

/** One armed, one disarmed, one from a host that predates `enabled`. */
const ROWS: WorkflowSummary[] = [
  { id: "armed", name: "Armed digest", enabled: true },
  { id: "paused", name: "Paused sweep", enabled: false },
  { id: "unknown", name: "Older host" },
];

function graphFor(id: string): WorkflowGraph {
  return {
    id,
    name: ROWS.find((r) => r.id === id)?.name ?? id,
    version: "v1",
    nodes: [{ id: "start", kind: "trigger", name: "Start" }],
    edges: [],
  };
}

function makeClient(rows: WorkflowSummary[] = ROWS, created?: WorkflowGraph) {
  return {
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
    post: async () => created ?? {},
    del: async () => {},
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

async function mountIndex(client: OpenCompanyClient) {
  window.location.hash = "#/workflows";
  await act(async () => {
    root.render(createElement(WorkflowsView, { client, company: "acme", sub: null }));
  });
}

/** Whether the row for `name` carries the paused badge. Throws when no such
 * row is on screen, so a selector that stops matching fails loudly rather than
 * reporting every workflow as armed. */
function pausedFor(selector: string, name: string): boolean {
  const row = Array.from(container.querySelectorAll<HTMLElement>(selector)).find((el) =>
    el.textContent?.includes(name),
  );
  if (!row) throw new Error(`no ${selector} for “${name}”`);
  return row.querySelector('[data-testid="workflow-index-paused"]') !== null;
}

/** Sets a controlled input the way a keystroke would, so React's own value
 * descriptor sees the change and `onChange` fires. */
function type(selector: string, value: string) {
  const input = document.querySelector<HTMLInputElement>(
    `[data-slot="dialog-content"] ${selector}`,
  );
  if (!input) throw new Error(`no input matching ${selector}`);
  Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, value);
  input.dispatchEvent(new Event("input", { bubbles: true }));
}

/** Opens the New-workflow dialog, fills the smallest draft `validate()` accepts
 * (the starter trigger node already carries an id and a name), and submits. The
 * stub client answers the POST with whatever `makeClient` was given.
 *
 * Create confirms the permanent id first (#1808): the form's Create opens a
 * confirm, and the confirm's own action runs the write — so this clicks both. */
async function createThroughDialog() {
  await act(async () => {
    container.querySelector<HTMLButtonElement>('[data-testid="workflow-create"]')?.click();
  });
  await act(async () => {
    type('[placeholder="e.g. campaign_pipeline"]', "nightly");
  });
  await act(async () => {
    type('[placeholder="e.g. Campaign pipeline"]', "Nightly sweep");
  });
  await act(async () => {
    document
      .querySelector<HTMLButtonElement>('[data-testid="workflow-dialog-submit"]')
      ?.click();
  });
  await act(async () => {
    document
      .querySelector<HTMLButtonElement>('[data-testid="workflow-id-confirm-create"]')
      ?.click();
  });
}

describe("the workflows index shows a disarmed schedule", () => {
  it("badges the paused card, and only the paused card", async () => {
    await mountIndex(makeClient());

    const card = (name: string) => pausedFor('[data-testid="workflow-card"]', name);
    expect(card("Armed digest")).toBe(false);
    expect(card("Paused sweep")).toBe(true);
    // The whole point of matching `=== false` rather than falsy: an older host
    // sends no field, and every one of its workflows would otherwise badge.
    expect(card("Older host")).toBe(false);
  });

  it("badges the paused list row too, so the reading survives the toggle", async () => {
    await mountIndex(makeClient());

    await act(async () => {
      container.querySelector<HTMLButtonElement>('[data-testid="workflow-index-list"]')?.click();
    });

    const row = (name: string) => pausedFor('[data-testid="workflow-list-row"]', name);
    expect(row("Paused sweep")).toBe(true);
    expect(row("Armed digest")).toBe(false);
    expect(row("Older host")).toBe(false);
  });

  it("says so on create, rather than a flat acknowledgement", async () => {
    await mountIndex(
      makeClient(ROWS, {
        id: "nightly",
        name: "Nightly sweep",
        version: "v1",
        // What #276 does to any graph authored WITH a schedule.
        enabled: false,
        nodes: [{ id: "start", kind: "trigger", name: "Start", schedule: "15 2 * * *" }],
        edges: [],
      }),
    );
    await createThroughDialog();

    expect(toasts.success).not.toHaveBeenCalledWith("Workflow created.");
    expect(toasts.warning).toHaveBeenCalledTimes(1);
    const [text, options] = toasts.warning.mock.calls[0] as [string, { action?: { label: string } }];
    expect(text).toContain("Created, and paused");
    expect(text).toContain("Nightly sweep");
    // The recovery travels with the sentence, exactly as the edit path's does —
    // being told a schedule is off is only half of what an operator needs.
    expect(options?.action?.label).toBe("Resume");
  });

  it("keeps the plain acknowledgement when the create came back armed", async () => {
    await mountIndex(
      makeClient(ROWS, {
        id: "manual",
        name: "Manual only",
        version: "v1",
        enabled: true,
        nodes: [{ id: "start", kind: "trigger", name: "Start" }],
        edges: [],
      }),
    );
    await createThroughDialog();

    expect(toasts.success).toHaveBeenCalledWith("Workflow created.");
    expect(toasts.warning).not.toHaveBeenCalled();
  });
});
