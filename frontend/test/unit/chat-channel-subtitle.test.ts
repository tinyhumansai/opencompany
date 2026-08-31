import { describe, expect, it } from "vitest";

import type { TeamMember } from "@/lib/team";
import {
  buildChannels,
  channelIntroSentence,
  channelSubtitle,
  channelTitle,
  type Channel,
} from "@/views/chat/model";

/**
 * The second slot beside a chat title, and what it is allowed to say (issue
 * #1180).
 *
 * The failure this guards renders perfectly. Nothing throws, no type objects,
 * and the header is laid out exactly as designed — a title, a divider, a muted
 * line — except that both sides of the divider hold the same words:
 *
 *     🫥  Backend Engineer  │  Backend Engineer
 *
 * It happens because the two slots read different fields that collapse to one
 * string. The title is the teammate's name, and `fromDto` falls back
 * `dto.name?.trim() || dto.role`; the slot used to read the role directly. A
 * company that declares roles and never names people — which is every agent in
 * `companies/agentic_software_company` — makes those the same string for every
 * teammate it employs.
 *
 * So two things are pinned here. `buildChannels` must fill a DM's `purpose`
 * from the field that is *parallel* to a desk's blurb (the description), and
 * `channelSubtitle` must answer `null` rather than hand a caller a string that
 * only repeats the title. The second is the load-bearing one: it is what keeps
 * a future field swap from re-introducing the duplicate, and what the header
 * keys on to drop the divider along with the text.
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

function dmFor(m: TeamMember): Channel {
  const dms = buildChannels([m], [], {
    [`dm:${m.id}`]: [{ id: "message", from: "you", text: "Hello", at: 1 }],
  }).find((s) => s.id === "dms");
  expect(dms?.channels).toHaveLength(1);
  return dms!.channels[0];
}

function channelFor(over: { channel: string; blurb: string }): Channel {
  const sections = buildChannels([], [
    { id: "d1", channel: over.channel, name: "Engineering", blurb: over.blurb },
  ]);
  const channels = sections.find((s) => s.id === "channels")!.channels;
  // The built-in `#general` channel is always first (issue #1743); this helper
  // is about the desk channel after it.
  expect(channels).toHaveLength(2);
  expect(channels[0].id).toBe("main");
  return channels[1];
}

/** The roster shape the sample company actually produces: a role, no name. */
const ROLE_ONLY = member({
  id: "agent_backend",
  name: "Backend Engineer",
  role: "Backend Engineer",
  description: "Build and operate the backend and services.",
});

describe("channelSubtitle on a DM", () => {
  it("says what the teammate does, for a teammate the host never named", () => {
    // The #1180 case end to end. The title says who; this says what for, out of
    // the description the roster entry was carrying unused all along.
    expect(channelSubtitle(dmFor(ROLE_ONLY))).toBe("Build and operate the backend and services.");
  });

  it("says nothing rather than the role when the role IS the title", () => {
    // The regression guard. With no description the fallback is the role, and
    // for this teammate the role is also the name — so the slot has no second
    // fact to offer and must not pretend otherwise by restyling the first.
    const dm = dmFor(member({ id: "agent_backend", name: "Backend Engineer", role: "Backend Engineer" }));
    expect(channelSubtitle(dm)).toBeNull();
    expect(channelSubtitle(dm)).not.toBe(channelTitle(dm));
  });

  it("keeps the role for a named teammate with no description", () => {
    // Not a blanket "drop the role": for a teammate the host *did* name, the
    // role is a genuinely different string from the title and worth the space.
    const dm = dmFor(member({ id: "agent_ada", name: "Ada", role: "Backend Engineer" }));
    expect(channelSubtitle(dm)).toBe("Backend Engineer");
  });

  it("catches a description that only restates the title in different case", () => {
    // A duplicate is a duplicate to a reader whatever its casing or padding,
    // and a manifest that answers "description" by retyping the role in
    // sentence case is the most likely way to write one by hand.
    const dm = dmFor(
      member({
        id: "agent_backend",
        name: "Backend Engineer",
        role: "Backend Engineer",
        description: "  backend engineer ",
      }),
    );
    expect(channelSubtitle(dm)).toBeNull();
  });

  it("catches a duplicate with doubled internal whitespace, not just outer padding", () => {
    // The doc comment on `channelSubtitle` promises "whitespace-insensitive"
    // without qualifying it to the ends of the string. A description
    // copy-pasted from the role with an extra space or a stray newline typed
    // in the middle is still the same non-fact to a reader, and a comparison
    // that only trims the outer edges would miss it.
    const dm = dmFor(
      member({
        id: "agent_backend",
        name: "Backend Engineer",
        role: "Backend Engineer",
        description: "Backend   Engineer",
      }),
    );
    expect(channelSubtitle(dm)).toBeNull();
  });
});

describe("channelSubtitle on a channel", () => {
  it("hands back the desk's blurb unchanged — a slug and a blurb are two facts", () => {
    // The do-not-break-channels guard. `#engineering` and "Build, test, and
    // secure the product." were never the duplicate this fixed, and the fix
    // must not cost a channel the one line it has to explain itself.
    expect(channelSubtitle(channelFor({ channel: "engineering", blurb: "Build, test, and secure the product." })))
      .toBe("Build, test, and secure the product.");
  });

  it("says nothing for a desk that wrote no blurb", () => {
    expect(channelSubtitle(channelFor({ channel: "engineering", blurb: "" }))).toBeNull();
  });

  it("applies the same rule to a blurb that just restates the slug", () => {
    // Kind-agnostic on purpose: `#engineering │ Engineering` is the identical
    // non-fact, and a rule that only fired for DMs would ship it.
    expect(channelSubtitle(channelFor({ channel: "engineering", blurb: "Engineering" }))).toBeNull();
  });
});

describe("channelIntroSentence", () => {
  it("ends a DM's opening line with exactly one full stop", () => {
    // The defect a browser caught and this suite did not. The clause used to
    // take a hardcoded ".", which was invisible while the subtitle was a role
    // and produced "…and services.." the moment it became a description that
    // punctuates itself. Asserted as a suffix rule, not a fixed string, so it
    // fails for any description whatever punctuation the manifest gave it.
    const line = channelIntroSentence(dmFor(ROLE_ONLY), false);
    expect(line).toBe(
      "This is the start of your direct message with Backend Engineer — build and operate the backend and services.",
    );
    expect(line.endsWith("..")).toBe(false);
  });

  it("still supplies a full stop for a description that came without one", () => {
    const dm = dmFor(
      member({ id: "agent_ada", name: "Ada", role: "Engineer", description: "Keeps the schedulers honest" }),
    );
    expect(channelIntroSentence(dm, false)).toBe(
      "This is the start of your direct message with Ada — keeps the schedulers honest.",
    );
  });

  it("drops the clause, not the sentence, when there is nothing to add", () => {
    // The #1180 read: no description and a name that IS the role. The line
    // still says where you are; it just stops rather than repeating itself.
    const dm = dmFor(member({ id: "agent_ops", name: "Ops Lead", role: "Ops Lead" }));
    expect(channelIntroSentence(dm, false)).toBe(
      "This is the start of your direct message with Ops Lead.",
    );
  });

  it("keeps a channel's two-sentence opening", () => {
    expect(
      channelIntroSentence(channelFor({ channel: "engineering", blurb: "Build, test, and secure the product." }), false),
    ).toBe("This is the very beginning of #engineering. Build, test, and secure the product.");
  });

  it("leaves no trailing space on a channel with no blurb", () => {
    const line = channelIntroSentence(channelFor({ channel: "engineering", blurb: "" }), false);
    expect(line).toBe("This is the very beginning of #engineering.");
    expect(line).toBe(line.trimEnd());
  });

  it("makes no claim about emptiness while history is still loading (issue #934)", () => {
    // Neither finished sentence may render before the host has answered — both
    // assert the channel has no history. The subtitle alone is not a claim.
    expect(channelIntroSentence(dmFor(ROLE_ONLY), true)).toBe(
      "Build and operate the backend and services.",
    );
    expect(channelIntroSentence(dmFor(member({ id: "a", name: "Ops Lead", role: "Ops Lead" })), true)).toBe("");
  });
});

describe("buildChannels fills a DM's purpose from the description", () => {
  it("prefers the description — the field parallel to a desk's blurb", () => {
    expect(dmFor(ROLE_ONLY).purpose).toBe("Build and operate the backend and services.");
  });

  it("falls back to the role when the teammate has no description", () => {
    // Still the fallback rather than an empty string: dropping the role here
    // would take the subtitle away from every *named* teammate too, and
    // `channelSubtitle` is the right place to decline a duplicate.
    expect(dmFor(member({ id: "agent_ada", name: "Ada", role: "Backend Engineer" })).purpose)
      .toBe("Backend Engineer");
  });
});
