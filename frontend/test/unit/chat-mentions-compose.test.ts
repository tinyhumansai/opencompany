import { describe, expect, it } from "vitest";

import {
  activeMentionQuery,
  aliasSet,
  insertMention,
  mentionablesFor,
  mentionRegex,
  mentionsOutsideChannel,
  mentionsOutsideRange,
  rankMentionables,
  reconcileMentions,
  reconcileWrap,
  resolvableMentions,
  stripCodeRegions,
  utf8ByteLength,
  type Mention,
  type Mentionable,
} from "@/views/chat/mentions";

/**
 * The composer half of @-mentions.
 *
 * Every rule here is one that is easy to get subtly wrong in a keydown handler
 * and hard to notice afterwards: a picker that opens inside an email address, a
 * chip that keeps pinging somebody whose name has been backspaced away, an
 * `@word` highlighted although it resolved to nobody. The server re-validates
 * what this produces, but it cannot fix any of those — by the time a message is
 * sent, the wrong person is already in the list.
 */

const engineer: Mentionable = {
  target: { kind: "agent", id: "engineer" },
  label: "engineer",
  aliases: ["engineer"],
  inChannel: true,
};
const jane: Mentionable = {
  target: { kind: "user", id: "u1" },
  label: "Jane Doe",
  aliases: ["jane doe", "jane-doe"],
};
const everyone: Mentionable = {
  target: { kind: "everyone" },
  label: "everyone",
  aliases: ["everyone", "channel", "here"],
};

describe("activeMentionQuery", () => {
  it("opens on an @ at the start or after whitespace", () => {
    expect(activeMentionQuery("@eng", 4)).toEqual({ start: 0, query: "eng" });
    expect(activeMentionQuery("hey @eng", 8)).toEqual({ start: 4, query: "eng" });
    expect(activeMentionQuery("(@eng", 5)).toEqual({ start: 1, query: "eng" });
  });

  it("does not open inside an email address", () => {
    expect(activeMentionQuery("jane@acme", 9)).toBeNull();
  });

  /**
   * The host's `opens_mention` draws the line at start, whitespace, and an
   * opening bracket — anything else means the `@` belongs to some other token,
   * and a picker there would resolve a mention the server's fallback
   * extraction would refuse.
   */
  it("does not open after punctuation that is not a bracket", () => {
    expect(activeMentionQuery("/docs/@eng", 10)).toBeNull();
    expect(activeMentionQuery("$@eng", 5)).toBeNull();
    expect(activeMentionQuery("x=@eng", 6)).toBeNull();
  });

  it("keeps the query open across a space, so a two-word name is reachable", () => {
    expect(activeMentionQuery("hi @Jane Do", 11)).toEqual({
      start: 3,
      query: "Jane Do",
    });
  });

  it("gives up at a newline rather than holding open across lines", () => {
    expect(activeMentionQuery("@eng\nnext", 9)).toBeNull();
  });

  it("gives up once the query gets implausibly long", () => {
    const long = `@${"x".repeat(40)}`;
    expect(activeMentionQuery(long, long.length)).toBeNull();
  });

  it("is null when the caret is not in a mention at all", () => {
    expect(activeMentionQuery("just talking", 12)).toBeNull();
  });

  /**
   * Without this the picker reopens the instant you pick somebody: inserting
   * `@engineer ` leaves the caret after a trailing space, the backward scan
   * still finds the `@`, and the list re-renders over the message you are now
   * trying to write.
   */
  it("closes once a known name is finished and followed by a space", () => {
    const known = aliasSet([engineer, jane]);
    expect(activeMentionQuery("hey @engineer ", 14, known)).toBeNull();
  });

  /** A space is also how a two-word name gets typed, so it cannot just close. */
  it("stays open through the space of a name still being typed", () => {
    const known = aliasSet([engineer, jane]);
    expect(activeMentionQuery("hi @Jane ", 9, known)).toEqual({
      start: 3,
      query: "Jane ",
    });
  });

  it("stays open on a finished name with no trailing space", () => {
    const known = aliasSet([engineer, jane]);
    expect(activeMentionQuery("hey @engineer", 13, known)).toEqual({
      start: 4,
      query: "engineer",
    });
  });

  /** With no directory to check against, the old behaviour is kept. */
  it("cannot close on a finished name when it has no aliases to check", () => {
    expect(activeMentionQuery("hey @engineer ", 14)).not.toBeNull();
  });
});

describe("aliasSet", () => {
  it("carries every spelling, plus the label", () => {
    const set = aliasSet([jane]);
    expect(set.has("jane doe")).toBe(true);
    expect(set.has("jane-doe")).toBe(true);
  });
});

describe("stripCodeRegions", () => {
  it("preserves length, so offsets computed against it stay valid", () => {
    const text = "a `code` b";
    const masked = stripCodeRegions(text);
    expect(masked).toHaveLength(text.length);
    expect(masked).not.toContain("code");
    expect(masked.startsWith("a ")).toBe(true);
  });

  it("masks fenced blocks and keeps the newlines", () => {
    const text = "before\n```\n@engineer\n```\nafter";
    const masked = stripCodeRegions(text);
    expect(masked).toHaveLength(text.length);
    expect(masked).not.toContain("@engineer");
    expect(masked.split("\n")).toHaveLength(text.split("\n").length);
  });

  it("masks fenced blocks indented up to three spaces", () => {
    // CommonMark renders a fence indented by 1-3 spaces as code, so an `@`
    // inside it must be masked exactly like a column-zero fence.
    const text = "before\n   ```\n@engineer\n   ```\nafter";
    const masked = stripCodeRegions(text);
    expect(masked).toHaveLength(text.length);
    expect(masked).not.toContain("@engineer");
    expect(masked.split("\n")).toHaveLength(text.split("\n").length);
  });

  it("does not treat a text-suffixed fence line as a closing fence", () => {
    // CommonMark only lets a closing fence be followed by spaces or tabs, so
    // `````not-a-close``` is still inside the block. If it closed the mask
    // early, a later `@engineer` the renderer still shows as code would
    // unmask and resolve.
    const text = "before\n```\ncode\n```not-a-close\n@engineer\n```\nafter";
    const masked = stripCodeRegions(text);
    expect(masked).not.toContain("@engineer");
    expect(masked).toHaveLength(text.length);
  });

  it("still closes a fence whose trailing whitespace is only spaces or tabs", () => {
    const text = "before\n```\n@engineer\n```  \nafter";
    const masked = stripCodeRegions(text);
    expect(masked).not.toContain("@engineer");
  });

  it("closes a fence over a CRLF line ending", () => {
    // A CR is the `\r` of a CRLF ending, not fence text — it must keep
    // closing the block exactly as it did before the suffix was restricted.
    const text = "before\n```\n@engineer\n```\r\nafter";
    const masked = stripCodeRegions(text);
    expect(masked).not.toContain("@engineer");
  });

  /**
   * A closing backtick run must be a whole run of exactly the opening length.
   * `` `@engineer`` `` (one opener, two trailing) is not a code span — the
   * two-backtick run can never close a one-backtick opener — so CommonMark
   * renders `@engineer` as visible text and the Rust scanner leaves it
   * unmasked. Blanking it here would make the console disagree with the
   * renderer and, for a mention that *does* open (see `resolvableMentions`
   * below), drop a ping the host would have delivered.
   */
  it("does not close a span on a prefix of a longer backtick run", () => {
    const text = "`@engineer``";
    const masked = stripCodeRegions(text);
    expect(masked).toContain("@engineer");
    expect(masked).toHaveLength(text.length);
  });

  it("still closes a span on an exactly equal run", () => {
    const text = "`@engineer`";
    const masked = stripCodeRegions(text);
    expect(masked).not.toContain("@engineer");
  });

  it("keeps a mention visible when a longer run cannot close a shorter opener", () => {
    const text = "``@engineer`";
    const masked = stripCodeRegions(text);
    expect(masked).toContain("@engineer");
  });
});

describe("rankMentionables", () => {
  it("puts channel members before everyone else", () => {
    const ranked = rankMentionables([jane, everyone, engineer], "e");
    expect(ranked[0]).toBe(engineer);
  });

  it("scores an exact match above a mere substring", () => {
    const other: Mentionable = {
      target: { kind: "agent", id: "senior_engineer" },
      label: "senior_engineer",
      aliases: ["senior_engineer"],
      inChannel: true,
    };
    const ranked = rankMentionables([other, engineer], "engineer");
    expect(ranked[0]).toBe(engineer);
  });

  it("drops rows that do not match at all", () => {
    expect(rankMentionables([engineer, jane], "zzz")).toEqual([]);
  });

  it("keeps everything, in order, for an empty query", () => {
    expect(rankMentionables([engineer, jane, everyone], "")).toHaveLength(3);
  });
});

describe("insertMention", () => {
  it("replaces the query and leaves the caret past a trailing space", () => {
    const draft = "hey @eng";
    const range = { start: 4, end: 8 };
    const { text, caret, mention } = insertMention(draft, range, engineer);
    expect(text).toBe("hey @engineer ");
    expect(caret).toBe(text.length);
    expect(mention).toEqual({
      target: { kind: "agent", id: "engineer" },
      text: "@engineer",
      offset: 4,
    });
    // The recorded span must actually be at the recorded offset, or the chip
    // renders over the wrong characters.
    expect(text.slice(mention.offset, mention.offset + mention.text.length)).toBe(
      "@engineer",
    );
  });

  it("keeps whatever followed the caret", () => {
    const { text } = insertMention("hey @eng please", { start: 4, end: 8 }, engineer);
    expect(text).toBe("hey @engineer  please");
  });

  /**
   * The host only opens a mention when the character after `@` is word-like,
   * so a person whose display name starts with an emoji cannot be picked by
   * their label — `@👩‍💻 Ada` would be dropped server-side. The row's slug
   * alias is the typable spelling that survives revalidation.
   */
  it("inserts a server-valid spelling when the display name cannot open a mention", () => {
    const emoji: Mentionable = {
      target: { kind: "user", id: "u1" },
      label: "👩‍💻 Ada",
      aliases: ["👩‍💻 ada", "ada"],
    };
    const { text, mention } = insertMention("hey @", { start: 4, end: 5 }, emoji);
    expect(text).toBe("hey @ada ");
    expect(mention.text).toBe("@ada");
    expect(text.slice(mention.offset, mention.offset + mention.text.length)).toBe(
      "@ada",
    );
  });

  /**
   * A display name with NO word characters at all — `🙂`, `!!!` — slugs to an
   * empty string, so the server's `user_slugs` now falls back to the email
   * local part to keep such a person mentionable. The picker row therefore
   * carries a real, typable alias, and the inserted spelling must be that
   * alias rather than the unusable symbol label.
   */
  it("inserts the fallback slug when a symbol-only name has no word characters", () => {
    const symbolOnly: Mentionable = {
      target: { kind: "user", id: "u1" },
      label: "🙂",
      aliases: ["🙂", "smiley"],
    };
    const { text, mention } = insertMention("hey @", { start: 4, end: 5 }, symbolOnly);
    expect(text).toBe("hey @smiley ");
    expect(mention.text).toBe("@smiley");
  });

  /** A name the host *can* open keeps its friendly label as the inserted text. */
  it("keeps the display label when it opens a mention", () => {
    const { text } = insertMention("hey @", { start: 4, end: 5 }, jane);
    expect(text).toBe("hey @Jane Doe ");
  });

  /**
   * The `#` spelling is desk-only server-side (`extract_with_known` narrows
   * `@#…` to desk targets), so a user whose display label starts with `#`
   * must fall back to a plain alias rather than inserting a visually
   * desk-shaped `@#Ada` that revalidation would still accept as the user.
   */
  it("falls back off a `#`-prefixed label for a user row", () => {
    const hashUser: Mentionable = {
      target: { kind: "user", id: "u1" },
      label: "#Ada",
      aliases: ["#ada", "ada"],
    };
    const { text, mention } = insertMention("hey @", { start: 4, end: 5 }, hashUser);
    expect(text).toBe("hey @ada ");
    expect(mention.text).toBe("@ada");
    expect(
      text.slice(mention.offset, mention.offset + mention.text.length),
    ).toBe("@ada");
  });

  /** On a desk row the `#` spelling is the point — `@#support` resolves. */
  it("keeps the `#` spelling on a desk row", () => {
    const hashDesk: Mentionable = {
      target: { kind: "desk", id: "#support" },
      label: "#support",
      aliases: ["#support", "support"],
    };
    const { text, mention } = insertMention("hey @", { start: 4, end: 5 }, hashDesk);
    expect(text).toBe("hey @#support ");
    expect(mention.text).toBe("@#support");
  });

  /**
   * A display label carrying inline Markdown delimiters — `Ada *Ops*` — would
   * be inserted verbatim as `@Ada *Ops*`. The host routes that mention fine,
   * but react-markdown splits the raw span across text and formatting nodes,
   * so `chipMentions` can never match the full mention text against one node
   * and no notification chip renders. The typable slug falls back instead.
   */
  it("falls back to a slug when the label contains Markdown delimiters", () => {
    const star: Mentionable = {
      target: { kind: "user", id: "u1" },
      label: "Ada *Ops*",
      aliases: ["ada *ops*", "ada-ops"],
    };
    const { text, mention } = insertMention("hey @", { start: 4, end: 5 }, star);
    expect(text).toBe("hey @ada-ops ");
    expect(mention.text).toBe("@ada-ops");
    expect(
      text.slice(mention.offset, mention.offset + mention.text.length),
    ).toBe("@ada-ops");
  });

  /**
   * An intraword underscore is not emphasis in CommonMark — `@Jane_Smith`
   * renders as one literal text node — so a `_` in the label does not stop
   * the friendly spelling from winning. Only delimiters that change the
   * rendered text do.
   */
  it("keeps a label whose underscore is intraword", () => {
    const underscore: Mentionable = {
      target: { kind: "user", id: "u1" },
      label: "Jane_Smith",
      aliases: ["jane_smith", "jane-smith"],
    };
    const { text, mention } = insertMention("hey @", { start: 4, end: 5 }, underscore);
    expect(text).toBe("hey @Jane_Smith ");
    expect(mention.text).toBe("@Jane_Smith");
  });
});

describe("mentionsOutsideRange", () => {
  const a: Mention = { target: { kind: "user", id: "a" }, text: "@Sam", offset: 0 };

  it("keeps a mention the replacement range does not touch", () => {
    const out = mentionsOutsideRange([a], { start: 6, end: 10 });
    expect(out).toEqual([a]);
  });

  it("drops a mention the replacement range overlaps", () => {
    expect(mentionsOutsideRange([a], { start: 0, end: 4 })).toEqual([]);
    expect(mentionsOutsideRange([a], { start: 1, end: 3 })).toEqual([]);
    expect(mentionsOutsideRange([a], { start: 3, end: 8 })).toEqual([]);
  });

  /**
   * The picker-replacement case this exists for: replacing the *first* of two
   * `@Sam` spans must not let the replaced identity re-anchor onto the second,
   * hand-typed occurrence at send time.
   */
  it("drops only the mention being replaced, keeping a same-text duplicate", () => {
    const b: Mention = { target: { kind: "user", id: "b" }, text: "@Sam", offset: 10 };
    const out = mentionsOutsideRange([a, b], { start: 0, end: 4 });
    expect(out).toEqual([b]);
  });

  /**
   * The composer's whole pick path: `insertMention` over an existing picked
   * `@Sam`, with the replaced mention dropped before reconcile. Sam's identity
   * must not survive onto the second, hand-typed `@Sam` — the message pings
   * engineer for the replacement and nobody for the raw literal.
   */
  it("does not re-anchor a replaced mention onto a same-text duplicate", () => {
    const draft = "@Sam then @Sam";
    const range = { start: 0, end: 4 };
    const result = insertMention(draft, range, engineer);
    const survivors = mentionsOutsideRange([a], range);
    const out = reconcileMentions(result.text, [...survivors, result.mention]);
    expect(out).toEqual([
      { target: { kind: "agent", id: "engineer" }, text: "@engineer", offset: 0 },
    ]);
  });
});

describe("reconcileMentions", () => {
  const mention: Mention = {
    target: { kind: "agent", id: "engineer" },
    text: "@engineer",
    offset: 4,
  };

  it("keeps a mention whose span is untouched", () => {
    expect(reconcileMentions("hey @engineer ok", [mention])).toEqual([mention]);
  });

  /** Backspacing through a chip has to un-mention it. */
  it("drops a mention whose text has been edited away", () => {
    expect(reconcileMentions("hey @engine ok", [mention])).toEqual([]);
  });

  it("re-anchors a mention that merely shifted", () => {
    const shifted = reconcileMentions("well hey @engineer ok", [mention]);
    expect(shifted).toHaveLength(1);
    expect(shifted[0].offset).toBe(9);
  });

  it("does not collapse two mentions of the same person onto one span", () => {
    const twice: Mention[] = [
      { ...mention, offset: 0 },
      { ...mention, offset: 10 },
    ];
    const out = reconcileMentions("@engineer @engineer", twice);
    expect(out.map((m) => m.offset)).toEqual([0, 10]);
  });

  /**
   * Two identical literals can name two different targets. Deleting the first
   * leaves the second's text — but at the first's old offset. A greedy forward
   * scan sees "the first span still there", keeps it, and drops the survivor;
   * the server then notifies the wrong person. The survivor has to keep its
   * identity.
   */
  it("keeps the survivor when two identical spans collapse onto one", () => {
    const a: Mention = { target: { kind: "user", id: "a" }, text: "@Sam", offset: 0 };
    const b: Mention = { target: { kind: "user", id: "b" }, text: "@Sam", offset: 6 };
    const out = reconcileMentions("@Sam", [a, b]);
    expect(out).toHaveLength(1);
    expect(out[0].target).toEqual(b.target);
    expect(out[0].offset).toBe(0);
  });

  /**
   * The mirror image of the test above, and why the composer hands the
   * pre-edit text over. Deleting the *second* of two `@Sam @Sam` mentions
   * leaves the first's text — but `@Sam` with both spans recorded is input-
   * identical to the first-deleted case, so no text-only rule can tell them
   * apart. The deleted region in `previous` picks out the mention the edit
   * actually removed, and the surviving first Sam keeps its identity instead
   * of being dropped and pinging the deleted second Sam.
   */
  it("keeps the first duplicate when the second is deleted", () => {
    const a: Mention = { target: { kind: "user", id: "a" }, text: "@Sam", offset: 0 };
    const b: Mention = { target: { kind: "user", id: "b" }, text: "@Sam", offset: 5 };
    const out = reconcileMentions("@Sam", [a, b], "@Sam @Sam");
    expect(out).toHaveLength(1);
    expect(out[0].target).toEqual(a.target);
    expect(out[0].offset).toBe(0);
  });

  /**
   * The deletion fix only sees the deleted region, and a pure insertion leaves
   * that region empty — so without the same check collapsing onto the
   * insertion point, breaking the first span's text (`@Sam` -> `@Sxam`) would
   * re-anchor the mention onto the unrelated hand-typed `@Sam` and ping its
   * target on the wrong span. The edit touched the mention, so it is dropped.
   */
  it("drops a mention broken by an insertion rather than re-anchoring to an unrelated occurrence", () => {
    const a: Mention = { target: { kind: "user", id: "a" }, text: "@Sam", offset: 0 };
    const out = reconcileMentions("@Sxam then @Sam", [a], "@Sam then @Sam");
    expect(out).toHaveLength(0);
  });

  it("keeps a mention when the insertion is outside its span", () => {
    const a: Mention = { target: { kind: "user", id: "a" }, text: "@Sam", offset: 0 };
    const out = reconcileMentions("hi @Sam", [a], "@Sam");
    expect(out).toHaveLength(1);
    expect(out[0].target).toEqual(a.target);
    expect(out[0].offset).toBe(3);
  });

  it("keeps the selected identity when an identical span is inserted before it", () => {
    const selected: Mention = {
      target: { kind: "user", id: "selected" },
      text: "@Sam",
      offset: 0,
    };
    const out = reconcileMentions("@Sam @Sam", [selected], "@Sam", 0);
    expect(out.map((m) => [m.target, m.offset])).toEqual([
      [{ kind: "user", id: "selected" }, 5],
    ]);
  });

  it("preserves the selected identity after a literal duplicate is inserted", () => {
    const selected: Mention = {
      target: { kind: "user", id: "selected" },
      text: "@Sam",
      offset: 0,
    };
    const out = reconcileMentions("@Sam @Sam", [selected], "@Sam", 5);
    expect(out.map((m) => [m.target, m.offset])).toEqual([
      [{ kind: "user", id: "selected" }, 5],
    ]);
  });
  it("returns them in reading order", () => {
    const out = reconcileMentions("@engineer and @engineer", [
      { ...mention, offset: 14 },
      { ...mention, offset: 0 },
    ]);
    expect(out.map((m) => m.offset)).toEqual([0, 14]);
  });
});

describe("reconcileWrap", () => {
  const sam: Mention = { target: { kind: "user", id: "u1" }, text: "@Sam", offset: 0 };

  /**
   * Wrapping a whole mention in `**` keeps its literal intact (`**@Sam**`
   * still reads `@Sam`) — the mention keeps its target and just shifts past
   * the leading mark. This is the case `reconcileMentions` would over-drop,
   * because it treats the entire edited region as deleted.
   */
  it("keeps an enclosed mention, shifted past the leading mark", () => {
    const out = reconcileWrap([{ ...sam, offset: 2 }], 2, 6, "**");
    expect(out).toEqual([{ ...sam, offset: 4 }]);
  });

  it("leaves a mention before the selection untouched", () => {
    const out = reconcileWrap([{ ...sam, offset: 0 }], 5, 8, "**");
    expect(out).toEqual([{ ...sam, offset: 0 }]);
  });

  it("shifts a mention after the selection past both marks", () => {
    const out = reconcileWrap([{ ...sam, offset: 3 }], 0, 3, "**");
    expect(out).toEqual([{ ...sam, offset: 7 }]);
  });

  /**
   * The failure `wrap` has to prevent: a Bold insertion *inside* the picked
   * `@Sam` (selection `[1, 4)`) breaks the literal, so send-time text-only
   * reconciliation would re-anchor it onto an unrelated hand-typed `@Sam` and
   * ping its target on the wrong span. The broken mention has to go instead.
   */
  it("drops a mention whose span an insertion point falls inside", () => {
    const out = reconcileWrap([{ ...sam, offset: 0 }], 1, 4, "**");
    expect(out).toEqual([]);
  });

  it("shifts a mention whose span merely abuts the selection", () => {
    const out = reconcileWrap([{ ...sam, offset: 0 }], 0, 4, "**");
    expect(out).toEqual([{ ...sam, offset: 2 }]);
  });

  it("handles a caret-only wrap at a mention's start", () => {
    const out = reconcileWrap([{ ...sam, offset: 5 }], 5, 5, "**");
    expect(out).toEqual([{ ...sam, offset: 9 }]);
  });
});

describe("mentionRegex", () => {
  /**
   * The rule that keeps a chip honest: it is a claim that somebody was
   * notified, so it is drawn only where the host actually delivered a mention.
   */
  it("matches nothing when there are no mentions", () => {
    const re = mentionRegex([]);
    expect("@engineer and @everyone".match(re)).toBeNull();
  });

  it("matches only the delivered spans", () => {
    const re = mentionRegex([{ text: "@engineer" }]);
    const hits = "@engineer told @nobody".match(re);
    expect(hits).toEqual(["@engineer"]);
  });

  it("prefers the longer span when one name prefixes another", () => {
    const re = mentionRegex([{ text: "@Ann" }, { text: "@Ann Lee" }]);
    expect("@Ann Lee".match(re)).toEqual(["@Ann Lee"]);
  });

  it("escapes regex metacharacters in a label", () => {
    const re = mentionRegex([{ text: "@a.b(c)" }]);
    expect("@a.b(c)".match(re)).toEqual(["@a.b(c)"]);
    expect("@axbyc".match(re)).toBeNull();
  });
});

describe("mentionablesFor", () => {
  const directory = {
    agents: [
      { id: "engineer", name: "Ada", role: "Backend Engineer" },
      { id: "ceo", name: "Rae", role: "Chief Executive" },
    ],
    people: [{ id: "u1", label: "Jane Doe", slug: "jane-doe" }],
    desks: [{ id: "engineering", name: "Engineering", memberIds: ["engineer"] }],
    everyone: { label: "everyone", aliases: ["everyone", "channel", "here"] },
  };

  it("marks only the teammates on this channel", () => {
    const rows = mentionablesFor(directory, ["engineer"]);
    const byLabel = Object.fromEntries(rows.map((r) => [r.label, r]));
    expect(byLabel.Ada.inChannel).toBe(true);
    expect(byLabel.Rae.inChannel).toBe(false);
    // Everyone can see every desk, so a person is never "outside" one.
    expect(byLabel["Jane Doe"].inChannel).toBeUndefined();
  });

  it("reaches a teammate by id or by display name", () => {
    const rows = mentionablesFor(directory, []);
    expect(rankMentionables(rows, "ada")[0].label).toBe("Ada");
  });

  /**
   * A desk called `engineering` and a teammate called `engineer` both match
   * "engineer", and the desk outranks an off-channel teammate by group. The
   * exact match has to win anyway: the person has already typed exactly who
   * they mean.
   */
  it("puts an exact match above a better-ranked partial one", () => {
    const rows = mentionablesFor(directory, []);
    expect(rankMentionables(rows, "engineer")[0].label).toBe("Ada");
    // And the desk is still offered, just second.
    expect(rankMentionables(rows, "engineer")[1].label).toBe("engineering");
  });

  it("takes the broadcast spellings from the host, not from a constant", () => {
    const rows = mentionablesFor(
      { ...directory, everyone: { label: "all", aliases: ["all"] } },
      [],
    );
    const broadcast = rows.find((r) => r.target.kind === "everyone");
    expect(broadcast?.label).toBe("all");
    expect(broadcast?.aliases).toEqual(["all"]);
  });

  it("says how many teammates a desk would address", () => {
    const rows = mentionablesFor(directory, []);
    expect(rows.find((r) => r.target.kind === "desk")?.hint).toContain("1 teammate");
  });

  it("excludes the current user from the picker when selfId is provided", () => {
    const rows = mentionablesFor(directory, [], "engineer");
    const labels = rows.map((r) => r.label);
    expect(labels).not.toContain("Ada");
    expect(labels).toContain("Rae");
    expect(labels).toContain("Jane Doe");
    expect(labels).toContain("everyone");
  });

  it("excludes a person from the picker when selfId matches", () => {
    const dirWithPerson = {
      ...directory,
      people: [
        { id: "me", label: "Me Myself", slug: "me" },
        { id: "u2", label: "Other Person", slug: "other" },
      ],
    };
    const rows = mentionablesFor(dirWithPerson, [], "me");
    const labels = rows.map((r) => r.label);
    expect(labels).not.toContain("Me Myself");
    expect(labels).toContain("Other Person");
  });

  /**
   * Two people can share a display name; the host mints each a distinct slug
   * so one can be told from the other. Rows that would otherwise be
   * indistinguishable have to say which one they will ping.
   */
  it("shows the slug when two people share a display name", () => {
    const dirWithSams = {
      agents: [],
      people: [
        { id: "u1", label: "Sam", slug: "sam-1" },
        { id: "u2", label: "Sam", slug: "sam-2" },
      ],
      desks: [],
      everyone: { label: "everyone", aliases: ["everyone"] },
    };
    const rows = mentionablesFor(dirWithSams, []);
    const hints = rows.filter((r) => r.target.kind === "user").map((r) => r.hint);
    expect(hints).toEqual(["Person — @sam-1", "Person — @sam-2"]);
  });

  it("keeps the plain hint for a name nobody shares", () => {
    const rows = mentionablesFor(directory, []);
    const jane = rows.find((r) => r.target.kind === "user");
    expect(jane?.hint).toBe("Person");
  });

  it("includes everyone when selfId is not provided", () => {
    const rows = mentionablesFor(directory, []);
    const labels = rows.map((r) => r.label);
    expect(labels).toContain("Ada");
    expect(labels).toContain("Rae");
    expect(labels).toContain("Jane Doe");
    expect(labels).toContain("everyone");
  });
});

describe("mentionsOutsideChannel", () => {
  const onChannel: Mention = {
    target: { kind: "agent", id: "engineer" },
    text: "@engineer",
    offset: 0,
  };
  const offChannel: Mention = {
    target: { kind: "agent", id: "ceo" },
    text: "@ceo",
    offset: 10,
  };

  it("names a teammate who cannot see this channel", () => {
    expect(mentionsOutsideChannel([onChannel, offChannel], ["engineer"])).toEqual([
      "ceo",
    ]);
  });

  it("is silent when membership is unknown, not noisy", () => {
    expect(mentionsOutsideChannel([offChannel], undefined)).toEqual([]);
  });

  it("never warns about a person, who can see every desk", () => {
    const person: Mention = {
      target: { kind: "user", id: "u1" },
      text: "@Jane Doe",
      offset: 0,
    };
    expect(mentionsOutsideChannel([person], ["engineer"])).toEqual([]);
  });

  it("deduplicates when the same teammate is mentioned twice", () => {
    const dup: Mention = {
      target: { kind: "agent", id: "ceo" },
      text: "@ceo",
      offset: 20,
    };
    expect(mentionsOutsideChannel([offChannel, dup], ["engineer"])).toEqual([
      "ceo",
    ]);
  });

  it("expands a desk mention to its members for the channel check", () => {
    const desk: Mention = {
      target: { kind: "desk", id: "eng" },
      text: "@eng",
      offset: 0,
    };
    const deskRow: Mentionable = {
      target: { kind: "desk", id: "eng" },
      label: "eng",
      aliases: ["eng", "engineering"],
      memberIds: ["engineer", "ceo"],
    };
    expect(
      mentionsOutsideChannel([desk], ["engineer"], [deskRow]),
    ).toEqual(["ceo"]);
  });

  it("leaves a desk mention alone when membership is not supplied", () => {
    const desk: Mention = {
      target: { kind: "desk", id: "eng" },
      text: "@eng",
      offset: 0,
    };
    expect(mentionsOutsideChannel([desk], ["engineer"])).toEqual([]);
  });
});

describe("resolvableMentions", () => {
  const directory: Mentionable[] = [
    {
      target: { kind: "agent", id: "engineer" },
      label: "engineer",
      aliases: ["engineer"],
      inChannel: true,
    },
    {
      target: { kind: "user", id: "u1" },
      label: "Jane Doe",
      aliases: ["jane doe", "jane-doe"],
    },
    {
      target: { kind: "everyone" },
      label: "everyone",
      aliases: ["everyone", "channel", "here"],
    },
  ];

  it("resolves every span the directory can name", () => {
    const out = resolvableMentions("@engineer @Jane Doe", directory);
    expect(out.map((m) => m.text)).toEqual(["@engineer", "@Jane Doe"]);
    expect(out[0].target).toEqual({ kind: "agent", id: "engineer" });
  });

  it("leaves a name shared by two targets as text", () => {
    const sam1: Mentionable = {
      target: { kind: "user", id: "u1" },
      label: "Sam",
      aliases: ["sam"],
    };
    const sam2: Mentionable = {
      target: { kind: "user", id: "u2" },
      label: "Sam",
      aliases: ["sam"],
    };
    expect(resolvableMentions("@Sam", [sam1, sam2])).toEqual([]);
  });

  it("ignores code regions", () => {
    expect(resolvableMentions("`@engineer`", directory)).toEqual([]);
    expect(
      resolvableMentions("before\n```\n@engineer\n```\nafter", directory),
    ).toEqual([]);
  });

  /**
   * An unequal backtick run is not code, so a mention inside it that opens
   * and closes must resolve: `` `code @engineer here`` `` (one opener, two
   * trailing) is rendered with `@engineer` visible, and the host's fallback
   * extraction resolves it — the console must not blank the span and send an
   * explicit empty list that suppresses the host's read.
   */
  it("resolves a mention inside an unequal, unclosed backtick run", () => {
    const out = resolvableMentions("`code @engineer here``", directory);
    expect(out.map((m) => m.text)).toEqual(["@engineer"]);
    expect(out[0].target).toEqual({ kind: "agent", id: "engineer" });
  });

  it("still does not resolve an @ immediately after a backtick", () => {
    // `@` after a backtick is not a mention-opening position on either side,
    // matching the host's `opens_mention` and the `jane@acme.com` rule.
    expect(resolvableMentions("`@engineer``", directory)).toEqual([]);
  });

  it("does not resolve an @ inside another token", () => {
    expect(resolvableMentions("jane@engineer", directory)).toEqual([]);
    expect(resolvableMentions("/docs/@eng", directory)).toEqual([]);
  });

  it("matches the host's ASCII-only mention boundaries", () => {
    // `opens_mention`/`closes_mention` accept only ASCII whitespace. A pasted
    // non-breaking space must neither open nor close a mention here, or the
    // picker would offer a span the host then drops.
    expect(resolvableMentions("hey\u00A0@engineer", directory)).toEqual([]);
    expect(resolvableMentions("@engineer\u00A0next", directory)).toEqual([]);
  });

  it("prefers the longest alias when one name prefixes another", () => {
    const ann: Mentionable = {
      target: { kind: "user", id: "a" },
      label: "Ann",
      aliases: ["ann"],
    };
    const annLee: Mentionable = {
      target: { kind: "user", id: "b" },
      label: "Ann Lee",
      aliases: ["ann lee"],
    };
    const out = resolvableMentions("@Ann Lee", [ann, annLee]);
    expect(out).toHaveLength(1);
    expect(out[0].text).toBe("@Ann Lee");
    expect(out[0].target).toEqual(annLee.target);
  });

  it("requires a clean boundary after the name", () => {
    expect(resolvableMentions("@engineerish", directory)).toEqual([]);
    expect(resolvableMentions("@engineer, thanks", directory)).toHaveLength(1);
  });

  it("resolves the desk-only `@#desk` spelling", () => {
    const desks: Mentionable[] = [
      {
        target: { kind: "desk", id: "engineering" },
        label: "engineering",
        aliases: ["engineering", "eng"],
      },
    ];
    const out = resolvableMentions("@#engineering", desks);
    expect(out).toHaveLength(1);
    expect(out[0].target).toEqual({ kind: "desk", id: "engineering" });
    expect(out[0].text).toBe("@#engineering");
  });

  it("treats `@#name` as desk-only, never matching a person or agent", () => {
    const rows: Mentionable[] = [
      {
        target: { kind: "agent", id: "eng" },
        label: "#engineering",
        aliases: ["engineering"],
      },
      {
        target: { kind: "desk", id: "engineering" },
        label: "engineering",
        aliases: ["engineering"],
      },
    ];
    const out = resolvableMentions("@#engineering", rows);
    expect(out).toHaveLength(1);
    expect(out[0].target).toEqual({ kind: "desk", id: "engineering" });
  });

  it("resolves the picker's label even when no alias repeats it", () => {
    const rows: Mentionable[] = [
      {
        target: { kind: "everyone" },
        label: "All hands",
        aliases: ["everyone", "channel", "here"],
      },
    ];
    const out = resolvableMentions("@All hands", rows);
    expect(out).toHaveLength(1);
    expect(out[0].target).toEqual({ kind: "everyone" });
    expect(out[0].text).toBe("@All hands");
  });
});

describe("utf8ByteLength", () => {
  it("counts bytes, not UTF-16 units", () => {
    expect(utf8ByteLength("")).toBe(0);
    expect(utf8ByteLength("hey @engineer")).toBe(13);
    expect(utf8ByteLength("👍 ")).toBe(5);
    expect(utf8ByteLength("é")).toBe(2);
    expect(utf8ByteLength("café")).toBe(5);
  });
});
