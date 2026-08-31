import { Sparkles, X } from "lucide-react";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

/**
 * The week-1 "save your first workflow" nudge (issue #1845).
 *
 * Server-backed, not a `localStorage` flag like the product tour
 * (`tour/state.ts`) — the `NotificationStore` row
 * `LifecycleScheduler` files on the host IS the "should this show" answer, so
 * this component is a dumb renderer: the caller (`WorkflowsView`) decides
 * whether to mount it at all, from `GET …/notifications?kind=workflow_nudge`.
 *
 * Deliberately reuses the tour's card chrome (`tour/TourTooltip.tsx` —
 * `rounded-2xl`, `bg-popover`, a `bg-primary/10` accent chip) rather than this
 * view's `Alert` banners: this is an invitation to try the product, not a
 * warning about something wrong, and should not read like one.
 */
export function Week1NudgeBanner({
  onCreate,
  onDismiss,
  className,
}: {
  /** Opens the create-workflow dialog — the same action the toolbar's "New
   * workflow" button and the empty state's "Create a workflow" button
   * trigger. */
  onCreate: () => void;
  /** Marks the nudge read without creating anything, and hides the banner. */
  onDismiss: () => void;
  className?: string;
}) {
  return (
    <div
      className={cn(
        "flex flex-wrap items-center justify-between gap-3 rounded-2xl border border-border bg-popover px-4 py-3 text-popover-foreground shadow-sm ring-1 ring-black/5 dark:ring-white/10",
        className,
      )}
      data-testid="workflow-week1-nudge"
    >
      <div className="flex items-start gap-3">
        <span className="mt-0.5 inline-flex size-7 shrink-0 items-center justify-center rounded-full bg-primary/10 text-primary">
          <Sparkles className="size-3.5" />
        </span>
        <div>
          <p className="text-sm font-medium leading-tight">Save your first workflow</p>
          <p className="mt-0.5 text-xs leading-relaxed text-muted-foreground">
            A workflow is the thing this company actually runs, on a schedule or on
            demand. Describe one in plain words and the copilot drafts it for you.
          </p>
        </div>
      </div>
      <div className="flex shrink-0 items-center gap-2">
        <Button size="sm" onClick={onCreate} data-testid="workflow-week1-nudge-create">
          Create a workflow
        </Button>
        <Button
          size="sm"
          variant="ghost"
          className="text-muted-foreground"
          onClick={onDismiss}
          aria-label="Dismiss"
          data-testid="workflow-week1-nudge-dismiss"
        >
          <X className="size-4" />
        </Button>
      </div>
    </div>
  );
}
