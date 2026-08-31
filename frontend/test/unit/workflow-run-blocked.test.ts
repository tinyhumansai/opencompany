import { describe, expect, it } from "vitest";

import type { WorkflowGraph, WorkflowRunOutcome } from "@/api/workflows";
import type { CompanyStreamEvent } from "@/hooks/use-events";
import {
  failedNodeOf,
  foldLiveRun,
  statesFromRun,
} from "@/views/workflows/graph";
import {
  isBlocked,
  parkedApprovalCount,
  runTone,
} from "@/views/workflows/run-health";

/**
 * How the console reads a run that stopped because a step is waiting on a
 * person (issues #881, #880).
 *
 * The shape is what makes this worth pinning: a blocked run carries **no**
 * error, is **not** cancelled, is **not** running, and routed **no** report — so
 * every existing check fell through to the green "ok" and told the operator
 * that a pipeline which delivered nothing had succeeded. Each test below is one
 * of those fall-throughs.
 */

/** A settled run with nothing wrong with it — the base every case varies. */
function run(over: Partial<WorkflowRunOutcome> = {}): WorkflowRunOutcome {
  return {
    seq: 1,
    atMillis: 1_700_000_000_000,
    workflowId: "feature_pipeline",
    scheduled: false,
    runId: "run-1",
    deliveries: [],
    pendingApprovals: [],
    ...over,
  };
}

/** A run whose first step blocked on a parked `publish_artifact`. */
function blockedRun(
  over: Partial<WorkflowRunOutcome> = {},
): WorkflowRunOutcome {
  return run({
    pendingApprovals: ["spec"],
    nodes: [{ nodeId: "spec", status: "blocked", elapsedMs: 42_000 }],
    blockedNodes: [
      { nodeId: "spec", tools: ["publish_artifact"], approvalIds: ["appr-1"] },
    ],
    approvals: [
      {
        nodeId: "spec",
        tool: "publish_artifact",
        outcome: "parked",
        approvalId: "appr-1",
      },
    ],
    ...over,
  });
}

describe("a blocked run's health reading", () => {
  it("is not green — the fall-through this whole change exists to close", () => {
    // Before #881 this exact object scored `ok`: no error, not cancelled, not
    // running, no undelivered reports.
    expect(runTone(blockedRun()).label).toBe("blocked");
    expect(runTone(blockedRun()).dot).toContain("status-blocked");
  });

  it("is not red either — nothing failed, and a fix is one click away", () => {
    expect(runTone(blockedRun()).dot).not.toContain("status-failed");
  });

  it("leaves an ordinary run's reading exactly as it was", () => {
    expect(runTone(run()).label).toBe("ok");
    expect(isBlocked(run())).toBe(false);
    // A host predating #881 sends no `blockedNodes` key at all, and absent must
    // read as "nothing blocked" rather than as unknown.
    expect(isBlocked(run({ blockedNodes: undefined }))).toBe(false);
  });

  it("still reports a genuine failure as a failure", () => {
    // The guard that matters in the other direction: blocked must never
    // outrank an error, or a real break hides behind an approval.
    expect(runTone(run({ error: "boom", blockedNodes: [] })).label).toBe(
      "failed",
    );
  });
});

describe("what the run parked", () => {
  it("counts the receipt, not the outstanding cards", () => {
    // Two parked calls under one blocked node: the count comes off the
    // per-call receipts, which is what the console's wording is built from.
    const two = blockedRun({
      approvals: [
        {
          nodeId: "spec",
          tool: "publish_artifact",
          outcome: "parked",
          approvalId: "a1",
        },
        {
          nodeId: "spec",
          tool: "publish_artifact",
          outcome: "parked",
          approvalId: "a2",
        },
      ],
    });
    expect(parkedApprovalCount(two)).toBe(2);
  });

  it("counts a park that FAILED too — those are the loudest rows", () => {
    // A failed park means the operator will never be asked about that call. It
    // must not silently vanish from the run's account of itself.
    const failed = blockedRun({
      blockedNodes: [
        { nodeId: "spec", tools: ["publish_artifact"], unparkable: 1 },
      ],
      approvals: [
        { nodeId: "spec", tool: "publish_artifact", outcome: "parkFailed" },
      ],
    });
    expect(parkedApprovalCount(failed)).toBe(1);
    expect(isBlocked(failed)).toBe(true);
  });

  it("reports zero for a run that parked nothing", () => {
    expect(parkedApprovalCount(run())).toBe(0);
  });
});

describe("a blocked node on the canvas", () => {
  it("overlays as blocked, not as a failure", () => {
    expect(statesFromRun(blockedRun())).toEqual({ spec: "blocked" });
  });

  it("is not named as the node the run failed at", () => {
    // `failureLocation` reads this, and it is only ever asked about a run with
    // an `error`. A blocked run has none — returning its node here would print
    // "it failed at spec" about a step that is merely waiting.
    expect(failedNodeOf(blockedRun())).toBeNull();
    expect(
      failedNodeOf(
        run({ nodes: [{ nodeId: "send", status: "error", elapsedMs: 1 }] }),
      ),
    ).toBe("send");
  });

  it("paints blocked from a live frame, and still fails safe on an unknown status", () => {
    const graph: WorkflowGraph = {
      id: "feature_pipeline",
      name: "Feature pipeline",
      version: null,
      nodes: [
        { id: "start", kind: "trigger", name: "Start" },
        { id: "spec", kind: "agent", name: "Spec", agent: "ceo" },
        { id: "done", kind: "output", name: "Done" },
      ],
      edges: [
        { from: "start", to: "spec" },
        { from: "spec", to: "done" },
      ],
    };
    const frames = (status: string): CompanyStreamEvent[] => [
      {
        type: "workflow_run_started",
        seq: 1,
        atMillis: 1,
        workflowId: "feature_pipeline",
        runId: "run-1",
        scheduled: false,
        startedBy: "operator",
      },
      {
        type: "workflow_node_finished",
        seq: 2,
        atMillis: 2,
        workflowId: "feature_pipeline",
        runId: "run-1",
        nodeId: "spec",
        status,
        elapsedMs: 42,
      },
    ];

    expect(
      foldLiveRun(frames("blocked"), "feature_pipeline", graph)?.states.spec,
    ).toBe("blocked");
    // A status word a newer host invents must not be promoted to the gentler
    // amber any more than it may be painted green.
    expect(
      foldLiveRun(frames("wat"), "feature_pipeline", graph)?.states.spec,
    ).toBe("error");
  });
});
