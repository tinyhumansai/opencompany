// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { SearchHit } from "@/api/workspace";
import { ConnectionScopeProvider } from "@/connections/ConnectionContext";
import { WorkspaceView } from "@/views/WorkspaceView";
import { SearchResults } from "@/views/workspace/SearchResults";

/**
 * Issue #1457: the console showed 20 of 50 matches and offered no way to reach
 * the other 30.
 *
 * Two halves. The cap was the console's own: the host's `clamp_limit` clamps
 * rather than refusing, its ceiling is 50 and its default is 20, and the search
 * call named no limit at all. And the truncation was disclosed only at the
 * *head* — the `<ul>` simply ended, so an operator who scrolled to the bottom
 * met the last row and read it as the last match.
 */

function hit(
  over: Partial<SearchHit> & { id: string; name: string; path: string },
): SearchHit {
  return {
    kind: "file",
    parentId: null,
    updatedAt: 1,
    matched: "content",
    createdBy: { kind: "operator" },
    updatedBy: { kind: "operator" },
    ...over,
  } as SearchHit;
}

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  (
    globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }
  ).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

function render(hits: SearchHit[], total: number, query = "design") {
  return act(async () => {
    root.render(
      createElement(SearchResults, {
        query,
        hits,
        total,
        loading: false,
        error: null,
        onOpen: vi.fn(),
        rosterNames: new Map(),
      }),
    );
  });
}

const ROW = hit({
  id: "n1",
  name: "API design.md",
  path: "Standards/Engineering/Backend/Rust/API design.md",
});

describe("a truncated hit list says so at the bottom (issue #1457)", () => {
  it("names how many matches were withheld, and what to do about it", async () => {
    await render([ROW], 50);

    const foot = container.querySelector(
      '[data-testid="workspace-search-more"]',
    );
    expect(foot).not.toBeNull();
    expect(foot?.textContent).toContain("49 more matches");
    // No offset on the route, so the remedy is a narrower query, not a page.
    expect(foot?.textContent).toContain("narrow your search");
  });

  it("says nothing when the list is the whole answer", async () => {
    await render([ROW], 1);
    expect(
      container.querySelector('[data-testid="workspace-search-more"]'),
    ).toBeNull();
  });

  it("takes the singular for exactly one withheld match", async () => {
    await render([ROW], 2);
    const foot = container.querySelector(
      '[data-testid="workspace-search-more"]',
    );
    expect(foot?.textContent).toContain("1 more match ");
  });
});

describe("the console asks the host for its ceiling (issue #1457)", () => {
  it("names limit=50 on the search request rather than taking the host default", async () => {
    Element.prototype.scrollIntoView = vi.fn();
    localStorage.clear();
    const seen: string[] = [];
    const client = {
      scopeFor: () => "/api/v1/company/acme",
      get: vi.fn(async (path: string) => {
        seen.push(path);
        return path.includes("/workspace/search") ? { hits: [], total: 0 } : [];
      }),
    } as unknown as OpenCompanyClient;

    await act(async () => {
      root.render(
        createElement(ConnectionScopeProvider, {
          scope: { connection: "c1", company: "acme" },
          children: createElement(WorkspaceView, { client, company: "acme" }),
        }),
      );
    });

    const box = container.querySelector(
      '[data-testid="workspace-search"]',
    ) as HTMLInputElement;
    await act(async () => {
      const setter = Object.getOwnPropertyDescriptor(
        HTMLInputElement.prototype,
        "value",
      )?.set;
      setter?.call(box, "design");
      box.dispatchEvent(new Event("input", { bubbles: true }));
    });
    // The box debounces before it asks.
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 400));
    });

    const search = seen.find((url) => url.includes("/workspace/search"));
    expect(
      search,
      `no search request in ${JSON.stringify(seen)}`,
    ).toBeDefined();
    // The reported defect: no limit at all, so the host applied its default 20
    // while the header truthfully reported "20 of 50".
    expect(search).toContain("limit=50");
  });
});
