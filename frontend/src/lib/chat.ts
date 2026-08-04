import type { ChatHistoryMessageDto, TurnStep } from "@/api/types";

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
   */
  parentId?: string;
  /** Emoji → count. Absent until someone reacts. */
  reactions?: Record<string, number>;
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

/** Build a stamped message. `at` is injected so callers stay pure/testable. */
export function makeMessage(
  from: ChatMessage["from"],
  text: string,
  opts: {
    channel?: string;
    at?: number;
    parentId?: string;
    steps?: TurnStep[];
    taskId?: string;
  } = {},
): ChatMessage {
  return {
    id: nextId(),
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
      id: `h${entry.id}`,
      from,
      text: entry.text,
      at: entry.atMillis,
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
