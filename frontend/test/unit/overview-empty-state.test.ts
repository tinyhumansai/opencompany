// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";

/**
 * Issue #1313 / PR review: the "No desks yet" empty state must only fire for
 * a genuinely empty company, not for one whose `GET /desks` merely failed.
 *
 * `Overview` reads six sources best-effort. A rejected `/desks` is normalized
 * to `[]` so the other five rings can still draw — but that same `[]` used to
 * satisfy the empty-state condition, overlaying "No desks yet" and hiding the
 * graph controls over a company that may well have desks whose read just
 * failed. The graph keeps `emptyState` off unless the desks read itself was
 * fulfilled.
 */

// Captured on every render of the (mocked) graph so a test can read what
// `Overview` actually decided.
const { graphProps } = vi.hoisted(() => ({
  graphProps: { emptyState: false, noDesks: false, nodeCount: 0 },
}));

vi.mock("@/views/overview/kg/KnowledgeGraph", () => ({
  KnowledgeGraph: (props: {
    emptyState?: boolean;
    noDesks?: boolean;
    graph?: { nodes: unknown[] };
  }) => {
    graphProps.emptyState = !!props.emptyState;
    graphProps.noDesks = !!props.noDesks;
    graphProps.nodeCount = props.graph?.nodes.length ?? 0;
    return null;
  },
}));

const { Overview } = await import("@/views/Overview");

/** Every `client.get` path this component reads, keyed by its suffix. */
const HEALTHY_GET: Record<string, unknown> = {
  "/tasks": [],
  "/users": [],
  // `GET /memory` answers with `{ items, totalContext, contextTruncated }`.
  "/memory": { items: [], totalContext: 0, contextTruncated: false },
  "/workflows": [],
};

/**
 * A fake host whose other five reads all succeed, so the only variable under
 * test is the desks read. Same shape as the sibling `overview-unreachable-host`
 * fixture: `get` is dispatched by path suffix, desks and team are their own
 * client methods.
 */
function fakeClient() {
  const get = vi.fn((path: string) => {
    const suffix = Object.keys(HEALTHY_GET).find((k) => path.endsWith(k));
    return Promise.resolve(suffix ? HEALTHY_GET[suffix] : []);
  });
  const listDesks = vi.fn().mockResolvedValue([]);
  const listTeam = vi.fn().mockResolvedValue([]);
  const client = {
    scopeFor: () => "/api/v1/company/acme",
    get,
    listDesks,
    listTeam,
  } as unknown as OpenCompanyClient;
  return { client, listDesks };
}

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  graphProps.emptyState = false;
  graphProps.noDesks = false;
  graphProps.nodeCount = 0;
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

async function render(host: OpenCompanyClient) {
  await act(async () => {
    root.render(createElement(Overview, { client: host, company: "acme" }));
  });
  // The six sources resolve as one `Promise.allSettled`, but the state it sets
  // lands a tick later; give React that tick rather than assuming one flush
  // covers it.
  await act(async () => {});
  await act(async () => {});
}

describe("the overview empty state", () => {
  it("appears only for a company a successful desks read found empty", async () => {
    const { client } = fakeClient();
    await render(client);

    // `listDesks` answered `[]` and nothing else failed: genuinely no desks.
    expect(graphProps.emptyState).toBe(true);
  });

  it("stays hidden when the desks read failed, even if the other reads answered", async () => {
    const mocks = fakeClient();
    mocks.listDesks.mockRejectedValue(new Error("desks unavailable"));

    await render(mocks.client);

    // The company may well have desks; the request just failed. Drawing the
    // graph without pillars is honest — claiming "No desks yet" is not.
    expect(graphProps.emptyState).toBe(false);
    expect(graphProps.noDesks).toBe(false);
  });

  /**
   * A deskless company still has a graph, and the two facts are separate.
   *
   * They were one flag, and the flag suppressed the canvas: a company with a
   * roster, tools and saved workflows but no `[[group_chat]]` got a blank
   * field under "No desks yet". The model has always placed a worker with no
   * desk on the core, so there was something to draw the whole time.
   */
  it("draws the graph for a deskless company that has a roster", async () => {
    const mocks = fakeClient();
    mocks.listDesks.mockResolvedValue([]);
    (mocks.client.listTeam as ReturnType<typeof vi.fn>).mockResolvedValue([
      { id: "engineer", name: "Engineer", role: "engineer", tools: ["workspace"] },
    ]);

    await render(mocks.client);

    // The fact about the company: no pillars, and the corner says so.
    expect(graphProps.noDesks).toBe(true);
    // The fact about the graph: there is more than the core node, so it draws.
    expect(graphProps.nodeCount).toBeGreaterThan(1);
    expect(graphProps.emptyState).toBe(false);
  });

  it("covers the canvas only when nothing but the core node is left to draw", async () => {
    const { client } = fakeClient();
    await render(client);

    expect(graphProps.nodeCount).toBe(1);
    expect(graphProps.emptyState).toBe(true);
    expect(graphProps.noDesks).toBe(true);
  });

  /**
   * Codex review on PR #1931: durable memory is passed to `KnowledgeGraph`
   * through its own `memory` prop rather than folded into `graph.nodes`, so a
   * deskless company with no roster, tasks, or workflows but a nonempty
   * memory constellation must not still be called `emptyState` — there is a
   * memory graph to look at even though `graph.nodes` holds only the core.
   */
  it("stays hidden for a deskless company whose only content is durable memory", async () => {
    const mocks = fakeClient();
    const get = mocks.client.get as ReturnType<typeof vi.fn>;
    get.mockImplementation((path: string) => {
      if (path.endsWith("/memory")) {
        return Promise.resolve({
          items: [
            {
              id: "fact-1",
              kind: "fact",
              origin: "fact",
              editable: true,
              title: "Founded in 2024",
              body: "The company was founded in 2024.",
              source: "operator",
              updatedAt: 0,
            },
          ],
          totalContext: 0,
          contextTruncated: false,
        });
      }
      return Promise.resolve([]);
    });

    await render(mocks.client);

    // The fact about the company: still no pillars.
    expect(graphProps.noDesks).toBe(true);
    // The fact about the graph proper: still only the core node.
    expect(graphProps.nodeCount).toBe(1);
    // But the memory constellation is not empty, so the empty state must not
    // cover it.
    expect(graphProps.emptyState).toBe(false);
  });
});
