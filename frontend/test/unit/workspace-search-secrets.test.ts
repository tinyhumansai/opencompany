// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { SearchHit } from "@/api/workspace";
import { SECRETS_LABEL } from "@/lib/workspace";
import { SearchResults } from "@/views/workspace/SearchResults";

/**
 * Issue #1465, the search half.
 *
 * Operator search is deliberately unfiltered — `search_workspace` keeps the
 * whole tree while `search_workspace_for_agent` drops `secrets/` — which is
 * right, and is exactly why a private note used to come back in this list
 * looking like any other. A search replaces the tree in the explorer pane, so a
 * hit row is the only thing on screen: the row that says nothing is the row a
 * screen-shared console leaks.
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
  hit({ id: "keys", name: "Stripe keys.md", path: "secrets/Stripe keys.md" }),
  hit({ id: "deep", name: "Twilio.md", path: "secrets/vendors/Twilio.md" }),
  // An ordinary note, and a lookalike folder that is ordinary shared content
  // host-side — marking it would promise a privacy it does not have.
  hit({ id: "runbook", name: "Runbook.md", path: "Playbooks/Runbook.md" }),
  hit({ id: "old", name: "Archived key.md", path: "secrets-old/Archived key.md" }),
];

function renderHits(hits: SearchHit[]) {
  act(() =>
    root.render(
      createElement(SearchResults, {
        query: "key",
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

function markerIn(id: string) {
  return rowFor(id).querySelector('[data-testid="workspace-search-secret"]');
}

describe("a note the agents cannot read, in the search hit list", () => {
  it("says so on the row", () => {
    renderHits(HITS);
    expect(markerIn("keys")?.textContent).toContain(SECRETS_LABEL);
    // The rule travels with the marker for a pointer, so the label is not a
    // bare two words — the tree row and the note header carry the same one.
    expect(markerIn("keys")?.getAttribute("title")).toContain("cannot list, read, search or write");
  });

  it("marks a note nested deeper than the folder itself", () => {
    renderHits(HITS);
    expect(markerIn("deep")).not.toBeNull();
  });

  it("leaves ordinary hits unmarked", () => {
    renderHits(HITS);
    expect(markerIn("runbook")).toBeNull();
    expect(markerIn("old")).toBeNull();
  });

  it("keeps the origin badge, unlike the derived marking", () => {
    // A `Seeded` note under `secrets/` really was seeded — the host lays down
    // one README there on first boot — so both badges are true and say
    // different things. (`derived/` suppresses `Seeded` because it is false
    // there; see #1377.)
    renderHits(HITS);
    expect(rowFor("keys").textContent).toContain("Seeded");
  });
});
