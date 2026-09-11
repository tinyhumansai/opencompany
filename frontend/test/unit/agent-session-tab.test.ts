// The Session tab is a tab, an address, and one merged stream.
//
// Source-shape assertions, in the idiom this suite already uses for wiring that
// is invisible to a reviewer of a single diff (`assert-design-tokens.sh` is the
// precedent every one of these cites). Three things have to stay true and none
// of them shows up in a rendered snapshot:
//
//   1. the tab exists and is addressable, so `#/company/agent/<id>?tab=session`
//      lands (`#/team/<id>` rewrites onto that and drops the query);
//   2. the stream is fed by the host's own per-agent route, not by a
//      console-side merge of per-desk histories;
//   3. the agent-to-agent collapses are the room's, not second copies.

import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

const detail = readFileSync("src/views/team/AgentDetailView.tsx", "utf8");
const session = readFileSync("src/views/team/AgentSession.tsx", "utf8");
const client = readFileSync("src/api/client.ts", "utf8");

describe("the Session tab", () => {
  /**
   * A tab is an address, not local state — the rule `task-detail-tab-address`
   * pins for the task page. `useHashTab` is what makes `#/team/<id>?tab=session`
   * resolve, so a link to a teammate's session survives a reload and can be
   * pasted to somebody else.
   */
  it("is registered in the tab table the hash resolves against", () => {
    expect(detail).toContain('{ id: "session", label: "Session"');
    expect(detail).toContain("useHashTab<AgentTab>(");
    expect(detail).toContain('AGENT_TABS.map((t) => t.id)');
  });

  it("renders the session panel under that id", () => {
    expect(detail).toMatch(
      /<PageTabPanel idBase="agent" id="session" value=\{tab\}>[\s\S]{0,400}<AgentSession/,
    );
  });

  /**
   * The stream must come from `GET {scope}/agents/{id}/session`.
   *
   * Which channels an agent can read is decided host-side by the same function
   * that decides the agent's own context. A console-side merge of per-desk
   * `chat/history` calls would be a second opinion about what a teammate can
   * see, and the two would drift — at which point the page starts claiming an
   * agent saw something it did not.
   */
  it("reads the host's own per-agent route rather than merging desks", () => {
    expect(client).toContain("/agents/${encodeURIComponent(agentId)}/session");
    expect(session).toContain("client.agentSession(agentId, company,");
    expect(session).not.toContain("getChatHistory");
  });

  /**
   * `fromHistory` is the room's mapping and is reused whole. It is what carries
   * `referralConversation` and `asideConversation` through untouched; a
   * hand-rolled map here would be a second answer to "what is a chat line".
   *
   * Called once per row (`fromHistory([row])`), not once for the whole array
   * (tinysweeper review): the old `fromHistory(rows)` shape correlated its
   * output back to `rows` by array index, which held only because `fromHistory`
   * happens to be a 1:1, order-preserving `.map` today — a later filter or
   * reorder inside it would silently misattribute a row's `channel` with no
   * type error to catch it. Per-row calls tie each message to its row
   * structurally instead of positionally.
   */
  it("maps rows through the room's own history mapping", () => {
    expect(session).toContain("fromHistory([row])");
    expect(session).not.toContain("fromHistory(rows)");
  });

  /**
   * The agent-to-agent collapses are imported from the room, not reimplemented.
   * An exchange between two teammates has to read the same way here as it does
   * in the channel it happened in.
   */
  it("reuses the room's referral and aside collapses", () => {
    expect(session).toContain('from "@/views/room/StepTimeline"');
    expect(session).toContain("<ReferralConversation crossing={message.referralConversation} />");
    expect(session).toContain("<AsideConversation aside={message.asideConversation} />");
    expect(session).toContain("<StepTimeline steps={message.steps} />");
  });

  /**
   * Every row says which channel it came from. Without it the stream is
   * unreadable: two teammates answering in two desks interleave with nothing to
   * tell them apart, which is the one thing merging the channels costs.
   */
  it("badges every row with the channel the host stamped", () => {
    expect(session).toContain("sessionChannel");
    expect(session).toContain('data-testid="agent-session-channel"');
  });

  /**
   * A host without the route is a host without this surface, not a failure. The
   * distinction matters because the honest message differs: "this host does not
   * keep one" invites no debugging, and a red error box does.
   */
  it("tells a 404 apart from a failed read", () => {
    expect(session).toContain('setLoad(status === 404 ? "unsupported" : "error")');
  });

  /**
   * The same stale-response guard `AgentRuns` carries (issue #1671): a read
   * started before a teammate switch must not commit its rows beneath the next
   * teammate's name.
   */
  it("discards a read that resolved after a teammate switch", () => {
    expect(session).toContain("generationRef");
    expect(session).toContain("if (generation !== generationRef.current) return;");
  });
});
