import { describe, expect, it } from "vitest";

import type { ApprovalSummary } from "@/api/types";
import { approvalsByDeadline } from "@/lib/approval-order";

const NOW = new Date("2026-08-23T10:00:00Z").getTime();

function approval(id: string, expires_at_millis?: number | null): ApprovalSummary {
  return {
    id,
    kind: "web_fetch",
    amount_usd: null,
    at_millis: NOW,
    expires_at_millis,
  };
}

describe("approvalsByDeadline", () => {
  it("puts the most urgent enforced deadlines first", () => {
    const ordered = approvalsByDeadline([
      approval("tomorrow", NOW + 24 * 60 * 60 * 1000),
      approval("soon", NOW + 4 * 60 * 1000),
      approval("later", NOW + 2 * 60 * 60 * 1000),
    ]);

    expect(ordered.map(({ id }) => id)).toEqual(["soon", "later", "tomorrow"]);
  });

  it("leaves legacy rows without a reported deadline after dated rows", () => {
    const ordered = approvalsByDeadline([
      approval("unknown-first"),
      approval("expired", NOW - 60 * 1000),
      approval("unknown-second", null),
    ]);

    expect(ordered.map(({ id }) => id)).toEqual(["expired", "unknown-first", "unknown-second"]);
  });

  it("does not mutate the host response or reorder equal deadlines", () => {
    const hostOrder = [approval("first", NOW + 60_000), approval("second", NOW + 60_000)];

    expect(approvalsByDeadline(hostOrder).map(({ id }) => id)).toEqual(["first", "second"]);
    expect(hostOrder.map(({ id }) => id)).toEqual(["first", "second"]);
  });
});
