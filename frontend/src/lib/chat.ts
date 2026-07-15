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
}

let seq = 0;
const nextId = () => `m${seq++}`;

/** Build a stamped message. `at` is injected so callers stay pure/testable. */
export function makeMessage(
  from: ChatMessage["from"],
  text: string,
  opts: { channel?: string; at?: number } = {},
): ChatMessage {
  return { id: nextId(), from, text, at: opts.at ?? Date.now(), channel: opts.channel };
}

/** Channels that are just "you talking to the company" — not a named agent. */
const COMPANY_VOICE_CHANNELS = new Set(["operator", "console", "chat", "owner", ""]);

/**
 * The threaded identity of a message's sender: a stable key plus a display
 * name. Consecutive messages with the same key are grouped in the transcript.
 */
export function senderOf(m: ChatMessage): { key: string; name: string } {
  if (m.from === "you") return { key: "you", name: "You" };
  if (m.from === "system") return { key: "system", name: "System" };
  const channel = m.channel?.trim().toLowerCase() ?? "";
  if (channel && !COMPANY_VOICE_CHANNELS.has(channel)) {
    return { key: `agent:${channel}`, name: agentName(m.channel!) };
  }
  return { key: "company", name: "Your company" };
}

function agentName(channel: string): string {
  return channel
    .replace(/[._-]+/g, " ")
    .replace(/\w\S*/g, (w) => w.charAt(0).toUpperCase() + w.slice(1));
}
