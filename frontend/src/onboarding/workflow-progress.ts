// What the gate's "Run a workflow" step should say about the runs it can see
// (bug B-004).
//
// The step ticks on `status.workflowRunSucceeded`, and that stays exactly as it
// is: `src/company/activation.rs` routes the question through the shared
// verdict ladder on purpose, because a run parked on an approval carries
// neither an error nor a cancellation and would otherwise latch the step —
// permanently — before anything had actually run to completion. A run that has
// not finished has not proven the company works, which is the whole claim the
// step makes.
//
// The bug was never the condition. It was the silence: the shipped
// `agentic-design-studio` bundle's most obvious first workflow ("Research
// request") escalates to a human on its first turn, so a founder who does
// exactly what the step asks watches the run park and the checklist not move,
// with nothing anywhere saying the run did not count or what would make it.
//
// So this classifies what the founder is actually looking at, and the step
// renders the answer. It is deliberately a pure function over rows the console
// already fetches — the tone ladder it defers to (`verdictOf`) is the same one
// the Steps panel, the run traces list and the host all read, so this can never
// disagree with the run's own badge two clicks away.

import type { WorkflowRunOutcome, WorkflowRunVerdict } from "@/api/workflows";
import { strandedApprovalCount, verdictOf } from "@/views/workflows/run-health";

export type GateWorkflowProgressKind =
  /** No run has been recorded for this company at all. */
  | "none"
  /** A run is in flight right now. */
  | "running"
  /** Parked on a person: approving is what finishes it. */
  | "waiting-on-you"
  /** Parked on approvals that have since left the queue — only a re-run helps. */
  | "needs-rerun"
  /** Reached a terminal state that is not success. */
  | "did-not-finish"
  /** Actually succeeded (the step should be ticked by the host). */
  | "succeeded";

export interface GateWorkflowProgress {
  kind: GateWorkflowProgressKind;
  /** The run being described — the most recent one, or absent for `none`. */
  run?: WorkflowRunOutcome;
  /** That run's verdict, so the caller can label it the way every other reader does. */
  verdict?: WorkflowRunVerdict;
}

/**
 * Describes the **most recent** run, not the best one.
 *
 * The founder just pressed Run and is looking at the checklist wondering why it
 * did not move; the run they are asking about is the one they just started.
 * Picking the newest *successful* run instead would answer a question nobody
 * asked, and picking the worst would nag about history they have already moved
 * past.
 *
 * `runs` is expected newest-first, which is `listWorkflowRuns`' documented
 * order (issue #228/#1012). Sorting again here would be a second, driftable
 * transcription of an order the host already guarantees — but an empty list and
 * a missing list are both simply "nothing to say".
 */
export function gateWorkflowProgress(
  runs: readonly WorkflowRunOutcome[] | undefined,
): GateWorkflowProgress {
  const run = runs?.[0];
  if (!run) return { kind: "none" };
  const verdict = verdictOf(run);
  return { kind: kindFor(verdict), run, verdict };
}

function kindFor(verdict: WorkflowRunVerdict): GateWorkflowProgressKind {
  switch (verdict) {
    case "running":
      return "running";
    // Both mean "a person has to act, and the step stays unticked until they
    // do" — the same kind so the button and the unticked checklist agree for
    // either. They do NOT always get the same sentence: `WorkflowRunOutcome
    // .blockedNodes` says a blocked run's agent node is not re-enterable, and
    // `awaiting-approval` itself can come from a pending DELIVERY rather than
    // a live gate — neither of which deciding actually continues the run the
    // way deciding a genuine gate approval does. `WorkflowStep` reads
    // `progress.verdict` and `gateApprovalTargets` to pick the honest wording.
    case "awaiting-approval":
    case "blocked":
      return "waiting-on-you";
    // Issue #1189's case, and it must NOT be sent to Approvals: the cards are
    // gone from the queue, so the page it would link to is empty and the only
    // thing that helps is running the workflow again. Pointing a founder at an
    // empty queue and calling it an action is the defect this whole task is
    // about, one screen over.
    case "stranded":
      return "needs-rerun";
    case "ok":
      return "succeeded";
    // `degraded` lands here deliberately: the run finished and its output is
    // valid, but the host did not count it as `ok`, so the step is not ticked
    // and saying "it worked" would contradict the checklist the founder is
    // staring at.
    case "failed":
    case "stopped":
    case "undelivered":
    case "degraded":
      return "did-not-finish";
  }
}

/**
 * The approval ids this run is waiting on that are still worth linking to.
 *
 * Mirrors `BlockedNodeApprovals`' own rule (issue #1143/#1189): a node whose
 * every card has been stranded has nothing to link to, so its ids are dropped
 * rather than offered as an action that lands on an empty page.
 *
 * CodeRabbit review, PR #2046: that rule used to apply only to `blockedNodes`
 * — `run.pendingApprovals` (the top-level gate-approval receipt) was unioned
 * in UNFILTERED, with no equivalent check against `run.strandedApprovals`.
 * The gap is invisible for a run that is PURELY stranded, because
 * `workflow_verdict.rs` scores that `stranded` outright and `kindFor` above
 * never reaches this function for it (`"stranded"` maps to `"needs-rerun"`,
 * not `"waiting-on-you"`) — but the host deliberately does NOT call a run
 * `stranded` while it ALSO carries a pending delivery (`workflow_verdict.rs`
 * lines 929-947: the delivery is still genuinely actionable), so a run with
 * every gate approval stranded AND a pending delivery reads `awaiting-approval`
 * — landing exactly on `"waiting-on-you"` with `pendingApprovals` ids this
 * function returned as if they were live. Since the host reports COUNTS, not
 * which specific ids survived, the same all-or-nothing rule `blockedNodes`
 * already uses is the only one available here: if every entry could be
 * stranded, drop the whole list rather than guess which one is not.
 */
export function gateApprovalTargets(run: WorkflowRunOutcome | undefined): string[] {
  if (!run) return [];
  const stranded = strandedApprovalCount(run);
  const topLevel = stranded >= run.pendingApprovals.length ? [] : run.pendingApprovals;
  const fromNodes = (run.blockedNodes ?? []).flatMap((node) => {
    const ids = node.approvalIds ?? [];
    const nodeStranded = node.stranded ?? 0;
    return nodeStranded >= ids.length ? [] : ids;
  });
  return Array.from(new Set([...topLevel, ...fromNodes]));
}
