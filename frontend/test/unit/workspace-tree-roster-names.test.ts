// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { TeamMemberDto } from "@/api/types";
import { ConnectionScopeProvider } from "@/connections/ConnectionContext";
import { WorkspaceView } from "@/views/WorkspaceView";

/**
 * The workspace tree names roster ids (issue #973).
 *
 * #931 was the same class of bug — the eight teammates minted before #686
 * (before it started slugging a name into the id) carry ULID-style ids, and a
 * surface that prints the id instead of the name tells the operator nothing.
 * #939 fixed the two connections surfaces (`McpServersSection`,
 * `ProviderDetail`, `RepositoriesCard`) but never touched this one, so the
 * `agents/` folders kept showing raw ids one tab over. This is the guard for
 * this surface, so a sixth one cannot regress the same way silently: it
 * renders the real tree component against a fake host and reads the DOM the
 * operator actually sees, the same way `provider-detail-render.test.ts` pins
 * #931's fix on the connections panel.
 */

/** A minimal `FsNode` off the wire — only the fields the tree reads. */
function node(over: {
  id: string;
  name: string;
  kind: "folder" | "file";
  parentId?: string;
  updatedAt?: number;
}) {
  return { updatedAt: 1, ...over };
}

function member(id: string, name: string): TeamMemberDto {
  return { id, name, role: name };
}

/** A fake host: `get` answers the workspace tree read, `listTeam` the roster. */
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
  // The tree read and the roster read are two separate effects; give both a
  // further tick to settle rather than assuming one `act` flush covers both.
  await act(async () => {});
}

describe("the workspace tree", () => {
  it("names a teammate's agents/ folder instead of its raw roster id", async () => {
    const tree = [
      node({ id: "agents-root", name: "Agents", kind: "folder" }),
      node({ id: "n-zeta", name: "zeta-id", kind: "folder", parentId: "agents-root" }),
      node({ id: "n-alpha", name: "alpha-id", kind: "folder", parentId: "agents-root" }),
    ];
    const team = [member("zeta-id", "Alex"), member("alpha-id", "Zoe")];

    await render(client(tree, team));

    expect(container.textContent).toContain("Alex");
    expect(container.textContent).toContain("Zoe");
    // The id is still real — the folder's actual path and the identity every
    // artifact it holds is stamped with — but it is not what the operator
    // reads on the row; it lives in the tooltip only.
    expect(container.textContent).not.toContain("zeta-id");
    expect(container.textContent).not.toContain("alpha-id");
    const zeta = container.querySelector('[title="zeta-id"]');
    expect(zeta?.textContent).toBe("Alex");
  });

  it("sorts agents/ folders by display name, not by the lexical id", async () => {
    // Chosen so id order and name order disagree in both directions: raw-id
    // order is [alpha-id, zeta-id], name order must be [Alex, Zoe].
    const tree = [
      node({ id: "agents-root", name: "Agents", kind: "folder" }),
      node({ id: "n-alpha", name: "alpha-id", kind: "folder", parentId: "agents-root" }),
      node({ id: "n-zeta", name: "zeta-id", kind: "folder", parentId: "agents-root" }),
    ];
    const team = [member("alpha-id", "Zoe"), member("zeta-id", "Alex")];

    await render(client(tree, team));

    const text = container.textContent ?? "";
    expect(text.indexOf("Alex")).toBeGreaterThanOrEqual(0);
    expect(text.indexOf("Zoe")).toBeGreaterThan(text.indexOf("Alex"));
  });

  it("sorts agents/ folders by modified time first, display name only as a tie-breaker (issue #1687)", async () => {
    // Display-name order (Alex, Zoe) and modified-time order disagree: Zoe's
    // folder is the more recently touched one, so it must lead despite
    // sorting after Alex alphabetically.
    const tree = [
      node({ id: "agents-root", name: "Agents", kind: "folder" }),
      node({ id: "n-alpha", name: "alpha-id", kind: "folder", parentId: "agents-root", updatedAt: 10 }),
      node({ id: "n-zeta", name: "zeta-id", kind: "folder", parentId: "agents-root", updatedAt: 20 }),
    ];
    const team = [member("alpha-id", "Alex"), member("zeta-id", "Zoe")];

    await render(client(tree, team));

    const text = container.textContent ?? "";
    expect(text.indexOf("Zoe")).toBeGreaterThanOrEqual(0);
    expect(text.indexOf("Alex")).toBeGreaterThan(text.indexOf("Zoe"));
  });

  it("names an artifacts/ folder by its teammate too", async () => {
    // `artifacts/` files every published deliverable under the agent that
    // published it, so its direct children are roster ids exactly as
    // `agents/`'s are. A resolver scoped to one root would print raw ids on the
    // surface an operator opens to see what the company produced.
    //
    // Spelled lowercase where the fixtures above are spelled `Agents` on
    // purpose: the host mints lowercase now and adopts the legacy capitalized
    // root rather than renaming it, so both spellings reach the console and
    // between them these cases pin that the resolver reads either.
    const tree = [
      node({ id: "artifacts-root", name: "artifacts", kind: "folder" }),
      node({ id: "n-zeta", name: "zeta-id", kind: "folder", parentId: "artifacts-root" }),
    ];
    const team = [member("zeta-id", "Alex")];

    await render(client(tree, team));

    expect(container.textContent).toContain("Alex");
    expect(container.textContent).not.toContain("zeta-id");
  });

  it("keeps a raw folder name outside agents/ unresolved even if it matches a roster id", async () => {
    // The resolver is scoped to agents/'s direct children, not a blanket
    // find-and-replace over every folder name in the tree — a folder an
    // operator happened to name after a teammate's id elsewhere in the tree
    // must not be relabeled.
    const tree = [
      node({ id: "standards-root", name: "Standards", kind: "folder" }),
      node({ id: "n-be", name: "backend_engineer", kind: "folder", parentId: "standards-root" }),
    ];
    const team = [member("backend_engineer", "Backend Engineer")];

    await render(client(tree, team));

    expect(container.textContent).toContain("backend_engineer");
    expect(container.textContent).not.toContain("Backend Engineer");
  });

  it("sorts a direct agents/ file by its raw name, not a roster id it happens to match", async () => {
    // Only a roster *folder*'s name is an id worth resolving into a display
    // name for sorting. A file living directly under `agents/` is unusual but
    // legal, and its raw name could coincidentally collide with a roster id —
    // that collision must not reorder it by a display name it was never given
    // one for (it still renders under its raw name either way; only the sort
    // key is at stake here).
    const tree = [
      node({ id: "agents-root", name: "Agents", kind: "folder" }),
      node({ id: "f-alpha", name: "alpha-id", kind: "file", parentId: "agents-root" }),
      node({ id: "f-zeta", name: "zeta-id", kind: "file", parentId: "agents-root" }),
    ];
    // Display-name order (if files were wrongly resolved) is reversed:
    // zeta-id -> "Alex" sorts before alpha-id -> "Zoe".
    const team = [member("alpha-id", "Zoe"), member("zeta-id", "Alex")];

    await render(client(tree, team));

    const text = container.textContent ?? "";
    expect(text.indexOf("alpha-id")).toBeGreaterThanOrEqual(0);
    expect(text.indexOf("zeta-id")).toBeGreaterThan(text.indexOf("alpha-id"));
  });

  it("falls back to the raw id when the roster has no name for it", async () => {
    // An id the roster does not carry (not loaded, deleted, or a host with no
    // `/team` route at all) must still render something — never a blank row.
    const tree = [
      node({ id: "agents-root", name: "Agents", kind: "folder" }),
      node({ id: "n-unknown", name: "019fadd6f457-000000000099", kind: "folder", parentId: "agents-root" }),
    ];

    await render(client(tree, []));

    expect(container.textContent).toContain("019fadd6f457-000000000099");
  });
});
