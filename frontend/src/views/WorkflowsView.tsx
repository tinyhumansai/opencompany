import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  Background,
  BackgroundVariant,
  Controls,
  type Node,
  ReactFlow,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import { useTheme } from "next-themes";
import {
  ArrowLeft,
  Bot,
  ChevronDown,
  FlaskConical,
  History,
  LayoutGrid,
  List as ListIcon,
  Loader2,
  Pause,
  Pencil,
  Play,
  Plug,
  Plus,
  Power,
  RotateCw,
  Square,
  Trash2,
  Workflow as WorkflowIcon,
} from "lucide-react";
import { toast } from "sonner";

import {
  cancelWorkflowRun,
  deleteWorkflow,
  fixWorkflowFromRun,
  getWorkflow,
  isDetached,
  isDryRun,
  listWorkflowRuns,
  listWorkflows,
  runWorkflow,
  setWorkflowEnabled,
  type PrefilledDraft,
  type WorkflowGraph,
  type WorkflowRunOutcome,
  type WorkflowRunOutputRecord,
  type WorkflowRunResult,
  type WorkflowSummary,
  workflowRunOutput,
} from "@/api/workflows";
import type { CompanyStreamEvent } from "@/hooks/use-events";
import { withHostParam } from "@/hooks/use-host-route";
import type { OpenCompanyClient } from "@/api/client";
import { ApiError } from "@/api/types";
import type {
  ApprovalSummary,
  GrantScope,
  NotificationDto,
  TeamMemberDto,
  Verdict,
} from "@/api/types";
import { Week1NudgeBanner } from "@/components/week1-nudge-banner";
import { pickActiveNudge, WEEK1_NUDGE_KIND } from "@/lib/week1-nudge";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "@/components/ui/alert-dialog";
import { PageHeader } from "@/components/page-header";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button, buttonVariants } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import { WorkflowNode } from "@/components/workflow-node";
import { WorkflowCreateDialog } from "@/views/WorkflowCreateDialog";
// Issue #1002: the run drawer decides a run's parked cards in place, using the
// same shared approval card the Approvals page and the inline chat card use.
import { useAskerNames } from "@/components/approval-card";
import type { DecidedApproval } from "@/views/chat/model";
import { cn } from "@/lib/utils";
import { startVisiblePolling } from "@/lib/visible-poll";
import type { NodeRunState } from "@/lib/workflow-sample";
import { workflowSavedToast } from "@/lib/workflow-saved-toast";
// Issue #303: the canvas arithmetic, the run-state folds and the three drawers
// moved out when this file passed 1800 lines and was about to grow an index and
// a copilot. See `workflows/graph.ts` for why the fold is pure.
import {
  elapsedFromRun,
  failedNodeOf,
  failureLocation,
  foldLiveRun,
  initialRunState,
  layout,
  LEGIBLE_FIT_ZOOM,
  nodeName,
  statesFromRun,
  windowHasRunStart,
} from "@/views/workflows/graph";
import { WorkflowMiniMap } from "@/views/workflows/WorkflowMiniMap";
import { WorkflowZoomReadout } from "@/views/workflows/WorkflowZoomReadout";
// Issue #1361: opens a long pipeline at a zoom its node titles survive.
import { FitGraphToPane } from "@/views/workflows/FitGraphToPane";
// Issue #1231: keeps the inspector from opening on top of the node it describes.
import {
  RevealSelectedNode,
  type RevealSelectedNodeHandle,
} from "@/views/workflows/RevealSelectedNode";
import { LastRunChip, RunHistoryPanel } from "@/views/workflows/RunHistoryPanel";
import { WorkflowIndex, type IndexMode } from "@/views/workflows/WorkflowIndex";
import { RunTracesList } from "@/views/workflows/RunTracesList";
import { RunTraceSheet } from "@/views/workflows/RunTraceSheet";
import { CopilotPanel } from "@/views/workflows/CopilotPanel";
import { classifyRunError } from "@/views/workflows/run-error";
import { runFailureFrom, type RunFailure } from "@/views/workflows/run-failure";
import { RunFailurePanel } from "@/views/workflows/RunFailurePanel";
import { RunResultPanel } from "@/views/workflows/RunResultPanel";
import { CanvasShell } from "@/views/workflows/CanvasShell";
import { approvalsForRun } from "@/views/workflows/run-approvals";
// Issue #981: which nodes produced a report that never went out, so the canvas
// card can say so beside the DONE badge instead of leaving it to a banner.
import { undeliveredNodes } from "@/views/workflows/run-health";
import { NodeDetailPanel } from "@/views/workflows/NodeDetailPanel";
import { type NodeOutputView, nodeOutputFor } from "@/views/workflows/run-output";

const NODE_TYPES = { oc: WorkflowNode };

/** Stable empty defaults, so an omitted approvals prop cannot churn renders. */
const EMPTY_APPROVALS: ApprovalSummary[] = [];
const EMPTY_DECIDING: ReadonlyMap<string, Verdict> = new Map();
const EMPTY_DECIDED: Record<string, DecidedApproval> = {};
const EMPTY_FAILED: Record<string, string> = {};

/** A stable empty default for `runEvents`, so an omitted prop does not hand the
 * fold a fresh array identity on every render. */
const EMPTY_RUN_EVENTS: CompanyStreamEvent[] = [];

/** A stable empty set for the canvas's undelivered-node marks (issue #981), so
 * the common case — a run that delivered what it routed — hands `layout` the
 * same identity every render and does not re-lay the graph out. */
const EMPTY_UNDELIVERED: Set<string> = new Set();

/** `decodeURIComponent` that survives a hand-edited address bar. A lone `%`
 * makes it throw, and a malformed hash must not take the whole view down —
 * comparing the raw segment simply fails to match, which is the right outcome. */
function safeDecode(value: string): string {
  try {
    return decodeURIComponent(value);
  } catch {
    return value;
  }
}

/**
 * Decompose the hash this view owns: `#/workflows/<workflowId>?run=<runId>`
 * (issue #339).
 *
 * **The run id lives in a query string, not a third path segment, and that is
 * load-bearing — do not "tidy" it into `#/workflows/<id>/runs/<runId>`.**
 * `useHashView`'s `canonicalize()` runs on every `hashchange` and rewrites the
 * hash to at most `head/sub`, so a third segment is silently dropped and the
 * run id would never survive the first hash event. What saves the query is that
 * the same function reads the path with everything after `?` stripped and
 * early-returns once the two segments already match — so it looks at
 * `workflows/<id>`, finds it identical, and leaves the URL alone, query and
 * all. Two segments plus a query is the only shape that round-trips.
 *
 * Read from `location` rather than from the `sub` prop because the query is
 * invisible to the router, and because the view's hash writer needs the
 * *current* URL — not the router's copy of it, which lags a `replaceState` it
 * never hears about — to know whether a write would be a no-op.
 */
function readWorkflowHash(): {
  /** Whether the hash currently names this view at all. */
  onWorkflows: boolean;
  workflowId: string | null;
  runId: string | null;
} {
  const [path, query] = window.location.hash.replace(/^#\/?/, "").split("?");
  const [head, id] = path.split("/").filter(Boolean);
  return {
    onWorkflows: head === "workflows",
    workflowId: id ? safeDecode(id) : null,
    runId: query ? new URLSearchParams(query).get("run") : null,
  };
}

/**
 * Rewrite `#/workflows/<id>` back to `#/workflows`, in place (issue #1110).
 *
 * `replaceState`, never a push, and that is the whole reason this is a function
 * rather than an assignment at each call site: every caller is the view
 * *correcting* the URL — a workflow the operator deleted, one another session
 * deleted, one a link named that this company does not have. Pushing any of
 * those would offer Back as a route to a workflow that is gone.
 *
 * Silent when the hash does not name a workflow, so a call made while another
 * view owns the hash (a company switch mid-navigation) cannot drag the operator
 * back here.
 */
function clearWorkflowFromHash(): void {
  const { onWorkflows, workflowId } = readWorkflowHash();
  if (!onWorkflows || workflowId === null) return;
  // `withHostParam` because this replaces the hash rather than editing it, and
  // a replace fires no `hashchange` — so a connection scope dropped here has
  // nothing to put it back (`use-host-route.ts`).
  window.history.replaceState(null, "", withHostParam("workflows"));
}

/**
 * A hairline between two groups of toolbar controls (issue #1135).
 *
 * The row it sits in holds three groups — run intent, the secondary actions,
 * and the two that change the workflow itself — and a uniform `gap-2` between
 * nine buttons says nothing about which of them belong together. The rule is
 * decoration only in the sense that it draws nothing new: it makes a grouping
 * that is already true in the markup visible, which is what stopped `Run`
 * reading as one pill among six and `Delete` as the neighbour of `Edit`.
 */
function ToolbarDivider() {
  return <span aria-hidden className="mx-1 h-5 w-px shrink-0 bg-border" />;
}

/**
 * The live Workflows canvas. Reads the company's saved graphs from the host's
 * `…/workflows` routes, lets the operator pick one, renders its real nodes and
 * edges (auto-laid-out left→right by longest-path depth, since saved graphs
 * carry no coordinates), and runs it via `…/workflows/{wid}/run` — surfacing the
 * engine's final output and any nodes left pending approval.
 */
export function WorkflowsView({
  client,
  company,
  sub = null,
  runEventTick = 0,
  runEvents = EMPTY_RUN_EVENTS,
  listEventTick = 0,
  approvals = EMPTY_APPROVALS,
  approvalsNow,
  decidingApprovals = EMPTY_DECIDING,
  decidedApprovals = EMPTY_DECIDED,
  failedApprovals = EMPTY_FAILED,
  onDecideApproval,
}: {
  client: OpenCompanyClient;
  company: string | null;
  /**
   * The second hash segment — `#/workflows/<workflowId>` (issue #339), so a
   * finished task card can link to the workflow it built or ran and land on
   * that graph rather than on whichever one happens to sort first.
   *
   * Unvalidated, as the router documents: only this view knows which workflow
   * ids exist, so it resolves the id against the loaded list itself and falls
   * back to its own default when the id names nothing.
   */
  sub?: string | null;
  /**
   * Bumped by the shell on every `workflow_run_finished` SSE event (issue
   * #228), so a run that finishes while this tab is open shows up without a
   * reload. Matters most for a scheduled run — nothing else would tell this
   * view a cron fired.
   */
  runEventTick?: number;
  /**
   * A rolling window of workflow run frames off the SSE stream (issue #371) —
   * runs starting, nodes finishing, runs settling.
   *
   * Unlike `runEventTick` these carry the payload, because the canvas paints
   * per-node state and a counter cannot say which node of which run just
   * finished. The two coexist: the tick still drives the history refetch.
   *
   * A **window**, not a latest-event slot: two frames routinely arrive inside
   * one React batch, and a slot drops the earlier one. The canvas state is
   * folded from the window rather than accumulated frame by frame, so a
   * coalesced render can never lose a node.
   */
  runEvents?: CompanyStreamEvent[];
  /**
   * Bumped by the shell on every frame that changes what the picker should show
   * — `workflow_created`, `workflow_updated`, `workflow_deleted` (issue #384)
   * and `workflow_enabled_changed` (issue #276) — so the picker follows what the
   * host holds while this tab stays open, including the armed state.
   *
   * Distinct from `runEventTick`, which is about what a run did. A workflow the
   * orchestrator authored (or a second session deleted) changes which entries
   * exist, and nothing else would tell this view that.
   *
   * A counter, not the payload: it re-reads the list, and the list is the
   * answer to all three frames — including the delete, which is simply an entry
   * the fresh list no longer has.
   */
  listEventTick?: number;
  /**
   * The company's parked approvals — the **whole** queue (issue #1002), passed
   * straight through to the run drawer, which narrows it to the run on screen.
   *
   * The same array the Approvals page renders and the sidebar badge counts, and
   * that is the point: this view is a second *reader* of one queue, so it can
   * never make the page show less or the badge disagree with it. Live, because
   * the feed refreshes it on every `approval_resolved` frame — which is what
   * stops a run drawer calling a step blocked after somebody else cleared it.
   */
  approvals?: ApprovalSummary[];
  /** The feed's clock, for the cards' "waiting N minutes" line. */
  approvalsNow?: number;
  /** The verdict an approval is waiting on, keyed by id (issue #1002). */
  decidingApprovals?: ReadonlyMap<string, Verdict>;
  /** Verdicts already witnessed, with their last-seen summary (issue #1002). */
  decidedApprovals?: Record<string, DecidedApproval>;
  /** Decisions that did not land, keyed by approval id (issue #1002). */
  failedApprovals?: Record<string, string>;
  /**
   * Record one decision on one approval — the shell's handler, which calls the
   * same per-id `resolveApproval` the Approvals page calls (issue #1002).
   *
   * Absent when the caller has not wired the surface, in which case the drawer
   * renders exactly as it did before #1002: it says the run parked cards and
   * points at the queue, without offering to decide them.
   */
  onDecideApproval?: (
    approval: ApprovalSummary,
    verdict: Verdict,
    scope: GrantScope,
  ) => void;
}) {
  const { resolvedTheme } = useTheme();
  const [workflows, setWorkflows] = useState<WorkflowSummary[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  // The current selection, readable inside async callbacks whose closure captured
  // a stale `selectedId` (issue #840 PR-3: guards the copilot-fix race).
  const selectedIdRef = useRef<string | null>(null);
  selectedIdRef.current = selectedId;
  // Issue #1089: guards the company-switch race in the Resume handler — the same
  // pattern as selectedIdRef: captured in the toast closure, checked after await.
  const companyRef = useRef<string | null>(company);
  companyRef.current = company;
  // Issue #1704 (review): how many times the selection has been torn down.
  //
  // The two refs above answer "where are we now", which is not the same question
  // as "did we leave and come back". On A → B → A the identity checks match
  // again, and that round trip is reachable: the switch to B clears
  // `fixingRunSeq`, which re-enables Fix, so the operator can retry the SAME
  // failed run while the first request is still in flight. The first reply would
  // then pass an identity-only guard, overwrite the retry's verdict, and clear
  // the spinner out from under a request that is still running.
  //
  // Bumped by the cleanup effect below — after commit, so it is not moved by a
  // render React discards — and captured by `handleFixWithCopilot` when the
  // request starts. It is checked ALONGSIDE the identity refs rather than
  // instead of them: those are assigned during render, so between a commit and
  // the passive effect that bumps this counter they are the only two that have
  // noticed the switch.
  const selectionGenRef = useRef(0);
  const [graph, setGraph] = useState<WorkflowGraph | null>(null);
  const [loadingList, setLoadingList] = useState(true);
  const [loadingGraph, setLoadingGraph] = useState(false);
  const [result, setResult] = useState<WorkflowRunResult | null>(null);
  // Issue #1007: the run POST that was rejected, held on screen.
  //
  // `result` is the drawer for a run that produced something, and it can only
  // be set from a settled body — which on the failure path never arrives. So a
  // failed run mounted no surface at all and the console returned to its
  // resting state behind a four-second toast. This is that outcome's drawer,
  // and it is state rather than a toast for the same reason `conflict` and
  // `runRefusal` are: reading it, finding the history row, and fixing the graph
  // all take longer than a toast lasts.
  const [runFailure, setRunFailure] = useState<RunFailure | null>(null);
  // Issue #1007: the dispatch this console is waiting on — when Run was pressed,
  // and whether it was a test run.
  //
  // What acknowledges the click in the history drawer. Between pressing Run and
  // the host journaling a row there was nothing there at all, which for a run
  // that takes minutes is most of its life. Rendered as one optimistic row, and
  // dropped the moment the host's own row for the same run lands — that row
  // carries the per-node trail this one cannot.
  const [pendingRun, setPendingRun] = useState<{
    startedAtMillis: number;
    dryRun: boolean;
  } | null>(null);
  // Issue #154: what the operator is asking this run to work on. `ranWith` is
  // pinned when the run is dispatched so the result panel echoes the request the
  // shown output came from, not whatever has been typed since.
  //
  // Issue #1204: `request` is the DRAFT held by the "Run with input" dialog, not
  // an implicit argument to running. It used to be a text box wired straight
  // into `run()`'s closure, sitting open on the toolbar; taking the box off the
  // bar without also cutting that wire would have been worse than leaving it
  // there, because a draft typed once and then forgotten would keep riding
  // along on every later press of Run with nothing on screen to say so. So the
  // payload is a PARAMETER of `run()` now (see below) and this state is only
  // ever read at the moment a dialog button is pressed. It survives the dialog
  // closing on purpose — reopening restores what was typed, which is what makes
  // "run it again with a tweak" one edit rather than a retype.
  const [request, setRequest] = useState("");
  const [ranWith, setRanWith] = useState("");
  // Issue #1204: whether that dialog is up.
  const [runInputOpen, setRunInputOpen] = useState(false);
  // Issue #1002: the roster read behind the cards' "Asked by" line, keyed to the
  // approvals of the run on screen rather than to the whole queue — so a console
  // with a run drawer closed does not fetch the roster for cards it is not
  // rendering. `useAskerNames` itself is keyed on the asker id set, so this stays
  // one read per company rather than one per poll.
  const runApprovalCards = useMemo(
    () => approvalsForRun(approvals, result?.runId),
    [approvals, result?.runId],
  );
  const askerNames = useAskerNames(client, company, runApprovalCards);
  // Issue #1704 (review): two load failures, two slots — they were one, and one
  // was not enough to describe either honestly.
  //
  // `listError` is a COMPANY-wide condition: the workflow list would not load.
  // `graphError` is about ONE workflow: its graph would not load. Sharing a slot
  // meant a successful list read cleared a graph failure and vice versa, and it
  // meant a selection change had to choose between two wrong answers — leave a
  // graph failure up on the index it does not describe, or wipe a list failure
  // exactly as the operator returns to the stale list it is about.
  //
  // So the lifetimes differ, and now they can: `graphError` is cleared by every
  // selection change, `listError` only by a COMPANY change (the axis its own
  // fetch is keyed on) or by a list read that succeeds.
  const [listError, setListError] = useState<string | null>(null);
  const [graphError, setGraphError] = useState<string | null>(null);
  // Issue #1845: the week-1 "save your first workflow" nudge, server-backed
  // (`GET …/notifications?kind=workflow_nudge`) rather than the tour's
  // `localStorage` flag — a signup earns this from `LifecycleScheduler` on the
  // host, and dismissing/creating persists back through `markNotificationsRead`
  // rather than a client-only flag, so it survives a reload.
  const [nudge, setNudge] = useState<NotificationDto | null>(null);
  // Readable from callbacks that must stay stable (`handleCreated`'s own
  // deps), the same pattern `selectedIdRef`/`companyRef` use above.
  const nudgeRef = useRef<NotificationDto | null>(null);
  nudgeRef.current = nudge;
  // Issue #1845 (review: PR #1878): set the moment `handleCreated` fires,
  // before `refreshNudge` below has necessarily resolved. A fetch already in
  // flight when a local create happens (the mount fetch, or a poll tick) can
  // land AFTER `handleCreated`, carrying the row the scheduler filed before
  // this create — `clearNudge` could not have marked it read yet, because at
  // that moment it did not know the row's id (`nudgeRef.current` was still
  // `null`). This nudge is a one-time, first-workflow-only ask (the
  // scheduler's own idempotency ledger never files a second one), so once
  // this session has created a workflow, no fetch response — stale or not —
  // should ever put the banner back up. Reset on a company switch, since a
  // create in one company says nothing about another.
  const hasCreatedLocallyRef = useRef(false);
  // codex review finding (comment 3892534919): `hasCreatedLocallyRef` only
  // ever invalidates a stale `refreshNudge` response against a LOCAL CREATE.
  // Dismissal (`clearNudge` below) had no equivalent — a poll already in
  // flight when the operator clicks Dismiss can resolve afterward carrying
  // the same row, still unread from the server's point of view at the
  // instant that response was captured, and `setNudge(active)` would put the
  // just-dismissed banner right back up. A monotonic generation counter,
  // bumped by every local action that should invalidate whatever is
  // currently in flight (dismissal, and — folded into the mount/reseat
  // effect below — a company or client change), closes both gaps with one
  // mechanism: a response is only applied if the request that produced it is
  // still the most recent one this component cares about.
  const nudgeRequestGeneration = useRef(0);
  // codex review finding (comment 3892594021): `pickActiveNudge` picks ONE
  // row to show — by design, per its own doc comment, since
  // `LifecycleScheduler` explicitly permits two racing replicas to both file
  // a nudge for the same user. Dismissal used to mark only the shown row
  // (`current.id`) read, so a genuine duplicate landed back on the very next
  // poll: the other, still-unread row is exactly what `pickActiveNudge`
  // picks next. Tracked separately from `nudge` (which is deliberately the
  // single row the banner renders) so `clearNudge` can mark every duplicate
  // read in one write instead of only the one on screen.
  const unreadNudgeIdsRef = useRef<string[]>([]);
  const refreshNudge = useCallback(() => {
    const requestCompany = company;
    const requestGeneration = ++nudgeRequestGeneration.current;
    client
      .notifications(requestCompany, "workflow_nudge")
      .then((feed) => {
        if (requestCompany !== companyRef.current) return; // stale: company switched mid-flight
        if (requestGeneration !== nudgeRequestGeneration.current) return; // stale: superseded by a newer request or a local action
        const rows = Array.isArray(feed?.notifications) ? feed.notifications : [];
        const active = pickActiveNudge(rows);
        unreadNudgeIdsRef.current = rows
          .filter((row) => row.kind === WEEK1_NUDGE_KIND && row.readAt === undefined)
          .map((row) => row.id);
        if (active && hasCreatedLocallyRef.current) {
          // This response was already stale the moment it landed — see
          // `hasCreatedLocallyRef`'s own doc comment. Reconcile the server
          // row rather than display it, the same best-effort mark-read
          // `clearNudge` performs below.
          setNudge(null);
          void client.markNotificationsRead([active.id], requestCompany).catch(() => {
            // The next poll tick retries; nothing renders in the meantime
            // either way, since `hasCreatedLocallyRef` stays set.
          });
          return;
        }
        setNudge(active);
      })
      .catch(() => {
        // An older host (404) or a transient failure: no banner, and nothing
        // else about the view changes — this is the least important thing on
        // screen, same reasoning the mention badge's own poll follows.
      });
  }, [client, company]);
  useEffect(() => {
    setNudge(null);
    hasCreatedLocallyRef.current = false;
    // Also invalidates anything already in flight against the previous
    // company or client (the "in-place host reseat that preserves the
    // company slug" half of comment 3892534919) — `refreshNudge`'s identity
    // already changes on either, so this effect already re-runs for both.
    nudgeRequestGeneration.current += 1;
    refreshNudge();
  }, [company, refreshNudge]);
  // Issue #1845 (review: PR #1878): the host files this nudge off a daily
  // scheduler tick, which mounts no SSE frame — nothing else tells a tab left
  // open across that tick that a nudge landed, so it would otherwise sit
  // unseen until the next reload or company switch. `approvalsNow` is the
  // same polling cadence the mention badge already piggybacks on for the
  // identical reason (`app-shell.tsx`'s own `feed.now` — no per-viewer SSE
  // projection either), so this re-runs the fetch on every tick rather than
  // adding a second poller.
  useEffect(() => {
    if (approvalsNow === undefined) return;
    refreshNudge();
    // eslint-disable-next-line react-hooks/exhaustive-deps -- fires on the poll tick only
  }, [approvalsNow]);
  // Marks the current nudge read (best-effort) and hides it locally at once,
  // rather than waiting for the next poll to confirm the write. Shared by the
  // banner's own Dismiss button and by a workflow actually getting created
  // (below) — both are "stop asking", the same action either way.
  const clearNudge = useCallback(() => {
    const current = nudgeRef.current;
    if (!current) return;
    // Invalidate any `refreshNudge` fetch already in flight — see
    // `nudgeRequestGeneration`'s own doc comment. Its `.then` still runs
    // (this does not cancel the network request), it just no longer applies
    // what it finds.
    nudgeRequestGeneration.current += 1;
    setNudge(null);
    // Every unread duplicate from the last refresh, not only the one shown —
    // see `unreadNudgeIdsRef`'s own doc comment. Falls back to just the shown
    // row if nothing was tracked yet (dismissed before any refresh landed).
    const ids = unreadNudgeIdsRef.current.length > 0 ? unreadNudgeIdsRef.current : [current.id];
    unreadNudgeIdsRef.current = [];
    void client.markNotificationsRead(ids, company).catch(() => {
      // The optimistic clear could be wrong (offline, older host); the next
      // poll below reconciles rather than leaving a stale local `null`.
      refreshNudge();
    });
  }, [client, company, refreshNudge]);
  // Issue #1845 (review: PR #1878): `listEventTick` bumps on every
  // `workflow_created` frame, and the frame is deliberately thin — no actor,
  // by design (`use-events.ts`) — so it cannot tell "this user's own create"
  // apart from a teammate's or the orchestrator's. Calling `clearNudge` here
  // used to persist THIS user's dismissal off of anyone's create in the
  // company, which could silence a nudge for someone who has never saved a
  // workflow themselves. `handleCreated` below already calls `clearNudge`
  // directly the moment this session's own create is confirmed, so the only
  // job left for the tick is picking up state this user changed elsewhere —
  // a dismissal or an attributed create from another of their own sessions —
  // which is exactly what re-asking the host's own per-user feed answers.
  // Skip the tick this effect mounts with (there is nothing to refresh yet).
  const nudgeListTickMounted = useRef(false);
  useEffect(() => {
    if (!nudgeListTickMounted.current) {
      nudgeListTickMounted.current = true;
      return;
    }
    if (nudgeRef.current) refreshNudge();
    // eslint-disable-next-line react-hooks/exhaustive-deps -- fires on the tick, reads current nudge via ref
  }, [listEventTick]);
  const [createOpen, setCreateOpen] = useState(false);
  // Issue #259: the same dialog, hydrated from the selected graph. Separate
  // state from `createOpen` rather than a mode flag, so the create path keeps
  // working exactly as it did and neither can be half-open.
  const [editOpen, setEditOpen] = useState(false);
  // Issue #1006: the graph the OPEN edit dialog is bound to, pinned when it
  // opens rather than read live off `graph`.
  //
  // `graph` moves for reasons that have nothing to do with the edit in
  // progress: a Back press changes `selectedId`, and the refetch lands a
  // DIFFERENT workflow's graph, which re-hydrated the dialog and destroyed
  // whatever was typed; a refetch that FAILS lands `null`, which unmounted the
  // dialog outright. Pinning makes both a no-op, while the effect that keeps it
  // in step still lets a re-read of the SAME workflow through — which is what
  // keeps the conflict banner's Reload and the History restore re-hydrating the
  // dialog as they are documented to.
  const [editGraph, setEditGraph] = useState<WorkflowGraph | null>(null);
  // Issue #840 (PR-3): a copilot-corrected graph to open the edit dialog on. When
  // set, the edit dialog hydrates from this correction (keeping `graph`'s version
  // token) instead of from the saved graph, so Save writes a new version.
  const [prefilledDraft, setPrefilledDraft] = useState<PrefilledDraft | null>(null);
  // The failed run whose copilot fix is in flight, so its history row spins.
  const [fixingRunSeq, setFixingRunSeq] = useState<number | null>(null);
  // A run the copilot judged un-fixable, shown inline under that run's row.
  const [fixReason, setFixReason] = useState<{ seq: number; reason: string } | null>(null);
  const [selectedNodeId, setSelectedNodeId] = useState<string | null>(null);
  /** Roster used only while an assigned agent node is being inspected. */
  const [nodeRoster, setNodeRoster] = useState<TeamMemberDto[]>([]);
  // Issue #228: what past runs did, read back from the host's journal. This is
  // the half that survives a reload — before it, a manual run's delivery rows
  // vanished when the drawer was dismissed and a scheduled run's never reached
  // the operator at all.
  const [runs, setRuns] = useState<WorkflowRunOutcome[]>([]);
  // Issue #1012: whether an older page of `runs` exists behind the oldest
  // `seq` currently held — gates the drawer's "Load older" affordance. Reset
  // to `false` whenever the effect below replaces `runs` wholesale (a fresh
  // newest-page fetch has not yet learned this), and updated by both that
  // effect and `loadOlderRuns` from each fetch's own `hasMore`.
  const [runsHasMore, setRunsHasMore] = useState(false);
  // Issue #1012 follow-up: the cursor the HOST issued for the page behind the
  // rows currently held. Not derivable here any more — the host cuts a page by
  // `seq` and displays it by `(atMillis, seq)`, so the boundary is the page's
  // lowest `seq` rather than its last row, and a clock regression makes those
  // two different runs. `undefined` means either "no older page" (`hasMore`
  // says which) or "host predates the field", which `loadOlderRuns` handles by
  // falling back to the old derivation rather than by stopping.
  const [runsNextBeforeSeq, setRunsNextBeforeSeq] = useState<number | undefined>(undefined);
  // The rows currently held, readable from `loadOlderRuns` without listing
  // `runs` as one of its dependencies — which would rebuild the callback on
  // every history refresh and, worse, capture a generation that has already
  // moved. Only the pre-#1012-host fallback reads it.
  const runsRef = useRef<WorkflowRunOutcome[]>(runs);
  runsRef.current = runs;
  // Which "generation" of the history list is on screen. Bumped at every ENTRY
  // of the first-page effect below, which replaces `runs` wholesale on all of
  // its paths — so an older page fetched against the previous list can tell it
  // is answering a question nobody is asking any more. Identity fields
  // (company, workflow) cannot see this case: a refresh within one company
  // changes neither.
  const historyGenRef = useRef(0);
  // A "Load older" fetch in flight, so the drawer can disable the control and
  // avoid a second click racing the first for the same older page.
  const [loadingOlderRuns, setLoadingOlderRuns] = useState(false);
  // Which workflow the rows in `runs` were fetched for.
  //
  // `graph` and `runs` are two independent requests off the same selection, so
  // a switch can land the new graph while the previous workflow's history is
  // still in state. Anything that pairs the two — the copilot's grounding — has
  // to be able to tell that the pair does not agree yet, and a bare "is it
  // loading" flag cannot: the mismatch outlives the load when a fetch fails.
  const [runsFor, setRunsFor] = useState<string | null>(null);
  const [historyOpen, setHistoryOpen] = useState(false);
  // A host predating the runs route answers 404. That is not an error worth
  // showing — the rest of the view works — so it just means "no history here".
  const [historySupported, setHistorySupported] = useState(true);
  // Bumped after a manual run so the history picks it up without a reload.
  const [runsTick, setRunsTick] = useState(0);
  // Issue #259: a delete in flight, and the host's message when a write was
  // refused because the graph moved under us.
  const [deleting, setDeleting] = useState(false);
  // Issue #276: a pause/resume in flight. Separate from `deleting` because the
  // two disable different things — a pause leaves Edit and Run usable.
  const [toggling, setToggling] = useState(false);
  // The confirm dialog is CONTROLLED on purpose. `AlertDialogAction` is a plain
  // `Button`, not an `AlertDialogPrimitive.Close` (only `AlertDialogCancel` is),
  // so confirming does not dismiss the dialog on its own — it stays up with its
  // backdrop swallowing every pointer event, over a view whose workflow has just
  // been deleted. Closing it explicitly in the confirm handler is the fix.
  const [confirmOpen, setConfirmOpen] = useState(false);
  // A version conflict is deliberately NOT a toast. It means the operator is
  // looking at a stale graph, so it has to persist next to the canvas with a way
  // out (Reload) — a toast that auto-dismisses would leave them staring at the
  // old graph believing the write landed.
  const [conflict, setConflict] = useState<string | null>(null);
  // Bumped by the conflict banner's Reload, to re-fetch the selected graph (and
  // with it a fresh `version`) without changing the selection.
  const [graphTick, setGraphTick] = useState(0);
  // Issue #371: the frontier painted between pressing Run and the first frame
  // arriving, so the canvas answers the click immediately. Cleared as soon as
  // the fold below has a run of its own to show.
  const [optimistic, setOptimistic] = useState<Record<string, NodeRunState> | null>(null);
  // A past run selected from the history panel, overlaid on the canvas. When
  // set it WINS over the live state — the operator asked to look at that run.
  // This is what makes a scheduled run's failure point visible after the fact,
  // which is the half of the issue a live canvas alone cannot answer.
  const [overlayRun, setOverlayRun] = useState<WorkflowRunOutcome | null>(null);
  // Issue #596: the durable per-node output of the overlaid past run, fetched
  // lazily once when a past run is opened (the history list is structural — no
  // output — so the inspector reads what each node produced from here). `record:
  // null` after a settled fetch means the run has no captured output (a 404 —
  // predates capture / dry / hard-aborted), which the inspector renders as an
  // explicit empty state.
  const [overlayOutput, setOverlayOutput] = useState<{
    runId: string;
    loading: boolean;
    record: WorkflowRunOutputRecord | null;
  } | null>(null);
  // The run this view just POSTed, held until its history row arrives.
  //
  // The fallback for a console with no live stream: if `/events` 404s or the
  // connection dropped, no progress frame ever lands and the canvas would stay
  // blank for a run the operator watched happen. Overlaying the row the host
  // journaled gets them the same per-node answer, just at the end instead of
  // during. Cleared as soon as it is used, or when live frames made it moot.
  const [awaitingRunId, setAwaitingRunId] = useState<string | null>(null);
  // Issue #383: the detached run this view started and has not yet seen settle.
  //
  // The Run button's guard used to be "a promise is outstanding". With a
  // detached run that promise resolves in milliseconds while the run keeps
  // going, so the guard has to key on the run's *observed* state instead: this
  // is set when the host accepts the run and cleared when the run settles —
  // from the live frames when the stream is up, and from the history row when
  // it is not. It is also what puts a Cancel button on screen, since it is
  // exactly the id that route addresses.
  const [activeRunId, setActiveRunId] = useState<string | null>(null);
  // The POST itself is in flight. Separate from `activeRunId` because the
  // button must be disabled during the request too, and a rejected request
  // never produces a run id.
  const [starting, setStarting] = useState(false);
  // Issue #528 / #514: a run the host REFUSED for a reason the operator can act
  // on — no inference source configured, or a saved one a restart owes. Like a
  // version conflict, this is not a toast: the fix is a few clicks away in
  // Settings, so it earns a persistent banner keyed on the structured `code`.
  const [runRefusal, setRunRefusal] = useState<{ code: string; message: string } | null>(null);
  // Issue #528: whether a NON-scheduled `workflow_run_started` frame for the
  // workflow on screen has arrived since the last dispatch — i.e. our own run
  // reached the engine before the connection could drop. A ref, not state,
  // because the `run()` catch reads it synchronously and it must not itself
  // trigger a render. Reset at the top of every `run()`.
  const sawOwnRunStartRef = useRef(false);
  // Issue #1007: the id that run became, when the live fold got far enough to
  // tell us. A ref for exactly the reasons above — the `run()` catch reads it
  // synchronously, and `activeRunId` is not in that callback's closure. It is
  // what lets a failed POST hand the history fetch a run id to pull forward,
  // rather than leaving the operator to guess which row was theirs.
  const ownRunIdRef = useRef<string | null>(null);
  // The run whose cancel came back 404, if any.
  //
  // Deliberately a run id rather than a boolean. A 404 is ambiguous — either
  // the host has no cancel route, or the run settled a moment before we asked,
  // which is perfectly normal — so a global flag would let one ordinary race
  // hide the Stop button for every later run in the session. Scoping it to the
  // run means the worst case is losing the affordance on the run that was
  // already over.
  const [cancelUnsupportedFor, setCancelUnsupportedFor] = useState<string | null>(null);
  const [cancelling, setCancelling] = useState(false);
  // Issue #1110: the index (cards or list) is no longer a panel that opens over
  // the canvas — it is what `#/workflows` *is*. There is exactly one piece of
  // state behind that, `selectedId`, and it is the URL's second segment: null
  // means the index, an id means that workflow's detail view.
  //
  // Introduced as a name rather than inlined because a dozen places read it and
  // "is a workflow open" is the question they are all asking. It is deliberately
  // NOT separate state — a second flag could disagree with the selection, and
  // the disagreement would be a canvas with no graph or an index with a
  // per-workflow toolbar over it.
  const detailOpen = selectedId !== null;
  // Issue #1110: a deep link whose workflow this company does not have.
  //
  // Set only after the resolver's fresh re-read has confirmed the absence (see
  // the follow effect), and rendered on the index rather than raised as a toast:
  // the operator arrived from somebody else's link and the useful reading —
  // "that link is dead, here is what this company does have" — has to still be
  // on screen while they scan the list for the workflow that replaced it.
  const [missingWorkflowId, setMissingWorkflowId] = useState<string | null>(null);
  // Which rendering the index uses, remembered across sessions — an operator
  // who prefers one has no reason to re-pick it every visit.
  const [indexMode, setIndexMode] = useState<IndexMode>(readIndexMode);
  // Issue #1697: the index's other axis — the company's workflows, or their
  // runs. Same remembered-preference treatment as `indexMode`.
  const [indexTab, setIndexTab] = useState<IndexTab>(readIndexTab);
  // Issue #1697: the run whose transcript sheet is open, or `null` when it's
  // closed. Holds the run itself (not just an id) because the traces list
  // already has the full `WorkflowRunOutcome` in hand — the sheet needs
  // nothing this view would otherwise have to re-fetch just to open it.
  const [traceRun, setTraceRun] = useState<WorkflowRunOutcome | null>(null);
  // Issue #303: the company-wide run page behind the index's health readings.
  //
  // Deliberately SEPARATE from `runs`, which is the selected workflow's history
  // and is scoped server-side by `?workflow=`. This one is unscoped on purpose —
  // one request has to cover every card — and that is exactly why the cards say
  // "No recent runs" rather than "never run": see `WorkflowIndex`.
  const [indexRuns, setIndexRuns] = useState<WorkflowRunOutcome[]>([]);
  const [indexRunsLoaded, setIndexRunsLoaded] = useState(false);
  // Issue #303: the per-workflow copilot panel is open.
  const [copilotOpen, setCopilotOpen] = useState(false);
  // Run ids the live fold has actually seen frames for. The no-stream fallback
  // above consults it so a console WITH a working stream never double-paints a
  // run it already watched, and one without it still gets the journaled answer.
  //
  // Issue #1010: this is now its ONLY reader, and it is cleared on every
  // workflow/company switch below. It used to gate `inFlightSeed` too, which
  // was the bug: a set that only ever grows cannot speak for a 300-frame window
  // that evicts, so a run whose start had aged out was reported as covered and
  // the seed was withheld from a fold that had nothing left to fold. The seed
  // now asks the window itself — see `windowHasRunStart`.
  const liveRanRef = useRef<Set<string>>(new Set());
  // Issue #863: the run this canvas adopted from the history rather than from a
  // start frame. Held so the trail STAYS on screen once that run settles — the
  // history read that reports it finished is the same read that stops reporting
  // it as in flight, and dropping the seed on that tick would blank a canvas the
  // operator has been watching fill in.
  const adoptedFromHistoryRef = useRef<string | null>(null);

  // ---- Issue #339: the canvas as a link target -----------------------------
  //
  // The workflow id asked for by the URL. Decoded here so it compares against
  // the ids in `workflows` — the router hands the segment back exactly as it
  // sits in the address bar, and the writer below percent-encodes it.
  const requestedWorkflowId = useMemo(() => (sub ? safeDecode(sub) : null), [sub]);
  // The run id asked for by `?run=`, which the router does NOT surface: it
  // reads the hash with everything after `?` stripped, so a query string is
  // invisible to it (and, crucially, survives its rewrite — see
  // {@link readWorkflowHash}). Kept in state and refreshed on `hashchange` so a
  // second card's link landing on an already-open canvas is noticed.
  const [requestedRunId, setRequestedRunId] = useState<string | null>(
    () => readWorkflowHash().runId,
  );
  useEffect(() => {
    const onHash = () => setRequestedRunId(readWorkflowHash().runId);
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);
  // The URL-requested ids this view has already acted on, so each request is
  // honoured exactly once. Without these, every later re-render that carries a
  // fresh `workflows` / `runs` array would re-apply the URL and drag the
  // operator back off whatever they had since selected or cleared.
  const appliedWorkflowRef = useRef<string | null>(null);
  const appliedRunRef = useRef<string | null>(null);

  // Issue #1045: the deep-link id whose absent-path re-read is in flight, so
  // that read is started at most once per id even though the follow effect
  // reruns on every fresh `workflows` array. Deliberately SEPARATE from
  // `appliedWorkflowRef`: an id is "applied" only once it has RESOLVED — got
  // selected, or was confirmed missing after a fresh read — whereas an id whose
  // re-read is still outstanding must stay unapplied, so a refresh that finally
  // carries a graph authored elsewhere can still select it.
  const resolvingWorkflowRef = useRef<string | null>(null);

  // How many local writes have landed. Compared across the list effect's await
  // so a `GET …/workflows` that was already in flight when a create, save or
  // delete completed cannot paint the picker with rows that predate it — the
  // entry the operator just made vanishing again, or the one they just deleted
  // coming back, until some later frame happened to correct it.
  //
  // Only reachable now that this effect re-runs on a live frame: it used to run
  // on mount and on a company switch, neither of which races an authoring
  // action. Nothing is lost by discarding such a response — the local handler
  // has already applied the newer truth, and each of these writes emits its own
  // frame, which brings a fresh list along behind it.
  const localWriteRef = useRef(0);

  // The id the list effect moved the selection to ON THE OPERATOR'S BEHALF —
  // a correction, not a navigation.
  //
  // The hash mirror below pushes a history entry whenever the selection moves
  // between two named workflows, which was right while only an operator action
  // could move it. A live delete moves it too, and pushing that leaves a place
  // the operator never went in their history: one Back press onto a URL naming
  // a workflow the host no longer has. The view does not even correct itself
  // there — the URL-follow effect treats an id it has already applied as an
  // echo and leaves it alone (which is also why no error toast appears, the
  // route by which that would fire being closed by the same guard) — so the
  // address bar names one graph while the canvas shows another, and that
  // address is what the operator would copy out and share.
  //
  // Holds the id rather than a bare flag so a marker that outlives its render
  // cannot suppress an unrelated push later — it counts only when it names the
  // very selection the mirror is about to write.
  //
  // Issue #1110: BOXED, because the selection it names can now be `null` — the
  // list effect no longer falls back to the first remaining row, it falls back
  // to the index. A bare `string | null` cannot tell "reconciled to nothing"
  // from "no marker set", and reading the second as the first would leave a
  // deleted workflow's id in the address bar over an index.
  const reconciledSelectionRef = useRef<{ id: string | null } | null>(null);

  // Load the workflow list.
  //
  // Issue #1110: it does NOT select anything on the operator's behalf. A list
  // that lands with nothing selected renders the index, which is the whole
  // point of the tab — "what workflows do I have?" — and a workflow is opened
  // only by a click or by a URL that names one.
  //
  // Re-runs on `listEventTick` (issue #384), i.e. whenever the host says a
  // workflow was created, edited or deleted anywhere — the orchestrator's
  // `create_workflow` tool, a second console session, a machine credential.
  // Before that this ran on mount and on a company switch only, so the picker
  // silently drifted from the host for the whole life of the tab.
  //
  // Re-reading the list is also what makes a **delete** land properly rather
  // than greying an entry out: the selection reconciliation below drops an id
  // the fresh list no longer has, which moves the canvas off a graph the host
  // does not hold and takes the entry out of the picker in the same pass. No
  // spinner and no flash — `loadingList` is not raised again, so the picker
  // just changes.
  useEffect(() => {
    let live = true;
    (async () => {
      const writesBefore = localWriteRef.current;
      try {
        const rows = await listWorkflows(client, company);
        if (!live) return;
        // A local write landed while this request was in flight. It holds the
        // newer truth and these rows predate it, so they are dropped rather
        // than allowed to undo it.
        if (localWriteRef.current !== writesBefore) return;
        setWorkflows(rows);
        setSelectedId((prev) => {
          // Keep the selection when the freshly loaded list still has it —
          // otherwise a stale id would leave the canvas on a graph the host
          // does not hold. One rule, two cases: a leftover id from the previous
          // company (this effect reruns on a switch), and the workflow on
          // screen being deleted from somewhere else (issue #384, this effect
          // reruns on the frame).
          //
          // Held selection BEFORE the URL, and that ordering is load-bearing
          // for #384 without costing #339 anything. On mount `prev` is null, so
          // a link still lands on the graph it names, flash-free — which is all
          // #339 asked for here, changes after mount being the follow effect's
          // job. But this effect now also reruns on a *live* frame, and the
          // hash trails the selection by a `hashchange`: reading the URL first
          // would let a frame arriving inside that gap — the console's own
          // `workflow_created`, for instance — snap the operator back off the
          // workflow they just made.
          if (prev && rows.some((r) => r.id === prev)) return prev;
          // Issue #339: a workflow id in the URL outranks the first-row
          // default — the operator followed a link to *that* graph, and
          // selecting `rows[0]` first and correcting a render later would fetch
          // a graph nobody asked for and show the wrong name on the way past.
          //
          // `requestedWorkflowId` is read from the closure and deliberately NOT
          // a dependency: it changes on every picker click (the writer below
          // mirrors the selection into the hash), and re-running this would
          // spend a full list round trip on each one.
          if (requestedWorkflowId && rows.some((r) => r.id === requestedWorkflowId)) {
            return requestedWorkflowId;
          }
          // Issue #1110: nothing valid to keep, so the view goes back to the
          // INDEX. It used to fall to `rows[0]`, which is the behaviour the
          // issue is about seen from its other side — on first load that is the
          // auto-select that drops the operator inside a workflow they did not
          // choose, and after a delete elsewhere it is a neighbouring graph
          // appearing on the canvas as though they had opened it.
          //
          // Exactly the same rule the local Delete button now follows, so a
          // workflow deleted from another session and one deleted from this
          // console leave the view in the same place.
          //
          // Marked as a reconciliation so the hash mirror replaces rather than
          // pushes: nobody navigated here, the workflow they were on stopped
          // existing. The marker is only worth setting when there was something
          // to leave — a `prev` of null that stays null never re-renders, so a
          // marker set there would sit unconsumed. (React re-invokes an updater
          // in StrictMode; writing the same marker twice says the same thing
          // twice.)
          if (prev !== null) reconciledSelectionRef.current = { id: null };
          return null;
        });
        setListError(null);
      } catch (e) {
        if (!live) return;
        setListError(e instanceof Error ? e.message : "could not load workflows");
      } finally {
        if (live) setLoadingList(false);
      }
    })();
    return () => {
      live = false;
    };
    // `requestedWorkflowId` is read above but deliberately not a dependency —
    // see the comment at the read.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [client, company, listEventTick]);

  // Issue #339: follow the URL while the view stays mounted — the operator
  // clicks a second task card's link without ever leaving this tab, so the
  // first-load resolution above never runs again.
  //
  // Guarded to once per RESOLVED id: this also reruns whenever `workflows` gets
  // a new array (a create, a rename, a company switch), and re-applying the URL
  // then would yank the selection back from wherever the operator had moved it.
  // A no-longer-current `sub` — the operator picked something else, so the hash
  // no longer matches — is left alone precisely because it was already applied.
  //
  // Issue #1045: an id is recorded in `appliedWorkflowRef` ONLY once it has
  // resolved — selected, or confirmed missing after a fresh read. An id that is
  // merely absent from the list on screen right now must NOT be frozen there:
  // a graph the orchestrator (or another session) authored lands in the picker
  // a beat after its link is followed, and that later refresh has to be allowed
  // to select it. And an absent id no longer toasts on sight — it toasts only
  // after a fresh re-read still cannot find it, which is what tells "renamed or
  // deleted" apart from "authored elsewhere and not pulled into this tab yet".
  useEffect(() => {
    if (!requestedWorkflowId || workflows.length === 0) return;
    if (appliedWorkflowRef.current === requestedWorkflowId) return;
    // Present in the list already on screen: select it, done. Marking it applied
    // HERE, inside the present branch, is half the #1045 fix — an absent id must
    // never reach the ref, or a later refresh carrying it would early-return
    // above instead of selecting it.
    if (workflows.some((w) => w.id === requestedWorkflowId)) {
      appliedWorkflowRef.current = requestedWorkflowId;
      setSelectedId(requestedWorkflowId);
      return;
    }
    // Absent from the list on screen. Before #1045 this toasted straight away,
    // against whatever the picker happened to hold the instant the id was first
    // seen — on a link followed to a just-authored graph, a list that predates
    // it — so the operator got a false "no workflow" and the canvas never
    // resolved. Re-read the list first, and decide on the fresh answer. At most
    // one read per requested id (the effect reruns on every `workflows` array).
    if (resolvingWorkflowRef.current === requestedWorkflowId) return;
    resolvingWorkflowRef.current = requestedWorkflowId;
    let live = true;
    const target = requestedWorkflowId;
    const writesBefore = localWriteRef.current;
    (async () => {
      try {
        const rows = await listWorkflows(client, company);
        // Same liveness discipline as the list effect above: bail if the view
        // unmounted or a dependency changed under us (both flip `live` via this
        // effect's cleanup), and drop the rows if a local write landed while the
        // read was in flight — that write holds newer truth than a read which
        // predates it.
        if (!live) return;
        if (localWriteRef.current !== writesBefore) return;
        if (appliedWorkflowRef.current === target) return;
        appliedWorkflowRef.current = target;
        if (rows.some((r) => r.id === target)) {
          // The graph exists after all — authored elsewhere and not yet pulled
          // into this tab. Adopt the fresh list so the picker shows it too, and
          // select it. No toast: nothing was wrong.
          setWorkflows(rows);
          setSelectedId(target);
          return;
        }
        // Still absent after a fresh read: the link genuinely names a workflow
        // this company no longer has.
        //
        // Issue #1110: land on the INDEX and say so there, rather than leaving
        // a detail view addressed to nothing. Before this the view kept
        // whatever was auto-selected and toasted "showing the current selection
        // instead", which was the wrong offer twice over — the operator never
        // chose that workflow, and four seconds later the only trace that the
        // link was dead had gone.
        //
        // The toast stays as well, because the banner alone is silent for an
        // operator who is already looking at the index when a second card's
        // link is followed into it: nothing on screen would move.
        setMissingWorkflowId(target);
        setSelectedId(null);
        clearWorkflowFromHash();
        toast.error(`This company has no workflow “${target}”.`, {
          description:
            "It may have been renamed or deleted since the link was made. Showing every workflow this company has instead.",
        });
      } catch {
        // A failed re-read is not proof the workflow is missing. Leave the id
        // unresolved so a later refresh can still land it, rather than toasting
        // a false "no workflow" off a transient network error.
        if (live) resolvingWorkflowRef.current = null;
      }
    })();
    return () => {
      live = false;
      // Let a rerun retry the re-read: this cleanup fires when a dependency
      // changed the read out from under us (an unrelated list refresh, a company
      // switch), and the abandoned read above will not clear the guard itself.
      resolvingWorkflowRef.current = null;
    };
    // `requestedWorkflowId` and `workflows` drive the resolution; `client` and
    // `company` scope the re-read. `listWorkflows`, `localWriteRef`,
    // `setWorkflows`, `setSelectedId` are stable.
  }, [requestedWorkflowId, workflows, client, company]);

  // Issue #339: mirror the selection back into the hash, so whatever is on the
  // canvas can be copied out of the address bar and shared.
  //
  // Reads `location` directly instead of comparing against `sub`: the router's
  // state lags a `replaceState` it never hears about, and this comparison is
  // the only thing stopping a write→hashchange→write loop.
  //
  // The `?run=` query is preserved implicitly. When the hash already names the
  // selected workflow this early-returns and never touches the URL, so an
  // arriving `#/workflows/x?run=r` keeps its run id; when the selection moves
  // to a different workflow the query is dropped, which is correct — that run
  // belongs to the graph being left behind. The connection scope is the one key
  // that does carry over (`withHostParam`): it names the host, not the graph.
  //
  // Replace vs push is decided by whether the view moved the selection on the
  // operator's behalf.
  //
  // Issue #1110 changed this rule, and the change is the whole of "Back must
  // move index ↔ detail". Filling in a bare `#/workflows` used to be the view
  // resolving its own default — nobody had navigated, so pushing it would have
  // left a duplicate-looking history entry — and it was written with `replace`.
  // There is no default to resolve any more: a bare `#/workflows` IS the index,
  // and the only thing that fills it in is an operator opening a workflow from
  // the index (or the picker, or a create). That is a navigation, it pushes,
  // and Back returns to the list they came from.
  //
  // A reconciliation is still not a navigation: the workflow they were on
  // stopped existing and the list effect moved them off it (issue #384).
  // Pushing that would offer Back as a route to a workflow that is gone — see
  // `reconciledSelectionRef` for why the view does not even correct itself once
  // it is there.
  //
  // A selection of `null` writes nothing HERE. It is also what an unresolved
  // deep link looks like for the render or two before the list lands, and
  // clearing the hash on sight would destroy the link before it could be
  // followed. Everything that genuinely leaves a workflow behind calls
  // {@link clearWorkflowFromHash} for itself, at the point where it knows.
  useEffect(() => {
    // Consumed on every run, whichever branch is taken below: a marker left
    // over from a reconciliation that did not end up writing the URL must not
    // decide a later, genuine navigation.
    const marker = reconciledSelectionRef.current;
    reconciledSelectionRef.current = null;
    const reconciled = marker !== null && marker.id === selectedId;
    if (!selectedId) {
      // A reconciliation onto the index (issue #1110): the list came back
      // without the workflow on screen. Take its id out of the address bar so
      // the URL names the index the operator is now looking at.
      if (reconciled) clearWorkflowFromHash();
      return;
    }
    const { onWorkflows, workflowId } = readWorkflowHash();
    // Another view owns the hash (a company switch mid-navigation, a stale
    // effect): rewriting it would drag the operator back here.
    if (!onWorkflows) return;
    if (workflowId === selectedId) return;
    const next = withHostParam(`workflows/${encodeURIComponent(selectedId)}`);
    if (reconciled) window.history.replaceState(null, "", next);
    else window.location.hash = next.slice(1);
  }, [selectedId]);

  // Issue #1110: what leaving a workflow — by any route — settles.
  //
  // The dead-link explanation belongs to the arrival that raised it and to
  // nothing after, so opening any workflow answers it. (A company switch does
  // too, below: it makes the banner a statement about a company nobody is
  // looking at.)
  //
  // The two body-level drawers are per-workflow surfaces, and going back to the
  // index is the one selection change that must close them. A workflow-to-
  // workflow switch deliberately does NOT: an operator comparing two histories
  // opened that panel and still wants it. But left open across a return to the
  // index — from the back button, from a delete here, from a delete somewhere
  // else — the NEXT workflow opened comes up wearing a drawer nobody asked it
  // for, showing that workflow's runs. One rule here rather than one per route,
  // because the routes to the index that are not the back button are exactly
  // the ones nobody remembers to update.
  useEffect(() => {
    if (selectedId !== null) {
      setMissingWorkflowId(null);
      return;
    }
    setHistoryOpen(false);
    setCopilotOpen(false);
  }, [selectedId]);
  useEffect(() => {
    setMissingWorkflowId(null);
    // Issue #1704 (review): the list failure is scoped to the company whose list
    // failed. It must NOT be cleared by a workflow change — that is the axis the
    // operator crosses to go and look at the stale list — but it must be cleared
    // here, or "could not load workflows" from the company just left would sit
    // over the next company's list while that list loads perfectly.
    setListError(null);
  }, [company]);

  // Fetch the selected workflow's full graph.
  useEffect(() => {
    if (!selectedId) {
      setGraph(null);
      return;
    }
    let live = true;
    setLoadingGraph(true);
    setResult(null);
    setRunFailure(null);
    setSelectedNodeId(null);
    (async () => {
      try {
        const g = await getWorkflow(client, company, selectedId);
        if (!live) return;
        setGraph(g);
        setGraphError(null);
        // A successful re-read is exactly what clears a stale-graph warning:
        // whatever `version` we now hold is current.
        setConflict(null);
      } catch (e) {
        if (!live) return;
        setGraph(null);
        setGraphError(e instanceof Error ? e.message : "could not load the workflow graph");
      } finally {
        if (live) setLoadingGraph(false);
      }
    })();
    return () => {
      live = false;
    };
  }, [client, company, selectedId, graphTick]);

  // Issue #1006: keep the open edit dialog bound to the workflow it was opened
  // against. Pin on open; afterwards adopt `graph` only while it is still the
  // SAME workflow — a re-read of it (the conflict banner's Reload, a History
  // restore) must reach the dialog, a different one must not, and a failed read
  // must not blank it. Cleared on close so the next Edit pins afresh.
  useEffect(() => {
    if (!editOpen) {
      setEditGraph(null);
      return;
    }
    setEditGraph((pinned) =>
      pinned === null || pinned.id === graph?.id ? graph : pinned,
    );
  }, [editOpen, graph]);

  // Load the SELECTED workflow's run history. Re-runs when the selection or
  // company changes, after a manual run, and on every `workflow_run_finished`
  // the shell forwards — so a scheduled run that fires with this tab open lands
  // here on its own.
  //
  // The `workflow` filter is not an optimization, it is the correctness of this
  // read. The host applies `?workflow=` BEFORE the `limit` cut precisely so a
  // rarely-run workflow still returns its own most recent runs. Fetching the
  // company-wide page and filtering client-side would undo that: once other
  // workflows produce `limit` more-recent runs, this workflow's history falls
  // out of the page and the panel would claim it "hasn't finished a run yet"
  // while the run sits journaled on the host — which is issue #228's own
  // symptom, reappearing in the console.
  useEffect(() => {
    // No selection yet (first render, or a company with no workflows): there is
    // no history to ask for, and asking unfiltered is the bug described above.
    // `historySupported` is deliberately left alone — nothing was learned about
    // whether the host serves this route.
    // Issue #1012 follow-up: bumped at ENTRY, before the early return, because
    // every path out of this effect replaces the list — including this one.
    // Any "Load older" response still in flight is now answering a superseded
    // list and must be dropped rather than appended.
    historyGenRef.current += 1;
    if (!selectedId) {
      setRuns([]);
      setRunsHasMore(false);
      setRunsNextBeforeSeq(undefined);
      setRunsFor(null);
      return;
    }
    let live = true;
    (async () => {
      try {
        const { runs: rows, hasMore, nextBeforeSeq } = await listWorkflowRuns(client, company, {
          workflow: selectedId,
          limit: 50,
        });
        if (!live) return;
        setRuns(rows);
        // Issue #1012: this effect always replaces the page wholesale (a
        // company switch, a run event, an explicit refresh) — any older runs
        // a "Load older" click had appended are gone with it, so `hasMore`
        // starts back over from this fresh newest page's own answer rather
        // than carrying forward whatever the appended state last said.
        setRunsHasMore(hasMore);
        setRunsNextBeforeSeq(nextBeforeSeq);
        setRunsFor(selectedId);
        setHistorySupported(true);
        // Issue #371, the no-live-stream fallback. If the run we just POSTed is
        // in this page and nothing was ever *reported* live (only the frontier
        // we derived ourselves), overlay the journaled row so the operator
        // still gets the per-node answer.
        if (awaitingRunId) {
          const mine = rows.find((r) => r.runId === awaitingRunId);
          if (mine) {
            setAwaitingRunId(null);
            // Only when the live fold never adopted this run — i.e. no frame
            // for it ever arrived. With a live stream this is a no-op.
            if (!liveRanRef.current.has(mine.runId ?? "") && (mine.nodes?.length ?? 0) > 0) {
              setOverlayRun(mine);
            }
          }
        }
      } catch (e) {
        if (!live) return;
        // Degrade quietly: an older host simply has no history to show.
        console.debug("[WorkflowsView] run history unavailable", e);
        setRuns([]);
        setRunsHasMore(false);
        setRunsNextBeforeSeq(undefined);
        // Still THIS workflow's answer — "the host has no history for it" — so
        // the pair agrees and the copilot may proceed, told via `runsKnown`
        // that nothing is known about runs rather than that there were none.
        // A `?run=` deep link reads the same signal (issue #339): it needs to
        // hear "the fetch came back empty" rather than wait on one that has.
        setRunsFor(selectedId);
        setHistorySupported(false);
      }
    })();
    return () => {
      live = false;
    };
    // `awaitingRunId` is read but deliberately not a dependency: it is cleared
    // inside, and listing it would re-run this fetch on that clear.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [client, company, selectedId, runsTick, runEventTick]);

  // Issue #1012: "Load older", the run-history drawer's pagination affordance.
  // APPENDS to `runs` rather than replacing it — unlike the effect above,
  // which always starts over from the newest page.
  const loadOlderRuns = useCallback(() => {
    if (!selectedId || loadingOlderRuns) return;
    // The boundary comes from the HOST (issue #1012 follow-up). It is the
    // page's lowest `seq`, which stopped being its last displayed row once the
    // cut moved to `seq` while the display stayed on `(atMillis, seq)` — under
    // a clock regression those are two different runs, and paging off the last
    // row skips the ones in between, permanently.
    //
    // The fallback is version skew, and it must be the OLD derivation rather
    // than "stop here": a host predating the field still cuts its pages in
    // display order, so its last row genuinely is the boundary. Reading an
    // absent cursor as "no more pages" would re-ship this fix as a fresh
    // silent truncation — #1012's own symptom. Gated on `hasMore` so a
    // finished history never asks for a page behind its last row.
    const cursor = runsNextBeforeSeq ?? (runsHasMore ? runsRef.current.at(-1)?.seq : undefined);
    if (cursor === undefined) return;
    // Everything this response is allowed to land on, captured BEFORE the
    // await — the same pattern the Resume handler uses for its company guard.
    const forCompany = companyRef.current;
    const forWorkflow = selectedId;
    const forGeneration = historyGenRef.current;
    setLoadingOlderRuns(true);
    (async () => {
      try {
        const { runs: older, hasMore, nextBeforeSeq } = await listWorkflowRuns(client, company, {
          workflow: selectedId,
          limit: 50,
          beforeSeq: cursor,
        });
        // Three checks, because no two of them are enough.
        //
        // * Company: a workflow id is NOT unique across companies —
        //   `create_company_workflow` checks it only within the requesting
        //   company — so two companies genuinely share one (a seed workflow
        //   shipped identically to both is the common case). Company A's older
        //   page would otherwise append onto company B's history.
        // * Workflow: the ordinary switch-while-in-flight.
        // * Generation: the first-page effect can have replaced the list
        //   without either identity field changing — a run finished, the 2s
        //   poll ticked, an explicit refresh. Appending an older page onto a
        //   list that has already started over duplicates rows and corrupts
        //   the cursor.
        if (
          companyRef.current !== forCompany ||
          selectedIdRef.current !== forWorkflow ||
          historyGenRef.current !== forGeneration
        ) {
          return;
        }
        setRuns((prev) => [...prev, ...older]);
        setRunsHasMore(hasMore);
        setRunsNextBeforeSeq(nextBeforeSeq);
      } catch (e) {
        // Same quiet degradation as the newest-page fetch — leave what is
        // already shown in place rather than losing it to a failed page.
        console.debug("[WorkflowsView] loading older run history failed", e);
      } finally {
        // Deliberately OUTSIDE the guard above. The flag belongs to the click,
        // not to the list the answer turned out to be for: skipping it on a
        // superseded response would wedge "Load older" as permanently busy.
        setLoadingOlderRuns(false);
      }
    })();
  }, [client, company, selectedId, runsNextBeforeSeq, runsHasMore, loadingOlderRuns]);

  // Issue #303: the run page the index's health readings are folded from.
  //
  // Fetched only while the index is on screen — every card reads from one
  // request, and a workflow's own detail view reads the scoped history instead.
  // It refreshes on `runEventTick` so a run finishing with the index up updates
  // the card that owns it, and on `runsTick` so a run started from here shows
  // as running.
  //
  // Issue #1110: the gate used to be "the Browse panel is open" and is now "no
  // workflow is open", which is the same question asked of the one piece of
  // state that decides what the tab renders.
  //
  // UNSCOPED, unlike the selected workflow's history above: `?workflow=` covers
  // exactly one graph, and the index needs every graph. The cost of that is a
  // page cut by `limit` across all workflows, which is precisely why the cards
  // are worded "No recent runs" — see `WorkflowIndex`'s `HealthLine`.
  useEffect(() => {
    if (detailOpen) return;
    let live = true;
    (async () => {
      try {
        // `hasMore` is ignored here: the index only needs enough of the
        // company-wide page to fold per-card health, not a pagination UI.
        const { runs: rows } = await listWorkflowRuns(client, company, { limit: 200 });
        if (!live) return;
        setIndexRuns(rows);
        setIndexRunsLoaded(true);
      } catch (e) {
        if (!live) return;
        // Same degradation as the scoped read: a host predating the runs route
        // answers 404, and the index is still worth showing without health.
        console.debug("[WorkflowsView] company-wide run page unavailable", e);
        setIndexRuns([]);
        setIndexRunsLoaded(true);
      }
    })();
    return () => {
      live = false;
    };
  }, [client, company, detailOpen, runsTick, runEventTick]);

  useEffect(() => {
    if (detailOpen || !indexRuns.some((run) => run.running)) return;
    return startVisiblePolling(() => setRunsTick((n) => n + 1), 2_000);
  }, [detailOpen, indexRuns]);

  // A company switch invalidates the whole page — another company's runs must
  // never be folded onto this one's cards, and `indexRunsLoaded` has to go back
  // to false so the cards say "Loading runs…" rather than "No recent runs"
  // about a company we have not asked about yet.
  useEffect(() => {
    setIndexRuns([]);
    setIndexRunsLoaded(false);
    setTraceRun(null);
    // Issue #1697: the open transcript names a run of the company being
    // left. Left up, its header resolves `workflowId` against the NEW
    // company's workflows — ids are not unique across companies — and its
    // output fetch 404s against a run the new company's host never held.
    setTraceRun(null);
  }, [company]);

  // The run page grouped by workflow, newest first.
  //
  // The host already returns the page newest-first, so this preserves order
  // rather than re-sorting — one ordering, decided server-side.
  const runsByWorkflow = useMemo(() => {
    const byId = new Map<string, WorkflowRunOutcome[]>();
    for (const row of indexRuns) {
      const list = byId.get(row.workflowId);
      if (list) list.push(row);
      else byId.set(row.workflowId, [row]);
    }
    return byId;
  }, [indexRuns]);

  // The traces sheet's run, kept LIVE (issue #1697 review): `traceRun` names
  // which run is open by its `seq`, and this re-resolves that seq against the
  // freshest `indexRuns` page on every render. Without it the sheet held the
  // snapshot from the moment it was opened, so a run still in flight when
  // clicked stayed "running" in the sheet forever — the index around it kept
  // refreshing and settling, but the sheet never saw it. Falls back to the
  // held snapshot when the run has aged off the capped page, which is the
  // same degradation `indexRuns` itself already accepts.
  const liveTraceRun = useMemo(
    () =>
      traceRun ? (indexRuns.find((r) => r.seq === traceRun.seq) ?? traceRun) : null,
    [traceRun, indexRuns],
  );

  // `input` is the trigger payload for THIS dispatch (issue #1204), handed in by
  // whichever control fired rather than read out of `request` state. The toolbar's
  // Run and Test run pass nothing and therefore genuinely run with no input; only
  // the "Run with input" dialog passes its draft. That is the whole guarantee the
  // dialog rests on: there is no way to run with a payload the operator cannot
  // see at the moment they press the button.
  const run = useCallback(async (dryRun = false, input = "") => {
    if (!selectedId) return;
    setStarting(true);
    // Clear the last refusal and the own-run-start watch before this attempt, so
    // neither leaks into the new run's triage (issue #528).
    setRunRefusal(null);
    sawOwnRunStartRef.current = false;
    ownRunIdRef.current = null;
    // Issue #1007: the LAST run's detail goes with the last run's marks.
    // `overlayRun` was cleared here from the start and `result` never was, so a
    // second run that failed left the first run's nodes, output and "Requested:"
    // line on screen — presented, with nothing to say otherwise, as the new
    // run's detail. `ranWith` is only pinned on success (below), so the echo was
    // stale in the same way.
    setResult(null);
    setRunFailure(null);
    // Issue #371/#382: clear the previous run's marks and seed the trigger as
    // done immediately, so the canvas responds to the click rather than waiting
    // on the first frame. The `workflow_run_started` frame re-sets the same thing
    // a moment later, which is idempotent. The per-node "running" lights now
    // arrive as reported `workflow_node_started` frames (#382) rather than being
    // guessed from the trigger's successors.
    //
    // Issue #542: a dry run paints NO optimistic frontier. It journals nothing
    // and emits no SSE frames, so there is no live fold to hand the canvas back
    // to — an optimistic frontier would pulse "running" on every node forever.
    // Its node-by-node answer arrives in the settled body's `nodes`, rendered in
    // the result panel, not on the canvas.
    setOverlayRun(null);
    setOptimistic(dryRun ? null : graph ? initialRunState(graph) : null);
    // Trimmed once here so the echoed request and the payload the host receives
    // can never disagree.
    const asked = input.trim();
    // Issue #1007: the browser's own clock, not the host's. It is what the
    // failure panel measures against and what the optimistic history row counts
    // from, and both are on screen before the host has said anything at all.
    const startedAtMillis = Date.now();
    setPendingRun({ startedAtMillis, dryRun });
    // Issue #1007: say the click landed. A synchronous run holds its request
    // open for the whole run, so the only other acknowledgement — the success
    // toast — arrives minutes later, with a button spinner and an optimistic
    // canvas in between and nothing that names the workflow. `info`, not
    // `loading`: a loading toast has no duration, and the console's toast
    // ceiling (#933) would dismiss it mid-run anyway.
    toast.info(
      dryRun
        ? `Test-running “${graph?.name ?? selectedId}” — nothing will be sent.`
        : `Running “${graph?.name ?? selectedId}”…`,
    );
    try {
      // Issue #528: run SYNCHRONOUSLY — no `detach`. The run's full `output` is
      // carried ONLY by this settled 200 body; the journal, SSE, and runs list
      // are structural (per-node status, no agent text), and there is no
      // run-detail route to fetch it back. So `RunResultPanel` — the only surface
      // that renders what a run produced — can mount only when this body reaches
      // the `setResult` branch below. The cost of asking synchronously is just
      // that the body must arrive: since #383 the host runs the sync path on a
      // spawned server task, so the run itself survives a dropped connection —
      // the connection carries the answer, it does not carry the run.
      const res = await runWorkflow(
        client,
        company,
        selectedId,
        asked ? { request: asked } : {},
        dryRun ? { dryRun: true } : undefined,
      );
      setRanWith(asked);
      // The `isDetached` branch is kept verbatim as a compatibility seam: a host
      // that answers with an acceptance for any reason still reads correctly —
      // discriminate on the SHAPE, never on what we asked for.
      if (isDetached(res)) {
        setActiveRunId(res.runId);
        setAwaitingRunId(res.runId);
        toast.success("Workflow started.");
      } else {
        setResult(res);
        setAwaitingRunId(res.runId ?? null);
        if (dryRun && !isDryRun(res)) {
          // Issue #542: we asked for a test run and the host ran it FOR REAL —
          // it predates test mode and silently ignored the flag. Discriminate on
          // the SHAPE (`isDryRun`), never on what we asked for, and say so
          // LOUDLY: real effects just fired — tokens spent, reports possibly
          // sent — which is the opposite of what the operator intended.
          toast.error("This host ran the workflow for real — it doesn't support test runs.", {
            description:
              "Your test run executed real effects (teammate turns, tools, and any report delivery). Update the host to get true no-effect test runs.",
          });
        } else {
          toast.success(dryRun ? "Test run complete — nothing was sent." : "Workflow ran.");
        }
      }
      // A dry run journals NOTHING (#542), so there is no history row to pull
      // forward for it — but a real run is journaled host-side (#228), so refresh
      // regardless: on a host that ignored the flag this is exactly the real run
      // whose row the operator needs to see.
      setRunsTick((n) => n + 1);
    } catch (e) {
      // Issue #528 / #514: triage on the STRUCTURED shape, not the message.
      const kind = classifyRunError(e, sawOwnRunStartRef.current);
      if (kind === "refusal-inference" && e instanceof ApiError) {
        // The host refused the run for a reason the operator clears from
        // Settings. Raise the persistent banner keyed on the code and swallow the
        // toast — a vanishing raw-string toast is a dead end for a fixable state.
        // Nothing ran, so drop the optimistic frontier and the in-flight guard.
        setRunRefusal({ code: e.code, message: e.message });
        setOptimistic(null);
        setActiveRunId(null);
      } else if (kind === "connection-lost") {
        // The request's connection dropped, but we saw the run start, and the
        // host's spawned task (#383) keeps walking the graph. Keep the optimistic
        // canvas and the Stop button (adopted from the live fold), tell the
        // operator where the outcome will surface, and lean on the history poll.
        toast.info(
          "The run continues on the host — watch the canvas; the outcome lands in History.",
        );
        setRunsTick((n) => n + 1);
        // Issue #1007: and OPEN the place that sentence points at. Telling
        // somebody the outcome lands in History while History is shut — it is
        // closed by default and nothing ever opened it — is the same dead end as
        // the failure toast: the drawer holds the only durable record of a run
        // whose response was lost, and it has to be on screen for that to count.
        setHistoryOpen(true);
        setAwaitingRunId(ownRunIdRef.current);
      } else {
        toast.error(e instanceof Error ? e.message : "could not run the workflow");
        // Issue #1007: the toast is now the *notification*, not the record. The
        // panel is what survives it, built from the structured error rather than
        // from its message — a code the host gave us reads differently from one
        // synthesised off a status line, and the panel says which it had.
        setRunFailure(
          runFailureFrom(e, {
            startedAtMillis,
            atMillis: Date.now(),
            request: asked,
            dryRun,
            runId: ownRunIdRef.current ?? undefined,
            sawRunStart: sawOwnRunStartRef.current,
          }),
        );
        // A run that failed is journaled too (#228), and is the outcome most
        // worth finding again later — so refresh the history on this path as well.
        setRunsTick((n) => n + 1);
        // Issue #1007: open the drawer that refresh feeds, so the journaled row
        // — the per-node trail, which names the step it died on — is on screen
        // rather than one click away behind a toolbar toggle. And hand the
        // history fetch the run id when the live fold got far enough to give us
        // one: it pulls that row forward onto the canvas (#371) for exactly the
        // console this matters most on, the one whose stream never delivered a
        // frame to paint from.
        setHistoryOpen(true);
        setAwaitingRunId(ownRunIdRef.current);
        // Drop the optimistic frontier so a failed run does not leave a node
        // pulsing "running" forever. The fold owns anything actually reported.
        setOptimistic(null);
        // Nothing was accepted, so nothing is in flight to guard against or to
        // offer a Cancel for.
        setActiveRunId(null);
      }
    } finally {
      setStarting(false);
    }
    // `request` is deliberately NOT a dependency (issue #1204): the payload
    // arrives as an argument, so this callback no longer changes identity on
    // every keystroke in the dialog.
  }, [client, company, selectedId, graph]);

  /**
   * Issue #383: stop the run this view started.
   *
   * The button goes away when the run settles rather than when this resolves —
   * the host has only *fired* the signal at that point, and the
   * `workflow_run_finished` frame is what actually says it stopped.
   *
   * A `404` here is ambiguous by design (unknown run, or already settled), so
   * the affordance is withdrawn only for **this** run, not for the session: the
   * settled-a-moment-ago case is ordinary, and a global flag would let one such
   * race hide Stop for every later run.
   */
  const cancel = useCallback(async () => {
    if (!activeRunId) return;
    setCancelling(true);
    try {
      await cancelWorkflowRun(client, company, activeRunId);
      toast.info("Stopping the run…");
    } catch (e) {
      if (e instanceof ApiError && e.status === 404) {
        setCancelUnsupportedFor(activeRunId);
        toast.error("This host can't stop runs", {
          description:
            "It's running a version without run cancellation, or the run already finished. It will still settle on its own.",
        });
      } else {
        toast.error(e instanceof Error ? e.message : "could not stop the run");
      }
    } finally {
      setCancelling(false);
    }
  }, [client, company, activeRunId]);

  // Issue #259: remove the selected workflow.
  //
  // The `version` from the graph we are looking at rides along, so this means
  // "delete the thing on my screen" rather than "delete whatever is there now".
  // If it changed underneath us the host refuses with a 409 and removes nothing,
  // and we surface that instead of quietly deleting a graph the operator never
  // saw.
  const remove = useCallback(async () => {
    if (!selectedId || !graph) return;
    const removedName = graph.name;
    setDeleting(true);
    try {
      await deleteWorkflow(client, company, selectedId, graph.version);
      // Drop it locally rather than re-listing: the host has confirmed, and a
      // re-list would flash an empty picker. A list request already in flight
      // predates this and would put the entry back — hence the bump.
      localWriteRef.current += 1;
      const remaining = workflows.filter((w) => w.id !== selectedId);
      setWorkflows(remaining);
      // Issue #1110: back to the INDEX, never to a neighbour. Selecting
      // `remaining[0]` put a workflow the operator had not asked for onto the
      // canvas under a toolbar addressed to it — the same wrong answer the tab
      // used to give on arrival, arriving a second way.
      //
      // The id is marked applied and taken out of the address bar here rather
      // than through the hash mirror: the mirror only ever hears about the
      // selection, and this is the one path that also has to stop the URL-follow
      // effect treating the id still sitting in the router's `sub` as a fresh
      // request to re-open the graph that was just deleted.
      appliedWorkflowRef.current = selectedId;
      setSelectedId(null);
      clearWorkflowFromHash();
      setGraph(null);
      setResult(null);
      setSelectedNodeId(null);
      setConflict(null);
      toast.success(`Deleted “${removedName}”.`);
    } catch (e) {
      // A 409 is the one failure the operator can actually act on, and acting
      // on it means reloading — so it gets the persistent banner, not a toast.
      if (e instanceof ApiError && e.status === 409) {
        setConflict(e.message);
      } else {
        toast.error(e instanceof Error ? e.message : "could not delete the workflow");
      }
    } finally {
      setDeleting(false);
    }
  }, [client, company, selectedId, graph, workflows]);

  // Issue #276: arm or pause this workflow's schedule.
  //
  // No `version` is sent and none is needed — this changes no graph content, so
  // there is nothing for a concurrent edit to lose, and demanding a token would
  // make a source-defined workflow untoggleable (only overlay bodies have one).
  // That is also why it is reachable when Edit and Delete are greyed out.
  const toggleEnabled = useCallback(async () => {
    if (!selectedId || !graph) return;
    // Only an explicit `false` is off — see `WorkflowSummary.enabled`.
    const next = graph.enabled === false;
    setToggling(true);
    try {
      const updated = await setWorkflowEnabled(client, company, selectedId, next);
      // Newer than any list request already in flight — see `localWriteRef`.
      localWriteRef.current += 1;
      // Take the host's answer rather than `next`: it re-read the store, so this
      // renders what is persisted instead of what we asked for.
      setGraph(updated);
      setWorkflows((prev) =>
        prev.map((w) => (w.id === updated.id ? { ...w, enabled: updated.enabled } : w)),
      );
      toast.success(
        updated.enabled === false
          ? `Paused “${updated.name}”. It won't run on its schedule; you can still run it by hand.`
          : `Resumed “${updated.name}”. It will run on its schedule again.`,
      );
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "could not change the workflow");
    } finally {
      setToggling(false);
    }
  }, [client, company, selectedId, graph]);

  /**
   * Say out loud that a write left this workflow's schedule off, and offer the
   * one click that undoes it.
   *
   * Issue #1017 wrote this for the edit path; issue #1209 extracted it, because
   * **create disarms too** and was raising a flat "Workflow created." over a
   * workflow the host had just switched off. Two write paths that produce the
   * same state owe the operator the same sentence, and a second copy of this
   * closure is a second copy of the switched-company and switched-selection
   * guards below — the two things this is careful about and the two things a
   * copy would eventually stop being careful about.
   *
   * `lead` is the only part that differs ("Saved, and paused" / "Created, and
   * paused"): what happened is not the same, what it means is.
   */
  const announceDisarm = useCallback(
    (lead: string, workflow: WorkflowGraph) => {
      toast.warning(
        `${lead} “${workflow.name}”. Its schedule is off — it won't run on its own until you resume it.`,
        {
          action: {
            label: "Resume",
            onClick: () => {
              void (async () => {
                try {
                  const resumeCompany = company;
                  const updated = await setWorkflowEnabled(client, company, workflow.id, true);
                  // The operator may have switched companies while this Resume
                  // was in flight. Discard the response — the new company's list
                  // is what matters, and mutating state keyed to the old one
                  // would overwrite it.
                  if (companyRef.current !== resumeCompany) return;
                  // Newer than any list request already in flight.
                  localWriteRef.current += 1;
                  // The operator may have selected a different workflow while
                  // this Resume was in flight. Only replace the displayed graph
                  // when it still belongs to the selection — otherwise the
                  // picker would identify the new workflow while the canvas
                  // showed (and a later edit would mutate) the old one. The list
                  // update below is safe unconditionally: it keys by id.
                  if (selectedIdRef.current === updated.id) {
                    setGraph(updated);
                  }
                  setWorkflows((prev) =>
                    prev.map((w) =>
                      w.id === updated.id ? { ...w, enabled: updated.enabled } : w,
                    ),
                  );
                  toast.success(
                    `Resumed “${updated.name}”. It will run on its schedule again.`,
                  );
                } catch (e) {
                  toast.error(
                    e instanceof Error ? e.message : "could not change the workflow",
                  );
                }
              })();
            },
          },
        },
      );
    },
    [client, company],
  );

  // Issue #259: the edit dialog saved. The host answers with the stored graph
  // AND a fresh version token, so holding onto it is what lets the operator
  // save again without a re-read — dropping it and re-fetching would be a round
  // trip that can only return the same thing.
  const handleSaved = useCallback((saved: WorkflowGraph) => {
    // Issue #1017: read the armed state BEFORE overwriting `graph`, so a save
    // that silently disarmed the workflow (a schedule edit comes back
    // `enabled: false`) can be told apart from an ordinary one below.
    const wasEnabled = graph?.enabled;
    // Newer than any list request already in flight — see `localWriteRef`.
    localWriteRef.current += 1;
    setGraph(saved);
    // The name and description are editable, so the picker entry has to move
    // with them. The id cannot change, which is what makes this a rewrite of
    // one row rather than a re-list.
    setWorkflows((prev) =>
      prev
        .map((w) =>
          w.id === saved.id
            ? {
                ...w,
                name: saved.name,
                description: saved.description,
                // Issue #276: an edit that adds a schedule comes back disarmed.
                // Carrying it here is what makes the row's switch tell the
                // operator so, off the very response that disarmed it.
                enabled: saved.enabled,
              }
            : w,
        )
        .sort((a, b) => a.name.localeCompare(b.name)),
    );
    // The write landed, so whatever we hold is current — the same reasoning as
    // the graph-load effect's clear.
    setConflict(null);
    // Issue #1017: a save that just disarmed the workflow gets a paused toast
    // with a one-click Resume, instead of a "saved" that hid that its schedule
    // was switched off. Every other save keeps the plain acknowledgement.
    if (workflowSavedToast(wasEnabled, saved.enabled) === "disarmed") {
      announceDisarm("Saved, and paused", saved);
    } else {
      toast.success("Workflow saved.");
    }
  }, [announceDisarm, graph]);

  // The creator posts the full graph back, so the new entry can be spliced
  // straight into the list and selected — no extra round trip to re-list.
  const handleCreated = useCallback((created: WorkflowGraph) => {
    // Newer than any list request already in flight — see `localWriteRef`.
    // This is the race the operator would notice most: the workflow they just
    // created disappearing out of the picker a moment after it appeared.
    localWriteRef.current += 1;
    setWorkflows((prev) => {
      const rest = prev.filter((w) => w.id !== created.id);
      return [
        ...rest,
        {
          id: created.id,
          name: created.name,
          description: created.description,
          editable: created.editable,
          // Issue #276: a workflow created WITH a schedule comes back switched
          // off. Splicing the host's answer rather than assuming `true` is what
          // makes the new row show "Resume" immediately, which is the operator's
          // only cue that the schedule is waiting on them.
          enabled: created.enabled,
        },
      ].sort((a, b) => a.name.localeCompare(b.name));
    });
    // Issue #1110: a create lands on the new workflow's DETAIL view, not back
    // on the index. Nobody authors a workflow in order to look at a list of
    // workflows — the next thing they want is its canvas, to run it or to keep
    // editing it. The hash mirror pushes that as a navigation, so Back returns
    // to the index they created it from.
    setSelectedId(created.id);
    // Issue #1209: create disarms too. `#276`'s rule switches off any workflow
    // authored WITH a schedule, and this used to acknowledge that with a flat
    // "Workflow created." — leaving the operator to notice the paused chip on a
    // detail screen nothing pointed them at. A create is armed-by-assumption
    // before the host answers, so `true` is the honest "before" to classify
    // against; the same reducer the edit path uses then names the one transition
    // worth interrupting for.
    if (workflowSavedToast(true, created.enabled) === "disarmed") {
      announceDisarm("Created, and paused", created);
    } else {
      toast.success("Workflow created.");
    }
    // Issue #1845: this console's own create is the clearest possible signal
    // — do not wait for the `workflow_created` SSE round trip to clear the
    // nudge when we already know it landed. Set BEFORE `clearNudge`, and
    // unconditionally: `clearNudge` only marks the row read when it already
    // knows the nudge's id (`nudgeRef.current`), which a fetch still in
    // flight at this instant has not supplied yet — see
    // `hasCreatedLocallyRef`'s own doc comment for how `refreshNudge`
    // reconciles that response when it lands.
    hasCreatedLocallyRef.current = true;
    clearNudge();
  }, [announceDisarm, clearNudge]);

  // Issue #1110: leave the workflow on screen and go back to the index.
  //
  // A push, unlike every other route to the index in this file: those are the
  // view correcting itself off a workflow that stopped existing, this is a
  // place the operator chose to go. Pushing keeps browser Back and this button
  // exact inverses of each other, which is what makes the pair learnable.
  //
  // Closing the per-workflow drawers is NOT done here — the effect that watches
  // the selection does it, so that every route back to the index closes them and
  // not just this one.
  const backToIndex = useCallback(() => {
    setSelectedId(null);
    const { onWorkflows, workflowId } = readWorkflowHash();
    if (onWorkflows && workflowId !== null) window.location.hash = "/workflows";
  }, []);

  // Issue #840 (PR-3): correct a failed run's workflow with the copilot. The
  // affordance lives on the journaled failed run (keyed by runId) — the one
  // surface that always carries the failure. On a corrected graph it opens the
  // edit dialog hydrated from the correction (which keeps the same id, so Save is
  // a new version); on an un-fixable failure it shows the reason inline under the
  // run. The failed run's workflow is the selected one (the history is per
  // selection), so the edit dialog's `graph` supplies the version token.
  const handleFixWithCopilot = useCallback(
    async (run: WorkflowRunOutcome) => {
      if (!run.runId) return;
      // Issue #1704 (review): the selection this request belongs to. Any switch
      // away invalidates it permanently, including one the operator switches
      // back from.
      const startedAtGen = selectionGenRef.current;
      setFixingRunSeq(run.seq);
      setFixReason(null);
      try {
        const res = await fixWorkflowFromRun(client, company, run.workflowId, {
          runId: run.runId,
          errorHint: run.error,
        });
        // This reply is about a run of `run.workflowId` in `company`, and
        // NEITHER outcome may land anywhere else.
        //
        // The correction arm has always checked this: the edit dialog binds to
        // the SELECTED workflow's `graph` for its version token, so opening it
        // after a switch would write this correction over a different workflow.
        //
        // Issue #1704: the un-fixable arm needs the same guard, and for a
        // sharper reason than symmetry. Clearing `fixReason` on the switch is
        // not enough on its own — the switch happens while this request is
        // still in flight, so the clear runs FIRST and the assignment below
        // would put the reason straight back, keyed by a `seq` that now names
        // an unrelated run. The guard is what makes the clear stick.
        if (
          selectionGenRef.current !== startedAtGen ||
          selectedIdRef.current !== run.workflowId ||
          companyRef.current !== company
        ) {
          toast.message(
            "Selection changed while the copilot was working — reopen Fix on that run to review its correction.",
          );
          return;
        }
        if (res.automatable && res.workflow) {
          setPrefilledDraft({
            summary: res.summary,
            workflow: res.workflow,
            notes: res.notes,
            readiness: res.readiness,
          });
          setEditOpen(true);
        } else {
          setFixReason({
            seq: run.seq,
            reason: res.reason ?? "the copilot could not correct it.",
          });
        }
      } catch (e) {
        // A capability gap (404/409) or a network failure — surface it and leave
        // the run row untouched; the operator can still edit by hand.
        toast.error(
          e instanceof Error ? e.message : "Couldn't reach the workflow copilot.",
        );
      } finally {
        // Only the run that set the slot may clear it — if a second Fix started
        // on another row while this one was in flight, this `finally` firing
        // first must not re-enable that still-running row's button.
        //
        // Issue #1704: and only while that run's own workflow is still on
        // screen. `seq` is allocated per company rather than per workflow, so
        // after a switch `run.seq` can name a DIFFERENT workflow's run whose
        // fix is genuinely running. The switch has already emptied this slot,
        // so there is nothing here left for this request to clear anyway.
        //
        // Issue #1704 (review): and the generation, because after A → B → A the
        // slot can be full again — with the operator's RETRY of this very run,
        // whose `seq` is identical. Clearing on identity alone would switch that
        // still-running row's spinner off and re-enable every Fix button.
        if (
          selectionGenRef.current === startedAtGen &&
          selectedIdRef.current === run.workflowId &&
          companyRef.current === company
        ) {
          setFixingRunSeq((current) => (current === run.seq ? null : current));
        }
      }
    },
    [client, company],
  );

  // Issue #371: the live canvas state, FOLDED from the frame window rather than
  // accumulated frame by frame.
  //
  // A fold is what makes this correct under React batching: several frames can
  // land in one render, and an accumulating reducer would see only the last —
  // losing a `workflow_run_started` that way strands every node frame behind
  // it. Recomputing from the window instead has no such state to lose.
  //
  // Issue #863: seeded with the run the HOST says is open, for the case the
  // window cannot cover. The window only holds what arrived since this console
  // connected, so a run that was already walking when the tab was opened (a
  // cron fire, a run started from chat, a reload, an `EventSource` reconnect)
  // has no start frame here and used to paint nothing at all — the whole run,
  // blank, which is what #863 reports. The history read already carries that
  // run with `running: true` and the nodes it has finished so far.
  const inFlightSeed = useMemo(() => {
    if (!selectedId || runsFor !== selectedId) return null;
    // Only a run this console has NOT been following: a run whose frames are in
    // the window folds from them, which is both fresher and the path every
    // existing guarantee is written against.
    const row = runs.find(
      (r) => r.runId && (r.running || r.runId === adoptedFromHistoryRef.current),
    );
    if (!row?.runId) return null;
    // Issue #1010: ask the WINDOW, not a set of every run this console has ever
    // seen a frame for. The fold only supersedes this seed when it can find the
    // run's own start frame, and the window is a rolling 300 that evicts — so
    // the old "has watched it live, ever" reading withheld the seed from a fold
    // that could no longer cover the run, and the canvas blanked. Switching
    // workflow away and back mid-run was the reliable way to see it: the ref
    // survived the switch while `adoptedFromHistoryRef` was cleared, so neither
    // clause held and `inFlightSeed` returned null for a run still going.
    if (
      windowHasRunStart(runEvents, row.runId) &&
      row.runId !== adoptedFromHistoryRef.current
    ) {
      return null;
    }
    return {
      runId: row.runId,
      states: statesFromRun(row),
      elapsed: elapsedFromRun(row),
      scheduled: row.scheduled,
    };
    // `runEvents` joins the deps with the guard above (issue #1010): the seed's
    // answer now depends on what the window holds, so it has to recompute when
    // the window changes.
  }, [runs, runsFor, selectedId, runEvents]);

  // Issue #921: the runs the HOST reports as no longer in flight. This is the
  // only authority that survives a dead stream — the `workflow_run_finished`
  // frame the fold would otherwise wait on is precisely the one that goes
  // missing when the console stops updating mid-run.
  //
  // Scoped to the selected workflow's own history for the same reason
  // `inFlightSeed` is: a run id is unique, but a list read for another workflow
  // cannot contain this run anyway, and pairing the two guards keeps one rule
  // rather than two.
  const settledRunIds = useMemo(() => {
    if (!selectedId || runsFor !== selectedId) return undefined;
    const ids = new Set<string>();
    for (const r of runs) if (r.runId && !r.running) ids.add(r.runId);
    return ids;
  }, [runs, runsFor, selectedId]);

  const liveRun = useMemo(
    () => foldLiveRun(runEvents, selectedId, graph, inFlightSeed, settledRunIds),
    [runEvents, selectedId, graph, inFlightSeed, settledRunIds],
  );

  // The optimistic frontier is only for the gap before the first frame. Once
  // the fold has adopted a run, it is the authority — and that run is recorded
  // so the no-stream fallback knows it was watched live.
  useEffect(() => {
    if (!liveRun) return;
    liveRanRef.current.add(liveRun.runId);
    // Issue #863: remember an adoption that came from the history rather than
    // from a start frame, so the seed survives the run settling.
    if (inFlightSeed && inFlightSeed.runId === liveRun.runId) {
      adoptedFromHistoryRef.current = liveRun.runId;
    }
    setOptimistic(null);
  }, [liveRun, inFlightSeed]);

  // Issue #528: adopt the live run's id while the synchronous POST is still in
  // flight.
  //
  // The always-detach path (#383) used to seed `activeRunId` straight from the
  // acceptance body, which is what put the mid-run Stop button on screen. Running
  // synchronously (so the settled body can carry the output) removed that seed,
  // so restore it from the live fold: while the request is open and the fold has
  // adopted OUR own — non-scheduled — run, take its id. A concurrent cron fire is
  // `scheduled` and must never be adopted here. Recording that we saw our own
  // start frame is also what lets the run() catch tell a survivable dropped
  // connection ("the run continues on the host") from a run that never began.
  useEffect(() => {
    if (!starting || !liveRun || !liveRun.active || liveRun.scheduled) return;
    sawOwnRunStartRef.current = true;
    // Issue #1007: the same seed, kept for the catch below.
    ownRunIdRef.current = liveRun.runId;
    if (activeRunId === null) setActiveRunId(liveRun.runId);
  }, [starting, liveRun, activeRunId]);

  // Issue #383: release the Run guard when the run we started actually settles,
  // as reported by the live frames. This is the fast path.
  useEffect(() => {
    if (!activeRunId || !liveRun) return;
    if (liveRun.runId === activeRunId && !liveRun.active) setActiveRunId(null);
  }, [liveRun, activeRunId]);

  // …and the same from the history, for a console with no live stream. Without
  // this the button would stay disabled forever on exactly the deployments the
  // detached run was added to help. `running` on a row is the host's own
  // in-flight marker, so a settled row is the release signal.
  useEffect(() => {
    if (!activeRunId) return;
    const mine = runs.find((r) => r.runId === activeRunId);
    if (mine && !mine.running) setActiveRunId(null);
  }, [runs, activeRunId]);

  // …but that release needs something to *re-read* the history, and without SSE
  // nothing does.
  //
  // The fetch fired when the run was accepted usually returns the row already
  // marked `running: true`, and the effect above only ever sees that one stale
  // snapshot: `runsTick` is bumped by the run POST and `runEventTick` only by
  // frames that are not arriving. So `activeRunId` stayed set, the Run controls
  // stayed disabled, and the view was wedged for the rest of the session — on
  // precisely the deployments detaching a run was meant to help.
  //
  // Polling stops on its own: the effect above clears `activeRunId` the moment
  // a settled row lands, and that unmounts this interval. With a working stream
  // the live fold clears it first, so at most one extra fetch is spent.
  //
  // Issue #921: it polls while the CANVAS believes a run is walking too, not
  // only while `activeRunId` is set. Those are different states, and the gap
  // between them is this bug: `activeRunId` is only ever adopted for a run this
  // console STARTED (the `starting` guard above), so a run being watched — a
  // cron fire, a run started from chat, a run adopted from history after a
  // reload — polled nothing. When its stream then died mid-run there was no
  // frame to settle the fold and no poll to correct it, so the canvas kept a
  // node pulsing and the header kept reading "running" until someone reloaded.
  //
  // Same self-limiting shape as before: `settledRunIds` clears `liveRun.active`
  // on the first settled row, which unmounts this interval. With a working
  // stream the finish frame gets there first and this costs at most one fetch.
  const watchingRun = Boolean(activeRunId) || Boolean(liveRun?.active);
  useEffect(() => {
    if (!watchingRun || !historySupported) return;
    // Issue #1009: a bare `setInterval` kept firing in a hidden tab, so a run
    // wedged "running" (a finish that never journaled) had a background console
    // re-reading history every 2s forever. `startVisiblePolling` pauses the
    // cadence while the tab is hidden and resumes — with one immediate read — on
    // the visible edge, so a backgrounded console stops asking. Foreground
    // behaviour is unchanged: the same 2s tick, and the backend's #1009
    // cross-check now settles the row so the next read clears `activeRunId`
    // (see the effect above) and this poll unmounts on its own.
    return startVisiblePolling(() => setRunsTick((n) => n + 1), 2_000);
  }, [watchingRun, historySupported]);

  // Switching workflow (or company) clears the canvas: another graph's node ids
  // are meaningless here, and a stale mark on a same-named node would be a lie.
  //
  // It also drops the in-flight guard: the run keeps going host-side and still
  // journals, but this view is no longer the place watching it, and leaving a
  // Cancel button pointed at another workflow's run would be worse than losing
  // the affordance.
  //
  // Issue #528: it must also drop the run RESULT and any run refusal. Now that a
  // synchronous run populates `result` on every modern host, a stale run-output
  // drawer (or a "no inference" banner) would otherwise outlive the switch and
  // read as belonging to the newly-selected workflow. The graph fetch clears
  // `result` too, but making it explicit here keeps both switch axes honest even
  // if the graph load is skipped or in flight.
  useEffect(() => {
    // Issue #1704 (review): every reply still in flight belongs to the selection
    // being torn down here, and stays invalid even if the operator comes back to
    // it. See `selectionGenRef` for what identity alone cannot tell apart.
    selectionGenRef.current += 1;
    setOptimistic(null);
    setOverlayRun(null);
    setActiveRunId(null);
    setResult(null);
    setRunRefusal(null);
    // Issue #1007: and the failed run's drawer, for exactly the same reason —
    // it names a run of the workflow being left, and left up it would read as
    // the newly-selected one's.
    setRunFailure(null);
    setPendingRun(null);
    // Issue #1204: and the run-input draft, which names work for the workflow
    // being LEFT — "the Q3 board deck" carried onto another workflow's dialog is
    // the same category of wrong as the drawers above, and the dialog itself
    // must not survive a switch it was opened on the other side of.
    setRequest("");
    setRunInputOpen(false);
    // Issue #863: the adopted run belonged to the workflow being left. Holding
    // it across the switch would keep painting one graph's trail onto another's
    // canvas the moment the two share a node id.
    adoptedFromHistoryRef.current = null;
    // Issue #1010: and its sibling, which was NOT cleared here — the asymmetry
    // that made switching away and back mid-run blank the canvas. The two refs
    // answer the same question from opposite sides and have to have the same
    // lifetime; leaving one behind is how "this console watched that run" came
    // to outlive the console's view of it.
    liveRanRef.current = new Set();
    // Issue #1704: the two copilot-fix slots. Both are keyed by run `seq`, and
    // `seq` is allocated per COMPANY rather than per workflow — so a value left
    // behind does not merely go unread, it lands on whichever run of the newly
    // selected workflow happens to share that number.
    //
    // `fixingRunSeq` is the worse of the two, because `RunHistoryPanel` disables
    // EVERY row's Fix button while it is set (one fix at a time). A leaked one
    // therefore does not just spin a row that is not fixing — it takes the
    // affordance away from a workflow no fix was ever requested for, until an
    // unrelated request the operator cannot see finishes.
    //
    // Clearing here is only half of it: the request that set them is still in
    // flight and would write them back. `handleFixWithCopilot` carries the
    // other half.
    setFixingRunSeq(null);
    setFixReason(null);
    // Issue #1704: and the version-conflict banner — the last of the persistent
    // banners still outliving the switch, after `result` (#528), `runRefusal`
    // (#528/#514) and `runFailure` (#1007) were each cleared here in turn for
    // exactly this reason. It states that the graph on screen is stale and
    // offers a Reload that re-reads the NEW selection: a false claim with a
    // remedy that quietly addresses something else. A successful graph read
    // clears it — but a graph read that FAILS does not, which is precisely the
    // case where the operator is left staring at it.
    setConflict(null);
    // Issue #1704: and the graph-load error, whose reach is wider still. It
    // renders outside the `detailOpen` gate, so "could not load the workflow
    // graph" about the workflow just left follows the operator all the way back
    // to the index and sits over a list that loaded perfectly.
    //
    // Issue #1704 (review): `graphError` ONLY. This used to be one `error` slot
    // shared with the workflow-list read, and clearing that here threw away a
    // company-wide "could not load workflows" at the exact moment the operator
    // returned to the list it describes — a stale list with nothing saying so.
    // The list read is keyed on the company, so its failure is cleared on the
    // company axis instead (see the `[company]` effect above).
    setGraphError(null);
  }, [selectedId, company]);

  // Issue #339: `?run=<runId>` — open the canvas showing that past run.
  //
  // Declared AFTER the clear effect above on purpose. Both can fire while a
  // deep link resolves, and effects run in declaration order, so an overlay set
  // here would otherwise be wiped by the very selection change that fetched the
  // history it came from.
  //
  // Run ids arrive late by nature: the history is a separate fetch, so this
  // waits for the SELECTED workflow's own history rather than reading whatever
  // list is currently in `runs` (which, until then, is the previous
  // workflow's).
  useEffect(() => {
    if (!requestedRunId) {
      // The link no longer names a run — the operator moved to another
      // workflow, or the hash was rewritten. Forget what was applied, so the
      // same id arriving again later reads as a fresh request rather than as an
      // echo of the one already handled.
      appliedRunRef.current = null;
      return;
    }
    if (appliedRunRef.current === requestedRunId) return;
    if (!selectedId || runsFor !== selectedId) return;
    appliedRunRef.current = requestedRunId;
    // `runId` is optional on the wire: a row journaled before the entry point
    // minted correlation ids carries none. Comparing it directly would let a
    // `requestedRunId` of `undefined` match the first such row — hence the
    // explicit presence check even though `requestedRunId` is a string here.
    const match = runs.find((r) => r.runId != null && r.runId === requestedRunId);
    if (!match) {
      // Never leave the canvas mid-gesture: it keeps painting the live state,
      // and the operator is told why the run they asked for isn't on it.
      toast.error("That run isn't in this workflow's run history.", {
        description:
          "It may have aged out of the journal, or belong to a different workflow. The canvas is showing the current state instead.",
      });
      return;
    }
    if ((match.nodes?.length ?? 0) === 0) {
      // Same guard the no-live-stream fallback uses: with no per-node trail
      // there is nothing to paint, and the overlay banner would claim every
      // node "was never reached" — a statement about the run that isn't true,
      // only about what was recorded of it.
      toast.info("That run has no step-by-step trail to show on the canvas.", {
        description:
          "It was recorded before per-node progress was journaled. Its delivery rows are in History.",
      });
      return;
    }
    setOverlayRun(match);
  }, [requestedRunId, runs, runsFor, selectedId]);

  // The Run guard, derived rather than held: the request is in flight, or a run
  // we started has not settled yet.
  const running = starting || activeRunId !== null;

  /**
   * Issue #1204: dispatch from the run-input dialog.
   *
   * Closes first, then runs. A synchronous run holds the request open for the
   * whole run (#528), so a dialog left up would sit over the canvas and the
   * history rail — the two surfaces that report what is happening — for as long
   * as the run takes.
   *
   * The draft is deliberately NOT cleared: the operator who ran on "Q3 board
   * deck" and wants to run again on "Q4 board deck" should be editing two
   * characters, not retyping the line. It is cleared when the workflow changes,
   * where it stops being about the work in front of them.
   */
  const runWithInput = useCallback(
    (dryRun: boolean) => {
      // The buttons are disabled in these states and Enter bypasses them, so the
      // guard is here rather than only on the controls.
      if (!selectedId || running || loadingGraph) return;
      setRunInputOpen(false);
      void run(dryRun, request);
    },
    [selectedId, running, loadingGraph, run, request],
  );

  // What the canvas actually paints, in priority order: a past run the operator
  // explicitly asked to see, else the live fold, else the optimistic frontier.
  const paintedStates = useMemo<Record<string, NodeRunState>>(() => {
    if (overlayRun) return statesFromRun(overlayRun);
    if (liveRun) return liveRun.states;
    return optimistic ?? {};
  }, [overlayRun, liveRun, optimistic]);
  const paintedElapsed = useMemo<Record<string, number>>(() => {
    if (overlayRun) return elapsedFromRun(overlayRun);
    return liveRun?.elapsed ?? {};
  }, [overlayRun, liveRun]);
  // Issue #981, in the same priority order. The live SSE fold carries no
  // delivery rows — delivery happens after the engine returns, so they arrive
  // with the settled body — and correctly contributes nothing here. The
  // just-finished manual run DOES have them, and is the case the issue was
  // filed about: an operator presses Run, watches the output node land on DONE,
  // and the report is gone. It is matched on `runId` so a stale result cannot
  // mark up a different run's canvas.
  const paintedUndelivered = useMemo<Set<string>>(() => {
    if (overlayRun) return undeliveredNodes(overlayRun.deliveries);
    if (result && (!liveRun || liveRun.runId === result.runId)) {
      return undeliveredNodes(result.deliveries ?? []);
    }
    return EMPTY_UNDELIVERED;
  }, [overlayRun, result, liveRun]);

  const { nodes, edges } = useMemo(
    () =>
      graph
        ? layout(graph, paintedStates, paintedElapsed, paintedUndelivered)
        : { nodes: [], edges: [] },
    [graph, paintedStates, paintedElapsed, paintedUndelivered],
  );

  const selected = workflows.find((w) => w.id === selectedId) ?? null;

  // The full node model (kind/name/summary/agent/config) for the clicked node,
  // looked up from the loaded graph so the inspector shows fields the laid-out
  // canvas node data doesn't carry (agent, config, …).
  const selectedNode = useMemo(
    () => graph?.nodes.find((n) => n.id === selectedNodeId) ?? null,
    [graph, selectedNodeId],
  );

  // Resolve an agent id to the teammate's display name in the inspector. Keep
  // this lazy: workflows with no selected agent node do not need a roster read.
  useEffect(() => {
    if (!selectedNode?.agent) {
      setNodeRoster([]);
      return;
    }
    let live = true;
    void client
      .listTeam(company)
      .then((team) => {
        if (live) setNodeRoster(team);
      })
      .catch(() => {
        if (live) setNodeRoster([]);
      });
    return () => {
      live = false;
    };
  }, [client, company, selectedNode?.agent]);

  // Issue #596: lazily fetch a past run's per-node output ONCE when it is
  // overlaid, so clicking any of its nodes can show what that node produced. A
  // 404 (predates capture / dry / hard-aborted / older host) settles to `record:
  // null`, which the inspector renders as an empty state rather than an error.
  const overlayRunId = overlayRun?.runId ?? null;
  useEffect(() => {
    if (!overlayRunId) {
      setOverlayOutput(null);
      return;
    }
    let cancelled = false;
    setOverlayOutput({ runId: overlayRunId, loading: true, record: null });
    workflowRunOutput(client, company, overlayRunId)
      .then((record) => {
        if (!cancelled) setOverlayOutput({ runId: overlayRunId, loading: false, record });
      })
      .catch(() => {
        // 404 (no captured output) or an older host without the route — either
        // way there is nothing to show, which the inspector states plainly.
        if (!cancelled) setOverlayOutput({ runId: overlayRunId, loading: false, record: null });
      });
    return () => {
      cancelled = true;
    };
  }, [overlayRunId, client, company]);

  // The clicked node's output on the run being inspected (issue #596): a past
  // run's node reads from the lazily-fetched durable snapshot; a live run's node
  // reads from the in-memory result — the same Output section renders both.
  // `undefined` when no run is being inspected, so the inspector shows only the
  // node's static config, exactly as before.
  const selectedNodeOutput = useMemo<NodeOutputView | undefined>(() => {
    if (!selectedNode) return undefined;
    if (overlayRun) {
      if (!overlayRun.runId) return { state: "unavailable" };
      if (!overlayOutput || overlayOutput.runId !== overlayRun.runId || overlayOutput.loading) {
        return { state: "loading" };
      }
      if (!overlayOutput.record) return { state: "unavailable" };
      const value = nodeOutputFor(overlayOutput.record.nodes, selectedNode.id);
      if (value === undefined) return { state: "unavailable" };
      // Issue #1008: a failed/blocked run's snapshot is flagged partial; carry it
      // so the inspector badges the capture. A live run (below) is never partial.
      return {
        state: "present",
        value,
        truncated: overlayOutput.record.truncated,
        partial: overlayOutput.record.partial,
      };
    }
    if (result) {
      const value = nodeOutputFor(result.output, selectedNode.id);
      if (value === undefined) return { state: "unavailable" };
      return { state: "present", value, truncated: false };
    }
    return undefined;
  }, [selectedNode, overlayRun, overlayOutput, result]);

  const onNodeClick = useCallback((_: unknown, node: Node) => {
    setSelectedNodeId(node.id);
  }, []);

  // Issue #1231: the reveal pan below has to know when the operator takes the
  // canvas over, so it can stop having an opinion about where the canvas
  // belongs. `onMove` forwards d3-zoom's `sourceEvent`, which React Flow leaves
  // null for a programmatic transition and sets to the real pointer or wheel
  // event for a gesture on the canvas. Truthy means theirs.
  //
  // "Programmatic" covers the reveal's own pan and `WorkflowMiniMap`'s
  // `setCenter`, so a minimap drag would not register here. That is not a hole
  // today only because the minimap sits at the canvas's bottom-right, which is
  // exactly where the inspector overlay is: the panel covers it whenever there
  // is a reveal to take over from. Anything that gives the operator a
  // programmatic way to move the canvas WHILE the inspector is open has to call
  // `operatorTookOver` itself.
  const revealRef = useRef<RevealSelectedNodeHandle | null>(null);
  const onMove = useCallback((event: MouseEvent | TouchEvent | null) => {
    if (event) revealRef.current?.operatorTookOver();
  }, []);

  // Issue #1007: what the history drawer renders — the host's rows, with one
  // optimistic row on top while this console has a run in flight that the host
  // has not journaled yet.
  //
  // Deliberately NOT folded into `runs`: everything else that reads that list
  // reads it as the host's record. The last-run chip, the copilot's grounding,
  // the in-flight seed and the settled-run set would all be reasoning about a
  // row the host has never seen.
  const historyRows = useMemo<WorkflowRunOutcome[]>(() => {
    // A dry run journals nothing (#542), so a row for it would appear in "Run
    // history" and then vanish when the request settles — worse than the button
    // spinner it was meant to improve on.
    if (!pendingRun || pendingRun.dryRun) return runs;
    // Settled, either way: `starting` is the POST still open, `activeRunId` the
    // run this console adopted from the live fold. With neither, the journal is
    // the whole answer and a synthetic row could only contradict it.
    if (!starting && !activeRunId) return runs;
    // The host's own row for this run has arrived. It carries the per-node
    // trail, so it supersedes this one rather than sitting under it.
    if (activeRunId && runs.some((r) => r.runId === activeRunId)) return runs;
    return [
      {
        // `seq` is this list's React key and the id `selectedRunSeq` compares
        // against, so it has to be stable and unable to collide with a real
        // journal position. Negative is both.
        seq: -1,
        atMillis: pendingRun.startedAtMillis,
        startedAtMillis: pendingRun.startedAtMillis,
        workflowId: selectedId ?? "",
        scheduled: false,
        runId: activeRunId ?? undefined,
        deliveries: [],
        pendingApprovals: [],
        running: true,
      },
      ...runs,
    ];
  }, [runs, pendingRun, starting, activeRunId, selectedId]);

  // The failure panel can only promise a node or copilot repair once the
  // journal has returned the matching durable run. Until then it can still
  // open History, but never guesses at a row based on time or position.
  const failureRun = useMemo(
    () =>
      runFailure?.runId
        ? runs.find((run) => run.runId === runFailure.runId) ?? null
        : null,
    [runFailure, runs],
  );
  const failureNode = failureRun ? failedNodeOf(failureRun) : null;

  // `runs` already holds only the selected workflow's runs, newest first — the
  // host filters and orders them. Re-filtering here would be a second source of
  // truth that can only ever disagree with the first.
  const lastRun = runs[0] ?? null;

  // Issue #259. `editable === false` is the host saying "PUT/DELETE on this id
  // will 409" — a source-defined graph, or a name-only entry with no saved
  // graph. A host predating #259 sends no field at all, and `undefined` must NOT
  // read as a refusal, so only an explicit `false` disables the affordance.
  const notEditable = graph?.editable === false;
  const canDelete = !!graph && !notEditable && !deleting;
  // Same predicate as Delete, and deliberately the same explanation: the host
  // refuses both writes for the same reason, with one message.
  const canEdit = !!graph && !notEditable && !loadingGraph && !deleting;
  const notEditableReason = graph
    ? `“${graph.name}” is defined by a file in the company source tree, so it can't be changed or removed from the console. Edit workflows/${graph.id}.toml in the company repository instead.`
    : undefined;
  // Issue #276. Same "only an explicit `false`" rule as `editable`: a host
  // predating #276 sends no field, and `undefined` must not render as paused.
  const paused = graph?.enabled === false;
  // Deliberately NOT gated on `notEditable`. Pausing writes to the company
  // record, not the source tree, so a source-defined workflow — the one an
  // operator most needs to stop without a redeploy — is toggleable even though
  // Edit and Delete beside it are not.
  const canToggle = !!graph && !toggling && !loadingGraph;
  // Only a trigger schedule makes the switch mean anything. A manual workflow
  // has nothing to pause, and offering the control anyway would suggest it does
  // — so it is hidden rather than shown greyed, which would read as "disabled
  // for now".
  const isScheduled = !!graph?.nodes.some((n) => n.kind === "trigger" && !!n.schedule);

  return (
    <div className="flex flex-1 flex-col overflow-hidden">
      {/* Issue #1135: the tab's toolbar, in two shapes.

          On the INDEX it is one row: where you are, and the two controls that
          act on the list — how it is drawn, and `New workflow`, which is this
          screen's primary action because making one is what an operator comes
          to a list of workflows to do.

          Inside a WORKFLOW it is two rows, because the controls answer two
          different questions and one undifferentiated strip of nine made
          neither of them readable. Row 1 is identity and state — where am I,
          and how is it doing. Row 2 is action — what do I want to do to it.
          `Run` is the only filled button on the detail screen: two primaries on
          one screen means neither reads as the main action, which is why `New
          workflow` moved to the index rather than being demoted here. */}
      {detailOpen ? (
        <div className="border-b px-4 py-3">
          <div className="flex flex-col gap-3" data-testid="workflow-detail-toolbar">
            {/* ── row 1 · identity and state ─────────────────────────────
                Issue #1110: the heading says where you are — this workflow's
                name, behind the control that goes back to the list, which is
                the ordinary shape of a list → detail pair and is what makes
                the two states tell themselves apart at a glance.

                Issue #1135 dropped the workflow picker that used to sit below
                it. You are already inside this workflow; its name is right
                here and "All workflows" goes back. Switching between them is
                what the index is for now. */}
            <div
              className="flex min-w-0 flex-col gap-1"
              data-testid="workflow-detail-identity"
            >
              <div className="flex min-w-0 flex-wrap items-center gap-2">
                {/* Issue #1110: what "Browse" became. It used to toggle a panel
                    over the canvas; the panel is the tab's front door now, so
                    the button that pointed at it is the way back out. Named for
                    the destination rather than the gesture — "Back" alone would
                    be ambiguous next to the browser's own Back, which lands in
                    the same place from here on purpose. */}
                <Button
                  size="sm"
                  variant="ghost"
                  className="-ml-2 h-8 shrink-0 px-2 text-muted-foreground hover:text-foreground"
                  onClick={backToIndex}
                  data-testid="workflow-back-to-index"
                  title="Back to every workflow in this company."
                >
                  <ArrowLeft className="mr-1.5 size-4" />
                  All workflows
                </Button>
                {/* Navigation is not identity. The hairline says so, so the
                    name reads as a heading rather than as the next link. */}
                <span aria-hidden className="h-4 w-px shrink-0 bg-border" />
                <h1 className="min-w-0 truncate text-sm font-semibold" data-testid="workflow-detail-name">
                  {selected?.name ?? graph?.name ?? selectedId}
                </h1>
                {/* Issue #228: the last run's outcome at a glance — including for
                    a scheduled run nobody watched, which is the case the issue is
                    about. Absent until this workflow has run at least once. */}
                {lastRun && <LastRunChip run={lastRun} />}
                {/* Issue #276: state an operator must not have to hover to learn.
                    A paused workflow looks exactly like a live one otherwise, and
                    the case that matters most — a schedule the disarm rule
                    switched off on create — has never run, so there is no
                    LastRunChip to hint at it. */}
                {paused && (
                  <Badge variant="outline" data-testid="workflow-paused-badge">
                    Schedule paused
                  </Badge>
                )}
              </div>
              {/* Issue #1135: its own line. Beside the chips it was the flexible
                  child of a row whose every other item is `shrink-0`, so it
                  truncated first and hardest — often to a few words — while
                  competing with them for the same reading. A line to itself both
                  gives it the width to be read and leaves the chips alone. */}
              {selected?.description && (
                <p
                  className="truncate text-xs text-muted-foreground"
                  data-testid="workflow-detail-description"
                >
                  {selected.description}
                </p>
              )}
            </div>

            {/* ── row 2 · action ─────────────────────────────────────────
                Three groups, hairline-separated, in the order an operator
                reaches for them: what to run, then the things that inspect a
                run, then the two that change or remove the workflow itself. */}
            <div
              className="flex flex-wrap items-center gap-2"
              data-testid="workflow-detail-actions"
            >
              {/* run intent — the point of the screen, and the only filled
                  control on it.

                  Issue #1204: it is ONE control in two halves now, not a text
                  box and a button. The box — "What should this run work on?" —
                  was the widest thing on a row of nine and empty on almost every
                  visit, so the common case (press Run) was reached past an input
                  that the uncommon case needed. The halves invert that: Run is a
                  click, and the payload is a click plus a dialog.

                  What the second half must NOT become is a hiding place. The
                  capability behind it is real (issue #154 — the host seeds the
                  payload as the trigger node's item, and a first step bound to
                  `=items` reads it), which is exactly why the draft is passed to
                  `run()` explicitly rather than left in a closure: pressing the
                  left half always means "no input", visibly and always. */}
              <div className="flex min-w-0 items-center gap-2">
                <div className="flex items-center" data-testid="workflow-run-split">
                  <Button
                    size="sm"
                    onClick={() => void run()}
                    disabled={!selectedId || running || loadingGraph}
                    data-testid="workflow-run"
                    className="rounded-r-none"
                  >
                    {running ? (
                      <Loader2 className="mr-1.5 size-4 animate-spin" />
                    ) : (
                      <Play className="mr-1.5 size-4" />
                    )}
                    Run
                  </Button>
                  {/* The other half. Same fill and no gap, because a detached
                      outline button beside Run reads as a ninth control rather
                      than as more of this one; the hairline is what says the two
                      belong together. Icon-only, with the name carried by
                      `sr-only` text so it is announced and so it does not widen
                      the row back out.

                      Its `disabled` predicate is Run's, copied whole and not
                      narrowed: opening the dialog against a graph that has not
                      loaded would offer a run that cannot be dispatched. */}
                  <Button
                    size="sm"
                    onClick={() => setRunInputOpen(true)}
                    disabled={!selectedId || running || loadingGraph}
                    data-testid="workflow-run-with-input"
                    className="rounded-l-none border-l border-primary-foreground/25 px-2"
                    title="Run this workflow on something specific — a topic, a link, a question. The first step receives it."
                  >
                    <ChevronDown className="size-4" />
                    <span className="sr-only">Run with input…</span>
                  </Button>
                </div>
                {/* Issue #383. Present only while a run this view started is actually
                    in flight — which is knowable at all because the host now hands
                    back the run id before the run ends. Hidden once the host has told
                    us it cannot stop runs, rather than left there to fail every time.
                    It belongs to the run group: it is the same run, undone. */}
                {activeRunId && cancelUnsupportedFor !== activeRunId && (
                  <Button
                    size="sm"
                    variant="outline"
                    onClick={() => void cancel()}
                    disabled={cancelling}
                    data-testid="workflow-cancel-run"
                    title="Stop this run. Steps that already finished stay in the run history; a step still executing is stopped where it is, not finished."
                  >
                    {cancelling ? (
                      <Loader2 className="mr-1.5 size-4 animate-spin" />
                    ) : (
                      <Square className="mr-1.5 size-4" />
                    )}
                    Stop
                  </Button>
                )}
              </div>

              {/* secondary — prove it, ask about it, read what it did, put its
                  schedule back. None of these is the main action, and they all
                  look alike so that Run does not.

                  The rule is INSIDE the group it introduces, not between the
                  two: at narrow widths the row wraps, and a divider that is a
                  sibling of both groups gets left dangling at the end of the
                  line the group it belonged to just left. */}
              <div className="flex flex-wrap items-center gap-2">
                <ToolbarDivider />
                {/* Issue #542: a dry run — the real graph over stubbed effects, so
                    nothing is sent and no tokens are spent. Shares the exact `run()`
                    dispatch (and its #528 error triage) as the Run button, passing the
                    dry flag; the result panel says what it was. */}
                <Button
                  size="sm"
                  variant="outline"
                  onClick={() => void run(true)}
                  disabled={!selectedId || running || loadingGraph}
                  data-testid="workflow-test-run"
                  title="Test run: walk the real workflow over stubbed effects to prove its routing and output shape. Nothing is sent, and no tokens are spent."
                >
                  <FlaskConical className="mr-1.5 size-4" />
                  Test run
                </Button>
                {/* Issue #1110: the Browse toggle that used to sit here is gone. It
                    opened the index over the canvas, and the index is what the tab
                    opens on now — a button that toggles the surface you arrived
                    through is one control for two states with one name. Its job as a
                    way back is done by "All workflows" at the head of row 1, where
                    a back affordance belongs. */}
                {/* Issue #303. Needs a loaded graph, not just a selection: the
                    copilot's whole grounding IS the graph, and opening it against a
                    workflow that failed to load would give it nothing to answer
                    from. */}
                <Button
                  size="sm"
                  variant="outline"
                  onClick={() => {
                    setCopilotOpen((open) => !open);
                    // The inspector occupies the same corner. Two stacked panels on
                    // top of each other is worse than either alone.
                    setSelectedNodeId(null);
                  }}
                  disabled={!graph}
                  aria-pressed={copilotOpen}
                  data-testid="workflow-copilot-toggle"
                  title="Ask about this workflow — what it does, or why a run failed."
                >
                  <Bot className="mr-1.5 size-4" />
                  Copilot
                </Button>
                {historySupported && (
                  <Button
                    size="sm"
                    variant="outline"
                    onClick={() => setHistoryOpen((open) => !open)}
                    aria-pressed={historyOpen}
                    data-testid="workflow-history-toggle"
                  >
                    <History className="mr-1.5 size-4" />
                    History
                    {historyRows.length > 0 && (
                      <Badge variant="secondary" className="ml-1.5 h-4 px-1.5 text-3xs font-normal">
                        {historyRows.length}
                      </Badge>
                    )}
                  </Button>
                )}
                {/* Issue #276. Pause stops the SCHEDULE, not the workflow — the title
                    says so, because "pause" on its own reads like "I can't run this",
                    and an operator debugging a workflow needs the opposite. Shown only
                    for a scheduled workflow: see `isScheduled`. */}
                {isScheduled && (
                  <Button
                    size="sm"
                    variant="outline"
                    onClick={() => void toggleEnabled()}
                    disabled={!canToggle}
                    aria-pressed={paused}
                    aria-label={paused ? "Resume schedule" : "Pause schedule"}
                    data-testid="workflow-toggle-enabled"
                    title={
                      paused
                        ? "Start running this on its schedule again. It keeps its graph either way."
                        : "Stop this running on its schedule. It keeps its graph and you can still run it by hand."
                    }
                  >
                    {toggling ? (
                      <Loader2 className="mr-1.5 size-4 animate-spin" />
                    ) : paused ? (
                      <Power className="mr-1.5 size-4" />
                    ) : (
                      <Pause className="mr-1.5 size-4" />
                    )}
                    {paused ? "Resume" : "Pause"}
                  </Button>
                )}
              </div>

              {/* utility · destructive — the two that change the workflow
                  rather than run it, Delete last and in the destructive tone.
                  Issue #1135's fourth complaint was that it sat flush against
                  Edit with nothing separating it: an outline button one gap
                  away from another outline button is a misclick away from a
                  deletion. */}
              <div className="flex items-center gap-2">
                <ToolbarDivider />
                {/* Issue #259. The wrapping span carries the explanation: a disabled
                    button swallows pointer events in most browsers, so a `title` on
                    the button itself would never show. */}
                <span title={notEditable ? notEditableReason : undefined}>
                  <Button
                    size="sm"
                    variant="outline"
                    onClick={() => setEditOpen(true)}
                    disabled={!canEdit}
                    aria-label="Edit workflow"
                    data-testid="workflow-edit"
                  >
                    <Pencil className="mr-1.5 size-4" />
                    Edit
                  </Button>
                </span>
                <span title={notEditable ? notEditableReason : undefined}>
                  <AlertDialog open={confirmOpen} onOpenChange={setConfirmOpen}>
                    <AlertDialogTrigger
                      render={
                        <Button
                          size="sm"
                          variant="destructive"
                          disabled={!canDelete}
                          aria-label="Delete workflow"
                          data-testid="workflow-delete"
                        >
                          {deleting ? (
                            <Loader2 className="mr-1.5 size-4 animate-spin" />
                          ) : (
                            <Trash2 className="mr-1.5 size-4" />
                          )}
                          Delete
                        </Button>
                      }
                    />
                    <AlertDialogContent>
                      <AlertDialogHeader>
                        <AlertDialogTitle>Delete “{graph?.name ?? selectedId}”?</AlertDialogTitle>
                        {/* Say exactly what goes and what stays. "Stops its schedule"
                            is the consequence an operator most needs spelled out, and
                            "past runs stay" stops them hesitating over losing history. */}
                        <AlertDialogDescription>
                          This removes the workflow and stops it running on its schedule. Past runs
                          stay in the run history. This can&apos;t be undone.
                        </AlertDialogDescription>
                      </AlertDialogHeader>
                      <AlertDialogFooter>
                        <AlertDialogCancel>Keep it</AlertDialogCancel>
                        <AlertDialogAction
                          onClick={() => {
                            // Close FIRST: `remove()` clears the selection, so a
                            // dialog left mounted would re-render its title as
                            // `Delete “”?` over a backdrop nothing can click past.
                            setConfirmOpen(false);
                            void remove();
                          }}
                          className="bg-destructive text-white hover:bg-destructive/90"
                          data-testid="workflow-delete-confirm"
                        >
                          Delete workflow
                        </AlertDialogAction>
                      </AlertDialogFooter>
                    </AlertDialogContent>
                  </AlertDialog>
                </span>
              </div>
            </div>
          </div>
        </div>
      ) : (
        /* The index's one row: the tab's own heading, and the controls that
           act on the list rather than on any workflow in it.

           Issue #1763: this row is the console's page header now. It is the
           shape the operator named as the reference, so `PageHeader` was
           derived from it and this page reads as it did — bar, hairline,
           inline count, actions right-aligned — with the title on the shared
           scale rather than on the `text-sm` that only this one row used. */
        <PageHeader
          title={indexTab === "runs" ? "Runs" : "Workflows"}
          count={indexTab === "runs" ? indexRuns.length : workflows.length}
          data-testid="workflow-index-header"
          actions={
            <>
              {/* Issue #1697: the graphs, or their runs — the index's other
                  axis, alongside Cards/List. Segmented for the same reason
                  that toggle is: one question, two answers. */}
              <div className="flex items-center gap-1 rounded-lg border p-0.5">
                {(
                  [
                    { value: "workflows", label: "Workflows", Icon: WorkflowIcon },
                    { value: "runs", label: "Runs", Icon: History },
                  ] as const
                ).map(({ value, label, Icon }) => (
                  <Button
                    key={value}
                    size="sm"
                    variant={indexTab === value ? "secondary" : "ghost"}
                    className="h-7 px-2"
                    onClick={() => {
                      setIndexTab(value);
                      writeIndexTab(value);
                    }}
                    aria-pressed={indexTab === value}
                    data-testid={`workflow-index-tab-${value}`}
                  >
                    <Icon className="mr-1.5 size-3.5" />
                    {label}
                  </Button>
                ))}
              </div>
              {/* Issue #1110: the index's Cards/List toggle, in the tab's one
                  toolbar. It used to sit in a header the index drew for itself,
                  which was fine while the index was a panel over the canvas and
                  wrong the moment it became the page — "Workflows 7" and "All
                  workflows 7" one above the other, with the toggle stranded under
                  the duplicate.

                  Segmented rather than two loose buttons, because the pair is one
                  question with two answers and reads as a switch.

                  Issue #1697: only meaningful for the Workflows tab — the Runs
                  tab is always a table, so this toggle would offer a choice it
                  does not act on. */}
              {indexTab === "workflows" && workflows.length > 0 && (
                <div className="flex items-center gap-1 rounded-lg border p-0.5">
                  {(
                    [
                      { value: "cards", label: "Cards", Icon: LayoutGrid },
                      { value: "list", label: "List", Icon: ListIcon },
                    ] as const
                  ).map(({ value, label, Icon }) => (
                    <Button
                      key={value}
                      size="sm"
                      variant={indexMode === value ? "secondary" : "ghost"}
                      className="h-7 px-2"
                      onClick={() => {
                        setIndexMode(value);
                        writeIndexMode(value);
                      }}
                      aria-pressed={indexMode === value}
                      data-testid={`workflow-index-${value}`}
                    >
                      <Icon className="mr-1.5 size-3.5" />
                      {label}
                    </Button>
                  ))}
                </div>
              )}
              {/* Issue #341: THE control named "New workflow" — the one an
                  operator, a screen reader or a spec should find under that
                  name. The empty-state call to action below is named
                  differently on purpose; two buttons answering to one name is
                  an ambiguity nothing can resolve.

                  Issue #1135 put it here and only here. It is an index-level
                  action, and it used to render in both states — stranded on a
                  third toolbar row inside a workflow, where "new" is the one
                  thing the screen is not about. Filled, because on the index it
                  is the primary action; the detail screen's primary is Run. */}
              <Button size="sm" onClick={() => setCreateOpen(true)} data-testid="workflow-create">
                <Plus className="mr-1.5 size-4" />
                New workflow
              </Button>
            </>
          }
        />
      )}

      {/* Issue #259: a write refused because the graph moved under us. Distinct
          from `error` on purpose — this one is recoverable, and the recovery is
          right here, so it must not be mistaken for a generic load failure. */}
      {detailOpen && conflict && (
        <div className="px-4 pt-3">
          <Alert variant="destructive" data-testid="workflow-conflict">
            <AlertDescription className="flex flex-wrap items-center justify-between gap-2">
              <span>{conflict}</span>
              <Button
                size="sm"
                variant="outline"
                onClick={() => setGraphTick((n) => n + 1)}
                data-testid="workflow-conflict-reload"
              >
                <RotateCw className="mr-1.5 size-4" />
                Reload
              </Button>
            </AlertDescription>
          </Alert>
        </div>
      )}

      {/* Issue #528 / #514: the host refused the run for a reason the operator
          can clear from Settings. Persistent (not a toast) and mirroring the
          conflict banner's layout, with the fix one click away. Keyed on the
          structured `code`, never the prose. */}
      {detailOpen && runRefusal && (
        <div className="px-4 pt-3">
          <Alert variant="destructive" data-testid="workflow-run-inference-alert">
            <AlertDescription className="flex flex-wrap items-center justify-between gap-2">
              <span>
                {runRefusal.code === "inference_required"
                  ? "This company has no inference provider configured, so workflows can't run. Set a provider under Settings → Inference, then run again."
                  : runRefusal.message}
              </span>
              {runRefusal.code === "inference_required" && (
                <a
                  href="#/settings/oauth"
                  className={cn(buttonVariants({ variant: "outline", size: "sm" }))}
                  data-testid="workflow-run-inference-cta"
                >
                  <Plug className="mr-1.5 size-4" />
                  Set up inference
                </a>
              )}
            </AlertDescription>
          </Alert>
        </div>
      )}

      {/* Issue #1704 (review): the company-wide list failure, on the index and
          on a detail view alike — a workflow open on screen does not make the
          list behind it any less stale. First, because it is the wider claim. */}
      {listError && (
        <div className="px-4 pt-3">
          <Alert variant="destructive" data-testid="workflow-list-error">
            <AlertDescription>{listError}</AlertDescription>
          </Alert>
        </div>
      )}

      {/* And the one about the workflow on screen. The selection-change effect
          clears it, so it cannot outlive the workflow it names. */}
      {graphError && (
        <div className="px-4 pt-3">
          <Alert variant="destructive" data-testid="workflow-graph-error">
            <AlertDescription>{graphError}</AlertDescription>
          </Alert>
        </div>
      )}

      {/* Issue #1110: a link named a workflow this company does not have.
          Rendered on the index, over the list of what it does have, because
          that list is the answer to the question a dead link raises. Not a
          detail shell addressed to nothing, and not only a toast — the operator
          arrived here from somebody else's link and may take a while to work
          out which workflow replaced it.

          Dismissible rather than timed: it is a statement about how they got
          here, and it stops being true the moment they open something. */}
      {missingWorkflowId && !detailOpen && (
        <div className="px-4 pt-3">
          <Alert data-testid="workflow-missing-link">
            <AlertDescription className="flex flex-wrap items-center justify-between gap-2">
              <span className="text-xs">
                This company has no workflow “{missingWorkflowId}”. It may have been renamed
                or deleted since that link was made. Everything it does have is below.
              </span>
              <Button
                size="sm"
                variant="outline"
                onClick={() => setMissingWorkflowId(null)}
                data-testid="workflow-missing-link-dismiss"
              >
                Dismiss
              </Button>
            </AlertDescription>
          </Alert>
        </div>
      )}

      {/* Issue #1845: the week-1 "save your first workflow" nudge. Index only
          (not the canvas detail) — it points at the same CTA the empty state
          offers, which only exists there, and a nudge to create a workflow
          while one is already open on screen would be an odd thing to say. */}
      {nudge && !detailOpen && (
        <div className="px-4 pt-3">
          <Week1NudgeBanner onCreate={() => setCreateOpen(true)} onDismiss={clearNudge} />
        </div>
      )}

      {/* Issue #371: the canvas is showing a PAST run, not the live one. Said
          plainly, with the way out attached — an unexplained ring on a node
          would otherwise read as the current state of the workflow. */}
      {detailOpen && overlayRun && (
        <div className="px-4 pt-3">
          <Alert data-testid="workflow-overlay-banner">
            <AlertDescription className="flex flex-wrap items-center justify-between gap-2">
              <span className="text-xs">
                Showing the {overlayRun.scheduled ? "scheduled" : "manual"} run from{" "}
                {new Date(overlayRun.atMillis).toLocaleString()}
                {overlayRun.error
                  ? ` — ${failureLocation(overlayRun, graph)}`
                  : overlayRun.cancelled
                    ? " — an operator stopped it."
                    : "."}{" "}
                Unmarked nodes were never reached.
              </span>
              <Button size="sm" variant="outline" onClick={() => setOverlayRun(null)}>
                Clear
              </Button>
            </AlertDescription>
          </Alert>
        </div>
      )}

      {/* Issue #1110: ONE branch, on the one piece of state that says where
          the operator is. No workflow open ⇒ the index fills the body; a
          workflow open ⇒ its canvas does.

          The index REPLACES the canvas rather than squeezing in above it
          (issue #303's reasoning, unchanged): a card grid needs the width,
          and a canvas is meaningless while the operator is still deciding
          which workflow they want.

          Issue #1107: the same branch also chooses the LAYOUT. The index is
          one full-width column and stays that way; the detail view is a
          `CanvasShell`, which is what adds the left rail slot. Run history is
          per-workflow chrome, so it can only exist on the side of this branch
          that has a workflow — a list of workflows has no single run to show
          history for (#1110). Gating the rail here rather than gating the
          panel makes that structural: the index cannot grow run chrome by
          accident, because the slot it would mount in does not exist there. */}
      {!detailOpen ? (
        <div className="relative flex-1 min-h-0">
          {indexTab === "runs" ? (
            // Issue #1697: the company-wide run page, unscoped by workflow —
            // the same request `runsByWorkflow` folds for the card health
            // strips, read here as its own table instead. `loading` follows
            // `indexRunsLoaded` rather than `loadingList`: the workflow list
            // and the run page are two different requests, and a company
            // with its workflows already loaded can still be waiting on runs.
            <RunTracesList
              runs={indexRuns}
              workflows={workflows}
              company={company}
              loading={!indexRunsLoaded}
              onSelectRun={setTraceRun}
            />
          ) : !loadingList && workflows.length === 0 ? (
            // Issue #813's on-ramp, which used to live behind the canvas's
            // empty selection. An empty company now lands here instead, so this
            // is where it has to be — and it is shown INSTEAD of the index
            // rather than inside it, because a Cards/List toggle over nothing
            // is chrome for a decision there is nothing to make.
            <div className="flex h-full flex-col items-center justify-center gap-3 px-4 text-center text-sm text-muted-foreground">
              <p>This company has no saved workflows yet.</p>
              {/* Issue #813: a first-time author has no on-ramp otherwise. One
                  compact prose block — what a workflow is, a worked example
                  (mirroring the copilot placeholder), and the create-time
                  copilot as the easiest path. Deliberately no template
                  gallery. */}
              <div className="max-w-md space-y-2 text-2xs leading-relaxed">
                <p>
                  A workflow runs a sequence of steps on a schedule or on demand: a{" "}
                  <span className="font-medium text-foreground">trigger</span> starts it,{" "}
                  <span className="font-medium text-foreground">teammates</span> and{" "}
                  <span className="font-medium text-foreground">tools</span> do the work,
                  and an <span className="font-medium text-foreground">output</span> step
                  reports the result somewhere.
                </p>
                <p className="italic">
                  e.g. “Every Monday morning, have the writer draft the weekly digest
                  and email it to the team.”
                </p>
                <p>
                  Describe it in plain words when you create one — the copilot drafts
                  the graph for you to review and edit.
                </p>
              </div>
              {/* Issue #341: opens the same dialog as the toolbar button, and
                  therefore must NOT carry the same name. "Create a workflow"
                  rather than "Create the first workflow" because this state is
                  also where deleting the last workflow lands, and by then there
                  is nothing first about it. */}
              <Button
                size="sm"
                variant="outline"
                onClick={() => setCreateOpen(true)}
                data-testid="workflow-create-empty"
              >
                <Plus className="mr-1.5 size-4" />
                Create a workflow
              </Button>
            </div>
          ) : (
            <WorkflowIndex
              workflows={workflows}
              runsByWorkflow={runsByWorkflow}
              onSelect={(id) => {
                setSelectedId(id);
                setHistoryOpen(true);
                setCopilotOpen(true);
              }}
              mode={indexMode}
              loading={loadingList}
              runsLoaded={indexRunsLoaded}
            />
          )}
        </div>
      ) : (
        /* Issue #1107 (extended by #1205): canvas with a rail on each side.
           `CanvasShell` owns the detail view's column layout and documents
           which slot a new panel belongs in — left rail, right rail, or right
           overlay. */
        <CanvasShell
          leftRail={
            historyOpen && historySupported ? (
              <RunHistoryPanel
                client={client}
                company={company}
                runs={historyRows}
                graph={graph}
                workflowName={selected?.name ?? selectedId ?? ""}
                onClose={() => setHistoryOpen(false)}
                selectedRunSeq={overlayRun?.seq ?? null}
                onSelectRun={(picked) =>
                  // Clicking the row already shown clears it, so the control is
                  // a toggle rather than a one-way trip into overlay mode.
                  setOverlayRun((prev) => (prev?.seq === picked.seq ? null : picked))
                }
                onFixWithCopilot={handleFixWithCopilot}
                fixingRunSeq={fixingRunSeq}
                fixReason={fixReason}
                hasMore={runsHasMore}
                onLoadOlder={loadOlderRuns}
                loadingOlder={loadingOlderRuns}
              />
            ) : null
          }
          // Issue #1205: `result` and `runFailure` are mutually exclusive by
          // construction — `run()` clears both on dispatch, and only one arm
          // of its try/catch sets one — so this is a ternary, not two
          // independent siblings. Both are per-workflow chrome, so this is
          // still gated by being inside the `detailOpen` branch, same as the
          // left rail above.
          rightRail={
            result ? (
              <RunResultPanel
                result={result}
                graph={graph}
                request={ranWith}
                onClose={() => setResult(null)}
                // Issue #1002: the whole queue, narrowed by the panel to this
                // run. Handed in unfiltered on purpose — the Approvals page
                // reads the same array and must keep showing every row.
                approvals={approvals}
                now={approvalsNow}
                askerNames={askerNames}
                deciding={decidingApprovals}
                decided={decidedApprovals}
                failed={failedApprovals}
                onDecide={onDecideApproval}
              />
            ) : runFailure ? (
              <RunFailurePanel
                failure={runFailure}
                onClose={() => setRunFailure(null)}
                onOpenHistory={
                  historySupported
                    ? () => {
                        setHistoryOpen(true);
                        if (failureRun) setOverlayRun(failureRun);
                      }
                    : undefined
                }
                onFixWithCopilot={
                  failureRun ? () => void handleFixWithCopilot(failureRun) : undefined
                }
                fixing={failureRun ? fixingRunSeq === failureRun.seq : false}
                failedStepName={failureNode ? nodeName(graph, failureNode) : null}
                onShowFailedStep={
                  failureRun && failureNode
                    ? () => {
                        setHistoryOpen(true);
                        setOverlayRun(failureRun);
                        setSelectedNodeId(failureNode);
                      }
                    : undefined
                }
              />
            ) : null
          }
        >
          {loadingList || loadingGraph ? (
            <div className="absolute inset-0 p-4">
              <Skeleton className="h-full w-full rounded-xl" />
            </div>
          ) : (
            <>
              <ReactFlow
                nodes={nodes}
                edges={edges}
                nodeTypes={NODE_TYPES}
                colorMode={resolvedTheme === "dark" ? "dark" : "light"}
                fitView
                // Issue #1361: `minZoom` here is the floor on the FIT, not on
                // zooming — `FitGraphToPane` below explains the difference and
                // `LEGIBLE_FIT_ZOOM` explains the number. Set on the options as
                // well as corrected by that component so that a fit which runs
                // before the pane is measured still cannot open the canvas
                // below the point where a node title is words.
                fitViewOptions={{ padding: 0.2, minZoom: LEGIBLE_FIT_ZOOM }}
                // Issue #1261: the library default (0.5) is above the scale
                // most shipped templates need to fit — an 8-10 node single-row
                // pipeline needs roughly 0.3. Left at the default, `fitView`
                // clamps to 0.5 and permanently crops the first/last node,
                // and the canvas's own Zoom Out control is disabled from
                // load because the viewport is already pinned at the floor.
                //
                // Unchanged by #1361, which floors the initial fit and nothing
                // else: an operator who reaches for Zoom Out to see a whole
                // pipeline's shape still gets all the way down to 0.1.
                minZoom={0.1}
                nodesDraggable={false}
                nodesConnectable={false}
                elementsSelectable
                onNodeClick={onNodeClick}
                onMove={onMove}
                onPaneClick={() => setSelectedNodeId(null)}
                proOptions={{ hideAttribution: true }}
              >
                <Background variant={BackgroundVariant.Dots} gap={20} size={1} />
                <Controls showInteractive={false}>
                  <WorkflowZoomReadout />
                </Controls>
                {/* Issue #1259: a custom minimap, not React Flow's built-in
                    `<MiniMap>` — see WorkflowMiniMap.tsx for why. */}
                <WorkflowMiniMap nodes={nodes} className="!hidden sm:!block" />
                {/* Issue #1231: renders nothing — it pans the selected node out
                    from under the inspector overlay, and pans back on close.
                    A child of `<ReactFlow>` because that is the only provider
                    `useReactFlow` can find in this view. It is handed the
                    INSPECTOR's node, not `selectedNodeId`: the copilot shares
                    the slot and wins while open (#303), and it has no one node
                    it must not hide. */}
                <RevealSelectedNode
                  handleRef={revealRef}
                  nodeId={!copilotOpen && selectedNode ? selectedNode.id : null}
                />
                {/* Issue #1361: renders nothing — it anchors the opening
                    viewport at the graph's start when the fit was clamped at
                    `LEGIBLE_FIT_ZOOM`, so a long pipeline opens readable and
                    at its trigger instead of unreadable and in its middle. A
                    child of `<ReactFlow>` for the same reason its two siblings
                    above are. */}
                <FitGraphToPane nodes={nodes} graphId={graph?.id ?? null} />
              </ReactFlow>
              {/* Issue #303: the copilot and the node inspector share the canvas's
                  right edge, and the copilot wins while it is open — it was
                  opened deliberately, whereas a node click is incidental and is
                  already cleared when the copilot opens. */}
              {copilotOpen && graph ? (
                <CopilotPanel
                  // Remount per workflow. The panel replays that workflow's own
                  // transcript on mount, and keying it means a workflow switch
                  // can never leave the previous conversation on screen — the
                  // "no cross-workflow leakage" criterion, on the client side.
                  key={graph.id}
                  client={client}
                  company={company}
                  graph={graph}
                  runs={runs}
                  runsKnown={historySupported}
                  // The graph on screen and the history in `runs` must be the
                  // same workflow's before anything is grounded on the pair.
                  runsReady={runsFor === graph.id}
                  // Issue #415: an applied proposal lands through the SAME
                  // handler the edit dialog's save does. One place decides what
                  // "the graph is now this" means, so a copilot edit and a canvas
                  // edit cannot leave the view in two different states.
                  onApplied={handleSaved}
                  onConflict={setConflict}
                  onClose={() => setCopilotOpen(false)}
                />
              ) : (
                selectedNode && (
                  <NodeDetailPanel
                    node={selectedNode}
                    roster={nodeRoster}
                    output={selectedNodeOutput}
                    onClose={() => setSelectedNodeId(null)}
                  />
                )
              )}
            </>
          )}
        </CanvasShell>
      )}

      {/* Issue #1110's reasoning, extended by #1205: `result` and `runFailure`
          are per-workflow chrome that must not outlive leaving the workflow.
          Both used to render as full-width strips here, below `CanvasShell`.
          They are now `CanvasShell`'s `rightRail` (see the call above), gated
          by the very same `detailOpen` branch structurally — a list of
          workflows has no single run's outcome to show, so there is nothing
          for a rail to be beside, the same reason run history (#1107) lives
          inside `CanvasShell` rather than out here. */}

      {/* Issue #1204: where the toolbar's run-input box went.

          Both dispatches live here, not just Run. The box fed the toolbar's Run
          AND its Test run, so a dialog offering only Run would have quietly
          taken "prove this graph against a real input, without sending
          anything" away — the rehearsal that matters most for a workflow whose
          first step reads `=items`. */}
      <Dialog open={runInputOpen} onOpenChange={setRunInputOpen}>
        <DialogContent className="sm:max-w-lg" data-testid="workflow-run-input-dialog">
          <DialogHeader>
            <DialogTitle>Run with input</DialogTitle>
            {/* Says what the payload DOES, which the bare placeholder never
                did. An operator who has to guess whether their graph reads this
                will guess wrong in both directions. */}
            <DialogDescription>
              What this run should work on. It is handed to the workflow&rsquo;s first
              step. Leave it empty to run the workflow as its schedule does.
            </DialogDescription>
          </DialogHeader>
          <Input
            autoFocus
            value={request}
            onChange={(e) => setRequest(e.target.value)}
            onKeyDown={(e) => {
              // Enter runs, exactly as it did from the toolbar box (#154).
              if (e.key === "Enter") runWithInput(false);
            }}
            placeholder="What should this run work on?"
            aria-label="Request for this run"
          />
          <DialogFooter>
            <Button
              variant="ghost"
              onClick={() => setRunInputOpen(false)}
              data-testid="workflow-run-input-cancel"
            >
              Cancel
            </Button>
            {/* Issue #542&apos;s test run, carrying the same payload. Outline
                beside the filled Run for the same reason it is outline on the
                toolbar: it is the rehearsal, not the act. */}
            <Button
              variant="outline"
              onClick={() => runWithInput(true)}
              disabled={!selectedId || running || loadingGraph}
              data-testid="workflow-run-input-test-run"
            >
              <FlaskConical className="mr-1.5 size-4" />
              Test run
            </Button>
            <Button
              onClick={() => runWithInput(false)}
              disabled={!selectedId || running || loadingGraph}
              data-testid="workflow-run-input-submit"
            >
              <Play className="mr-1.5 size-4" />
              Run
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <WorkflowCreateDialog
        client={client}
        company={company}
        open={createOpen}
        onOpenChange={setCreateOpen}
        onCreated={handleCreated}
      />

      {/* Issue #259: the same dialog in edit mode. The graph carries the version
          token the save is conditional on, so it is passed as loaded rather
          than copied — and a 409 lands in the banner above, which outlives the
          dialog and holds the way out.

          It is `editGraph`, the graph pinned when the dialog opened, not the
          live `graph` (issue #1006): the selection can move out from under an
          open dialog, and neither a different workflow's graph nor a failed
          read may reach an edit in progress. `open` is gated on it for the
          original reason too — a null `workflow` IS create mode, so the
          operator would be looking at a blank New workflow form wearing the
          Edit title. */}
      <WorkflowCreateDialog
        client={client}
        company={company}
        open={editOpen && editGraph !== null}
        onOpenChange={(o) => {
          setEditOpen(o);
          // Issue #840 (PR-3): a copilot correction is single-use — dropping it on
          // close means the next plain Edit hydrates from the saved graph, not a
          // stale correction.
          if (!o) setPrefilledDraft(null);
        }}
        workflow={editGraph}
        onSaved={handleSaved}
        onConflict={setConflict}
        prefilledDraft={prefilledDraft}
      />

      {/* Issue #1697: the traces list's transcript sheet. Top-level rather
          than nested inside the `!detailOpen` branch — it is opened only from
          the Runs tab, but keeping it mounted regardless of which index tab
          is showing means switching tabs while it's open doesn't unmount it
          out from under the operator. */}
      <RunTraceSheet
        client={client}
        company={company}
        run={liveTraceRun}
        workflowName={
          (liveTraceRun && workflows.find((w) => w.id === liveTraceRun.workflowId)?.name) ??
          liveTraceRun?.workflowId ??
          ""
        }
        onClose={() => setTraceRun(null)}
      />
    </div>
  );
}

/** Where the index's cards-or-list preference is remembered. */
const INDEX_MODE_KEY = "oc.workflows.indexMode";

/** The remembered index rendering, defaulting to cards.
 *
 * Every access is guarded: `localStorage` throws outright in a browser with
 * site data blocked, and a preference is never worth failing a render over.
 */
function readIndexMode(): IndexMode {
  try {
    return window.localStorage.getItem(INDEX_MODE_KEY) === "list" ? "list" : "cards";
  } catch {
    return "cards";
  }
}

/** Remembers the index rendering. Best-effort, for the same reason. */
function writeIndexMode(mode: IndexMode): void {
  try {
    window.localStorage.setItem(INDEX_MODE_KEY, mode);
  } catch {
    // A preference that cannot be saved is not an error worth surfacing.
  }
}

/** Which index the Workflows tab shows: the graphs, or their runs (issue
 * #1697). Local state rather than a hash segment, same as {@link IndexMode} —
 * `readWorkflowHash`'s two-segment-plus-query contract is deliberately narrow
 * (see its own comment), and a preference this small does not need a
 * shareable URL to earn a spot in it. */
type IndexTab = "workflows" | "runs";

/** Where the index's Workflows-or-Runs preference is remembered. */
const INDEX_TAB_KEY = "oc.workflows.indexTab";

/** The remembered tab, defaulting to the graphs — same best-effort guard as
 * {@link readIndexMode}. */
function readIndexTab(): IndexTab {
  try {
    return window.localStorage.getItem(INDEX_TAB_KEY) === "runs" ? "runs" : "workflows";
  } catch {
    return "workflows";
  }
}

/** Remembers the index tab. Best-effort, for the same reason. */
function writeIndexTab(tab: IndexTab): void {
  try {
    window.localStorage.setItem(INDEX_TAB_KEY, tab);
  } catch {
    // A preference that cannot be saved is not an error worth surfacing.
  }
}
