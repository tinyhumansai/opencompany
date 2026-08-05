// The chat workspace's data model: channels, direct messages, and the grouping
// rules the timeline reads. Everything here is pure — the view owns the state.

import type { ApprovalSummary, DeskDto, Verdict } from "@/api/types";
import type { ChatMessage, Reaction } from "@/lib/chat";
import { defaultDesks, type Desk } from "@/lib/desks";
import { initials as nameInitials, type TeamMember } from "@/lib/team";

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
  const slug = d.name.trim().toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "");
  return {
    id: d.id,
    channel: slug || d.id,
    name: d.name,
    blurb: d.description ?? "",
    members: d.members,
    overlayMembers: d.overlayMembers,
  };
}

/**
 * Every channel's transcript, keyed by channel id. Owned by `AppShell`, not
 * `ChatView`, so a transcript survives `ChatView` unmounting when the operator
 * steps into Tasks, Settings, or any other view and comes back.
 */
export type Transcripts = Record<string, ChatMessage[]>;

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
 * strategy/creative/front-desk trio. Every roster teammate additionally gets
 * a DM, so a one-to-one line exists for anyone the company employs.
 *
 * Both kinds post to the same company endpoint. A channel scopes a transcript
 * and gives the company side a stable identity; it is not a separate backend.
 */
export function buildChannels(members: TeamMember[], desks: Desk[] = defaultDesks()): ChannelSection[] {
  const channels: Channel[] = desks.map((d) => ({
    id: d.id,
    name: d.channel,
    voice: d.name,
    kind: "channel" as const,
    purpose: d.blurb,
    tone: d.tone,
    memberIds: d.members,
  }));

  const dms: Channel[] = members.map((m) => ({
    id: dmChannelId(m),
    name: m.name,
    kind: "dm" as const,
    purpose: m.role,
    tone: m.tone,
    member: m,
  }));

  return [
    { id: "channels", label: "Channels", channels },
    { id: "dms", label: "Direct messages", channels: dms },
  ];
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
 * The name-derived DM id this console minted before issue #364.
 *
 * Kept for one release, and for one purpose: a `#/chat/dm:ada-1f3k` link that
 * somebody bookmarked or pasted into a ticket still has to land on Ada's DM.
 * Only {@link resolveDmChannelId} calls it — nothing addresses a channel by it,
 * so no new state is ever written under a legacy id.
 */
export function legacyDmChannelId(member: TeamMember): string {
  const name = member.name.trim();
  const slug = name.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "");
  return `dm:${slug ? `${slug}-` : ""}${nameHash(name)}`;
}

/**
 * The current DM channel id a URL segment names, or `null` when it names no
 * DM this company has.
 *
 * Resolves the current id first, then the pre-#364 name-derived form, so an old
 * link keeps working without the old id ever becoming addressable again.
 */
export function resolveDmChannelId(id: string, members: TeamMember[]): string | null {
  if (!id.startsWith("dm:")) return null;
  const match = members.find(
    (m) => dmChannelId(m) === id || legacyDmChannelId(m) === id,
  );
  return match ? dmChannelId(match) : null;
}

function nameHash(name: string): string {
  let hash = 0;
  for (let i = 0; i < name.length; i++) hash = (hash * 31 + name.charCodeAt(i)) | 0;
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
  const member = members.find((m) => m.id === threadId);
  return member ? dmChannelId(member) : null;
}

export function findChannel(sections: ChannelSection[], id: string | null): Channel | null {
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
export function channelMembers(channel: Channel, roster: TeamMember[]): TeamMember[] | null {
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

/* ---- senders ---- */

export type SenderKind = "you" | "company" | "agent" | "system";

export interface Sender {
  /** Stable identity, so consecutive lines from one voice group together. */
  key: string;
  name: string;
  kind: SenderKind;
  tone?: string;
}

/** Channel names the host uses for its own voice rather than a named agent. */
const COMPANY_VOICE = new Set(["operator", "console", "chat", "owner", ""]);

/**
 * Who said a line, within a channel.
 *
 * The company side wears the channel's identity unless the reply names a
 * distinct originating channel — then it reads as that agent, which is how a
 * single endpoint produces a multi-voice transcript.
 */
export function senderOf(m: ChatMessage, channel: Channel): Sender {
  if (m.from === "you") return { key: "you", name: "You", kind: "you" };
  if (m.from === "system") return { key: "system", name: "System", kind: "system" };

  const named = m.channel?.trim().toLowerCase() ?? "";
  if (named && !COMPANY_VOICE.has(named)) {
    return { key: `agent:${named}`, name: titleize(m.channel ?? ""), kind: "agent", tone: named };
  }

  // A desk speaks as itself and wears its own tone; only the main line — the
  // one channel with no tone of its own — speaks as the company.
  return {
    key: `channel:${channel.id}`,
    name: channel.voice ?? channel.name,
    kind: channel.kind === "dm" || channel.tone ? "agent" : "company",
    tone: channel.tone,
  };
}

function titleize(s: string): string {
  return s.replace(/[._-]+/g, " ").replace(/\w\S*/g, (w) => w.charAt(0).toUpperCase() + w.slice(1));
}

export const initials = nameInitials;

/* ---- timeline grouping ---- */

/** Consecutive lines from one sender inside this window collapse into a run. */
const GROUP_WINDOW_MS = 5 * 60 * 1000;

export interface TimelineEntry {
  message: ChatMessage;
  sender: Sender;
  /** True when this row continues the run above it — no avatar, no name. */
  continuation: boolean;
  /** Set on the first row of a new calendar day; the divider label. */
  dayLabel?: string;
  /** Replies hanging off this row, oldest first. */
  replies: ChatMessage[];
}

/**
 * Flatten a channel's messages into rows the timeline can render directly.
 *
 * Replies are folded into their parent rather than laid out inline: a parent
 * carries its own replies and renders a summary row, matching how a threaded
 * chat keeps the main channel readable.
 */
export function buildTimeline(messages: ChatMessage[], channel: Channel): TimelineEntry[] {
  const replies = new Map<string, ChatMessage[]>();
  for (const m of messages) {
    if (!m.parentId) continue;
    const bucket = replies.get(m.parentId);
    if (bucket) bucket.push(m);
    else replies.set(m.parentId, [m]);
  }

  const entries: TimelineEntry[] = [];
  let prev: TimelineEntry | undefined;

  for (const m of messages) {
    if (m.parentId) continue;
    const sender = senderOf(m, channel);
    const newDay = !prev || !sameDay(prev.message.at, m.at);
    const continuation =
      !newDay &&
      !!prev &&
      prev.sender.key === sender.key &&
      sender.kind !== "system" &&
      m.at - prev.message.at < GROUP_WINDOW_MS &&
      // A row with replies ends its run — the summary row below it would
      // otherwise sit between two lines that read as one utterance.
      prev.replies.length === 0;

    const entry: TimelineEntry = {
      message: m,
      sender,
      continuation,
      dayLabel: newDay ? formatDay(m.at) : undefined,
      replies: replies.get(m.id) ?? [],
    };
    entries.push(entry);
    prev = entry;
  }

  return entries;
}

/**
 * An approval this console has watched being decided (#379).
 *
 * The **summary is kept, not just the verdict**, and that is the whole point:
 * the host drops a resolved approval from `GET …/approvals` immediately, so a
 * console holding only the verdict has nothing left to draw and the card blinks
 * out of the thread the moment it is decided — which reads as the request
 * having been lost, not answered. Keeping the last-seen summary is what lets it
 * settle in place instead.
 */
export interface DecidedApproval {
  verdict: Verdict;
  approval: ApprovalSummary;
}

/**
 * One row of a channel, which is no longer only a message (#379).
 *
 * A parked approval is a **distinct kind**, not a synthetic `ChatMessage`. It
 * has to be: a card is decidable, carries live server state, and settles into a
 * terminal state — none of which a message row can represent, and faking one
 * would mean inventing an id, a sender and a body for something that is not an
 * utterance. Keeping it separate is also what lets the card *derive* from
 * `feed.approvals` rather than being appended once and then going stale.
 *
 * Both kinds carry `at` so the two streams interleave on real time, which is
 * the only ordering that reads correctly: the request appears where the
 * conversation was when it was raised.
 */
export type TimelineItem =
  | { kind: "message"; key: string; at: number; entry: TimelineEntry }
  | {
      kind: "approval";
      key: string;
      at: number;
      approval: ApprovalSummary;
      /**
       * The verdict this console has witnessed for the card, if any.
       *
       * A decided approval leaves `feed.approvals` on the next refresh, so
       * without this the card would simply vanish mid-glance — an abrupt
       * unmount that reads as the request having been lost. Holding the
       * witnessed verdict lets it settle into a terminal state instead.
       */
      decided: Verdict | null;
    };

/**
 * Interleave a channel's messages and the approvals raised in it, oldest first.
 *
 * `approvals` is expected to be pre-filtered to this channel by the caller —
 * the thread→channel mapping lives in the shell, which owns the desk list and
 * the roster, and this module stays pure.
 *
 * A `decided` card is kept even once the feed has dropped it, so the operator
 * sees their own decision land rather than the card disappearing.
 */
export function buildTimelineItems(
  entries: TimelineEntry[],
  approvals: ApprovalSummary[],
  decided: Record<string, DecidedApproval> = {},
): TimelineItem[] {
  const items: TimelineItem[] = entries.map((entry) => ({
    kind: "message" as const,
    key: entry.message.id,
    at: entry.message.at,
    entry,
  }));
  for (const approval of approvals) {
    items.push({
      kind: "approval",
      key: `approval:${approval.id}`,
      at: approval.at_millis,
      approval,
      decided: decided[approval.id]?.verdict ?? null,
    });
  }
  // Stable within a millisecond: a card raised by the very turn whose reply
  // shares its timestamp should sit after that reply, not shuffle between
  // renders. `sort` is stable in every engine this ships to, so equal `at`
  // keeps insertion order — messages first, then cards.
  return items.sort((a, b) => a.at - b.at);
}

/* ---- formatting ---- */

export function formatTime(at: number): string {
  return new Date(at).toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });
}

export function sameDay(a: number, b: number): boolean {
  return new Date(a).toDateString() === new Date(b).toDateString();
}

export function formatDay(at: number): string {
  const d = new Date(at);
  const today = new Date();
  const yesterday = new Date(today);
  yesterday.setDate(today.getDate() - 1);
  if (d.toDateString() === today.toDateString()) return "Today";
  if (d.toDateString() === yesterday.toDateString()) return "Yesterday";
  return d.toLocaleDateString(undefined, { weekday: "long", month: "long", day: "numeric" });
}

/* ---- reactions ---- */

/** The palette the hover bar offers, in the order it offers them. */
export const QUICK_REACTIONS = ["👍", "🎉", "👀", "✅", "❤️"] as const;

/**
 * Toggle the reader's own reaction, leaving everyone else's alone (issue #364).
 *
 * Reactions are per-person rows now, so a toggle adds or removes exactly one —
 * the reader's. It used to replace a count, which meant tapping an emoji
 * somebody else had already used silently wiped their reaction.
 *
 * `label` is how the reader will be shown in the chip's tooltip until the host
 * says otherwise. Returns `undefined` for an empty result so a message with no
 * reactions carries no key at all.
 */
export function toggleReaction(
  reactions: Reaction[] | undefined,
  emoji: string,
  label: string,
): Reaction[] | undefined {
  const rows = reactions ?? [];
  const mine = rows.some((r) => r.emoji === emoji && r.mine);
  const next = mine
    ? rows.filter((r) => !(r.emoji === emoji && r.mine))
    : [...rows, { emoji, by: label, mine: true }];
  return next.length ? next : undefined;
}

/** Whether the reader has already reacted to a message with this emoji. */
export function hasReacted(reactions: Reaction[] | undefined, emoji: string): boolean {
  return !!reactions?.some((r) => r.emoji === emoji && r.mine);
}

/** One emoji's chip: its rows collapsed into a count and a who-list. */
export interface ReactionChip {
  emoji: string;
  count: number;
  /** Whether one of the rows is the reader's. */
  mine: boolean;
  /** Everyone who reacted with it, in the order the host listed them. */
  by: string[];
}

/**
 * Group per-person reaction rows into the chips the row renders.
 *
 * Chips keep first-reacted order rather than sorting by count, so a message's
 * reactions do not reshuffle under the reader as others react.
 */
export function reactionChips(reactions: Reaction[] | undefined): ReactionChip[] {
  const chips: ReactionChip[] = [];
  const byEmoji = new Map<string, ReactionChip>();
  for (const row of reactions ?? []) {
    let chip = byEmoji.get(row.emoji);
    if (!chip) {
      chip = { emoji: row.emoji, count: 0, mine: false, by: [] };
      byEmoji.set(row.emoji, chip);
      chips.push(chip);
    }
    chip.count += 1;
    chip.mine ||= row.mine;
    chip.by.push(row.by);
  }
  return chips;
}
