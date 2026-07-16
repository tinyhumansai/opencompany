// Conversation threads: WhatsApp-style "chats" with the company. Every thread
// talks to the same company chat endpoint; a thread just scopes a transcript
// and gives the company side a consistent identity (a "desk" you're talking to).

import type { ChatMessage } from "./chat";

export interface ThreadContact {
  name: string;
  kind: "company" | "agent";
  /** Tailwind avatar tone key for agent desks; company uses the brand mark. */
  tone?: string;
}

export interface Thread {
  id: string;
  contact: ThreadContact;
  /** Short blurb shown under the name when the thread has no messages yet. */
  blurb: string;
  messages: ChatMessage[];
}

/** The default chat list: the company's main line plus a few focused desks. */
export function defaultThreads(): Thread[] {
  return [
    {
      id: "main",
      contact: { name: "Your company", kind: "company" },
      blurb: "The main line — ask for anything",
      messages: [],
    },
    {
      id: "strategy",
      contact: { name: "Strategy desk", kind: "agent", tone: "sky" },
      blurb: "Plans, priorities, and direction",
      messages: [],
    },
    {
      id: "creative",
      contact: { name: "Creative studio", kind: "agent", tone: "violet" },
      blurb: "Copy, design, and campaigns",
      messages: [],
    },
    {
      id: "frontdesk",
      contact: { name: "Front desk", kind: "agent", tone: "amber" },
      blurb: "Scheduling, inbox, and errands",
      messages: [],
    },
  ];
}
