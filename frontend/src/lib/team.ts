// The company's team: the agents that do the work. When the host exposes its
// roster (`GET .../team`) the console shows that; otherwise it starts from a
// generic, company-agnostic roster the operator can edit. Either way, agents
// are user-definable here.

import type { TeamMemberDto } from "@/api/types";

export interface TeamMember {
  id: string;
  name: string;
  role: string;
  description: string;
  /** Avatar tone key; derived from the id so colors stay stable. */
  tone: string;
}

const TONE_KEYS = ["sky", "violet", "amber", "emerald", "rose", "cyan", "indigo", "teal"];

export function toneFor(seed: string): string {
  let hash = 0;
  for (let i = 0; i < seed.length; i++) hash = (hash * 31 + seed.charCodeAt(i)) | 0;
  return TONE_KEYS[Math.abs(hash) % TONE_KEYS.length];
}

export function initials(name: string): string {
  return (
    name
      .trim()
      .split(/\s+/)
      .slice(0, 2)
      .map((p) => p.charAt(0).toUpperCase())
      .join("") || "?"
  );
}

/** Map a host roster entry into the console's team model. */
export function fromDto(dto: TeamMemberDto): TeamMember {
  const name = dto.name?.trim() || dto.role;
  return {
    id: dto.id,
    name,
    role: dto.role,
    description: dto.description ?? "",
    tone: toneFor(dto.id || name),
  };
}

let n = 0;
const id = () => `member-${n++}`;

function member(name: string, role: string, description: string): TeamMember {
  return { id: id(), name, role, description, tone: toneFor(name) };
}

/** A generic starter team that fits any company; the operator edits from here. */
export function starterTeam(): TeamMember[] {
  return [
    member("Ops Lead", "Operations Lead", "Keeps work moving and unblocks the team."),
    member("Researcher", "Researcher", "Gathers facts, sources, and context."),
    member("Writer", "Writer", "Drafts copy, docs, and outbound messages."),
    member("Designer", "Designer", "Creates visuals and holds the brand."),
    member("Analyst", "Analyst", "Measures performance and reports back."),
    member("Front Desk", "Front Desk", "Scheduling, inbox, and everyday errands."),
  ];
}

/** Create a member from operator-entered fields. */
export function newMember(fields: { name: string; role: string; description: string }): TeamMember {
  const memberId = id();
  return {
    id: memberId,
    name: fields.name.trim(),
    role: fields.role.trim(),
    description: fields.description.trim(),
    tone: toneFor(memberId),
  };
}

export const TEAM_TONES: Record<string, string> = {
  sky: "bg-sky-500/15 text-sky-600 dark:text-sky-400",
  violet: "bg-violet-500/15 text-violet-600 dark:text-violet-400",
  amber: "bg-amber-500/15 text-amber-600 dark:text-amber-400",
  emerald: "bg-emerald-500/15 text-emerald-600 dark:text-emerald-400",
  rose: "bg-rose-500/15 text-rose-600 dark:text-rose-400",
  cyan: "bg-cyan-500/15 text-cyan-600 dark:text-cyan-400",
  indigo: "bg-indigo-500/15 text-indigo-600 dark:text-indigo-400",
  teal: "bg-teal-500/15 text-teal-600 dark:text-teal-400",
};
