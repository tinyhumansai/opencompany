import { describe, expect, it, vi } from "vitest";

import { drainReReadQueue } from "@/lib/re-read-queue";

describe("drainReReadQueue (issue #1701)", () => {
  it("leaves a parked thread parked while its channel is unknown", () => {
    const pending = new Set(["thread-a"]);
    const reRead = vi.fn();
    drainReReadQueue(pending, {}, reRead);
    expect(reRead).toHaveBeenCalledTimes(0);
    expect(pending).toEqual(new Set(["thread-a"]));
  });

  it("replays a parked thread exactly once when its channel appears", () => {
    const pending = new Set(["thread-a"]);
    const reRead = vi.fn();
    drainReReadQueue(pending, { "thread-a": "general" }, reRead);
    expect(reRead).toHaveBeenCalledTimes(1);
    expect(reRead).toHaveBeenCalledWith("thread-a");
    expect(pending.size).toBe(0);
  });

  it("does not re-read on a second drain against the same populated map", () => {
    const pending = new Set(["thread-a"]);
    const reRead = vi.fn();
    const map = { "thread-a": "general" };
    drainReReadQueue(pending, map, reRead);
    drainReReadQueue(pending, map, reRead);
    expect(reRead).toHaveBeenCalledTimes(1);
  });

  it("does not replay a stale thread cleared on company switch", () => {
    // Old company parked `thread-a`; the switch clears the queue before the new
    // company's channel map lands — even one that reuses the `general` id.
    const pending = new Set(["thread-a"]);
    pending.clear();
    const reRead = vi.fn();
    drainReReadQueue(pending, { "thread-a": "general" }, reRead);
    expect(reRead).toHaveBeenCalledTimes(0);
  });

  it("never parks a thread that settles after the map is populated", () => {
    // The fast path in the callback folds directly and never enqueues; the
    // queue stays empty, so a drain is a no-op.
    const pending = new Set<string>();
    const reRead = vi.fn();
    drainReReadQueue(pending, { "thread-a": "general" }, reRead);
    expect(reRead).toHaveBeenCalledTimes(0);
    expect(pending.size).toBe(0);
  });

  /**
   * Issue #1781 review (Codex P2): the map only ever holds the four
   * canonical General spellings, but a settled turn can park under whichever
   * casing the host accepted it under. A bare `channelMap[threadId]` index
   * never matches an uncanonical id, so a thread parked as `"MAIN"` stayed
   * parked forever even once the map was fully populated with `"main"`.
   */
  it("replays a parked thread whose id is an uncanonical General spelling", () => {
    const pending = new Set(["MAIN"]);
    const reRead = vi.fn();
    drainReReadQueue(pending, { main: "general" }, reRead);
    expect(reRead).toHaveBeenCalledTimes(1);
    expect(reRead).toHaveBeenCalledWith("MAIN");
    expect(pending.size).toBe(0);
  });
});
