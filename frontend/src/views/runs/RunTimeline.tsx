// The step timeline (issue #1573): the grouped, expandable render of a
// `TimelineEntry` list, lifted out of `TaskDetailView.tsx` so a teammate's run
// history renders the same rows a card's attempts do.
//
// **Moved, not rewritten.** The rows below are the ones the Task Detail screen
// has always drawn — the failure coalescing, the waiting bands, the outcome
// icons, the "called with"/"result" split. Two screens showing the same trace
// in two dialects would be worse than either dialect, and the second surface is
// the moment that stops being hypothetical.
//
// The one change the move required: the empty case is a prop rather than a
// hard-coded "dispatch this task from the board". That sentence is true of a
// card and false of a chat turn, and the shared renderer has no way to tell
// which it is holding.

import { useMemo, useState, type ReactElement, type ReactNode } from "react";
import {
  AlertCircle,
  Brain,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  Hourglass,
  Loader2,
  MessageSquare,
  Play,
  ShieldCheck,
  StickyNote,
  Wrench,
} from "lucide-react";

import type {
  StepStatus,
  TimelineEntry,
  TimelineKind,
} from "@/api/tasks";
import type { RunStatus } from "@/api/runs";
import { AWAITING_APPROVAL_LABEL, STEP_FAILURE_LABEL } from "@/api/types";
import { formatUsdCost } from "@/lib/cost";
import {
  formatDuration,
  timeOf,
  waitingBandHeight,
} from "@/lib/timeline-format";
import { Badge } from "@/components/ui/badge";
import { cn } from "@/lib/utils";

/**
 * The tone of a run's status chip. `waiting_approval` and `paused` share the
 * amber "parked" tone the waiting band already uses — they differ in *who*
 * unblocks them, not in whether the company is stuck.
 *
 * `declined` shares the neutral muted tone `cancelled` uses (issue #1809): a
 * by-design compiler refusal is terminal but is neither a failure nor a
 * success, so it must never take the red failure tone — nor the blue "running"
 * tone the default arm would otherwise hand a status it did not recognise.
 */
export function runStatusTone(status: RunStatus): string {
  switch (status) {
    case "succeeded":
      return "border-status-done/40 text-status-done-text";
    case "failed":
      return "border-status-failed/40 text-status-failed-text";
    case "cancelled":
    case "declined":
      return "border-muted-foreground/30 text-muted-foreground";
    case "waiting_approval":
    case "paused":
    case "blocked":
      return "border-status-blocked/40 text-status-blocked-text";
    default:
      return "border-status-running/40 text-status-running-text";
  }
}

/** One rendered timeline row — a single entry, or a run of grouped failures. */
interface TimelineGroup {
  key: string;
  kind: TimelineKind;
  /** A run step's outcome (#242); absent for journal-derived entries. */
  status?: StepStatus;
  label: string;
  count: number;
  entries: TimelineEntry[];
}

/**
 * One item in the rendered timeline: an event row, or a waiting band (#305).
 *
 * The band is not an event — nothing is journaled while the company waits — so
 * it cannot be a `TimelineGroup`. Making the list a union keeps the band's
 * variable height out of the row renderer entirely.
 */
type TimelineItem =
  | { row: "group"; key: string; group: TimelineGroup }
  | { row: "wait"; key: string; millis: number; live: boolean };

/**
 * Folds a timeline into rows, coalescing consecutive same-label `tool_failed`
 * entries into one `×N` row. Every other kind is its own row.
 *
 * Waiting bands (#305) are spliced in *before* the approval row that ended the
 * wait — the band is the pause that led to the decision, so it reads in that
 * order. Approvals are never coalesced, so no band can land inside a `×N`
 * group.
 *
 * Only *completed* waits appear here: the live wait on a task parked on an
 * operator right now is the task detail's `AwaitingApprovalRow`, not a band in
 * this timeline (issue #1354).
 */
export function groupTimeline(entries: TimelineEntry[]): TimelineItem[] {
  const groups: TimelineGroup[] = [];
  for (const e of entries) {
    const last = groups[groups.length - 1];
    if (
      isFailureRow(e) &&
      last &&
      last.kind === e.kind &&
      last.label === e.label &&
      isFailureRow(last.entries[last.entries.length - 1])
    ) {
      last.count += 1;
      last.entries.push(e);
    } else {
      groups.push({
        key: e.costKey ?? String(e.seq),
        kind: e.kind,
        status: e.status,
        label: e.label,
        count: 1,
        entries: [e],
      });
    }
  }

  const items: TimelineItem[] = [];
  for (const g of groups) {
    const waited =
      g.kind === "approval" ? g.entries[0].waitedMillis : undefined;
    if (waited !== undefined && waited > 0) {
      // `wait-` prefixed so a band can never collide with a row key, which is
      // the bare sequence number.
      items.push({
        row: "wait",
        key: `wait-${g.entries[0].seq}`,
        millis: waited,
        live: false,
      });
    }
    items.push({ row: "group", key: g.key, group: g });
  }
  return items;
}

const KIND_ICON: Record<TimelineKind, ReactElement> = {
  dispatched: <Play className="size-3.5" />,
  reply: <MessageSquare className="size-3.5" />,
  tool_failed: <AlertCircle className="size-3.5" />,
  approval: <ShieldCheck className="size-3.5" />,
  completed: <CheckCircle2 className="size-3.5" />,
  // The run-trace kinds (#242). Same renderer, three more icon rows.
  tool_call: <Wrench className="size-3.5" />,
  thinking: <Brain className="size-3.5" />,
  note: <StickyNote className="size-3.5" />,
};

/**
 * The icon for a row. A run step's **outcome** outranks its kind (#242): a
 * failed tool call reads as a failure, and one still in flight reads as a
 * spinner — which is the honest render of a step the trace recorded as
 * `running` because the host died mid-call, not an error.
 *
 * Task-timeline entries carry no `status`, so they fall through to the kind
 * icon exactly as before.
 */
function rowIcon(kind: TimelineKind, status?: StepStatus): ReactElement {
  if (status === "running")
    return <Loader2 className="size-3.5 animate-spin" />;
  // A parked step is waiting on a person, not broken — it takes the hourglass
  // the waiting band already uses, never the failure icon (#411).
  if (status === "awaiting_approval")
    return <Hourglass className="size-3.5" />;
  if (status === "error") return <AlertCircle className="size-3.5" />;
  return KIND_ICON[kind];
}

function kindTone(kind: TimelineKind, status?: StepStatus): string {
  // Outcome first, for the same reason `rowIcon` reads it first.
  if (status === "running") return "text-status-running-text";
  if (status === "awaiting_approval")
    return "text-status-blocked-text";
  if (status === "error") return "text-status-failed-text";
  switch (kind) {
    case "completed":
      return "text-status-done-text";
    case "tool_failed":
      return "text-status-failed-text";
    case "approval":
      return "text-status-blocked-text";
    default:
      return "text-muted-foreground";
  }
}

/**
 * Whether a row is a failure, from either surface: the journal's `tool_failed`
 * kind, or a run step whose recorded outcome was an error (#242). This is what
 * the `×N` coalescing keys on, so a tool that failed six times in a row reads
 * the same in a run's trace as it does on the task timeline.
 */
function isFailureRow(entry: TimelineEntry): boolean {
  return entry.kind === "tool_failed" || entry.status === "error";
}

/**
 * A trace, grouped into rows.
 *
 * `empty` is what to draw when there is nothing to draw. It is a prop because
 * the renderer genuinely cannot know: the same zero rows mean "dispatch this
 * card to start its timeline" on a board card and "steps appear here as the
 * attempt runs" on a live chat turn, and a shared default would be wrong on one
 * of them every time.
 */
export function TimelineList({
  entries,
  empty = null,
}: {
  entries: TimelineEntry[];
  empty?: ReactNode;
}) {
  const items = useMemo(() => groupTimeline(entries), [entries]);
  if (items.length === 0) return <>{empty}</>;
  return (
    <ol className="space-y-1.5">
      {items.map((item) =>
        item.row === "wait" ? (
          <WaitingBand key={item.key} millis={item.millis} live={item.live} />
        ) : (
          <TimelineRow key={item.key} group={item.group} />
        ),
      )}
    </ol>
  );
}

/**
 * A waiting period, rendered as space rather than as another uniform row (#305).
 *
 * This is the acceptance criterion the timeline exists for: a four-hour wait and
 * a four-second wait must not look alike. The height carries the comparison at a
 * glance; the printed duration carries the exact figure, including past the
 * point the height saturates.
 */
function WaitingBand({ millis, live }: { millis: number; live: boolean }) {
  // Height is quantised to the 4s poll while the band is live, so a 1s text tick
  // does not relayout the list underneath the reader's cursor every second.
  const height = waitingBandHeight(
    live ? Math.round(millis / 4000) * 4000 : millis,
  );
  return (
    <li
      className={cn(
        "flex items-center justify-center gap-1.5 rounded-lg border border-dashed",
        "border-status-blocked/40 bg-status-blocked-soft text-2xs text-status-blocked-text",
        live && "animate-pulse",
      )}
      style={{ minHeight: height }}
      aria-label={`Waiting on a human for ${formatDuration(millis)}`}
    >
      <Hourglass className="size-3.5 shrink-0" aria-hidden />
      <span className="font-medium tabular-nums">
        {live
          ? `Waiting on you · ${formatDuration(millis)}`
          : `Waited ${formatDuration(millis)}`}
      </span>
    </li>
  );
}

/**
 * A step's expanded body: **what it was doing** above **what came back** (#411).
 *
 * Labelled, because the two used to be one anonymous string and an operator had
 * no way to tell an argument from an answer. `detail` on a journal entry has no
 * second half, so it renders alone exactly as it did before.
 */
function StepBody({ entry }: { entry: TimelineEntry }) {
  return (
    <div className="space-y-1">
      {entry.detail && (
        <div>
          {entry.result && (
            <div className="text-3xs font-medium uppercase tracking-wide text-muted-foreground">
              Called with
            </div>
          )}
          <pre className="whitespace-pre-wrap break-words font-mono text-2xs text-muted-foreground">
            {entry.detail}
          </pre>
        </div>
      )}
      {entry.result && (
        <div>
          {entry.detail && (
            <div className="text-3xs font-medium uppercase tracking-wide text-muted-foreground">
              Result
            </div>
          )}
          <pre className="whitespace-pre-wrap break-words font-mono text-2xs text-muted-foreground">
            {entry.result}
            {entry.truncated && " (cut short)"}
          </pre>
        </div>
      )}
    </div>
  );
}

/** A small state chip on a timeline row. Mirrors the chat timeline's. */
function StepStateChip({
  tone,
  children,
}: {
  tone: "amber" | "rose";
  children: ReactNode;
}) {
  return (
    <span
      className={cn(
        "shrink-0 rounded px-1 py-px text-3xs font-medium",
        tone === "amber"
          ? "bg-status-blocked-soft text-status-blocked-text"
          : "bg-status-failed-soft text-status-failed-text",
      )}
    >
      {children}
    </span>
  );
}

function TimelineRow({ group }: { group: TimelineGroup }) {
  const [open, setOpen] = useState(false);
  // A step that only reports what came back is still worth expanding — "how far
  // did it get" lives in `result`, not just in `detail` (#411).
  const details = group.entries.filter((e) => e.detail || e.result);
  const expandable = details.length > 0 || group.count > 1;
  const first = group.entries[0];

  return (
    <li className="rounded-lg border bg-card">
      <button
        className={cn(
          "flex w-full flex-wrap items-center gap-2 px-3 py-2 text-left text-xs",
          expandable ? "cursor-pointer" : "cursor-default",
        )}
        disabled={!expandable}
        onClick={() => expandable && setOpen((o) => !o)}
      >
        <span className={cn("shrink-0", kindTone(group.kind, group.status))}>
          {rowIcon(group.kind, group.status)}
        </span>
        {/* A floor, not just `min-w-0`: a row carrying a state chip AND a
            duration would otherwise squeeze the label to "Workspa…", hiding
            the one thing that says which step this is. */}
        <span className="min-w-[7rem] flex-1 truncate font-medium">
          {group.label}
        </span>
        {/* The typed state a step reached, by lookup rather than by reading its
            prose (#411). Only on a single row — a ×N group's entries can differ
            and one chip would have to speak for all of them. */}
        {group.count === 1 && first.status === "awaiting_approval" && (
          <StepStateChip tone="amber">{AWAITING_APPROVAL_LABEL}</StepStateChip>
        )}
        {group.count === 1 && first.failure && (
          <StepStateChip tone="rose">
            {STEP_FAILURE_LABEL[first.failure]}
          </StepStateChip>
        )}
        {group.count === 1 && first.truncated && (
          <StepStateChip tone="amber">Result cut</StepStateChip>
        )}
        {group.count > 1 && (
          <Badge variant="outline" className="shrink-0 font-normal">
            ×{group.count}
          </Badge>
        )}
        {formatUsdCost(first.cost, "line") && (
          <span className="shrink-0 font-medium tabular-nums text-foreground">
            {formatUsdCost(first.cost, "line")}
          </span>
        )}
        {/* A run step's own duration (#242); journal entries carry none. A
            gated call never ran, so its 0ms is the absence of a measurement
            rather than a fast one (#411). */}
        {group.count === 1 && first.status === "awaiting_approval" ? (
          <span className="shrink-0 text-2xs text-muted-foreground">
            didn't run
          </span>
        ) : (
          group.count === 1 &&
          first.elapsedMs !== undefined && (
            <span className="shrink-0 text-2xs tabular-nums text-muted-foreground">
              {first.elapsedMs < 1000
                ? `${first.elapsedMs}ms`
                : formatDuration(first.elapsedMs)}
            </span>
          )
        )}
        <span className="shrink-0 text-2xs tabular-nums text-muted-foreground">
          {timeOf(first.atMillis)}
        </span>
        {expandable &&
          (open ? (
            <ChevronDown className="size-3.5 shrink-0 text-muted-foreground" />
          ) : (
            <ChevronRight className="size-3.5 shrink-0 text-muted-foreground" />
          ))}
      </button>
      {open && expandable && (
        <div className="space-y-2 border-t px-3 py-2">
          {group.count > 1
            ? group.entries.map((e) => (
                <div key={e.seq} className="text-2xs">
                  <div className="mb-0.5 text-muted-foreground">
                    {timeOf(e.atMillis)}
                  </div>
                  <StepBody entry={e} />
                </div>
              ))
            : details.map((e) => <StepBody key={e.seq} entry={e} />)}
        </div>
      )}
    </li>
  );
}
