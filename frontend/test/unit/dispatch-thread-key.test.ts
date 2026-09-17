import { describe, expect, it } from "vitest";

import { dispatchThreadKey } from "@/lib/chat";

/**
 * A chat turn that dispatches succeeds the moment it hands the work over, so
 * its working row settles while the real agent turn is only starting. The
 * thread then rendered minutes of live work as silence and produced a reply
 * from nowhere. These keys are what hold the row up for that window.
 */
describe("dispatchThreadKey", () => {
  it("separates a thread from its own channel", () => {
    // Two different waits: work raised inside a thread, and work raised at
    // channel level. A row belongs to whichever asked.
    expect(dispatchThreadKey("main", "50")).not.toBe(dispatchThreadKey("main"));
  });

  it("is stable for the same thread", () => {
    expect(dispatchThreadKey("main", "50")).toBe(dispatchThreadKey("main", "50"));
  });

  it("reads an empty chat id as the General thread", () => {
    // `""` is General spelled empty, not a missing origin — the same rule
    // `dispatchMarkerPlacement` applies to the completion half.
    expect(dispatchThreadKey("")).toBe(dispatchThreadKey("main"));
  });

  it("keeps two channels apart", () => {
    expect(dispatchThreadKey("engineering")).not.toBe(dispatchThreadKey("main"));
  });
});
