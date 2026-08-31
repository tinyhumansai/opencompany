// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { SearchHit } from "@/api/workspace";
import { SearchResults } from "@/views/workspace/SearchResults";

/**
 * Issue #1375, on the hit row itself.
 *
 * The excerpt rendered the host's full window into a two-line clamp about 250px
 * wide, so a match past the first dozen words was clamped away — two lines of
 * arbitrary prose with nothing marked in them. And the path rendered in full
 * with a tail `truncate`, cutting the discriminating end off
 * `Standards/Engineering/Backend/Rust/API design` while repeating the filename
 * already shown on the line above.
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

describe("a hit's path identifies it (issue #1375)", () => {
  it("shows the parent folders, not the filename it already printed above", async () => {
    await render([ROW], 1);
    const path = container.querySelector(
      '[data-testid="workspace-search-path"]',
    );

    expect(path?.textContent).toBe("Standards/Engineering/Backend/Rust");
    expect(path?.textContent).not.toContain("API design.md");
  });

  it("ellipsises from the start so the discriminating tail survives", async () => {
    await render([ROW], 1);
    const path = container.querySelector(
      '[data-testid="workspace-search-path"]',
    ) as HTMLElement;

    // `truncate` cuts the tail; RTL makes the browser drop from the left at the
    // real rendered width instead.
    expect(path.getAttribute("dir")).toBe("rtl");
    expect(path.className).toContain("truncate");
    // `<bdi>` pins segment order so the RTL context cannot reorder the path.
    expect(path.tagName.toLowerCase()).toBe("bdi");
  });

  it("still answers for a note at the root", async () => {
    await render([hit({ id: "n2", name: "README.md", path: "README.md" })], 1);
    expect(
      container.querySelector('[data-testid="workspace-search-path"]')
        ?.textContent,
    ).toBe("Workspace root");
  });
});

describe("an excerpt shows the match it came back for (issue #1375)", () => {
  it("marks the query even when the host's window puts it late", async () => {
    const late = hit({
      id: "n3",
      name: "Notes.md",
      path: "Product/Notes.md",
      excerpt:
        "This section opens with a long preamble about ownership and review cadence before it ever mentions design at all.",
    });
    await render([late], 1);

    const excerpt = container.querySelector(
      '[data-testid="workspace-search-excerpt"]',
    );
    const mark = excerpt?.querySelector("mark");
    expect(mark?.textContent).toBe("design");
    // The reported defect: the match sat past the two-line clamp. It is now
    // near the front of the rendered text.
    expect((excerpt?.textContent ?? "").indexOf("design")).toBeLessThan(40);
    expect(excerpt?.textContent?.startsWith("…")).toBe(true);
  });
});
