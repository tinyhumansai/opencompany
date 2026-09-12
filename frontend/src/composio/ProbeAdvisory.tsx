import { AlertTriangle, X } from "lucide-react";

import { probeCopy } from "./classify";
import type { ComposioSubmitOutcome } from "./classify";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

/**
 * What the last attempt came back as.
 *
 * Two tones, and the difference is not decorative. An **advisory** means the
 * credential was stored and only the check failed — amber and dismissible,
 * because colouring it red would be a lie about what happened. A **rejection**
 * means nothing was stored, which is the one case that is actually an error.
 *
 * `aria-live` because both arrive without a navigation: a banner that appears
 * where a button used to be is invisible to a screen reader otherwise.
 */
export function ProbeAdvisory({
  outcome,
  skipOffered,
  busy,
  onSkip,
  onDismiss,
  testIdPrefix = "composio-probe",
}: {
  outcome: ComposioSubmitOutcome;
  skipOffered: boolean;
  busy: boolean;
  onSkip: () => void;
  onDismiss: () => void;
  /**
   * Namespace for this banner's test ids.
   *
   * A write's outcome and a check's verdict can be on screen at once — they are
   * separate state, cleared on separate actions — and two banners answering to
   * one id is a selector that silently picks whichever rendered first.
   */
  testIdPrefix?: string;
}) {
  const error = outcome.kind === "rejected";
  return (
    <div
      role="status"
      aria-live="polite"
      data-testid={error ? `${testIdPrefix}-error` : `${testIdPrefix}-advisory`}
      className={cn(
        "flex flex-wrap items-start gap-2 rounded-md border p-3 text-xs",
        error
          ? "border-status-failed/40 bg-status-failed-soft"
          : "border-status-blocked/40 bg-status-blocked-soft",
      )}
    >
      <AlertTriangle
        className={cn(
          "mt-0.5 size-3.5 shrink-0",
          error ? "text-status-failed-text" : "text-status-blocked-text",
        )}
      />
      <span className="min-w-0 flex-1">
        {/* Never the raw upstream string for an unclassified failure — see
            `classify.ts`. This banner is the most screenshot-able surface on
            the page. */}
        {outcome.message || probeCopy("unknown")}
      </span>
      {skipOffered && (
        <Button
          variant="outline"
          size="sm"
          disabled={busy}
          data-testid="composio-skip-verify"
          onClick={onSkip}
        >
          Add anyway
        </Button>
      )}
      <Button
        variant="ghost"
        size="icon"
        aria-label="Dismiss"
        data-testid={`${testIdPrefix}-dismiss`}
        onClick={onDismiss}
      >
        <X className="size-4" />
      </Button>
    </div>
  );
}
