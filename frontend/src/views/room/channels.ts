// What a channel is: the desks, the direct messages, the archived `#general`
// line, and the id grammar that keeps them apart.
//
// Split out of the old `model.ts` (issue: room store / P2). Pure — the view owns
// the state.
//
// The invariants that live here and are easy to break are documented on the
// functions themselves: `#general` is not a desk, a DM id keys on the teammate's
// roster id (with `legacyDmChannelId` still resolving the pre-#1141 spelling),
// and nothing resolves until `/desks` has answered.

// The chat workspace's data model: channels, direct messages, and the grouping
// rules the timeline reads. Everything here is pure — the view owns the state.

import type { DeskDto } from "@/api/types";
import {
  generalAwareChannel,
  MAIN_THREAD_ID,
  type ChatMessage,
} from "@/lib/chat";
import {
  deskClaimsGeneralChannel,
  GENERAL_CHANNEL,
  isGeneralChannel,
  type Desk,
} from "@/lib/desks";
import type { TeamMember } from "@/lib/team";
import type { Transcripts } from "./timeline";

/**
 * A host desk (`GET .../desks`), shaped into the console's `Desk`. The host
 * has no separate channel-slug or blurb field, so the slug is derived from
 * the desk's name and the blurb falls back to its description — the id is
 * the one field that must survive untouched, since it doubles as the chat
 * thread id `send` addresses.
 *
 * `members` / `overlayMembers` come through as the host sent them, order
 * included — `members[0]` is the desk's lead, and the rest is the hierarchy the
 * company declared. Dropping them here is what made every channel show the
 * whole company (issue #369).
 */
export function deskFromDto(d: DeskDto): Desk {
  const slug = d.name
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-|-$/g, "");
  return {
    id: d.id,
    channel: slug || d.id,
    name: d.name,
    blurb: d.description ?? "",
    members: d.members,
    overlayMembers: d.overlayMembers,
    overlayCreated: d.overlayCreated,
    responder: d.responder,
  };
}

export type ChannelKind = "channel" | "dm";

export interface Channel {
  id: string;
  /** The bare name — rendered after a `#` for channels, plain for DMs. */
  name: string;
  /**
   * How the company side signs its messages here. A channel's name is a slug
   * (`front-desk`); its voice is who is speaking (`Front desk`).
   */
  voice?: string;
  kind: ChannelKind;
  /** One line under the title, and the tooltip on it. */
  purpose: string;
  /** Private channels wear a lock instead of a hash. */
  private?: boolean;
  /** Avatar tone key; DMs and desk channels both carry one. */
  tone?: string;
  /** The roster entry behind a DM, when there is one. */
  member?: TeamMember;
  /**
   * Who is in this channel, as roster teammate **ids** in the desk's own order
   * (lead first). Ids rather than resolved `TeamMember`s so this model stays
   * pure and a channel never goes stale when the roster reloads — resolve with
   * {@link channelMembers}.
   *
   * Absent for DMs (a two-person line needs no list) and for the static
   * fallback desks, which have no membership concept; a consumer that finds it
   * absent falls back to the whole roster (issue #369).
   */
  memberIds?: string[];
  /**
   * Whether this channel is a read-only archive — the legacy `#general` line.
   * It renders no composer and offers no membership editing.
   */
  system?: boolean;
  /**
   * Whether this channel has **no lead** (issue #1835): an `auto` desk, whose
   * answerer is picked per message. `memberIds[0]` carries no rank here, so a
   * consumer must not badge it — the host's own `desk_lead` is `None` for such
   * a channel by definition. Absent/false for every lead desk and DM.
   */
  leadless?: boolean;
}

export interface ChannelSection {
  id: string;
  label: string;
  channels: Channel[];
}

/**
 * The channel list.
 *
 * `desks` become the `#channels` — they are the standing lines you can
 * address, and each already carries a name, a blurb, and a tone. Defaults to
 * `lib/desks.ts`'s static set for a host that doesn't expose `.../desks` yet
 * (issue #53); the caller fetches the real ones and passes them in once they
 * land, so a company's own desks show up instead of the generic
 * strategy/creative/front-desk trio. Every roster teammate is a DM, those with
 * a transcript first, newest conversation first.
 *
 * Both kinds post to the same company endpoint. A channel scopes a transcript
 * and gives the company side a stable identity; it is not a separate backend.
 *
 * There is no company-wide channel: a message that names no chat goes to the
 * default agent's DM. The old `#general` history stays readable through
 * {@link legacyGeneralChannel}, which is never offered here.
 */
export function buildChannels(
  members: TeamMember[],
  // Defaults to no desks, not to the fabricated trio. The parameter exists so
  // a caller that has not read `/desks` yet can still build the rail's roster
  // half; standing in three desks the company never declared is the bug this
  // default used to carry into every such caller.
  desks: Desk[] = [],
  transcripts: Transcripts = {},
): ChannelSection[] {
  const channels: Channel[] = desks.map((d) => ({
    id: d.id,
    name: d.channel,
    voice: d.name,
    kind: "channel" as const,
    // An `auto` channel with no blurb of its own states its routing rule —
    // the honest line about who answers, in place of a rank nothing confers
    // (issue #1835). An operator-written blurb still wins.
    purpose:
      d.blurb ||
      (d.responder === "auto"
        ? "Best fit picks up anything you don't @-mention"
        : d.blurb),
    tone: d.tone,
    memberIds: d.members,
    leadless: d.responder === "auto" || undefined,
  }));

  // Every teammate, not only the ones already spoken to.
  //
  // Filtering on a non-empty transcript made the section an *inbox*: it listed
  // conversations that existed. On a company nobody has DM'd yet that is an
  // empty section reading "Nothing here yet.", under a heading naming the one
  // thing an operator most wants from a roster of teammates — and the way to
  // start one was a "+" whose dialog lists exactly the people this section was
  // declining to.
  //
  // A DM channel is not created by being listed. `directMessageChannels` mints
  // one row per member with a `dmChannelId`, and the transcript is whatever the
  // host has for that id — empty until somebody speaks.
  //
  // Ordering carries what the filter used to: anyone with a transcript sorts
  // first, most recent at the top. `latestMessageAt` answers 0 for an empty or
  // absent transcript, so untouched rows tie and fall through to the name — a
  // stable order rather than roster order, which is the host's and can move
  // under a reader.
  const dms = directMessageChannels(members).sort((a, b) => {
    const byRecency =
      latestMessageAt(transcripts[b.id]) - latestMessageAt(transcripts[a.id]);
    return byRecency !== 0 ? byRecency : a.name.localeCompare(b.name);
  });

  return [
    { id: "channels", label: "Channels", channels },
    { id: "dms", label: "Direct messages", channels: dms },
  ];
}

/**
 * The archived company-wide line, as a read-only channel.
 *
 * Only reachable by an explicit `#/chat/main` (or other General spelling) deep
 * link in a company where no blueprint desk claims that line — see
 * {@link generalChannelId}. Its history is still served, so old conversations
 * stay readable; it is never offered as somewhere to type.
 */
export function legacyGeneralChannel(members: TeamMember[]): Channel {
  return {
    id: MAIN_THREAD_ID,
    name: GENERAL_CHANNEL,
    kind: "channel",
    purpose: "Archived company-wide line. Read-only.",
    memberIds: members.map((m) => m.id),
    system: true,
  };
}

/** Every roster teammate as a DM target, including conversations not yet started. */
export function directMessageChannels(members: TeamMember[]): Channel[] {
  return members.map((m) => ({
    id: dmChannelId(m),
    name: m.name,
    kind: "dm" as const,
    // The teammate's **description**, which is the field parallel to a desk's
    // `blurb` above — both answer "what is this line for", and neither repeats
    // what the title already said. This used to read `m.role`, an identity
    // field in a description slot, and that is precisely what made the header
    // say the same words twice (issue #1180): `fromDto` falls back
    // `dto.name?.trim() || dto.role`, so a company that names roles rather than
    // people has name === role, and the title and the slot after the divider
    // resolved to one string. The role is still the fallback — for a teammate
    // the host *did* name it is a real second fact — and {@link channelSubtitle}
    // is what declines to render even that when it just echoes the title.
    purpose: m.description.trim() || m.role,
    tone: m.tone,
    member: m,
  }));
}

/** A DM target addressed by `id`, whether or not it is in the rail yet. */
export function directMessageForId(
  members: TeamMember[],
  id: string | null,
): Channel | null {
  if (!id) return null;
  return (
    directMessageChannels(members).find((channel) => channel.id === id) ?? null
  );
}

function latestMessageAt(messages: ChatMessage[] | undefined): number {
  return (
    messages?.reduce((latest, message) => Math.max(latest, message.at), 0) ?? 0
  );
}

/**
 * A DM's channel id, and so its URL.
 *
 * Keyed on the teammate's **id**, which is now the one stable thing about them
 * (issue #364). A host roster entry has always had a stable agent id; what was
 * missing was a stable id for a console-invented teammate, and `lib/team.ts`'s
 * counter was why this used to key on the name instead.
 *
 * Keying on the name had a cost that only shows up later: renaming a teammate
 * silently moved their DM to a new URL and orphaned every message already
 * journaled under the old one. An id does not change when a person's name does,
 * which is the entire reason to prefer it.
 */
export function dmChannelId(member: TeamMember): string {
  return `dm:${member.id}`;
}

/**
 * The **host thread** a teammate's DM is addressed on — not always the same
 * string as {@link dmChannelId}, which is its console-local channel id.
 *
 * Issue #364 re-keyed DMs onto the bare teammate id, and for every ordinary
 * teammate that is still what this answers. The exception is a teammate whose
 * id is itself a General spelling: the host folds that bare key to the
 * company-wide line before it ever reaches the roster — deliberately, so `main`
 * cannot be captured by a teammate called `main` — and answers it as the
 * orchestrator. Addressed bare, that DM was writable and unreadable at once.
 *
 * So exactly that one teammate is addressed prefixed, which `chat_responder`
 * unwraps and resolves (`chat_responder("dm:main") == Some("main")`).
 *
 * **Every seam that turns a roster member into a thread id has to ask this**,
 * not only the sender: the live thread → channel map, the rehydration targets
 * that recover history after a reload, and the Approvals page resolving an
 * origin back to its conversation. Any one of them left on the bare id puts
 * that DM's replies, its recovered history, or its approval link back on the
 * company's line (issue #1743).
 */
export function dmThreadId(member: TeamMember): string {
  return isGeneralChannel(member.id) ? dmChannelId(member) : member.id;
}

/** The teammate a thread addresses, whether it is bare or `dm:`-prefixed. */
export function memberForThread(
  members: TeamMember[],
  threadId: string,
): TeamMember | null {
  return (
    members.find((m) => dmThreadId(m) === threadId) ??
    members.find((m) => dmChannelId(m) === threadId) ??
    null
  );
}

/**
 * The name-derived DM id this console minted before issue #364.
 *
 * Kept for one release, and for one purpose: a `#/chat/dm:ada-1f3k` link that
 * somebody bookmarked or pasted into a ticket still has to land on Ada's DM.
 * Only {@link resolveDmChannelId} calls it — nothing addresses a channel by it,
 * so no new state is ever written under a legacy id.
 */
export function legacyDmChannelId(member: TeamMember): string {
  const name = member.name.trim();
  const slug = name
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-|-$/g, "");
  return `dm:${slug ? `${slug}-` : ""}${nameHash(name)}`;
}

/**
 * The current DM channel id a URL segment names, or `null` when it names no
 * DM this company has.
 *
 * Resolves the current id first, then the pre-#364 name-derived form, so an old
 * link keeps working without the old id ever becoming addressable again.
 */
export function resolveDmChannelId(
  id: string,
  members: TeamMember[],
): string | null {
  if (!id.startsWith("dm:")) return null;
  const match = members.find(
    (m) => dmChannelId(m) === id || legacyDmChannelId(m) === id,
  );
  return match ? dmChannelId(match) : null;
}

/**
 * The channel id a `#/chat/<id>` hash segment names, with the URL escaping
 * undone.
 *
 * The hash router hands views its raw segments without decoding, while hrefs
 * that mint channel links — an approval card's "Open the conversation" pill —
 * write them with `encodeURIComponent`, so a DM id arrives as `dm%3A<agent-id>`
 * rather than `dm:<agent-id>`. Decode here, the same boundary
 * `taskIdFromSegment` keeps for `#/tasks/<id>`.
 *
 * On a malformed escape the raw segment comes back rather than `null`: a
 * typo'd address should still surface as an unknown channel (issue #370's
 * notice), not silently collapse onto the fallback.
 */
export function channelIdFromSegment(segment: string | null): string | null {
  if (!segment) return null;
  try {
    return decodeURIComponent(segment);
  } catch {
    return segment;
  }
}

function nameHash(name: string): string {
  let hash = 0;
  for (let i = 0; i < name.length; i++)
    hash = (hash * 31 + name.charCodeAt(i)) | 0;
  return (hash >>> 0).toString(36);
}

/**
 * The chat channel a **host thread id** belongs to, or `null` when this
 * company has no channel that owns it.
 *
 * This is the one subtle rule in the chat's addressing, kept in one place so a
 * caller cannot get it wrong twice. A desk's channel id *is* its thread id —
 * {@link deskFromDto} leaves `DeskDto.id` untouched precisely so addressing the
 * channel routes to that desk. A DM's is not: its channel id is the
 * console-local {@link dmChannelId}, while the thread id the host journals
 * under (and `chat` / `chat/history` take) is the roster teammate's agent id.
 *
 * Anything routing a host-side event — which always names a *thread* — into
 * {@link Transcripts} has to make that distinction. Issue #367 is what the
 * console looks like when it doesn't.
 */
export function channelIdForThread(
  threadId: string,
  desks: Desk[],
  members: TeamMember[],
): string | null {
  if (desks.some((d) => d.id === threadId)) return threadId;
  // The archived `#general` line is in no desk list, so it is resolved by
  // name. The host journals it under four ids — `""`, `main`, `General`,
  // `general` — and folds them on read; checked before the roster, in the
  // order the host resolves (`responder_for`: desk, General fold, roster).
  if (isGeneralChannel(threadId)) {
    return generalChannelId(desks);
  }
  const member = members.find((m) => m.id === threadId);
  if (member) return dmChannelId(member);
  // The prefixed form, last, exactly as `chat_responder` unwraps it.
  //
  // A teammate whose id *is* a General spelling cannot be addressed bare — the
  // fold above answers first and the company's line wins, which is deliberate
  // (`chat_responder("main") == None`, "a teammate called `main` does not
  // inherit the company's line"). `RoomView` therefore addresses that one DM as
  // `dm:<id>`, which the host does answer. The frames it emits carry that
  // prefixed key, so without this arm they resolved to no channel at all and
  // the reply never appeared — the DM was writable and unreadable at once.
  const prefixed = threadId.startsWith("dm:")
    ? threadId.slice("dm:".length)
    : null;
  const dmMember = prefixed ? members.find((m) => m.id === prefixed) : null;
  return dmMember ? dmChannelId(dmMember) : null;
}

/**
 * The channel the legacy company-wide line renders in: the archived `main`
 * line ({@link legacyGeneralChannel}), or the id of a blueprint desk that
 * claims a General spelling (`is_general_channel` is guarded on
 * `!record.desk_exists`, so such a desk keeps its own line).
 */
export function generalChannelId(desks: Desk[]): string {
  return desks.find(deskClaimsGeneralChannel)?.id ?? MAIN_THREAD_ID;
}

/**
 * The channel that renders `threadId`, given the shell's thread → channel map.
 *
 * The map holds one entry per id the company can be addressed on, which cannot
 * cover the General spellings exhaustively: the host compares them
 * case-insensitively (`is_general_chat`) and then emits on live events the raw
 * id it was addressed with, so an API client posting to `MAIN` produces frames
 * keyed `MAIN` and a four-literal map misses them. A missed frame is not a
 * cosmetic loss — the reply and the working indicator simply do not appear
 * until polling happens to recover the durable history.
 *
 * So: exact match first, then the same question the host asks.
 */
export function channelForThread(
  map: Readonly<Record<string, string>>,
  threadId: string,
): string | null {
  // One implementation, in `lib/chat.ts`, because `dispatchMarkerPlacement`
  // lives there and has to resolve a thread the same way (issue #1743). A
  // second copy here is how three of the four live-frame consumers ended up
  // indexing the map directly and missing every casing the host accepts.
  return generalAwareChannel(map, threadId);
}

export function findChannel(
  sections: ChannelSection[],
  id: string | null,
): Channel | null {
  if (!id) return null;
  for (const s of sections) {
    const hit = s.channels.find((c) => c.id === id);
    if (hit) return hit;
  }
  return null;
}

/**
 * The first channel across all sections, or `null` when there are none.
 *
 * The last-resort selection, so the chat never renders blank while any channel
 * exists (issue #366), and the address of last resort for a line with no
 * channel of its own (issue #368). Both used to reach for a literal `"main"`
 * instead — an id carried only by the first *fallback* desk, so it matched
 * nothing at all on a company with desks of its own.
 */
export function firstChannel(sections: ChannelSection[]): Channel | null {
  for (const s of sections) {
    if (s.channels.length > 0) return s.channels[0];
  }
  return null;
}

/**
 * A channel's own members, resolved against the roster — `null` when the
 * channel names no membership (a DM, or a fallback desk), which is the caller's
 * cue to fall back to the whole roster rather than draw an empty pane.
 *
 * Maps over the **ids**, not over the roster: the desk's order is meaningful —
 * `memberIds[0]` is the lead — and filtering the roster would silently reorder
 * everyone into roster order and lose that. An id with no roster row (a
 * teammate removed since the desks were fetched) drops out rather than
 * rendering a placeholder for somebody who isn't there.
 */
export function channelMembers(
  channel: Channel,
  roster: TeamMember[],
): TeamMember[] | null {
  if (!channel.memberIds) return null;
  const byId = new Map(roster.map((m) => [m.id, m]));
  return channel.memberIds
    .map((id) => byId.get(id))
    .filter((m): m is TeamMember => m !== undefined);
}

/** How a channel is titled in the header and the rail. */
export function channelTitle(channel: Channel): string {
  return channel.kind === "dm" ? channel.name : `#${channel.name}`;
}

/**
 * The line that goes *beside* the title — the muted slot after the header's
 * divider, the rail row's tooltip, the conversation intro's clause — or `null`
 * when there is nothing to say that the title has not already said.
 *
 * `null` rather than the empty string, and a rule rather than a DM special
 * case. A subtitle exists to add a second fact; one that repeats the first is
 * not a hierarchy, it is the same word twice with two type styles, and the
 * honest render of "nothing more to say" is nothing. Issue #1180 is what that
 * looks like when it ships: every agent in a company that declares roles and no
 * names read `Backend Engineer │ Backend Engineer` across the top of its DM.
 *
 * The comparison is case- and whitespace-insensitive because the duplicate is a
 * duplicate to a reader either way — a manifest whose description restates the
 * role in sentence case is the same non-fact as one that restates it verbatim.
 *
 * Kind-agnostic on purpose. A DM is where this bites today, but a desk whose
 * blurb is just its own name is the identical duplicate under `#`, and a rule
 * that only fires for DMs would let that one through. Channels are otherwise
 * untouched: a blurb that says something the slug does not — which is every
 * desk that bothered to write one — comes back exactly as before.
 */
export function channelSubtitle(channel: Channel): string | null {
  const purpose = channel.purpose.trim();
  if (!purpose) return null;
  // Collapse runs of internal whitespace too, not just the outer trim — a
  // manifest description copy-pasted from the role with a doubled space or a
  // stray newline in the middle is still the same duplicate to a reader, and
  // the doc comment above promises "whitespace-insensitive" without
  // qualification.
  const normalize = (s: string) => s.trim().replace(/\s+/g, " ").toLowerCase();
  if (normalize(purpose) === normalize(channel.name)) return null;
  return purpose;
}

/**
 * The line under the identity block at the top of an empty transcript.
 *
 * Pure, exported and here rather than inline in `MessageTimeline` because it is
 * four branches of prose over two nullable inputs, and prose defects are
 * invisible to a type checker. The one this arrived with: the DM branch used to
 * append a full stop by hand, which was right while the subtitle was a role
 * (`Backend Engineer`) and produced "…and services.." the moment issue #1180
 * made it a description that brings its own punctuation. `sentence` is the rule
 * for that, so every branch goes through it.
 *
 * `loading` renders the subtitle alone: both finished sentences are positive
 * claims that the channel has no history, and neither may be made before the
 * host has answered (issue #934).
 */
export function channelIntroSentence(
  channel: Channel,
  loading: boolean,
): string {
  const subtitle = channelSubtitle(channel);
  if (loading) return subtitle ? sentence(subtitle) : "";
  if (channel.kind === "dm") {
    // No subtitle drops the clause, not the sentence: where you are is still
    // worth saying, the tautology after it is not.
    return subtitle
      ? `This is the start of your direct message with ${channel.name} — ${sentence(lower(subtitle))}`
      : `This is the start of your direct message with ${channel.name}.`;
  }
  return `This is the very beginning of ${channelTitle(channel)}.${subtitle ? ` ${sentence(subtitle)}` : ""}`;
}

/** Lowercases the first character only, for a clause continuing a sentence. */
function lower(s: string): string {
  return s.charAt(0).toLowerCase() + s.slice(1);
}

/** Terminates `s` with a full stop unless it already ends in punctuation. */
function sentence(s: string): string {
  const t = s.trim();
  return /[.!?]$/.test(t) ? t : `${t}.`;
}

/**
 * The face a DM wears — the `TeammateAvatar` seed for the teammate on the
 * other end — or `null` for anything that has no face: a channel, and a DM
 * with no roster entry behind it (both of those wear a glyph instead).
 *
 * One function rather than the same props written out at each call site,
 * because the rail row and the header sit on screen together, and a call site
 * that seeded its mascot differently from another would draw a *different*
 * face for the same person a few pixels away — worse than the generic glyph
 * the header used to show (issue #1170). Deriving both from here is what
 * makes that drift impossible rather than merely unlikely.
 *
 * `avatar` is `channel.member.avatar` — the id-seeded mascot key `fromDto`
 * already computed onto the roster entry (issue #1185) — rather than
 * `channel.name`: a teammate's face must survive a rename, and `TeamMember`
 * already carries the seed that does that. `tone` needs no such rerouting;
 * `buildChannels` already sets it from `member.tone`, which was id-seeded from
 * the start.
 */
export function dmFace(
  channel: Channel,
): { name: string; tone?: string; avatar?: string } | null {
  if (channel.kind !== "dm" || !channel.member) return null;
  return {
    name: channel.name,
    tone: channel.tone,
    avatar: channel.member.avatar,
  };
}

/**
 * Whether this chat target's composer offers "Do it once" / "Build me the
 * workflow" (issues #580, #845).
 *
 * True for every real chat target — a channel line and a DM can each open a
 * board card, and the card is what routes a `workflow` request to the builder
 * pass. It is deliberately a total function over [`ChannelKind`] rather than an
 * inline `kind === "channel"`, so adding a kind is a decision someone makes here
 * instead of a control that silently fails to appear.
 *
 * Not every composer asks: the thread and copilot composers pass nothing and
 * keep their plain `(text)` `onSend`. A thread reply continues a message that
 * already made this choice, and a copilot line is about one graph rather than a
 * request to the company.
 *
 * #580 shipped the control on channels only. Nothing downstream was ever scoped
 * to channels — the chat route reads `deliverable` off the payload whatever
 * thread it came from — so the asymmetry lived entirely in the caller, and a DM
 * asking for a workflow had no way to say so. It went as a `once` card, was
 * dispatched to a desk agent holding no authoring tool, and came back a refusal.
 */
export function offersDeliverableChoice(kind: ChannelKind): boolean {
  switch (kind) {
    case "channel":
    case "dm":
      return true;
  }
}

/* ---- senders ---- */

/**
 * A crossing currently running, as the timeline needs to describe it.
 *
 * The two kinds are different events, not two spellings of one. A crossing to
 * a PERSON is a two-way exchange — `pair_messages` lets the pair alternate, so
 * both seats spend turns and neither is merely answering. A crossing to a DESK
 * is that desk answering, as a whole room since #2332, and the host's `target`
 * for it is only the library's first-eligible seat: naming that seat would
 * credit one member with the room's work, so the desk is named instead.
 */
export type ReferralWorking = { row?: number } & (
  | { direct: true; asker: string; target: string }
  | { direct: false; desk: string }
);

/**
 * The transcript row ids whose crossing is still running.
 *
 * `h<seq>` is how `fromHistory` names a durable row, and the frame's
 * `sequence` is the row a crossing folds onto — so the two meet here.
 */
export function runningCrossingRows(
  working: Record<string, ReferralWorking>,
): string[] {
  return Object.values(working)
    .map((crossing) => crossing.row)
    .filter((row): row is number => row !== undefined)
    .map((row) => `h${row}`);
}

/** How the timeline says a crossing is running, given the names it holds. */
export function referralWorkingLabel(
  crossing: ReferralWorking,
  nameOf: (id: string) => string,
): string {
  return crossing.direct
    ? `${nameOf(crossing.asker)} and ${nameOf(crossing.target)} are talking`
    : `the ${nameOf(crossing.desk)} desk is answering`;
}
