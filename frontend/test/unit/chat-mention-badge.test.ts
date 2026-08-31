import { describe, expect, it } from "vitest";

import type { NotificationDto } from "@/api/types";
import {
  mentionCountsByChannel,
  mentionsToClear,
  renderedChannelIdForContext,
  threadViewAdvancesChannel,
  threadsToReReadForMentions,
} from "@/lib/mention-badge";

/**
 * The mention badge is the durable half of the feature: the SSE feed only
 * reaches an open browser, so a mention that landed overnight is visible here
 * and nowhere else. Getting the counting wrong therefore does not degrade the
 * feature, it removes it — a badge that clears too eagerly loses the summons
 * entirely, with nothing left to notice it by.
 */

const note = (over: Partial<NotificationDto> & Pick<NotificationDto, "id">): NotificationDto => ({
  kind: "mention",
  subjectKind: "message",
  subjectId: "42",
  title: "someone mentioned you",
  createdAt: 1,
  context: "engineering",
  ...over,
});

describe("mentionCountsByChannel", () => {
  it("counts unread mentions per channel", () => {
    expect(
      mentionCountsByChannel([
        note({ id: "a" }),
        note({ id: "b" }),
        note({ id: "c", context: "design" }),
      ]),
    ).toEqual({ engineering: 2, design: 1 });
  });

  it("ignores a mention that has been read", () => {
    expect(
      mentionCountsByChannel([note({ id: "a", readAt: 5 }), note({ id: "b" })]),
    ).toEqual({ engineering: 1 });
  });

  /**
   * `kind`, not `subjectKind`. A later notification about a message that is not
   * a mention — a reply, a reaction — must not silently start badging as one.
   */
  it("counts only rows whose kind is a mention", () => {
    expect(
      mentionCountsByChannel([
        note({ id: "a", kind: "reply" }),
        note({ id: "b" }),
      ]),
    ).toEqual({ engineering: 1 });
  });

  it("drops a row with no channel rather than placing it arbitrarily", () => {
    expect(mentionCountsByChannel([note({ id: "a", context: undefined })])).toEqual({});
  });

  it("maps the legacy main thread onto the rendered main channel", () => {
    expect(mentionCountsByChannel([note({ id: "a", context: "main" })], "general")).toEqual({
      general: 1,
    });
  });

  /**
   * An unaddressed message (an API client omitting `chat`) lands in the General
   * desk and can store `"General"` or `""` as its context. The rail has no
   * `General` row — it is built from the company's real desk ids — so every
   * spelling of the default desk has to badge the rendered main channel, or the
   * mention renders nowhere and can never be cleared.
   */
  it("maps every spelling of the General desk onto the rendered main channel", () => {
    for (const context of ["General", "general", ""]) {
      expect(mentionCountsByChannel([note({ id: "a", context })], "general")).toEqual({
        general: 1,
      });
    }
  });
  it("is empty for an empty feed", () => {
    expect(mentionCountsByChannel([])).toEqual({});
  });

  /**
   * `mainChannelId` may be `undefined` while the desk list has not resolved,
   * or for a company with no desks at all. A general-chat spelling then has
   * nowhere to badge — it must be dropped, not placed under a `""` channel the
   * rail never has (which would render nowhere and could never be cleared).
   * Direct desk/DM ids are unaffected: they badge from the rendered set.
   */
  it("drops general-chat spellings when there is no rendered main channel", () => {
    const feed = [
      note({ id: "main", context: "main" }),
      note({ id: "general", context: "General" }),
      note({ id: "dm", context: "dm:teammate" }),
    ];
    expect(mentionCountsByChannel(feed, undefined, new Set(["dm:teammate"]))).toEqual({
      "dm:teammate": 1,
    });
  });

  /**
   * A company can name a desk `general` (or `main`) and not put it first. The
   * host then stores the *canonical* desk id under that name, and the id is a
   * real rendered channel — so the exact match must win over the
   * legacy-spelling alias, or a mention meant for that desk would badge the
   * default thread, and opening the default thread would silently clear it.
   */
  describe("with a real desk id that matches a legacy spelling", () => {
    const rendered = new Set(["engineering", "general", "design"]);

    it("badges the real desk, not the default thread", () => {
      expect(
        mentionCountsByChannel([note({ id: "a", context: "general" })], "engineering", rendered),
      ).toEqual({ general: 1 });
    });

    it("still aliases a spelling that names no rendered channel", () => {
      expect(
        mentionCountsByChannel(
          [
            note({ id: "a", context: "main" }),
            note({ id: "b", context: "General" }),
            note({ id: "c", context: "" }),
          ],
          "engineering",
          rendered,
        ),
      ).toEqual({ engineering: 3 });
    });
  });

  /**
   * A host answering `GET {scope}/notifications` with something other than the
   * documented shape must not take the console down.
   *
   * This is not hypothetical: a mocked host that returns a bare `[]` for
   * unmatched routes made `feed.notifications` `undefined`, and iterating it
   * threw during render — blanking the entire app and failing every unrelated
   * spec in the file. The badge is the least important thing on the screen and
   * has to fail like it.
   */
  it("survives a caller handing it something that is not a list", () => {
    for (const bad of [undefined, null, "nope", 7, {}]) {
      expect(
        mentionCountsByChannel(bad as unknown as NotificationDto[]),
      ).toEqual({});
    }
  });
});

describe("mentionsToClear", () => {
  const feed = [
    note({ id: "eng-1" }),
    note({ id: "eng-2" }),
    note({ id: "eng-read", readAt: 9 }),
    note({ id: "design-1", context: "design" }),
  ];

  /**
   * The case a bare "mark all read" gets wrong: opening one channel must not
   * clear a summons waiting in another.
   */
  it("clears only the opened channel's unread mentions", () => {
    expect(mentionsToClear(feed, "engineering")).toEqual(["eng-1", "eng-2"]);
    expect(mentionsToClear(feed, "design")).toEqual(["design-1"]);
  });

  it("clears legacy main mentions when the rendered main channel is opened", () => {
    expect(mentionsToClear([note({ id: "a", context: "main" })], "general", "general")).toEqual([
      "a",
    ]);
  });
  it("clears General-desk spellings when the rendered main channel is opened", () => {
    for (const context of ["General", "general", ""]) {
      expect(mentionsToClear([note({ id: "a", context })], "general", "general")).toEqual(["a"]);
    }
  });
  it("returns nothing for a channel with no mentions", () => {
    expect(mentionsToClear(feed, "random")).toEqual([]);
  });

  /**
   * The mirror of the count-arm drop: with `mainChannelId` undefined the count
   * never badged a general-chat spelling, so opening any real channel must not
   * clear one either — clearing would hand the clear back to the host for a
   * notification that never rendered.
   */
  it("never clears a general-chat mention when no rendered main channel exists", () => {
    expect(mentionsToClear([note({ id: "a", context: "General" })], "engineering", undefined)).toEqual(
      [],
    );
  });

  /**
   * Same real-desk-id hazard as the count: when `general` is a real desk that
   * is not the default thread, its canonical id is a rendered channel, so
   * opening the *default* thread must not clear it — that is the summons
   * somebody would miss — and only opening the real desk clears it.
   */
  describe("with a real desk id that matches a legacy spelling", () => {
    const rendered = new Set(["engineering", "general", "design"]);
    const feed = [note({ id: "general-1", context: "general" })];

    it("clears the real desk's mention only when that desk is opened", () => {
      expect(
        mentionsToClear(feed, "general", "engineering", new Set(["general"]), rendered),
      ).toEqual(["general-1"]);
      expect(
        mentionsToClear(feed, "engineering", "engineering", new Set(["engineering"]), rendered),
      ).toEqual([]);
    });

    it("still clears a non-rendered legacy spelling when the default thread is opened", () => {
      expect(
        mentionsToClear(
          [note({ id: "a", context: "main" })],
          "engineering",
          "engineering",
          new Set(["engineering"]),
          rendered,
        ),
      ).toEqual(["a"]);
    });
  });

  /**
   * A mention inside a thread reply must not clear on channel-open alone: the
   * main timeline folds replies into their parent (`buildTimeline`), so a
   * collapsed thread hides the text even while the channel is on screen —
   * clearing it would lose the summons without the person ever seeing it. The
   * notification names the message by its host sequence, which the loaded
   * transcript's reply map keys by the console's `h<seq>` id.
   */
  describe("with a mention inside a thread reply", () => {
    const replies = new Map([["h42", "h7"]]);
    const feed = [note({ id: "threaded", context: "engineering", subjectId: "42" })];

    it("keeps it unread while the channel is open but its thread is collapsed", () => {
      expect(mentionsToClear(feed, "engineering", "engineering", new Set(["engineering"]), new Set(), replies, null)).toEqual([]);
    });

    it("clears it the moment the thread panel makes the reply visible", () => {
      expect(
        mentionsToClear(feed, "engineering", "engineering", new Set(["engineering"]), new Set(), replies, "h7"),
      ).toEqual(["threaded"]);
    });

    it("clears a different thread's mention only when that thread is the open one", () => {
      // A reply under another parent is still hidden: opening a sibling thread
      // must not clear it either.
      expect(
        mentionsToClear(feed, "engineering", "engineering", new Set(["engineering"]), new Set(), replies, "h99"),
      ).toEqual([]);
    });
  });

  it("still clears a top-level mention on channel open", () => {
    // A message with no parent id is on screen the moment the channel is: the
    // reply gate must not hold it hostage.
    expect(
      mentionsToClear(feed, "engineering", "engineering", new Set(["engineering"]), new Set(), new Map(), null),
    ).toEqual(["eng-1", "eng-2"]);
  });

  /**
   * An empty list is a real instruction to the host ("mark nothing"), distinct
   * from omitting ids ("mark everything") — so the caller must not send it as
   * though it meant the latter.
   */
  it("returns an empty list rather than undefined when there is nothing to clear", () => {
    expect(mentionsToClear([], "engineering")).toEqual([]);
  });

  /**
   * A mention whose subject message is outside the loaded history window must
   * not silently clear — the person was never shown the summoning text, and
   * clearing it would lose the summons for good (Codex P1).
   */
  describe("with loadedMessageIds restricting what is visible", () => {
    const loaded = new Set(["h1", "h2", "h7"]);

    it("clears a top-level mention whose subject IS in the loaded transcript", () => {
      expect(
        mentionsToClear(
          [note({ id: "visible", subjectId: "1" })],
          "engineering",
          "engineering",
          new Set(["engineering"]),
          new Set(),
          new Map(),
          null,
          loaded,
        ),
      ).toEqual(["visible"]);
    });

    it("keeps a top-level mention whose subject is NOT in the loaded transcript", () => {
      expect(
        mentionsToClear(
          [note({ id: "ghost", subjectId: "99" })],
          "engineering",
          "engineering",
          new Set(["engineering"]),
          new Set(),
          new Map(),
          null,
          loaded,
        ),
      ).toEqual([]);
    });

    it("still clears a thread reply mention when its parent thread is open and the reply is loaded", () => {
      const replies = new Map([["h42", "h7"]]);
      const loadedWithReply = new Set(["h1", "h2", "h7", "h42"]);
      expect(
        mentionsToClear(
          [note({ id: "r", context: "engineering", subjectId: "42" })],
          "engineering",
          "engineering",
          new Set(["engineering"]),
          new Set(),
          replies,
          "h7",
          loadedWithReply,
        ),
      ).toEqual(["r"]);
    });
  });

  /**
   * The Conversation read path for a main-thread mention on a company with
   * real desks. The badge maps `main` onto the first desk's rail channel, but
   * that channel hydrates the desk's own thread — so the main-thread subject
   * can never be in the channel's loaded set. Conversation renders the `main`
   * thread, and reports that view with the *thread's* loaded ids; when those
   * contain the subject, the mention must clear exactly as a desk-channel
   * mention would. This is the case that makes the "subject must be loaded"
   * gate usable rather than a permanent badge: the surface that shows the
   * message supplies the set that proves it.
   */
  describe("with a main-context mention read through the Conversation main thread", () => {
    const feed = [note({ id: "main-1", context: "main", subjectId: "7" })];

    it("clears it when the main thread has loaded the subject", () => {
      expect(
        mentionsToClear(
          feed,
          "engineering",
          "engineering",
          new Set(["engineering"]),
          new Set(["engineering"]),
          new Map(),
          null,
          new Set(["h7"]),
        ),
      ).toEqual(["main-1"]);
    });

    it("keeps it while the main thread has not loaded the subject", () => {
      expect(
        mentionsToClear(
          feed,
          "engineering",
          "engineering",
          new Set(["engineering"]),
          new Set(["engineering"]),
          new Map(),
          null,
          new Set(["h1", "h2"]),
        ),
      ).toEqual([]);
    });
  });
});

describe("renderedChannelIdForContext", () => {
  it("places an exact rendered channel id outright", () => {
    expect(
      renderedChannelIdForContext("engineering", "general", new Set(["engineering", "design"])),
    ).toBe("engineering");
  });

  it("aliases every general-chat spelling onto the rendered main channel", () => {
    const rendered = new Set(["general", "engineering"]);
    for (const spelling of ["main", "General", "GENERAL", ""]) {
      expect(renderedChannelIdForContext(spelling, "general", rendered)).toBe("general");
    }
  });

  it("drops a general spelling with no rendered main channel", () => {
    expect(renderedChannelIdForContext("main", undefined, new Set(["engineering"]))).toBeUndefined();
  });

  it("keeps a non-rendered desk id rather than aliasing it", () => {
    // A real desk whose id happens to be `general` wins outright; a context
    // that names no rendered channel falls through to the alias, and anything
    // else stays itself so a yet-to-render channel is not lost.
    expect(renderedChannelIdForContext("design", "general", new Set(["engineering"]))).toBe(
      "design",
    );
  });

  it("returns undefined for a missing context", () => {
    expect(renderedChannelIdForContext(undefined, "general", new Set(["engineering"]))).toBeUndefined();
    expect(renderedChannelIdForContext(null, "general", new Set(["engineering"]))).toBeUndefined();
  });
});

describe("threadViewAdvancesChannel", () => {
  /**
   * Conversation reports every thread view through the same channel-id path as
   * `onChannelViewed`. For a desk or DM thread that is accurate — the thread's
   * transcript IS the channel's — so the full channel-view side effects
   * (advancing the unread floor and persisted read marker) are correct there.
   */
  it("advances a desk thread's own channel", () => {
    expect(threadViewAdvancesChannel("engineering", "engineering")).toBe(true);
  });

  it("advances a DM thread's channel (same transcript, different id)", () => {
    expect(threadViewAdvancesChannel("ada", "dm:ada")).toBe(true);
  });

  /**
   * A company with no configured desks runs on the fallback `main` desk, whose
   * channel id is also `main` — the thread and the channel are the same store,
   * so reading the thread is reading the channel.
   */
  it("advances the main thread when it is its own channel", () => {
    expect(threadViewAdvancesChannel("main", "main")).toBe(true);
  });

  /**
   * The hazard that makes the gate worth having (Codex P1): with real desks the
   * rail has no `General` row, so `main` aliases the first desk's channel for
   * *badging*, but the main thread's transcript is the legacy General
   * conversation — a different store from the desk's own. Advancing the desk's
   * read state here would permanently un-badge unread lines the operator never
   * saw. The mention clear still applies; this only withholds the read advance.
   */
  it("withholds the read advance when the main thread aliases a real desk", () => {
    expect(threadViewAdvancesChannel("main", "engineering")).toBe(false);
  });
});

describe("threadsToReReadForMentions", () => {
  // The console's thread → channel map: `main` aliases the first desk, desks
  // keep their own id as channel, DMs are `dm:<member>`.
  const byThread: Record<string, string> = {
    main: "general",
    engineering: "engineering",
    ada: "dm:ada",
  };
  const loaded = {
    engineering: new Set(["h1", "h2"]),
    "dm:ada": new Set(["h7"]),
  };

  it("re-reads the thread of a mention whose subject is not loaded", () => {
    expect(
      threadsToReReadForMentions(
        [note({ id: "m", context: "engineering", subjectId: "42" })],
        loaded,
        byThread,
        "general",
        new Set(),
      ),
    ).toEqual({ threadIds: ["engineering"], subjects: ["h42"] });
  });

  it("re-reads a DM thread for a dm: context", () => {
    expect(
      threadsToReReadForMentions(
        [note({ id: "m", context: "dm:ada", subjectId: "9" })],
        loaded,
        byThread,
        "general",
        new Set(),
      ),
    ).toEqual({ threadIds: ["ada"], subjects: ["h9"] });
  });

  it("resolves a general-chat mention onto the main thread", () => {
    expect(
      threadsToReReadForMentions(
        [note({ id: "m", context: "General", subjectId: "5" })],
        loaded,
        byThread,
        "general",
        new Set(),
      ),
    ).toEqual({ threadIds: ["main"], subjects: ["h5"] });
  });

  it("skips a mention whose subject is already loaded", () => {
    expect(
      threadsToReReadForMentions(
        [note({ id: "m", context: "engineering", subjectId: "1" })],
        loaded,
        byThread,
        "general",
        new Set(),
      ),
    ).toEqual({ threadIds: [], subjects: [] });
  });

  it("skips mentions already seen this session", () => {
    expect(
      threadsToReReadForMentions(
        [note({ id: "m", context: "engineering", subjectId: "42" })],
        loaded,
        byThread,
        "general",
        new Set(["h42"]),
      ),
    ).toEqual({ threadIds: [], subjects: [] });
  });

  it("skips read and non-mention rows", () => {
    expect(
      threadsToReReadForMentions(
        [
          note({ id: "read", context: "engineering", subjectId: "42", readAt: 3 }),
          { ...note({ id: "reaction", context: "engineering", subjectId: "42" }), kind: "reaction" },
        ],
        loaded,
        byThread,
        "general",
        new Set(),
      ),
    ).toEqual({ threadIds: [], subjects: [] });
  });

  it("dedupes two missing mentions that share a thread", () => {
    expect(
      threadsToReReadForMentions(
        [
          note({ id: "m1", context: "engineering", subjectId: "42" }),
          note({ id: "m2", context: "engineering", subjectId: "43" }),
        ],
        loaded,
        byThread,
        "general",
        new Set(),
      ),
    ).toEqual({ threadIds: ["engineering"], subjects: ["h42", "h43"] });
  });

  it("skips a context no channel renders", () => {
    expect(
      threadsToReReadForMentions(
        [note({ id: "m", context: "ghost-desk", subjectId: "42" })],
        loaded,
        byThread,
        "general",
        new Set(),
      ),
    ).toEqual({ threadIds: [], subjects: [] });
  });

  it("re-reads the first desk's own thread, not the main alias", () => {
    // The first configured desk is not the legacy General thread. `channelMap`
    // inserts the `main` alias before the desk's self-mapping, so a naive
    // channel→thread lookup would select `main` and re-read the General
    // conversation — never the desk's own — leaving the mentioned message
    // absent and the badge stuck until reload (Codex P2).
    const firstDeskByThread: Record<string, string> = {
      main: "engineering",
      engineering: "engineering",
      ada: "dm:ada",
    };
    expect(
      threadsToReReadForMentions(
        [note({ id: "m", context: "engineering", subjectId: "42" })],
        {},
        firstDeskByThread,
        "engineering",
        new Set(),
      ),
    ).toEqual({ threadIds: ["engineering"], subjects: ["h42"] });
  });

  it("still re-reads the main thread for a legacy general mention on the first desk", () => {
    const firstDeskByThread: Record<string, string> = {
      main: "engineering",
      engineering: "engineering",
      ada: "dm:ada",
    };
    expect(
      threadsToReReadForMentions(
        [note({ id: "m", context: "General", subjectId: "5" })],
        {},
        firstDeskByThread,
        "engineering",
        new Set(),
      ),
    ).toEqual({ threadIds: ["main"], subjects: ["h5"] });
  });
});
