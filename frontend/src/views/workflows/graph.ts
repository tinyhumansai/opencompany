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
 * how it rendered before #371. */
export function layout(
  graph: WorkflowGraph,
  runStates: Record<string, NodeRunState> = {},
  elapsed: Record<string, number> = {},
): { nodes: Node<WorkflowNodeData>[]; edges: Edge[] } {
  const depth = new Map<string, number>(graph.nodes.map((n) => [n.id, 0]));
  for (let i = 0; i < graph.nodes.length; i++) {
    let changed = false;
    for (const e of graph.edges) {
      const d = (depth.get(e.from) ?? 0) + 1;
      if (d > (depth.get(e.to) ?? 0)) {
        depth.set(e.to, d);
        changed = true;
      }
    }
    if (!changed) break;
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
      position: { x: layer * COL_GAP, y: row * ROW_GAP },
      data: {
        kind: n.kind,
        name: n.name,
        // Agent nodes surface their roster id; otherwise the node's summary.
        summary: n.summary ?? (n.agent ? `Agent: ${n.agent}` : ""),
        emoji: meta.emoji,
        color: meta.color,
        runState: runStates[n.id],
        elapsedMs: elapsed[n.id],
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
