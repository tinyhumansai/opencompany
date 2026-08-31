import { describe, expect, it } from "vitest";

import {
  channelIdFromSegment,
  channelIdForThread,
  dmChannelId,
  legacyDmChannelId,
  resolveDmChannelId,
} from "@/views/chat/model";
import type { Desk } from "@/lib/desks";
import type { TeamMember } from "@/lib/team";

/**
 * Channel-id derivation and the pre-#364 legacy-URL shim.
 *
 * Two id namespaces meet here and the console has already been wrong about it
 * once (issue #367). Both failure modes are silent: a thread routed to a
 * channel that does not exist drops its messages into a bucket nothing renders,
 * and a legacy DM link that stops resolving lands the reader on an
 * unknown-channel notice. Neither raises.
 */

function member(over: Partial<TeamMember> & Pick<TeamMember, "id" | "name">): TeamMember {
  return {
    role: "Engineer",
    description: "",
    tone: "sky",
    avatar: "green",
    inboxEnabled: false,
    effectiveTools: [],
    desks: [],
    ...over,
  };
}

const ADA = member({ id: "agent-ada", name: "Ada" });
const GRACE = member({ id: "agent-grace", name: "Grace" });
const ROSTER = [ADA, GRACE];

describe("dmChannelId", () => {
  it("keys on the teammate's id, so a rename cannot move their DM", () => {
    const renamed = member({ id: "agent-ada", name: "Ada Lovelace" });
    expect(dmChannelId(renamed)).toBe(dmChannelId(ADA));
  });
});

describe("resolveDmChannelId (the legacy-URL shim)", () => {
  it("resolves a current DM id to itself", () => {
    expect(resolveDmChannelId(dmChannelId(ADA), ROSTER)).toBe("dm:agent-ada");
  });

  it("resolves a bookmarked pre-#364 name-derived id to the CURRENT id", () => {
    // The whole point of the shim: an old link in somebody's ticket still lands
    // on Ada's DM, and what comes back is the id in use today — so nothing new
    // is ever written under the legacy form.
    const old = legacyDmChannelId(ADA);
    expect(old).not.toBe(dmChannelId(ADA));
    expect(resolveDmChannelId(old, ROSTER)).toBe("dm:agent-ada");
  });

  it("returns null for a DM this company has nobody for", () => {
    expect(resolveDmChannelId("dm:agent-nobody", ROSTER)).toBeNull();
  });

  it("returns null for anything that is not a DM id", () => {
    expect(resolveDmChannelId("engineering", ROSTER)).toBeNull();
  });
});

describe("channelIdFromSegment (URL decoding at the hash boundary)", () => {
  it("leaves an already-literal channel id alone", () => {
    expect(channelIdFromSegment("dm:agent-ada")).toBe("dm:agent-ada");
    expect(channelIdFromSegment("engineering")).toBe("engineering");
  });

  it("decodes an encoded DM id, as an approval-card href mints it", () => {
    // `#/chat/${encodeURIComponent(channelId)}` writes `dm%3Aagent-ada`, and
    // the router passes that segment through untouched. Without the decode the
    // id compares against nothing and the link lands on the fallback channel.
    expect(channelIdFromSegment("dm%3Aagent-ada")).toBe("dm:agent-ada");
  });

  it("feeds the decoded id into the legacy shim", () => {
    const old = legacyDmChannelId(ADA);
    expect(resolveDmChannelId(channelIdFromSegment(encodeURIComponent(old))!, ROSTER)).toBe(
      dmChannelId(ADA),
    );
  });

  it("returns null for no segment", () => {
    expect(channelIdFromSegment(null)).toBeNull();
  });

  it("returns the raw segment on malformed escapes, so it reads as unknown", () => {
    // `#/chat/%` is a typo, not a channel — the raw value should surface in the
    // unknown-channel notice rather than collapsing silently onto the fallback.
    expect(channelIdFromSegment("%")).toBe("%");
  });
});

describe("channelIdForThread (issue #367)", () => {
  const desks: Desk[] = [
    { id: "engineering", channel: "engineering", name: "Engineering", blurb: "" },
  ];

  it("passes a desk thread id through — a desk's channel id IS its thread id", () => {
    expect(channelIdForThread("engineering", desks, ROSTER)).toBe("engineering");
  });

  it("maps a teammate's agent id to the console-local DM channel id", () => {
    // The asymmetry that makes this function necessary: the host journals a DM
    // under the teammate's agent id, but the console addresses the channel by
    // `dm:<id>`. Routing the raw thread id would file the message under a
    // channel that does not exist.
    expect(channelIdForThread("agent-grace", desks, ROSTER)).toBe("dm:agent-grace");
  });

  it("returns null for a thread this company owns no channel for", () => {
    // Explicitly not a fallback to the first channel: filing a stranger's
    // messages into a real transcript is worse than declining to route them.
    expect(channelIdForThread("some-other-company-thread", desks, ROSTER)).toBeNull();
  });
});
