import { describe, expect, it } from "vitest";

import { liveFrameThreadKey, MAIN_THREAD_ID } from "@/lib/chat";
import { dmThreadId } from "@/views/room/model";
import type { TeamMember } from "@/lib/team";

/**
 * Which spellings a live turn frame's thread id is rewritten through before it
 * keys `liveStepsByThread` / `receiptByThread`.
 *
 * Calls the production resolver, not a copy of its conditional — a duplicated
 * rule here would keep passing through a regression in the one `onTurnEvent`
 * actually uses (CodeRabbit on #2068).
 */

const ADA: TeamMember = { id: "ada", name: "Ada" } as TeamMember;

/** The shell's thread → channel map once the desk list has landed. */
const LOADED: Record<string, string> = {
  ada: "dm:ada",
  "": MAIN_THREAD_ID,
  [MAIN_THREAD_ID]: MAIN_THREAD_ID,
};

describe("a live frame's thread key", () => {
  it("leaves an agent DM in the host-thread namespace the readers use", () => {
    // The identity `ChatView` reads by and `onSendStart` arms under.
    expect(dmThreadId(ADA)).toBe("ada");
    // So the frame keys the same way — not `dm:ada`, which the map answers with
    // and which no render or receipt lookup ever asks for.
    expect(liveFrameThreadKey(LOADED, "ada")).toBe(dmThreadId(ADA));
    expect(LOADED["ada"]).toBe("dm:ada");
  });

  it("resolves a General alias whatever casing the caller addressed", () => {
    // The host echoes the caller's spelling; all of them are the same line, and
    // the console armed its maps at the archive's id.
    for (const spelling of ["", "main", "General", "general"]) {
      expect(liveFrameThreadKey(LOADED, spelling)).toBe(MAIN_THREAD_ID);
    }
  });

  it("keys a General alias identically before and after the desks load", () => {
    // The regression this pins: the map is empty until `/desks` lands, so a
    // `tool_call` could key one way and its `tool_result` another. A result
    // whose call is not in its bucket is dropped, so the call row would stay
    // `running` for good — in a bucket nothing renders.
    const beforeDesks = liveFrameThreadKey({}, "General");
    const afterDesks = liveFrameThreadKey(LOADED, "General");

    expect(beforeDesks).toBe(afterDesks);
    expect(beforeDesks).toBe(MAIN_THREAD_ID);
  });

  it("keys a DM identically before and after the desks load, too", () => {
    expect(liveFrameThreadKey({}, "ada")).toBe(liveFrameThreadKey(LOADED, "ada"));
  });

  it("honours a blueprint desk that owns the General line", () => {
    // A company declaring `[[group_chat]] id = "general"` keeps its own desk,
    // and the map is the only thing that knows. Resolution must defer to it
    // rather than assume the archive.
    const blueprint = { ...LOADED, [MAIN_THREAD_ID]: "growth" };
    expect(liveFrameThreadKey(blueprint, "General")).toBe("growth");
  });
});
