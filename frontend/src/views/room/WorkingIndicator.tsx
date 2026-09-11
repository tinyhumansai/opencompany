import { useEffect, useState } from "react";

import type { TurnStep } from "@/api/types";
import { cn } from "@/lib/utils";

/**
 * What a teammate wears while a turn is running (issue #787).
 *
 * ## It says what is happening, not something amusing
 *
 * This follows OpenHuman's handling rather than the rotating-word style: the
 * line is **derived from the run's real state**. When a step is in flight it
 * shows that step's own label — the one the host authored and the tool timeline
 * beside it already renders — with a pulsing status mark. When there is nothing
 * specific to say it falls back to a plain "Working…".
 *
 * The label is never re-derived here. `TurnStep.label` is the host's answer,
 * and a second phrasing computed console-side is exactly the drift #264
 * forbids: two surfaces describing one step differently.
 *
 * ## Ambiguity is answered with less, not with a guess
 *
 * OpenHuman's `workerStatusFromEntry` returns `undefined` for the ambiguous
 * case *"so the card stays label-only rather than render a misleading state"*.
 * Same rule here: with no running step — a turn that has not reported one yet,
 * or one whose steps have all settled while the reply is still being written —
 * this shows the generic "Working…" rather than naming the last thing it saw.
 * A stale step name would read as current, which is worse than saying little.
 *
 * ## The assistive text does not follow the label
 *
 * The visible line changes as steps come and go. The screen-reader string is
 * stable, because a live region re-announcing every step transition is noise
 * where one "a reply is coming" is the whole message.
 *
 * ## Reduced motion
 *
 * `prefers-reduced-motion: reduce` stops the pulse. The mark and the label
 * still say a turn is running; they stop moving to say it.
 */
export function WorkingIndicator({
  srLabel,
  steps,
  queued,
  name,
  className,
}: {
  /** The stable assistive string, e.g. "Replying…". Never the step label. */
  srLabel: string;
  /**
   * The turn's steps so far, when this surface has them. The newest `running`
   * one names the line; absent or all-settled falls back to "Working…".
   */
  steps?: readonly TurnStep[];
  /**
   * The turn is accepted but has not started — its durable row is still
   * `pending`, meaning it is queued behind other turns on the per-company
   * serial lock rather than doing anything (issue #983).
   *
   * Worth its own state because the honest word is the only queue fix available
   * at this stage: the serial train is real, and a spinner that implies progress
   * while a turn waits its turn is the console telling the operator something
   * untrue. It cannot show a step label — a queued turn has produced no steps —
   * so this deliberately outranks `steps`.
   */
  queued?: boolean;
  /**
   * The teammate the host recorded as answering, already resolved to a display
   * name — never a raw id, on the same terms as the receipt's own rule.
   *
   * This is the reload leg's only name. A receipt names whoever the first live
   * frame named, but a receipt does not survive a reload, and the row that does
   * could previously say nothing but "Working…". Naming the seat is the
   * difference between "somebody is on this" and a spinner that could equally
   * mean the console has lost the turn.
   *
   * Not a guess, and so not the ambiguity the note above refuses: it is the
   * host's own recorded expectation for this run. It ranks BELOW a running
   * step, which is more specific and more current, and the turn's first frame
   * replaces it outright.
   */
  name?: string;
  className?: string;
}) {
  const reduced = usePrefersReducedMotion();
  const running = runningStepLabel(steps);
  const idle = name ? `${name} is working…` : GENERIC_LABEL;

  return (
    <span
      // Locatable from the E2E specs, which is how the reload leg proves the
      // row was re-armed from the open-turn read (issue #983). The visible label
      // is `aria-hidden` and its twin is `sr-only`, so matching on text alone
      // resolves to two nodes and trips strict mode.
      data-testid="working-indicator"
      data-queued={queued ? "true" : "false"}
      className={cn(
        "flex items-center gap-2 rounded-full bg-muted px-3 py-2 text-sm text-muted-foreground",
        className,
      )}
    >
      <span
        aria-hidden
        className={cn(
          "size-1.5 shrink-0 rounded-full",
          queued ? "bg-status-idle" : "bg-status-running",
          // A queued turn is not progressing, so the mark does not move: the
          // pulse is the "something is happening" signal and there is nothing
          // happening yet.
          !reduced && !queued && "animate-pulse",
        )}
      />
      {/* `aria-hidden`, because the stable label below is what should be read. */}
      <span aria-hidden className="truncate">
        {queued ? QUEUED_LABEL : (running ?? idle)}
      </span>
      {/* CodeRabbit: the visible line already names the teammate (`idle`,
          above) once a step settles; the sr-only twin was still falling back
          to the generic `srLabel` in that same case, so an AT user never got
          the identity a sighted reader saw. Mirrors the visible fallback
          order exactly — a running step still outranks the name here too, so
          this never announces "Amendments is working…" while the visible
          line (and the live step timeline beside it) is naming a step. */}
      <span className="sr-only">
        {queued ? QUEUED_LABEL : !running && name ? idle : srLabel}
      </span>
    </span>
  );
}

/** What the line says when no step is in flight. */
export const GENERIC_LABEL = "Working…";

/**
 * What the line says while the turn is accepted but has not started.
 *
 * "Queued" rather than "Working", because it is the truth: the turn is waiting
 * on the per-company serial lock. See the `queued` prop.
 */
export const QUEUED_LABEL = "Queued…";

/**
 * The label of the newest step still running, or `undefined`.
 *
 * Last rather than first: a turn's steps arrive in order, so the most recent
 * `running` entry is the one actually in flight. A settled step is never
 * named — see the ambiguity note on {@link WorkingIndicator}.
 */
export function runningStepLabel(steps?: readonly TurnStep[]): string | undefined {
  if (!steps?.length) return undefined;
  for (let i = steps.length - 1; i >= 0; i -= 1) {
    const step = steps[i];
    if (step.status === "running") {
      const label = step.label?.trim();
      return label ? label : undefined;
    }
  }
  return undefined;
}

/**
 * Whether the viewer asked for reduced motion, kept live.
 *
 * Reads `false` where `matchMedia` is unavailable (jsdom without a stub, an
 * older embedded webview): a pulse for a viewer who never asked for stillness
 * is the lesser failure.
 *
 * Subscribing has two spellings. `MediaQueryList` only became an `EventTarget`
 * in Safari 14; before that — and in the WebKitGTK builds of that vintage, both
 * of which Tauri v2's floor still admits — it carries the deprecated
 * `addListener` alone. Calling `addEventListener` there does not degrade, it
 * throws a `TypeError` out of this effect and takes the chat view down with it.
 * A moving dot is not worth that, so prefer the modern spelling and fall back.
 */
function usePrefersReducedMotion(): boolean {
  const [reduced, setReduced] = useState(false);
  useEffect(() => {
    const mql = window.matchMedia?.("(prefers-reduced-motion: reduce)");
    if (!mql) return;
    setReduced(mql.matches);
    const onChange = () => setReduced(mql.matches);
    if (typeof mql.addEventListener === "function") {
      mql.addEventListener("change", onChange);
      return () => mql.removeEventListener("change", onChange);
    }
    mql.addListener(onChange);
    return () => mql.removeListener(onChange);
  }, []);
  return reduced;
}
