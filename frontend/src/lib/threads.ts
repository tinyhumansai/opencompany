// Conversation threads: WhatsApp-style "chats" with the company. Every thread
// talks to the same company chat endpoint; a thread just scopes a transcript
// and gives the company side a consistent identity (a "desk" you're talking to).

import type { DeskDto, OperatorChannelDto, TeamMemberDto } from "../api/types";
import { MAIN_THREAD_ID, type ChatMessage } from "./chat";
import { toneFor } from "./team";

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
  /** Whether the composer for this thread is disabled. */
  readOnly?: boolean;
}

/** Avatar tones rotated across desk threads. */
const DESK_TONES = ["sky", "violet", "amber", "emerald", "rose", "cyan"];

/** The company's main line — the orchestrator you talk to for anything. */
function mainThread(): Thread {
  return {
    id: MAIN_THREAD_ID,
    contact: { name: "Your company", kind: "company" },
    blurb: "The main line — ask for anything",
    messages: [],
  };
}

/**
 * The read-only Operator system feed (issue #1757 rework), as a thread for
 * the legacy `#/conversation` route.
 *
 * `Conversation` — the route Chat became the nav-listed successor to in #361
 * — still reads plain `threads`, unlike Chat's own channel model which
 * already appends the pinned row (`operatorSection`, `views/chat/model.ts`).
 * Without this, `#/conversation` never receives an Operator thread at all,
 * so its `readOnly` plumbing (`Conversation.tsx` already forwards
 * `thread.readOnly` to the composer) has nothing to gate and workflow
 * reports cannot be opened there (issue #1781 review, Codex P2).
 *
 * `readOnly: true` for the same reason Chat's pinned row carries no member
 * or mutation routes: this is an aggregation surface, not a conversation —
 * see `operatorChannelFrom`'s `system: true` for the Channel-model sibling
 * of this same fact.
 */
export function operatorThread(dto: OperatorChannelDto): Thread {
  return {
    id: dto.id,
    contact: { name: dto.name, kind: "company" },
    blurb: dto.description,
    messages: [],
    readOnly: true,
  };
}

/** The default chat list: the company's main line plus a few focused desks. */
export function defaultThreads(): Thread[] {
  return [
    mainThread(),
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

/**
 * Build the chat list from the company's real desks (issue #53): the main line
 * (the orchestrator) first, then one thread per desk keyed by its id.
 *
 * A company with no desks gets the main line and nothing else. It used to get
 * {@link defaultThreads} — Strategy desk, Creative studio, Front desk — which
 * put three threads in the list for desks the company had never declared and
 * the host could not route to. `defaultThreads` is now only for a host that
 * never answered at all (no `/desks` route, or a failed read): the shell's
 * `.catch` leg, not this one. An empty answer is an answer.
 */
export function threadsFromDesks(desks: DeskDto[]): Thread[] {
  const deskThreads: Thread[] = desks.map((desk, i) => ({
    id: desk.id,
    contact: {
      name: desk.name,
      kind: "agent",
      tone: DESK_TONES[i % DESK_TONES.length],
    },
    blurb: desk.description ?? "A desk of your company",
    messages: [],
  }));
  return [mainThread(), ...deskThreads];
}

/**
 * One DM thread per roster teammate (issue #151 §3.3): the agent's own console,
 * so an operator can follow up with the teammate who did the work instead of
 * going back through the orchestrator.
 *
 * Keyed by the **agent id**, which the host resolves straight to that teammate.
 * Desks are listed first and win any id collision — `existingIds` is what keeps
 * a teammate who is already reachable as a desk from appearing twice.
 *
 * Kept separate from {@link threadsFromDesks} so a host that exposes desks but
 * not `/team` (or fails that fetch) simply gets no DMs, rather than losing its
 * desk list too.
 */
export function agentDmThreads(
  team: TeamMemberDto[],
  existingIds: Iterable<string>,
): Thread[] {
  const taken = new Set(existingIds);
  const seen = new Set<string>();
  const threads: Thread[] = [];
  for (const member of team) {
    const id = member.id?.trim();
    // A teammate with no id has nothing the host could route to, and a
    // duplicate id would collide with the thread already added for it.
    if (!id || taken.has(id) || seen.has(id)) continue;
    seen.add(id);
    const name = member.name?.trim() || member.role;
    threads.push({
      id,
      contact: { name, kind: "agent", tone: toneFor(id) },
      blurb: member.description?.trim() || member.role,
      messages: [],
    });
  }
  return threads;
}
