import type { ChatHistoryMessageDto, TurnStep } from "@/api/types";

/** One person's reaction on one line. Mirrors `ChatReactionDto` on the host. */
export interface Reaction {
  emoji: string;
  /** Who reacted, as a display label — never a raw user id. */
  by: string;
  /** Whether the reader is the one who reacted. */
  mine: boolean;
}

/** One line in the conversation with the company. */
export interface ChatMessage {
  id: string;
  from: "you" | "company" | "system";
  text: string;
  /** Wall-clock the line was added, for timestamps and grouping. */
  at: number;
  /**
   * The reply's originating channel (e.g. "operator"). Threads the company
   * side by sender: a distinct channel reads as its own agent in the chat.
   */
  channel?: string;
  /**
   * The message this one replies to. A line with a parent is a thread reply:
   * it stays out of the channel timeline and renders inside the thread panel.
   *
   * Always another line's `id`, so it moves with {@link reconcileIds} when an
   * optimistic id is replaced by the durable one the host assigned.
   */
  parentId?: string;
  /**
   * Who reacted to this line with what — one row per person per emoji, not a
   * count (issue #364).
   *
   * A count could not say who reacted, and could not tell the reader whether
   * one of the reactions was their own, which is what makes a chip a toggle.
   * The renderer groups these into chips; nothing groups a count back into
   * people. Absent until someone reacts.
   */
  reactions?: Reaction[];
  /**
   * The scrubbed processing steps behind a company reply (tool calls, thinking,
   * surfaced failures), rendered as a timeline above the bubble. Absent/empty
   * on your own messages and on tool-less replies.
   */
  steps?: TurnStep[];
  /**
   * The board card this line is about (issue #246): one the turn opened, or one
   * created from this message by "Add to board". Renders as a chip linking to
   * `#/tasks/<id>`.
   */
  taskId?: string;
}

/**
 * How long a card title derived from a chat message may be before it is
 * elided. Long enough to keep a normal one-line ask intact, short enough that a
 * pasted paragraph does not become a board card nobody can scan.
 */
const TITLE_CAP = 80;

/**
 * A board-card title derived from a chat message (issue #246).
 *
 * Takes the first non-blank line — a multi-line ask reads as "headline, then
 * detail", and the detail belongs in the card's note, which is where the full
 * text goes. Returns an empty string for a message with nothing in it, so the
 * caller can refuse rather than opening a blank card.
 *
 * Elides on **code points**, not UTF-16 units, so a title cut mid-emoji cannot
 * produce a lone surrogate; the cap includes the ellipsis rather than
 * overshooting by one.
 */
export function titleFromMessage(text: string): string {
  const line = text
    .split("\n")
    .map((l) => l.trim())
    .find((l) => l.length > 0);
  if (!line) return "";
  const points = Array.from(line);
  return points.length <= TITLE_CAP
    ? line
    : `${points.slice(0, TITLE_CAP - 1).join("")}…`;
}

let seq = 0;
const nextId = () => `m${seq++}`;

/**
 * The identity a live-streamed company reply must be built with (issue #483).
 *
 * A reply arriving over the stream and the same reply coming back from
 * `chat/history` are one message, and the console has to be able to tell. Both
 * sides carry the host's `StoredEvent` sequence — the stream frame as `seq`,
 * the history entry as `id` — so stamping the live line with it makes the two
 * resolve to the same console id, and `hydrateChannel`'s id dedupe recognises
 * the line it already has.
 *
 * Without this the live line was born under an ephemeral `m<counter>` id that
 * hydration could never match, so a reply that arrived while its channel was
 * closed was rendered twice on opening it.
 *
 * This exists as its own function so the identity decision has somewhere a test
 * can reach. Inlined at the two call sites, nothing could assert it was still
 * being made.
 */
export function liveReplyIdentity(event: { seq: number }): { messageId: string } {
  return { messageId: String(event.seq) };
}

/**
 * Build a stamped message. `at` is injected so callers stay pure/testable.
 *
 * `messageId` is the host's own id for the line, when the caller already has
 * one — a reply that came back from the chat POST does. Passing it means the
 * bubble is born durable and can be replied to or reacted on immediately,
 * rather than waiting for a reconciliation it does not need.
 */
export function makeMessage(
  from: ChatMessage["from"],
  text: string,
  opts: {
    channel?: string;
    at?: number;
    parentId?: string;
    steps?: TurnStep[];
    taskId?: string;
    messageId?: string;
  } = {},
): ChatMessage {
  return {
    id: opts.messageId ? hostMessageId(opts.messageId) : nextId(),
    from,
    text,
    at: opts.at ?? Date.now(),
    channel: opts.channel,
    parentId: opts.parentId,
    steps: opts.steps,
    taskId: opts.taskId,
  };
}

/**
 * Maps a desk's persisted transcript (`GET .../chat/history`, issue #65) to
 * the console's chat lines, preserving `mine`/author/text and ordering — the
 * backend already returns messages oldest-first. `id`s are namespaced with an
 * `h` prefix so a rehydrated line can never collide with one built locally by
 * {@link makeMessage} (`m<seq>`).
 */
export function fromHistory(entries: ChatHistoryMessageDto[]): ChatMessage[] {
  return entries.map((entry) => {
    const from: ChatMessage["from"] = entry.mine ? "you" : "company";
    return {
      id: hostMessageId(entry.id),
      from,
      text: entry.text,
      at: entry.atMillis,
      // The host names the parent by its own id, which lives in the same
      // namespace as `entry.id` — so it takes the same prefix, or the reply
      // would point at a line no console id matches (issue #364).
      parentId: entry.parentId ? hostMessageId(entry.parentId) : undefined,
      // Reactions come through whoever the host said reacted; nothing is
      // inferred here, `mine` included.
      reactions: entry.reactions?.length ? entry.reactions : undefined,
      // A sent message never carries a channel; only attribute one when the
      // line came from someone/something else, mirroring `ChatPane.send`.
      channel: from === "company" ? entry.channel : undefined,
      // Rehydrate the persisted tool-call timeline so it survives a thread
      // switch / reload — the render already draws `m.steps` (Conversation.tsx).
      steps: from === "company" ? entry.steps : undefined,
      // Rehydrate the "card opened" chip (issue #246) for the same reason as
      // the timeline above: a chip that lives only on the live POST response
      // vanishes on the first thread switch.
      taskId: from === "company" ? entry.taskId : undefined,
    };
  });
}

/**
 * The console id for a message the host has journaled (issue #364).
 *
 * Two id namespaces meet in a transcript, and keeping them apart is the whole
 * job of this prefix: `m<n>` is a browser-minted counter that means nothing
 * outside this tab, and `h<seq>` is the host's own sequence position, which any
 * reader — a reload, a second operator — resolves to the same message. The `h`
 * is what lets {@link isHostMessageId} tell "saved" from "not saved yet"
 * without asking the server.
 */
export function hostMessageId(seq: string): string {
  return `h${seq}`;
}

/** Whether an id names a message the host has journaled. */
export function isHostMessageId(id: string | undefined): boolean {
  return !!id && id.startsWith("h");
}

/**
 * The host-side id an `h`-prefixed console id names, or `null` for a local one.
 *
 * The inverse of {@link hostMessageId}, used when a durable id has to go back
 * over the wire — a thread reply's parent, a reaction's target.
 */
export function toHostMessageId(id: string | undefined): string | null {
  return isHostMessageId(id) ? id!.slice(1) : null;
}

/**
 * Replace an optimistic message id with the durable one the host assigned, and
 * re-point anything that was already replying to it (issue #364).
 *
 * The subtle half is the re-parenting, and it is why this is a named function
 * with its own tests rather than three lines inside a `setState`. A send is
 * rendered before the POST resolves, so a fast operator can open a thread on
 * that bubble and reply to it while its id is still the local counter. Swapping
 * only the parent's own id would leave those replies pointing at an id that no
 * longer exists — they would silently drop out of the thread and reappear in
 * the channel, which reads as the console losing them.
 *
 * Pure and total: unknown ids pass through, and a list with nothing to change
 * is returned as-is so React sees no new array.
 */
export function reconcileIds(
  messages: ChatMessage[],
  localId: string,
  hostSeq: string,
): ChatMessage[] {
  const nextId = hostMessageId(hostSeq);
  if (nextId === localId) return messages;
  let changed = false;
  const next = messages.map((m) => {
    const isTarget = m.id === localId;
    const isChild = m.parentId === localId;
    if (!isTarget && !isChild) return m;
    changed = true;
    return {
      ...m,
      ...(isTarget ? { id: nextId } : {}),
      ...(isChild ? { parentId: nextId } : {}),
    };
  });
  return changed ? next : messages;
}
