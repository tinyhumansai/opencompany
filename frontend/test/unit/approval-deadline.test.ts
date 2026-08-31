// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { ApprovalSummary } from "@/api/types";
import { ApprovalMeta } from "@/components/approval-card";
import { approvalDeadline, approvalStatusLabel, untilLabel } from "@/lib/language";

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
  it("links a reported desk thread back to its conversation", async () => {
    await act(async () => {
      root.render(
        createElement(ApprovalMeta, {
          approval: approval({ thread: "engineering" }),
          now: T0,
          askerNames: new Map(),
          chatChannelByThread: { engineering: "engineering" },
        }),
      );
    });

    const link = container.querySelector<HTMLAnchorElement>('a[href="#/chat/engineering"]');
    expect(link?.textContent).toContain("Open the conversation");
  });

  it("uses the resolved DM channel rather than the host thread id", async () => {
    await act(async () => {
      root.render(
        createElement(ApprovalMeta, {
          approval: approval({ thread: "agent-grace" }),
          now: T0,
          askerNames: new Map(),
          chatChannelByThread: { "agent-grace": "dm:agent-grace" },
        }),
      );
    });

    const link = container.querySelector<HTMLAnchorElement>('a[href="#/chat/dm%3Aagent-grace"]');
    expect(link?.textContent).toContain("Open the conversation");
  });

  it("links a native workflow gate to its exact run", async () => {
    await render(
      approval({
        kind: "workflow.approve",
        workflow_run_id: "run/1",
        workflow_id: "invoice sync",
      }),
      T0,
    );

    const link = container.querySelector<HTMLAnchorElement>(
      'a[href="#/workflows/invoice%20sync?run=run%2F1"]',
    );
    expect(link?.textContent).toContain("Open the run");
  });

  /**
   * Issue #1418: the run link must survive role redaction.
   *
   * A member reader's summary arrives with `payload` stripped (#618), but the
   * workflow id is a top-level field that rides through with `workflow_run_id`
   * — so the member holding up the stalled run still gets the address instead
   * of "Origin unavailable".
   */
  it("keeps the run link when role redaction stripped the payload", async () => {
    await render(
      approval({
        kind: "workflow.approve",
        workflow_run_id: "run/1",
        workflow_id: "invoice sync",
        contents_hidden: true,
        payload: undefined,
      }),
      T0,
    );

    const link = container.querySelector<HTMLAnchorElement>(
      'a[href="#/workflows/invoice%20sync?run=run%2F1"]',
    );
    expect(link?.textContent).toContain("Open the run");
    expect(container.textContent).not.toContain("Origin unavailable");
  });

  it("says when the host did not report an addressable origin", async () => {
    await render(approval({ agent: null }), T0);
    expect(container.textContent).toContain("Origin unavailable");
  });

  it("counts a resolved thread link as an origin while chat hydration is empty", async () => {
    // The shell hydrates `chatChannelByThread` separately from the card's own
    // `useApprovalThreadLinks`, so a card can resolve its "Asked in" link
    // before the shell does — or while the shell's read has failed. The origin
    // is visibly available either way, so the footer must not say otherwise.
    await act(async () => {
      root.render(
        createElement(ApprovalMeta, {
          approval: approval({ thread: "engineering" }),
          now: T0,
          askerNames: new Map(),
          thread: { channelId: "engineering", label: "#engineering" },
        }),
      );
    });

    expect(container.textContent).toContain("Asked in #engineering");
    expect(container.textContent).not.toContain("Origin unavailable");
  });

  it("renders beside how long the request has already waited", async () => {
    await render(approval({ expires_at_millis: T0 + 24 * HOUR }), T0 + 2 * HOUR);
    const text = container.textContent ?? "";
    // Both halves of "is this still worth deciding?".
    expect(text).toContain("2h ago");
    expect(text).toContain("Declines itself in 22h");
  });

  /**
   * Issue #1403. The old line read "declined in 22h" — a past participle two
   * words after a genuinely past-tense age, in identical muted grey. The whole
   * point of the new wording is that it cannot be scanned as "this was
   * declined", so the absence of the bare participle is the assertion.
   */
  it("never says a pending request was declined", async () => {
    await render(approval({ expires_at_millis: T0 + 24 * HOUR }), T0 + 2 * HOUR);
    expect(container.textContent ?? "").not.toContain("declined in");
  });

  /**
   * The tone is load-bearing rather than decorative: this is the only line on
   * the card saying the decision gets taken FOR the operator if they scroll
   * past. A card with a day left must stay quiet, or the emphasis on the one
   * with four minutes left buys nothing.
   */
  it("says a deadline inside the hour loudly, and a distant one quietly", async () => {
    await render(approval({ expires_at_millis: T0 + 24 * HOUR }), T0 + 23.5 * HOUR);
    expect(container.querySelector(".text-status-blocked-text")).not.toBeNull();

    await render(approval({ expires_at_millis: T0 + 24 * HOUR }), T0 + 2 * HOUR);
    expect(container.querySelector(".text-status-blocked-text")).toBeNull();
    expect(container.querySelector(".text-status-failed-text")).toBeNull();
  });

  it("explains a card whose deadline has already passed", async () => {
    // The host sweeps expired approvals once a minute, so a card can sit in the
    // queue briefly after its deadline. It used to render "declined in 0m" with
    // live Approve and Decline buttons beside it.
    await render(approval({ expires_at_millis: T0 + HOUR }), T0 + 2 * HOUR);
    const text = container.textContent ?? "";
    expect(text).toContain("Past its deadline");
    expect(text).not.toContain("0m");
    expect(container.querySelector(".text-status-failed-text")).not.toBeNull();
  });

  it("says nothing when the host reports no deadline", async () => {
    // An absent `expires_at_millis` means the HOST has no deadlines, not that
    // this card has none. The console must not invent one: an operator who
    // acted on a computed "in 3h" would be refused by a gate that never agreed
    // to it.
    await render(approval(), T0 + 2 * HOUR);
    const text = container.textContent ?? "";
    expect(text).toContain("2h ago");
    expect(text).not.toContain("eclines itself");
    expect(text).not.toContain("deadline");
    expect(text).not.toContain("Declines itself in");
  });

  it("leaves the rest of the card untouched either way", async () => {
    await render(approval({ expires_at_millis: T0 + 24 * HOUR }), T0 + 2 * HOUR);
    const withDeadline = container.textContent ?? "";
    await render(approval(), T0 + 2 * HOUR);
    const without = container.textContent ?? "";
    // The deadline is additive: strip it and the two lines agree.
    expect(
      withDeadline
        .replace("·Declines itself in 22h", "")
        .replace("Declines itself in 22h", ""),
    ).toBe(without);
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
 * The deadline's own wording and tone (#1403), as strings.
 *
 * Pinned here rather than only through the DOM because these six buckets are
 * the whole fix: three of them did not exist before, and one of them —
 * "under a minute" — replaces a string ("in 0m") that an operator could read as
 * "already declined".
 */
describe("approvalDeadline", () => {
  it("counts a distant deadline down quietly", () => {
    expect(approvalDeadline(T0 + 22 * HOUR, T0)).toEqual({
      text: "Declines itself in 22h",
      tone: "normal",
    });
    expect(approvalDeadline(T0 + 3 * 24 * HOUR, T0)).toEqual({
      text: "Declines itself in 3d",
      tone: "normal",
    });
  });

  it("turns loud inside the last hour", () => {
    expect(approvalDeadline(T0 + 59 * 60_000, T0)).toEqual({
      text: "Declines itself in 59m",
      tone: "soon",
    });
    expect(approvalDeadline(T0 + 4 * 60_000, T0)).toEqual({
      text: "Declines itself in 4m",
      tone: "soon",
    });
    // The boundary belongs to the quiet arm: an hour is an hour, not "soon".
    expect(approvalDeadline(T0 + 60 * 60_000, T0).tone).toBe("normal");
  });

  it("spells out the last minute rather than rounding it to zero", () => {
    // The bug this replaces: `untilLabel` rounds, so twenty-five seconds left
    // printed the same "in 0m" as a deadline that had already gone.
    expect(approvalDeadline(T0 + 25_000, T0)).toEqual({
      text: "Declines itself in under a minute",
      tone: "soon",
    });
    expect(approvalDeadline(T0 + 59_000, T0).text).toBe("Declines itself in under a minute");
  });

  it("floors rather than rounding, so the time left is never overstated", () => {
    // 90 seconds is one minute of usable time, not two. `untilLabel` would
    // round this up; on a deadline the operator is racing, the only safe
    // rounding error is the one that makes them act sooner.
    expect(approvalDeadline(T0 + 90_000, T0).text).toBe("Declines itself in 1m");
    expect(approvalDeadline(T0 + 90 * 60_000, T0).text).toBe("Declines itself in 1h");
  });

  it("says a passed deadline is passed, and never counts backwards", () => {
    expect(approvalDeadline(T0 - HOUR, T0)).toEqual({
      text: "Past its deadline — declining itself",
      tone: "passed",
    });
    // Exactly on the deadline is past it: there is no time left to offer.
    expect(approvalDeadline(T0, T0).tone).toBe("passed");
    expect(approvalDeadline(T0 - 1, T0).text).not.toContain("0m");
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
