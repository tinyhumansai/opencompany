import type { NotificationDto } from "@/api/types";
import { MAIN_THREAD_ID, hostMessageId } from "@/lib/chat";

/**
 * Mirrors the host's `is_general_chat` (`src/server/chat_history.rs`, issue
 * #65): the console addresses its default thread as `"main"`, an unaddressed
 * chat route stores the default desk `"General"`, and older events carry `""`.
 * All four spellings are one desk, and a notification context that names it has
 * to badge the *rendered* main channel — the rail is built from real desk ids,
 * none of which is `"General"`.
 */
function isGeneralChat(context: string | null | undefined): boolean {
  if (context === undefined || context === null) return false;
  const folded = context.toLowerCase();
  return context === "" || folded === MAIN_THREAD_ID || folded === "general";
}

/**
 * The rendered channel a mention's `context` badges, or `undefined` when it has
 * nowhere to land.
 *
 * Shared by the badge placement in [`mentionCountsByChannel`] and the app
 * shell's "re-read the thread a mention's message is missing from" trigger, so
 * both resolve the same channel for the same row:
 *
 * - An exact rendered channel-id match wins outright: a real desk whose id
 *   happens to be `general` (or `main`, or — impossibly — `""`) is *that*
 *   channel, so a mention stored under the canonical id has to badge that
 *   desk, not the default thread the legacy spellings alias to (issue #65).
 * - Only a context that names no rendered channel falls back to the alias:
 *   a legacy general-chat spelling badges the rendered main channel, and one
 *   with no rendered main channel is dropped (the rail has no row to badge).
 */
export function renderedChannelIdForContext(
  context: string | null | undefined,
  mainChannelId: string | undefined,
  renderedChannelIds: ReadonlySet<string>,
): string | undefined {
  if (context === undefined || context === null) return undefined;
  if (renderedChannelIds.has(context)) return context;
  if (isGeneralChat(context)) return mainChannelId;
  return context;
}

/**
 * Whether viewing `threadId` in Conversation counts as actually viewing the
 * channel it maps to — and therefore may advance that channel's read state.
 *
 * Almost always true: a desk thread's transcript *is* its channel's transcript,
 * and a DM thread's transcript is the DM channel's (different id, same store).
 * The one exception is the `main` thread on a company with real desks. The rail
 * is built from real desk ids and has no `General` row, so `channelMap` aliases
 * `main` onto the first desk's channel *for badging* — but the main thread's
 * transcript is the legacy General conversation, a different store from the
 * first desk's own. Marking that desk read because the operator read `main`
 * would permanently un-badge messages they never saw (Codex P1). The mention
 * clear stays: the thread's loaded ids prove the summoning message is on screen
 * ([`mentionsToClear`] is fed those ids, so the general-chat arm clears only
 * what actually rendered).
 */
export function threadViewAdvancesChannel(
  threadId: string,
  channelId: string,
): boolean {
  return !(threadId === MAIN_THREAD_ID && channelId !== MAIN_THREAD_ID);
}

/**
 * The mention badge: how many unread mentions of **you** sit in each channel.
 *
 * # Why this is not the unread count
 *
 * They answer different questions and come from different places, and the whole
 * value of the mention badge is that it does *not* inherit the unread badge's
 * caveat:
 *
 * | | Unread | Mentions |
 * |---|---|---|
 * | Derived | in this browser, from what this tab has seen | by the host, from who was named |
 * | Survives a reload | only via the read-state floor | yes, it is a stored row |
 * | Means | "you have not looked here" | "somebody asked *you* something" |
 *
 * Merging them would take the durable, per-person fact and give it the
 * best-effort one's meaning. So the rail renders two badges, and only one of
 * them carries the "this tab only" tooltip.
 */
export function mentionCountsByChannel(
  notifications: readonly NotificationDto[],
  /**
   * The id of the rendered channel that stands in for the legacy general
   * thread. `undefined` means there is none yet — the desk list has not
   * loaded, or a company has no desks at all — and a general-chat spelling
   * then has nowhere to badge. It is dropped rather than placed under an id
   * the rail never has, which would render nowhere and could never be cleared
   * (tinysweeper). Direct desk/DM ids are unaffected: they badge from the
   * `renderedChannelIds` arm regardless.
   */
  mainChannelId?: string,
  renderedChannelIds: ReadonlySet<string> = new Set(),
): Record<string, number> {
  const out: Record<string, number> = {};
  // Defensive against a caller handing us something that is not a list. The
  // types say it cannot happen; a host returning an unexpected shape says
  // otherwise, and the consequence of being wrong here is a render-time throw
  // that blanks the console rather than a missing badge.
  if (!Array.isArray(notifications)) return out;
  for (const n of notifications) {
    // Unread only. A mention you have already dealt with is not a summons.
    if (n.readAt !== undefined) continue;
    // `kind` rather than `subjectKind`: a future notification about a message
    // that is not a mention (a reply, a reaction) must not silently start
    // badging as one.
    if (n.kind !== "mention") continue;
    // A row with no channel cannot be placed on the rail. Counted nowhere
    // rather than counted somewhere arbitrary.
    if (n.context === undefined || n.context === null) continue;
    // The placement arm shared with the app shell's re-read trigger, so a
    // mention is badged and recovered from the same channel it is placed on.
    const channelId = renderedChannelIdForContext(
      n.context,
      mainChannelId,
      renderedChannelIds,
    );
    // A general-chat spelling with no rendered main channel (see the param
    // doc) is dropped, not placed under a channel that can never render or
    // clear.
    if (channelId === undefined) continue;
    out[channelId] = (out[channelId] ?? 0) + 1;
  }
  return out;
}

/**
 * The ids to mark read when a channel is opened.
 *
 * Only that channel's, and only the unread ones — opening `#engineering` must
 * not silently clear a mention waiting in `#design`, which is exactly what a
 * bare "mark all" would do and exactly the summons somebody would then miss.
 */
export function mentionsToClear(
  notifications: readonly NotificationDto[],
  channelId: string,
  /**
   * Same semantics as [`mentionCountsByChannel`]'s param: the rendered main
   * channel, or `undefined` when none exists yet. A general-chat mention can
   * then never match — `channelId === mainChannelId` is false for every real
   * channel — which is exactly right, because the count arm never badged it
   * in the first place.
   */
  mainChannelId?: string,
  visibleThreadIds: ReadonlySet<string> = new Set([channelId]),
  renderedChannelIds: ReadonlySet<string> = new Set(),
  /**
   * The loaded transcript's thread replies, keyed by the console id
   * (`h<seq>`) of the reply to the id of the parent it is folded under.
   * A mention inside a thread reply must not clear on channel-open alone —
   * see the gate at the end of the filter.
   */
  replyParents: ReadonlyMap<string, string> = new Map(),
  /** The thread panel currently open, or `null` when none is. */
  openThreadId: string | null = null,
  /**
   * The set of all message ids (`h<seq>`) in the currently loaded transcript
   * for this channel. When provided, a mention whose subject message is absent
   * from this set — outside the history window, or history hydration failed —
   * is not cleared, because the person was never shown the text that mentioned
   * them. Without this, the function cannot distinguish "a top-level message
   * rendered on screen" from "a message that was never loaded" (Codex P1).
   */
  loadedMessageIds: ReadonlySet<string> | undefined = undefined,
): string[] {
  return notifications
    .filter((n) => {
      if (n.readAt !== undefined || n.kind !== "mention" || n.context === undefined) {
        return false;
      }
      let inChannel: boolean;
      if (renderedChannelIds.has(n.context)) {
        // A real desk or DM channel id: only opening that exact channel clears
        // it. `isGeneralChat` must not reroute a real desk named `general` onto
        // the default thread — that is how a mention for the real General desk
        // ends up silently cleared by opening a different channel.
        inChannel = n.context === channelId;
      } else if (n.context === channelId) {
        // Clear only once the main channel's history is actually on screen —
        // a mention is durable, and clearing it before the named message has
        // loaded would lose the summons for good.
        inChannel = channelId !== mainChannelId || visibleThreadIds.has(channelId);
      } else {
        // A general-chat spelling names the main channel: opening it clears
        // those mentions too.
        inChannel = channelId === mainChannelId && isGeneralChat(n.context);
      }
      if (!inChannel) return false;
      // A mention inside a thread reply stays until that reply is actually on
      // screen. The main timeline folds replies into their parent
      // (`buildTimeline`), so a collapsed thread hides the text even while the
      // channel is open — clearing it would lose the summons without the
      // person ever seeing it. The notification names the message by its host
      // sequence (`subjectId`); the loaded transcript's reply map keys by the
      // console's `h<seq>` id, so the two meet through `hostMessageId`.
      const consoleId = hostMessageId(n.subjectId);
      const replyParent = replyParents.get(consoleId);
      if (replyParent !== undefined && replyParent !== openThreadId) return false;
      // When the loaded transcript's message set is known, require the subject
      // to be present — a message outside the history window (or one that
      // hydration failed to fetch) was never displayed, and clearing its
      // mention would lose the summons with nothing left to notice it by.
      if (loadedMessageIds !== undefined && !loadedMessageIds.has(consoleId)) return false;
      return true;
    })
    .map((n) => n.id);
}

/**
 * Which host threads need their history re-read because a newly polled
 * mention's message is absent from the loaded transcript.
 *
 * A mention posted by another operator never reaches an open console through
 * SSE — `OperatorMessage` is deliberately dropped from the stream projection —
 * and the transcripts are otherwise re-read only when a turn *this tab is
 * watching* settles, which a turn another operator's message opened is not. So
 * a mention that lands while the tab is already open has no later delivery
 * path, and opening its channel cannot satisfy the `loadedMessageIds` gate in
 * [`mentionsToClear`]: the operator can neither see the summons nor clear it
 * until reloading. Re-read those threads so the mentioned message lands.
 *
 * `seenSubjects` is the set of subject ids already scheduled this session —
 * the caller adds each returned `subjects` entry to it, so one mention
 * triggers at most one re-read rather than one per poll. The fold that
 * rebuilds the transcript dedupes by message id, so re-reading a thread whose
 * message arrived some other way is a no-op.
 */
export function threadsToReReadForMentions(
  notifications: readonly NotificationDto[],
  /**
   * The message ids currently loaded per rendered channel, keyed by channel id
   * (the same ids `mentionsToClear`'s `loadedMessageIds` holds).
   */
  loadedByChannel: Readonly<Record<string, ReadonlySet<string>>>,
  /** The console's thread-id → channel-id map, as `AppShell` keeps it. */
  chatChannelByThread: Readonly<Record<string, string>>,
  mainChannelId: string | undefined,
  seenSubjects: ReadonlySet<string>,
): { threadIds: string[]; subjects: string[] } {
  const threadIds = new Set<string>();
  const subjects = new Set<string>();
  const renderedChannelIds = new Set(Object.values(chatChannelByThread));
  for (const n of notifications) {
    if (n.readAt !== undefined || n.kind !== "mention") continue;
    const subject = hostMessageId(n.subjectId);
    if (seenSubjects.has(subject)) continue;
    const context = n.context;
    if (context === undefined || context === null) continue;
    const channelId = renderedChannelIdForContext(
      context,
      mainChannelId,
      renderedChannelIds,
    );
    if (channelId === undefined) continue;
    // The message is already on screen for this channel — nothing to recover.
    if (loadedByChannel[channelId]?.has(subject)) continue;
    // The host thread to re-read, decided from the *context* rather than a
    // channel→thread lookup. When the mention sits on the first desk, the map
    // holds both `main -> <first desk>` and `<first desk> -> <first desk>`,
    // with the alias inserted first — a naive lookup would win `main` and
    // re-read the legacy General thread instead of the desk's own, so the
    // mentioned message stays absent and the badge stays stuck (Codex P2).
    const threadId = renderedChannelIds.has(context)
      ? // A real rendered channel. A desk's channel id doubles as its thread
        // id (self-mapping), which must win over the `main` alias. A DM
        // channel has no self-mapping — the member id keys it — so the
        // reverse lookup is unambiguous there.
        chatChannelByThread[context] === context
        ? context
        : Object.entries(chatChannelByThread).find(([, c]) => c === context)?.[0]
      : isGeneralChat(context)
        ? // A legacy general-chat spelling — the console's default thread.
          MAIN_THREAD_ID
        : undefined;
    if (threadId !== undefined) {
      threadIds.add(threadId);
      subjects.add(subject);
    }
  }
  return { threadIds: [...threadIds], subjects: [...subjects] };
}
