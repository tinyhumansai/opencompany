/**
 * The duplicate-folder repair on the console side (issue #759).
 *
 * A publish race can leave two sibling folders with one name, after which every
 * publish beneath that path is refused as ambiguous. The host merges what it
 * safely can and *reports* what it will not touch, and these pin the three
 * places the console could quietly lose half of that answer: reading the wrong
 * fold list, dropping the residuals, or replaying the changes onto its own copy
 * of the tree in the wrong order.
 */
import { describe, expect, it } from "vitest";

import { OpenCompanyClient } from "@/api/client";
import { mergeDuplicateFolders, residualReason, type RepairOutcome } from "@/api/workspace";
import { applyRepair, type FsNode } from "@/lib/workspace";

function client(handler: (req: { method: string; url: string }) => unknown) {
  const transport = {
    request: async (req: { method: string; url: string; body?: string }) => ({
      status: 200,
      statusText: "OK",
      url: req.url,
      text: JSON.stringify(handler(req)),
      header: () => null,
    }),
    subscribe: () => () => {},
  };
  return new OpenCompanyClient(
    { baseUrl: "", company: "acme", operatorToken: "t0ken", sessionHeader: null },
    transport as never,
  );
}

const fold = {
  id: "dupe",
  name: "reports",
  intoId: "keep",
  moved: [{ id: "note", name: "q2.md" }],
  removed: true,
};

const residual = {
  id: "rival",
  name: "summary.md",
  parentId: "dupe",
  cause: "fileInTheWay" as const,
};

describe("mergeDuplicateFolders", () => {
  it("asks for a preview and reads the preview's field", async () => {
    let asked = "";
    const api = client((req) => {
      asked = req.url;
      return { wouldMerge: [fold], residuals: [residual] };
    });

    const outcome = await mergeDuplicateFolders(api, "acme", true);

    expect(asked).toContain("/workspace/merge-duplicate-folders?dry_run=true");
    expect(outcome.folders).toEqual([fold]);
    expect(outcome.residuals).toEqual([residual]);
  });

  it("reads only the field it asked for, so a preview can never read as a merge", async () => {
    // The host and this caller disagreeing about `dryRun` is the case that
    // matters: a preview rendering someone else's `merged` list would tell the
    // operator their tree had been changed when nothing was touched.
    const api = client(() => ({ merged: [fold], residuals: [] }));

    const outcome = await mergeDuplicateFolders(api, "acme", true);

    expect(outcome.folders).toEqual([]);
  });

  it("keeps the residuals even when nothing merged", async () => {
    // The half of the answer that says the tree is NOT repaired. Defaulting it
    // away would turn "two documents on one path" into silence.
    const api = client(() => ({ merged: [], residuals: [residual] }));

    const outcome = await mergeDuplicateFolders(api, "acme", false);

    expect(outcome).toEqual({ folders: [], residuals: [residual] });
  });

  it("survives a host that omits the lists entirely", async () => {
    const api = client(() => ({}));

    expect(await mergeDuplicateFolders(api, "acme", false)).toEqual({
      folders: [],
      residuals: [],
    });
  });
});

describe("residualReason", () => {
  it("tells the operator what to do, not what the host called it", () => {
    for (const cause of ["fileSharesTheName", "fileInTheWay", "treeMovedOn", "danglingParent"] as const) {
      const reason = residualReason(cause);
      expect(reason).not.toContain(cause);
      expect(reason.length).toBeGreaterThan(20);
    }
  });
});

describe("applyRepair", () => {
  // A raced tree is one an agent's publish walk made, so the fixtures carry an
  // agent origin rather than the operator default. `applyRepair` never reads it —
  // it is here because `FsNode` requires it, and a faithful fixture beats a
  // convenient one.
  const BY_AGENT = { kind: "agent", id: "cmo" } as const;
  const tree: FsNode[] = [
    {
      id: "keep",
      name: "reports",
      kind: "folder",
      parentId: null,
      updatedAt: 1,
      createdBy: BY_AGENT,
      updatedBy: BY_AGENT,
    },
    {
      id: "dupe",
      name: "reports",
      kind: "folder",
      parentId: null,
      updatedAt: 2,
      createdBy: BY_AGENT,
      updatedBy: BY_AGENT,
    },
    {
      id: "note",
      name: "q2.md",
      kind: "file",
      parentId: "dupe",
      updatedAt: 3,
      createdBy: BY_AGENT,
      updatedBy: BY_AGENT,
    },
    {
      id: "rival",
      name: "summary.md",
      kind: "file",
      parentId: "dupe",
      updatedAt: 4,
      createdBy: BY_AGENT,
      updatedBy: BY_AGENT,
    },
  ];

  it("relocates what moved and drops the folder that went", () => {
    const outcome: RepairOutcome = {
      folders: [{ ...fold, moved: [{ id: "note", name: "q2.md" }, { id: "rival", name: "summary.md" }] }],
      residuals: [],
    };

    const after = applyRepair(tree, outcome);

    expect(after.map((n) => [n.id, n.parentId])).toEqual([
      ["keep", null],
      ["note", "keep"],
      ["rival", "keep"],
    ]);
  });

  it("keeps a folder the host did not remove, and everything still in it", () => {
    const outcome: RepairOutcome = { folders: [{ ...fold, removed: false }], residuals: [residual] };

    const after = applyRepair(tree, outcome);

    expect(after.map((n) => [n.id, n.parentId])).toEqual([
      ["keep", null],
      ["dupe", null],
      ["note", "keep"],
      ["rival", "dupe"],
    ]);
  });

  it("takes a stale child down with the folder that was removed", () => {
    // The host only deletes a folder it has just proved empty — but this copy of
    // the tree can be older than that proof, and a row left hanging off a parent
    // that no longer exists renders nowhere and is impossible to delete.
    const stale: FsNode[] = [
      ...tree,
      {
        id: "ghost",
        name: "old.md",
        kind: "file",
        parentId: "dupe",
        updatedAt: 5,
        createdBy: BY_AGENT,
        updatedBy: BY_AGENT,
      },
    ];
    const outcome: RepairOutcome = {
      folders: [{ ...fold, moved: [{ id: "note", name: "q2.md" }], removed: true }],
      residuals: [],
    };

    const after = applyRepair(stale, outcome);

    expect(after.map((n) => n.id)).toEqual(["keep", "note"]);
  });

  it("changes nothing when the repair did nothing", () => {
    expect(applyRepair(tree, { folders: [], residuals: [] })).toEqual(tree);
  });
});
