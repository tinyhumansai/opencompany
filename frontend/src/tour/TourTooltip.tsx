import type { ReactNode } from "react";
import type { TooltipRenderProps } from "react-joyride";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

/**
 * The tour's spotlight card — a design-system-native replacement for
 * react-joyride's default tooltip. Everything is driven by theme tokens
 * (`popover`, `primary`, `muted`, `border`), so it reads correctly in both
 * light and dark. Shows a step chip, the copy, a segmented progress rail, and
 * Skip / Back / Next controls.
 */
export function TourTooltip({
  index,
  size,
  step,
  backProps,
  primaryProps,
  skipProps,
  tooltipProps,
  isLastStep,
}: TooltipRenderProps) {
  return (
    <div
      {...tooltipProps}
      className="w-[340px] max-w-[calc(100vw-2rem)] overflow-hidden rounded-2xl border border-border bg-popover text-popover-foreground shadow-xl ring-1 ring-black/5 dark:ring-white/10"
    >
      <div className="px-4 pt-4">
        <span className="inline-flex items-center rounded-full bg-primary/10 px-2 py-0.5 text-[11px] font-medium tracking-wide text-primary tabular-nums">
          Step {index + 1} of {size}
        </span>
        <h3 className="mt-2.5 font-heading text-[15px] leading-tight font-semibold">
          {step.title as ReactNode}
        </h3>
        <p className="mt-1.5 text-[13px] leading-relaxed text-muted-foreground">
          {step.content as ReactNode}
        </p>
      </div>

      {/* Segmented progress rail — one bar per step, filled up to the current. */}
      <div className="flex gap-1 px-4 pt-3.5">
        {Array.from({ length: size }).map((_, i) => (
          <span
            key={i}
            className={cn(
              "h-1 flex-1 rounded-full transition-colors",
              i <= index ? "bg-primary" : "bg-muted",
            )}
          />
        ))}
      </div>

      <div className="flex items-center justify-between gap-2 p-4 pt-3.5">
        <Button variant="ghost" size="sm" className="text-muted-foreground" {...skipProps}>
          Skip tour
        </Button>
        <div className="flex items-center gap-2">
          {index > 0 && (
            <Button variant="outline" size="sm" {...backProps}>
              Back
            </Button>
          )}
          <Button size="sm" {...primaryProps}>
            {isLastStep ? "Done" : "Next"}
          </Button>
        </div>
      </div>
    </div>
  );
}
