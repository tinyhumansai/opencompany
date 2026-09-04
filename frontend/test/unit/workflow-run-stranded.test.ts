// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { WorkflowGraph, WorkflowRunOutcome } from "@/api/workflows";
import { RunHistoryPanel } from "@/views/workflows/RunHistoryPanel";
import {
  awaitingCount,
  isStranded,
  liveApprovalCount,
  liveParkedApprovalCount,
  runTone,
} from "@/views/workflows/run-health";

/**
 * Issue #1189: a run nobody can act on any more must stop being advertised as
 * approvable.
 *
 * #1150 stopped the blocked-node LIST offering a stranded approval as a
 * decision. The same run went on saying the opposite thing three more times —
 * in the summary sentence, in the header chip, and in the host's own verdict —
 * and a fourth time in the outcome chain, which is the one the marketing
 * tenant's 34 runs actually render: they carry `pendingApprovals` and NO
 * `blockedNodes`, so `isBlocked` is false and they land in the
 * `pendingApprovals` arm, whose copy is "Approve or decline it in Approvals to
 * carry the run on."
 *
 * A jsdom render rather than a pure test wherever the claim is about what the
 * drawer paints. Every test below fails on the code before this change.
 */

const GRAPH: WorkflowGraph = {
  id: "daily_sports_news",
  name: "Daily sports news",
  version: null,
  nodes: [
    { id: "fetch_bbc", kind: "agent", name: "Fetch BBC", agent: "writer" },
    { id: "fetch_espn", kind: "agent", name: "Fetch ESPN", agent: "writer" },
  ],
  edges: [],
};

/** The marketing tenant's shape: gates on `pendingApprovals`, no blocked nodes,
 * no receipts, and the host reporting every gate stranded. */
function strandedGateRun(
  over: Partial<WorkflowRunOutcome> = {},
): WorkflowRunOutcome {
  return {
    seq: 1,
    atMillis: 1_700_000_000_000,
    workflowId: "daily_sports_news",
    scheduled: true,
    runId: "run-g",
    deliveries: [],
    pendingApprovals: ["fetch_bbc", "fetch_espn", "fetch_guardian"],
    strandedApprovals: 3,
    verdict: "stranded",
    nodes: [],
    ...over,
  };
}

/** The `feature_pipeline` shape: a blocked node whose every card the queue lost. */
function strandedBlockedRun(
  over: Partial<WorkflowRunOutcome> = {},
): WorkflowRunOutcome {
  return {
    seq: 2,
    atMillis: 1_700_000_000_000,
    workflowId: "feature_pipeline",
    scheduled: false,
    runId: "run-b",
    deliveries: [],
    pendingApprovals: ["backend"],
    strandedApprovals: 1,
    verdict: "stranded",
    nodes: [{ nodeId: "backend", status: "blocked", elapsedMs: 42_000 }],
    blockedNodes: [
      {
        nodeId: "backend",
        tools: ["shell", "curl"],
        approvalIds: ["appr-1", "appr-2", "appr-3"],
        stranded: 3,
      },
    ],
    approvals: [
      { nodeId: "backend", tool: "shell", outcome: "parked", approvalId: "appr-1" },
      { nodeId: "backend", tool: "curl", outcome: "parked", approvalId: "appr-2" },
      { nodeId: "backend", tool: "shell", outcome: "parked", approvalId: "appr-3" },
    ],
    ...over,
  };
}

let container: HTMLDivElement;
let root: Root;

async function renderHistory(run: WorkflowRunOutcome) {
  await act(async () => {
    root.render(
      createElement(RunHistoryPanel, {
        runs: [run],
        graph: GRAPH,
        workflowName: "Daily sports news",
        onClose: () => {},
        selectedRunSeq: null,
        onSelectRun: () => {},
      }),
    );
  });
}

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

describe("a gate run whose cards the queue no longer holds", () => {
  it("renders the stranded line instead of pointing at Approvals", async () => {
    await renderHistory(strandedGateRun());
    const line = container.querySelector(
      '[data-testid="workflow-run-stranded"]',
    );
    expect(line).not.toBeNull();
    expect(line?.textContent).toContain("nothing here is waiting on you");
    // The sentence #1143 exists to kill, on the arm nothing had fixed.
    expect(container.textContent).not.toContain(
      "Approve or decline it in Approvals",
    );
    // And it must not have fallen into the old awaiting arm at all.
    expect(
      container.querySelector('[data-testid="workflow-run-awaiting"]'),
    ).toBeNull();
  });

  it("says nothing about WHY the cards are gone", async () => {
    await renderHistory(strandedGateRun());
    const text = container.textContent ?? "";
    // Approving a gate spawns a NEW run and records no link back, so an
    // all-approved run and a card-loss run are indistinguishable from here.
    // The copy may not claim either.
    expect(text).not.toMatch(/lost|expired|deleted|discarded/i);
    // Re-run is offered as an option, not as a remedy for a stated cause.
    expect(text).toContain("Run the workflow again if you still need it");
  });

  it("keeps the reports it routed before the gate", async () => {
    await renderHistory(
      strandedGateRun({
        deliveries: [
          {
            node: "digest",
            kind: "channel",
            target: "engineering",
            status: "sent",
            detail: "posted",
            reason: "channel-posted",
          },
        ],
      }),
    );
    expect(container.textContent).toContain("engineering");
    expect(container.textContent).not.toContain("no reports were routed");
  });

  it("does not badge the run with a pending-approval count", async () => {
    await renderHistory(strandedGateRun());
    // The row badge is the run row's own copy of the drawer's claim. A paused
    // gate files no receipt, so `parked` is 0 and the fallback used to paint
    // "3 pending approvals" in amber on the one run with nothing pending.
    expect(container.textContent).not.toContain("pending approval");
  });
});

describe("the header chip for a stranded run", () => {
  it("does not say anything is awaiting approval", () => {
    const run = strandedGateRun();
    // The count behind the chip's ` · N awaiting approval` segment.
    expect(awaitingCount(run)).toBe(0);
    expect(liveApprovalCount(run)).toBe(0);
    expect(runTone(run).label).not.toContain("awaiting approval");
    expect(runTone(run).label).toBe("stranded");
  });

  it("wears the idle dot, not the amber one", () => {
    // Amber is the console's "needs your attention, go and decide this" state,
    // which is the one claim a stranded run must not make.
    expect(runTone(strandedGateRun()).dot).toBe("bg-status-idle");
  });
});

describe("a blocked run whose every card the queue lost", () => {
  it("stops promising the run continues on its own", async () => {
    await renderHistory(strandedBlockedRun());
    const text = container.textContent ?? "";
    expect(text).not.toContain("continues this run automatically");
    expect(text).toContain("Nothing here is waiting on you any more");
    // …and it must NOT have fallen into the "the policy refused this" copy,
    // which is where a naive `parked === 0` lands. No policy refused anything.
    expect(text).not.toContain("change the policy");
    expect(text).not.toContain("could not be queued for approval");
  });

  it("still renders the blocked-node list #1143 added", async () => {
    await renderHistory(strandedBlockedRun());
    // The paragraph and the list must say the same thing, not merely avoid
    // saying opposite things — this is the pairing that broke in the issue.
    expect(
      container.querySelector(
        '[data-testid="workflow-blocked-approval-stranded"]',
      )?.textContent,
    ).toContain("cannot be continued");
    expect(
      container.querySelectorAll(
        '[data-testid="workflow-blocked-approval-link"]',
      ),
    ).toHaveLength(0);
  });

  it("keeps the Approve copy and the decide links when only SOME are gone", async () => {
    // The negative that makes the two above mean anything. A rule that fired on
    // any stranded card would satisfy them and be worse than no rule: it would
    // retire a run with a decision still sitting in the queue.
    await renderHistory(
      strandedBlockedRun({
        strandedApprovals: 0,
        verdict: "blocked",
        blockedNodes: [
          {
            nodeId: "backend",
            tools: ["shell", "curl"],
            approvalIds: ["appr-1", "appr-2", "appr-3"],
            stranded: 1,
            // Gated tool calls, none of them a question the agent raised —
            // which is the case where "approving continues the run" is true
            // (issue B-013). Stated rather than left absent: absent means "this
            // host cannot answer", and the copy degrades for that.
            blockers: 0,
          },
        ],
      }),
    );
    const text = container.textContent ?? "";
    expect(text).toContain("Decide them in Approvals.");
    expect(text).toContain("continues this run automatically");
    expect(
      container.querySelectorAll(
        '[data-testid="workflow-blocked-approval-link"]',
      ).length,
    ).toBeGreaterThan(0);
    // Two of three receipts survive, so the sentence counts two.
    expect(text).toContain("This run parked 2 approvals");
  });
});

describe("the counts the drawer branches on", () => {
  it("subtracts stranded gates from the awaiting count", () => {
    const run = strandedGateRun({ strandedApprovals: 1, verdict: "blocked" });
    // Three gates, one with no card left → two people are still being waited on.
    expect(liveApprovalCount(run)).toBe(2);
    expect(awaitingCount(run)).toBe(2);
  });

  it("counts a parked report on top of the live gates", () => {
    const run = strandedGateRun({
      strandedApprovals: 3,
      deliveries: [
        {
          node: "digest",
          kind: "channel",
          target: "engineering",
          status: "pending",
          detail: "parked",
          reason: "parked-for-approval",
        },
      ],
    });
    // Every gate is stranded, but the report is genuinely parked — somebody is
    // still being waited on, and the run is NOT stranded.
    expect(liveApprovalCount(run)).toBe(0);
    expect(awaitingCount(run)).toBe(1);
    expect(isStranded({ ...run, verdict: undefined })).toBe(false);
  });

  it("clamps the live parked count at zero", () => {
    // The receipt excludes parks that never landed while `stranded` counts only
    // ids that did, so the two lists are not identical and the subtraction must
    // not be trusted to stay ordered.
    const run = strandedBlockedRun({
      approvals: [
        { nodeId: "backend", tool: "shell", outcome: "parked", approvalId: "appr-1" },
      ],
      blockedNodes: [
        {
          nodeId: "backend",
          tools: ["shell"],
          approvalIds: ["appr-1", "appr-2", "appr-3"],
          stranded: 3,
        },
      ],
    });
    expect(liveParkedApprovalCount(run)).toBe(0);
  });

  it("reads a host that sent no stranded key as 'not reconciled'", () => {
    // Never as "nothing is stranded" — but also never as stranded. Inventing a
    // dead end would retire a run an operator can still act on.
    const old = strandedGateRun({ strandedApprovals: undefined, verdict: undefined });
    expect(isStranded(old)).toBe(false);
    expect(awaitingCount(old)).toBe(3);
  });
});
