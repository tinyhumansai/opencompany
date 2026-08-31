// The run-history drawer and the last-run chip (issue #228, extended by #371
// and #383).
//
// Extracted from `WorkflowsView.tsx` (issue #303). The tone/count helpers moved
// further out again, to `run-health.ts`, because the workflow cards need the
// same reading — see that file's header.

import { useCallback, useEffect, useRef, useState, type RefObject } from "react";
import { Info } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { observatoryHref } from "@/views/observatory/hash";
import { Button } from "@/components/ui/button";
import {
  Popover,
  PopoverContent,
  PopoverTitle,
  PopoverTrigger,
} from "@/components/ui/popover";
import type { OpenCompanyClient } from "@/api/client";
import {
  fetchRunArtifacts,
  type DeliveryReport,
  type DeliveryStatus,
  type RunArtifactRow,
  type WorkflowGraph,
  type WorkflowRunNode,
  type WorkflowRunOutcome,
} from "@/api/workflows";
import { artifactHref } from "@/lib/task-output";

import { BlockedNodeApprovals } from "./BlockedNodeApprovals";
import { failedNodeOf, nodeName } from "./graph";
import {
  awaitingCount,
  formatDuration,
  isBlocked,
  isRunning,
  isStranded,
  liveParkedApprovalCount,
  relativeTime,
  runDuration,
  runTone,
  undeliveredCount,
  undeliveredNodes,
} from "./run-health";
import { stripEnginePrefixes } from "./run-error-message";

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

/**
 * Plain-English definitions for the run statuses this panel surfaces (issue
 * #1798).
 *
 * A run row spells its terminal state in prose, but its at-a-glance signals —
 * the coloured status dot, the "not delivered" badge, the "parked" badge — are
 * a private vocabulary an operator cannot act on if they cannot read it. These
 * one-liners give each its meaning, and they are the ONE source the dot's hover
 * title, the badge titles and the header legend all read, so the three can
 * never drift.
 *
 * The wording follows the semantics these terms are actually computed from, not
 * a guess: `not delivered` from {@link isUndelivered}/{@link undeliveredCount}
 * in `run-health.ts` (a report that will not reach its destination without a
 * change), `stranded` from {@link isStranded} (paused for an approval no
 * decision can still move), `blocked` from {@link isBlocked} (a step waiting on
 * an approval), `stopped` from a cancelled run, `parked` from
 * {@link liveParkedApprovalCount}. Keyed by the labels {@link runTone} returns
 * (see `VERDICT_TONE` in `run-health.ts`) so a dot can be looked up directly.
 */
const RUN_STATUS_DEFINITIONS = {
  running: "Still working through its steps — nothing here is final yet.",
  ok: "Finished, with nothing left undelivered and nobody waiting on you — every report either reached its destination or didn't need to (a dry run, or one an earlier run already delivered).",
  failed:
    "The run ended in error — usually a step that failed and the workflow needs a fix, but sometimes nothing in the graph got the chance to run at all, and the error can be a host restart or a capability that failed to build rather than anything wrong with the workflow. Read the error before assuming the workflow needs correcting.",
  // Codex review on #1821 (eleventh pass): this still asserted "the step
  // that was mid-flight" as if every stopped run has one. A run cancelled
  // before it ever reached the graph — `a_run_cancelled_before_it_starts_
  // does_not_walk_the_graph` in `runner.rs` is exactly this path, and
  // `RunHistoryPanel`'s own row body already treats an empty `startedNodes`
  // as its own case — has no mid-flight step to speak of, so the row body's
  // vacuous "every step that had started completed" holds trivially over
  // zero steps, but this definite description does not: it names a step
  // that, for that run, never existed. The trailing sentence states that
  // case rather than leaving it implied by an empty set.
  // Codex review on #1821 (thirteenth pass): `WorkflowNodeFinished` is
  // appended best-effort (`runner.rs`'s progress collector logs a failed
  // append and lets the run proceed unaffected) — the row body's
  // `midFlightNode` hedge already treats a missing finish row as inconclusive
  // rather than as proof a step was cut off. This definition still promised
  // the mid-flight step's own completion "was recorded", unconditionally —
  // true only when that node's finish append happened to succeed.
  stopped:
    "An operator stopped this run before it finished. A step that was mid-flight when the stop landed normally ran to completion — only a step stuck waiting on an outside call is cut off where it was — though its own completion record can go missing if that journal write silently failed. A run stopped before any step began has no such step at all.",
  blocked:
    "A step is waiting on you before the run can go on — usually a card sitting in Approvals, but a call that could not be queued for approval at all leaves nothing there to decide. That isn't always a workflow problem — the approvals queue itself can refuse the write, and no workflow change fixes that.",
  stranded:
    "The run paused for an approval, but nothing is waiting on you any more and no decision left can move it. Run it again if you still need it.",
  "not delivered":
    "The step ran, but its report never reached its destination and won't without a change.",
  "awaiting approval":
    "The run is parked on an approval and needs your decision in Approvals.",
  parked: "The run filed this into the Approvals queue for you to decide.",
} satisfies Record<string, string>;

/** CodeRabbit review on PR #1821: `RUN_STATUS_DEFINITIONS` was `Record<string,
 * string>` and `RUN_STATUS_LEGEND` was `readonly string[]`, so TypeScript
 * accepted a legend entry with no matching definition, and the legend's own
 * render site (`RUN_STATUS_DEFINITIONS[term]`, no fallback) had no guard
 * against one. Deriving the legend's element type from the map's own keys
 * makes that pairing a compile error instead of a convention — see the
 * `typecheck:unit` regression test in `workflow-run-status-legend.test.ts`. */
type RunStatusTerm = keyof typeof RUN_STATUS_DEFINITIONS;

/** The statuses worth a standing key, in the order the legend lists them: the
 * ones an operator hits without a definition anywhere else on the row.
 *
 * Exported so a unit test can assert membership directly against the source
 * of truth the header legend renders from, rather than opening the tooltip
 * portal (which mounts to `document.body`, not the render container) to read
 * it back out of the DOM. */
export const RUN_STATUS_LEGEND: readonly RunStatusTerm[] = [
  "blocked",
  "stranded",
  "not delivered",
  "parked",
  "stopped",
  "failed",
];

/** The status hover title for the run dot: its verdict word, plus the one-line
 * definition when there is one. Falls back to the bare word for a verdict this
 * map does not name (a host could grow an eighth — see `verdictOf`). */
function statusDotTitle(label: string): string {
  // `label` is any tone `verdictOf` returns, not narrowed to `RunStatusTerm` —
  // that's the whole point of the fallback below — so the lookup is cast back
  // to the pre-`satisfies` shape rather than widening the map's own type.
  const def = (RUN_STATUS_DEFINITIONS as Record<string, string | undefined>)[
    label
  ];
  return def ? `${label} — ${def}` : label;
}

/** The discoverable half of issue #1798: an info affordance in the panel header
 * whose popover is a short key to the run statuses. The per-badge hover titles
 * answer "what is THIS one"; this answers "what are all of them" for an
 * operator who does not know a badge is hoverable.
 *
 * Codex review on #1821 (fifth pass): Base UI's `Tooltip.Trigger` only opens
 * on hover or focus — by design, per Base UI's own split between Tooltip
 * (glance-only) and Popover (press-activatable, and the one that focuses the
 * popup itself rather than the virtual keyboard when opened by touch). A tap
 * satisfies neither, and `closeOnClick` on the tooltip trigger actively
 * cancels a pending open on click rather than starting one — so on a
 * touch-only device this affordance, the ONE place the parked/blocked/
 * stranded/not-delivered definitions previously described as "the panel's
 * one keyboard- and touch-reachable affordance" (see the "make the parked
 * badge discoverable" commit), was itself unreachable by touch. `Popover`
 * opens on press out of the box; `openOnHover` keeps the existing hover
 * discovery for the mouse case.
 *
 * Codex review on #1821 (sixth pass): the heading below used to be a plain
 * `span`. `Popover.Popup` renders `role="dialog"` and only wires its own
 * `aria-labelledby` when a `Popover.Title` is present to supply the id — a
 * bare `span` supplies nothing, so a screen-reader user who opened this
 * dialog heard an unnamed one. `PopoverTitle` is the primitive's own heading
 * component for exactly this; using it is what makes the popup register the
 * id in the first place. */
function RunStatusLegend() {
  return (
    <Popover>
      {/* Codex review on #1821 (eleventh pass): a bare `<button>` with no
          padding, height or width sizes itself to its one child — the
          `size-3.5` icon, ~14×14px — which is fine for a mouse but far below
          what a touch tap can reliably hit. `icon-xs` is the smallest sized
          hit-area the button scale already defines (`size-6`, 24×24px) and is
          the same fix `TaskDetailView`'s redact trigger and the styleguide's
          own Popover example (`render={<Button ... />}`) already use for an
          icon-only trigger, rather than inventing a one-off pixel value here. */}
      <PopoverTrigger
        openOnHover
        render={
          <Button
            variant="ghost"
            size="icon-xs"
            aria-label="What these run statuses mean"
            data-testid="workflow-run-legend"
            className="text-muted-foreground hover:text-foreground"
          />
        }
      >
        <Info className="size-3.5" aria-hidden="true" />
      </PopoverTrigger>
      <PopoverContent className="flex max-h-(--available-height) max-w-xs flex-col items-start gap-1 overflow-y-auto text-left">
        <PopoverTitle>What these statuses mean</PopoverTitle>
        {RUN_STATUS_LEGEND.map((term) => (
          <span key={term} className="block">
            <span className="font-medium">{term}</span> —{" "}
            {RUN_STATUS_DEFINITIONS[term]}
          </span>
        ))}
      </PopoverContent>
    </Popover>
  );
}

/** The delivery block of the run drawer: one line per attempt to route an
 * output node's report. This is the ONLY place an operator learns a report
 * didn't leave the building — a delivery failure never fails the run. */
export function DeliveryRows({ deliveries }: { deliveries: DeliveryReport[] }) {
  // Two counters, not one. A parked report is waiting on the operator, not
  // broken — badging it red alongside a transport failure would send them
  // hunting for a bug when the fix is a click in Approvals.
  const pending = deliveries.filter((d) => d.status === "pending").length;
  // Issue #981: the shared rung, not a fourth transcription of it. The filter
  // this replaces badged every test run "1 not delivered" — a `dry-run` row is
  // a report nothing attempted, on purpose — and said the same of a gate
  // continuation whose report an earlier run had already sent.
  const undelivered = undeliveredCount(deliveries);
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
            title={RUN_STATUS_DEFINITIONS["not delivered"]}
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
            ? `Last run failed: ${stripEnginePrefixes(run.error)}`
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
  client,
  company,
  runs,
  graph,
  workflowName,
  onClose,
  selectedRunSeq,
  onSelectRun,
  onFixWithCopilot,
  fixingRunSeq,
  fixReason,
  hasMore,
  onLoadOlder,
  loadingOlder,
}: {
  /**
   * The host client the lazy per-run "Files associated" fetch reads through
   * (issue #1684). Optional, like {@link onFixWithCopilot}: when absent the
   * files affordance is simply not offered — the live view always passes it, so
   * the omission only happens in focused render tests that assert other rows.
   */
  client?: OpenCompanyClient;
  /** The scoped company for that fetch — `null` for the default scope. */
  company?: string | null;
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
  /**
   * Whether an older page of `runs` exists behind the oldest `seq` currently
   * held (issue #1012) — the silent-truncation half of that issue. Omitted
   * (or `false`) hides "Load older" entirely, which is also how a host
   * predating the pagination fields degrades: no crash, just no affordance.
   */
  hasMore?: boolean;
  /** Fetch and append the next-older page. Absent hides "Load older" even if
   * `hasMore` is true — a caller with nowhere to route the click should not
   * offer it. */
  onLoadOlder?: () => void;
  /** An older-page fetch is in flight, so "Load older" shows as busy. */
  loadingOlder?: boolean;
}) {
  // Only one fix may be in flight at a time: `handleFixWithCopilot` sets a
  // single `prefilledDraft`/`editOpen` slot, so a second Fix started on a
  // different row of this same panel while the first is still running would
  // race it for that slot — whichever resolves last silently wins, which
  // could show the operator the wrong run's correction. Disabling every row's
  // button (not just the in-flight one's) while `fixingRunSeq` is set turns
  // that race into "wait your turn".
  const anyFixInFlight = fixingRunSeq != null;
  const selectedRowRef = useRef<HTMLDivElement>(null);
  // The failure panel can select a row on behalf of an operator who never
  // opened History themselves. Keep the selected failure in view without
  // changing their scroll position when it is already visible.
  useEffect(() => {
    selectedRowRef.current?.scrollIntoView?.({ block: "nearest" });
  }, [selectedRunSeq]);
  // Issue #1007: a clock, ticking only while a row is actually in flight. The
  // elapsed time on a running row is the console's acknowledgement that the
  // click did something, and it is only true if it moves.
  const now = useRunningClock(runs.some(isRunning));
  return (
    // Issue #1107: a left rail at `xl`, the bottom strip it has always been
    // below that. `CanvasShell` owns the placement and the width; this owns
    // the chrome, and the two readings differ only in which edge carries the
    // border and whether the list is capped or grows.
    //
    // `aside` + `aria-label`: at `xl` the rail is painted left of a canvas it
    // follows in the DOM, so it is reachable as a named complementary landmark
    // rather than only by tabbing past the graph.
    <aside
      aria-label="Run history"
      className="flex h-full flex-col border-t bg-card/60 xl:border-t-0 xl:border-r"
      data-testid="workflow-run-history"
    >
      {/* `flex-wrap` rather than a breakpoint: at 320px the workflow name drops
          to its own line on its own, and at full width it stays inline where
          there is room for it. */}
      <div className="flex items-start justify-between gap-2 border-b px-4 py-2">
        <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-0.5">
          <span className="text-sm font-medium">Run history</span>
          <Badge variant="secondary">{runs.length}</Badge>
          <RunStatusLegend />
          {workflowName && (
            <span className="max-w-full truncate text-xs text-muted-foreground">
              {workflowName}
            </span>
          )}
        </div>
        <Button
          variant="ghost"
          size="sm"
          className="-mr-2 shrink-0"
          onClick={onClose}
        >
          Dismiss
        </Button>
      </div>
      {/* Capped as a strip, growing as a rail. `min-h-0` is what actually lets
          it scroll inside the column — without it the flex item floors at its
          content height and the rail overflows the view instead. */}
      <div className="max-h-72 overflow-auto px-4 py-3 xl:min-h-0 xl:max-h-none xl:flex-1">
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
                client={client}
                company={company}
                run={run}
                graph={graph}
                now={now}
                selected={run.seq === selectedRunSeq}
                selectedRowRef={run.seq === selectedRunSeq ? selectedRowRef : undefined}
                onSelect={() => onSelectRun(run)}
                onFixWithCopilot={onFixWithCopilot}
                fixing={fixingRunSeq === run.seq}
                fixDisabled={anyFixInFlight}
                fixReason={fixReason?.seq === run.seq ? fixReason.reason : null}
              />
            ))}
            {/* Issue #1012: the honest half of the page cap — a truncated
                history says so, with a way to see more, rather than silently
                ending at `limit` and reading as the whole story. */}
            {hasMore && onLoadOlder && (
              <Button
                variant="ghost"
                size="sm"
                className="w-full"
                data-testid="workflow-run-load-older"
                disabled={loadingOlder}
                onClick={onLoadOlder}
              >
                {loadingOlder ? "Loading…" : "Load older"}
              </Button>
            )}
          </div>
        )}
      </div>
    </aside>
  );
}

/** One finished run: a summary line, its per-node trail, and its delivery rows.
 *
 * Clicking it overlays that run's node states on the canvas (issue #371) —
 * which is what makes a scheduled run's failure point visible, the case the
 * live canvas by definition cannot cover because nobody was watching.
 *
 * Exported for {@link RunTraceSheet}, which renders the same row inside the
 * traces-list transcript sheet — a run must read identically whether it's
 * opened from a workflow's own history or from the company-wide list. */
export function RunHistoryRow({
  client,
  company,
  run,
  graph,
  now,
  selected,
  selectedRowRef,
  onSelect,
  onFixWithCopilot,
  fixing,
  fixDisabled,
  fixReason,
}: {
  /** The host client for this row's lazy files fetch (issue #1684), if wired. */
  client?: OpenCompanyClient;
  /** The scoped company for that fetch. */
  company?: string | null;
  run: WorkflowRunOutcome;
  /** The selected workflow's graph, for node ids → names (issue #1007). */
  graph: WorkflowGraph | null;
  /** The clock a still-running row counts against (issue #1007). */
  now: number;
  selected: boolean;
  selectedRowRef?: RefObject<HTMLDivElement>;
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
  // Issue #981: which of those nodes produced a report that never went out.
  // Joined off `DeliveryReport.node` — the same rows the delivery block below
  // renders — so the chip and the block cannot disagree, and so the node the
  // operator clicks into stops claiming a clean run this row calls
  // `not delivered`.
  const droppedNodes = undeliveredNodes(run.deliveries);
  // Issue #881 / #880: read once, so the chip, the badge and the terminal line
  // below cannot disagree about whether this run stopped for a person.
  const blocked = run.blockedNodes ?? [];
  // Issue #900: only the receipts that actually landed a card, because this
  // paragraph tells the operator one is waiting. Counting a failed park here
  // said "needs your approval" and "decide it in Approvals" about a call the
  // very next sentence admitted nobody would ever be asked about.
  //
  // Issue #1189 took the same argument one step further: a card that landed and
  // has since fallen out of the queue is in exactly the position of one that
  // never landed, and `decidableApprovalCount` cannot see the difference —
  // a receipt records that a card was opened, never that it is still open. So
  // the count is the live one, and the sentence below can stand behind it.
  const parked = liveParkedApprovalCount(run);
  // Issue #1189: the run's own reading, so the paragraph, the badge and the
  // blocked-node list beneath them all branch off ONE fact.
  const stranded = isStranded(run);
  // The loud half: calls nobody will ever be asked about, because the park
  // itself failed or the excess was dropped past the per-turn cap. Strictly
  // worse than a parked one — there is no card to click.
  const unparkable = blocked.reduce((n, b) => n + (b.unparkable ?? 0), 0);
  // Codex review on #1821 (twelfth pass): `unparkable` sums two outcomes
  // (`caps/mod.rs`'s `park_gated_calls`) that are NOT the same failure —
  // `discarded` (this run's turn asked for more gated calls than one batch
  // may raise, so the excess was dropped before the queue ever saw it —
  // `MAX_APPROVAL_REQUESTS_PER_TURN`) and `parkFailed` (the queue itself
  // refused the write, or this runtime has none wired). Only `run.approvals`
  // — never `WorkflowBlockedNode.unparkable`, which the sum above reads —
  // keeps the two apart, via `outcome`. The closing remedy sentence below
  // named "the approvals queue itself may have refused it" as the cause for
  // BOTH, which is wrong for a discarded call: it was never offered to the
  // queue at all.
  const runApprovals = run.approvals ?? [];
  const discardedCalls = runApprovals.filter((a) => a.outcome === "discarded").length;
  const parkFailedCalls = runApprovals.filter((a) => a.outcome === "parkFailed").length;
  const failedNode = failedNodeOf(run);
  // Codex review on #1821 (eighth pass): `RunCancel` stops a run at the next
  // node boundary — the node already executing normally finishes and is
  // journaled, per the legend definition's own hedge ("only a step stuck
  // waiting on an outside call is cut off where it was"). `startedNodes`
  // names what the run was standing on; a settled run pairs every entry with
  // a `nodes` finish row UNLESS that node is the one the stop actually cut
  // off mid-flight (see `startedNodes`'s own doc comment in `api/workflows.ts`).
  //
  // Codex review on #1821 (ninth pass): `startedNodes`'s own doc comment says
  // "absent must read as 'no start trail', never as 'nothing started'" — a
  // host predating #1010/#382 sends nothing at all, which the `?? []`
  // fallback below used to fold into "no node was mid-flight", so a
  // cancelled run from that host always rendered the completed-cleanly
  // sentence even though the run's actual boundary is unrecorded and
  // unknowable. `knowsStartTrail` keeps that distinction: known-absent
  // trail (`[]`, nothing had started) and unknown trail (`undefined`) are
  // not the same fact.
  const knowsStartTrail = run.startedNodes !== undefined;
  const midFlightNode = (run.startedNodes ?? []).find(
    (nodeId) => !nodes.some((n) => n.nodeId === nodeId),
  );
  const errorMessage = run.error ? stripEnginePrefixes(run.error) : null;
  const duration = runDuration(run, now);
  // Completed, quiet runs are the common case. They need enough separation to
  // scan but not the full card chrome reserved for a state that asks something
  // of the operator. Each condition below protects a branch further down this
  // row, so no detail disappears into a deceptively light treatment.
  const compact =
    !run.error &&
    !run.cancelled &&
    !isRunning(run) &&
    !isBlocked(run) &&
    !isStranded(run) &&
    run.pendingApprovals.length === 0 &&
    undeliveredCount(run.deliveries) === 0 &&
    run.deliveries.length === 0 &&
    (run.notices?.length ?? 0) === 0;
  return (
    <div
      ref={selectedRowRef}
      className={`${
        compact
          ? "border-b border-x-0 border-t-0 rounded-none bg-transparent px-0 py-2"
          : "rounded-lg border bg-background/40 p-2"
      } ${run.error ? "border-status-failed/50 bg-status-failed-soft" : ""} ${
        selected ? "ring-2 ring-primary/40" : ""
      }`}
      data-testid="workflow-run-row"
    >
      <div className="mb-1 flex flex-wrap items-center gap-2">
        {/* Issue #1798: the run's verdict is otherwise a colour with no word.
            Its hover title names the state and defines it — the same one-liner
            the header legend lists — so the dot stops being a signal only a
            reader who already knows the palette can act on. `title` alone is a
            mouse-only affordance (Codex review on #1821, fifth pass): a
            non-focusable, unlabelled span gives keyboard, touch and
            screen-reader users nothing, since the header legend defines the
            terms but never says which one applies to THIS row. `role="img"` +
            `aria-label` puts the same one-liner in the accessibility tree, the
            way any other status icon this console labels. */}
        <span
          className={`size-1.5 rounded-full ${tone.dot}`}
          data-testid="workflow-run-status-dot"
          title={statusDotTitle(tone.label)}
          role="img"
          aria-label={statusDotTitle(tone.label)}
        />
        {run.scheduled && (
          <Badge variant="outline" className="h-4 px-1.5 text-3xs font-normal">
            scheduled
          </Badge>
        )}
        {/* The bridge to the Observatory: this panel says what each NODE did,
            and that view says what each node's AGENT did — the steps, the tool
            calls, the reasoning. Rendered only when the row carries a run id,
            since a row journaled before #371 has none to address. */}
        {run.runId && (
          <a
            href={observatoryHref(run.runId)}
            className="text-2xs text-muted-foreground underline underline-offset-2 hover:text-foreground"
            data-testid="workflow-run-inspect"
            onClick={(event) => event.stopPropagation()}
          >
            Inspect
          </a>
        )}
        <span
          className="text-2xs text-muted-foreground"
          title={new Date(run.atMillis).toLocaleString()}
        >
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
            title={RUN_STATUS_DEFINITIONS.parked}
          >
            parked {parked} approval{parked === 1 ? "" : "s"}
          </Badge>
        ) : (
          // Issue #1189: `!stranded`, because this badge is the run row's own
          // copy of the claim the drawer makes below. A stranded run has an
          // empty receipt (a paused gate files none), so `parked` is 0 and this
          // fallback fired — painting "3 pending approvals", in amber, on the
          // one run for which nothing is pending at all.
          !stranded &&
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
            <RunNodeChip
              key={`${node.nodeId}-${node.elapsedMs}`}
              node={node}
              undelivered={droppedNodes.has(node.nodeId)}
            />
          ))}
        </div>
      )}
      {run.error ? (
        // The outcome that used to be quietest of all: a run that died left one
        // host-stdout warning and nothing an operator could ever find.
        <>
          <div className="rounded-md border border-status-failed/50 bg-background/40 p-2">
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
            <p className="text-2xs font-medium text-status-failed-text">
              {failedNode
                ? `This run failed at “${nodeName(graph, failedNode)}”: ${errorMessage}`
                : `This run failed: ${errorMessage}`}
            </p>
            <p className="mt-1 text-2xs text-muted-foreground">
              {failedNode
                ? "Review the error details, then correct the workflow and run it again."
                : nodes.length > 0
                  ? // Codex review on #1821 (ninth pass): `failedNode` null does
                    // NOT mean nothing ran — a host restart can interrupt a run
                    // after one or more nodes already completed, and none of
                    // those finish rows carries an error (the synthetic outcome
                    // the boot sweep writes belongs to no node). `nodes.length`
                    // is the same signal `failureLocation` in `graph.ts` already
                    // branches on for this exact case; this sentence collapsed
                    // it into "nothing in the graph got the chance to run",
                    // which is only true when `nodes` is empty too.
                    //
                    // Codex review on #1821 (tenth pass): `WorkflowNodeFinished`
                    // is appended best-effort (`runner.rs`'s progress collector
                    // — a failed append is `warn!`-logged and the run proceeds
                    // unaffected), so a missing finish row is NOT proof the
                    // node behind it never failed; the append for the actual
                    // culprit can silently drop while `run.error` still lands.
                    // Naming a specific alternate cause ("host or capability
                    // problem") overclaimed what an absent row can prove — this
                    // now says only that the record may be incomplete, the same
                    // epistemic stance the legend definition above already
                    // takes ("Read the error before assuming the workflow needs
                    // correcting").
                    `Review the error details — ${nodes.length} step${nodes.length === 1 ? "" : "s"} completed before this run ended. What actually went wrong may not be fully recorded here, so read the error before deciding whether the workflow needs a fix.`
                  : // Codex review on #1821 (eighth pass): `failedNode` is null in
                    // exactly the cases the legend definition above already hedges
                    // on — a host restart, a capability that failed to build, or a
                    // graph that would not compile — none of which a workflow edit
                    // fixes. This sentence still said "correct the workflow"
                    // unconditionally, contradicting the definition two rounds ago
                    // fixed to say the opposite for this same run.
                    //
                    // Codex review on #1821 (twelfth pass): `nodes` being empty is
                    // not proof nothing ran, for the same reason the sibling arm
                    // above was fixed in the tenth pass — `WorkflowNodeFinished` is
                    // appended best-effort (`runner.rs`), so the FIRST failing
                    // node's own finish row can silently fail to journal while
                    // `run.error` still lands, leaving `nodes` empty even though
                    // that node executed. Naming "nothing in the graph got the
                    // chance to run" as the reading overclaimed what an empty trail
                    // can prove, the same overclaim the tenth pass removed from the
                    // `nodes.length > 0` arm. This now says only that the record may
                    // be incomplete, matching that arm's epistemic stance.
                    "Review the error details — no step is recorded as having completed, but an empty trail here isn't proof nothing ran: a step's own record can fail to save even when it executed. Read the error before deciding whether this is a host/capability problem or the workflow itself."}
            </p>
            <details className="mt-1">
              <summary className="cursor-pointer text-2xs text-muted-foreground">
                Details
              </summary>
              <pre className="mt-1 overflow-auto rounded border bg-muted/40 p-2 font-mono text-2xs leading-snug text-foreground">
                {run.error}
              </pre>
            </details>
          </div>
          {/* Issue #840 (PR-3): correction is an action, not part of the
              destructive error framing. Keeping it outside gives the neutral
              control its ordinary token treatment.

              `failedNode` gated it too (Codex review on #1821, eighth pass):
              the copilot re-wires the WORKFLOW, so offering it for a run that
              never traced to a step — a host restart, a failed capability
              build, an uncompilable graph — offers to fix something the run
              gave no evidence was broken.

              Codex review on #1821 (twelfth pass): that premise doesn't hold.
              `failedNode` comes from `failedNodeOf(run)`, which finds nothing
              when the failing node's own `WorkflowNodeFinished` never
              journaled (best-effort append, `runner.rs`) — the exact case the
              tenth/eleventh-pass fixes above already established `nodes`
              being empty does not prove nothing ran. The backend endpoint
              this button drives (`fix_from_run` → `resolve_fix_error`) never
              required a `failed_node_id` either: it only needs `run.error`
              non-empty (guaranteed here, inside the `run.error ?` branch) and
              works from the error text alone when no node is named — the
              request this callback sends doesn't even carry a node id. The
              copilot itself is the one that reads the error text and decides
              automatable-or-not (`fix_evidence_prompt` in
              `workflow_build.rs` tells it plainly: "If the failure cannot be
              fixed by re-wiring the graph … say so"), surfaced back here as
              `fixReason` when it declines. That is the explicit,
              failure-text-grounded classification the button's gate should
              defer to — not a frontend guess off a best-effort per-node
              record that can go missing for a node that genuinely failed. */}
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
        </>
      ) : run.cancelled ? (
        // Issue #383, the third terminal reading. Deliberately not a
        // destructive Alert: nothing went wrong, somebody decided they had seen
        // enough. It says "stopped", not "finished", because a node still
        // mid-flight when the stop lands — named by `midFlightNode` — is cut
        // off rather than allowed to complete, and a side effect it had
        // started may be half-done. `RunCancel` stops at the next node
        // boundary otherwise, so that is the exception, not the rule.
        <p
          className="text-2xs text-muted-foreground"
          data-testid="workflow-run-cancelled"
        >
          An operator stopped this run
          {nodes.length > 0
            ? ` after ${nodes.length} step${nodes.length === 1 ? "" : "s"}`
            : " before any step finished"}
          .{" "}
          {midFlightNode
            ? // Codex review on #1821 (tenth pass): `WorkflowNodeFinished` is
              // appended best-effort — a failed append is logged and the run
              // proceeds unaffected (`runner.rs`'s progress collector) — so an
              // unmatched `startedNodes` entry is not proof the node was cut
              // off; it is equally consistent with a node that completed
              // normally but whose own finish record silently failed to
              // journal. Naming a definitive hard abort overclaimed what a
              // missing row alone can prove.
              "The steps above completed; the one still running was either stopped where it was or finished without its own record being saved — its actual outcome isn't confirmed here."
            : knowsStartTrail
              ? // Codex review on #1821 (eighth pass): the legend definition
                // above was fixed to say the mid-flight step "normally ran to
                // completion and was recorded" — but this sentence still
                // claimed unconditionally that a step was cut off, which is
                // only true when `midFlightNode` names one. Most cancels land
                // cleanly at a boundary with nothing interrupted at all.
                //
                // Codex review on #1821 (thirteenth pass): `WorkflowNodeStarted`
                // is ALSO appended best-effort (`runner.rs`'s progress
                // collector) — the same fire-and-forget semantics the
                // tenth-pass fix already established for
                // `WorkflowNodeFinished`. A node whose own start silently
                // failed to journal never appears in `startedNodes` at all,
                // so it can never become `midFlightNode` even if it was
                // genuinely running when the stop landed. "Every step that
                // had started completed" therefore only speaks for the steps
                // the record actually captured, not for every step that in
                // fact started — scoped the claim to what was recorded and
                // added the hedge instead of promising the wider one.
                "Every step recorded as started also finished and was recorded before the stop took effect — though a step whose own start silently failed to journal wouldn't appear here at all, so this can't rule out one being cut off unseen."
              : // Codex review on #1821 (ninth pass): a host predating
                // #1010/#382 sends no `startedNodes` at all, and its own doc
                // comment says that must read as "no start trail", never as
                // "nothing started" — so it must not be folded into the same
                // claim as a settled run whose receipt confirms every started
                // step finished. This run's actual cancel boundary is
                // unrecorded and unknowable from here.
                "Whether the step in progress when the stop landed finished is not recorded for this run."}{" "}
          Any approvals it had already raised are still waiting for you.
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
        // run parked stays true. Since issue #899 (Stage 1), approving a parked
        // call CONTINUES this run automatically — so the closing sentence says
        // that, with the honest caveat that the continuation re-runs the agent's
        // turn and may ask again if it diverges. The unparkable-only case still
        // cannot continue and says so.
        <>
        <p
          className="text-2xs text-[var(--status-blocked-text)]"
          data-testid="workflow-run-blocked"
        >
          {/* Issue #900: the verb used to be unconditionally "needs your
              approval", even when every one of the blocked node's calls was
              unparkable — a call nobody will ever be asked about. That read
              as a promise of a card that does not exist, and contradicted the
              closing sentence below whenever `parked` was 0. */}
          {/* Issue #1189: THREE branches, on both clauses, because dropping
              `parked` to 0 for a stranded run flipped each of them to something
              wrong in its own way. The opening clause became "could not be
              queued for approval" — but these calls WERE queued; the card was
              opened and later lost, which is a different fact and the only one
              of the two an operator can act on differently. The closing clause
              became "change the policy and run the workflow again" — but no
              policy refused anything here, so it sends them to edit a setting
              that was never the problem. */}
          Not finished — {blocked.map((b) => `“${b.nodeId}”`).join(", ")}{" "}
          {parked > 0
            ? blocked.length === 1
              ? "needs your approval"
              : "need your approval"
            : stranded
              ? "needed your approval"
              : "could not be queued for approval"}, so{" "}
          {blocked.length === 1 ? "it" : "they"} produced nothing and the steps
          after {blocked.length === 1 ? "it" : "them"} did not run.{" "}
          {parked > 0 &&
            `This run parked ${parked} approval${parked === 1 ? "" : "s"}. `}
          {unparkable > 0 &&
            `${unparkable} call${unparkable === 1 ? "" : "s"} could not be queued for approval at all, so you will not be asked about ${unparkable === 1 ? "it" : "them"}. `}
          {parked > 0
            ? `Approve ${parked === 1 ? "it" : "them"} in Approvals and this run continues on its own — approving re-runs the step, so a changed decision may ask again.`
            : stranded
              ? // Says only what is observable. Approving a gate starts a NEW
                // run rather than continuing this one, and records no link back
                // — so a run whose approvals were all decided and one whose
                // cards were lost look identical from here, and claiming
                // either would be a diagnosis the console cannot make. Re-run
                // is offered as an option, not as a remedy for a stated cause.
                "Nothing here is waiting on you any more, and this run cannot be continued. Run the workflow again if you still need it."
              : // Codex review on #1821 (eighth pass, same site as the sixth):
                // `parkFailed` fires both when the approvals queue itself
                // refused the write AND when this runtime never wired one at
                // all — the frontend has no field naming which, per the
                // legend definition's own hedge two rounds ago ("the
                // approvals queue itself can refuse the write, and no
                // workflow change fixes that"). "Change the policy" names a
                // cause that is never the one in either code path; nothing
                // about policy content is what failed here.
                //
                // Codex review on #1821 (twelfth pass): this named "the
                // approvals queue itself may have refused it" as the cause
                // even when every unparkable call was `discarded` — dropped
                // by the per-turn cap before the queue ever saw it, a
                // different and distinguishable cause (`run.approvals`'
                // `outcome`, computed above). A run with ONLY `discarded`
                // calls (and at least one, so the classification is positive
                // rather than absent) gets its own sentence; a run with any
                // `discarded` alongside `parkFailed` calls names both; every
                // other case — `parkFailed` only, or a run whose `approvals`
                // rows predate this classification and carry neither outcome
                // — keeps the original sentence, which is this default
                // rather than a claim this fold cannot back.
                discardedCalls > 0 && parkFailedCalls === 0
                  ? "Nothing here could be queued for approval — this run's turn asked for more approvals than one batch may raise, so the excess was dropped before the queue ever saw it. Run it again — a turn that asks for fewer approvals at once will queue cleanly."
                  : discardedCalls > 0 && parkFailedCalls > 0
                    ? "Nothing here could be queued for approval — some were dropped because this run's turn asked for more approvals than one batch may raise, and the rest because the approvals queue itself may have refused them, which no workflow change fixes. Run it again once you've cut how many approvals one turn asks for and confirmed the queue is healthy."
                    : "Nothing here could be queued for approval — the approvals queue itself may have refused it, which no workflow change fixes. Run it again once that's resolved."}
        </p>
        {/* Issue #1014 (PR-B): the gated tool names per blocked node and a link
            per parked card to the Approvals queue — the sentence above says
            "decide it in Approvals" and, until this, pointed nowhere. */}
        <BlockedNodeApprovals
          blockedNodes={blocked}
          approvalRows={run.approvals}
        />
        </>
      ) : run.running ? (
        // Same root cause as the tone bug: a run still walking its graph has no
        // error, no cancellation and no deliveries yet, so it fell through to
        // the "Finished" line below and told the operator it was over. It is
        // not, and its reports have not been routed yet.
        <p className="text-2xs text-muted-foreground">
          Still running — reports are routed when it finishes.
        </p>
      ) : stranded ? (
        // Issue #1189, and the arm the issue text does not enumerate — but the
        // one the 34 runs actually render. The chain above it is
        // `error → cancelled → isBlocked → running`, and `isBlocked` reads
        // `blockedNodes.length`. A fully stranded GATE run has no blocked-node
        // rows at all (a paused gate writes none), so it fell straight through
        // to the `pendingApprovals` arm below — whose closing line is "Approve
        // or decline it in Approvals to carry the run on." Fixing the summary,
        // the badge and the verdict and leaving this would have shipped the
        // same defect on the half the issue calls bigger.
        //
        // Placed above `pendingApprovals` to mirror the host ladder, where
        // `stranded` outranks `awaiting-approval` for exactly this reason: both
        // arms describe a run stopped for a person, and only one of them is
        // still true.
        //
        // Muted rather than amber: amber is the console's "needs your
        // attention" state, and nothing here needs anybody's. Same reasoning as
        // the tone in `run-health.ts`.
        <>
          {/* The reports it DID route before the gate, on the same terms the
              awaiting arm shows them: replacing them would trade one silent
              omission for another. */}
          {run.deliveries.length > 0 && (
            <DeliveryRows deliveries={run.deliveries} />
          )}
          <p
            className="text-2xs text-muted-foreground"
            data-testid="workflow-run-stranded"
          >
            Not finished — this run stopped for your approval on{" "}
            {run.pendingApprovals.map((node) => `“${node}”`).join(", ")}, and
            nothing here is waiting on you any more. No decision left can move
            it
            {run.deliveries.length === 0 ? ", and no reports were routed" : ""}.
            Run the workflow again if you still need it.
          </p>
        </>
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
      {/* Issue #1684: the files this run produced, deep-linked into the card
          that made each. Rendered ONLY when the run carries a `runId` — a
          pre-#371 orphan row has nothing to key a per-run fetch on — and the
          fetch is lazy, fired on first expand, so a collapsed row (the common
          case in a long history) makes zero network calls. */}
      {run.runId && client && (
        <RunFilesSection
          client={client}
          company={company ?? null}
          runId={run.runId}
        />
      )}
    </div>
  );
}

/** The lazy "Files associated" disclosure on a run row (issue #1684).
 *
 * A native `<details>` so the row makes no request until an operator opens it —
 * the whole point of the lazy per-run route behind it. The fetch fires once, on
 * the first expand; a failed fetch clears the latch so the next open retries.
 * Each file deep-links into its card's Artifacts tab at the run's version
 * ({@link artifactHref}), with the workspace-node link offered as a second hop
 * when the file was mirrored into the shared tree. */
function RunFilesSection({
  client,
  company,
  runId,
}: {
  client: OpenCompanyClient;
  company: string | null;
  runId: string;
}) {
  const [files, setFiles] = useState<RunArtifactRow[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState(false);
  const [truncated, setTruncated] = useState(false);
  // A one-shot latch: the fetch runs on the first open and not on every toggle,
  // and a collapse-then-reopen does not re-hit the route. Cleared on failure so
  // a reopen can retry.
  const requested = useRef(false);
  const detailsRef = useRef<HTMLDetailsElement>(null);
  // The scope the most recent request was made for, kept current on every
  // render. A response that settles after a company/runId change can compare
  // the scope it was fired for against this and recognise itself as stale —
  // the race in issue #1693 where the previous request's `.then`/`.catch`
  // would otherwise overwrite the new scope's state.
  const scopeRef = useRef({ company, runId });
  scopeRef.current = { company, runId };

  const load = useCallback(() => {
    if (requested.current) return;
    requested.current = true;
    setLoading(true);
    setError(false);
    const scope = { company, runId };
    const stale = () =>
      scopeRef.current.company !== scope.company ||
      scopeRef.current.runId !== scope.runId;
    fetchRunArtifacts(client, company, runId)
      .then(({ files, truncated }) => {
        if (stale()) return;
        setFiles(files);
        setTruncated(truncated);
      })
      .catch(() => {
        if (stale()) return;
        requested.current = false;
        setError(true);
      })
      .finally(() => {
        if (!stale()) setLoading(false);
      });
  }, [client, company, runId]);

  // `RunHistoryPanel` keys each row only by `run.seq` (not by company), and
  // journal sequences commonly repeat across companies and workflows. When an
  // operator switches company or workflow while a row stays expanded, React
  // can reuse THIS component instance for an unrelated run — the one-shot
  // latch above then never re-fires, and the old scope's files (titles,
  // paths) stay on screen under the new run. Reset on every scope change, and
  // re-fetch immediately if the disclosure is already open — the `onToggle`
  // handler below only fires on an open/close transition, not on a prop
  // change while already open.
  useEffect(() => {
    requested.current = false;
    setFiles(null);
    setTruncated(false);
    setError(false);
    setLoading(false);
    if (detailsRef.current?.open) {
      load();
    }
  }, [company, runId, load]);

  return (
    <details
      ref={detailsRef}
      className="mt-1.5"
      data-testid="workflow-run-files"
      onToggle={(e) => {
        if ((e.currentTarget as HTMLDetailsElement).open) load();
      }}
    >
      <summary
        className="cursor-pointer text-2xs text-muted-foreground"
        data-testid="workflow-run-files-toggle"
      >
        Files associated
      </summary>
      <div className="mt-1 space-y-1">
        {loading && (
          <p className="text-2xs text-muted-foreground">Loading…</p>
        )}
        {error && (
          <p
            className="text-2xs text-status-failed-text"
            data-testid="workflow-run-files-error"
          >
            Couldn't load this run's files. Reopen to try again.
          </p>
        )}
        {files && files.length === 0 && (
          <p
            className="text-2xs text-muted-foreground"
            data-testid="workflow-run-files-empty"
          >
            No files from this run.
          </p>
        )}
        {truncated && (
          <p
            className="text-2xs text-muted-foreground"
            data-testid="workflow-run-files-truncated"
          >
            Showing this run's newest files only.
          </p>
        )}
        {files?.map((file) => (
          <div
            key={`${file.taskId}-${file.artifactId}`}
            className="flex flex-col"
            data-testid="workflow-run-file"
          >
            {/* The canonical Artifacts-tab hash the whole console navigates
                by — no new routing, the Tasks view reads it and focuses the
                card + artifact at the run's version. */}
            <a
              className="truncate text-2xs text-primary hover:underline"
              href={artifactHref(file.taskId, file.artifactId, file.latestVersion)}
            >
              {file.title}
            </a>
            <span className="text-3xs text-muted-foreground">
              {file.taskTitle ? `${file.taskTitle} · ` : ""}
              {/* A legacy record (issue #244) has no source path; label it as
                  such rather than showing an empty secondary line. */}
              {file.source ?? "(legacy)"}
              {file.workspaceNodeId && (
                <>
                  {" · "}
                  <a
                    className="hover:underline"
                    href={`#/workspace/${encodeURIComponent(file.workspaceNodeId)}`}
                    data-testid="workflow-run-file-workspace"
                  >
                    Open in workspace
                  </a>
                </>
              )}
            </span>
          </div>
        ))}
      </div>
    </details>
  );
}

/** One node's outcome in a history row: its id, how it went, how long it took —
 * and, since issue #981, whether the report it produced actually went out.
 *
 * The two are separate facts and the chip states them separately. `node.status`
 * answers "did the engine run this step?", and for a dropped report the honest
 * answer is `ok`: delivery happens after the engine returns, so the node really
 * did run and its work stands. What was wrong was that the chip said only that,
 * beside a run the same panel scored `undelivered`. So the green dot stays and a
 * second, labelled segment carries the delivery — nothing here is re-tinted to
 * mean something it does not. */
function RunNodeChip({
  node,
  undelivered,
}: {
  node: WorkflowRunNode;
  undelivered: boolean;
}) {
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
      {undelivered && (
        <span
          className="flex items-center gap-1 border-l border-status-failed/40 pl-1.5 text-[var(--status-failed-text)]"
          data-testid="workflow-run-node-undelivered"
          title="This step ran. Its report did not go out — see Report delivery below."
        >
          <span className="size-1.5 rounded-full bg-status-failed" />
          not delivered
        </span>
      )}
    </span>
  );
}

/**
 * A once-a-second clock, live only while something on screen is counting
 * against it (issue #1007).
 *
 * Gated rather than always-on: the history rail stays up for as long as the
 * operator leaves it open, and a settled row's duration is a fixed number that
 * re-rendering every second cannot change.
 */
export function useRunningClock(active: boolean): number {
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
