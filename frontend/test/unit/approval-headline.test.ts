// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { ApprovalSummary, GrantScope, Verdict } from "@/api/types";
import { approvalAction, approvalHeadline, approvalSummary } from "@/lib/language";
import { ApprovalCard } from "@/views/ApprovalsView";
import { ApprovalRow } from "@/views/chat/ApprovalRow";

/**
 * Defect B-068: the Approvals queue named the tool category and not the
 * request.
 *
 * A teammate's sign-off request parks as `request_approval` — one effect kind
 * whatever is being asked — so the kind cannot carry the question, and the
 * queue's headline read "Use one of its tools" over a card about approving a
 * pricing range. The request's own title was directly underneath it, in a
 * monospace block printed key-by-key (`title:`, `question:`, `context:`) and
 * clipped mid-word until "Show everything" was pressed. The chat card got this
 * right, which is the tell: two surfaces composing one card in two places, and
 * only one of them read the payload.
 *
 * Both halves are asserted at the DOM, because the claim is about what an
 * operator scanning a queue actually reads.
 */

const T0 = new Date("2026-03-02T10:00:00Z").getTime();

/** The reported card: a teammate asking for sign-off on a pricing range. */
const PRICING: ApprovalSummary = {
  id: "a1",
  kind: "request_approval",
  amount_usd: null,
  at_millis: T0,
  agent: "priya",
  thread: "dm:priya",
  payload: {
    title: "Approve autumn range pricing",
    question: "Approve the autumn range at GBP24 a candle and GBP85 for the set?",
    context: "Filed at agents/priya/autumn-range-pricing-signoff.md",
  },
};

let container: HTMLDivElement;
let root: Root;

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

async function renderQueueCard(approval: ApprovalSummary) {
  await act(async () => {
    root.render(
      createElement(ApprovalCard, {
        approval,
        now: T0 + 60_000,
        askerNames: new Map([["priya", "Priya"]]),
        deciding: null,
        batchIndex: 1,
        batchTotal: 1,
        onDecide: (_verdict: Verdict, _scope: GrantScope) => {},
      }),
    );
  });
}

async function renderChatCard(approval: ApprovalSummary) {
  await act(async () => {
    root.render(
      createElement(ApprovalRow, {
        approvals: [approval],
        now: T0 + 60_000,
        askerNames: new Map([["priya", "Priya"]]),
        variant: "compact" as const,
        deciding: new Map(),
        decided: {},
        failed: {},
        onDecide: () => {},
      }),
    );
  });
}

/** The card's headline paragraph — the first line an operator reads. */
function headlineText(): string {
  const p = container.querySelector("p");
  if (!p) throw new Error("no headline rendered");
  return p.textContent ?? "";
}

describe("what an approval card is named after (B-068)", () => {
  it("names the request on the Approvals page, not only its category", async () => {
    await renderQueueCard(PRICING);
    const headline = headlineText();
    expect(headline).toContain("Approve autumn range pricing");
    // The category is still there — it says what kind of decision this is —
    // but it is no longer the whole of it.
    expect(headline).toContain(approvalAction(PRICING));
    expect(headline).not.toBe(approvalAction(PRICING));
  });

  it("gives the queue and the conversation the same words for one card", async () => {
    await renderQueueCard(PRICING);
    const queue = headlineText();
    act(() => root.unmount());
    container.remove();
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
    await renderChatCard(PRICING);
    expect(headlineText()).toBe(queue);
  });

  it("leads with the title, whatever order the payload arrived in", () => {
    // The tool sends its arguments verbatim, so the key order is the model's.
    // Without a declared order the three ranked equally and whichever came
    // first led the card.
    const reordered: ApprovalSummary = {
      ...PRICING,
      payload: {
        context: "Filed at agents/priya/autumn-range-pricing-signoff.md",
        question: "Approve the autumn range at GBP24 a candle?",
        title: "Approve autumn range pricing",
      },
    };
    expect(approvalHeadline(reordered)).toBe(
      "Use one of its tools — Approve autumn range pricing",
    );
  });

  it("carries the request into the lines the console speaks out loud", () => {
    // "Extended the deadline for Use one of its tools." was a sentence this
    // product said. Every toast and live-region announcement on the Approvals
    // page is built from `approvalSummary`.
    expect(approvalSummary(PRICING)).toContain("Approve autumn range pricing");
  });

  it("keeps saying what kind of decision it is when the payload is withheld", async () => {
    // #618: "nothing to show" and "not shown to you" must not look alike, and
    // a member who cannot read the request still has to know it is a sign-off.
    const hidden: ApprovalSummary = {
      ...PRICING,
      payload: undefined,
      contents_hidden: true,
    };
    expect(approvalHeadline(hidden)).toBe(approvalAction(hidden));
    await renderQueueCard(hidden);
    expect(headlineText()).toContain(approvalAction(hidden));
  });
});
