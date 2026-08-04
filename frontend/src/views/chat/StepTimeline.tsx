import { useState } from "react";
import { AlertTriangle, Brain, ChevronDown, ChevronRight, SquareKanban, Wrench, type LucideIcon } from "lucide-react";

import type { TurnStep, TurnStepKind } from "@/api/types";
import { cn } from "@/lib/utils";

/**
 * The scrubbed processing steps behind a company reply, rendered above its
 * bubble. Collapsed by default to a one-line "N steps · M failed" summary;
 * auto-expands when any step failed so a silent MCP failure is visible, not
 * buried. Renders nothing when there are no steps (a memory-served / tool-less
 * reply). Ported from the retired Conversation page (issue #246) so the chat
 * workspace keeps the same tool-call visibility it had.
 */
export function StepTimeline({ steps }: { steps: TurnStep[] }) {
  const failed = steps.filter((s) => s.status === "error").length;
  const hasError = failed > 0;
  const [open, setOpen] = useState(hasError);

  if (steps.length === 0) return null;

  return (
    <div className="mt-1 w-full max-w-[85%] sm:max-w-[75%]">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
        className={cn(
          "flex items-center gap-1 rounded-md px-1.5 py-0.5 text-[11px] font-medium transition-colors hover:bg-accent/60",
          hasError ? "text-destructive" : "text-muted-foreground",
        )}
      >
        {open ? <ChevronDown className="size-3" /> : <ChevronRight className="size-3" />}
        <span>
          {steps.length} step{steps.length === 1 ? "" : "s"}
          {failed > 0 && ` · ${failed} failed`}
        </span>
      </button>
      {open && (
        <ol className="mt-0.5 flex flex-col gap-1 rounded-lg border bg-card/60 px-2.5 py-1.5">
          {steps.map((step, i) => (
            <StepRow key={i} step={step} />
          ))}
        </ol>
      )}
    </div>
  );
}

function StepRow({ step }: { step: TurnStep }) {
  const error = step.status === "error";
  const Icon = stepIcon(step.kind);
  return (
    <li
      className={cn(
        "flex items-center gap-1.5 text-[11px] leading-relaxed",
        error ? "text-destructive" : "text-muted-foreground",
      )}
    >
      <Icon className={cn("size-3 shrink-0", step.status === "running" && "animate-pulse")} />
      <span className={cn("font-medium", !error && "text-foreground/80")}>{step.label}</span>
      {step.detail && <span className="min-w-0 truncate">— {step.detail}</span>}
      {typeof step.elapsedMs === "number" && (
        <span className="ml-auto shrink-0 tabular-nums opacity-70">{formatElapsed(step.elapsedMs)}</span>
      )}
    </li>
  );
}

function stepIcon(kind: TurnStepKind): LucideIcon {
  switch (kind) {
    case "tool_call":
      return Wrench;
    case "thinking":
      return Brain;
    case "note":
      return AlertTriangle;
    default:
      return Wrench;
  }
}

function formatElapsed(ms: number): string {
  return ms < 1000 ? `${ms}ms` : `${(ms / 1000).toFixed(1)}s`;
}

/**
 * The "a card opened from this reply" chip (issue #246) — links straight to
 * the board card a turn opened, or the one it dispatched to.
 */
export function CardChip({ taskId }: { taskId: string }) {
  return (
    <a
      href={`#/tasks/${encodeURIComponent(taskId)}`}
      className="mt-1.5 flex w-fit items-center gap-1 rounded-full bg-accent px-2 py-0.5 text-[11px] font-medium text-accent-foreground transition-opacity hover:opacity-80"
    >
      <SquareKanban className="size-3 shrink-0" />
      Card opened
    </a>
  );
}
