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
   * The scrubbed processing steps behind a company reply (tool calls, thinking,
   * surfaced failures), rendered as a timeline above the bubble. Absent/empty
   * on your own messages and on tool-less replies.
   */
  steps?: TurnStep[];
}

let seq = 0;
const nextId = () => `m${seq++}`;

/** Build a stamped message. `at` is injected so callers stay pure/testable. */
export function makeMessage(
  from: ChatMessage["from"],
  text: string,
  opts: { channel?: string; at?: number; steps?: TurnStep[] } = {},
): ChatMessage {
  return {
    id: nextId(),
    from,
    text,
    at: opts.at ?? Date.now(),
    channel: opts.channel,
    steps: opts.steps,
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
    };
  });
}
