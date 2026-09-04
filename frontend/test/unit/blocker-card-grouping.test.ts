import { describe, expect, it } from "vitest";

import type { ApprovalSummary } from "@/api/types";
import { approvalBatchKey, buildTimelineItems } from "@/views/chat/model";

/**
 * A blocker folds by its root cause (#1862): every card stalled on one broken
 * integration is one question, even across turns a batch would keep apart.
 */

const T0 = new Date("2026-03-02T10:00:00Z").getTime();

function blocker(over: Partial<ApprovalSummary> & Pick<ApprovalSummary, "id">): ApprovalSummary {
  return {
    kind: "blocker.infrastructure",
    amount_usd: null,
    at_millis: T0,
    agent: "eng",
    thread: "eng",
    ...over,
  };
}

describe("approvalBatchKey for blockers", () => {
  it("folds blockers sharing a group_key into one key, across different batches", () => {
    const a = blocker({ id: "a", batch: "turn-1", group_key: "connection:slack" });
    const b = blocker({ id: "b", batch: "turn-2", group_key: "connection:slack" });
    expect(approvalBatchKey(a)).toBe(approvalBatchKey(b));
  });

  it("keeps distinct connections apart", () => {
    const slack = blocker({ id: "a", group_key: "connection:slack" });
    const notion = blocker({ id: "b", group_key: "connection:notion" });
    expect(approvalBatchKey(slack)).not.toBe(approvalBatchKey(notion));
  });

  it("groups an ungrouped blocker alone", () => {
    const a = blocker({ id: "a" });
    const b = blocker({ id: "b" });
    expect(approvalBatchKey(a)).not.toBe(approvalBatchKey(b));
  });

  it("leaves an ordinary approval on its batch", () => {
    const a: ApprovalSummary = { ...blocker({ id: "a" }), kind: "web_fetch", batch: "turn-9" };
    expect(approvalBatchKey(a)).toBe("turn-9");
  });

  // A blocker carries `batch` too — the host sets it on every parked
  // approval alike (issue #842) — but folding by it the way an ordinary
  // gated call does hid `ApprovalRow`'s only answer box whenever a turn
  // happened to park a blocker alongside something else, and left the
  // batch's "Approve all" free to resolve the blocker as a wordless retry.
  it("never inherits an ordinary call's turn batch, even parked in the very same turn", () => {
    const a = blocker({ id: "a", batch: "turn-5" });
    const b = blocker({ id: "b", batch: "turn-5" });
    expect(approvalBatchKey(a)).not.toBe(approvalBatchKey(b));
    expect(approvalBatchKey(a)).toBe("solo:a");
  });

  it("splits a blocker out of a batch it shares with an ordinary gated call", () => {
    const question = blocker({ id: "b", batch: "turn-7" });
    const gatedCall: ApprovalSummary = {
      ...blocker({ id: "g" }),
      kind: "web_fetch",
      batch: "turn-7",
    };
    expect(approvalBatchKey(question)).not.toBe(approvalBatchKey(gatedCall));
    // The gated call keeps behaving exactly as before — only the blocker splits out.
    expect(approvalBatchKey(gatedCall)).toBe("turn-7");
  });
});

describe("buildTimelineItems groups connection blockers into one card", () => {
  it("renders one card for three parks on one broken connection", () => {
    const items = buildTimelineItems(
      [],
      [
        blocker({ id: "a", group_key: "connection:slack" }),
        blocker({ id: "b", group_key: "connection:slack" }),
        blocker({ id: "c", group_key: "connection:slack" }),
      ],
    );
    const cards = items.filter((i) => i.kind === "approval");
    expect(cards).toHaveLength(1);
    expect(cards[0].kind === "approval" && cards[0].approvals).toHaveLength(3);
  });
});
