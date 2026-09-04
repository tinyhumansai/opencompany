// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { WorkflowRunOutcome } from "@/api/workflows";
import { WorkflowStep } from "@/onboarding/WorkflowStep";

/**
 * Codex review on #2046: `blocked` and `awaiting-approval` both render the
 * gate's "waiting-on-you" kind (same button, same unticked step), but three
 * distinct things can produce that pairing and only one of them means
 * deciding actually continues the run:
 *
 * - A live gate approval (`pendingApprovals`) — deciding it really does
 *   resume the run.
 * - `blocked`: `WorkflowRunOutcome.blockedNodes` (frontend/src/api/workflows.ts)
 *   says the agent node is not re-enterable — deciding the card does not
 *   continue the run, the operator still has to run the workflow again.
 * - `awaiting-approval` purely from a pending DELIVERY (`deliveries[].status
 *   === "pending"`): a snapshot taken once the run already finished: deciding
 *   it only sends the report, and the run's own outcome is unaffected —
 *   `DeliveryReport` carries no id, so there is nothing for Approvals to link
 *   to either (second Codex finding on #2046).
 *
 * The copy must not promise automatic continuation for a run it will never be
 * true for, in either of the last two cases.
 */

const run = (over: Partial<WorkflowRunOutcome> = {}): WorkflowRunOutcome => ({
  seq: 1,
  atMillis: 1_700_000_000_000,
  workflowId: "research-request",
  scheduled: false,
  deliveries: [],
  pendingApprovals: [],
  ...over,
});

function fakeClient(runs: WorkflowRunOutcome[]): OpenCompanyClient {
  return {
    scopeFor: () => "/api/v1/company",
    get: async (path: string) => {
      if (path.includes("/workflows/runs")) return { runs, hasMore: false };
      if (path.includes("/workflows")) return [];
      throw new Error(`unexpected path: ${path}`);
    },
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

async function render(runs: WorkflowRunOutcome[]) {
  await act(async () => {
    root.render(
      createElement(WorkflowStep, {
        client: fakeClient(runs),
        company: null,
        onOpenWorkflows: () => {},
        onOpenApprovals: () => {},
      }),
    );
  });
  // Flush the pending microtask the effect's Promise.allSettled leaves behind.
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
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

describe("WorkflowStep's wording for a run waiting on a person", () => {
  it("promises the run carries on for awaiting-approval", async () => {
    await render([run({ verdict: "awaiting-approval", pendingApprovals: ["ap-1"] })]);
    const text = container.querySelector('[data-testid="gate-workflow-waiting"]')?.textContent;
    expect(text, "awaiting-approval must render the waiting testid").toBeTruthy();
    expect(text).toContain("the run carries on");
  });

  it("does NOT promise automatic continuation for a blocked run", async () => {
    await render([
      run({
        verdict: "blocked",
        blockedNodes: [{ nodeId: "escalate_to_human", tools: ["ask"], approvalIds: ["ap-7"] }],
      }),
    ]);
    const text = container.querySelector('[data-testid="gate-workflow-blocked"]')?.textContent;
    expect(text, "a blocked run must render the dedicated blocked testid").toBeTruthy();
    expect(text).not.toContain("the run carries on");
    expect(text).toContain("run it again");
  });

  it("does NOT promise automatic continuation for a delivery-only approval", async () => {
    // No `pendingApprovals`, no `blockedNodes` — only a delivery report parked
    // for send. `gateApprovalTargets` (used both for the Approvals button and
    // by `WorkflowStep` itself) correctly has nothing to offer here.
    await render([
      run({
        verdict: "awaiting-approval",
        deliveries: [{ node: "n1", kind: "email", status: "pending", detail: "queued" }],
      }),
    ]);
    const text = container.querySelector(
      '[data-testid="gate-workflow-awaiting-delivery"]',
    )?.textContent;
    expect(
      text,
      "a delivery-only awaiting-approval must render its own dedicated testid",
    ).toBeTruthy();
    expect(text).not.toContain("the run carries on");
    expect(text).toContain("run it again");
    // Nothing decidable via Approvals for this card — the button must not render.
    expect(container.querySelector('[data-testid="gate-workflow-open-approvals"]')).toBeNull();
  });

  it("does NOT invite deciding a blocked run whose gated calls never queued at all", async () => {
    // Codex review on #2046: `blockedNodes[].approvalIds` is ABSENT (not just
    // stranded) when every one of a node's gated calls failed to park — the
    // "unparkable" shape `RunHistoryPanel` already special-cases. There is
    // nothing to decide, so the copy must not say "decide the approval".
    await render([
      run({
        verdict: "blocked",
        blockedNodes: [{ nodeId: "escalate_to_human", tools: ["ask"] }],
      }),
    ]);
    const text = container.querySelector(
      '[data-testid="gate-workflow-blocked-unparkable"]',
    )?.textContent;
    expect(
      text,
      "an unparkable blocked run must render its own dedicated testid",
    ).toBeTruthy();
    expect(text).not.toContain("Decide the approval");
    expect(text).toContain("run it again");
    expect(container.querySelector('[data-testid="gate-workflow-open-approvals"]')).toBeNull();
  });

  it("does NOT claim a pending delivery carries the run on, even alongside a live gate approval", async () => {
    // Codex review on #2046: a run can carry BOTH a live gate approval and a
    // pending delivery at once (a parallel branch already reached an output
    // node while another is still paused at a gate). `gateApprovalTargets`
    // alone can't see this — it only reads `pendingApprovals` and
    // `blockedNodes`, never `deliveries` — so the plain "the run carries on"
    // sentence would be read as covering the delivery too.
    await render([
      run({
        verdict: "awaiting-approval",
        pendingApprovals: ["ap-1"],
        deliveries: [{ node: "n1", kind: "email", status: "pending", detail: "queued" }],
      }),
    ]);
    const text = container.querySelector(
      '[data-testid="gate-workflow-waiting-mixed"]',
    )?.textContent;
    expect(text, "a mixed gate+delivery run must render its own testid").toBeTruthy();
    expect(text).not.toContain("Decide the approval and the run carries on");
    expect(text).toContain("carries that part of the run on");
    // The gate approval itself IS decidable, so the button must still show.
    expect(container.querySelector('[data-testid="gate-workflow-open-approvals"]')).toBeTruthy();
  });

  it("treats a fully-stranded gate approval plus a pending delivery as delivery-only", async () => {
    // CodeRabbit review, PR #2046: `gateApprovalTargets` used to union
    // `pendingApprovals` in unfiltered, with no check against
    // `strandedApprovals` (unlike `blockedNodes`, which already dropped a
    // fully-stranded node's ids). Reachable specifically because the host
    // does NOT call a run outright `stranded` while it also carries a
    // pending delivery (workflow_verdict.rs) — so this run reads
    // `awaiting-approval`, not `stranded`, even though every gate card is
    // gone. Before the fix this rendered the "mixed" case and offered an
    // Approvals button pointing at nothing.
    await render([
      run({
        verdict: "awaiting-approval",
        pendingApprovals: ["ap-1"],
        strandedApprovals: 1,
        deliveries: [{ node: "n1", kind: "email", status: "pending", detail: "queued" }],
      }),
    ]);
    const text = container.querySelector(
      '[data-testid="gate-workflow-awaiting-delivery"]',
    )?.textContent;
    expect(
      text,
      "a fully-stranded gate approval must not be treated as a live target",
    ).toBeTruthy();
    expect(container.querySelector('[data-testid="gate-workflow-waiting-mixed"]')).toBeNull();
    expect(container.querySelector('[data-testid="gate-workflow-open-approvals"]')).toBeNull();
  });
});
