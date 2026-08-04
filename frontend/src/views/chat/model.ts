// The chat workspace's data model: channels, direct messages, and the grouping
// rules the timeline reads. Everything here is pure — the view owns the state.

import type { ChatMessage } from "@/lib/chat";
import { defaultDesks } from "@/lib/desks";
import { initials as nameInitials, type TeamMember } from "@/lib/team";

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
}

export interface ChannelSection {
  id: string;
  label: string;
  channels: Channel[];
}

/**
 * The channel list.
 *
 * The company's desks (`lib/desks.ts`) become the `#channels` — they are the
 * standing lines you can address, and each already carries a name, a blurb, and
 * a tone. Every roster teammate additionally gets a DM, so a one-to-one line
 * exists for anyone the company employs.
 *
 * Both kinds post to the same company endpoint. A channel scopes a transcript
 * and gives the company side a stable identity; it is not a separate backend.
 */
export function buildChannels(members: TeamMember[]): ChannelSection[] {
  const channels: Channel[] = defaultDesks().map((d) => ({
    id: d.id,
    name: d.channel,
    voice: d.name,
    kind: "channel" as const,
    purpose: d.blurb,
    tone: d.tone,
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
 * Keyed on the teammate's name rather than their roster id: the starter roster
 * mints ids from a module counter (`lib/team.ts`), so they differ between two
 * calls in the same session and a `#/chat/dm:member-3` link would point at a
 * different person — or nobody — on the next mount.
 */
export function dmChannelId(member: TeamMember): string {
  return `dm:${member.name.trim().toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "")}`;
}

export function findChannel(sections: ChannelSection[], id: string | null): Channel | null {
  if (!id) return null;
  for (const s of sections) {
    const hit = s.channels.find((c) => c.id === id);
    if (hit) return hit;
  }
  return null;
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

/** Toggle one emoji on a message, returning the next reaction map. */
export function toggleReaction(
  reactions: Record<string, number> | undefined,
  emoji: string,
): Record<string, number> | undefined {
  const next = { ...(reactions ?? {}) };
  if (next[emoji]) {
    delete next[emoji];
  } else {
    next[emoji] = 1;
  }
  return Object.keys(next).length ? next : undefined;
}
