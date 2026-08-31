import { describe, expect, it } from "vitest";

import { avatarFor } from "@/lib/team";
import type { TeamMember } from "@/lib/team";
import { buildChannels, dmFace, type Channel } from "@/views/chat/model";

/**
 * The seed a DM's avatar is drawn from (issues #1170, #1185).
 *
 * The failure this guards is silent and only visible side by side: the rail row
 * and the chat header both draw the teammate on the other end of a DM, and
 * `TeammateAvatar` hashes its mascot out of whatever seed it is handed. Seed
 * the two from different fields and one person wears two faces on one screen,
 * which is worse than the generic glyph the header used to draw. Nothing
 * throws, nothing fails to render, and no type objects.
 *
 * So this pins the seed itself rather than a rendered tile: both call sites go
 * through `dmFace`, and `dmFace` now promises the teammate's id-seeded
 * `avatar` — the field `fromDto` already computes onto every roster entry so a
 * rename can never change a teammate's face — rather than a name that would
 * have to be hashed again at render.
 */

function member(over: Partial<TeamMember> & Pick<TeamMember, "id" | "name">): TeamMember {
  return {
    role: "Engineer",
    description: "",
    tone: "sky",
    // Id-seeded by default, matching what `fromDto` computes — a caller
    // testing a specific mismatch overrides it explicitly.
    avatar: avatarFor(over.id),
    inboxEnabled: false,
    effectiveTools: [],
    desks: [],
    ...over,
  };
}

function dmFor(m: TeamMember): Channel {
  const dms = buildChannels([m], [], {
    [`dm:${m.id}`]: [{ id: "message", from: "you", text: "Hello", at: 1 }],
  }).find((s) => s.id === "dms");
  expect(dms?.channels).toHaveLength(1);
  return dms!.channels[0];
}

describe("dmFace", () => {
  it("hands back the teammate's own name, tone, and id-seeded avatar, so every caller seeds alike", () => {
    const ada = member({ id: "agent_ada", name: "Ada", tone: "violet" });
    expect(dmFace(dmFor(ada))).toEqual({ name: "Ada", tone: "violet", avatar: avatarFor("agent_ada") });
  });

  it("resolves to one mascot for one teammate", () => {
    // What the rail row and the header each end up drawing. They call the same
    // function, so the only way these can differ is a change to `dmFace` — the
    // point of routing both through it.
    const face = dmFace(dmFor(member({ id: "agent_backend", name: "Backend Engineer" })));
    expect(face).not.toBeNull();
    expect(face!.avatar).toBe(avatarFor("agent_backend"));
  });

  it("seeds on the id, not the name — a rename must not change the face", () => {
    // Two roster rows sharing an id but not a name — the rename case this
    // exists to protect — resolve to the *same* avatar, and it's the id's.
    const renamed = dmFace(dmFor(member({ id: "agent_ada", name: "Adam" })));
    const original = dmFace(dmFor(member({ id: "agent_ada", name: "Ada" })));
    expect(renamed!.avatar).toBe(original!.avatar);
    expect(renamed!.avatar).toBe(avatarFor("agent_ada"));
  });

  it("has no face for a channel — a desk line has no one person behind it", () => {
    const desks = buildChannels([], [{ id: "d1", channel: "front-desk", name: "Front desk", blurb: "" }]);
    const channel = desks.find((s) => s.id === "channels")!.channels[0];
    expect(channel.kind).toBe("channel");
    expect(dmFace(channel)).toBeNull();
  });

  it("has no face for a DM with no roster entry behind it", () => {
    // Both call sites fall back to a glyph here rather than inventing a mascot
    // for a teammate the roster cannot name.
    const orphan: Channel = { id: "dm:gone", name: "Gone", kind: "dm", purpose: "" };
    expect(dmFace(orphan)).toBeNull();
  });

  it("gives a private channel no face either — the lock still speaks for it", () => {
    const locked: Channel = { id: "c1", name: "ops", kind: "channel", purpose: "", private: true };
    expect(dmFace(locked)).toBeNull();
  });
});
