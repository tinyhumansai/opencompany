import { ExternalLink, Sparkles } from "lucide-react";

import { cn } from "@/lib/utils";
import {
  CHARGEBEE_SIGNUP_URL,
  offerCopy,
  offerState,
} from "@/views/finance/chargebee-offer";

/**
 * The slim strip a **connected** company keeps at the top of Invoicing.
 *
 * # Why it is one line and not the pitch
 *
 * The pitch argues for a decision that has already been made. Leaving it up
 * would tax every visit to the invoice list with an advertisement for something
 * this company already bought — which is the same mistake the collapsible
 * connection panel exists to avoid. What survives the decision is the reward,
 * so what survives on screen is the reward, on one line.
 *
 * # What it does not claim
 *
 * It does not say how many credits this company has earned or has left. There
 * is no host route that knows: `GET …/billing` reports the credential's state
 * and nothing about an offer, and a figure rendered here would have to be
 * invented. So the strip states the offer's terms and links out to where the
 * balance actually lives, and a real earned/claimable counter waits on a host
 * endpoint that can answer it honestly.
 *
 * Absent entirely when no offer has been agreed — the same three-state
 * discipline as `ChargebeePitch`, from the same `offerState`.
 */
export function ChargebeeCreditsBanner() {
  const state = offerState();
  const offer = offerCopy(state);
  if (!offer) return null;

  return (
    <div
      className={cn(
        "flex flex-wrap items-center gap-x-2 gap-y-1 rounded-lg px-3 py-2",
        "bg-[color-mix(in_oklab,var(--brand-chargebee)_10%,var(--card))]",
        state === "preview" &&
          "border border-dashed border-(--brand-chargebee)",
      )}
      data-testid="chargebee-credits-banner"
      data-offer-state={state}
    >
      <Sparkles className="size-3.5 shrink-0 text-(--brand-chargebee)" />
      <span className="text-xs font-medium">{offer.headline}</span>
      <span className="text-xs text-muted-foreground">{offer.trigger}.</span>
      {CHARGEBEE_SIGNUP_URL ? (
        <a
          className="ml-auto inline-flex items-center gap-1 text-xs font-medium underline underline-offset-4"
          href={CHARGEBEE_SIGNUP_URL}
          target="_blank"
          rel="noreferrer noopener"
          data-testid="chargebee-credits-claim"
        >
          View on Chargebee <ExternalLink className="size-3" />
        </a>
      ) : null}
      {state === "preview" ? (
        <span className="text-xs text-(--brand-chargebee)">Placeholder</span>
      ) : null}
    </div>
  );
}
