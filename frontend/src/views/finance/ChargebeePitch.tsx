import {
  ArrowRight,
  ExternalLink,
  FileText,
  RefreshCw,
  Sparkles,
} from "lucide-react";

import { ChargebeeIcon } from "@/components/chargebee-icon";
import { Badge } from "@/components/ui/badge";
import { Button, buttonVariants } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { cn } from "@/lib/utils";
import {
  CHARGEBEE_MIGRATION_DOCS_URL,
  CHARGEBEE_SIGNUP_URL,
  offerCopy,
  offerState,
} from "@/views/finance/chargebee-offer";

interface Props {
  /** Opens the credential panel below — the "I already have a site" path. */
  onConnect: () => void;
}

/**
 * Finance → Invoicing, for a company that has **not** connected Chargebee.
 *
 * # Why this replaced a credential form
 *
 * The page used to open on a Site field and an API-key field. That is the right
 * surface for somebody who already has a Chargebee account and came here to
 * paste its key — and it is the wrong first thing for everybody else, because
 * it answers a question ("where do the credentials go") that nobody who has not
 * decided to use Chargebee is asking yet. A form is not a reason.
 *
 * So the decision comes first and the form comes second: what agent-run billing
 * buys you, what moving actually involves, and what this company earns for
 * doing it. The credential form is still one click away and unchanged —
 * `onConnect` opens it — because the person who *did* come here to paste a key
 * must not be made to read a pitch to reach it.
 *
 * # Nothing here renders a claim that has not been agreed
 *
 * The credits block is the only part of this that promises anything, and it is
 * the only part that can be absent: `offerState` returns `absent` in a
 * production build with no agreed terms, and the whole strip does not render.
 * In a dev build it returns `preview` and the strip renders **visibly marked**,
 * so this design can be reviewed before the terms exist without a placeholder
 * ever reaching a tenant. See `chargebee-offer.ts`.
 *
 * The outbound CTA is under the same discipline. With no partner URL set it is
 * a disabled control that says so, rather than a link to Chargebee's generic
 * trial page: a generic signup carries no attribution, so a company that
 * migrated through it could not be credited for the offer above.
 */
export function ChargebeePitch({ onConnect }: Props) {
  const state = offerState();
  const offer = offerCopy(state);
  const canStart = CHARGEBEE_SIGNUP_URL.length > 0;

  return (
    <Card className="relative overflow-hidden" data-testid="chargebee-pitch">
      {/*
        The brand wash. `color-mix` against the token rather than a hard-coded
        rgba, so the tint follows `--brand-chargebee` if the mark's colour is
        ever corrected, and so it composes over whichever `--card` the active
        theme resolved — a fixed translucent orange reads as mud on the dark
        surface and as a highlighter on the light one.
      */}
      <div
        aria-hidden
        className="pointer-events-none absolute inset-0"
        style={{
          background:
            "linear-gradient(135deg, color-mix(in oklab, var(--brand-chargebee) 12%, transparent) 0%, transparent 55%)",
        }}
      />
      {/*
        The mark, oversized and bled off the corner. Decorative — the title row
        below already names Chargebee at a legible size, and this is the half of
        "highlight Chargebee" that type cannot do.
      */}
      <ChargebeeIcon className="pointer-events-none absolute -top-10 -right-10 size-56 text-(--brand-chargebee) opacity-[0.07]" />

      <CardContent className="relative space-y-6">
        <div className="space-y-3">
          <div className="flex items-center gap-2">
            <ChargebeeIcon className="size-4 text-(--brand-chargebee)" />
            <span className="text-sm font-medium">Chargebee</span>
            <Badge variant="outline" className="text-muted-foreground">
              Not connected
            </Badge>
          </div>

          <h2 className="max-w-2xl text-2xl font-semibold tracking-tight">
            Let your agents run billing end to end.
          </h2>
          <p className="max-w-2xl text-sm text-muted-foreground">
            Chargebee is the subscription-billing platform behind this
            company&apos;s invoicing. Connect a site and your teammates can
            raise an invoice from a conversation, send it, and know when it is
            paid — without anybody opening a billing console.
          </p>
        </div>

        {offer ? (
          <div
            className={cn(
              "flex flex-wrap items-center gap-x-3 gap-y-2 rounded-lg px-4 py-3",
              // The offer's own surface, tinted from the brand token so it
              // reads as part of the Chargebee block rather than as a generic
              // callout that happens to sit inside one.
              "bg-[color-mix(in_oklab,var(--brand-chargebee)_10%,var(--card))]",
              // `preview` is deliberately ugly. It is a dashed outline around
              // wording nobody has agreed to, and it exists so a reviewer can
              // never mistake it for the real thing.
              state === "preview" &&
                "border border-dashed border-(--brand-chargebee)",
            )}
            data-testid="chargebee-offer"
            data-offer-state={state}
          >
            <Sparkles className="size-4 shrink-0 text-(--brand-chargebee)" />
            <span className="text-sm font-medium">{offer.headline}</span>
            <span className="text-sm text-muted-foreground">
              {offer.trigger}.
            </span>
            {state === "preview" ? (
              <Badge variant="outline" className="text-(--brand-chargebee)">
                Placeholder
              </Badge>
            ) : null}
          </div>
        ) : null}

        <div className="flex flex-wrap items-center gap-3">
          {canStart ? (
            // An anchor wearing the button's own variant, which is how this
            // console renders a link that acts like a primary control (the
            // Button primitive here has no `asChild`). It must stay an `<a>`:
            // this leaves the console for another origin, and a `<button>` that
            // navigates cannot be opened in a new tab or copied.
            <a
              className={buttonVariants()}
              href={CHARGEBEE_SIGNUP_URL}
              target="_blank"
              rel="noreferrer noopener"
              data-testid="chargebee-start"
            >
              Start on Chargebee
              <ArrowRight className="size-4" />
            </a>
          ) : (
            // Not a link to the generic trial page: see the note above. A
            // control that says why it cannot act is worth more than one that
            // works and silently forfeits the offer behind it.
            <Button disabled data-testid="chargebee-start-unavailable">
              Start on Chargebee
            </Button>
          )}
          <Button
            variant="outline"
            onClick={onConnect}
            data-testid="chargebee-already-have"
          >
            I already have a Chargebee site
          </Button>
          <a
            className="inline-flex items-center gap-1 text-xs font-medium text-muted-foreground underline underline-offset-4 hover:text-foreground"
            href={CHARGEBEE_MIGRATION_DOCS_URL}
            target="_blank"
            rel="noreferrer noopener"
            data-testid="chargebee-migration-docs"
          >
            What moving involves <ExternalLink className="size-3" />
          </a>
        </div>

        {!canStart ? (
          <p
            className="text-xs text-muted-foreground"
            data-testid="chargebee-start-note"
          >
            The Chargebee signup link has not been configured on this build, so
            the button above cannot send you anywhere yet. If you already have a
            site, connect it now.
          </p>
        ) : null}

        <div className="grid gap-4 border-t pt-6 sm:grid-cols-3">
          <Benefit
            icon={<FileText className="size-4 text-(--brand-chargebee)" />}
            title="Agents bill for you"
            body="Raise and send an invoice from a chat message. Payment status comes back on its own, so nobody has to go and look."
          />
          <Benefit
            icon={<RefreshCw className="size-4 text-(--brand-chargebee)" />}
            title="Bring the billing you have"
            body="Chargebee imports existing customers, plans and subscriptions, so moving across is a migration rather than a rebuild."
          />
          <Benefit
            icon={<Sparkles className="size-4 text-(--brand-chargebee)" />}
            title={offer ? "Earn credits doing it" : "One key, every agent"}
            body={
              offer
                ? `${offer.headline} ${offer.trigger} — spent on the model your agents think with.`
                : "Connect once and every teammate in this company can invoice. The key is stored write-only and never shown again."
            }
          />
        </div>

        {offer ? (
          <p
            className="text-xs text-muted-foreground"
            data-testid="chargebee-offer-fine-print"
          >
            {offer.finePrint}
          </p>
        ) : null}
      </CardContent>
    </Card>
  );
}

function Benefit({
  icon,
  title,
  body,
}: {
  icon: React.ReactNode;
  title: string;
  body: string;
}) {
  return (
    <div className="space-y-1.5">
      <div className="flex items-center gap-2">
        {icon}
        <span className="text-sm font-medium">{title}</span>
      </div>
      <p className="text-xs leading-relaxed text-muted-foreground">{body}</p>
    </div>
  );
}
