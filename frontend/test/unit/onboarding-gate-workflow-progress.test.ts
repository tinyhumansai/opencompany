// Bug B-004: the gate's step 3 promised "one real run proves it works", ticked
// only on `workflowRunSucceeded`, and said nothing at all when the shipped
// bundle's own first workflow parked on an approval instead. The step stays
// honestly unticked — a parked run has proven nothing, which is exactly why
// `src/company/activation.rs` routes the question through the verdict ladder —
// but the founder now gets told why, and what would finish it.

import { describe, expect, it } from "vitest";

import type { WorkflowRunOutcome } from "@/api/workflows";
import { gateApprovalTargets, gateWorkflowProgress } from "@/onboarding/workflow-progress";

const run = (over: Partial<WorkflowRunOutcome> = {}): WorkflowRunOutcome => ({
  seq: 1,
  atMillis: 1_700_000_000_000,
  workflowId: "research-request",
  scheduled: false,
  deliveries: [],
  pendingApprovals: [],
  ...over,
});

describe("gateWorkflowProgress", () => {
  it("says nothing when the company has never run anything", () => {
    expect(gateWorkflowProgress([]).kind).toBe("none");
    expect(gateWorkflowProgress(undefined).kind).toBe("none");
  });

  it("reads a run parked on an approval as waiting on the founder (the B-004 case)", () => {
    // What "Research request" actually does on its first turn: escalates to a
    // human, parks, and carries neither an error nor a cancellation.
    const parked = run({ verdict: "awaiting-approval", pendingApprovals: ["ap-1"] });
    const progress = gateWorkflowProgress([parked]);
    expect(progress.kind).toBe("waiting-on-you");
    expect(progress.verdict).toBe("awaiting-approval");
    expect(progress.run).toBe(parked);
  });

  it("reads a blocked run the same way — a person still has to act", () => {
    expect(gateWorkflowProgress([run({ verdict: "blocked" })]).kind).toBe("waiting-on-you");
  });

  it("does NOT send a stranded run to Approvals — that page would be empty", () => {
    // Issue #1189: the cards are gone, so the only thing that helps is a re-run.
    expect(gateWorkflowProgress([run({ verdict: "stranded" })]).kind).toBe("needs-rerun");
  });

  it("reads a live run as running", () => {
    expect(gateWorkflowProgress([run({ verdict: "running", running: true })]).kind).toBe(
      "running",
    );
  });

  it("reads a succeeded run as succeeded", () => {
    expect(gateWorkflowProgress([run({ verdict: "ok" })]).kind).toBe("succeeded");
  });

  it.each(["failed", "stopped", "undelivered", "degraded"] as const)(
    "reads a %s run as not finished, so the step stays honestly unticked",
    (verdict) => {
      expect(gateWorkflowProgress([run({ verdict })]).kind).toBe("did-not-finish");
    },
  );

  it("describes the most recent run, not the best one", () => {
    // The founder just pressed Run; the run they are asking about is that one.
    const newest = run({ seq: 9, verdict: "awaiting-approval", pendingApprovals: ["ap-9"] });
    const older = run({ seq: 1, verdict: "ok" });
    expect(gateWorkflowProgress([newest, older]).run?.seq).toBe(9);
    expect(gateWorkflowProgress([newest, older]).kind).toBe("waiting-on-you");
  });

  it("falls back to the shared ladder when the host sends no verdict", () => {
    // A host predating #981 sends no `verdict`; `verdictOf` derives the same
    // reading from the rows it did send, so this must not read as a clean run.
    expect(gateWorkflowProgress([run({ pendingApprovals: ["ap-1"] })]).kind).toBe(
      "waiting-on-you",
    );
  });
});

describe("gateApprovalTargets", () => {
  it("offers the run's pending approvals", () => {
    expect(gateApprovalTargets(run({ pendingApprovals: ["ap-1", "ap-2"] }))).toEqual([
      "ap-1",
      "ap-2",
    ]);
  });

  it("offers a blocked node's approvals", () => {
    const r = run({
      blockedNodes: [{ nodeId: "escalate_to_human", tools: ["ask"], approvalIds: ["ap-7"] }],
    });
    expect(gateApprovalTargets(r)).toEqual(["ap-7"]);
  });

  it("drops a node whose every card has been stranded", () => {
    // Linking these lands the founder on an empty queue — the dead end #1143
    // exists for, and the one this whole task is about one screen over.
    const r = run({
      blockedNodes: [
        { nodeId: "n1", tools: ["ask"], approvalIds: ["ap-1", "ap-2"], stranded: 2 },
      ],
    });
    expect(gateApprovalTargets(r)).toEqual([]);
  });

  it("keeps a partly-stranded node's surviving cards", () => {
    const r = run({
      blockedNodes: [
        { nodeId: "n1", tools: ["ask"], approvalIds: ["ap-1", "ap-2"], stranded: 1 },
      ],
    });
    expect(gateApprovalTargets(r)).toEqual(["ap-1", "ap-2"]);
  });

  it("does not repeat an id that is both pending and on a blocked node", () => {
    const r = run({
      pendingApprovals: ["ap-1"],
      blockedNodes: [{ nodeId: "n1", tools: ["ask"], approvalIds: ["ap-1"] }],
    });
    expect(gateApprovalTargets(r)).toEqual(["ap-1"]);
  });

  it("has nothing to offer for no run", () => {
    expect(gateApprovalTargets(undefined)).toEqual([]);
  });

  it("drops the top-level pending approvals once every one is stranded", () => {
    // CodeRabbit review, PR #2046: the blocked-node case above already
    // dropped a fully-stranded node's ids; `pendingApprovals` itself had no
    // equivalent check against `strandedApprovals`. Reachable specifically
    // when a pending DELIVERY keeps the host from calling the run outright
    // `stranded` (workflow_verdict.rs) even though every gate card IS gone.
    const r = run({
      pendingApprovals: ["ap-1", "ap-2"],
      strandedApprovals: 2,
      deliveries: [{ node: "n1", kind: "email", status: "pending", detail: "queued" }],
    });
    expect(gateApprovalTargets(r)).toEqual([]);
  });

  it("keeps the top-level pending approvals while only some are stranded", () => {
    const r = run({ pendingApprovals: ["ap-1", "ap-2"], strandedApprovals: 1 });
    expect(gateApprovalTargets(r)).toEqual(["ap-1", "ap-2"]);
  });

  it("treats an absent strandedApprovals as nothing stranded, not everything", () => {
    // A host predating issue #1189 sends no `strandedApprovals` at all —
    // absent must read as "not reconciled", never as "all of it".
    const r = run({ pendingApprovals: ["ap-1"] });
    expect(gateApprovalTargets(r)).toEqual(["ap-1"]);
  });
});
