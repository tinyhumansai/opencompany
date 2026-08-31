import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import { MAIN_THREAD_ID } from "@/lib/chat";
import {
  defaultDesks,
  deskClaimsGeneralChannel,
  GENERAL_CHANNEL,
  isGeneralChannel,
  type Desk,
} from "@/lib/desks";
import type { TeamMember } from "@/lib/team";
import {
  buildChannels,
  channelForThread,
  channelIdForThread,
  channelMembers,
  directMessageChannels,
  directMessageForId,
  dmThreadId,
  memberForThread,
} from "@/views/chat/model";

/**
 * The built-in `#general` channel (issue #1743).
 *
 * The defect it closes is narrow and easy to miss: `#general` existed only in
 * `defaultDesks()`, the **fallback** set used when the host exposes no desks at
 * all. So the moment a company had real desks — which is every shipped company
 * — the company-wide line vanished from the rail, and there was nowhere to
 * address everyone.
 *
 * Three properties are pinned here, and each is a requirement rather than a
 * rendering detail:
 *
 * 1. it is present whatever the host's desk list says, including when that list
 *    is long and real;
 * 2. its membership is the roster, derived on every render, with nothing
 *    written anywhere — so a teammate added a moment ago is in it;
 * 3. it is not a desk, and carries no desk affordance, because it never reaches
 *    the surfaces that offer them.
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

const ROSTER: TeamMember[] = [
  member({ id: "ceo", name: "Ada", role: "Chief", isOrchestrator: true }),
  member({ id: "eng", name: "Blake", role: "Engineer" }),
];

const DESKS: Desk[] = [
  { id: "engineering", channel: "engineering", name: "Engineering", blurb: "", members: ["eng"] },
  { id: "growth", channel: "growth", name: "Growth", blurb: "", members: ["ceo"] },
];

function channels(members: TeamMember[], desks: Desk[]) {
  return buildChannels(members, desks, {}).find((s) => s.id === "channels")!.channels;
}

describe("the built-in #general channel", () => {
  it("is the first channel in a company that has real desks", () => {
    const rail = channels(ROSTER, DESKS);
    expect(rail.map((c) => c.name)).toEqual([GENERAL_CHANNEL, "engineering", "growth"]);
    expect(rail[0].kind).toBe("channel");
  });

  it("replaces the static fallback's main row rather than sitting beside it", () => {
    const rail = channels(ROSTER, defaultDesks());
    expect(rail.filter((c) => c.name === GENERAL_CHANNEL)).toHaveLength(1);
    // And the one that survived is the derived channel, not the members-less
    // fallback row.
    expect(rail[0].memberIds).toEqual(["ceo", "eng"]);
  });

  it("steps aside for a blueprint desk that authored the id `general`", () => {
    // The host grandfathers this: `is_general_channel` is guarded on
    // `!record.desk_exists`, so such a desk keeps its lead and its writes and
    // `responder_for` routes to that lead. Adding the built-in one beside it
    // put two `#general` rows in the rail folding onto one transcript — the
    // host's `is_general_chat` treats `main` and `general` as one conversation
    // — while a send could pick either responder.
    const authored: Desk[] = [
      { id: "general", channel: "general", name: "Ops lead", blurb: "The line", members: ["ceo"] },
      ...DESKS,
    ];
    const rail = channels(ROSTER, authored);
    expect(rail.filter((c) => isGeneralChannel(c.id))).toHaveLength(1);
    expect(rail.map((c) => c.id)).toEqual(["general", "engineering", "growth"]);
    // And it is the real desk that survived: its lead, its blurb, its members.
    expect(rail[0].voice).toBe("Ops lead");
    expect(rail[0].memberIds).toEqual(["ceo"]);
  });

  it("keeps a blueprint desk that authored the id `main` instead of hiding it", () => {
    // The reverse failure: this desk was filtered out of the rail, so the
    // built-in channel took the slot and named the orchestrator as who answers
    // — while the host still routed `main` to this desk's lead, because
    // `responder_for` checks desks first. The UI both hid a real desk and
    // misstated the responder.
    const authored: Desk[] = [
      { id: "main", channel: "general", name: "Front office", blurb: "The line", members: ["eng"] },
      ...DESKS,
    ];
    const rail = channels(ROSTER, authored);
    expect(rail.filter((c) => isGeneralChannel(c.id))).toHaveLength(1);
    expect(rail.map((c) => c.id)).toEqual(["main", "engineering", "growth"]);
    expect(rail[0].voice).toBe("Front office");
    expect(rail[0].memberIds).toEqual(["eng"]);
    // Not the derived channel's claim about who picks up an unmentioned message.
    expect(rail[0].purpose).toBe("The line");
  });

  it("steps aside for a blueprint desk whose display *name* is General", () => {
    // The host matches a desk key by id **or** case-insensitive name
    // (`resolve_desk_id`), and reserves both spellings against newly created
    // desks — so `[[group_chat]] id = "ops", name = "General"` is a real,
    // grandfathered case. An id-only test rendered the built-in channel *and*
    // this desk, whose own `#` name is also `general`: two rows, one host
    // conversation. It is not cosmetic either — `everyone_desk` folds the
    // console's `main` to `General`, `resolve_desk_id("General")` then selects
    // `ops`, and `@everyone` on the line scopes to that desk's members.
    const namedGeneral: Desk[] = [
      { id: "ops", channel: "general", name: "General", blurb: "The line", members: ["ceo"] },
      ...DESKS,
    ];
    const rail = channels(ROSTER, namedGeneral);
    expect(rail.filter((c) => c.name === GENERAL_CHANNEL)).toHaveLength(1);
    expect(rail.map((c) => c.id)).toEqual(["ops", "engineering", "growth"]);
    // The desk survived, with its own membership — not the derived roster.
    expect(rail[0].memberIds).toEqual(["ceo"]);
    expect(rail[0].purpose).toBe("The line");
    // And every General spelling resolves to it, since nothing else renders
    // the line — the same rule an id-declared desk gets.
    for (const spelling of ["", "main", "General", "general"]) {
      expect(channelIdForThread(spelling, namedGeneral, ROSTER)).toBe("ops");
    }
  });

  it("does not step aside for a desk merely named after something else", () => {
    // The guard is the four spellings the host folds, not a fuzzy match: a
    // desk called `Generals` or `Main Street` claims nothing.
    const nearby: Desk[] = [
      { id: "ops", channel: "generals", name: "Generals", blurb: "", members: ["ceo"] },
      { id: "street", channel: "main-street", name: "Main Street", blurb: "", members: ["eng"] },
    ];
    const rail = channels(ROSTER, nearby);
    expect(rail.map((c) => c.id)).toEqual([MAIN_THREAD_ID, "ops", "street"]);
    expect(channelIdForThread("main", nearby, ROSTER)).toBe(MAIN_THREAD_ID);
  });

  it("has no fabricated general desk left in the fallback set to be confused for one", () => {
    // The rule above is "a desk claims the line". That is only a fact about the
    // company if the console has stopped inventing such a desk itself.
    expect(defaultDesks().some((d) => isGeneralChannel(d.id))).toBe(false);
  });

  it("is present on a company with no desks at all", () => {
    expect(channels(ROSTER, [])[0].name).toBe(GENERAL_CHANNEL);
  });

  it("holds the whole roster, derived — a teammate added later is in it", () => {
    const before = channels(ROSTER, DESKS)[0];
    expect(channelMembers(before, ROSTER)!.map((m) => m.id)).toEqual(["ceo", "eng"]);

    const grown = [...ROSTER, member({ id: "designer", name: "Cass", role: "Designer" })];
    const after = channels(grown, DESKS)[0];
    expect(channelMembers(after, grown)!.map((m) => m.id)).toEqual(["ceo", "eng", "designer"]);

    // Nothing about the desk list changed to make that true.
    expect(DESKS.map((d) => d.members)).toEqual([["eng"], ["ceo"]]);
  });

  it("names the orchestrator as who picks up an unmentioned message", () => {
    expect(channels(ROSTER, DESKS)[0].purpose).toBe(
      "Everyone's here. Ada picks up anything you don't @-mention.",
    );
  });

  it("makes no claim about who answers when the host does not say", () => {
    const silent = ROSTER.map((m) => ({ ...m, isOrchestrator: undefined }));
    expect(channels(silent, DESKS)[0].purpose).toBe(
      "Everyone's here — the whole company on one line",
    );
  });

  it("carries no overlay membership, so no surface can offer a remove", () => {
    // `overlayMembers` is what the console reads to decide a member is
    // removable. A desk row has it; the built-in channel has no such concept —
    // the `Channel` shape it produces carries only ids.
    const general = channels(ROSTER, DESKS)[0];
    expect(Object.keys(general).sort()).toEqual(
      ["id", "kind", "memberIds", "name", "purpose", "voice"].sort(),
    );
  });

  it("is absent from the desk list every desk affordance is built from", () => {
    // The host does not list it under `GET .../desks`, so the org chart, the
    // assignee picker and the desk counts never see it. This asserts the
    // console does not put it back: `buildChannels` composes it for the rail
    // and returns a `Channel`, never a `Desk`.
    expect(DESKS.some((d) => isGeneralChannel(d.id))).toBe(false);
  });
});

describe("resolving a host thread to the general channel", () => {
  it("folds every spelling the host journals it under", () => {
    for (const spelling of ["", "main", "General", "general", "MAIN"]) {
      expect(channelIdForThread(spelling, DESKS, ROSTER)).toBe("main");
    }
  });

  it("leaves a real desk thread alone", () => {
    expect(channelIdForThread("engineering", DESKS, ROSTER)).toBe("engineering");
  });

  it("still resolves a teammate DM", () => {
    expect(channelIdForThread("eng", DESKS, ROSTER)).toBe("dm:eng");
  });

  it("keeps the line for the company when a teammate's id is a General spelling", () => {
    // The host reserves `main` and `general` against newly minted teammates
    // (`RESERVED_AGENT_IDS`), but a manifest can still declare one. This used
    // to answer `dm:main` — the roster was consulted before the fold — and the
    // consequence was not cosmetic: `GET chat/history?desk=main` returns the
    // **folded General conversation** (`is_general_chat` has folded `""`,
    // `main`, `General` and `general` into one since issue #65), so the
    // company-wide line was hydrated into that teammate's DM on every reload
    // while `#general` itself resolved to no channel and got none.
    //
    // The host settles it: `responder_for` now answers the bare key as the
    // orchestrator and routes the teammate's DM under `dm:<id>`, so the
    // transcript and the responder finally name the same conversation. This
    // mirrors that order — desk, then the General fold, then the roster.
    const withMain = [...ROSTER, member({ id: "main", name: "Mainard" })];
    expect(channelIdForThread("main", DESKS, withMain)).toBe("main");
    for (const spelling of ["", "General", "general", "MAIN"]) {
      expect(channelIdForThread(spelling, DESKS, withMain)).toBe("main");
    }
    // Every other teammate's DM is untouched — this moved one key, not the rule.
    expect(channelIdForThread("eng", DESKS, withMain)).toBe("dm:eng");
    // A desk still outranks both, exactly as it does on the host.
    const deskMain: Desk[] = [
      { id: "main", channel: "front-office", name: "Front office", blurb: "", members: ["eng"] },
    ];
    expect(channelIdForThread("main", deskMain, withMain)).toBe("main");
    // And with nobody claiming it, the fold is unchanged.
    expect(channelIdForThread("main", DESKS, ROSTER)).toBe("main");
  });

  /**
   * ...and that teammate's DM is readable, which is the other half.
   *
   * The fold above is deliberate — `chat_responder("main")` is `None`, so a
   * teammate called `main` cannot capture the company's line — but it left the
   * DM writable and unreadable at once: `ChatView` addressed the host with the
   * bare `member.id` (issue #364 re-keyed DMs onto it), so a message composed
   * in that DM was written and answered in `#general`, under a transcript the
   * DM could not read back.
   *
   * Both ends therefore have to agree on the prefixed address: the host answers
   * it (`chat_responder("dm:main") == Some("main")`), and this resolves the
   * frames it emits under that key. Asserted here because the comment above has
   * claimed this routing since #1743 while nothing held the sender to it.
   */
  it("reads back the DM of a teammate whose id is a General spelling", () => {
    const withMain = [...ROSTER, member({ id: "main", name: "Mainard" })];
    expect(channelIdForThread("dm:main", DESKS, withMain)).toBe("dm:main");
    // The bare key still belongs to the company, unchanged by the arm above.
    expect(channelIdForThread("main", DESKS, withMain)).toBe("main");
    // The prefix resolves nobody it should not.
    expect(channelIdForThread("dm:nobody", DESKS, withMain)).toBeNull();
    expect(channelIdForThread("dm:", DESKS, withMain)).toBeNull();
    // An ordinary teammate is reachable either way, so no existing DM moved.
    expect(channelIdForThread("eng", DESKS, withMain)).toBe("dm:eng");
    expect(channelIdForThread("dm:eng", DESKS, withMain)).toBe("dm:eng");
  });

  /**
   * ...and the DM is reachable from the console, which is the third half.
   *
   * `directMessageChannels` used to filter this teammate out, correctly: while
   * the DM was addressed by the bare id, a row here opened the company's line
   * under that teammate's name. The prefixed address removes that reason, and
   * leaving the filter in place would have left the new route working and
   * unreachable — no row in the New message picker, and `directMessageForId`
   * rejecting an explicit `#/chat/dm:main` link, which is the one address the
   * rest of this change exists to honour.
   */
  it("offers that teammate as a DM target, now that the address is its own", () => {
    const withMain = [...ROSTER, member({ id: "main", name: "Mainard" })];
    const ids = directMessageChannels(withMain).map((c) => c.id);
    expect(ids).toContain("dm:main");
    // And the deep link resolves, which is what the picker's row opens.
    expect(directMessageForId(withMain, "dm:main")?.name).toBe("Mainard");
    // Every other teammate is still offered exactly once.
    expect(ids).toContain("dm:eng");
    expect(new Set(ids).size).toBe(ids.length);
  });

  /**
   * And the sender actually uses it.
   *
   * `activeThreadId` is what `ChatView` puts in `chat`. The resolver above is
   * inert unless that one DM is addressed prefixed, so this holds the sender to
   * the same rule rather than trusting the comment beside it.
   */
  it("addresses that one DM prefixed, and leaves every other bare", () => {
    const view = readFileSync(
      new URL("../../src/views/ChatView.tsx", import.meta.url),
      "utf8",
    );
    expect(view).toContain("dmThreadId(active.member)");
  });

  /**
   * ...and so does every other seam that turns a member into a thread id.
   *
   * The sender was only the first. The shell seeds the live thread -> channel
   * map from the roster and builds its rehydration targets from it, and the
   * Approvals page resolves an origin back from a recorded thread; each one
   * left on the bare id put that DM's replies, its recovered history, or its
   * approval link back on the company's line. One rule, asked in one place.
   */
  it("asks the same question at the map, the hydration and the approval origin", () => {
    const shell = readFileSync(
      new URL("../../src/components/app-shell.tsx", import.meta.url),
      "utf8",
    );
    // The live thread -> channel map.
    expect(shell).toContain("members.map(dmThreadId)");
    expect(shell).not.toContain("members.map((m) => m.id)");
    // The rehydration targets.
    expect(shell).toContain("threadId: dmThreadId(m)");
    expect(shell).not.toContain("threadId: m.id,");

    const card = readFileSync(
      new URL("../../src/components/approval-card.tsx", import.meta.url),
      "utf8",
    );
    expect(card).toContain("memberForThread(members, approval.thread)");
    expect(card).not.toContain("candidate.id === approval.thread");
  });

  /**
   * A second, independent lookup in the same file as the one above: resolving
   * the *channel* an approval's thread renders in (`ApprovalMeta`'s "Asked in"
   * link), not resolving the *member* a DM thread addresses (`originOf`). Both
   * have to fold General spellings the same way `channelForThread` does — a
   * bare `chatChannelByThread[a.thread]` index misses any casing other than
   * the map's own literal keys, which is exactly the gap `channelForThread`
   * exists to close for every other thread-to-channel lookup in the shell
   * (issue #1781 review, Codex P2).
   */
  it("resolves the approval-meta channel link through channelForThread, not a bare index", () => {
    const card = readFileSync(
      new URL("../../src/components/approval-card.tsx", import.meta.url),
      "utf8",
    );
    expect(card).toContain("channelForThread(chatChannelByThread, a.thread)");
    expect(card).not.toContain("chatChannelByThread?.[a.thread]");
  });

  /**
   * The rule itself, rather than the call sites that apply it.
   */
  it("addresses only the General-spelling teammate prefixed", () => {
    const mainard = member({ id: "main", name: "Mainard" });
    const eng = member({ id: "eng", name: "Engie" });
    expect(dmThreadId(mainard)).toBe("dm:main");
    expect(dmThreadId(eng)).toBe("eng");
    // And the reverse lookup accepts either address.
    const roster = [eng, mainard];
    expect(memberForThread(roster, "dm:main")?.id).toBe("main");
    expect(memberForThread(roster, "eng")?.id).toBe("eng");
    expect(memberForThread(roster, "dm:eng")?.id).toBe("eng");
    expect(memberForThread(roster, "nobody")).toBeNull();
  });

  it("lets a blueprint desk that authored a general id keep its own thread", () => {
    const authored: Desk[] = [
      { id: "general", channel: "general", name: "General", blurb: "", members: ["ceo"] },
    ];
    expect(channelIdForThread("general", authored, ROSTER)).toBe("general");
  });

  it("sends every other spelling to that desk too, since nothing else renders the line", () => {
    // `buildChannels` adds no built-in channel beside a grandfathered desk, so
    // answering `main` here would name a channel that does not exist — and a
    // live frame, an unread badge or an approval link addressed to it would
    // land in a bucket the operator cannot open.
    const authored: Desk[] = [
      { id: "general", channel: "ops-room", name: "Ops lead", blurb: "", members: ["ceo"] },
    ];
    for (const spelling of ["", "main", "General", "general", "MAIN"]) {
      expect(channelIdForThread(spelling, authored, ROSTER)).toBe("general");
    }
    // And the same, the other way round, for a desk that authored `main`.
    const authoredMain: Desk[] = [
      { id: "main", channel: "front-office", name: "Front office", blurb: "", members: ["eng"] },
    ];
    for (const spelling of ["", "main", "General", "general"]) {
      expect(channelIdForThread(spelling, authoredMain, ROSTER)).toBe("main");
    }
  });
});

describe("deskClaimsGeneralChannel", () => {
  // Mirrors the host's `resolve_desk_id`, which matches a desk by id **or** by
  // case-insensitive name. Testing the id alone was the gap: the host reserves
  // both spellings when a desk is created, so a desk answering to `General` by
  // name is exactly as real, and exactly as grandfathered, as one answering by
  // id.
  it("matches on the id or the display name", () => {
    expect(deskClaimsGeneralChannel({ id: "general", name: "Ops lead" })).toBe(true);
    expect(deskClaimsGeneralChannel({ id: "main", name: "Front office" })).toBe(true);
    expect(deskClaimsGeneralChannel({ id: "ops", name: "General" })).toBe(true);
    expect(deskClaimsGeneralChannel({ id: "ops", name: "MAIN" })).toBe(true);
  });

  it("matches nothing else", () => {
    expect(deskClaimsGeneralChannel({ id: "engineering", name: "Engineering" })).toBe(false);
    expect(deskClaimsGeneralChannel({ id: "ops", name: "Generals" })).toBe(false);
    expect(deskClaimsGeneralChannel({ id: "street", name: "Main Street" })).toBe(false);
  });
});

describe("isGeneralChannel", () => {
  it("matches exactly the four spellings the host folds", () => {
    for (const yes of ["", "main", "Main", "MAIN", "general", "General"]) {
      expect(isGeneralChannel(yes)).toBe(true);
    }
    for (const no of ["engineering", "generals", "main-line", "dm:main"]) {
      expect(isGeneralChannel(no)).toBe(false);
    }
  });

  /**
   * **Case-folded, never trimmed** — because the host is not.
   *
   * `is_general_chat` compares with `eq_ignore_ascii_case` against the string
   * exactly as journaled, so a client that posts `chat: "  Main  "` has that
   * spelling stored verbatim. Trimming here was strictly worse than being
   * strict: the console rendered the live reply in `#general` while
   * `chat/history?desk=main` did not return it, so the message vanished on the
   * next reload. A frame that lands and then disappears reads as data loss.
   *
   * Asserted rather than merely dropped from the list above: silence about
   * padding is what let the two sides disagree in the first place.
   */
  it("does not trim, because the host does not", () => {
    for (const padded of ["  main  ", " general", "General ", "\tmain", "main\n"]) {
      expect(isGeneralChannel(padded)).toBe(false);
    }
  });
});

/**
 * The two desk affordances `ChatView` derives from a channel, and why neither
 * may reach the built-in one.
 *
 * A full `ChatView` render needs the whole client and every hook, so this uses
 * the source-contract idiom `chat-rail-focus.test.ts` established for exactly
 * that case: pin the wiring the behaviour rests on. The behaviour itself is
 * verified in a browser — see the PR.
 *
 * Both gates matter because `#general` is the one channel that carries
 * `memberIds` **without** being a desk. Every previous non-desk channel (a DM,
 * a static fallback desk) was excluded by having no `memberIds` at all, so both
 * tests read as "has membership ⇒ is a desk" — an inference that is true of
 * every channel except this one.
 */
describe("ChatView offers no desk affordance on the built-in channel", () => {
  const here = dirname(fileURLToPath(import.meta.url));
  const chatView = readFileSync(resolve(here, "../../src/views/ChatView.tsx"), "utf8");
  // Collapsed so an assertion pins the wiring rather than the line wrapping
  // Prettier happens to choose for it.
  const source = chatView.replace(/\s+/g, " ");

  it("decides by the desk list, not by the id's spelling", () => {
    // Both affordances hang off this one predicate. Asking the desk list is
    // what makes the built-in channel excluded for the right reason — and what
    // keeps a blueprint desk that authored a General id from being hidden with
    // it, since the host grandfathers that desk and the org chart holds it.
    expect(source).toContain(
      'const activeIsDesk = active.kind === "channel" && (desks ?? []).some((d) => d.id === active.id);',
    );
  });

  it("does not badge anyone its lead", () => {
    // `memberIds[0]` is the roster's first row here, not a hierarchy.
    expect(source).toContain("activeIsDesk && !active.leadless ? active.memberIds?.[0] : undefined }");
  });

  it("does not offer the org-chart link that would open on a desk that does not exist", () => {
    expect(source).toContain("onManageDesk={ activeIsDesk && active.memberIds");
  });

  it("no longer needs the id predicate at all", () => {
    expect(source).toContain('import { defaultDesks, type Desk } from "@/lib/desks";');
    expect(source).not.toContain("isGeneralChannel(active.id)");
  });
});

/**
 * The shell's host-thread → channel map, and the line #1743 had to change.
 *
 * `channelMap` used to seed `map[MAIN_THREAD_ID] = desks[0].id`: with no
 * `#general` channel to land in, the company's main line was parked on
 * whichever desk sorted first so it would at least be somewhere findable.
 * That is now wrong in a way a browser makes obvious — an unaddressed message
 * and its reply rendered inside `#engineering`, with an unread badge, while
 * the host's own history for that desk was empty.
 *
 * The map is module-private to `app-shell.tsx`, so this pins the wiring the
 * same way the `ChatView` block above does.
 */
describe("the shell maps the main line to #general, not to the first desk", () => {
  const here2 = dirname(fileURLToPath(import.meta.url));
  const shell = readFileSync(resolve(here2, "../../src/components/app-shell.tsx"), "utf8").replace(
    /\s+/g,
    " ",
  );

  it("no longer parks the main line on the first desk", () => {
    expect(shell).not.toContain("map[MAIN_THREAD_ID] = desks[0].id");
  });

  it("maps every spelling the host journals the general line under", () => {
    // Through `channelIdForThread`, so the one rule above decides — not a
    // second copy that can disagree with the rail about where the line renders.
    expect(shell).toContain(
      'for (const spelling of ["", MAIN_THREAD_ID, "General", GENERAL_CHANNEL]) { const channelId = channelIdForThread(spelling, desks, members); if (channelId) map[spelling] = channelId; }',
    );
  });

  it("lands an unaddressed system line in #general rather than the first desk", () => {
    expect(shell).toContain(
      "setFirstDeskChannelId(channelIdForThread(MAIN_THREAD_ID, chatDesks, roster));",
    );
  });

  it("names #general as a rehydration target, since it is in no desk list", () => {
    expect(shell).toContain(
      "channelId: channelIdForThread(MAIN_THREAD_ID, chatDesks, roster) ?? MAIN_THREAD_ID,",
    );
  });

  /**
   * The last-resort `.catch` handler is a second, independent path to the
   * same landing/rehydration decision the two tests above pin for the success
   * path — and `defaultDesks()` dropping its fabricated `main` row regressed
   * it the same way, one call further down: `fallbackDesks[0]?.id` used to
   * agree with the company-wide line only by accident (the first fallback
   * desk happened to be that fabricated row), and the explicit
   * `{ channelId: MAIN_THREAD_ID, threadId: MAIN_THREAD_ID }` rehydration
   * entry it also carried was dropped with it — so `mainThread()` stayed in
   * `threadIds` (via `defaultThreads()`) with no channel to rehydrate
   * through (issue #1781 review, Codex P2/medium).
   */
  it("lands the unexpected-error fallback on #general too, not the first fallback desk", () => {
    expect(shell).toContain("setFirstDeskChannelId(MAIN_THREAD_ID);");
    expect(shell).not.toContain("setFirstDeskChannelId(fallbackDesks[0]?.id ?? null);");
  });

  it("names #general as a rehydration target on the unexpected-error fallback too", () => {
    expect(shell).toContain(
      "{ channelId: MAIN_THREAD_ID, threadId: MAIN_THREAD_ID }, ...fallbackDesks.map((d) => ({ channelId: d.id, threadId: d.id })),",
    );
  });

  it("hydrates a DM from a thread id that actually belongs to that DM", () => {
    // A DM's history is fetched under the address it is written on. For a
    // teammate whose id is a General spelling the bare id is *not* that
    // address — the host folds it, and `GET chat/history?desk=main` answers
    // with the whole company-wide conversation, so the DM's own transcript
    // could never be recovered after a reload. Both halves go through the
    // shared rules (`dmThreadId`, `channelIdForThread`) rather than repeating a
    // literal here that can disagree with the rail.
    expect(shell).toContain("threadId: dmThreadId(m)");
    expect(shell).toContain(
      "channelId: channelIdForThread(dmThreadId(m), chatDesks, roster) ?? dmChannelId(m)",
    );
    expect(shell).not.toContain(
      "...roster.map((m) => ({ channelId: dmChannelId(m), threadId: m.id })),",
    );
  });
});

/**
 * The shell's thread → channel lookup, which cannot be a plain `map[id]`.
 *
 * The host compares the General spellings case-insensitively and then emits, on
 * every live event, the raw id the caller addressed. A map of four literals
 * therefore misses `MAIN` or `GENERAL` from an API client — and a missed frame
 * is a reply and a working indicator that simply never appear until polling
 * recovers the durable history.
 */
describe("resolving a live frame's thread id against the shell's map", () => {
  const MAP = { "": "main", main: "main", General: "main", general: "main", eng: "dm:eng" };

  it("matches exactly when it can", () => {
    expect(channelForThread(MAP, "eng")).toBe("dm:eng");
    expect(channelForThread(MAP, "main")).toBe("main");
  });

  it("accepts every casing of a General spelling, as the host does", () => {
    for (const spelling of ["MAIN", "GENERAL", "Main", "gEnErAl"]) {
      expect(channelForThread(MAP, spelling)).toBe("main");
    }
  });

  it("follows the map to a grandfathered desk rather than assuming `main`", () => {
    const owned = { "": "general", main: "general", General: "general", general: "general" };
    expect(channelForThread(owned, "MAIN")).toBe("general");
  });

  it("answers null for a thread the map does not know", () => {
    expect(channelForThread(MAP, "workflows")).toBeNull();
  });
});

/**
 * The shell actually calling `channelForThread` at every thread-to-channel
 * lookup, not just the two the earlier General-channel restoration touched
 * (`channelMap`'s own construction, and `firstDeskChannelId`).
 *
 * PR #1781 review: these four call sites were bare `map[key]` indexes on
 * `chatChannelByThread`/`chatChannelByThreadRef.current`, which the tests
 * above prove misses any casing other than the map's own literal keys and
 * (for the live-reply lookup) drops the frame until polling recovers the
 * durable history. Pinned by source-text, the same idiom
 * `chat-rail-focus.test.ts` established, since a full `AppShell` render needs
 * the whole client and every hook.
 */
describe("the shell resolves every thread-to-channel lookup through channelForThread", () => {
  const here = dirname(fileURLToPath(import.meta.url));
  const shell = readFileSync(resolve(here, "../../src/components/app-shell.tsx"), "utf8");

  it("resolves a live reply's channel through the fold, not a bare index", () => {
    expect(shell).toContain("channelForThread(chatChannelByThread, event.chatId)");
    expect(shell).not.toContain("chatChannelByThread[event.chatId]");
  });

  it("resolves the addressed-notice target through the fold", () => {
    expect(shell).toContain("channelForThread(chatChannelByThread, threadId)");
  });

  it("resolves both `chatChannelByThreadRef.current` lookups through the fold", () => {
    const matches = shell.match(/channelForThread\(chatChannelByThreadRef\.current, threadId\)/g);
    expect(matches?.length ?? 0).toBe(2);
    expect(shell).not.toContain("chatChannelByThreadRef.current[threadId]");
  });

  /**
   * PR #1781 review (Codex P2, comment 3878664647): `channelMap` only knows
   * desks and roster teammates, so `setChatChannelByThread(channelMap(...))`
   * alone never taught `chatChannelByThread` the Operator channel's own
   * id → id pair. `channelForThread(chatChannelByThread, event.chatId)`
   * (pinned above) then missed on `event.chatId === operatorChannel.id` and
   * `renderAgentReply` returned without rendering the live SSE frame — the
   * Operator transcript and its unread state only caught up on the
   * five-second history poll, whose own `channels` rehydration-target list
   * (a few lines further down) already carried this id and was masking the
   * gap. Folding the id into the state map itself, not just the poll
   * targets, is what closes it for the live path too.
   */
  it("folds the Operator channel's id into chatChannelByThread, not just the poll targets", () => {
    expect(shell).toContain(
      "...(operatorChannel ? { [operatorChannel.id]: operatorChannel.id } : {})",
    );
    expect(shell).not.toContain("setChatChannelByThread(channelMap(chatDesks, roster));");
  });
});
