// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { ApprovalSummary, GrantScope, Verdict } from "@/api/types";
import { ApprovalCard } from "@/views/ApprovalsView";
import { ApprovalRow } from "@/views/chat/ApprovalRow";

/**
 * Defect B-076: what a decide button announces itself as.
 *
 * The two surfaces that render one were wrong in opposite directions. On the
 * Approvals page every button carried the whole request payload plus a raw
 * request id — "Approve: Use one of its tools — title: … — question: … — asked
 * by Priya — just this once — request 1788361285043", four hundred characters
 * of it. On the chat card the same buttons carried no `aria-label` at all, so
 * the accessible name was "Approve" with nothing to say what was being
 * approved.
 *
 * The id is the part that had no defence. #1411 appended it only when the
 * contents were **not** hidden — that is, exactly when the label already
 * carried the request's own words, the amount, the method and the batch
 * position, every one of them a discriminator a person can act on. On a hidden
 * card, where those are absent and an opaque number would be the only thing
 * telling two buttons apart, it was omitted and the composition time did the
 * job. Present only where it added nothing.
 */

const T0 = new Date("2026-03-02T10:00:00Z").getTime();

const PRICING: ApprovalSummary = {
  id: "a1",
  kind: "request_approval",
  amount_usd: null,
  at_millis: T0,
  // So the Extend button renders — it carries a composed name too, and it was
  // the one whose toast the report quotes.
  expires_at_millis: T0 + 24 * 60 * 60_000,
  agent: "priya",
  thread: "dm:priya",
  payload: {
    title: "Approve autumn range pricing",
    question: "Approve the autumn range at GBP24 a candle?",
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

/** Every button's accessible name, in DOM order. */
function names(): string[] {
  return Array.from(container.querySelectorAll("button")).map(
    (b) => b.getAttribute("aria-label") ?? b.textContent?.trim() ?? "",
  );
}

function named(verb: string): string {
  const hit = names().find((n) => n.startsWith(verb));
  if (hit == null) throw new Error(`no button named ${verb} in ${names()}`);
  return hit;
}

describe("what a decide button announces (B-076)", () => {
  it("does not read a raw request id out to a screen reader", async () => {
    await act(async () => {
      root.render(
        createElement(ApprovalCard, {
          approval: PRICING,
          now: T0 + 60_000,
          askerNames: new Map([["priya", "Priya"]]),
          deciding: null,
          batchIndex: 1,
          batchTotal: 1,
          onDecide: (_verdict: Verdict, _scope: GrantScope) => {},
        }),
      );
    });

    for (const verb of ["Approve", "Decline", "Extend the deadline"]) {
      const name = named(verb);
      expect(name).not.toContain(String(T0));
      // …and it still names the request, which is the whole reason #1411
      // composed a label rather than leaving the visible text to speak.
      expect(name).toContain("Approve autumn range pricing");
    }
  });

  it("names the request on the chat card's buttons too", async () => {
    await act(async () => {
      root.render(
        createElement(ApprovalRow, {
          approvals: [PRICING],
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

    // Before this the accessible name was the visible text alone — "Approve",
    // over a card whose subject was in a paragraph the button does not
    // reference.
    expect(named("Approve")).toContain("Approve autumn range pricing");
    expect(named("Decline")).toContain("Approve autumn range pricing");
    expect(named("Approve")).not.toContain(String(T0));
  });
});
