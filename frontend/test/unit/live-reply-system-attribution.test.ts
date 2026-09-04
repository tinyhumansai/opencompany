import { describe, expect, it } from "vitest";

import { liveReplyAttribution } from "@/lib/live-reply";

/**
 * A live `agent_reply` frame attributed to the runtime itself
 * (`SYSTEM_AUTHOR` = `"system"` on the Rust side, B-101's mention-ambiguity
 * notice) must render as the centred system pill, not a named teammate's
 * bubble.
 *
 * Before this rule was named, `renderAgentReply` (app-shell.tsx) always built
 * the live frame with `from: "company"` — so a detached-delivery ambiguity
 * note rendered with an avatar and reply/reaction controls, exactly the
 * "small lie about who decided" `post_mention_ambiguity_note`'s own doc
 * comment says attributing to `SYSTEM_AUTHOR` exists to avoid — until the
 * next history reload silently swapped it to the pill `fromHistory` gives
 * every `entry.author === "system"` row (tinysweeper / Codex review, PR
 * #2052). This pins the rule so a regression to a hard-coded `"company"`
 * fails a test instead of only a live screenshot mid-send.
 */
describe("liveReplyAttribution", () => {
  it("attributes the runtime's own agentId to the system pill", () => {
    expect(liveReplyAttribution("system")).toBe("system");
  });

  it("attributes every named agent to the company voice", () => {
    expect(liveReplyAttribution("engineer")).toBe("company");
    expect(liveReplyAttribution("")).toBe("company");
  });
});
