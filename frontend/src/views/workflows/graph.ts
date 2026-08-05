// Pure graph + run-state helpers for the Workflows surface.
//
// Extracted verbatim from `WorkflowsView.tsx` (issue #303) when that file passed
// 1800 lines and was about to grow an index and a copilot. Nothing here touches
// React: it is layout arithmetic and folds over run data, which is exactly the
// part worth testing and reusing on its own.

import type { Edge, Node } from "@xyflow/react";

import type {
  WorkflowGraph,
  WorkflowRunOutcome,
} from "@/api/workflows";
import type { CompanyStreamEvent } from "@/hooks/use-events";
import { nodeKindMeta, type NodeRunState, type WorkflowNodeData } from "@/lib/workflow-sample";

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
 * incoherent picture. */
export function foldLiveRun(
  events: CompanyStreamEvent[],
  selectedId: string | null,
  graph: WorkflowGraph | null,
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
  if (startIndex === -1) return null;
  const started = events[startIndex];
  if (started.type !== "workflow_run_started") return null;

  const runId = started.runId;
  // The trigger fired by definition, and the engine reports no step for it, so
  // nothing else would ever mark it. Its successors are where execution is now.
  const states = initialRunState(graph);
  const elapsed: Record<string, number> = {};
  let active = true;

  for (let i = startIndex + 1; i < events.length; i++) {
    const e = events[i];
    if (e.type === "workflow_node_finished") {
      if (e.runId !== runId) continue;
      // Anything that is not "ok" is treated as a failure: an unknown status
      // word from a newer host must never paint a node as succeeded.
      const state: NodeRunState = e.status === "ok" ? "ok" : "error";
      states[e.nodeId] = state;
      elapsed[e.nodeId] = e.elapsedMs;
      // Advance the frontier. Only a successful node hands execution on — a
      // failed one under the default `stop` policy ends the run, and lighting
      // up its successors would claim work that never happened.
      if (state === "ok") {
        for (const id of successorsOf(graph, e.nodeId)) {
          if (!states[id]) states[id] = "running";
        }
      }
      continue;
    }
    if (e.type === "workflow_run_finished" && e.workflowId === selectedId) {
      // A pre-#371 host sends no runId; treat that as "the run on screen"
      // rather than ignoring it, else the canvas would spin forever.
      if (!e.runId || e.runId === runId) active = false;
    }
  }

  if (!active) {
    // Nothing is executing any more, so the derived marks go. The REPORTED
    // ok/error ones stay — they are the answer to "how far did it get?" — until
    // a reselect or a rerun.
    for (const [id, state] of Object.entries(states)) {
      if (state === "running") delete states[id];
    }
  }

  return { runId, states, elapsed, active };
}

/** A node's display name, falling back to its id when the graph is not loaded
 * (a company switch mid-overlay) — never a blank quote. */
export function nodeName(graph: WorkflowGraph | null, nodeId: string): string {
  return graph?.nodes.find((n) => n.id === nodeId)?.name ?? nodeId;
}

/** The node ids `from` hands execution to. */
export function successorsOf(graph: WorkflowGraph, from: string): string[] {
  return graph.edges.filter((e) => e.from === from).map((e) => e.to);
}

/** The canvas state a run starts in (issue #371): every trigger marked done,
 * its successors marked running.
 *
 * Both halves are derived rather than reported. The engine emits no step for a
 * trigger node — it is the thing that fired, not a thing that ran — and it has
 * no `on_step_start` hook at all, so "where is it now" has to come from the
 * graph. After a branch point this briefly marks more than one arm; that
 * corrects itself as the real finishes arrive. */
export function initialRunState(graph: WorkflowGraph): Record<string, NodeRunState> {
  const state: Record<string, NodeRunState> = {};
  for (const node of graph.nodes) {
    if (node.kind !== "trigger") continue;
    state[node.id] = "ok";
    for (const id of successorsOf(graph, node.id)) state[id] = "running";
  }
  return state;
}

/** The per-node states of a PAST run, for overlaying it on the canvas.
 *
 * No `running` ever comes out of this: the run is over. A node the run never
 * reached simply has no entry, so it renders unmarked — "not reached" and not
 * "still to come", which for a failed run is the honest reading. */
export function statesFromRun(run: WorkflowRunOutcome): Record<string, NodeRunState> {
  const state: Record<string, NodeRunState> = {};
  for (const node of run.nodes ?? []) {
    state[node.nodeId] = node.status === "ok" ? "ok" : "error";
  }
  return state;
}

/** Per-node durations of a past run, keyed for the canvas. */
export function elapsedFromRun(run: WorkflowRunOutcome): Record<string, number> {
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
export function failureLocation(run: WorkflowRunOutcome, graph: WorkflowGraph | null): string {
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
 * built — where naming a node would be a fabrication. */
export function failedNodeOf(run: WorkflowRunOutcome): string | null {
  return (run.nodes ?? []).find((n) => n.status !== "ok")?.nodeId ?? null;
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
