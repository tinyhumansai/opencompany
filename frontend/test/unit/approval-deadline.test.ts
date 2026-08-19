// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { ApprovalSummary } from "@/api/types";
import { ApprovalMeta } from "@/components/approval-card";
import { approvalStatusLabel, untilLabel } from "@/lib/language";

/**
 * Issue #971: nothing may vanish unannounced.
 *
 * Requests now age out on their own, so the card has to say when. This suite is
 * mostly pure — `untilLabel` and the status wording — with one rendering
 * exception, earned the same way `approval-batch-card` earns its: the claim is
 * about what an operator reads on the card, and "the deadline is absent when
 * the host does not report one" is a claim about the rendered line rather than
 * about a function's return value.
 */

const T0 = new Date("2026-03-02T10:00:00Z").getTime();
const HOUR = 60 * 60 * 1000;

function approval(overrides: Partial<ApprovalSummary> = {}): ApprovalSummary {
  return {
    id: "a1",
    kind: "web_fetch",
    amount_usd: null,
    at_millis: T0,
    agent: "seo",
    payload: { url: "https://espn.com/nba" },
    ...overrides,
  };
}

let container: HTMLDivElement;
let root: Root;

async function render(a: ApprovalSummary, now: number) {
  await act(async () => {
    root.render(
      createElement(ApprovalMeta, {
        approval: a,
        now,
        askerNames: new Map([["seo", "SEO Specialist"]]),
      }),
    );
  });
}

beforeEach(() => {
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("the deadline on an approval card", () => {
  it("renders beside how long the request has already waited", async () => {
    await render(approval({ expires_at_millis: T0 + 24 * HOUR }), T0 + 2 * HOUR);
    const text = container.textContent ?? "";
    // Both halves of "is this still worth deciding?".
    expect(text).toContain("2h ago");
    expect(text).toContain("declined in 22h");
  });

  it("says nothing when the host reports no deadline", async () => {
    // An absent `expires_at_millis` means the HOST has no deadlines, not that
    // this card has none. The console must not invent one: an operator who
    // acted on a computed "in 3h" would be refused by a gate that never agreed
    // to it.
    await render(approval(), T0 + 2 * HOUR);
    const text = container.textContent ?? "";
    expect(text).toContain("2h ago");
    expect(text).not.toContain("declined");
    expect(text).not.toContain("in ");
  });

  it("leaves the rest of the card untouched either way", async () => {
    await render(approval({ expires_at_millis: T0 + 24 * HOUR }), T0 + 2 * HOUR);
    const withDeadline = container.textContent ?? "";
    await render(approval(), T0 + 2 * HOUR);
    const without = container.textContent ?? "";
    // The deadline is additive: strip it and the two lines agree.
    expect(withDeadline.replace("·declined in 22h", "").replace("declined in 22h", "")).toBe(
      without,
    );
    expect(without).toContain("SEO Specialist");
  });
});

/**
 * `untilLabel` moved out of `ApprovalsView` so the grant rows and the approval
 * cards share one implementation (#971). These pin the buckets across the move
 * — a shared helper that quietly rounds differently would show the same
 * deadline two ways on one screen, which is worse than the duplication was.
 */
describe("untilLabel, after the move into the language layer", () => {
  it("keeps its minute / hour / day buckets", () => {
    expect(untilLabel(T0 + 42 * 60_000, T0)).toBe("in 42m");
    expect(untilLabel(T0 + 59 * 60_000, T0)).toBe("in 59m");
    expect(untilLabel(T0 + 6 * HOUR, T0)).toBe("in 6h");
    expect(untilLabel(T0 + 23 * HOUR, T0)).toBe("in 23h");
    expect(untilLabel(T0 + 3 * 24 * HOUR, T0)).toBe("in 3d");
  });

  it("clamps a passed deadline at zero rather than counting backwards", () => {
    expect(untilLabel(T0 - HOUR, T0)).toBe("in 0m");
  });
});

/**
 * The `expired` status has been declared since #333 and was unreachable until
 * approvals started ageing out for every company. It needs words before a
 * surface reaches it, or an operator is shown a runtime identifier.
 */
describe("what became of an approval, in plain language", () => {
  it("tells the no an operator made from the one the deadline made", () => {
    expect(approvalStatusLabel("denied")).toBe("Declined");
    expect(approvalStatusLabel("expired")).toBe("Expired — nobody decided in time");
    expect(approvalStatusLabel("approved")).toBe("Approved");
    expect(approvalStatusLabel("pending")).toBe("Waiting on you");
  });
});
