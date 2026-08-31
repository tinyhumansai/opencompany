// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { WorkflowGraph, WorkflowSummary } from "@/api/workflows";

/**
 * Issue #1110: the Workflows tab lands on the INDEX, and opens one workflow
 * only when the operator picks it.
 *
 * Every assertion here is about *which surface is on screen for a given URL*,
 * which is a fact about rendered output and about nothing else — no pure helper
 * can hold it, so this file renders the view, the way
 * `workflow-run-failure.test.ts` earns the same exception.
 *
 * The five decisions the issue asked to be made deliberately are the five
 * blocks below: no auto-select, a shareable detail URL, a dead link landing on
 * the index with an explanation, a create landing on its own detail page, and a
 * delete returning to the index rather than to a neighbour. The sixth — that
 * browser Back moves index ↔ detail — is asserted through the history writes
 * those transitions make, since jsdom's history does not run a real back stack.
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
  { id: "alpha", name: "Alpha digest" },
  { id: "beta", name: "Beta report" },
];

function graphFor(id: string): WorkflowGraph {
  const row = ROWS.find((r) => r.id === id);
  return {
    id,
    name: row?.name ?? id,
    version: "v1",
    nodes: [
      { id: "start", kind: "trigger", name: "Start" },
      { id: "done", kind: "output", name: "Done" },
    ],
    edges: [{ from: "start", to: "done" }],
  };
}

/** A client over {@link ROWS}, recording the ids whose graph was fetched and
 * the ids that were deleted. The graph fetch fires for exactly the open
 * workflow, so it is also how a test reads back what got selected. */
function makeClient(rows: WorkflowSummary[] = ROWS) {
  const graphGets: string[] = [];
  const deletes: string[] = [];
  const client = {
    scopeFor: (company: string | null) => `/api/v1/${company ?? "company"}`,
    get: async (path: string) => {
      if (path.endsWith("/workflows")) return rows;
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
    del: async (path: string) => {
      const id = path.match(/\/workflows\/([^/?]+)/)?.[1];
      if (id) deletes.push(decodeURIComponent(id));
    },
    // Issue #1845: the week-1 nudge banner polls this on mount; an empty
    // feed keeps it a no-op for every test in this file, which is not about
    // the nudge.
    notifications: async () => ({ notifications: [], unread: 0 }),
    markNotificationsRead: async () => ({ unread: 0 }),
  } as unknown as OpenCompanyClient;
  return { client, graphGets, deletes };
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

const indexUp = () => container.querySelector('[data-testid="workflow-index"]') !== null;
const cards = () =>
  Array.from(container.querySelectorAll<HTMLButtonElement>('[data-testid="workflow-card"]'));
const detailName = () =>
  container.querySelector('[data-testid="workflow-detail-name"]')?.textContent ?? null;
const named = (label: string) =>
  Array.from(container.querySelectorAll("button")).find(
    (b) => b.textContent?.trim() === label,
  ) ?? null;

describe("the Workflows tab opens on the index", () => {
  it("selects nothing, and puts no per-workflow control on screen", async () => {
    const { client, graphGets } = makeClient();
    await mountAt("#/workflows", client);

    expect(indexUp()).toBe(true);
    expect(cards()).toHaveLength(2);
    // The reading that is the issue: the view never fetched a graph, because it
    // never picked one. On main it fetched `alpha`'s.
    expect(graphGets).toEqual([]);
    // None of the controls that need a subject is rendered.
    expect(named("Run")).toBeNull();
    expect(named("Test run")).toBeNull();
    expect(named("Edit")).toBeNull();
    expect(named("Delete")).toBeNull();
    expect(container.querySelector('[role="combobox"]')).toBeNull();
    // The one control that is not about a single workflow stays.
    expect(named("New workflow")).not.toBeNull();
    // …and nothing was written to the address bar, so the tab is still
    // shareable as "the workflows list".
    expect(window.location.hash).toBe("#/workflows");
  });

  it("opens a workflow when one is picked, and pushes its own URL", async () => {
    const { client, graphGets } = makeClient();
    await mountAt("#/workflows", client);

    await act(async () => {
      cards()[1].click();
    });

    expect(indexUp()).toBe(false);
    expect(detailName()).toBe("Beta report");
    expect(graphGets).toContain("beta");
    expect(named("Run")).not.toBeNull();
    // A push, not a replace: the index the operator came from is still behind
    // them, so browser Back returns to it.
    expect(window.location.hash).toBe("#/workflows/beta");
  });

  it("goes back to the index from the detail view, and drops the id again", async () => {
    const { client } = makeClient();
    await mountAt("#/workflows/alpha", client);
    expect(detailName()).toBe("Alpha digest");

    await act(async () => {
      container
        .querySelector<HTMLButtonElement>('[data-testid="workflow-back-to-index"]')
        ?.click();
    });

    expect(indexUp()).toBe(true);
    expect(named("Run")).toBeNull();
    expect(window.location.hash).toBe("#/workflows");
  });
});

describe("a URL naming a workflow opens it", () => {
  it("renders the detail view for the id in the hash, with no index in the way", async () => {
    const { client, graphGets } = makeClient();
    await mountAt("#/workflows/alpha", client);

    expect(indexUp()).toBe(false);
    expect(detailName()).toBe("Alpha digest");
    expect(graphGets).toContain("alpha");
    // The link survives verbatim — nothing rewrote the URL on the way in, which
    // is what makes it shareable and reload-proof.
    expect(window.location.hash).toBe("#/workflows/alpha");
  });

  it("lands on the index, explained, when the id names nothing", async () => {
    const { client, graphGets } = makeClient();
    await mountAt("#/workflows/gone", client);

    expect(graphGets).not.toContain("gone");
    expect(indexUp()).toBe(true);
    const banner = container.querySelector('[data-testid="workflow-missing-link"]');
    expect(banner).not.toBeNull();
    expect(banner?.textContent).toContain("gone");
    // No empty detail shell: nothing was auto-opened in its place either.
    expect(detailName()).toBeNull();
    expect(graphGets).toEqual([]);
    // And the dead id is out of the address bar, in place — Back must not
    // return to a workflow that does not exist.
    expect(window.location.hash).toBe("#/workflows");
  });
});

describe("leaving a workflow behind", () => {
  // Both routes back to the index, because the rule lives on the selection
  // rather than on the back button: the one nobody remembers to update is the
  // delete, and it is the one that leaves the drawer open the longest.
  for (const route of ["the back button", "a delete"] as const) {
    it(`closes the per-workflow drawers on ${route}, then opens both defaults for the next selection`, async () => {
      const { client } = makeClient();
      await mountAt("#/workflows/alpha", client);

      await act(async () => {
        container
          .querySelector<HTMLButtonElement>('[data-testid="workflow-history-toggle"]')
          ?.click();
      });
      expect(container.querySelector('[data-testid="workflow-run-history"]')).not.toBeNull();

      if (route === "the back button") {
        await act(async () => {
          container
            .querySelector<HTMLButtonElement>('[data-testid="workflow-back-to-index"]')
            ?.click();
        });
      } else {
        await act(async () => {
          container.querySelector<HTMLButtonElement>('[data-testid="workflow-delete"]')?.click();
        });
        await act(async () => {
          document
            .querySelector<HTMLButtonElement>('[data-testid="workflow-delete-confirm"]')
            ?.click();
        });
      }

      expect(container.querySelector('[data-testid="workflow-index"]')).not.toBeNull();
      expect(container.querySelector('[data-testid="workflow-run-history"]')).toBeNull();
      expect(container.querySelector('[data-testid="workflow-copilot"]')).toBeNull();

      await act(async () => {
        cards()[route === "the back button" ? 1 : 0].click();
      });

      expect(detailName()).toBe("Beta report");
      // The old workflow's drawers did close on the index. Opening a workflow
      // from that index is a new request, and #1683 deliberately starts both
      // panes for the newly selected workflow.
      expect(container.querySelector('[data-testid="workflow-run-history"]')).not.toBeNull();
      expect(container.querySelector('[data-testid="workflow-copilot"]')).not.toBeNull();
    });
  }

  it("returns to the index after deleting the open one, without picking a neighbour", async () => {
    const { client, graphGets, deletes } = makeClient();
    await mountAt("#/workflows/alpha", client);
    expect(detailName()).toBe("Alpha digest");

    await act(async () => {
      container.querySelector<HTMLButtonElement>('[data-testid="workflow-delete"]')?.click();
    });
    await act(async () => {
      document
        .querySelector<HTMLButtonElement>('[data-testid="workflow-delete-confirm"]')
        ?.click();
    });

    expect(deletes).toEqual(["alpha"]);
    expect(indexUp()).toBe(true);
    // `beta` is all that is left, and it must NOT have been opened: the
    // operator deleted a workflow, they did not ask to work on another one.
    expect(graphGets).not.toContain("beta");
    expect(cards().map((c) => c.textContent)).toHaveLength(1);
    expect(window.location.hash).toBe("#/workflows");
  });
});
