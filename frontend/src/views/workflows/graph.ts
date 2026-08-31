// Pure graph + run-state helpers for the Workflows surface.
//
// Extracted verbatim from `WorkflowsView.tsx` (issue #303) when that file passed
// 1800 lines and was about to grow an index and a copilot. Nothing here touches
// React: it is layout arithmetic and folds over run data, which is exactly the
// part worth testing and reusing on its own.

import type { Edge, Node } from "@xyflow/react";

import type { WorkflowGraph, WorkflowRunOutcome } from "@/api/workflows";
import type { CompanyStreamEvent } from "@/hooks/use-events";
import {
  nodeKindMeta,
  type NodeRunState,
  type WorkflowNodeData,
} from "@/lib/workflow-sample";

/** Horizontal gap between layers and vertical gap between nodes in a layer. */
const COL_GAP = 300;
const ROW_GAP = 150;

/**
 * A nominal node size, for consumers that read the node object we hand React
 * Flow rather than the size React Flow measured (issue #1230).
 *
 * The minimap is the one that matters. `MiniMapNodes` reads
 * `node.internals.userNode` — the object THIS function returns — and skips any
 * node for which `measured ?? width ?? initialWidth` is undefined. This layout
 * is a `useMemo` that rebuilds every node on every run-state repaint, so React
 * Flow's measurement never survives onto that object and the minimap painted
 * **nothing**: an empty 200x150 box in every workflow, in both themes, while
 * `fitView` worked fine because it reads `node.internals` instead.
 *
 * `initialWidth`/`initialHeight` rather than `width`/`height` on purpose: they
 * are a hint, and `measured` still wins for layout, hit-testing and the fit. So
 * nothing on the canvas moves — only the minimap gains something to draw.
 *
 * Measured values on a real graph run 180-204 x 56-71 (the node grows with a
 * summary line); these are the middle of that, which is all a 200px-wide
 * overview needs.
 */
export const NODE_W = 190;
export const NODE_H = 64;

/** Default minimap container size — React Flow's own defaults, kept as the
 * ceiling so a graph with real vertical spread renders exactly as before. */
const MINIMAP_WIDTH = 200;
const MINIMAP_MAX_HEIGHT = 150;
/** Floor so the minimap never shrinks to an unusable, unclickable strip. */
const MINIMAP_MIN_HEIGHT = 96;
/**
 * The minimum fraction of the minimap's own rendered height that a laid-out
 * node's real height should occupy (issue #1259).
 *
 * React Flow's built-in `<MiniMap>` "contain"-fits a bounding box into
 * whatever width/height it is given — `viewScale = Math.max(scaledWidth,
 * scaledHeight)`, not overridable via any prop (verified against the vendored
 * `@xyflow/react` source). A graph whose nodes sit at zero vertical spread
 * (every shipped company workflow template: one node per depth layer, `layout`
 * below never gives two nodes the same x) has a content box tens of pixels
 * tall and thousands of pixels wide. Fit into the default 200x150 container,
 * that padded the viewBox's height ~30-40x past the real content, squashing
 * every node rect into an imperceptible sliver — #1230 made the rects exist,
 * but they were still invisible in practice.
 *
 * Shrinking the container alone turned out not to be enough: the built-in
 * `<MiniMap>`'s bounding box is the union of the node bounds AND the current
 * pan/zoom viewport rect (`getBoundsOfRects(nodesBounds, viewBB)` in the
 * vendored source), and for a wide, short graph the on-screen canvas itself —
 * zoomed out to fit that width, within React Flow's default `minZoom` floor —
 * ends up with a viewport rect far taller than any node. That viewport
 * height, not the node height, dominated the built-in bounding box no matter
 * what container size the minimap was given, so shrinking the container just
 * moved which axis inflated. `WorkflowMiniMap` (in `WorkflowMiniMap.tsx`)
 * replaces the built-in component so the minimap's own scale comes from
 * `contentBounds` below — node positions only, never the live viewport —
 * and `minimapDimensions` here picks a container height low enough that a
 * node's real height fills at least this fraction of it, floored so the
 * minimap never becomes an unusable sliver itself, and capped at the
 * previous default so a graph with real vertical spread (siblings sharing a
 * layer) renders exactly as it did before this existed.
 */
const NODE_MIN_VISIBLE_FRACTION = 0.25;

/** The pure content bounding box of a laid-out node array: position plus the
 * same nominal size hint the minimap itself reads (`initialWidth`/
 * `initialHeight`, falling back to NODE_W/NODE_H — see #1230). Deliberately
 * independent of React Flow's own internal bounding-box calculation, which
 * unions in the live pan/zoom viewport (see NODE_MIN_VISIBLE_FRACTION above)
 * — this is node geometry only, so both `minimapDimensions` and
 * `WorkflowMiniMap`'s own render math can share one source of truth. */
export function contentBounds(nodes: Node<WorkflowNodeData>[]): {
  minX: number;
  minY: number;
  width: number;
  height: number;
} {
  if (nodes.length === 0) {
    return { minX: 0, minY: 0, width: 0, height: 0 };
  }
  let minX = Infinity;
  let minY = Infinity;
  let maxX = -Infinity;
  let maxY = -Infinity;
  for (const n of nodes) {
    const w = n.initialWidth ?? NODE_W;
    const h = n.initialHeight ?? NODE_H;
    minX = Math.min(minX, n.position.x);
    minY = Math.min(minY, n.position.y);
    maxX = Math.max(maxX, n.position.x + w);
    maxY = Math.max(maxY, n.position.y + h);
  }
  return { minX, minY, width: maxX - minX, height: maxY - minY };
}

/**
 * The minimap's own container size for a laid-out node array (issue #1259).
 *
 * Derived from `contentBounds` above, not React Flow's post-mount DOM
 * measurement, so it is available synchronously off the same array `layout()`
 * just produced.
 */
export function minimapDimensions(
  nodes: Node<WorkflowNodeData>[],
): { width: number; height: number } {
  const bounds = contentBounds(nodes);
  if (bounds.width <= 0 || bounds.height <= 0) {
    return { width: MINIMAP_WIDTH, height: MINIMAP_MAX_HEIGHT };
  }
  const idealHeight =
    (bounds.height * MINIMAP_WIDTH) / (bounds.width * NODE_MIN_VISIBLE_FRACTION);
  const height = Math.min(
    MINIMAP_MAX_HEIGHT,
    Math.max(MINIMAP_MIN_HEIGHT, idealHeight),
  );
  return { width: MINIMAP_WIDTH, height };
}

/** Matches the built-in `<MiniMap>`'s default — padding around the content so
 * nodes at the very edge aren't drawn flush against the minimap's border. */
const MINIMAP_OFFSET_SCALE = 5;

/**
 * The minimap SVG's `viewBox`, for a laid-out node array (issue #1259).
 *
 * The same "contain" fit React Flow's built-in `<MiniMap>` itself uses —
 * `viewScale = Math.max(scaledWidth, scaledHeight)`, scaled up to
 * `minimapDimensions`'s container size, padded by `MINIMAP_OFFSET_SCALE` —
 * but computed from `contentBounds` alone. `WorkflowMiniMap.tsx` draws node
 * rects directly at their real `position`, so this is the only piece of
 * scaling math the minimap needs, and it's plain arithmetic on `nodes` —
 * worth its own pure, tested function rather than living inline in a
 * component.
 */
export function minimapViewBox(nodes: Node<WorkflowNodeData>[]): {
  x: number;
  y: number;
  width: number;
  height: number;
} {
  const bounds = contentBounds(nodes);
  const { width: containerWidth, height: containerHeight } =
    minimapDimensions(nodes);
  if (bounds.width <= 0 || bounds.height <= 0) {
    return { x: 0, y: 0, width: containerWidth, height: containerHeight };
  }
  const scaledWidth = bounds.width / containerWidth;
  const scaledHeight = bounds.height / containerHeight;
  const viewScale = Math.max(scaledWidth, scaledHeight);
  const offset = MINIMAP_OFFSET_SCALE * viewScale;
  const viewWidth = viewScale * containerWidth;
  const viewHeight = viewScale * containerHeight;
  return {
    x: bounds.minX - (viewWidth - bounds.width) / 2 - offset,
    y: bounds.minY - (viewHeight - bounds.height) / 2 - offset,
    width: viewWidth + offset * 2,
    height: viewHeight + offset * 2,
  };
}

/** The result of folding the SSE frame window down to one run's canvas state. */
export interface LiveRun {
  runId: string;
  states: Record<string, NodeRunState>;
  elapsed: Record<string, number>;
  /** False once the run has settled — its ok/error marks stay, its running ones go. */
  active: boolean;
  /**
   * Whether a cron started this run rather than an operator (issue #528).
   *
   * Read verbatim off the winning `workflow_run_started` frame. It lets the
   * console tell its OWN manual run apart from a concurrent scheduled fire while
   * the run POST is still open: the synchronous run path (#528) re-seeds the
   * mid-run Stop button and the connection-lost triage from the live fold, and a
   * cron run — which nobody clicked Run for — must never feed either.
   */
  scheduled: boolean;
}

/** A run the host says is still in flight, read from the run history (issue
 * #863) so the canvas can join a run it did not watch from the beginning.
 *
 * The frame window only holds what arrived since this console connected, so
 * everything before that — a cron fire, a run started from chat, the run that
 * was already walking when the tab was opened or the page reloaded, the frames
 * lost while an `EventSource` reconnected — is invisible to it. The host
 * journals the same trail durably and serves it on `…/workflows/runs` with
 * `running: true`, which is what this carries in. */
export interface InFlightRun {
  runId: string;
  /** The nodes the host has already recorded as finished for this run. */
  states: Record<string, NodeRunState>;
  elapsed: Record<string, number>;
  scheduled: boolean;
}

/** Folds the run-progress frame window (issue #371) into the canvas state for
 * the workflow on screen.
 *
 * Pure, and recomputed from scratch on every window change. That is the point:
 * an accumulating reducer loses frames that arrive inside one React batch,
 * which for a graph with a sub-millisecond transform node is the normal case.
 *
 * Only the MOST RECENT run of the selected workflow is folded, and every frame
 * is matched on its run id. One SSE connection carries every run in the
 * company, so without that a cron fire would repaint a canvas an operator is
 * watching, and two concurrent runs of the same graph would interleave into one
 * incoherent picture.
 *
 * Issue #863: `inFlight` is the run the *host* says is open, and it is what
 * this fold adopts when the window carries no start frame for the selected
 * workflow. Without it a console that joined mid-run painted nothing at all —
 * not a partial trail, nothing — for the whole run, because the fold's only
 * way to learn a run's id was a `workflow_run_started` frame it had missed.
 * The window still wins when it has a start of its own: a live start is newer
 * than any history read, and a rerun must supersede the run before it.
 *
 * Issue #921: `settledRunIds` is the one thing the window does NOT get to win.
 * A run is active here only while NO `workflow_run_finished` frame for it has
 * arrived — so a stream that dies mid-run leaves the fold reporting `active`
 * forever, with the node it was on stuck pulsing and the header still reading
 * "running" three minutes after the host finished. The frame that would clear
 * it is exactly the one that did not arrive, so waiting for it is waiting for
 * the thing that broke.
 *
 * The host's run history is the authority on *settled*, and it is reachable by
 * polling rather than by a frame: a run the host lists as no longer `running`
 * IS finished, whatever the window still shows. The reported ok/error marks
 * survive — `settle` only clears the orphaned `running` ones — so the canvas
 * keeps its honest "how far did it get?" answer rather than blanking. */
export function foldLiveRun(
  events: CompanyStreamEvent[],
  selectedId: string | null,
  graph: WorkflowGraph | null,
  inFlight?: InFlightRun | null,
  settledRunIds?: ReadonlySet<string>,
): LiveRun | null {
  if (!selectedId || !graph) return null;

  // The last start for this workflow wins — a rerun supersedes the run before.
  let startIndex = -1;
  for (let i = events.length - 1; i >= 0; i--) {
    const e = events[i];
    if (e.type === "workflow_run_started" && e.workflowId === selectedId) {
      startIndex = i;
      break;
    }
  }
  if (startIndex === -1)
    return inFlight
      ? foldFromHistory(events, graph, inFlight, settledRunIds)
      : null;
  const started = events[startIndex];
  if (started.type !== "workflow_run_started") return null;

  const runId = started.runId;
  const scheduled = started.scheduled;
  // The trigger fired by definition, and the engine reports no step for it, so
  // nothing else would ever mark it. Every other node's state is REPORTED by the
  // host from here (#382), not derived.
  const states = initialRunState(graph);
  const elapsed: Record<string, number> = {};
  const active =
    applyFrames(events, startIndex + 1, runId, selectedId, states, elapsed) &&
    !settledRunIds?.has(runId);

  settle(states, active);
  return { runId, states, elapsed, active, scheduled };
}

/** The same fold, for a run this console did not see start (issue #863).
 *
 * Seeded from the host's own record of the run rather than from a start frame,
 * then brought up to date with every frame in the window that belongs to it —
 * so a console that joined mid-run shows the trail so far AND keeps painting as
 * the rest of the graph walks.
 *
 * The whole window is scanned, not a suffix: there is no start frame to scan
 * from, and every frame is matched on the run id anyway, so a frame belonging
 * to some other run cannot be picked up wherever it sits. */
function foldFromHistory(
  events: CompanyStreamEvent[],
  graph: WorkflowGraph,
  inFlight: InFlightRun,
  settledRunIds?: ReadonlySet<string>,
): LiveRun {
  const states = { ...initialRunState(graph), ...inFlight.states };
  const elapsed = { ...inFlight.elapsed };
  // `selectedId` is not passed on: a run id is the stronger match, and the
  // host told us this run is the selected workflow's. A pre-#371 finish frame
  // (no run id) is deliberately NOT honoured here — with no start frame to pair
  // it with, "the run on screen" is a guess, and guessing a run settled would
  // clear a live node on a console that is watching it correctly.
  const active =
    applyFrames(events, 0, inFlight.runId, null, states, elapsed) &&
    !settledRunIds?.has(inFlight.runId);

  settle(states, active);
  return {
    runId: inFlight.runId,
    states,
    elapsed,
    active,
    scheduled: inFlight.scheduled,
  };
}

/** Applies every frame belonging to `runId` from `from` onward, and reports
 * whether the run is still active afterwards.
 *
 * `selectedId` widens the settle check to a `workflow_run_finished` that
 * carries no run id (a pre-#371 host); pass `null` where there is no start
 * frame to pair such a frame with. */
function applyFrames(
  events: CompanyStreamEvent[],
  from: number,
  runId: string,
  selectedId: string | null,
  states: Record<string, NodeRunState>,
  elapsed: Record<string, number>,
): boolean {
  let active = true;
  for (let i = from; i < events.length; i++) {
    const e = events[i];
    // Issue #382: the engine now reports when a node BEGINS, so "running" is a
    // fact rather than the old topology-derived frontier guess. Light the node
    // up; its later finished frame overwrites this with ok/error. Guarded so a
    // frame that somehow arrived out of order cannot downgrade a settled node
    // back to running.
    if (e.type === "workflow_node_started") {
      if (e.runId !== runId) continue;
      if (
        states[e.nodeId] !== "ok" &&
        states[e.nodeId] !== "error" &&
        states[e.nodeId] !== "blocked"
      ) {
        states[e.nodeId] = "running";
      }
      continue;
    }
    if (e.type === "workflow_node_finished") {
      if (e.runId !== runId) continue;
      // Issue #881: three reported readings now, not two. `blocked` is named
      // explicitly and everything else that is not "ok" stays a failure — an
      // unknown status word from a newer host must never paint a node as
      // succeeded, and must not be quietly promoted to "blocked" either.
      states[e.nodeId] = nodeStateFrom(e.status);
      elapsed[e.nodeId] = e.elapsedMs;
      continue;
    }
    if (e.type === "workflow_run_finished") {
      // A pre-#371 host sends no runId; treat that as "the run on screen"
      // rather than ignoring it, else the canvas would spin forever.
      if (e.runId === runId) active = false;
      else if (!e.runId && selectedId !== null && e.workflowId === selectedId)
        active = false;
    }
  }
  return active;
}

/** Clears the orphans a settled run leaves behind.
 *
 * Nothing is executing any more, so any node still marked "running" is an
 * ORPHAN — a start whose finish never arrived because the run was cancelled or
 * crashed on it. Clear those; the REPORTED ok/error marks stay, as the answer
 * to "how far did it get?", until a reselect or a rerun. */
function settle(states: Record<string, NodeRunState>, active: boolean): void {
  if (active) return;
  for (const [id, state] of Object.entries(states)) {
    if (state === "running") delete states[id];
  }
}

/** A node's display name, falling back to its id when the graph is not loaded
 * (a company switch mid-overlay) — never a blank quote. */
export function nodeName(graph: WorkflowGraph | null, nodeId: string): string {
  return graph?.nodes.find((n) => n.id === nodeId)?.name ?? nodeId;
}

/** The canvas state a run starts in (issue #371): every trigger marked done.
 *
 * Only the triggers are seeded, and that mark alone is derived: the engine emits
 * no step for a trigger node — it is the thing that fired, not a thing that ran —
 * so "it fired" has to come from the graph. Every OTHER node's "running" mark is
 * now REPORTED by the host's `workflow_node_started` frame (issue #382), so this
 * no longer guesses a frontier by lighting up a trigger's successors — that guess
 * over-marked both arms of a branch until the real finishes corrected it. */
export function initialRunState(
  graph: WorkflowGraph,
): Record<string, NodeRunState> {
  const state: Record<string, NodeRunState> = {};
  for (const node of graph.nodes) {
    if (node.kind !== "trigger") continue;
    state[node.id] = "ok";
  }
  return state;
}

/** The per-node states of a run, for overlaying it on the canvas.
 *
 * A node the run never reached simply has no entry, so it renders unmarked —
 * "not reached" and not "still to come", which for a failed run is the honest
 * reading.
 *
 * Issue #1010: `running` now CAN come out of this, and only for a run the host
 * still reports as in flight. `startedNodes` is the opening bracket the history
 * fold never carried, so before this the only per-node facts a joining console
 * had were finishes — and it painted the graph with a hole exactly where the
 * work was happening. Started-minus-finished is the node executing right now.
 *
 * The `run.running` guard is load-bearing, not defensive. `startedNodes` is a
 * receipt that deliberately survives the finish (so a cancelled run still names
 * the node it was standing on), so a settled run whose last node never finished
 * would otherwise overlay a spinner that nothing can ever clear — the same
 * orphan `settle()` exists to remove from the live fold, reintroduced on the
 * one surface that has no fold to settle it. */
export function statesFromRun(
  run: WorkflowRunOutcome,
): Record<string, NodeRunState> {
  const state: Record<string, NodeRunState> = {};
  // Starts first, so a finish for the same node overwrites its "running" with
  // the reported outcome. The two lists are independently ordered — one by
  // start, one by finish — so relying on their interleaving would be a bug;
  // relying on finish-beats-start is just the bracket order the engine emits in.
  if (run.running) {
    for (const nodeId of run.startedNodes ?? []) state[nodeId] = "running";
  }
  for (const node of run.nodes ?? []) {
    state[node.nodeId] = nodeStateFrom(node.status);
  }
  return state;
}

/** Whether the frame window can fold this run on its own (issue #1010).
 *
 * The question {@link foldLiveRun} actually asks: it folds from the window only
 * when it finds a `workflow_run_started` for the run, and falls back to the
 * history seed otherwise. So "has this console watched the run live?" — which
 * a ref accumulating run ids answered, and answered forever — is the wrong
 * question. The window is a rolling 300 frames and the ref was never cleared,
 * so a run whose start had been evicted was still reported as covered, the seed
 * was withheld, and the canvas went blank on a run the operator was watching:
 * switching workflow away and back mid-run was enough to reproduce it.
 *
 * Node frames alone are deliberately NOT enough. `foldFromHistory` applies every
 * frame in the window that belongs to the run, so a seed plus surviving node
 * frames is strictly better than a seed alone — it is only the START that makes
 * the seed redundant. */
export function windowHasRunStart(
  events: CompanyStreamEvent[],
  runId: string,
): boolean {
  return events.some(
    (e) => e.type === "workflow_run_started" && e.runId === runId,
  );
}

/** Maps a reported node status onto a canvas state (issue #881).
 *
 * One place, so the live trail and a past run's overlay cannot disagree about
 * what "blocked" looks like. Fail-safe in the same direction it always was:
 * only the two words the host actually reports are trusted, and anything else
 * — including a status a newer host invents — paints as a failure rather than
 * as a success or as the gentler amber. */
function nodeStateFrom(status: string): NodeRunState {
  if (status === "ok") return "ok";
  if (status === "blocked") return "blocked";
  return "error";
}

/** Per-node durations of a past run, keyed for the canvas. */
export function elapsedFromRun(
  run: WorkflowRunOutcome,
): Record<string, number> {
  const out: Record<string, number> = {};
  for (const node of run.nodes ?? []) out[node.nodeId] = node.elapsedMs;
  return out;
}

/** Where a failed run stopped, in plain words (issue #371).
 *
 * Only ever called for a run with an `error`. A run an operator stopped
 * (issue #383) carries no error and is worded by the caller, because "where did
 * it stop" is not the interesting question about it — somebody chose the moment.
 *
 * Three genuinely different cases, and collapsing them fabricates precision:
 *
 * * a node reported `error` — the engine names it, so say it exactly;
 * * nodes ran but none errored — an **interrupted** run, whose synthetic
 *   outcome was written by the boot sweep and belongs to no node. Saying "it
 *   failed at X" here would blame a node that succeeded, and saying "before any
 *   node ran" would contradict the marks on the canvas;
 * * nothing ran at all — a graph that would not compile, or a capability that
 *   could not be built.
 */
export function failureLocation(
  run: WorkflowRunOutcome,
  graph: WorkflowGraph | null,
): string {
  const failed = failedNodeOf(run);
  if (failed) return `it failed at “${nodeName(graph, failed)}”.`;
  const ran = run.nodes?.length ?? 0;
  if (ran > 0) {
    return `it stopped after ${ran} node${ran === 1 ? "" : "s"}, without any of them reporting a failure.`;
  }
  return "it failed before any node ran.";
}

/** The node a run failed at, when its trail names one (issue #371).
 *
 * The engine reports a failing node as an `error` step before the run ends, so
 * this is exact rather than inferred. `null` when the run failed with no
 * errored node — a graph that would not compile, a capability that could not be
 * built — where naming a node would be a fabrication.
 *
 * Issue #881: a `blocked` node is explicitly NOT a failure. It stopped for a
 * person, and the run that stopped with it carries no error at all — so
 * returning it here would have the panel say "it failed at X" about a step that
 * is merely waiting. `WorkflowRunOutcome.blockedNodes` names those instead. */
export function failedNodeOf(run: WorkflowRunOutcome): string | null {
  return (
    (run.nodes ?? []).find((n) => n.status !== "ok" && n.status !== "blocked")
      ?.nodeId ?? null
  );
}

/** Lays a saved graph out left→right by longest-path depth, stacking siblings
 * vertically within each layer. Cycles are bounded by an iteration cap, so a
 * back edge never loops forever.
 *
 * `runStates` / `elapsed` (issue #371) tint each node with what the run on
 * screen did. Both default to empty, which is the resting canvas — identical to
 * how it rendered before #371.
 *
 * `undelivered` (issue #981) is the set of `output` nodes whose report did not
 * go out. Deliberately a THIRD argument rather than another `NodeRunState`: it
 * is a different subsystem's verdict, taken after the engine returned, and a
 * node in it is normally also in `runStates` as `ok` — both are true and the
 * card renders both. See {@link undeliveredNodes} in `run-health.ts`. */
/**
 * The edges that close a cycle and must be left out of the depth arithmetic, so
 * that layering — a longest-path relaxation, defined only on a DAG — terminates
 * on a graph that has loops in it.
 *
 * Exported for the layout tests, which is the only reason it is not local to
 * {@link layout}: the arithmetic in this module is deliberately pure so it can
 * be checked without a canvas, and *which* edge it decided to break is the
 * assertion that distinguishes this working from it merely not crashing.
 *
 * ## Which edge gets broken, and why it is not the traversal's choice
 *
 * Any edge of a cycle would make layering terminate; only one of them reads
 * correctly to the person who drew the graph. A revision loop should lay out as
 * a pipeline with an arrow curving back from the retry, not as a pipeline cut at
 * an arbitrary point — so the rule has to be about what the author meant, not
 * about the order a traversal happened to arrive.
 *
 * The first version of this used a DFS and broke whichever edge closed onto the
 * recursion stack. That is wrong whenever a node *before* a loop branches
 * straight into the loop's middle, because the traversal then enters the loop
 * from the wrong side. The shipped `agentic_math_lab/euler_solve` is exactly
 * that shape: `cost` fans out to both `solve` and `approach`, the DFS reaches
 * `solve` first, and it marked the ordinary forward edge `approach -> solve`
 * instead of the authored return `agree -> approach` — which laid `approach` out
 * *after* `agree` and made the normal approach-to-solve path run backwards.
 *
 * So the rule is two conditions, and neither is about traversal order:
 *
 * 1. The edge is genuinely inside a cycle — its target can reach its source.
 *    An edge that merely points at an already-visited node is not a loop.
 * 2. It points backwards in the order the author **declared** the nodes. A
 *    workflow is written in the order the work flows, so the edge that runs
 *    against that order is the return.
 *
 * Both graphs the previous rule disagreed about come out right: `euler_solve`
 * yields `agree -> approach`, and an issue-to-PR lifecycle with two revision
 * loops yields `plan_approved -> read_and_plan` and `revise_pr -> qa_review`.
 *
 * A cycle whose edges *all* point forward in declaration order — possible when
 * the nodes were not written in flow order — has no authored return to find, so
 * one of its edges is broken arbitrarily rather than none. Termination must not
 * depend on the author having been tidy.
 *
 * Reachability is recomputed per candidate edge, which is `O(E * (V + E))`.
 * That is chosen for legibility over an SCC pass: a workflow graph is tens of
 * nodes, this runs on repaint of a canvas that is already doing more work than
 * this per frame, and the condition reads as the sentence it implements.
 *
 * Back edges are excluded from the depth arithmetic **only**. They are still
 * handed to React Flow and still drawn; a loop the operator authored stays
 * visible.
 */
export function backEdges(graph: WorkflowGraph): Set<WorkflowGraph["edges"][number]> {
  const order = new Map<string, number>(graph.nodes.map((n, i) => [n.id, i]));
  const adjacency = new Map<string, string[]>(graph.nodes.map((n) => [n.id, []]));
  for (const e of graph.edges) {
    const from = adjacency.get(e.from);
    // An edge naming a node the graph does not have is dangling; it cannot be
    // part of a cycle, and the relaxation already ignores it.
    if (from && adjacency.has(e.to)) from.push(e.to);
  }

  /** Whether `to` is reachable from `from`, ignoring `skip` edges. */
  const reaches = (from: string, to: string, skip: Set<WorkflowGraph["edges"][number]>) => {
    const blocked = new Set<string>();
    for (const e of skip) blocked.add(`${e.from}\u0000${e.to}`);
    const seen = new Set([from]);
    const stack = [from];
    while (stack.length > 0) {
      const at = stack.pop()!;
      if (at === to) return true;
      for (const next of adjacency.get(at) ?? []) {
        if (blocked.has(`${at}\u0000${next}`)) continue;
        if (!seen.has(next)) {
          seen.add(next);
          stack.push(next);
        }
      }
    }
    return false;
  };

  const none = new Set<WorkflowGraph["edges"][number]>();
  // Condition 1: genuinely inside a cycle.
  const cyclic = graph.edges.filter(
    (e) => order.has(e.from) && order.has(e.to) && reaches(e.to, e.from, none),
  );
  // Condition 2: runs against the authored order. A self-loop satisfies this
  // by `>=`, which is right — it is its own return.
  const back = new Set(cyclic.filter((e) => order.get(e.from)! >= order.get(e.to)!));

  // Whatever is still cyclic once those are gone had no authored return to
  // find. Break it arbitrarily rather than leaving the relaxation to diverge.
  for (const e of cyclic) {
    if (back.has(e)) continue;
    if (reaches(e.to, e.from, back)) back.add(e);
  }

  return back;
}

export function layout(
  graph: WorkflowGraph,
  runStates: Record<string, NodeRunState> = {},
  elapsed: Record<string, number> = {},
  undelivered: Set<string> = new Set(),
): { nodes: Node<WorkflowNodeData>[]; edges: Edge[] } {
  // Layering is a longest-path relaxation, which is only defined on a DAG — so
  // the cycles come out first. A workflow with a revision loop (`plan rejected
  // -> re-plan`, `PR needs changes -> re-review`) is a correct graph the editor
  // lets you draw and the engine runs, and before this it pumped every node in
  // the cycle by +1 per pass: the `!changed` break never fired, the loop ran its
  // full `nodes.length` iterations, and the depths it left behind were the
  // iteration count rather than the graph's shape. Measured on an 11-node
  // issue-to-PR lifecycle with two revision loops: the trigger sat at x=0 and
  // every other node landed between x=9600 and x=11700, a graph 11890 units
  // wide instead of 2890. On screen that is one node alone on the left and the
  // rest off the right edge, with nothing to say the layout had given up.
  const back = backEdges(graph);
  const depth = new Map<string, number>(graph.nodes.map((n) => [n.id, 0]));
  for (let i = 0; i < graph.nodes.length; i++) {
    let changed = false;
    for (const e of graph.edges) {
      if (back.has(e)) continue;
      const d = (depth.get(e.from) ?? 0) + 1;
      if (d > (depth.get(e.to) ?? 0)) {
        depth.set(e.to, d);
        changed = true;
      }
    }
    if (!changed) break;
  }
  // A backstop, not the fix: `backEdges` already breaks every cycle, so this
  // clamp is unreachable on any graph it has seen. It stays because the failure
  // it prevents is silent and disproportionate — a future edge kind that slips
  // past the DFS would put nodes thousands of units off-screen, which reads as
  // "the canvas is broken", while a clamped graph merely stacks a column.
  const maxLayer = Math.max(0, graph.nodes.length - 1);
  for (const [id, d] of depth) {
    if (d > maxLayer) depth.set(id, maxLayer);
  }

  const rowInLayer = new Map<number, number>();
  const nodes: Node<WorkflowNodeData>[] = graph.nodes.map((n) => {
    const layer = depth.get(n.id) ?? 0;
    const row = rowInLayer.get(layer) ?? 0;
    rowInLayer.set(layer, row + 1);
    const meta = nodeKindMeta(n.kind);
    return {
      id: n.id,
      type: "oc",
      // See NODE_W/NODE_H: a size hint the minimap can read off the node we
      // hand over, since this array is rebuilt on every repaint and React
      // Flow's own measurement does not survive that.
      initialWidth: NODE_W,
      initialHeight: NODE_H,
      position: { x: layer * COL_GAP, y: row * ROW_GAP },
      data: {
        kind: n.kind,
        name: n.name,
        // Agent nodes surface their roster id; otherwise the node's summary.
        summary: n.summary ?? (n.agent ? `Teammate: ${n.agent}` : ""),
        emoji: meta.emoji,
        color: meta.color,
        runState: runStates[n.id],
        elapsedMs: elapsed[n.id],
        // `undefined` rather than `false` when it delivered fine, so a resting
        // node's data is byte-for-byte what it was before this existed.
        reportUndelivered: undelivered.has(n.id) || undefined,
      },
    };
  });

  const edges: Edge[] = graph.edges.map((e, i) => ({
    id: `${e.from}-${e.to}-${i}`,
    source: e.from,
    target: e.to,
    label: e.label,
    animated: true,
  }));

  return { nodes, edges };
}

/**
 * The zoom the canvas will not fit BELOW on load (issue #1361).
 *
 * Not a floor on zooming — that is `<ReactFlow minZoom>`, which issue #1261
 * deliberately set to 0.1 and which this must not touch. This is a floor on the
 * **initial fit**, and the two are different decisions: an operator who chooses
 * to zoom out to 0.1 is looking at shape, while a canvas that *opens* below
 * legibility has made that choice for them.
 *
 * WHY THERE HAS TO BE ONE. `layout` above puts one node per depth layer at
 * `COL_GAP`, so a ten-node pipeline — `feature_pipeline`, shipped in every
 * company — is 2890 x 64 graph units. Against a ~1224 x 782 canvas that is an
 * aspect ratio of 45:1 fitting into 1.6:1, and `fitView` resolves it the only
 * way it can: by zooming out until the width fits. Measured before this
 * existed: **0.353** at a 1440px window, **0.28** at 1100px.
 *
 * WHERE THE NUMBER COMES FROM. A node's title is `text-sm` — 14px, and the one
 * line that says what the step does. The console's own type scale bottoms out
 * at `text-3xs` = 10px (`src/index.css`), which is the smallest size this
 * design system is willing to render words at. A node title must not be
 * rendered smaller than that:
 *
 *     14px x 0.75 = 10.5px  >=  the 10px floor
 *
 * so 0.75. At that zoom a ten-node pipeline is 2168px wide and about six of its
 * nodes are on screen at once; the operator pans for the rest, which is what
 * the minimap (#1259) and every comparable canvas expect. A four-node workflow
 * still fits whole, because its natural fit is already above this and nothing
 * here applies.
 */
export const LEGIBLE_FIT_ZOOM = 0.75;

/** The gap between the canvas's left edge and the first node, when the fit was
 * clamped and the graph runs off the right. A fixed gutter rather than a
 * fraction: it is a margin around a thing, not a share of a space. */
const START_GUTTER = 32;

/**
 * The x React Flow's own fit produces for `bounds` at `zoom` — the centred
 * placement `getViewportForBounds` computes as `width / 2 - centerX * zoom`.
 *
 * Reproduced here so a caller can recognise that placement when it sees it.
 * That recognition is the whole trigger for the correction below: "the viewport
 * is sitting at the fit's floor" is not enough on its own, because an operator
 * who zooms in and back out lands on exactly that zoom too, and a canvas that
 * jumped to the start whenever they did would be worse than the bug. "At the
 * floor AND placed exactly where the fit would place it" is a state only the
 * fit produces.
 */
export function centredFitX(
  bounds: { minX: number; width: number },
  paneWidth: number,
  zoom: number,
): number {
  const centreX = bounds.minX + bounds.width / 2;
  return paneWidth / 2 - centreX * zoom;
}

/**
 * Where to put the viewport when {@link LEGIBLE_FIT_ZOOM} stops `fitView` from
 * shrinking the graph any further — or `null` when it does not, which is the
 * common case and means "leave React Flow's own fit alone".
 *
 * The correction is needed because `getViewportForBounds` **centres** the
 * bounds it was given. That is right when everything fits and wrong the moment
 * it does not: centring a graph too wide for the pane hides its first node and
 * its last one, and drops the operator into the middle of a pipeline with no
 * indication that either end exists. A pipeline is read from its trigger, so a
 * clamped fit anchors there and lets the graph run off to the right, where the
 * arrows already point.
 *
 * Vertically the content stays centred: the clamp is a width problem, and a
 * graph with sibling rows is no taller than the pane at this zoom.
 *
 * Pure, and given the pane's size rather than reading it, so the arithmetic can
 * be tested without a canvas.
 */
export function startAnchoredFit(
  bounds: { minX: number; minY: number; width: number; height: number },
  paneWidth: number,
  paneHeight: number,
): { x: number; y: number; zoom: number } | null {
  if (bounds.width <= 0 || bounds.height <= 0) return null;
  if (paneWidth <= 0 || paneHeight <= 0) return null;

  const usableWidth = paneWidth - START_GUTTER * 2;
  const usableHeight = paneHeight - START_GUTTER * 2;
  if (usableWidth <= 0 || usableHeight <= 0) return null;

  // The zoom `fitView` would have chosen, unclamped. Anything at or above the
  // floor is a fit that already shows the whole graph legibly, and this must
  // not second-guess it.
  const naturalZoom = Math.min(
    usableWidth / bounds.width,
    usableHeight / bounds.height,
  );
  if (naturalZoom >= LEGIBLE_FIT_ZOOM) return null;

  const zoom = LEGIBLE_FIT_ZOOM;
  return {
    x: START_GUTTER - bounds.minX * zoom,
    y: (paneHeight - bounds.height * zoom) / 2 - bounds.minY * zoom,
    zoom,
  };
}
