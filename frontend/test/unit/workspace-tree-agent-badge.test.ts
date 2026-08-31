// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { TeamMemberDto } from "@/api/types";
import { ConnectionScopeProvider } from "@/connections/ConnectionContext";
import { WorkspaceView } from "@/views/WorkspaceView";

/**
 * The workspace tree's provenance pill names the teammate (issue #1723).
 *
 * The same leak as #973, #1369 and #1688, on the one surface those fixes did
 * not cover: the row label already resolved the roster id into a display name,
 * and the badge one line over printed the raw snake_case handle beside it — so
 * an `Agents/<agent>/` row read "SEO Specialist" *and* `seo_specialist`, the
 * polished name and the engine plumbing at once.
 *
 * Rendered against the real component and read out of the DOM rather than
 * unit-testing the resolver, because the resolver was never the bug: it exists,
 * it is correct, and this call site simply did not go through it. A test of
 * `rosterDisplayName` would have stayed green throughout.
 */

function node(over: {
  id: string;
  name: string;
  kind: "folder" | "file";
  parentId?: string;
  updatedAt?: number;
  createdBy?: { kind: "agent"; id: string } | { kind: "seed" } | { kind: "operator" };
}) {
  return { updatedAt: 1, ...over };
}

function member(id: string, name: string): TeamMemberDto {
  return { id, name, role: name };
}

function client(tree: ReturnType<typeof node>[], team: TeamMemberDto[]): OpenCompanyClient {
  return {
    scopeFor: () => "/api/v1/company/acme",
    get: vi.fn().mockResolvedValue(tree),
    listTeam: vi.fn().mockResolvedValue(team),
  } as unknown as OpenCompanyClient;
}

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

async function render(host: OpenCompanyClient) {
  await act(async () => {
    root.render(
      createElement(ConnectionScopeProvider, {
        scope: { connection: "c1", company: "acme" },
        children: createElement(WorkspaceView, { client: host, company: "acme" }),
      }),
    );
  });
  // Tree read and roster read are two separate effects.
  await act(async () => {});
}

function badges(): HTMLElement[] {
  return Array.from(container.querySelectorAll('[data-testid="workspace-tree-agent-badge"]'));
}

describe("the workspace tree's agent provenance badge", () => {
  it("reads the teammate's display name, not the raw roster handle", async () => {
    const tree = [
      node({ id: "standards", name: "standards", kind: "folder" }),
      node({
        id: "n-brief",
        name: "brief.md",
        kind: "file",
        parentId: "standards",
        createdBy: { kind: "agent", id: "seo_specialist" },
      }),
    ];

    await render(client(tree, [member("seo_specialist", "SEO Specialist")]));

    const [badge] = badges();
    expect(badge).toBeTruthy();
    expect(badge.textContent).toBe("SEO Specialist");
    expect(badge.textContent).not.toContain("seo_specialist");
  });

  it("keeps the raw handle addressable on the badge's own tooltip", async () => {
    // The handle is the teammate's real folder name and the identity every
    // artifact it holds is stamped with. Resolving the label must not put it
    // out of reach — an operator disambiguating two similarly-named teammates
    // has nowhere else on this row to look.
    const tree = [
      node({ id: "standards", name: "standards", kind: "folder" }),
      node({
        id: "n-brief",
        name: "brief.md",
        kind: "file",
        parentId: "standards",
        createdBy: { kind: "agent", id: "seo_specialist" },
      }),
    ];

    await render(client(tree, [member("seo_specialist", "SEO Specialist")]));

    expect(badges()[0].getAttribute("title")).toBe("Created by teammate seo_specialist");
  });

  it("falls back to the handle when the roster has no name for it", async () => {
    // An id the roster does not carry — not loaded, deleted, or a host with no
    // `/team` route at all — must still render something rather than a blank
    // pill.
    const tree = [
      node({ id: "standards", name: "standards", kind: "folder" }),
      node({
        id: "n-brief",
        name: "brief.md",
        kind: "file",
        parentId: "standards",
        createdBy: { kind: "agent", id: "analytics_analyst" },
      }),
    ];

    await render(client(tree, []));

    expect(badges()[0].textContent).toBe("analytics_analyst");
  });

  it("says nothing on a teammate's own folder, whose label already names them", async () => {
    // The row's label IS the resolved teammate name here, so the pill would
    // repeat it back verbatim — which is the redundancy #1723 opens with, and
    // resolving the pill without suppressing it would only have made both
    // halves say the same thing.
    const tree = [
      node({ id: "agents-root", name: "agents", kind: "folder" }),
      node({
        id: "n-seo",
        name: "seo_specialist",
        kind: "folder",
        parentId: "agents-root",
        createdBy: { kind: "agent", id: "seo_specialist" },
      }),
    ];

    await render(client(tree, [member("seo_specialist", "SEO Specialist")]));

    expect(container.textContent).toContain("SEO Specialist");
    expect(badges()).toHaveLength(0);
  });

  it("says nothing on the deliverables inside the author's own artifacts subtree", async () => {
    // `artifacts/<agent>/<task>/<file>` is attributed wholesale by the folder
    // it hangs under, so a per-row pill repeats the same fact once per row —
    // four identical pills stacked down a 256px pane, each eating the width
    // the *name* needs. That is the redundancy that made a `<title>.<id>`
    // folder name unreadable at depth in the first place.
    const tree = [
      node({ id: "artifacts-root", name: "artifacts", kind: "folder" }),
      node({
        id: "n-fe",
        name: "frontend_engineer",
        kind: "folder",
        parentId: "artifacts-root",
        createdBy: { kind: "agent", id: "frontend_engineer" },
      }),
      node({
        id: "n-task",
        name: "checkout-flow-redesign-spike.01hq8zm4xk3n7y2p9v1w5c8t01",
        kind: "folder",
        parentId: "n-fe",
        createdBy: { kind: "agent", id: "frontend_engineer" },
      }),
      node({
        id: "n-file",
        name: "spike.md",
        kind: "file",
        parentId: "n-task",
        createdBy: { kind: "agent", id: "frontend_engineer" },
      }),
    ];

    await render(client(tree, [member("frontend_engineer", "Frontend Engineer")]));

    const rows = Array.from(container.querySelectorAll("button"));
    for (const label of ["Frontend Engineer", "checkout-flow-redesign-spike"]) {
      const row = rows.find((b) => b.textContent?.includes(label));
      await act(async () => row?.click());
    }

    expect(container.textContent).toContain("checkout-flow-redesign-spike");
    expect(badges()).toHaveLength(0);
  });

  it("says nothing inside a folder minted under the kebab spelling of the id", async () => {
    // `ensure_artifact_folder` puts the roster id through `kebab_name`
    // (`src/company/workspace_names.rs`), so a company provisioned since that
    // rule shipped carries `artifacts/frontend-engineer/` while its roster says
    // `frontend_engineer`. That is the *same* teammate, and the suppression has
    // to see it as one — otherwise every row of every such company's artifacts
    // subtree wears a pill repeating its own enclosing folder.
    //
    // Comparing the raw ids instead would reopen it: the two spellings can
    // never be two teammates, because a roster id is `[a-z0-9_]` starting with
    // a letter (`manifest::is_snake_case`, and `ids::agent_slug` for the minted
    // ones), so no legal id contains a hyphen at all.
    const tree = [
      node({ id: "artifacts-root", name: "artifacts", kind: "folder" }),
      node({
        id: "n-fe",
        name: "frontend-engineer",
        kind: "folder",
        parentId: "artifacts-root",
        createdBy: { kind: "agent", id: "frontend_engineer" },
      }),
      node({
        id: "n-file",
        name: "spike.md",
        kind: "file",
        parentId: "n-fe",
        createdBy: { kind: "agent", id: "frontend_engineer" },
      }),
    ];

    await render(client(tree, [member("frontend_engineer", "Frontend Engineer")]));

    const rows = Array.from(container.querySelectorAll("button"));
    const folderRow = rows.find((b) => b.textContent?.includes("Frontend Engineer"));
    await act(async () => folderRow?.click());

    expect(container.textContent).toContain("spike");
    expect(badges()).toHaveLength(0);
  });

  it("still badges a node an agent wrote outside its own subtree", async () => {
    // The suppression is "this subtree already says who wrote it", not "agent
    // nodes are unbadged". A deliverable filed anywhere else keeps the marker,
    // which is the whole of #326.
    const tree = [
      node({ id: "standards", name: "standards", kind: "folder" }),
      node({
        id: "n-note",
        name: "api-review.md",
        kind: "file",
        parentId: "standards",
        createdBy: { kind: "agent", id: "frontend_engineer" },
      }),
    ];

    await render(client(tree, [member("frontend_engineer", "Frontend Engineer")]));

    expect(badges().map((b) => b.textContent)).toEqual(["Frontend Engineer"]);
  });

  it("still badges an agent-authored node inside a teammate's own folder", async () => {
    // Suppression is scoped to the teammate's own roster folder, not to the
    // subtree beneath it: a deliverable one teammate published into another's
    // folder is exactly the case the marker exists for.
    const tree = [
      node({ id: "agents-root", name: "agents", kind: "folder" }),
      node({
        id: "n-seo",
        name: "seo_specialist",
        kind: "folder",
        parentId: "agents-root",
        createdBy: { kind: "agent", id: "seo_specialist" },
      }),
      node({
        id: "n-note",
        name: "audit.md",
        kind: "file",
        parentId: "n-seo",
        createdBy: { kind: "agent", id: "analytics_analyst" },
      }),
    ];
    const team = [
      member("seo_specialist", "SEO Specialist"),
      member("analytics_analyst", "Analytics Analyst"),
    ];

    await render(client(tree, team));

    // The teammate folder is collapsed by default; open it.
    const rows = Array.from(container.querySelectorAll("button"));
    const folderRow = rows.find((b) => b.textContent?.includes("SEO Specialist"));
    await act(async () => folderRow?.click());

    expect(badges().map((b) => b.textContent)).toEqual(["Analytics Analyst"]);
  });
});
