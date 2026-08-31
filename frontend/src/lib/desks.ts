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
 * Does this desk answer to the company-wide line?
 *
 * Id **or** display name, mirroring the host's `resolve_desk_id`, which matches
 * a desk by either — so a blueprint declaring `id = "ops", name = "General"`
 * routes the built-in line at that desk exactly as one declaring
 * `id = "general"` does. An id-only test rendered it as a second `#general` row
 * beside the built-in channel while the host answered from the desk, which is
 * the same duplicate this predicate exists to prevent.
 *
 * **A blueprint desk only.** `resolve_desk_id` searches the manifest desks and
 * then the operator-created overlay ones, and it declines the overlay half for
 * a General key outright — so an overlay desk *named* `General` answers only to
 * its own id and does not have the line. It is projected under that id like any
 * other desk (`GET .../desks`), and letting its name claim the line here would
 * take `#general` off the rail in a company where the host still answers it as
 * the orchestrator. `overlayCreated` is the host's own word for the
 * distinction, carried through {@link Desk} for exactly this.
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
 * **No `#general` row.** It used to carry one — `id: "main"` — back when the
 * company-wide line existed only here. Since issue #1743 the rail projects
 * `#general` from the roster instead ({@link buildChannels}), so a row here
 * would be a second one sitting beside it.
 *
 * Removing it is not just de-duplication. While it existed, `isGeneralChannel`
 * was true of a row in this fabricated set *and* of a desk a blueprint really
 * declared, and nothing downstream could tell the two apart — which is exactly
 * how a `[[group_chat]] id = "general"` came out as two channels folding onto
 * one transcript, and a `[[group_chat]] id = "main"` came out hidden while the
 * host still routed to its lead. Console-side desk fabrication being
 * indistinguishable from a real desk is the same shape as issue #370.
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
