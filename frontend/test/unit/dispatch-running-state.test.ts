import { describe, expect, it } from "vitest";

import {
  clearDispatchRunning,
  dispatchThreadKey,
  isConversationWaiting,
  markDispatchRunning,
  type DispatchRunning,
} from "@/lib/chat";

/**
 * The shell holds in-flight dispatches as `taskId -> threadKey`, and the
 * shape is the point (tinysweeper, #2369).
 *
 * A per-thread **count** is decremented by whichever terminal arrives, so an
 * unrelated card completing in the same thread takes a live attempt's row
 * down, and that attempt's own completion then finds nothing to clear. Keying
 * by task id means a completion can only ever clear the attempt it belongs to.
 *
 * These call the reducers the shell and the room actually apply, so the test
 * fails if that logic drifts rather than agreeing with a copy of it.
 */
const started = markDispatchRunning;
const completed = clearDispatchRunning;
const waitingOn = (running: DispatchRunning, key: string): boolean =>
  Object.values(running).includes(key);

describe("in-flight dispatch marks", () => {
  it("clears only the attempt that finished", () => {
    let running = started({}, "task-a", "main", "50");
    running = started(running, "task-b", "main", "50");

    // B finishes first. A is still running and must keep its row.
    running = completed(running, "task-b");

    expect(waitingOn(running, dispatchThreadKey("main", "50"))).toBe(true);
    expect(Object.keys(running)).toEqual(["task-a"]);
  });

  it("a completion for an untracked card changes nothing", () => {
    // A board-created card raises no mark, so its terminal must not remove
    // somebody else's.
    const running = started({}, "task-a", "main", "50");

    expect(completed(running, "task-from-the-board")).toBe(running);
  });

  it("is empty once every attempt has reported", () => {
    let running = started({}, "task-a", "main");
    running = completed(running, "task-a");

    expect(waitingOn(running, dispatchThreadKey("main"))).toBe(false);
  });

  it("keeps a thread's work apart from its channel's", () => {
    const running = started({}, "task-a", "main", "50");

    expect(waitingOn(running, dispatchThreadKey("main", "50"))).toBe(true);
    expect(waitingOn(running, dispatchThreadKey("main"))).toBe(false);
  });
});

describe("which conversation is waiting", () => {
  const CHANNEL = "general";

  it("answers per surface, not per conversation", () => {
    // One shared answer painted a channel-level attempt's working row inside
    // whatever thread was open (#2369). Channel-level work is the channel's
    // wait and nothing else's.
    const running = started({}, "task-1", CHANNEL);
    expect(isConversationWaiting(running, CHANNEL)).toBe(true);
    expect(isConversationWaiting(running, CHANNEL, "12")).toBe(false);
  });

  it("keeps a thread's own work inside that thread", () => {
    const running = started({}, "task-1", CHANNEL, "12");
    expect(isConversationWaiting(running, CHANNEL, "12")).toBe(true);
    expect(isConversationWaiting(running, CHANNEL, "99")).toBe(false);
    expect(isConversationWaiting(running, CHANNEL)).toBe(false);
  });

  it("does not leak across conversations", () => {
    const running = started({}, "task-1", "dm:a+b");
    expect(isConversationWaiting(running, CHANNEL)).toBe(false);
    expect(isConversationWaiting(running, "dm:a+b")).toBe(true);
  });

  it("stops showing waiting once that task's terminal lands", () => {
    let running = started({}, "task-1", CHANNEL, "12");
    running = completed(running, "task-2");
    expect(isConversationWaiting(running, CHANNEL, "12")).toBe(true);
    running = completed(running, "task-1");
    expect(isConversationWaiting(running, CHANNEL, "12")).toBe(false);
  });

  it("tracks a channel wait and a thread wait at the same time", () => {
    let running = started({}, "task-ch", CHANNEL);
    running = started(running, "task-th", CHANNEL, "12");
    expect(isConversationWaiting(running, CHANNEL)).toBe(true);
    expect(isConversationWaiting(running, CHANNEL, "12")).toBe(true);
    running = completed(running, "task-ch");
    expect(isConversationWaiting(running, CHANNEL)).toBe(false);
    expect(isConversationWaiting(running, CHANNEL, "12")).toBe(true);
  });
});
