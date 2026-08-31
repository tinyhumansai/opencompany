// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { SearchHit } from "@/api/workspace";
import { DERIVED_LABEL } from "@/lib/workspace";
import { SearchResults } from "@/views/workspace/SearchResults";

/**
 * Issue #1377, the search half.
 *
 * A search replaces the tree in the explorer pane, so a hit list is the only
 * thing on screen — and in the seeded company a query like "the" returns eight
 * `derived/` files interleaved with hand-written notes. They used to be
 * distinguishable only by a `Seeded` badge, which is not merely uninformative:
 * `derived::publish` stamps `WorkspaceOrigin::Seed` so the write guard can tell
 * its own derivation from a person, and `Seeded` in this console means "shipped
 * with the company bundle and was typed by nobody". These files shipped with
 * nothing; they were rendered seconds ago and are rewritten on every ledger
 * write.
 *
 * So the cases here are: the true label appears, and the false one does not.
 */

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

function hit(partial: Partial<SearchHit> & { id: string; name: string; path: string }): SearchHit {
  return {
    kind: "file",
    parentId: null,
    updatedAt: 0,
    createdBy: { kind: "seed" },
    updatedBy: { kind: "seed" },
    matched: "content",
    ...partial,
  } as SearchHit;
}

const HITS: SearchHit[] = [
  hit({ id: "goals", name: "GOALS.md", path: "derived/GOALS.md" }),
  // A seeded hand-written note: same `Seeded` origin, and it keeps the badge —
  // for this one the word is true.
  hit({ id: "release", name: "Release checklist.md", path: "playbooks/release-checklist.md" }),
];

function renderHits(hits: SearchHit[]) {
  act(() =>
    root.render(
      createElement(SearchResults, {
        query: "the",
        hits,
        total: hits.length,
        loading: false,
        error: null,
        onOpen: () => {},
        rosterNames: new Map(),
      }),
    ),
  );
}

function rowFor(id: string): HTMLElement {
  const row = container.querySelector<HTMLElement>(`[data-node-id="${id}"]`);
  if (!row) throw new Error(`no hit row for ${id}`);
  return row;
}

describe("a ledger's file in the search hit list", () => {
  it("is named by what writes it", () => {
    renderHits(HITS);
    const marker = rowFor("goals").querySelector('[data-testid="workspace-search-derived"]');
    expect(marker?.textContent).toContain(DERIVED_LABEL);
    // The reason travels with it for a pointer, so the label is not a bare
    // rule — the tree row and the note header carry the same sentence.
    expect(marker?.getAttribute("title")).toContain("re-derived");
  });

  it("does not also claim it was seeded", () => {
    renderHits(HITS);
    expect(rowFor("goals").textContent).not.toContain("Seeded");
  });

  it("leaves an ordinary seeded note its badge", () => {
    renderHits(HITS);
    const ordinary = rowFor("release");
    expect(ordinary.textContent).toContain("Seeded");
    expect(ordinary.querySelector('[data-testid="workspace-search-derived"]')).toBeNull();
  });
});
