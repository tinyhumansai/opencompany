// The run-history drawer and the last-run chip (issue #228, extended by #371
// and #383).
//
// Extracted from `WorkflowsView.tsx` (issue #303). The tone/count helpers moved
// further out again, to `run-health.ts`, because the workflow cards need the
// same reading — see that file's header.

import { useEffect, useState } from "react";

import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import type {
  DeliveryReport,
  DeliveryStatus,
  WorkflowGraph,
  WorkflowRunNode,
  WorkflowRunOutcome,
} from "@/api/workflows";

import { failedNodeOf, nodeName } from "./graph";
import {
  awaitingCount,
  decidableApprovalCount,
  formatDuration,
  isBlocked,
  isRunning,
  relativeTime,
  runDuration,
  runTone,
  undeliveredCount,
} from "./run-health";

/** Badge styling per delivery outcome. A report that did NOT go out must not
 * look like one that did — `denied` and `failed` are the two an operator has to
 * act on, so they get the loud treatment. `pending` is neither: the report is
 * waiting in Approvals, so it reads as informational, not as a failure. */
const DELIVERY_TONE: Record<DeliveryStatus, string> = {
  sent: "border-status-done/40 bg-status-done-soft",
  pending: "border-status-blocked/40 bg-status-blocked-soft",
  skipped: "border-status-blocked/40 bg-status-blocked-soft",
  denied: "border-status-failed/40 bg-status-failed-soft",
  failed: "border-status-failed/40 bg-status-failed-soft",
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
          <Badge
            variant="outline"
            className="h-4 px-1.5 text-3xs font-normal border-status-blocked/40 bg-status-blocked-soft"
          >
            {pending} awaiting approval
          </Badge>
        )}
        {undelivered > 0 && (
          <Badge
            variant="outline"
            className="h-4 px-1.5 text-3xs font-normal border-status-failed/40 bg-status-failed-soft"
          >
            {undelivered} not delivered
          </Badge>
        )}
      </div>
      {deliveries.map((d, i) => (
        <div
          key={`${d.node}-${d.target ?? ""}-${i}`}
          className="flex flex-wrap items-baseline gap-1.5"
        >
          <Badge
            variant="outline"
            className={`h-4 px-1.5 text-3xs font-normal ${DELIVERY_TONE[d.status] ?? ""}`}
          >
            {d.status}
          </Badge>
          <span className="font-mono text-2xs">{d.node}</span>
          <span className="text-2xs text-muted-foreground">
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
  // Issue #846: gates and parked reports together. The chip said "Manual run"
  // and a green dot for a run whose first node was still waiting on a person.
  const awaiting = awaitingCount(run);
  return (
    <Badge
      variant="outline"
      className="h-5 gap-1.5 px-2 text-3xs font-normal"
      data-testid="workflow-last-run-chip"
      title={
        run.running
          ? "This run is still going."
          : run.error
            ? `Last run failed: ${run.error}`
            : run.cancelled
              ? "An operator stopped this run before it finished."
              : `Last ${run.scheduled ? "scheduled" : "manual"} run — ${tone.label}`
      }
    >
      <span className={`size-1.5 rounded-full ${tone.dot}`} />
      {run.scheduled ? "Scheduled" : "Manual"} run
      {/* The in-flight case is worded before the terminal ones for the same
          reason `runTone` checks it first: a run that has not finished has not
          failed and has not succeeded, and the counts below are not final. */}
      {run.running
        ? " running"
        : run.error
          ? " failed"
          : run.cancelled
            ? " stopped"
            : undelivered > 0
              ? ` · ${undelivered} not delivered`
              : awaiting > 0
                ? ` · ${awaiting} awaiting approval`
                : ""}
      <span className="text-muted-foreground">
        · {relativeTime(run.atMillis)}
      </span>
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
  graph,
  workflowName,
  onClose,
  selectedRunSeq,
  onSelectRun,
  onFixWithCopilot,
  fixingRunSeq,
  fixReason,
}: {
  runs: WorkflowRunOutcome[];
  /**
   * The selected workflow's graph, for turning a node id into the name the
   * operator gave it (issue #1007). `null` while it loads or after a failed
   * read, which {@link nodeName} degrades to the raw id for.
   */
  graph: WorkflowGraph | null;
  workflowName: string;
  onClose: () => void;
  /** The run currently overlaid on the canvas, if any (issue #371). */
  selectedRunSeq: number | null;
  /** Overlay this run's per-node states on the canvas (issue #371). */
  onSelectRun: (run: WorkflowRunOutcome) => void;
  /**
   * Correct this failed run's workflow with the copilot (issue #840, PR-3). When
   * absent (no brain wired, or a host without the route) the affordance is not
   * offered at all.
   */
  onFixWithCopilot?: (run: WorkflowRunOutcome) => void;
  /** The run whose copilot fix is in flight, so its row shows a spinner. */
  fixingRunSeq?: number | null;
  /** A run the copilot judged un-fixable, shown inline under that run's row. */
  fixReason?: { seq: number; reason: string } | null;
}) {
  // Only one fix may be in flight at a time: `handleFixWithCopilot` sets a
  // single `prefilledDraft`/`editOpen` slot, so a second Fix started on a
  // different row of this same panel while the first is still running would
  // race it for that slot — whichever resolves last silently wins, which
  // could show the operator the wrong run's correction. Disabling every row's
  // button (not just the in-flight one's) while `fixingRunSeq` is set turns
  // that race into "wait your turn".
  const anyFixInFlight = fixingRunSeq != null;
  // Issue #1007: a clock, ticking only while a row is actually in flight. The
  // elapsed time on a running row is the console's acknowledgement that the
  // click did something, and it is only true if it moves.
  const now = useRunningClock(runs.some(isRunning));
  return (
    <div className="border-t bg-card/60" data-testid="workflow-run-history">
      <div className="flex items-center justify-between px-4 py-2">
        <div className="flex items-center gap-2">
          <span className="text-sm font-medium">Run history</span>
          {workflowName && (
            <span className="truncate text-xs text-muted-foreground">
              {workflowName}
            </span>
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
                graph={graph}
                now={now}
                selected={run.seq === selectedRunSeq}
                onSelect={() => onSelectRun(run)}
                onFixWithCopilot={onFixWithCopilot}
                fixing={fixingRunSeq === run.seq}
                fixDisabled={anyFixInFlight}
                fixReason={fixReason?.seq === run.seq ? fixReason.reason : null}
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
  graph,
  now,
  selected,
  onSelect,
  onFixWithCopilot,
  fixing,
  fixDisabled,
  fixReason,
}: {
  run: WorkflowRunOutcome;
  /** The selected workflow's graph, for node ids → names (issue #1007). */
  graph: WorkflowGraph | null;
  /** The clock a still-running row counts against (issue #1007). */
  now: number;
  selected: boolean;
  onSelect: () => void;
  /** Correct this run's workflow with the copilot (issue #840, PR-3). */
  onFixWithCopilot?: (run: WorkflowRunOutcome) => void;
  /** Whether this row's copilot fix is currently in flight. */
  fixing?: boolean;
  /** A DIFFERENT row's fix is in flight — disabled without the "Fixing…" label. */
  fixDisabled?: boolean;
  /** The copilot's reason this failure could not be fixed by re-wiring, if any. */
  fixReason?: string | null;
}) {
  const tone = runTone(run);
  const nodes = run.nodes ?? [];
  // Issue #881 / #880: read once, so the chip, the badge and the terminal line
  // below cannot disagree about whether this run stopped for a person.
  const blocked = run.blockedNodes ?? [];
  // Issue #900: `decidableApprovalCount`, not `parkedApprovalCount` — this
  // paragraph tells the operator a card is waiting, so it must count only
  // the receipts that actually landed one. Counting a failed park here said
  // "needs your approval" and "decide it in Approvals" about a call the very
  // next sentence admitted nobody would ever be asked about.
  const parked = decidableApprovalCount(run);
  // The loud half: calls nobody will ever be asked about, because the park
  // itself failed or the excess was dropped past the per-turn cap. Strictly
  // worse than a parked one — there is no card to click.
  const unparkable = blocked.reduce((n, b) => n + (b.unparkable ?? 0), 0);
  const failedNode = failedNodeOf(run);
  const duration = runDuration(run, now);
  return (
    <div
      className={`rounded-lg border bg-background/40 p-2 ${
        selected ? "ring-2 ring-primary/40" : ""
      }`}
      data-testid="workflow-run-row"
    >
      <div className="mb-1 flex flex-wrap items-center gap-2">
        <span className={`size-1.5 rounded-full ${tone.dot}`} />
        <Badge variant="outline" className="h-4 px-1.5 text-3xs font-normal">
          {run.scheduled ? "scheduled" : "manual"}
        </Badge>
        <span className="text-2xs text-muted-foreground">
          {new Date(run.atMillis).toLocaleString()} ·{" "}
          {relativeTime(run.atMillis)}
          {/* Issue #1007: how long it took, which nothing on this surface said.
              A run that failed in 200ms was refused before it started; one that
              failed after four minutes got somewhere first, and the two want
              different next moves. `null` on a row journaled before #371, whose
              only recorded time is its finish. */}
          {duration != null && (
            <span data-testid="workflow-run-duration">
              {" · "}
              {isRunning(run) ? "running for " : "took "}
              {formatDuration(duration)}
            </span>
          )}
        </span>
        {/* Issue #880: what the run PARKED, in those words. A blocked run's
            `pendingApprovals` names the nodes it stopped at, which is a
            different count from the cards it opened — and "pending" is the
            phrasing that goes stale, since nothing here is refreshed when the
            operator approves one. The receipt wins where there is one. */}
        {parked > 0 ? (
          <Badge
            variant="outline"
            className="h-4 px-1.5 text-3xs font-normal border-status-blocked/40 bg-status-blocked-soft"
            data-testid="workflow-run-parked"
          >
            parked {parked} approval{parked === 1 ? "" : "s"}
          </Badge>
        ) : (
          run.pendingApprovals.length > 0 && (
            <Badge
              variant="outline"
              className="h-4 px-1.5 text-3xs font-normal border-status-blocked/40 bg-status-blocked-soft"
            >
              {run.pendingApprovals.length} pending approval
              {run.pendingApprovals.length === 1 ? "" : "s"}
            </Badge>
          )
        )}
        {run.running && (
          <Badge
            variant="outline"
            className="h-4 px-1.5 text-3xs font-normal border-status-running/40 bg-status-running-soft"
          >
            running
          </Badge>
        )}
        {nodes.length > 0 && (
          <Button
            size="sm"
            variant="ghost"
            className="ml-auto h-5 px-2 text-3xs"
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
        <div
          className="mb-1 flex flex-wrap gap-1"
          data-testid="workflow-run-nodes"
        >
          {nodes.map((node) => (
            <RunNodeChip key={`${node.nodeId}-${node.elapsedMs}`} node={node} />
          ))}
        </div>
      )}
      {run.error ? (
        // The outcome that used to be quietest of all: a run that died left one
        // host-stdout warning and nothing an operator could ever find.
        <Alert variant="destructive" className="py-2">
          <AlertDescription className="text-2xs">
            {/* Name the node when the trail names one — the engine reports a
                failing node as an errored step, so this is exact. When it does
                not (a graph that would not compile, a capability that could not
                be built), say nothing about nodes rather than guessing.

                Issue #1007: the NAME the operator gave the node, not its raw
                id. The engine's trail is keyed by id, so this line named `n_3`
                while the run drawer's timeline, the canvas and the overlay
                banner all named "Draft the digest" for the same step.
                `nodeName` falls back to the id when the graph is not loaded,
                and a graph edited since the run can only give back the id it
                no longer holds — both of which are the old reading, never a
                wrong name. */}
            {failedNode
              ? `This run failed at “${nodeName(graph, failedNode)}”: `
              : "This run failed: "}
            {run.error}
            {/* Issue #840 (PR-3): correct the workflow with the copilot. Offered
                only on the journaled failed run (keyed by runId) — the one
                surface that always carries the failure, unlike the sync run
                result — and only when the parent wired a handler (a brain is
                available). */}
            {onFixWithCopilot && run.runId && (
              <div className="mt-1.5">
                <Button
                  size="sm"
                  variant="outline"
                  className="h-6 px-2 text-3xs"
                  disabled={fixing || fixDisabled}
                  onClick={() => onFixWithCopilot(run)}
                  data-testid="workflow-run-fix-with-copilot"
                >
                  {fixing ? "Fixing…" : "Fix with copilot"}
                </Button>
                {fixReason && (
                  <p
                    className="mt-1 text-2xs text-muted-foreground"
                    data-testid="workflow-run-fix-not-automatable"
                  >
                    The copilot couldn't fix this by re-wiring the workflow: {fixReason}
                  </p>
                )}
              </div>
            )}
          </AlertDescription>
        </Alert>
      ) : run.cancelled ? (
        // Issue #383, the third terminal reading. Deliberately not a
        // destructive Alert: nothing went wrong, somebody decided they had seen
        // enough. It says "stopped", not "finished", because the node that was
        // executing was dropped where it was rather than allowed to complete —
        // so a side effect it had started may be half-done.
        <p
          className="text-2xs text-muted-foreground"
          data-testid="workflow-run-cancelled"
        >
          An operator stopped this run
          {nodes.length > 0
            ? ` after ${nodes.length} step${nodes.length === 1 ? "" : "s"}`
            : " before any step finished"}
          . The steps above completed; the one still running was stopped where
          it was. Any approvals it had already raised are still waiting for you.
        </p>
      ) : isBlocked(run) ? (
        // Issue #881, the fourth terminal reading — and the one that had NO
        // arm at all, which is how a run that delivered nothing came to read as
        // a clean success. A blocked run carries no error, is not cancelled,
        // is not running, and routed no report, so it fell straight through to
        // the "Finished — this run routed no reports" line below. That sentence
        // is what lied.
        //
        // Deliberately not a destructive Alert: nothing broke. Same amber the
        // gated-call notice already uses — "needs your attention, nothing is
        // wrong" — and the same 11px rung as its siblings.
        //
        // Wording is the review item here. "Parked N approvals", never "waiting
        // on N": nothing refreshes this row when the operator approves one, so
        // an outstanding count is stale on arrival, while a record of what the
        // run parked stays true. And it says plainly that approving does not
        // continue THIS run — an agent step is not resumable, so the operator
        // has to run the workflow again or they will sit waiting for a
        // continuation that never comes.
        <p
          className="text-2xs text-[var(--status-blocked-text)]"
          data-testid="workflow-run-blocked"
        >
          {/* Issue #900: the verb used to be unconditionally "needs your
              approval", even when every one of the blocked node's calls was
              unparkable — a call nobody will ever be asked about. That read
              as a promise of a card that does not exist, and contradicted the
              closing sentence below whenever `parked` was 0. */}
          Not finished — {blocked.map((b) => `“${b.nodeId}”`).join(", ")}{" "}
          {parked > 0
            ? blocked.length === 1
              ? "needs your approval"
              : "need your approval"
            : "could not be queued for approval"}, so{" "}
          {blocked.length === 1 ? "it" : "they"} produced nothing and the steps
          after {blocked.length === 1 ? "it" : "them"} did not run.{" "}
          {parked > 0 &&
            `This run parked ${parked} approval${parked === 1 ? "" : "s"}. `}
          {unparkable > 0 &&
            `${unparkable} call${unparkable === 1 ? "" : "s"} could not be queued for approval at all, so you will not be asked about ${unparkable === 1 ? "it" : "them"}. `}
          {parked > 0
            ? `Decide ${parked === 1 ? "it" : "them"} in Approvals, then run the workflow again — approving does not continue this run.`
            : "Nothing here can be approved; change the policy and run the workflow again."}
        </p>
      ) : run.running ? (
        // Same root cause as the tone bug: a run still walking its graph has no
        // error, no cancellation and no deliveries yet, so it fell through to
        // the "Finished" line below and told the operator it was over. It is
        // not, and its reports have not been routed yet.
        <p className="text-2xs text-muted-foreground">
          Still running — reports are routed when it finishes.
        </p>
      ) : run.pendingApprovals.length > 0 ? (
        <>
          {/* A paused run can still have routed reports — the output nodes it
              reached BEFORE the gate. Those rows are shown as they always were,
              with the waiting line above rather than instead of them: replacing
              them would trade one silent omission for another. */}
          {run.deliveries.length > 0 && (
            <DeliveryRows deliveries={run.deliveries} />
          )}
          {/* Issue #846. This is the arm that was missing, and its absence is
              how a run waiting on a human came to report success: a paused run
              has no error, no cancellation, is not `running` (the engine
              settled it) and routed nothing, so it fell through every branch
              to the "Finished" line below — while its gate sat undecided on
              the Approvals page.

              "Not finished" is the claim, stated in the operator's terms
              rather than the engine's. The run object really is settled;
              what has not happened is the work, and the work is what the
              operator is asking about. Naming the nodes matters as much as
              the state: a scheduled run that silently did nothing is exactly
              the failure this reads as, and the fix is a click, so the row
              says which click. */}
          <p
            className="text-2xs text-[var(--status-blocked-text)]"
            data-testid="workflow-run-awaiting"
          >
            Not finished — waiting for your approval on{" "}
            {run.pendingApprovals.map((node) => `“${node}”`).join(", ")}.
            Nothing past {run.pendingApprovals.length === 1 ? "it" : "them"} has
            run
            {run.deliveries.length === 0 ? ", and no reports were routed" : ""}.
            Approve or decline it in Approvals to carry the run on.
          </p>
        </>
      ) : run.deliveries.length > 0 ? (
        // Deliberately the SAME component the live run drawer uses, so a report
        // reads identically whether it's on screen now or a week old.
        <DeliveryRows deliveries={run.deliveries} />
      ) : (
        <p className="text-2xs text-muted-foreground">
          Finished — this run routed no reports.
        </p>
      )}
      {/* Issue #638. Rendered ALONGSIDE the outcome above rather than as one
          more branch of it, because a notice is not a terminal state — a run
          can succeed, be stopped, or fail and still have discarded gated calls
          the operator needs to know about. Folding it into the chain would have
          made it invisible for every outcome except the one branch it sat in.

          Deliberately not a destructive Alert: nothing failed. It is the same
          tone as the cancelled line — something happened that you need to know,
          not something that went wrong.

          Coloured with `--status-blocked-text` rather than a palette amber:
          that token is the console's "needs your attention, nothing is broken"
          state, it is the one a gated call already reads as elsewhere, and it
          themes for both schemes on its own — which the `dark:` pair it
          replaced had to restate by hand. `text-2xs` is the same 11px rung the
          sibling lines above use, by name. */}
      {(run.notices ?? []).map((notice, i) => (
        <p
          key={i}
          className="text-2xs text-[var(--status-blocked-text)]"
          data-testid="workflow-run-notice"
        >
          {notice}
        </p>
      ))}
    </div>
  );
}

/** One node's outcome in a history row: its id, how it went, how long it took. */
function RunNodeChip({ node }: { node: WorkflowRunNode }) {
  // Issue #881: three tones, not two. A blocked step is neither green nor red —
  // painting it red sends an operator hunting for a bug when the fix is a click
  // in Approvals, and painting it green is the lie the issue was filed about.
  // The amber token is the console's standing "needs a person, nothing is
  // broken" state, which a gated call already reads as elsewhere.
  const tone =
    node.status === "ok"
      ? {
          border: "border-status-done/40 bg-status-done-soft",
          dot: "bg-status-done",
        }
      : node.status === "blocked"
        ? {
            border: "border-status-blocked/50 bg-status-blocked-soft",
            dot: "bg-status-blocked",
          }
        : {
            border: "border-status-failed/50 bg-status-failed-soft",
            dot: "bg-status-failed",
          };
  return (
    <span
      className={`inline-flex items-center gap-1 rounded border px-1.5 py-0.5 text-3xs ${tone.border}`}
      data-testid={`workflow-run-node-${node.status}`}
    >
      <span className={`size-1.5 rounded-full ${tone.dot}`} />
      <span className="font-medium">{node.nodeId}</span>
      <span className="font-mono opacity-70">
        {node.elapsedMs < 1000
          ? `${node.elapsedMs}ms`
          : `${(node.elapsedMs / 1000).toFixed(1)}s`}
      </span>
    </span>
  );
}

/**
 * A once-a-second clock, live only while something on screen is counting
 * against it (issue #1007).
 *
 * Gated rather than always-on: the history drawer sits under the canvas for as
 * long as the operator leaves it open, and a settled row's duration is a fixed
 * number that re-rendering every second cannot change.
 */
function useRunningClock(active: boolean): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!active) return;
    // Read once on the way in too: the interval's first tick is a second away,
    // and a row that mounts already running should not show a stale elapsed.
    setNow(Date.now());
    const timer = window.setInterval(() => setNow(Date.now()), 1_000);
    return () => window.clearInterval(timer);
  }, [active]);
  return now;
}
