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
}

let seq = 0;
const nextId = () => `m${seq++}`;

/** Build a stamped message. `at` is injected so callers stay pure/testable. */
export function makeMessage(
  from: ChatMessage["from"],
  text: string,
  opts: { channel?: string; at?: number; parentId?: string } = {},
): ChatMessage {
  return {
    id: nextId(),
    from,
    text,
    at: opts.at ?? Date.now(),
    channel: opts.channel,
    parentId: opts.parentId,
  };
}
