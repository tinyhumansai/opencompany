import { describe, expect, it } from "vitest";

import { isDuplicateLiveReply } from "@/lib/live-reply";

/**
 * A live `agent_reply` frame is a duplicate only when it is genuinely the
 * same event as one already on screen — never merely because the text
 * matches (Codex review, PR #2052).
 *
 * The recent-tail content check `renderAgentReply` used existed for one
 * narrow reason: the operator's own turn renders locally, immediately, under
 * an ephemeral `m<n>` id, and the backend's own SSE echo of that same reply
 * can arrive before the awaited POST resolves and reconciles that row to its
 * durable id — in that window only content can recognise the echo. Content
 * matching with no further check is too broad: an operator who repeats the
 * same ambiguous `@name` produces two B-101 notices with identical wording,
 * and the second one was silently dropped outright rather than merely
 * deduped — a stronger failure than a duplicate render, since the second
 * refusal never appeared until a reload.
 */
describe("isDuplicateLiveReply", () => {
  it("is a duplicate when this event's own durable id is already on screen", () => {
    const tail = [{ id: "h7", from: "system" as const, text: "pinged nobody" }];
    expect(isDuplicateLiveReply(tail, { seq: 7, text: "pinged nobody" }, "system")).toBe(true);
    // Even with different text — the id match is decisive on its own.
    expect(isDuplicateLiveReply(tail, { seq: 7, text: "different wording" }, "system")).toBe(
      true,
    );
  });

  it("is a duplicate of an unreconciled optimistic echo matched by content", () => {
    // `m3` is the ephemeral id the operator's own optimistic bubble carries
    // before `reconcileIds` runs — never a host id.
    const tail = [{ id: "m3", from: "company" as const, text: "here is your answer" }];
    expect(
      isDuplicateLiveReply(tail, { seq: 12, text: "here is your answer" }, "company"),
    ).toBe(true);
  });

  it("is NOT a duplicate of a different durable event with identical text", () => {
    // The bug this pins: a second, genuinely distinct B-101 notice (its own
    // seq, its own durable id) must render even though an operator repeating
    // the same `@name` gives it the exact same wording as the first.
    const tail = [{ id: "h4", from: "system" as const, text: "you meant one of two teammates" }];
    expect(
      isDuplicateLiveReply(
        tail,
        { seq: 9, text: "you meant one of two teammates" },
        "system",
      ),
    ).toBe(false);
  });

  it("is not a duplicate when neither id nor content matches", () => {
    const tail = [{ id: "h4", from: "company" as const, text: "unrelated" }];
    expect(isDuplicateLiveReply(tail, { seq: 9, text: "something else" }, "system")).toBe(
      false,
    );
  });

  it("ignores a content match against a different `from`", () => {
    // Same text, same (durable) tail row, but a different voice — not the
    // same event by any measure this function uses.
    const tail = [{ id: "m1", from: "you" as const, text: "hello" }];
    expect(isDuplicateLiveReply(tail, { seq: 2, text: "hello" }, "company")).toBe(false);
  });
});
