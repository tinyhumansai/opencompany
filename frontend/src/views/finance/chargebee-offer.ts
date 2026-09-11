// The Chargebee migration offer, in one place.
//
// # Why this is a module and not copy inside the component
//
// Every string here is a **commercial claim** — what a company is promised for
// moving its billing, what they have to do to earn it, and the terms it is
// subject to. None of it is the console's to invent, and a claim invented to
// fill a layout is one somebody eventually has to honour. So the pitch reads
// its wording from here, and the surface that renders it is written to say
// *nothing* about credits until these are filled in (see `offerState`).
//
// Fill these in and the credits pitch appears on Finance → Invoicing. Leave
// them and the page still redesigns around Chargebee — it simply makes no offer.

/** The offer's wording. Every field is required before any of it renders. */
export interface ChargebeeOffer {
  /** The reward, as a headline. e.g. `"$500 in agent credits"`. */
  headline: string;
  /** What earns it, as a sentence fragment following the headline. */
  trigger: string;
  /** Terms, eligibility, expiry — whatever legal wants under it. */
  finePrint: string;
}

/**
 * The offer, or `null` while it has not been agreed.
 *
 * TODO(offer): replace with the real terms. Until then this is `null`, and the
 * page deliberately makes no claim about credits in production.
 */
export const CHARGEBEE_OFFER: ChargebeeOffer | null = null;

/**
 * Where "Start on Chargebee" goes.
 *
 * TODO(link): the partner/referral URL, with its attribution parameters intact.
 * Empty until then, and the pitch renders its outbound CTA as unavailable
 * rather than falling back to Chargebee's generic trial page — a generic link
 * carries no attribution, so a migration driven from here would not be credited
 * and the offer above could not be honoured for the company that took it.
 */
export const CHARGEBEE_SIGNUP_URL = "";

/**
 * Chargebee's own migration documentation — not a placeholder, and not part of
 * the offer. It is the answer to "what does moving actually involve", which is
 * the question that stops a migration far more often than the price does.
 */
export const CHARGEBEE_MIGRATION_DOCS_URL =
  "https://www.chargebee.com/docs/2.0/migration.html";

/**
 * What the surfaces may render right now.
 *
 * - `live` — real terms, agreed. Render them.
 * - `preview` — not agreed, but this is a dev build. Render them **visibly
 *   marked as placeholder** so the design can be reviewed before the terms
 *   exist. Never reachable in a production bundle.
 * - `absent` — not agreed, production. Say nothing about credits at all.
 *
 * The `preview` rung is the whole reason this is a three-state function rather
 * than a null check. A placeholder that renders like real copy is how "TODO:
 * e.g. $500 in credits" ends up on a tenant's billing page, and the fix cannot
 * be "remember to take it out".
 */
export type OfferState = "live" | "preview" | "absent";

export function offerState(
  offer: ChargebeeOffer | null = CHARGEBEE_OFFER,
  isDev: boolean = import.meta.env.DEV,
): OfferState {
  if (offer) return "live";
  return isDev ? "preview" : "absent";
}

/** The wording to render, real or placeholder, for a state that renders any. */
export const PLACEHOLDER_OFFER: ChargebeeOffer = {
  headline: "500 free agent credits",
  trigger: "when this company sends its first Chargebee invoice",
  finePrint:
    "Placeholder terms — not an offer. Set CHARGEBEE_OFFER to publish.",
};

/** The offer a surface should display, given the state it resolved. */
export function offerCopy(state: OfferState): ChargebeeOffer | null {
  if (state === "live") return CHARGEBEE_OFFER;
  if (state === "preview") return PLACEHOLDER_OFFER;
  return null;
}
