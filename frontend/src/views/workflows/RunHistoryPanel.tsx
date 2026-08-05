// The run-history drawer and the last-run chip (issue #228, extended by #371
// and #383).
//
// Extracted from `WorkflowsView.tsx` (issue #303). The tone/count helpers moved
// further out again, to `run-health.ts`, because the workflow cards need the
// same reading — see that file's header.

import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import type {
  DeliveryReport,
  DeliveryStatus,
  WorkflowRunNode,
  WorkflowRunOutcome,
} from "@/api/workflows";

import { failedNodeOf } from "./graph";
import { pendingCount, relativeTime, runTone, undeliveredCount } from "./run-health";

/** Badge styling per delivery outcome. A report that did NOT go out must not
 * look like one that did — `denied` and `failed` are the two an operator has to
 * act on, so they get the loud treatment. `pending` is neither: the report is
 * waiting in Approvals, so it reads as informational, not as a failure. */
const DELIVERY_TONE: Record<DeliveryStatus, string> = {
  sent: "border-emerald-500/40 bg-emerald-500/10",
  pending: "border-sky-500/40 bg-sky-500/10",
  skipped: "border-amber-500/40 bg-amber-500/10",
  denied: "border-red-500/40 bg-red-500/10",
  failed: "border-red-500/40 bg-red-500/10",
};

/** The delivery block of the run drawer: one line per attempt to route an
 * output node's report. This is the ONLY place an operator learns a report
 * didn't leave the building — a delivery failure never fails the run. */
export function DeliveryRows({ deliveries }: { deliveries: DeliveryReport[] }) {
  // Two counters, not one. A parked report is waiting on the operator, not
  // broken — badging it red alongside a transport failure would send them
  // hunting for a bug when the fix is a click in Approvals.
  const pending = deliveries.filter((d) => d.status === "pending").length;
  const undelivered = deliveries.filter(
    (d) => d.status !== "sent" && d.status !== "pending",
  ).length;
  return (
    <div className="mb-3 space-y-1.5 rounded-lg border bg-background/40 p-2">
      <div className="flex items-center gap-2">
        <span className="text-xs font-medium">Report delivery</span>
        {pending > 0 && (
          <Badge variant="outline" className="h-4 px-1.5 text-[10px] font-normal border-sky-500/40 bg-sky-500/10">
            {pending} awaiting approval
          </Badge>
        )}
        {undelivered > 0 && (
          <Badge variant="outline" className="h-4 px-1.5 text-[10px] font-normal border-red-500/40 bg-red-500/10">
            {undelivered} not delivered
          </Badge>
        )}
      </div>
      {deliveries.map((d, i) => (
        <div key={`${d.node}-${d.target ?? ""}-${i}`} className="flex flex-wrap items-baseline gap-1.5">
          <Badge
            variant="outline"
            className={`h-4 px-1.5 text-[10px] font-normal ${DELIVERY_TONE[d.status] ?? ""}`}
          >
            {d.status}
          </Badge>
          <span className="font-mono text-[11px]">{d.node}</span>
          <span className="text-[11px] text-muted-foreground">
            → {d.kind}
            {d.target ? ` ${d.target}` : ""} — {d.detail}
          </span>
        </div>
      ))}
    </div>
  );
}

/** The last-run chip beside the workflow title: a status dot, the undelivered
 * count when there is one, and how long ago it ran. This is the at-a-glance
 * answer to "did last night's scheduled run actually deliver?" — the question
 * that had no answer at all before issue #228. */
export function LastRunChip({ run }: { run: WorkflowRunOutcome }) {
  const tone = runTone(run);
  const undelivered = undeliveredCount(run.deliveries);
  const pending = pendingCount(run.deliveries);
  return (
    <Badge
      variant="outline"
      className="h-5 gap-1.5 px-2 text-[10px] font-normal"
      data-testid="workflow-last-run-chip"
      title={
        run.error
          ? `Last run failed: ${run.error}`
          : run.cancelled
            ? "An operator stopped this run before it finished."
            : `Last ${run.scheduled ? "scheduled" : "manual"} run — ${tone.label}`
      }
    >
      <span className={`size-1.5 rounded-full ${tone.dot}`} />
      {run.scheduled ? "Scheduled" : "Manual"} run
      {run.error
        ? " failed"
        : run.cancelled
          ? " stopped"
          : undelivered > 0
          ? ` · ${undelivered} not delivered`
          : pending > 0
            ? ` · ${pending} awaiting approval`
            : ""}
      <span className="text-muted-foreground">· {relativeTime(run.atMillis)}</span>
    </Badge>
  );
}

/** The run-history drawer: one row per finished run of the selected workflow,
 * newest first, each expanding to the very same {@link DeliveryRows} block the
 * live run drawer shows.
 *
 * This is the durable half of issue #228. A manual run's delivery rows used to
 * live only in the run drawer until it was dismissed, and a scheduled run's only
 * on the host's stdout — which on a hosted tenant is the platform team, not the
 * operator. These rows come back from the company's journal, so they survive a
 * console reload and a run nobody was watching. */
export function RunHistoryPanel({
  runs,
  workflowName,
  onClose,
  selectedRunSeq,
  onSelectRun,
}: {
  runs: WorkflowRunOutcome[];
  workflowName: string;
  onClose: () => void;
  /** The run currently overlaid on the canvas, if any (issue #371). */
  selectedRunSeq: number | null;
  /** Overlay this run's per-node states on the canvas (issue #371). */
  onSelectRun: (run: WorkflowRunOutcome) => void;
}) {
  return (
    <div className="border-t bg-card/60" data-testid="workflow-run-history">
      <div className="flex items-center justify-between px-4 py-2">
        <div className="flex items-center gap-2">
          <span className="text-sm font-medium">Run history</span>
          {workflowName && (
            <span className="truncate text-xs text-muted-foreground">{workflowName}</span>
          )}
          <Badge variant="secondary">{runs.length}</Badge>
        </div>
        <Button variant="ghost" size="sm" onClick={onClose}>
          Dismiss
        </Button>
      </div>
      <div className="max-h-72 overflow-auto px-4 pb-3">
        {runs.length === 0 ? (
          <p className="text-xs text-muted-foreground">
            This workflow hasn't finished a run yet. Runs appear here once they
            do — including scheduled ones that run while you're away.
          </p>
        ) : (
          <div className="space-y-2">
            {runs.map((run) => (
              <RunHistoryRow
                key={run.seq}
                run={run}
                selected={run.seq === selectedRunSeq}
                onSelect={() => onSelectRun(run)}
              />
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

/** One finished run: a summary line, its per-node trail, and its delivery rows.
 *
 * Clicking it overlays that run's node states on the canvas (issue #371) —
 * which is what makes a scheduled run's failure point visible, the case the
 * live canvas by definition cannot cover because nobody was watching. */
function RunHistoryRow({
  run,
  selected,
  onSelect,
}: {
  run: WorkflowRunOutcome;
  selected: boolean;
  onSelect: () => void;
}) {
  const tone = runTone(run);
  const nodes = run.nodes ?? [];
  const failedNode = failedNodeOf(run);
  return (
    <div
      className={`rounded-lg border bg-background/40 p-2 ${
        selected ? "ring-2 ring-primary/40" : ""
      }`}
      data-testid="workflow-run-row"
    >
      <div className="mb-1 flex flex-wrap items-center gap-2">
        <span className={`size-1.5 rounded-full ${tone.dot}`} />
        <Badge variant="outline" className="h-4 px-1.5 text-[10px] font-normal">
          {run.scheduled ? "scheduled" : "manual"}
        </Badge>
        <span className="text-[11px] text-muted-foreground">
          {new Date(run.atMillis).toLocaleString()} · {relativeTime(run.atMillis)}
        </span>
        {run.pendingApprovals.length > 0 && (
          <Badge
            variant="outline"
            className="h-4 px-1.5 text-[10px] font-normal border-amber-500/40 bg-amber-500/10"
          >
            {run.pendingApprovals.length} pending approval
            {run.pendingApprovals.length === 1 ? "" : "s"}
          </Badge>
        )}
        {run.running && (
          <Badge
            variant="outline"
            className="h-4 px-1.5 text-[10px] font-normal border-sky-500/40 bg-sky-500/10"
          >
            running
          </Badge>
        )}
        {nodes.length > 0 && (
          <Button
            size="sm"
            variant="ghost"
            className="ml-auto h-5 px-2 text-[10px]"
            onClick={onSelect}
            aria-pressed={selected}
            data-testid="workflow-run-overlay-toggle"
          >
            {selected ? "Hide on canvas" : "Show on canvas"}
          </Button>
        )}
      </div>

      {/* Issue #371: the per-node trail, which is what turns "it failed" into
          "it failed HERE". Absent for a run journaled before #371 — those rows
          render exactly as they always did. */}
      {nodes.length > 0 && (
        <div className="mb-1 flex flex-wrap gap-1" data-testid="workflow-run-nodes">
          {nodes.map((node) => (
            <RunNodeChip key={`${node.nodeId}-${node.elapsedMs}`} node={node} />
          ))}
        </div>
      )}
      {run.error ? (
        // The outcome that used to be quietest of all: a run that died left one
        // host-stdout warning and nothing an operator could ever find.
        <Alert variant="destructive" className="py-2">
          <AlertDescription className="text-[11px]">
            {/* Name the node when the trail names one — the engine reports a
                failing node as an errored step, so this is exact. When it does
                not (a graph that would not compile, a capability that could not
                be built), say nothing about nodes rather than guessing. */}
            {failedNode ? `This run failed at “${failedNode}”: ` : "This run failed: "}
            {run.error}
          </AlertDescription>
        </Alert>
      ) : run.cancelled ? (
        // Issue #383, the third terminal reading. Deliberately not a
        // destructive Alert: nothing went wrong, somebody decided they had seen
        // enough. It says "stopped", not "finished", because the node that was
        // executing was dropped where it was rather than allowed to complete —
        // so a side effect it had started may be half-done.
        <p
          className="text-[11px] text-muted-foreground"
          data-testid="workflow-run-cancelled"
        >
          An operator stopped this run
          {nodes.length > 0
            ? ` after ${nodes.length} step${nodes.length === 1 ? "" : "s"}`
            : " before any step finished"}
          . The steps above completed; the one still running was stopped where it
          was. Any approvals it had already raised are still waiting for you.
        </p>
      ) : run.deliveries.length > 0 ? (
        // Deliberately the SAME component the live run drawer uses, so a report
        // reads identically whether it's on screen now or a week old.
        <DeliveryRows deliveries={run.deliveries} />
      ) : (
        <p className="text-[11px] text-muted-foreground">
          Finished — this run routed no reports.
        </p>
      )}
    </div>
  );
}

/** One node's outcome in a history row: its id, how it went, how long it took. */
function RunNodeChip({ node }: { node: WorkflowRunNode }) {
  const ok = node.status === "ok";
  return (
    <span
      className={`inline-flex items-center gap-1 rounded border px-1.5 py-0.5 text-[10px] ${
        ok
          ? "border-emerald-500/40 bg-emerald-500/10"
          : "border-red-500/50 bg-red-500/10"
      }`}
    >
      <span className={`size-1.5 rounded-full ${ok ? "bg-emerald-500" : "bg-red-500"}`} />
      <span className="font-medium">{node.nodeId}</span>
      <span className="font-mono opacity-70">
        {node.elapsedMs < 1000 ? `${node.elapsedMs}ms` : `${(node.elapsedMs / 1000).toFixed(1)}s`}
      </span>
    </span>
  );
}
