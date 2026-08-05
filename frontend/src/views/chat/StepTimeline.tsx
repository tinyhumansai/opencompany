import { useState } from "react";
import {
  AlertTriangle,
  Brain,
  ChevronDown,
  ChevronRight,
  Hourglass,
  Scissors,
  SquareKanban,
  Wrench,
  type LucideIcon,
} from "lucide-react";

import {
  AWAITING_APPROVAL_LABEL,
  STEP_FAILURE_LABEL,
  isFailedStep,
  type TurnStep,
  type TurnStepKind,
} from "@/api/types";
import { cn } from "@/lib/utils";

/**
 * The scrubbed processing steps behind a company reply, rendered above its
 * bubble. Collapsed by default to a one-line "N steps · M failed" summary;
 * auto-expands when any step failed so a silent MCP failure is visible, not
 * buried. Renders nothing when there are no steps (a memory-served / tool-less
 * reply). Ported from the retired Conversation page (issue #246) so the chat
 * workspace keeps the same tool-call visibility it had.
 *
 * `defaultOpen` is for the *live* timeline of a turn still running (issue
 * #367): there the rows are the content — they are what says the company is
 * working and on what — so they start open rather than behind a count. A
 * finished reply's steps stay collapsed, where they are supporting detail.
 * Either way the operator's own toggle wins from the first click.
 */
export function StepTimeline({
  steps,
  defaultOpen = false,
}: {
  steps: TurnStep[];
  defaultOpen?: boolean;
}) {
  const failed = steps.filter((s) => isFailedStep(s.status)).length;
  // A parked step is counted and surfaced separately — it is not a failure, and
  // it is the most actionable thing in the list, so it must not hide behind a
  // collapsed summary either (#411).
  const parked = steps.filter((s) => s.status === "awaiting_approval").length;
  const hasError = failed > 0;
  const [open, setOpen] = useState(defaultOpen || hasError || parked > 0);

  if (steps.length === 0) return null;

  return (
    <div className="mt-1 w-full max-w-[85%] sm:max-w-[75%]">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
        className={cn(
          "flex items-center gap-1 rounded-md px-1.5 py-0.5 text-[11px] font-medium transition-colors hover:bg-accent/60",
          hasError
            ? "text-destructive"
            : parked > 0
              ? "text-amber-600 dark:text-amber-400"
              : "text-muted-foreground",
        )}
      >
        {open ? <ChevronDown className="size-3" /> : <ChevronRight className="size-3" />}
        <span>
          {steps.length} step{steps.length === 1 ? "" : "s"}
          {failed > 0 && ` · ${failed} failed`}
          {parked > 0 && ` · ${parked} awaiting approval`}
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
  const error = isFailedStep(step.status);
  const parked = step.status === "awaiting_approval";
  const Icon = parked ? Hourglass : stepIcon(step.kind);
  return (
    <li
      className={cn(
        "flex flex-col gap-0.5 text-[11px] leading-relaxed",
        error
          ? "text-destructive"
          : parked
            ? "text-amber-600 dark:text-amber-400"
            : "text-muted-foreground",
      )}
    >
      <div className="flex items-center gap-1.5">
        <Icon className={cn("size-3 shrink-0", step.status === "running" && "animate-pulse")} />
        <span className={cn("font-medium", !error && !parked && "text-foreground/80")}>
          {step.label}
        </span>
        {/* The typed state, rendered by lookup — never by reading `result`. */}
        {parked && <StepChip tone="amber">{AWAITING_APPROVAL_LABEL}</StepChip>}
        {step.failure && <StepChip tone="rose">{STEP_FAILURE_LABEL[step.failure]}</StepChip>}
        {step.truncated && (
          <StepChip tone="amber">
            <Scissors className="size-2.5 shrink-0" aria-hidden />
            Result cut
          </StepChip>
        )}
        {step.detail && <span className="min-w-0 truncate">— {step.detail}</span>}
        <span className="ml-auto shrink-0 tabular-nums opacity-70">
          {formatElapsed(step.elapsedMs, step.status)}
        </span>
      </div>
      {/* What came back, on its own line: it is the answer to "how far did we
          get", and inlining it would push the arguments off the row. */}
      {step.result && (
        <span className="min-w-0 truncate pl-[18px] opacity-80">{step.result}</span>
      )}
    </li>
  );
}

function StepChip({
  tone,
  children,
}: {
  tone: "amber" | "rose";
  children: React.ReactNode;
}) {
  return (
    <span
      className={cn(
        "flex shrink-0 items-center gap-1 rounded px-1 py-px text-[10px] font-medium",
        tone === "amber"
          ? "bg-amber-500/15 text-amber-700 dark:text-amber-400"
          : "bg-rose-500/15 text-rose-700 dark:text-rose-400",
      )}
    >
      {children}
    </span>
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

/**
 * A step's duration.
 *
 * A gated call never ran, so it reports `0ms` — which read identically to a
 * fast success and was one of the things #411 called out. Say "didn't run"
 * instead: a duration of zero on a step that never left the process is not a
 * measurement, it is the absence of one.
 */
function formatElapsed(ms: number | undefined, status: TurnStep["status"]): string {
  if (status === "awaiting_approval") return "didn't run";
  if (typeof ms !== "number") return "";
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
