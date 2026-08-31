// The pure half of the agent profile panel: what a teammate's face is worth
// summarising when you click it, and the two addresses the panel hands off to.
//
// Kept out of the component because both are quietly easy to get wrong. A
// summary that shows the blueprint text where an override is in force describes
// an agent nobody is running, and an "Edit" button that navigates to the plain
// detail address lands the operator on a read-only page with the control they
// asked for one more click away.

import type { AgentDetailDto } from "@/api/types";
import { avatarRef } from "@/lib/avatar";
import { summarizeGrants, tierLabel, type ToolGrantSummary } from "@/lib/agent";
import { roleSubtitle, toneFor } from "@/lib/team";

/** How many characters of the persona the panel shows before it truncates. */
const ABOUT_LIMIT = 320;

/** A teammate as the profile panel renders them. */
export interface AgentProfile {
  /** Name where the teammate has one, else the role — same fallback as the card. */
  display: string;
  /** The role, or `null` when it would only repeat {@link display}. */
  subtitle: string | null;
  /** Avatar seed: the id where there is one, so a rename never changes the face. */
  seed: string;
  tone: string;
  avatar: string;
  /** "Orchestrator" or "Worker", resolved by the host rather than by the tier string. */
  tier: string;
  /** Where this teammate came from, in the operator's words. */
  origin: string;
  /**
   * The persona actually in force, clipped for a panel. The **effective**
   * instructions first — that is the text the agent runs on — and the
   * description only as a fallback, because a teammate can have one without the
   * other.
   */
  about: string | null;
  /** True when `about` had to be cut, so the panel can say "read the rest". */
  aboutTruncated: boolean;
  tools: ToolGrantSummary;
}

export function agentProfile(detail: AgentDetailDto): AgentProfile {
  const display = detail.name?.trim() || detail.role;
  const seed = detail.id || display;
  const full = detail.instructions?.trim() || detail.description?.trim() || "";
  const clipped = clip(full, ABOUT_LIMIT);
  return {
    display,
    subtitle: roleSubtitle(display, detail.role),
    seed,
    tone: toneFor(seed),
    // The face this teammate chose, else the hashed default — the same rule
    // the roster row uses, so clicking a customised face in chat opens the
    // panel already wearing it rather than swapping it for the default.
    avatar: avatarRef(detail.avatar, seed),
    tier: tierLabel(detail),
    origin: detail.source === "manifest" ? "Company blueprint" : "Added here",
    about: clipped || null,
    aboutTruncated: clipped !== full,
    tools: summarizeGrants(detail.tools),
  };
}

/**
 * Cut on a word boundary where there is one near the limit, so the panel ends
 * on a word rather than mid-syllable. Text at or under the limit comes back
 * untouched — `agentProfile` compares the two to decide whether anything was
 * lost, so returning an equal-but-shortened string would claim a truncation
 * that never happened.
 */
function clip(text: string, limit: number): string {
  if (text.length <= limit) return text;
  const head = text.slice(0, limit);
  const space = head.lastIndexOf(" ");
  return `${(space > limit * 0.6 ? head.slice(0, space) : head).trimEnd()}…`;
}

/**
 * The teammate's own page.
 *
 * `edit: true` appends the `?edit` flag `AgentDetailView` opens its form on
 * (`use-hash-flag.ts`). A query suffix rather than a third path segment because
 * `useHashView` splits the hash at `?` before it parses segments — so the flag
 * rides along without the router ever seeing it, and Back closes the editor
 * instead of leaving the page.
 */
export function agentHref(agentId: string, options: { edit?: boolean } = {}): string {
  const path = `#/team/${encodeURIComponent(agentId)}`;
  return options.edit ? `${path}?edit` : path;
}
