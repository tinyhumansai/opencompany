import { describe, expect, it } from "vitest";

import type { WorkflowDraftFromDescription, WorkflowGraph } from "@/api/workflows";
import { draftBanners } from "@/lib/workflow-draft";

/**
 * The create-time copilot's banner reducer (issue #813).
 *
 * The dialog only wires these three slots into `<Alert>`s, so the branch logic —
 * a drafted graph shows a summary and any host corrections; a one-off shows a
 * reason — is proved here rather than through a render.
 */

/** A minimal graph, enough to make a draft "automatable with a workflow". */
const GRAPH: WorkflowGraph = {
  id: "weekly-digest",
  name: "Weekly digest",
  version: null,
  nodes: [],
  edges: [],
};

function drafted(over: Partial<WorkflowDraftFromDescription>): WorkflowDraftFromDescription {
  return { automatable: true, ...over };
}

describe("draftBanners", () => {
  it("builds a summary line and passes host notes through for a drafted graph", () => {
    const banners = draftBanners(
      drafted({
        workflow: GRAPH,
        summary: "email the weekly digest",
        notes: ["Assigned the “Write” step to teammate `qa_engineer`."],
      }),
    );
    expect(banners.summary).toBe(
      "Drafted: email the weekly digest — review below, then Create.",
    );
    expect(banners.notes).toEqual([
      "Assigned the “Write” step to teammate `qa_engineer`.",
    ]);
    expect(banners.reason).toBeNull();
  });

  it("falls back to a bare summary line and no notes when the host sent none", () => {
    const banners = draftBanners(drafted({ workflow: GRAPH }));
    expect(banners.summary).toBe("Drafted — review below, then Create.");
    expect(banners.notes).toEqual([]);
    expect(banners.reason).toBeNull();
  });

  it("drops blank notes rather than rendering empty bullets", () => {
    const banners = draftBanners(
      drafted({ workflow: GRAPH, summary: "x", notes: ["  ", "kept"] }),
    );
    expect(banners.notes).toEqual(["kept"]);
  });

  it("surfaces the reason for a not-automatable answer, with a default", () => {
    const withReason = draftBanners({ automatable: false, reason: "this only runs once" });
    expect(withReason.summary).toBeNull();
    expect(withReason.notes).toEqual([]);
    expect(withReason.reason).toBe("this only runs once");

    const noReason = draftBanners({ automatable: false });
    expect(noReason.reason).toBe("This is better done once than built into a workflow.");
  });
});
