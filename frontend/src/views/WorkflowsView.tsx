import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  Background,
  BackgroundVariant,
  Controls,
  MiniMap,
  type Node,
  ReactFlow,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import { useTheme } from "next-themes";
import {
  Bot,
  FlaskConical,
  History,
  LayoutGrid,
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
import type { OpenCompanyClient } from "@/api/client";
import { ApiError } from "@/api/types";
import type { ApprovalSummary, GrantScope, Verdict } from "@/api/types";
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
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button, buttonVariants } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
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
// Issue #303: the canvas arithmetic, the run-state folds and the three drawers
// moved out when this file passed 1800 lines and was about to grow an index and
// a copilot. See `workflows/graph.ts` for why the fold is pure.
import {
  elapsedFromRun,
  failureLocation,
  foldLiveRun,
  initialRunState,
  layout,
  statesFromRun,
  windowHasRunStart,
} from "@/views/workflows/graph";
import { LastRunChip, RunHistoryPanel } from "@/views/workflows/RunHistoryPanel";
import { WorkflowIndex, type IndexMode } from "@/views/workflows/WorkflowIndex";
import { CopilotPanel } from "@/views/workflows/CopilotPanel";
import { classifyRunError } from "@/views/workflows/run-error";
import { runFailureFrom, type RunFailure } from "@/views/workflows/run-failure";
import { RunFailurePanel } from "@/views/workflows/RunFailurePanel";
import { RunResultPanel } from "@/views/workflows/RunResultPanel";
import { approvalsForRun } from "@/views/workflows/run-approvals";
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
  const [request, setRequest] = useState("");
  const [ranWith, setRanWith] = useState("");
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
  const [error, setError] = useState<string | null>(null);
  const [createOpen, setCreateOpen] = useState(false);
  // Issue #259: the same dialog, hydrated from the selected graph. Separate
  // state from `createOpen` rather than a mode flag, so the create path keeps
  // working exactly as it did and neither can be half-open.
  const [editOpen, setEditOpen] = useState(false);
  // Issue #840 (PR-3): a copilot-corrected graph to open the edit dialog on. When
  // set, the edit dialog hydrates from this correction (keeping `graph`'s version
  // token) instead of from the saved graph, so Save writes a new version.
  const [prefilledDraft, setPrefilledDraft] = useState<PrefilledDraft | null>(null);
  // The failed run whose copilot fix is in flight, so its history row spins.
  const [fixingRunSeq, setFixingRunSeq] = useState<number | null>(null);
  // A run the copilot judged un-fixable, shown inline under that run's row.
  const [fixReason, setFixReason] = useState<{ seq: number; reason: string } | null>(null);
  const [selectedNodeId, setSelectedNodeId] = useState<string | null>(null);
  // Issue #228: what past runs did, read back from the host's journal. This is
  // the half that survives a reload — before it, a manual run's delivery rows
  // vanished when the drawer was dismissed and a scheduled run's never reached
  // the operator at all.
  const [runs, setRuns] = useState<WorkflowRunOutcome[]>([]);
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
  // Issue #303: the browse index (cards or list) is up instead of the canvas.
  //
  // Closed by default, and that is deliberate rather than timid: the toolbar
  // picker, the Run button and the Edit/Delete affordances are what this tab
  // opens onto today, and moving the canvas behind a landing screen would
  // change the first thing every operator sees for the sake of a browse
  // surface most of them reach for occasionally.
  const [indexOpen, setIndexOpen] = useState(false);
  // Which rendering the index uses, remembered across sessions — an operator
  // who prefers one has no reason to re-pick it every visit.
  const [indexMode, setIndexMode] = useState<IndexMode>(readIndexMode);
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
  const reconciledSelectionRef = useRef<string | null>(null);

  // Load the workflow list, and auto-select the first entry.
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
          // Nothing valid to keep: fall to the first remaining entry, exactly
          // as the local Delete button does — so a workflow deleted from
          // another session and one deleted from this button leave the view in
          // the same place. A company whose last workflow just went away
          // selects nothing, and the canvas empties.
          //
          // Marked as a reconciliation so the hash mirror replaces rather than
          // pushes: nobody navigated here, the workflow they were on stopped
          // existing. Writing the same id twice (React re-invokes an updater in
          // StrictMode) says the same thing twice.
          const fallback = rows[0]?.id ?? null;
          reconciledSelectionRef.current = fallback;
          return fallback;
        });
        setError(null);
      } catch (e) {
        if (!live) return;
        setError(e instanceof Error ? e.message : "could not load workflows");
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
        // this company no longer has. Say so, once.
        toast.error(`This company has no workflow “${target}”.`, {
          description:
            "It may have been renamed or deleted since the link was made. Showing the current selection instead.",
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
  // belongs to the graph being left behind.
  //
  // Replace vs push is decided by whether the hash already names a workflow,
  // and by whether the operator moved the selection at all.
  //
  // Filling in a bare `#/workflows` is this view resolving its own default, not
  // a place the operator has been — pushing it would put an entry in the
  // history stack that looks identical to the one before it, so their first
  // Back press out of the tab would appear to do nothing. Moving from one named
  // workflow to another IS a navigation they took, and Back should undo it.
  //
  // A reconciliation is neither: the workflow they were on stopped existing and
  // the list effect moved them off it (issue #384). Pushing that would offer
  // Back as a route to a workflow that is gone — see `reconciledSelectionRef`
  // for why the view does not even correct itself once it is there.
  useEffect(() => {
    // Consumed on every run, whichever branch is taken below: a marker left
    // over from a reconciliation that did not end up writing the URL must not
    // decide a later, genuine navigation.
    const reconciled = reconciledSelectionRef.current === selectedId;
    reconciledSelectionRef.current = null;
    if (!selectedId) return;
    const { onWorkflows, workflowId } = readWorkflowHash();
    // Another view owns the hash (a company switch mid-navigation, a stale
    // effect): rewriting it would drag the operator back here.
    if (!onWorkflows) return;
    if (workflowId === selectedId) return;
    const next = `#/workflows/${encodeURIComponent(selectedId)}`;
    if (workflowId === null || reconciled) window.history.replaceState(null, "", next);
    else window.location.hash = next.slice(1);
  }, [selectedId]);

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
        setError(null);
        // A successful re-read is exactly what clears a stale-graph warning:
        // whatever `version` we now hold is current.
        setConflict(null);
      } catch (e) {
        if (!live) return;
        setGraph(null);
        setError(e instanceof Error ? e.message : "could not load the workflow graph");
      } finally {
        if (live) setLoadingGraph(false);
      }
    })();
    return () => {
      live = false;
    };
  }, [client, company, selectedId, graphTick]);

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
    if (!selectedId) {
      setRuns([]);
      setRunsFor(null);
      return;
    }
    let live = true;
    (async () => {
      try {
        const rows = await listWorkflowRuns(client, company, {
          workflow: selectedId,
          limit: 50,
        });
        if (!live) return;
        setRuns(rows);
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

  // Issue #303: the run page the index's health readings are folded from.
  //
  // Fetched only while the index is open — every card reads from one request,
  // and a company that never opens the browse panel should not pay for it. It
  // refreshes on `runEventTick` so a run finishing with the index up updates
  // the card that owns it, and on `runsTick` so a run started from here shows
  // as running.
  //
  // UNSCOPED, unlike the selected workflow's history above: `?workflow=` covers
  // exactly one graph, and the index needs every graph. The cost of that is a
  // page cut by `limit` across all workflows, which is precisely why the cards
  // are worded "No recent runs" — see `WorkflowIndex`'s `HealthLine`.
  useEffect(() => {
    if (!indexOpen) return;
    let live = true;
    (async () => {
      try {
        const rows = await listWorkflowRuns(client, company, { limit: 200 });
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
  }, [client, company, indexOpen, runsTick, runEventTick]);

  // A company switch invalidates the whole page — another company's runs must
  // never be folded onto this one's cards, and `indexRunsLoaded` has to go back
  // to false so the cards say "Loading runs…" rather than "No recent runs"
  // about a company we have not asked about yet.
  useEffect(() => {
    setIndexRuns([]);
    setIndexRunsLoaded(false);
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

  const run = useCallback(async (dryRun = false) => {
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
    const asked = request.trim();
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
              "Your test run executed real effects (agent turns, tools, and any report delivery). Update the host to get true no-effect test runs.",
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
  }, [client, company, selectedId, request, graph]);

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
      setSelectedId(remaining[0]?.id ?? null);
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

  // Issue #259: the edit dialog saved. The host answers with the stored graph
  // AND a fresh version token, so holding onto it is what lets the operator
  // save again without a re-read — dropping it and re-fetching would be a round
  // trip that can only return the same thing.
  const handleSaved = useCallback((saved: WorkflowGraph) => {
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
    toast.success("Workflow saved.");
  }, []);

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
    setSelectedId(created.id);
    toast.success("Workflow created.");
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
      setFixingRunSeq(run.seq);
      setFixReason(null);
      try {
        const res = await fixWorkflowFromRun(client, company, run.workflowId, {
          runId: run.runId,
          errorHint: run.error,
        });
        if (res.automatable && res.workflow) {
          // The edit dialog binds to the SELECTED workflow's `graph` for its
          // version token; if the operator changed selection while the fix was in
          // flight, opening it now would write the correction of `run.workflowId`
          // over a different workflow. Abandon rather than save the wrong one.
          if (selectedIdRef.current !== run.workflowId) {
            toast.message(
              "Selection changed while the copilot was working — reopen Fix on that run to review its correction.",
            );
            return;
          }
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
        setFixingRunSeq((current) => (current === run.seq ? null : current));
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

  const { nodes, edges } = useMemo(
    () =>
      graph
        ? layout(graph, paintedStates, paintedElapsed)
        : { nodes: [], edges: [] },
    [graph, paintedStates, paintedElapsed],
  );

  const selected = workflows.find((w) => w.id === selectedId) ?? null;

  // id → display name, for the toolbar picker's closed state. A workflow's id
  // is not its name, and the trigger renders the raw value unless it is handed
  // this mapping — issue #270, where the closed picker read
  // `e2e_conflict_1785687393855` while the open popup read "Conflict probe".
  const workflowLabels = useMemo(
    () => Object.fromEntries(workflows.map((w) => [w.id, w.name])),
    [workflows],
  );

  // The full node model (kind/name/summary/agent/config) for the clicked node,
  // looked up from the loaded graph so the inspector shows fields the laid-out
  // canvas node data doesn't carry (agent, config, …).
  const selectedNode = useMemo(
    () => graph?.nodes.find((n) => n.id === selectedNodeId) ?? null,
    [graph, selectedNodeId],
  );

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
      <div className="flex flex-wrap items-center justify-between gap-2 border-b px-4 py-3">
        <div className="flex min-w-0 items-center gap-2">
          <h2 className="text-sm font-semibold">Workflows</h2>
          <Badge variant="secondary">{workflows.length}</Badge>
          {/* Issue #228: the last run's outcome at a glance — including for a
              scheduled run nobody watched, which is the case the issue is
              about. Absent until this workflow has run at least once. */}
          {lastRun && <LastRunChip run={lastRun} />}
          {/* Issue #276: state an operator must not have to hover to learn. A
              paused workflow looks exactly like a live one otherwise, and the
              case that matters most — a schedule the disarm rule switched off on
              create — has never run, so there is no LastRunChip to hint at it. */}
          {paused && (
            <Badge variant="outline" data-testid="workflow-paused-badge">
              Schedule paused
            </Badge>
          )}
          {selected?.description && (
            <p className="hidden truncate text-xs text-muted-foreground sm:block">
              {selected.description}
            </p>
          )}
        </div>
        {/* Issue #824. `flex-wrap` on the ACTION ROW, not just on the header
            above it. The header already wraps, but that wrap only breaks
            between its two children — and this row is one child whose buttons
            are `shrink-0`, so on its own line it stayed 1386px wide inside a
            1289px container and overflowed into an `overflow-hidden` ancestor.
            Nothing in that chain scrolls, so `New workflow` was not merely
            off-screen, it could not be clicked.

            The row grows every time a control is added — `Pause` (#814) took
            the overhang from 22px to 113px, which is what finally made it
            visible — so it needs a break point of its own rather than a wider
            budget. `justify-end` keeps the wrapped line aligned to the right
            edge it belongs to instead of drifting left under the title. */}
        <div className="flex min-w-0 flex-wrap items-center justify-end gap-2">
          <Select
            // Issue #406, half one. NOT `selectedId ?? undefined`: Base UI
            // decides controlled-vs-uncontrolled ONCE, on the first render,
            // from `value !== undefined` — and on that render the list has not
            // arrived, so `selectedId` is still null. Handing it `undefined`
            // there locked the picker uncontrolled for its whole life, and
            // every selection this view makes for itself — the auto-select
            // when the list lands, a card in Browse, the re-select after a
            // delete — then moved `selectedId` without ever reaching the
            // picker, which went on rendering its own untouched initial value.
            // Clicking an option worked, which is why only the operator saw
            // this. `null` is Base UI's own "nothing is selected", so passing
            // it keeps the picker controlled from the first render on.
            value={selectedId}
            onValueChange={(v) => setSelectedId(v)}
            disabled={loadingList || workflows.length === 0}
            // Issue #406, half two. This map is what makes the trigger show a
            // name instead of an id (#270). It replaces a `SelectValue`
            // function child that did the same lookup by hand — because a
            // function child ALSO overrides `placeholder`, which is how an
            // empty selection came to render nothing at all rather than "Pick
            // a workflow". Formatting through `items` leaves the placeholder
            // reachable.
            items={workflowLabels}
          >
            <SelectTrigger className="h-8 w-56">
              <SelectValue placeholder={loadingList ? "Loading…" : "Pick a workflow"} />
            </SelectTrigger>
            <SelectContent>
              {workflows.map((w) => (
                <SelectItem key={w.id} value={w.id}>
                  {w.name}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <Input
            value={request}
            onChange={(e) => setRequest(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && selectedId && !running && !loadingGraph) {
                void run();
              }
            }}
            disabled={!selectedId || running}
            placeholder="What should this run work on?"
            aria-label="Request for this run"
            className="h-8 w-64"
          />
          <Button size="sm" onClick={() => void run()} disabled={!selectedId || running || loadingGraph}>
            {running ? (
              <Loader2 className="mr-1.5 size-4 animate-spin" />
            ) : (
              <Play className="mr-1.5 size-4" />
            )}
            Run
          </Button>
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
          {/* Issue #383. Present only while a run this view started is actually
              in flight — which is knowable at all because the host now hands
              back the run id before the run ends. Hidden once the host has told
              us it cannot stop runs, rather than left there to fail every time. */}
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
          {/* Issue #303. Deliberately placed AFTER the picker and Run: this
              toggles what fills the body, and grouping it with the other
              body-level toggle (History) reads better than sitting beside the
              selection controls it does not change. */}
          <Button
            size="sm"
            variant="outline"
            onClick={() => setIndexOpen((open) => !open)}
            aria-pressed={indexOpen}
            data-testid="workflow-browse-toggle"
            title="Browse every workflow as cards or as a list."
          >
            <LayoutGrid className="mr-1.5 size-4" />
            Browse
          </Button>
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
                    variant="outline"
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
                    This removes the workflow and stops it running on its schedule. Past runs stay
                    in the run history. This can&apos;t be undone.
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
          {/* Issue #341: THE control named "New workflow". It is in the
              toolbar in every state, so it is the one an operator — or a
              screen reader, or a spec — should find under that name. The
              empty-state call to action below is named differently on
              purpose; two buttons answering to one name is an ambiguity
              nothing can resolve. */}
          <Button
            size="sm"
            variant="outline"
            onClick={() => setCreateOpen(true)}
            data-testid="workflow-create"
          >
            <Plus className="mr-1.5 size-4" />
            New workflow
          </Button>
        </div>
      </div>

      {/* Issue #259: a write refused because the graph moved under us. Distinct
          from `error` on purpose — this one is recoverable, and the recovery is
          right here, so it must not be mistaken for a generic load failure. */}
      {conflict && (
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
      {runRefusal && (
        <div className="px-4 pt-3">
          <Alert variant="destructive" data-testid="workflow-run-inference-alert">
            <AlertDescription className="flex flex-wrap items-center justify-between gap-2">
              <span>
                {runRefusal.code === "inference_required"
                  ? "This company has no inference provider configured, so workflows can't run. Set a provider under Settings → Connections → Inference, then run again."
                  : runRefusal.message}
              </span>
              {runRefusal.code === "inference_required" && (
                <a
                  href="#/settings/connections"
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

      {error && (
        <div className="px-4 pt-3">
          <Alert variant="destructive">
            <AlertDescription>{error}</AlertDescription>
          </Alert>
        </div>
      )}

      {/* Issue #371: the canvas is showing a PAST run, not the live one. Said
          plainly, with the way out attached — an unexplained ring on a node
          would otherwise read as the current state of the workflow. */}
      {overlayRun && (
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

      <div className="relative flex-1">
        {/* Issue #303: browsing REPLACES the canvas rather than squeezing in
            above it. A card grid needs the width, and the canvas is meaningless
            while the operator is deciding which workflow they want. Picking one
            drops straight back to that workflow's canvas. */}
        {indexOpen ? (
          <WorkflowIndex
            workflows={workflows}
            runsByWorkflow={runsByWorkflow}
            selectedId={selectedId}
            onSelect={(id) => {
              setSelectedId(id);
              setIndexOpen(false);
            }}
            mode={indexMode}
            onModeChange={(mode) => {
              setIndexMode(mode);
              writeIndexMode(mode);
            }}
            loading={loadingList}
            runsLoaded={indexRunsLoaded}
          />
        ) : loadingList || loadingGraph ? (
          <div className="absolute inset-0 p-4">
            <Skeleton className="h-full w-full rounded-xl" />
          </div>
        ) : !selectedId ? (
          <div className="flex h-full flex-col items-center justify-center gap-3 px-4 text-center text-sm text-muted-foreground">
            <p>This company has no saved workflows yet.</p>
            {/* Issue #813: a first-time author has no on-ramp otherwise. One
                compact prose block — what a workflow is, a worked example
                (mirroring the copilot placeholder), and the create-time copilot
                as the easiest path. Deliberately no template gallery. */}
            <div className="max-w-md space-y-2 text-2xs leading-relaxed">
              <p>
                A workflow runs a sequence of steps on a schedule or on demand: a{" "}
                <span className="font-medium text-foreground">trigger</span> starts it,{" "}
                <span className="font-medium text-foreground">agents</span> and{" "}
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
          <>
            <ReactFlow
              nodes={nodes}
              edges={edges}
              nodeTypes={NODE_TYPES}
              colorMode={resolvedTheme === "dark" ? "dark" : "light"}
              fitView
              fitViewOptions={{ padding: 0.2 }}
              nodesDraggable={false}
              nodesConnectable={false}
              elementsSelectable
              onNodeClick={onNodeClick}
              onPaneClick={() => setSelectedNodeId(null)}
              proOptions={{ hideAttribution: true }}
            >
              <Background variant={BackgroundVariant.Dots} gap={20} size={1} />
              <Controls showInteractive={false} />
              <MiniMap pannable zoomable className="!hidden sm:!block" />
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
                  output={selectedNodeOutput}
                  onClose={() => setSelectedNodeId(null)}
                />
              )
            )}
          </>
        )}
      </div>

      {result && (
        <RunResultPanel
          result={result}
          graph={graph}
          request={ranWith}
          onClose={() => setResult(null)}
          // Issue #1002: the whole queue, narrowed by the panel to this run.
          // Handed in unfiltered on purpose — the Approvals page reads the same
          // array and must keep showing every row.
          approvals={approvals}
          now={approvalsNow}
          askerNames={askerNames}
          deciding={decidingApprovals}
          decided={decidedApprovals}
          failed={failedApprovals}
          onDecide={onDecideApproval}
        />
      )}

      {/* Issue #1007: the same slot, for the outcome that had no surface at all.
          The two are mutually exclusive by construction — `run()` clears both on
          dispatch and only one of its arms sets one — so they are rendered as
          siblings rather than as a branch. */}
      {runFailure && (
        <RunFailurePanel
          failure={runFailure}
          onClose={() => setRunFailure(null)}
        />
      )}

      {historyOpen && historySupported && (
        <RunHistoryPanel
          runs={historyRows}
          graph={graph}
          workflowName={selected?.name ?? selectedId ?? ""}
          onClose={() => setHistoryOpen(false)}
          selectedRunSeq={overlayRun?.seq ?? null}
          onSelectRun={(picked) =>
            // Clicking the row already shown clears it, so the control is a
            // toggle rather than a one-way trip into overlay mode.
            setOverlayRun((prev) => (prev?.seq === picked.seq ? null : picked))
          }
          onFixWithCopilot={handleFixWithCopilot}
          fixingRunSeq={fixingRunSeq}
          fixReason={fixReason}
        />
      )}

      <WorkflowCreateDialog
        client={client}
        company={company}
        open={createOpen}
        onOpenChange={setCreateOpen}
        onCreated={handleCreated}
      />

      {/* Issue #259: the same dialog in edit mode. `graph` carries the version
          token the save is conditional on, so it is passed as loaded rather
          than copied — and a 409 lands in the banner above, which outlives the
          dialog and holds the way out.

          `open` is gated on `graph` too: without it, a selection that goes away
          under an open dialog (a company switch, a failed re-read) would leave
          `workflow` null, which IS create mode — the operator would be looking
          at a blank New workflow form wearing the Edit title. */}
      <WorkflowCreateDialog
        client={client}
        company={company}
        open={editOpen && graph !== null}
        onOpenChange={(o) => {
          setEditOpen(o);
          // Issue #840 (PR-3): a copilot correction is single-use — dropping it on
          // close means the next plain Edit hydrates from the saved graph, not a
          // stale correction.
          if (!o) setPrefilledDraft(null);
        }}
        workflow={graph}
        onSaved={handleSaved}
        onConflict={setConflict}
        prefilledDraft={prefilledDraft}
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
