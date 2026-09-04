// The run-output drawer for a synchronous run, and the defensive parse that
// turns the engine's nested state into readable per-node cards (issues #68,
// #154, #170).
//
// Extracted verbatim from `WorkflowsView.tsx` (issue #303).

import { SquareKanban } from "lucide-react";
import { useMemo } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

import type { ApprovalSummary, GrantScope, Verdict } from "@/api/types";
import type {
  WorkflowGraph,
  WorkflowRunBoardRow,
  WorkflowRunNode,
  WorkflowRunResult,
} from "@/api/workflows";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { ApprovalRow } from "@/views/chat/ApprovalRow";
import type { DecidedApproval } from "@/views/chat/model";

import { BlockedNodeApprovals } from "./BlockedNodeApprovals";
import { DeliveryRows } from "./RunHistoryPanel";
// Issue #596: the per-node parse is now shared with the durable run inspector
// and the pre-publish approvals card, so it lives in `run-output.ts`.
import {
  type NodeResult,
  noNodeResultsNotice,
  parseRunNodes,
} from "./run-output";
// Issue #1002: which of the company's parked cards this run is held on.
import { blockingApprovals, runApprovals, type RunApproval } from "./run-approvals";
import { resumeClaimFor } from "./resume-claim";
// Issue #981: the one rung for "this report did not go out", shared with the
// history rows, the run dot, the SSE toast and the host itself.
import { undeliveredCount, undeliveredNodes } from "./run-health";

/** Stable empty defaults, so an omitted prop cannot churn the card's props. */
const EMPTY_APPROVALS: ApprovalSummary[] = [];
const EMPTY_NAMES = new Map<string, string>();
const EMPTY_DECIDING: ReadonlyMap<string, Verdict> = new Map();
const EMPTY_DECIDED: Record<string, DecidedApproval> = {};
const EMPTY_FAILED: Record<string, string> = {};
const EMPTY_VERDICTS: Record<string, Verdict> = {};

/**
 * The in-flight verdict for one approval, as its own map.
 *
 * Narrowed per row for the reason #373 established one surface over: a decision
 * in flight on one of a run's cards must not freeze the buttons on the others.
 * Falls back to a shared empty map so an idle row's props do not churn on every
 * render of the drawer.
 */
function decidingFor(
  id: string,
  deciding: ReadonlyMap<string, Verdict>,
): ReadonlyMap<string, Verdict> {
  const verdict = deciding.get(id);
  return verdict ? new Map([[id, verdict]]) : EMPTY_DECIDING;
}

/** The run-output drawer: one readable card per executed node (the producing
 * agent and its reply, markdown-rendered) plus the branch each condition node
 * took, any nodes left pending approval, and the raw engine state collapsed
 * behind a toggle. Falls back to a raw JSON dump when the output doesn't match
 * the expected per-node shape. */
export function RunResultPanel({
  result,
  graph,
  request,
  onClose,
  approvals = EMPTY_APPROVALS,
  now,
  askerNames = EMPTY_NAMES,
  deciding = EMPTY_DECIDING,
  decided = EMPTY_DECIDED,
  failed = EMPTY_FAILED,
  onDecide,
}: {
  result: WorkflowRunResult;
  graph: WorkflowGraph | null;
  /** What the operator asked this run for (issue #154); "" when they asked for
   * nothing, in which case the line is omitted rather than showing a bare dash. */
  request: string;
  onClose: () => void;
  /**
   * The company's parked approvals — the **whole** queue (issue #1002), the
   * same array the Approvals page and the sidebar badge read.
   *
   * Handed in whole and narrowed here, deliberately. Nothing upstream filters
   * it, so this surface cannot make the page show less or the badge disagree
   * with it: it is a second *reader* of one queue, not a second queue.
   *
   * And it is a **live** array — the feed refreshes it on every
   * `approval_resolved` frame — which is what stops the drawer calling a step
   * blocked after somebody else cleared it. Optional, so a caller that does not
   * wire it gets exactly the pre-#1002 drawer.
   */
  approvals?: ApprovalSummary[];
  /** The feed's clock, for the "waiting N minutes" line. Defaults to now. */
  now?: number;
  /** Agent id → display name, for the "Asked by" line. */
  askerNames?: Map<string, string>;
  /** The verdict an approval is waiting on, keyed by id; empty when idle. */
  deciding?: ReadonlyMap<string, Verdict>;
  /**
   * Verdicts already witnessed, with the last-seen summary — from this drawer,
   * the Approvals page, or another console over the stream.
   *
   * The host drops a resolved approval from the queue at once, so this is what
   * lets a decided row settle in place rather than blinking out of the panel.
   */
  decided?: Record<string, DecidedApproval>;
  /** Decisions that did not land, keyed by approval id — the message to show. */
  failed?: Record<string, string>;
  /**
   * Record one decision, on one approval, by its own id.
   *
   * One ordinary per-id resolve per approval — the same call the Approvals page
   * makes, through the same client. There is no batch decision on the wire and
   * none is invented here: each park stays a separate single-use decision, so a
   * racing operator on the other surface gets a real approval or a `NotParked`
   * no-op receipt, never a double execution.
   */
  onDecide?: (
    approval: ApprovalSummary,
    verdict: Verdict,
    scope: GrantScope,
    /** The operator's answer to a blocker's question (B-046) — see `ApprovalRow`. */
    answer?: string,
  ) => void;
}) {
  const nodeResults = useMemo(
    () => parseRunNodes(result.output, graph),
    [result.output, graph],
  );
  // B-005 / B-039: why there are no cards, said in terms of how this run
  // actually ended. `null` when there are cards.
  const noResults = useMemo(
    () => noNodeResultsNotice(nodeResults, result.output, result.verdict),
    [nodeResults, result.output, result.verdict],
  );
  const deliveries = result.deliveries ?? [];
  const pendingDeliveryCount = deliveries.filter(
    (d) => d.status === "pending",
  ).length;
  // Issue #981: the shared predicate. The local filter this replaces counted a
  // `dry-run` row, so the panel that exists to make a TEST run readable told the
  // operator their test had failed to deliver every single time.
  const droppedReports = undeliveredCount(deliveries);
  // Which nodes those rows belong to, so the Steps trail can say it beside the
  // step instead of leaving the delivery block to say it alone.
  const droppedNodes = undeliveredNodes(deliveries);
  // Issue #542: a test run. The header says so plainly, and the delivery rows
  // (already `skipped`) read as "would have gone to …" rather than as failures.
  const isDry = result.dryRun === true;
  const nodeTimeline = result.nodes ?? [];
  // Issues #881 / #880. Read once so the badge, the sentence and the step chips
  // cannot disagree about whether this run stopped for a person.
  const blockedNodes = result.blockedNodes ?? [];
  const parkedCount = result.approvals?.length ?? 0;
  // Issue B-013: what deciding this run's cards DOES is not this screen's to
  // word. It used to be, and it stated the gated-call behaviour for both kinds
  // — "this run continues on its own" — while the Observatory told the same
  // operator, about the same run, that answering does not restart it.
  // `resumeClaimFor` is the one place either screen answers that now.
  const resumeClaim = resumeClaimFor(blockedNodes);
  const resumeSuffix = resumeClaim ? ` ${resumeClaim}` : "";
  // Issue #1014: the board writes this run performed — shipped on the wire
  // since #661 but never rendered, so "this run opened card X" was invisible.
  const board = result.board ?? [];
  // Issue #1002. Derived from the LIVE queue, never from `result.approvals` —
  // that is a receipt of what this run opened (#880) and nothing ever comes back
  // to flip a row on it, so a panel grounded there would go on calling a step
  // blocked long after the card was cleared. The receipt is used for the one
  // thing it *is* authoritative about: which node parked which card.
  const runRows = useMemo(
    () => runApprovals(approvals, decided, result.runId, result.approvals),
    [approvals, decided, result.runId, result.approvals],
  );
  const stillWaiting = useMemo(() => blockingApprovals(runRows), [runRows]);
  /** Whether the drawer is actually offering the decision, not just naming it. */
  const canDecideHere = runRows.length > 0 && !!onDecide;

  return (
    // Issue #1205: a right rail at `xl`, the bottom strip it used to always be
    // below that — the same pattern `RunHistoryPanel` proved on the left for
    // issue #1107, mirrored. `border-l` (not `border-r`) because this rail
    // sits on the canvas's right, so it borders the canvas on its OWN left
    // edge; `CanvasShell` owns the placement and the width.
    <aside
      aria-label="Run result"
      className="flex h-full flex-col border-t bg-card/60 xl:border-t-0 xl:border-l"
      data-testid="workflow-run-result"
    >
      <div className="flex items-center justify-between px-4 py-2">
        <div className="flex items-center gap-2">
          <span className="text-sm font-medium">Run result</span>
          {isDry && (
            <Badge
              variant="outline"
              /* Brand, not a status hue. A dry run is a *mode the operator
                 chose*, and it sits beside three genuine status badges — in
                 amber, cyan and red — where a fourth state colour would read
                 as a fourth outcome. */
              className="border-primary/40 bg-primary/10 text-primary"
              data-testid="workflow-run-dry-badge"
            >
              Test run — nothing was sent, no tokens spent
            </Badge>
          )}
          {pendingDeliveryCount > 0 && (
            <Badge
              variant="outline"
              className="border-status-blocked/40 bg-status-blocked-soft"
            >
              {pendingDeliveryCount} report
              {pendingDeliveryCount === 1 ? "" : "s"} awaiting approval
            </Badge>
          )}
          {droppedReports > 0 && (
            <Badge
              variant="outline"
              className="border-status-failed/40 bg-status-failed-soft"
            >
              {droppedReports} report{droppedReports === 1 ? "" : "s"} not
              delivered
            </Badge>
          )}
          {/* Issue #880: what this run PARKED, in those words. Nothing here is
              refreshed when the operator approves a card, so a "pending" count
              is stale the moment they do; a receipt of what the run opened
              stays true. The live answer lives on the Approvals page. */}
          {parkedCount > 0 ? (
            <Badge
              variant="outline"
              className="border-status-blocked/40 bg-status-blocked-soft"
              data-testid="workflow-run-result-parked"
            >
              parked {parkedCount} approval{parkedCount === 1 ? "" : "s"}
            </Badge>
          ) : (
            result.pendingApprovals.length > 0 && (
              <Badge
                variant="outline"
                className="border-status-blocked/40 bg-status-blocked-soft"
              >
                {result.pendingApprovals.length} pending approval
                {result.pendingApprovals.length === 1 ? "" : "s"}
              </Badge>
            )
          )}
        </div>
        <Button variant="ghost" size="sm" onClick={onClose}>
          Dismiss
        </Button>
      </div>
      {/* Capped as a strip, growing as a rail — same as `RunHistoryPanel`'s
          body. `min-h-0` is what actually lets it scroll inside the column;
          without it the flex item floors at its content height and the rail
          overflows the view instead. */}
      <div className="max-h-72 overflow-auto px-4 pb-3 xl:min-h-0 xl:max-h-none xl:flex-1">
        {request && (
          <p className="mb-2 text-xs text-muted-foreground">
            <span className="font-medium text-foreground">Requested:</span>{" "}
            {request}
          </p>
        )}
        {/* Issue #881: the sentence the drawer never had. A blocked run comes
            back with no error, nothing delivered and — before this — eight
            green node cards, so the operator's only reading was "it worked".
            Since issue #899 (Stage 1) it says approving CONTINUES this run
            automatically, with the honest caveat that the continuation re-runs
            the agent's turn and may ask again if it diverges. */}
        {blockedNodes.length > 0 && (
          <p
            className="mb-2 text-xs text-[var(--status-blocked-text)]"
            data-testid="workflow-run-result-blocked"
          >
            Not finished — {blockedNodes.map((b) => `“${b.nodeId}”`).join(", ")}{" "}
            {blockedNodes.length === 1 ? "needs" : "need"} your approval, so{" "}
            {blockedNodes.length === 1 ? "it" : "they"} produced nothing and the
            steps after {blockedNodes.length === 1 ? "it" : "them"} did not run.{" "}
            {parkedCount > 0
              ? `This run parked ${parkedCount} approval${parkedCount === 1 ? "" : "s"}; decide ${parkedCount === 1 ? "it" : "them"} ${
                  // Issue #1002: the cards are on this screen now, so say so —
                  // and keep naming Approvals, because that page is still the
                  // queue and still holds every one of them. Only claimed when
                  // the section is actually rendering: a run whose cards this
                  // console cannot join (an old host that sends back no run id)
                  // still gets the original sentence rather than a pointer to
                  // something that is not there.
                  canDecideHere ? "below or in Approvals" : "in Approvals"
                }.${resumeSuffix}`
              : "Nothing here can be approved; change the policy and run the workflow again."}
          </p>
        )}
        {/* Issue #1014 (PR-B): the gated tool names per blocked node, and a link
            per parked card to the Approvals queue. Kept consistent with the run
            history drawer — the section below can also decide them inline when
            this console can join the live queue (#1002), but the tool names and
            a pointer to the page stand even when it cannot. */}
        {blockedNodes.length > 0 && (
          <BlockedNodeApprovals
            blockedNodes={blockedNodes}
            approvalRows={result.approvals}
          />
        )}
        {blockedNodes.length === 0 && result.pendingApprovals.length > 0 && (
          <p className="mb-2 text-xs text-muted-foreground">
            Waiting on: {result.pendingApprovals.join(", ")}
          </p>
        )}

        {/* Issue #1002: the half the drawer never had — the cards this run is
            held on, decidable here, scoped to the run on screen. The Approvals
            page still lists every one of these and stays the queue; this is a
            second reader of it, narrowed, so an operator does not have to leave
            the run they are looking at to unblock it. */}
        {canDecideHere && onDecide && (
          <RunApprovalsSection
            rows={runRows}
            stillWaiting={stillWaiting.length}
            graph={graph}
            now={now ?? Date.now()}
            askerNames={askerNames}
            deciding={deciding}
            failed={failed}
            onDecide={onDecide}
          />
        )}

        {deliveries.length > 0 && <DeliveryRows deliveries={deliveries} />}

        {/* Issue #1014: the cards this run opened or re-owned. Shipped on the
            wire since #661, but the drawer never rendered them — so a run that
            opened a card in To-do read as if it touched nothing. */}
        {board.length > 0 && <BoardRows board={board} />}

        {/* Issue #542: the per-node timeline, carried on every synchronous run's
            body. It is most load-bearing for a dry run, which journals nothing —
            this is the only place a test run's step-by-step trail appears, since
            the canvas paints no optimistic frontier for it. */}
        {nodeTimeline.length > 0 && (
          <NodeTimeline
            nodes={nodeTimeline}
            graph={graph}
            undelivered={droppedNodes}
          />
        )}

        {noResults ? (
          <p
            className="mb-2 text-xs text-muted-foreground"
            data-testid="workflow-run-no-node-results"
          >
            {noResults.message}
          </p>
        ) : (
          <div
            className="mb-2 space-y-2"
            data-testid="workflow-run-node-results"
          >
            {(nodeResults ?? []).map((n) => (
              <NodeResultCard key={n.id} node={n} />
            ))}
          </div>
        )}

        <details open={noResults?.showRaw ?? false}>
          <summary className="cursor-pointer text-xs text-muted-foreground">
            Show raw engine output
          </summary>
          <pre className="mt-1 rounded-lg border bg-muted/40 p-2 font-mono text-2xs leading-snug">
            {JSON.stringify(result.output, null, 2)}
          </pre>
        </details>
      </div>
    </aside>
  );
}

/** The label and badge tone for one board action. The two `*Failed` arms get a
 * failed hue — a write the node was told would happen and did not — while the
 * two success arms stay neutral, since the card itself is the loud thing. */
const BOARD_ACTION: Record<
  WorkflowRunBoardRow["action"],
  { label: string; tone: string; failed: boolean }
> = {
  spawned: { label: "Opened", tone: "border-border bg-muted/60", failed: false },
  assigned: { label: "Assigned", tone: "border-border bg-muted/60", failed: false },
  spawnFailed: {
    label: "Open failed",
    tone: "border-status-failed/40 bg-status-failed-soft",
    failed: true,
  },
  assignFailed: {
    label: "Assign failed",
    tone: "border-status-failed/40 bg-status-failed-soft",
    failed: true,
  },
};

/**
 * The board writes a run performed (issue #1014) — one row per card it opened
 * or re-owned, shipped on the wire since #661 but never rendered until now.
 *
 * A row with a `taskId` links straight to its card through the same
 * `#/tasks/:id` hash route the Approvals footer and the whole console use. A
 * `spawnFailed` row has no card to point at — no write landed — so it renders
 * the attempted title as plain text, which is the only thing that explains the
 * failure.
 */
function BoardRows({ board }: { board: WorkflowRunBoardRow[] }) {
  const failed = board.filter((r) => BOARD_ACTION[r.action].failed).length;
  return (
    <div
      className="mb-3 space-y-1.5 rounded-lg border bg-background/40 p-2"
      data-testid="workflow-run-board"
    >
      <div className="flex items-center gap-2">
        <span className="text-xs font-medium">Board</span>
        {failed > 0 && (
          <Badge
            variant="outline"
            className="h-4 px-1.5 text-3xs font-normal border-status-failed/40 bg-status-failed-soft"
          >
            {failed} not written
          </Badge>
        )}
      </div>
      {board.map((row, i) => {
        const meta = BOARD_ACTION[row.action];
        // The card link — every arm but `spawnFailed` names a card by id.
        const label = row.title ?? "the card";
        return (
          <div
            key={`${row.action}-${row.taskId ?? ""}-${i}`}
            className="flex flex-wrap items-baseline gap-1.5"
          >
            <Badge
              variant="outline"
              className={`h-4 px-1.5 text-3xs font-normal ${meta.tone}`}
            >
              {meta.label}
            </Badge>
            {row.taskId ? (
              <a
                href={`#/tasks/${encodeURIComponent(row.taskId)}`}
                className="flex w-fit items-center gap-1 text-2xs font-medium text-accent-foreground underline-offset-2 hover:underline"
              >
                <SquareKanban className="size-3 shrink-0" />
                {label}
              </a>
            ) : (
              <span className="text-2xs">{label}</span>
            )}
            {row.assignee && (
              <span className="text-2xs text-muted-foreground">
                → {row.assignee}
              </span>
            )}
          </div>
        );
      })}
    </div>
  );
}

/**
 * The cards this run is held on, decidable in place (issue #1002).
 *
 * ## One row per approval, per-node, one decision each
 *
 * Each row renders the **shipped** approval card — the same
 * `@/components/approval-card` pieces the Approvals page and the inline chat
 * card are built from, through `ApprovalRow` — over a single-item list. A
 * single-item card is exactly what that component rendered before #842
 * consolidated batches: headline, payload, asker, waiting time and the
 * grant-scope control, with one Approve and one Decline. Sharing the card
 * rather than restating it is what keeps two surfaces from offering different
 * things for one approval, and it means role redaction (#618) reaches here for
 * free — a member sees "not shown to you" here for the same reason they see it
 * on the page.
 *
 * **No batch gesture, deliberately.** There is no "approve all for this run"
 * here: a run holds until *every* parked gate is decided only after #994, and
 * until then run-shaped wording ("the run continues once the remaining N are
 * decided") would be a claim today's behaviour does not honour. Per-node
 * resolve is true now and ships now; the grouping folds in with #994's `batch`
 * field.
 *
 * ## What it does not do
 *
 * It resolves. It does not re-implement resolving: `onDecide` is the shell's
 * one handler, calling the one `resolveApproval` the page calls, per id. No
 * optimistic local state either — a row reads decided only once a verdict has
 * actually been witnessed, so a stale entry can never be offered for a second
 * decision from here.
 */
function RunApprovalsSection({
  rows,
  stillWaiting,
  graph,
  now,
  askerNames,
  deciding,
  failed,
  onDecide,
}: {
  rows: RunApproval[];
  /** How many of them nobody has decided yet. */
  stillWaiting: number;
  graph: WorkflowGraph | null;
  now: number;
  askerNames: Map<string, string>;
  deciding: ReadonlyMap<string, Verdict>;
  failed: Record<string, string>;
  onDecide: (
    approval: ApprovalSummary,
    verdict: Verdict,
    scope: GrantScope,
    /** The operator's answer to a blocker's question (B-046) — see `ApprovalRow`. */
    answer?: string,
  ) => void;
}) {
  const nameById = useMemo(
    () => new Map(graph?.nodes.map((n) => [n.id, n.name]) ?? []),
    [graph],
  );

  return (
    <div
      className="mb-3 rounded-lg border bg-background/40 p-2"
      data-testid="workflow-run-approvals"
      data-still-waiting={stillWaiting}
    >
      <p className="text-xs font-medium">
        {stillWaiting > 0
          ? `Waiting on you — ${stillWaiting} of ${rows.length} still to decide`
          : // Every card answered. Since issue #899 (Stage 1) the last decision
            // continues the run automatically, so this says the run is picking
            // back up — with the caveat, carried by the blocked sentence above,
            // that the re-run may ask again if the agent's turn diverges.
            `All ${rows.length} decided — this run is continuing`}
      </p>
      <div className="mt-1 space-y-1">
        {rows.map((row) => {
          const node = row.nodeId
            ? (nameById.get(row.nodeId) ?? row.nodeId)
            : null;
          return (
            <div key={row.approval.id} data-testid="workflow-run-approval">
              {node && (
                <p className="px-4 text-3xs uppercase tracking-wide text-muted-foreground">
                  {node}
                </p>
              )}
              <ApprovalRow
                approvals={[row.approval]}
                now={now}
                askerNames={askerNames}
                // Narrowed to this row's own approval: a decision in flight on
                // another card of the same run is not this card's business.
                deciding={decidingFor(row.approval.id, deciding)}
                decided={row.verdict ? { [row.approval.id]: row.verdict } : EMPTY_VERDICTS}
                failed={failed}
                onDecide={onDecide}
              />
            </div>
          );
        })}
      </div>
    </div>
  );
}

/** One node's readable result: its name, the producing agent, and its reply
 * (markdown-rendered). Falls back to a subtle placeholder / the branch it took
 * when it produced no text (e.g. a trigger or a condition node). */
function NodeResultCard({ node }: { node: NodeResult }) {
  return (
    <div className="rounded-lg border bg-background/40 p-2">
      <div className="mb-1 flex items-center gap-2">
        <span className="truncate text-xs font-medium">{node.name}</span>
        {node.port !== null && (
          <Badge variant="outline" className="h-4 px-1.5 text-3xs font-normal">
            branch: {node.port}
          </Badge>
        )}
      </div>
      {node.messages.map((m, i) => (
        <div key={i} className={i > 0 ? "mt-2 border-t pt-2" : undefined}>
          {m.agentRef && (
            <p className="mb-1 text-3xs uppercase tracking-wide text-muted-foreground">
              {m.agentRef}
            </p>
          )}
          {m.text ? (
            <div className="prose prose-sm max-w-none dark:prose-invert">
              <ReactMarkdown remarkPlugins={[remarkGfm]}>
                {m.text}
              </ReactMarkdown>
            </div>
          ) : (
            <p className="text-sm text-muted-foreground">—</p>
          )}
        </div>
      ))}
      {node.messages.length === 0 && (
        <p className="text-sm text-muted-foreground">—</p>
      )}
    </div>
  );
}

/** The per-node timeline of a run (issue #542): one row per node that finished,
 * in finish order, showing whether it succeeded and how long it took. Node ids
 * are mapped to their display name from the loaded graph when available. This is
 * a test run's ONLY step-by-step trail, and a real run carries the same rows. */
function NodeTimeline({
  nodes,
  graph,
  undelivered,
}: {
  nodes: WorkflowRunNode[];
  graph: WorkflowGraph | null;
  /** The nodes whose report did not go out (issue #981). */
  undelivered: Set<string>;
}) {
  const nameById = new Map(graph?.nodes.map((n) => [n.id, n.name]) ?? []);
  return (
    <div
      className="mb-3 space-y-1.5 rounded-lg border bg-background/40 p-2"
      data-testid="workflow-run-node-timeline"
    >
      <span className="text-xs font-medium">Steps</span>
      {nodes.map((n, i) => (
        <div key={`${n.nodeId}-${i}`} className="space-y-0.5">
          <div className="flex flex-wrap items-baseline gap-1.5">
            <Badge
              variant="outline"
              /* Issue #881: three tones. The default arm was "anything that is
                 not an error is green", which painted a blocked step — one that
                 produced nothing — exactly like a step that delivered. */
              className={`h-4 px-1.5 text-3xs font-normal ${
                n.status === "error"
                  ? "border-status-failed/40 bg-status-failed-soft"
                  : n.status === "blocked"
                    ? "border-status-blocked/50 bg-status-blocked-soft"
                    : n.status === "declined"
                      ? "border-status-idle/50 bg-status-idle-soft"
                    : "border-status-done/40 bg-status-done-soft"
              }`}
            >
              {n.status}
            </Badge>
            <span className="text-2xs">
              {nameById.get(n.nodeId) ?? n.nodeId}
            </span>
            <span className="text-2xs text-muted-foreground">
              {n.elapsedMs} ms
            </span>
            {/* Issue #981. A SECOND badge rather than a different first one:
                `n.status` answers "did the engine run this step?", and for a
                dropped report the honest answer is `ok` — delivery happens after
                the engine returns, so the node ran and its work stands. What was
                wrong was that this row said only that, next to a run the drawer
                above scored `not delivered`. Two labelled badges state two facts;
                one re-tinted badge would just move the ambiguity. */}
            {undelivered.has(n.nodeId) && (
              <Badge
                variant="outline"
                className="h-4 px-1.5 text-3xs font-normal border-status-failed/40 bg-status-failed-soft"
                data-testid="workflow-run-step-undelivered"
                title="This step ran. Its report did not go out — see Report delivery above."
              >
                not delivered
              </Badge>
            )}
          </div>
          {/* Issue #1014: the engine's own broken-wiring list for this node —
              every config binding that resolved to null, by its config path.
              Paths only (the host forwards no resolved value), so this points
              the operator at *where* a step's wiring came up empty without
              echoing what any node produced. */}
          {n.diagnostics && n.diagnostics.length > 0 && (
            <p
              className="pl-1 text-3xs text-[var(--status-failed-text)]"
              data-testid="workflow-run-node-diagnostics"
            >
              unresolved wiring: {n.diagnostics.join(", ")}
            </p>
          )}
        </div>
      ))}
    </div>
  );
}
