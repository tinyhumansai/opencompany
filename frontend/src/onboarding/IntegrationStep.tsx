import { useEffect, useState } from "react";
import { ArrowRight, KeyRound } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import { getComposioStatus } from "@/api/composio";
import { Button } from "@/components/ui/button";

/**
 * Step 2 of the first-run gate, built for the card it is drawn in (bug B-001).
 *
 * **Not `OAuthView`.** The gate used to embed that route-level view whole, on
 * the reasoning that reusing the console's own flow could never drift from the
 * page the sidebar reaches. The reasoning was sound and the result was not: a
 * full connections page inside a checklist card renders eight provider tiles
 * that all read "not available here", a disabled "connect by slug" box, and two
 * bare password fields — a screen whose every control is dead, with no sentence
 * anywhere saying what the founder is supposed to do about it. Reuse is only
 * free when both callers can give the component what it assumes, and a
 * height-constrained card cannot give a route-level view a route.
 *
 * So this is the card-sized thing instead: name what the step wants, say
 * plainly what has to exist before any provider can be connected, point at the
 * page where that is entered — and, because a founder on a build with no
 * credential path has no way to satisfy it at all, offer an honest way past it
 * that is remembered (see [`markGateStepWaived`] for why that has to be durable
 * rather than session-scoped).
 *
 * **Two different sentences for two different reasons `integrationConnected`
 * reads false** (Codex review, PR #2046). `src/company/activation.rs` derives
 * that step from whether an active Composio CONNECTION exists — not from
 * whether a CREDENTIAL exists. A hosted founder can already have an
 * `attested`/`company`/`static` credential (`ComposioCredentialSource`,
 * `@/api/composio`) and simply not have connected a provider yet, which is an
 * ordinary, always-completable action — the exact opposite of the "self-hosted,
 * no lever at all" case the original copy and its waiver escape hatch were
 * written for. Telling that founder "this company needs a credential" and
 * offering to waive a step they can finish normally would both be wrong, so
 * this reads `getComposioStatus` the same way `OAuthView` does and branches on
 * `credentialSource !== "none"`.
 *
 * **The waiver needs its own "do we actually know" flag, separate from the
 * copy's `hasCredential`** (Codex review, PR #2046). `hasCredential` starts
 * `false` so the COPY defaults to the safe "no credential" reading while the
 * read is in flight or fails — but the waive button used to key off that same
 * boolean, which means it was VISIBLE for that entire window too. A durable
 * waiver clicked during it would permanently mark a step skipped that a
 * confirmed-slow `getComposioStatus` might have gone on to report as already
 * credentialed — exactly the "waive a step you could complete normally" harm
 * the credential-vs-connection fix above exists to prevent, just reached
 * through the timing instead of the verdict. `credentialConfirmed` is `true`
 * only once a read has actually SETTLED with an answer (never on failure —
 * unknown stays unknown, not "confirmed none"), and the waive button and its
 * footer both gate on it in addition to `!hasCredential`.
 */
export function IntegrationStep({
  client,
  company,
  onOpenApps,
  onWaive,
}: {
  client: OpenCompanyClient;
  company: string | null;
  /** Leaves the gate for the real Apps page — see `OnboardingGate`'s `onLeave`. */
  onOpenApps: () => void;
  /** Records this step as answered as far as this build allows. */
  onWaive: () => void;
}) {
  // Defaults to "no credential" — the same copy this card always showed
  // before this read existed — so a still-loading or failed read costs one
  // extra "enter a credential" prompt rather than ever claiming a credential
  // exists when the read could not confirm one.
  const [hasCredential, setHasCredential] = useState(false);
  // Separate from `hasCredential` on purpose — see this component's own doc.
  // Only a SUCCESSFUL read flips this; a failure leaves it `false` right
  // alongside "still loading", so the durable waiver stays unreachable for
  // either until a read has actually confirmed there is nothing to connect.
  const [credentialConfirmed, setCredentialConfirmed] = useState(false);

  useEffect(() => {
    let live = true;
    void getComposioStatus(client, company).then(
      (status) => {
        if (!live) return;
        setHasCredential(status.credentialSource !== "none");
        setCredentialConfirmed(true);
      },
      () => {
        /* transient failure — stay on the safe "no credential" default, and
         * leave `credentialConfirmed` false so the waiver stays withheld too */
      },
    );
    return () => {
      live = false;
    };
  }, [client, company]);

  return (
    <div className="space-y-4" data-testid="gate-integration-step">
      {hasCredential ? (
        <div className="space-y-2 text-sm text-muted-foreground" data-testid="gate-integration-has-credential">
          <p>
            Teammates reach Gmail, Slack and GitHub through a connected account. This
            company already has a credential to connect one with — Apps is where you pick
            a provider and connect it.
          </p>
          <p className="flex items-start gap-2 rounded-lg border bg-muted/40 px-3 py-2">
            <KeyRound aria-hidden className="mt-0.5 size-4 shrink-0" />
            <span>
              This step needs an actual connected provider, not just a credential — open
              Apps and connect one to finish it.
            </span>
          </p>
        </div>
      ) : (
        <div className="space-y-2 text-sm text-muted-foreground">
          <p>
            Teammates reach Gmail, Slack and GitHub through a connected account. Before any
            provider can be connected, this company needs a credential to connect it with — a
            TinyHumans account key, or a Composio token of your own.
          </p>
          <p className="flex items-start gap-2 rounded-lg border bg-muted/40 px-3 py-2">
            <KeyRound aria-hidden className="mt-0.5 size-4 shrink-0" />
            <span>
              Self-hosted builds ship without one. Until a credential is entered, every
              provider stays unavailable — that is the build, not a fault in your setup.
            </span>
          </p>
        </div>
      )}

      <div className="flex flex-wrap items-center gap-2">
        <Button onClick={onOpenApps} data-testid="gate-integration-open-apps">
          {hasCredential ? "Connect a provider in Apps" : "Enter a credential in Apps"}
          <ArrowRight className="size-4" />
        </Button>
        {/* A credential that exists is always a completable step — offering to
            waive it the way a build with no lever at all needs to would invite
            a founder who could just connect a provider to skip it instead.
            Gated on `credentialConfirmed` too (Codex review, PR #2046): while
            the read is still in flight or has failed, we do not yet KNOW
            which case this is, and a durable waiver clicked in that window
            would be as wrong as the credential-vs-connection mix-up this
            whole component exists to prevent. */}
        {!hasCredential && credentialConfirmed && (
          <Button variant="ghost" onClick={onWaive} data-testid="gate-integration-waive">
            I don&apos;t have one — skip this step
          </Button>
        )}
      </div>

      {!hasCredential && credentialConfirmed && (
        <p className="text-xs text-muted-foreground">
          Skipping is remembered for this company, so this step won&apos;t be asked again in a
          new tab. Connect an account later from Apps whenever you have a credential.
        </p>
      )}
    </div>
  );
}
