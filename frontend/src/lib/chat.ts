/** One line in the conversation with the company. */
export interface ChatMessage {
  id: string;
  from: "you" | "company" | "system";
  text: string;
}

let seq = 0;
export const nextMessageId = () => `m${seq++}`;
