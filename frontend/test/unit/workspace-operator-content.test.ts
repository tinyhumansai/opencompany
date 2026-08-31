import { describe, expect, it } from "vitest";

import type { FsNode } from "@/api/workspace";
import { childrenOf, hasOperatorContent, SYSTEM_ROOTS } from "@/lib/workspace";

/**
 * Issues #1481 and #1382 (item 4).
 *
 * `ensure_workspace_scaffold` runs on every boot, so `nodes.length === 0` is
 * unreachable on a live company — which is why the explorer's "This workspace
 * is empty" branch was dead code describing a state that cannot happen. The
 * question the empty state actually needs answered is different: has anybody
 * written anything here *yet*, because "pick a note from the explorer" is the
 * wrong instruction for someone whose explorer holds three rows they did not
 * create.
 */

function node(over: {
  id: string;
  name: string;
  kind: "folder" | "file";
  parentId?: string | null;
  updatedAt?: number;
}): FsNode {
  return {
    parentId: null,
    updatedAt: 1,
    createdBy: { kind: "seed" },
    updatedBy: { kind: "seed" },
    ...over,
  } as FsNode;
}

/** Exactly what a freshly provisioned company boots with. */
const SCAFFOLD: FsNode[] = [
  node({ id: "agents", name: "Agents", kind: "folder" }),
  node({ id: "secrets", name: "secrets", kind: "folder" }),
  node({ id: "readme", name: "README.md", kind: "file", parentId: "secrets" }),
  node({ id: "artifacts", name: "Artifacts", kind: "folder" }),
  node({
    id: "artifacts-readme",
    name: "README.md",
    kind: "file",
    parentId: "artifacts",
  }),
];

describe("hasOperatorContent", () => {
  it("is false for a workspace holding nothing but the scaffold", () => {
    expect(hasOperatorContent(SCAFFOLD)).toBe(false);
  });

  it("is false for a genuinely empty tree", () => {
    expect(hasOperatorContent([])).toBe(false);
  });

  it("is true the moment a person writes one note", () => {
    expect(
      hasOperatorContent([
        ...SCAFFOLD,
        node({ id: "n1", name: "Plan.md", kind: "file" }),
      ]),
    ).toBe(true);
  });

  it("does not count a teammate's own Agents/<id>/ folder, which the host mints", () => {
    expect(
      hasOperatorContent([
        ...SCAFFOLD,
        node({
          id: "roster",
          name: "01JQZY8T7K",
          kind: "folder",
          parentId: "agents",
        }),
      ]),
    ).toBe(false);
  });

  it("counts a note filed inside a teammate's folder, which a person did choose", () => {
    expect(
      hasOperatorContent([
        ...SCAFFOLD,
        node({
          id: "roster",
          name: "01JQZY8T7K",
          kind: "folder",
          parentId: "agents",
        }),
        node({ id: "n1", name: "Brief.md", kind: "file", parentId: "roster" }),
      ]),
    ).toBe(true);
  });

  it("counts a note a person files inside Artifacts", () => {
    expect(
      hasOperatorContent([
        ...SCAFFOLD,
        node({ id: "n1", name: "Plan.md", kind: "file", parentId: "artifacts" }),
      ]),
    ).toBe(true);
  });

  it("counts a folder an operator named the same as a system root elsewhere in the tree", () => {
    // The scaffold set is root-scoped; `Product/secrets/` is somebody's folder.
    expect(
      hasOperatorContent([
        ...SCAFFOLD,
        node({ id: "product", name: "Product", kind: "folder" }),
        node({
          id: "nested",
          name: "secrets",
          kind: "folder",
          parentId: "product",
        }),
      ]),
    ).toBe(true);
  });

  it("mirrors the host's SYSTEM_ROOTS by name", () => {
    expect([...SYSTEM_ROOTS]).toEqual(["agents", "artifacts", "secrets"]);
  });
});

describe("childrenOf pins derived last (issue #1382)", () => {
  it("sorts each sibling group by modified time, then name", () => {
    const tree = [
      node({ id: "old", name: "Older", kind: "folder", updatedAt: 10 }),
      node({ id: "tie-z", name: "Zebra", kind: "folder", updatedAt: 20 }),
      node({ id: "derived", name: "derived", kind: "folder", updatedAt: 40 }),
      node({ id: "new", name: "Newest", kind: "folder", updatedAt: 30 }),
      node({ id: "tie-a", name: "Alpha", kind: "folder", updatedAt: 20 }),
      node({ id: "file-old", name: "Older.md", kind: "file", updatedAt: 10 }),
      node({ id: "file-new", name: "Newest.md", kind: "file", updatedAt: 30 }),
    ];

    expect(childrenOf(tree, null).map((n) => n.name)).toEqual([
      "Newest",
      "Alpha",
      "Zebra",
      "Older",
      "derived",
      "Newest.md",
      "Older.md",
    ]);
  });

  it("sorts the read-only folder after the ones a person made", () => {
    const tree = [
      node({ id: "z", name: "Zebra", kind: "folder" }),
      node({ id: "d", name: "derived", kind: "folder" }),
      node({ id: "a", name: "Alpha", kind: "folder" }),
    ];

    expect(childrenOf(tree, null).map((n) => n.name)).toEqual([
      "Alpha",
      "Zebra",
      "derived",
    ]);
  });

  it("still puts every folder before every file", () => {
    const tree = [
      node({ id: "f", name: "Note.md", kind: "file" }),
      node({ id: "d", name: "derived", kind: "folder" }),
    ];

    expect(childrenOf(tree, null).map((n) => n.kind)).toEqual([
      "folder",
      "file",
    ]);
  });

  it("leaves an operator's own folder named derived alone relative to files", () => {
    // Name-based and therefore blunt, but only the host's folder carries it and
    // the effect on anything else is cosmetic ordering.
    const tree = [
      node({ id: "d", name: "Derived", kind: "folder", parentId: "x" }),
      node({ id: "a", name: "Alpha", kind: "folder", parentId: "x" }),
    ];

    expect(childrenOf(tree, "x").map((n) => n.name)).toEqual([
      "Alpha",
      "Derived",
    ]);
  });
});
