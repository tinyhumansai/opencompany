import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { removalWarnings } from "./routing";
import type { RemovalImpact } from "./routing";

/** Which of the two removals is being confirmed. */
export type RemovalIntent = "key" | "provider";

/**
 * Confirming a removal, with what it costs said out loud.
 *
 * ## Why this is a dialog and not a second click
 *
 * Both of these actions are one menu item away from each other, both read as
 * "remove", and they are not the same thing: one is recoverable by retyping a
 * credential, the other deletes a record, its endpoint and every route that
 * named it. An operator who means the first and gets the second finds out when
 * a workload stops resolving.
 *
 * ## Why it names facts rather than asking "are you sure?"
 *
 * A confirmation that only asks for confidence is a speed bump. It tells the
 * operator nothing they did not already know, and the only thing it reliably
 * teaches is to click through. [`removalWarnings`](./routing.ts) names what is
 * actually about to change — the default marker, the workloads whose routes
 * reset, the fact that this was the last provider switched on — one sentence per
 * fact, so the one an operator cares about is visible at a glance rather than
 * buried in a paragraph.
 *
 * ## The softer option is offered, not implied
 *
 * Most of the time somebody removing a provider wants it to stop being used, and
 * **switching it off does that** while keeping the endpoint, the credential and
 * the routes. It is offered here, on the screen where the destructive choice is
 * being made, because that is the moment it is useful — a hint somewhere else on
 * the page is a hint nobody reads at the point of the decision.
 */
export function RemoveProviderDialog({
  intent,
  label,
  impact,
  busy,
  onCancel,
  onDisable,
  onConfirm,
}: {
  /** `null` when nothing is being confirmed — the dialog is closed. */
  intent: RemovalIntent | null;
  label: string;
  impact: RemovalImpact;
  busy: boolean;
  onCancel: () => void;
  /** Offered only where it is a real alternative — see the component doc. */
  onDisable?: () => void;
  onConfirm: () => void;
}) {
  if (!intent) return null;
  const lines = removalWarnings(intent, label, impact);

  return (
    <Dialog open onOpenChange={(next) => !next && onCancel()}>
      <DialogContent className="sm:max-w-md" data-testid="inference-remove-dialog">
        <DialogHeader>
          <DialogTitle>
            {intent === "key" ? `Remove ${label}'s key?` : `Remove ${label}?`}
          </DialogTitle>
          {/* The first line is always the distinction between the two, because
              it is the one an operator is most likely to have got wrong. */}
          <DialogDescription>{lines[0]}</DialogDescription>
        </DialogHeader>

        {lines.length > 1 && (
          <ul className="grid gap-1.5" data-testid="inference-remove-impact">
            {lines.slice(1).map((line) => (
              <li key={line} className="text-xs text-status-blocked-text">
                {line}
              </li>
            ))}
          </ul>
        )}

        <DialogFooter>
          <Button type="button" variant="outline" disabled={busy} onClick={onCancel}>
            Cancel
          </Button>
          {onDisable && (
            <Button
              type="button"
              variant="outline"
              disabled={busy}
              data-testid="inference-remove-disable-instead"
              onClick={onDisable}
            >
              Switch it off instead
            </Button>
          )}
          <Button
            type="button"
            variant="destructive"
            disabled={busy}
            data-testid="inference-remove-confirm"
            onClick={onConfirm}
          >
            {intent === "key" ? "Remove key" : "Remove provider"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
