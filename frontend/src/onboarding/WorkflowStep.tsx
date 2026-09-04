import { useCallback, useEffect, useRef, useState } from "react";
import { AlertCircle, ArrowRight, Clock, Loader2, UserCheck } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import {
  listWorkflowRuns,
  listWorkflows,
  type WorkflowRunOutcome,
  type WorkflowSummary,
} from "@/api/workflows";
import { Button } from "@/components/ui/button";
import { startVisiblePolling } from "@/lib/visible-poll";
import {
  gateApprovalTargets,
  gateWorkflowProgress,
  type GateWorkflowProgress,
} from "@/onboarding/workflow-progress";
import { pendingCount } from "@/views/workflows/run-health";

/**
 * How often the step re-reads run history (Codex review, PR #2046). Mirrors
 * `useActivationGate`'s `POLL_MS` — the same cadence the rest of the gate
 * already re-checks itself on, so a run finishing does not read as more or
 * less responsive than the funnel step beside it.
 */
const RUNNING_POLL_MS = 5000;

/**
 * Step 3 of the first-run gate, built for the card it is drawn in (bugs
 * B-003 / B-004 / B-006).
 *
 * **Not `WorkflowsView`.** The gate used to embed that route-level view whole,
 * lazily, inside a checklist card. `WorkflowsView` is a page: a graph canvas, a
 * run-history rail and a floating Copilot panel, all of which size themselves
 * against a full-height route container. Given a ~280px card instead, the graph
 * clipped, the Copilot panel overlapped it, and the Copilot's own prompt text
 * was cut mid-sentence — while ~300px of the actual page sat empty underneath
 * (B-003). A component cannot be reused into a box that cannot give it what it
 * assumes; the honest fix is a different component, not a taller box.
 *
 * It also made every in-app link inside that view inert (B-006): the gate
 * renders instead of the router outlet, so "decide in Approvals" changed the
 * hash and re-rendered the same checklist. Everything this step offers goes out
 * through `onLeave` instead.
 *
 * What it shows is the one thing the step is actually about — whether a run has
 * happened and what it came to. A run parked on an approval is named and linked
 * rather than passed over in silence, which is B-004: the step stays honestly
 * unticked (the host is right that a parked run has proven nothing) but the
 * founder is told why and what would finish it.
 */
export function WorkflowStep({
  client,
  company,
  onOpenWorkflows,
  onOpenApprovals,
}: {
  client: OpenCompanyClient;
  company: string | null;
  /** Leaves the gate for the real Workflows page. */
  onOpenWorkflows: () => void;
  /** Leaves the gate for the real Approvals page. */
  onOpenApprovals: () => void;
}) {
  const [progress, setProgress] = useState<GateWorkflowProgress | null>(null);
  const [names, setNames] = useState<Map<string, string>>(new Map());
  const [failed, setFailed] = useState(false);

  // Codex + CodeRabbit review, PR #2046: shared by EVERY run-history read
  // this component issues — the initial mount read and every poll tick alike
  // — not a counter local to the poll's own loop. Without that, an older
  // response from one path (say, the initial read, if it happens to be
  // slower than a poll tick that started after it) could still overwrite a
  // newer response from the other path, because each path only checked
  // itself for staleness. One `useRef`, mirroring `useActivationGate`'s own
  // `generation` ref, makes "is this still the latest request" a single
  // question with a single answer no matter which effect asked it.
  const latestRunsRequest = useRef(0);
  // Codex review, PR #2046: guards against a HOST that consistently answers
  // slower than `RUNNING_POLL_MS`. Without it, every tick still increments
  // `latestRunsRequest` before the PREVIOUS tick's request lands, so every
  // single response arrives already stale by the "latest issued" check above
  // and gets discarded forever — the card would sit on its first spinner
  // permanently while requests piled up unboundedly underneath it. Mirrors
  // `useActivationGate`'s own `inFlight` ref for the identical shape of
  // problem: skip starting a new request while one is still outstanding,
  // rather than trying to out-race it.
  const runsInFlight = useRef(false);
  const fetchRuns = useCallback(() => {
    if (runsInFlight.current) return;
    runsInFlight.current = true;
    const requestId = ++latestRunsRequest.current;
    void listWorkflowRuns(client, company, { limit: 5 })
      .then(
        (page) => {
          if (requestId !== latestRunsRequest.current) return;
          setProgress(gateWorkflowProgress(page.runs));
          setFailed(false);
        },
        () => {
          if (requestId !== latestRunsRequest.current) return;
          setFailed(true);
        },
      )
      .finally(() => {
        // Guarded the same way: a stale request finishing late must not clear
        // the flag out from under whichever request is now current.
        if (requestId === latestRunsRequest.current) runsInFlight.current = false;
      });
  }, [client, company]);

  useEffect(() => {
    let live = true;
    // A fresh (client, company) starts its own request regardless of whether
    // a now-irrelevant one from the PREVIOUS company is still outstanding —
    // `fetchRuns`' own `requestId` check keeps that old response from ever
    // being applied once it does land.
    runsInFlight.current = false;
    fetchRuns();
    // A host that cannot list workflows (or predates the route, which answers
    // 404) should cost this step its labels, not its answer — so a rejection
    // here is silently left as the fallback `name()` already renders. Kept as
    // its own independent read, not `Promise.allSettled` on a shared await
    // (CodeRabbit review, PR #2046): the run history is the load-bearing
    // half, and a slow (not merely failed) workflow-name lookup must not hold
    // the founder on "Checking your runs…" once run history has answered.
    void listWorkflows(client, company).then((workflows) => {
      if (!live) return;
      setNames(new Map(workflows.map((w: WorkflowSummary) => [w.id, w.name] as const)));
    }, () => {});
    return () => {
      live = false;
    };
  }, [client, company, fetchRuns]);

  // Codex review, PR #2046 (three rounds of the same shape of finding): a run
  // can change from OUTSIDE this mount at any point before it reaches an
  // outcome this card will never revise on its own — a run finishes, another
  // tab/operator decides an approval or strands it, or the founder starts a
  // NEW run entirely while this card is still showing "No run yet" or an
  // older one's outcome. Nothing here re-fetches by itself, and the
  // activation poll cannot unmount this step for a company that has not (yet)
  // activated — so a step that only polled some kinds and not others kept
  // reappearing as a fresh finding, one kind at a time. Simplest fix that
  // actually closes the class: poll unconditionally for as long as this step
  // is mounted, the same way `useActivationGate` does — `fetchRuns`'s own
  // in-flight guard above is what keeps that cheap on a slow host, and this
  // step un-mounts on its own once the founder moves past it.
  useEffect(() => {
    // CodeRabbit review, PR #2046: a tick fires the next read without waiting
    // for the previous one to have committed its state update, so two
    // requests could be in flight together — the shared `latestRunsRequest`
    // ref above (not a counter local to this effect) is what stops an OLDER
    // one that resolves LATER from overwriting a NEWER response, from this
    // effect or the mount effect alike.
    return startVisiblePolling(fetchRuns, RUNNING_POLL_MS);
  }, [fetchRuns]);

  const label = (run: WorkflowRunOutcome | undefined) =>
    (run && names.get(run.workflowId)) ?? run?.workflowId ?? "your workflow";

  return (
    <div className="space-y-4" data-testid="gate-workflow-step">
      {progress === null && !failed && (
        <p className="flex items-center gap-2 text-sm text-muted-foreground">
          <Loader2 aria-hidden className="size-4 animate-spin" />
          Checking your runs…
        </p>
      )}

      {failed && (
        <p className="text-sm text-muted-foreground">
          Couldn&apos;t read this company&apos;s run history just now. Open Workflows to run
          one and watch it there.
        </p>
      )}

      {progress && <ProgressLine progress={progress} name={label(progress.run)} />}

      <div className="flex flex-wrap items-center gap-2">
        <Button onClick={onOpenWorkflows} data-testid="gate-workflow-open">
          Open Workflows
          <ArrowRight className="size-4" />
        </Button>
        {progress?.kind === "waiting-on-you" && gateApprovalTargets(progress.run).length > 0 && (
          <Button
            variant="outline"
            onClick={onOpenApprovals}
            data-testid="gate-workflow-open-approvals"
          >
            <UserCheck className="size-4" />
            Decide it in Approvals
          </Button>
        )}
      </div>
    </div>
  );
}

function ProgressLine({
  progress,
  name,
}: {
  progress: GateWorkflowProgress;
  name: string;
}) {
  const shell = (icon: React.ReactNode, body: React.ReactNode, testId: string) => (
    <p
      className="flex items-start gap-2 rounded-lg border bg-muted/40 px-3 py-2 text-sm text-muted-foreground"
      data-testid={testId}
    >
      {icon}
      <span>{body}</span>
    </p>
  );

  switch (progress.kind) {
    case "none":
      return (
        <p className="text-sm text-muted-foreground" data-testid="gate-workflow-none">
          No run yet. Open Workflows, pick one, and press Run — this step ticks when a run
          finishes.
        </p>
      );
    case "running":
      return shell(
        <Loader2 aria-hidden className="mt-0.5 size-4 shrink-0 animate-spin" />,
        <>
          <span className="font-medium text-foreground">{name}</span> is still running. This
          step ticks when it finishes.
        </>,
        "gate-workflow-running",
      );
    case "waiting-on-you": {
      // B-004: the sentence that was missing. It says the run happened, why it
      // did not count, and what closes it — rather than leaving the founder to
      // conclude the button did nothing.
      //
      // `blocked` gets its own sentence: `WorkflowRunOutcome.blockedNodes`
      // (frontend/src/api/workflows.ts) is explicit that an agent node is not
      // re-enterable, so deciding the card does NOT continue this run — the
      // operator still has to run the workflow again. Promising "the run
      // carries on" for that case would be a claim the host never makes.
      if (progress.verdict === "blocked") {
        // Codex review, PR #2046: a blocked node's `approvalIds` is absent
        // entirely when every one of its gated calls failed to park
        // (`parkFailed`/discarded) — the same "unparkable" shape
        // `RunHistoryPanel` already special-cases — not merely present-but-
        // stranded. `gateApprovalTargets` already reads `[]` for that; when
        // it does, there is nothing to "decide" at all, so the sentence must
        // not invite the founder to decide a card that was never queued.
        if (gateApprovalTargets(progress.run).length === 0) {
          return shell(
            <AlertCircle aria-hidden className="mt-0.5 size-4 shrink-0" />,
            <>
              <span className="font-medium text-foreground">{name}</span> stopped on a step
              that couldn&apos;t be queued for approval at all, so this step hasn&apos;t
              ticked and there&apos;s nothing here to decide. Open Workflows to see why,
              then run it again.
            </>,
            "gate-workflow-blocked-unparkable",
          );
        }
        return shell(
          <UserCheck aria-hidden className="mt-0.5 size-4 shrink-0" />,
          <>
            <span className="font-medium text-foreground">{name}</span> ran and stopped to
            ask you something — it&apos;s waiting on an approval, so it hasn&apos;t
            finished yet and this step hasn&apos;t ticked. Decide the approval, then run
            it again to finish.
          </>,
          "gate-workflow-blocked",
        );
      }
      // Codex review, PR #2046: `awaiting-approval` is not only a run paused
      // mid-flight at a gate node — `awaitingCount` (frontend/src/views/
      // workflows/run-health.ts) also counts a pending DELIVERY, and that is a
      // different mechanism wearing the same verdict. `DeliveryStatus`'s own
      // doc says `pending` is a SNAPSHOT taken once the run already finished:
      // nothing comes back to flip it, deciding it only sends the report, and
      // the journal keeps scoring that same run as not-`ok` regardless (this
      // step still needs a clean rerun to tick). `DeliveryReport` carries no
      // id either, so `gateApprovalTargets` correctly offers nothing to link
      // Approvals to — used here as the same signal, rather than inventing a
      // second predicate the two could drift apart on.
      {
        const hasTarget = gateApprovalTargets(progress.run).length > 0;
        const hasPendingDelivery = pendingCount(progress.run?.deliveries ?? []) > 0;
        if (!hasTarget) {
          return shell(
            <UserCheck aria-hidden className="mt-0.5 size-4 shrink-0" />,
            <>
              <span className="font-medium text-foreground">{name}</span> ran, but a report
              it produced is still waiting on your approval to send — deciding that only
              sends the report, so this step hasn&apos;t ticked. Open Workflows and run it
              again if you want this step to tick.
            </>,
            "gate-workflow-awaiting-delivery",
          );
        }
        // Codex review, PR #2046: a run CAN carry both a live gate approval
        // and a pending delivery at once — a parallel branch that already
        // reached an output node while another is still paused at a gate.
        // `hasTarget` alone used to pick this sentence and its unqualified
        // "the run carries on" — true of the gate approval Approvals links
        // to, but the SAME sentence would be read as covering the delivery
        // too, and deciding that one does not continue anything (same reason
        // as the delivery-only case above). Naming both keeps the promise
        // scoped to the part of it that is actually true.
        if (hasPendingDelivery) {
          return shell(
            <UserCheck aria-hidden className="mt-0.5 size-4 shrink-0" />,
            <>
              <span className="font-medium text-foreground">{name}</span> ran and stopped to
              ask you something — deciding the approval in Approvals carries that part of
              the run on. It also produced a report still waiting on a separate approval to
              send; deciding that one only sends the report and won&apos;t finish this step
              on its own.
            </>,
            "gate-workflow-waiting-mixed",
          );
        }
        return shell(
          <UserCheck aria-hidden className="mt-0.5 size-4 shrink-0" />,
          <>
            <span className="font-medium text-foreground">{name}</span> ran and stopped to
            ask you something — it&apos;s waiting on an approval, so it hasn&apos;t
            finished yet and this step hasn&apos;t ticked. Decide the approval and the run
            carries on.
          </>,
          "gate-workflow-waiting",
        );
      }
    }
    case "needs-rerun":
      return shell(
        <AlertCircle aria-hidden className="mt-0.5 size-4 shrink-0" />,
        <>
          <span className="font-medium text-foreground">{name}</span> stopped on an approval
          that is no longer in the queue, so it can&apos;t be continued. Run it again.
        </>,
        "gate-workflow-needs-rerun",
      );
    case "did-not-finish":
      return shell(
        <AlertCircle aria-hidden className="mt-0.5 size-4 shrink-0" />,
        <>
          The last run of <span className="font-medium text-foreground">{name}</span> ended{" "}
          <span className="font-medium text-foreground">{progress.verdict}</span> rather than
          finishing cleanly, so this step hasn&apos;t ticked. Open Workflows to see what
          happened.
        </>,
        "gate-workflow-did-not-finish",
      );
    case "succeeded":
      return shell(
        <Clock aria-hidden className="mt-0.5 size-4 shrink-0" />,
        <>
          <span className="font-medium text-foreground">{name}</span> finished. This step
          ticks as soon as the console re-reads your setup.
        </>,
        "gate-workflow-succeeded",
      );
  }
}
