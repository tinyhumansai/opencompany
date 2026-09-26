// The company's desks: the standing lines you can address. Each one becomes a
// channel in the chat workspace. They all post to the same company endpoint —
// a desk scopes a transcript and fixes the company side's identity, it is not
// a separate backend.

import { isGeneralChannel } from "@/lib/chat";

// `GENERAL_CHANNEL` and `isGeneralChannel` live in `lib/chat.ts` beside
// `MAIN_THREAD_ID` — they are facts about chat addressing, and
// `dispatchMarkerPlacement` there has to apply them. Re-exported so every
// reader keeps importing them from where desks are described.
export { GENERAL_CHANNEL, isGeneralChannel } from "@/lib/chat";

/**
 * Does this desk answer to the legacy company-wide line?
 *
 * Id **or** display name, mirroring the host's `resolve_desk_id`, which matches
 * a desk by either — so a blueprint declaring `id = "ops", name = "General"`
 * owns the line exactly as one declaring `id = "general"` does, and a General
 * spelling in a deep link opens that desk rather than the read-only archive.
 *
 * **A blueprint desk only.** `resolve_desk_id` declines the operator-created
 * overlay half for a General key, so an overlay desk *named* `General` answers
 * only to its own id and does not have the line. `overlayCreated` is the host's
 * own word for the distinction, carried through {@link Desk} for exactly this.
 *
 * The host reserves both spellings against newly created desks; a manifest can
 * still declare either, and that is the grandfathered case.
 */
export function deskClaimsGeneralChannel(desk: {
  id: string;
  name: string;
  overlayCreated?: boolean;
}): boolean {
  if (desk.overlayCreated) return false;
  return isGeneralChannel(desk.id) || isGeneralChannel(desk.name);
}

export interface Desk {
  id: string;
  /** The channel name, rendered after a `#`. Lowercase, no spaces. */
  channel: string;
  /** How the desk signs its messages — a person-ish name, not a slug. */
  name: string;
  /** One line on what the desk is for; the channel's purpose. */
  blurb: string;
  /** Avatar tone key. The main line uses the brand mark instead. */
  tone?: string;
  /**
   * The desk's own members, as roster teammate ids, in the host's order —
   * `members[0]` is the lead. Optional on purpose: the static desks below have
   * no membership at all, and "this desk's membership is unknown" has to stay
   * distinguishable from "this desk has nobody on it". A consumer that finds
   * it absent should fall back to the company-wide roster rather than render
   * an empty channel (issue #369).
   */
  members?: string[];
  /**
   * The subset of {@link members} added through the operator overlay rather
   * than declared in the manifest. Carried through so a later surface can tell
   * the removable members from the blueprint ones without refetching.
   */
  overlayMembers?: string[];
  /**
   * Whether the whole desk was operator-created rather than declared in the
   * manifest blueprint — the host's own `overlayCreated` (issue #1743).
   *
   * Needed because the two are not interchangeable to the host: only a
   * blueprint desk is grandfathered onto the company-wide line
   * ({@link deskClaimsGeneralChannel}). Absent on the static fallback desks,
   * which are neither.
   */
  overlayCreated?: boolean;
  /**
   * How the desk routes its unmentioned messages (issue #1835). `"auto"` is a
   * leadless channel — `members[0]` carries no rank and the host picks a
   * best-fit member per message. Absent means `"lead"`, today's model.
   */
  responder?: "lead" | "auto";
}

/**
 * A few focused desks, for a host that exposes none of its own.
 *
 * **No `#general` row.** A fabricated General desk would be indistinguishable
 * from one a blueprint really declared, and {@link deskClaimsGeneralChannel}
 * would then hand it the legacy line — the same shape as issue #370.
 */
export function defaultDesks(): Desk[] {
  return [
    {
      id: "strategy",
      channel: "strategy",
      name: "Strategy desk",
      blurb: "Plans, priorities, and direction",
      tone: "sky",
    },
    {
      id: "creative",
      channel: "creative",
      name: "Creative studio",
      blurb: "Copy, design, and campaigns",
      tone: "violet",
    },
    {
      id: "frontdesk",
      channel: "front-desk",
      name: "Front desk",
      blurb: "Scheduling, inbox, and errands",
      tone: "amber",
    },
  ];
}
