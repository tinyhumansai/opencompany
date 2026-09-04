import { describe, expect, it } from "vitest";

import { decideApprovalToastLine } from "@/components/app-shell";

/**
 * Defect B-070's app-shell gap: `decideApproval` is the shared resolve
 * handler behind four wiring sites — a chat inline row via `ApprovalRow`, the
 * task board, task detail, and the ledgers board — and it always said
 * "Approved — the teammate is picking it up now" for an approve, regardless
 * of what the approval actually was.
 *
 * `ApprovalsView`'s own standalone queue page already checked
 * `isAnswerOnlyBlocker` before choosing that sentence (`ApprovalsView.tsx`).
 * This shared handler did not, so answering a blocker with no card and no
 * workflow run behind it — one truly nothing resumes for — from chat, the
 * board, task detail or ledgers showed the false "picking it up now" promise.
 * Nothing caught it because no test called `decideApproval` with an
 * answer-only blocker at all.
 */
describe("the toast line decideApproval leaves behind", () => {
  it("says the answer is recorded, not that work is starting, for a true answer-only blocker", () => {
    // Unlinked, and no workflow run behind it — a question asked
    // mid-conversation, with nothing to re-enter.
    const unlinkedQuestion = { kind: "blocker.information", task: { link: "unlinked" } } as const;
    expect(decideApprovalToastLine(unlinkedQuestion, "approve", 0)).toBe(
      "Answer recorded — this doesn't restart anything on its own",
    );
  });

  it("still promises the resume for a blocker raised from a card", () => {
    const cardQuestion = {
      kind: "blocker.information",
      task: { link: "task", id: "t-1" },
      agent: "eng",
    } as const;
    expect(decideApprovalToastLine(cardQuestion, "approve", 0)).toBe(
      "Approved — the teammate is picking it up now",
    );
  });

  it("still promises the resume for a workflow node's blocker, which really does re-enter its node", () => {
    // Unlinked like the true answer-only case above, but carrying a
    // `workflow_run_id` — `resume_node_blocker` re-dispatches this node from
    // the run's own trigger input (issues #1863, #2005), so it must not be
    // demoted to "recorded" the way the truly answer-only case is.
    const workflowNode = {
      kind: "blocker.information",
      task: { link: "unlinked" },
      workflow_run_id: "run-1",
      agent: "eng",
    } as const;
    expect(decideApprovalToastLine(workflowNode, "approve", 0)).toBe(
      "Approved — the teammate is picking it up now",
    );
  });

  it("promises the resume for an ordinary gated call, which is not a blocker at all", () => {
    const gatedCall = { kind: "payment.send", task: { link: "unlinked" }, agent: "eng" } as const;
    expect(decideApprovalToastLine(gatedCall, "approve", 0)).toBe(
      "Approved — the teammate is picking it up now",
    );
  });

  it("names what is still owed on an approve that did not release the turn", () => {
    const gatedCall = { kind: "payment.send", task: { link: "unlinked" }, agent: "eng" } as const;
    expect(decideApprovalToastLine(gatedCall, "approve", 2)).toBe(
      "Approved — waiting on 2 more sign-offs before the teammate continues",
    );
  });

  /**
   * CodeRabbit review, PR #2054: a native `workflow.approve` gate carries a
   * `workflow_run_id` (so it is never answer-only) but no `agent` — the
   * runtime performs that gate itself, and `ApprovalsView`'s own `decide`
   * already branches on `agent` for exactly this reason (`a.agent ?
   * approvedLine(...) : approvedByRuntimeLine(...)`). This shared handler —
   * behind the chat inline row, the task board, task detail and the ledgers
   * board — did not, and said "the teammate is picking it up now" for a gate
   * no teammate is involved in.
   */
  it("says the runtime carries it out, not that a teammate does, for an agentless gate", () => {
    const runtimeGate = {
      kind: "workflow.approve",
      task: { link: "unlinked" },
      workflow_run_id: "run-1",
      agent: null,
    } as const;
    expect(decideApprovalToastLine(runtimeGate, "approve", 0)).toBe(
      "Approved — carrying it out now",
    );
  });

  it("names what is still owed on an agentless gate too, without naming a teammate", () => {
    const runtimeGate = {
      kind: "workflow.approve",
      task: { link: "unlinked" },
      workflow_run_id: "run-1",
      agent: null,
    } as const;
    expect(decideApprovalToastLine(runtimeGate, "approve", 2)).toBe(
      "Approved — waiting on 2 more sign-offs before it runs",
    );
  });

  it("never claims a resume for a decline", () => {
    const unlinkedQuestion = { kind: "blocker.information", task: { link: "unlinked" } } as const;
    const cardQuestion = {
      kind: "blocker.information",
      task: { link: "task", id: "t-1" },
      agent: "eng",
    } as const;
    expect(decideApprovalToastLine(unlinkedQuestion, "deny", 0)).toBe("Declined — recorded.");
    expect(decideApprovalToastLine(cardQuestion, "deny", 0)).toBe("Declined — recorded.");
  });
});
