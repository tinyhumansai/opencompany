// Turning what this console already holds into results.
//
// Pure, like its neighbours: collections and a parsed query in, `SearchResult`s
// out. No fetching happens here — {@link useSearch} owns that — so every rule
// about *what counts as a hit* and *what a row says* is testable without a
// host, a client, or a rendered modal.

import type { Desk } from "@/lib/desks";
import type { TeamMember } from "@/lib/team";
import type { ChatHistoryMessageDto } from "@/api/types";
import type { SearchHit } from "@/api/workspace";
import { hostMessageId } from "@/lib/chat";
import { dmChannelId } from "@/views/room/channels";
import { bestScore, excerptAround, matchRanges, score } from "./rank";
import type { SearchQuery } from "./query";
import type { SearchResult } from "./types";

/** How many of any one kind are worth showing before the list stops helping. */
export const PER_GROUP_LIMIT = 6;

/**
 * Channels, matched on the name you would type and the purpose you might
 * remember instead.
 *
 * The action says which of the two things Enter will do, because in a fused
 * palette both are reachable from the same row: `#engineering` goes there, and
 * `#engineering box` searches inside it. Inferring that from the shape of what you
 * typed is not something an operator should have to do.
 */
export function channelResults(desks: readonly Desk[], query: SearchQuery): SearchResult[] {
  if (query.scope && query.scope.kind !== "channel") return [];
  const term = query.scope?.kind === "channel" ? query.scope.name : query.term;

  return desks
    .map((desk): SearchResult | null => {
      const value = bestScore([desk.channel, desk.name, desk.blurb], term);
      if (value === 0) return null;
      return {
        kind: "channel" as const,
        id: `channel:${desk.id}`,
        title: `#${desk.channel}`,
        // +1 for the `#` this title adds in front of the matched name.
        titleMatches: matchRanges(desk.channel, term).map(
          ([from, to]) => [from + 1, to + 1] as const,
        ),
        subtitle: desk.blurb || undefined,
        scopeName: desk.channel,
        href: `#/chat/${encodeURIComponent(desk.id)}`,
        action: `Go to #${desk.channel}`,
        score: value,
      };
    })
    .filter((hit): hit is SearchResult => hit !== null)
    .sort(byScore)
    .slice(0, PER_GROUP_LIMIT);
}

/**
 * Agents, matched on name and on the role you would describe them by when the
 * name is the half you have forgotten.
 */
export function agentResults(members: readonly TeamMember[], query: SearchQuery): SearchResult[] {
  if (query.scope && query.scope.kind !== "person") return [];
  const term = query.scope?.kind === "person" ? query.scope.name : query.term;

  return members
    .map((member): SearchResult | null => {
      const value = bestScore([member.name, member.id, member.role], term);
      if (value === 0) return null;
      return {
        kind: "agent" as const,
        id: `agent:${member.id}`,
        title: member.name,
        titleMatches: matchRanges(member.name, term),
        subtitle: member.role || undefined,
        // The id, never the display name — see `SearchResult.scopeName`.
        scopeName: member.id,
        // The console-local channel id, which is what the hash router routes.
        // NOT `dmThreadId` — that is the host thread this DM is addressed on,
        // and the two differ for a teammate whose id spells General.
        href: `#/chat/${encodeURIComponent(dmChannelId(member))}`,
        action: `Message ${member.name}`,
        score: value,
      };
    })
    .filter((hit): hit is SearchResult => hit !== null)
    .sort(byScore)
    .slice(0, PER_GROUP_LIMIT);
}

/** Where a set of messages came from, so a row can say it. */
export interface MessageContext {
  /** The channel id these messages belong to, for the row's link. */
  channelId: string;
  /** What to call it on the row — `#autumn-launch`, or an agent's name. */
  label: string;
  /** Resolves an author id to something worth reading. */
  nameFor: (author: string) => string;
}

/**
 * Messages inside one conversation.
 *
 * Scoped by construction: the caller has already fetched exactly one
 * conversation's history, because that is the search this console can answer
 * honestly. There is no host route for message search across a company, and
 * searching only the channels that happen to be loaded would answer "no
 * matches" for a message that plainly exists.
 */
export function messageResults(
  messages: readonly ChatHistoryMessageDto[],
  query: SearchQuery,
  context: MessageContext,
): SearchResult[] {
  const term = query.term;
  if (!term) return [];

  // When each hit was sent, kept beside the results rather than on them: it is
  // a sort key, not something a row says, and `SearchResult` is what the modal
  // renders.
  const recency = new Map(messages.map((message) => [`message:${message.id}`, message.atMillis]));

  return messages
    .map((message): SearchResult | null => {
      const value = score(message.text, term);
      if (value === 0) return null;
      const { excerpt, ranges } = excerptAround(message.text, term);
      const who = context.nameFor(message.author);
      return {
        kind: "message" as const,
        id: `message:${message.id}`,
        title: who,
        titleMatches: [],
        subtitle: `${context.label} · ${formatWhen(message.atMillis)}`,
        excerpt,
        excerptMatches: ranges,
        // `hostMessageId`, not the bare host id: `fromHistory` renders every
        // journaled line under an `h`-prefixed console id, and that is what
        // `data-message-id` carries. Linking the bare one lands on the channel
        // and then finds nothing to scroll to.
        //
        // A reply is folded into its thread and is not in the main timeline at
        // all, so its link names the parent too — `RoomView` opens the panel
        // and then finds the line inside it. Without that the link lands in the
        // conversation and quietly gives up.
        href: messageHref(context.channelId, message),
        action: `Open in ${context.label}`,
        score: value,
      };
    })
    .filter((hit): hit is SearchResult => hit !== null)
    // Most recent first among equally good matches: in a conversation later is
    // usually the one being looked for, and the history route answers
    // oldest-first — so without the second key a search for a word as common as
    // "yes" fills the limit with the oldest six and hides today's.
    .sort((a, b) => b.score - a.score || (recency.get(b.id) ?? 0) - (recency.get(a.id) ?? 0))
    .slice(0, PER_GROUP_LIMIT);
}

/**
 * Where a message result opens.
 *
 * A reply lives in its thread rather than in the timeline, so its address
 * names the thread as well — `RoomView` consumes `?thread=` first and the
 * line is then on screen to be found.
 */
function messageHref(channelId: string, message: ChatHistoryMessageDto): string {
  const anchor = `m=${encodeURIComponent(hostMessageId(message.id))}`;
  const parent = message.parentId
    ? `thread=${encodeURIComponent(hostMessageId(message.parentId))}&`
    : "";
  return `#/chat/${encodeURIComponent(channelId)}?${parent}${anchor}`;
}

/**
 * Workspace files, from the host's own search.
 *
 * The one source that arrives already matched: `GET …/workspace/search` did the
 * work and says whether it was the name or the body, so this only shapes it.
 */
export function fileResults(hits: readonly SearchHit[], query: SearchQuery): SearchResult[] {
  if (query.scope && query.scope.kind !== "file") return [];
  const term = query.scope?.kind === "file" ? query.scope.name : query.term;

  // Ranked THEN limited. Slicing first drops a name match sitting behind six
  // body matches — the one hit most likely to be the answer.
  return hits
    .map((hit): SearchResult => {
    const excerpt = hit.excerpt ? excerptAround(hit.excerpt, term) : null;
    return {
      kind: "file" as const,
      id: `file:${hit.id}`,
      title: hit.name,
      titleMatches: matchRanges(hit.name, term),
      subtitle: hit.path,
      excerpt: excerpt?.excerpt,
      excerptMatches: excerpt?.ranges,
      href: `#/workspace/${encodeURIComponent(hit.id)}`,
      action: `Open ${hit.name}`,
      score: hit.matched === "name" ? 800 : 500,
    };
    })
    .sort(byScore)
    .slice(0, PER_GROUP_LIMIT);
}

/** Highest first; ties keep the order the source gave them. */
function byScore(a: SearchResult, b: SearchResult): number {
  return b.score - a.score;
}

/**
 * A date a person can place at a glance.
 *
 * Time for today, weekday within the last week, and a date beyond that — the
 * three answers to "when was this" that are actually useful at different
 * distances.
 */
export function formatWhen(atMillis: number, now = Date.now()): string {
  const when = new Date(atMillis);
  const today = new Date(now);
  const elapsed = now - atMillis;
  const DAY = 24 * 60 * 60 * 1000;
  // The local calendar day, not the last 24 hours: 23:00 yesterday read at
  // 10:00 today is not "today", and rendering it as a bare time says it is.
  const sameDay =
    when.getFullYear() === today.getFullYear() &&
    when.getMonth() === today.getMonth() &&
    when.getDate() === today.getDate();
  if (sameDay) {
    return when.toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });
  }
  if (elapsed < 7 * DAY) {
    return when.toLocaleDateString(undefined, { weekday: "long" });
  }
  return when.toLocaleDateString(undefined, { day: "numeric", month: "short" });
}
