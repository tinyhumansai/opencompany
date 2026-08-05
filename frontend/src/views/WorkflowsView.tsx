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
  History,
  LayoutGrid,
  Loader2,
  Pencil,
  Play,
  Plus,
  RotateCw,
  Square,
  Trash2,
} from "lucide-react";
import { toast } from "sonner";

import {
  cancelWorkflowRun,
  deleteWorkflow,
  getWorkflow,
  isDetached,
  listWorkflowRuns,
  listWorkflows,
  runWorkflow,
  type WorkflowGraph,
  type WorkflowRunOutcome,
  type WorkflowRunResult,
  type WorkflowSummary,
} from "@/api/workflows";
import type { CompanyStreamEvent } from "@/hooks/use-events";
import type { OpenCompanyClient } from "@/api/client";
import { ApiError } from "@/api/types";
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
import { Button } from "@/components/ui/button";
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
} from "@/views/workflows/graph";
import { LastRunChip, RunHistoryPanel } from "@/views/workflows/RunHistoryPanel";
import { WorkflowIndex, type IndexMode } from "@/views/workflows/WorkflowIndex";
import { CopilotPanel } from "@/views/workflows/CopilotPanel";
import { RunResultPanel } from "@/views/workflows/RunResultPanel";
import { NodeDetailPanel } from "@/views/workflows/NodeDetailPanel";

const NODE_TYPES = { oc: WorkflowNode };

/** A stable empty default for `runEvents`, so an omitted prop does not hand the
 * fold a fresh array identity on every render. */
const EMPTY_RUN_EVENTS: CompanyStreamEvent[] = [];

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
  runEventTick = 0,
  runEvents = EMPTY_RUN_EVENTS,
}: {
  client: OpenCompanyClient;
  company: string | null;
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
}) {
  const { resolvedTheme } = useTheme();
  const [workflows, setWorkflows] = useState<WorkflowSummary[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [graph, setGraph] = useState<WorkflowGraph | null>(null);
  const [loadingList, setLoadingList] = useState(true);
  const [loadingGraph, setLoadingGraph] = useState(false);
  const [result, setResult] = useState<WorkflowRunResult | null>(null);
  // Issue #154: what the operator is asking this run to work on. `ranWith` is
  // pinned when the run is dispatched so the result panel echoes the request the
  // shown output came from, not whatever has been typed since.
  const [request, setRequest] = useState("");
  const [ranWith, setRanWith] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [createOpen, setCreateOpen] = useState(false);
  // Issue #259: the same dialog, hydrated from the selected graph. Separate
  // state from `createOpen` rather than a mode flag, so the create path keeps
  // working exactly as it did and neither can be half-open.
  const [editOpen, setEditOpen] = useState(false);
  const [selectedNodeId, setSelectedNodeId] = useState<string | null>(null);
  // Issue #228: what past runs did, read back from the host's journal. This is
  // the half that survives a reload — before it, a manual run's delivery rows
  // vanished when the drawer was dismissed and a scheduled run's never reached
  // the operator at all.
  const [runs, setRuns] = useState<WorkflowRunOutcome[]>([]);
  const [historyOpen, setHistoryOpen] = useState(false);
  // A host predating the runs route answers 404. That is not an error worth
  // showing — the rest of the view works — so it just means "no history here".
  const [historySupported, setHistorySupported] = useState(true);
  // Bumped after a manual run so the history picks it up without a reload.
  const [runsTick, setRunsTick] = useState(0);
  // Issue #259: a delete in flight, and the host's message when a write was
  // refused because the graph moved under us.
  const [deleting, setDeleting] = useState(false);
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
  // Run ids the live fold has actually seen frames for. The fallback above
  // consults it so a console WITH a working stream never double-paints a run it
  // already watched, and one without it still gets the journaled answer.
  const liveRanRef = useRef<Set<string>>(new Set());

  // Load the workflow list once, and auto-select the first entry.
  useEffect(() => {
    let live = true;
    (async () => {
      try {
        const rows = await listWorkflows(client, company);
        if (!live) return;
        setWorkflows(rows);
        // Keep the selection only if it still exists in the freshly loaded list
        // (this effect also reruns on company change) — otherwise a stale id
        // from the previous company would fetch the wrong/nonexistent graph.
        setSelectedId((prev) =>
          prev && rows.some((r) => r.id === prev) ? prev : (rows[0]?.id ?? null),
        );
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
  }, [client, company]);

  // Fetch the selected workflow's full graph.
  useEffect(() => {
    if (!selectedId) {
      setGraph(null);
      return;
    }
    let live = true;
    setLoadingGraph(true);
    setResult(null);
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

  const run = useCallback(async () => {
    if (!selectedId) return;
    setStarting(true);
    // Issue #371: clear the previous run's marks and paint the opening frontier
    // immediately, so the canvas responds to the click rather than waiting on
    // the first frame. The `workflow_run_started` frame re-sets the same thing
    // a moment later, which is idempotent.
    setOverlayRun(null);
    setOptimistic(graph ? initialRunState(graph) : null);
    // Trimmed once here so the echoed request and the payload the host receives
    // can never disagree.
    const asked = request.trim();
    try {
      // Issue #383: ask to detach. The host answers as soon as the run has an
      // id, so the console stops holding a request open for the whole run — the
      // thing a proxy's idle timeout severs.
      const res = await runWorkflow(
        client,
        company,
        selectedId,
        asked ? { request: asked } : {},
        { detach: true },
      );
      setRanWith(asked);
      // Discriminate on the SHAPE, never on what we asked for. A host predating
      // #383 ignores `detach` and answers with the settled run — a perfectly
      // good answer, just a different one.
      if (isDetached(res)) {
        setActiveRunId(res.runId);
        setAwaitingRunId(res.runId);
        toast.success("Workflow started.");
      } else {
        setResult(res);
        setAwaitingRunId(res.runId ?? null);
        toast.success("Workflow ran.");
      }
      // The run is journaled host-side (#228); pull the history forward so the
      // chip and the panel reflect it immediately.
      setRunsTick((n) => n + 1);
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "could not run the workflow");
      // A run that failed is journaled too (#228), and is the outcome most
      // worth finding again later — so refresh the history on this path as well.
      setRunsTick((n) => n + 1);
      // Drop the optimistic frontier so a failed run does not leave a node
      // pulsing "running" forever. The fold owns anything actually reported.
      setOptimistic(null);
      // Nothing was accepted, so nothing is in flight to guard against or to
      // offer a Cancel for.
      setActiveRunId(null);
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
      // re-list would flash an empty picker.
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

  // Issue #259: the edit dialog saved. The host answers with the stored graph
  // AND a fresh version token, so holding onto it is what lets the operator
  // save again without a re-read — dropping it and re-fetching would be a round
  // trip that can only return the same thing.
  const handleSaved = useCallback((saved: WorkflowGraph) => {
    setGraph(saved);
    // The name and description are editable, so the picker entry has to move
    // with them. The id cannot change, which is what makes this a rewrite of
    // one row rather than a re-list.
    setWorkflows((prev) =>
      prev
        .map((w) =>
          w.id === saved.id
            ? { ...w, name: saved.name, description: saved.description }
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
    setWorkflows((prev) => {
      const rest = prev.filter((w) => w.id !== created.id);
      return [...rest, { id: created.id, name: created.name, description: created.description }].sort(
        (a, b) => a.name.localeCompare(b.name),
      );
    });
    setSelectedId(created.id);
    toast.success("Workflow created.");
  }, []);

  // Issue #371: the live canvas state, FOLDED from the frame window rather than
  // accumulated frame by frame.
  //
  // A fold is what makes this correct under React batching: several frames can
  // land in one render, and an accumulating reducer would see only the last —
  // losing a `workflow_run_started` that way strands every node frame behind
  // it. Recomputing from the window instead has no such state to lose.
  const liveRun = useMemo(
    () => foldLiveRun(runEvents, selectedId, graph),
    [runEvents, selectedId, graph],
  );

  // The optimistic frontier is only for the gap before the first frame. Once
  // the fold has adopted a run, it is the authority — and that run is recorded
  // so the no-stream fallback knows it was watched live.
  useEffect(() => {
    if (!liveRun) return;
    liveRanRef.current.add(liveRun.runId);
    setOptimistic(null);
  }, [liveRun]);

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
  useEffect(() => {
    if (!activeRunId || !historySupported) return;
    const timer = window.setInterval(() => setRunsTick((n) => n + 1), 2_000);
    return () => window.clearInterval(timer);
  }, [activeRunId, historySupported]);

  // Switching workflow (or company) clears the canvas: another graph's node ids
  // are meaningless here, and a stale mark on a same-named node would be a lie.
  //
  // It also drops the in-flight guard: the run keeps going host-side and still
  // journals, but this view is no longer the place watching it, and leaving a
  // Cancel button pointed at another workflow's run would be worse than losing
  // the affordance.
  useEffect(() => {
    setOptimistic(null);
    setOverlayRun(null);
    setActiveRunId(null);
  }, [selectedId, company]);

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

  // The full node model (kind/name/summary/agent/config) for the clicked node,
  // looked up from the loaded graph so the inspector shows fields the laid-out
  // canvas node data doesn't carry (agent, config, …).
  const selectedNode = useMemo(
    () => graph?.nodes.find((n) => n.id === selectedNodeId) ?? null,
    [graph, selectedNodeId],
  );

  const onNodeClick = useCallback((_: unknown, node: Node) => {
    setSelectedNodeId(node.id);
  }, []);

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
          {selected?.description && (
            <p className="hidden truncate text-xs text-muted-foreground sm:block">
              {selected.description}
            </p>
          )}
        </div>
        <div className="flex items-center gap-2">
          <Select
            value={selectedId ?? undefined}
            onValueChange={(v) => setSelectedId(v)}
            disabled={loadingList || workflows.length === 0}
          >
            <SelectTrigger className="h-8 w-56">
              {/* The trigger renders the raw value unless told otherwise, and a
                  workflow's id is not its name — the closed picker showed
                  `e2e_conflict_1785687393855` where the popup showed "Conflict
                  probe". Resolve it back through the list the popup is built
                  from; fall back to the id if the list has not arrived yet. */}
              <SelectValue placeholder={loadingList ? "Loading…" : "Pick a workflow"}>
                {(selected) =>
                  workflows.find((w) => w.id === selected)?.name ?? String(selected ?? "")
                }
              </SelectValue>
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
              {runs.length > 0 && (
                <Badge variant="secondary" className="ml-1.5 h-4 px-1.5 text-[10px] font-normal">
                  {runs.length}
                </Badge>
              )}
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
          <Button size="sm" variant="outline" onClick={() => setCreateOpen(true)}>
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
            <Button size="sm" variant="outline" onClick={() => setCreateOpen(true)}>
              <Plus className="mr-1.5 size-4" />
              New workflow
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
                onClose={() => setCopilotOpen(false)}
              />
            ) : (
              selectedNode && (
                <NodeDetailPanel node={selectedNode} onClose={() => setSelectedNodeId(null)} />
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
        />
      )}

      {historyOpen && historySupported && (
        <RunHistoryPanel
          runs={runs}
          workflowName={selected?.name ?? selectedId ?? ""}
          onClose={() => setHistoryOpen(false)}
          selectedRunSeq={overlayRun?.seq ?? null}
          onSelectRun={(picked) =>
            // Clicking the row already shown clears it, so the control is a
            // toggle rather than a one-way trip into overlay mode.
            setOverlayRun((prev) => (prev?.seq === picked.seq ? null : picked))
          }
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
        onOpenChange={setEditOpen}
        workflow={graph}
        onSaved={handleSaved}
        onConflict={setConflict}
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
