// Issue #580: turn a card's STORED workflow proposal into the same reviewable
// diff the copilot's proposal card renders (#415).
//
// The builder pass (`src/harness/workflow_build.rs`) settles a `workflow`-card
// to In Review carrying a `TaskWorkflowProposal` whose `ops` is NOT a
// `ProposalOp[]` — it is the camelCase authoring graph the host rebuilds from,
// a `WorkflowGraphSpec` `{ id, name, description?, nodes[], edges[] }` (see
// `src/company/workflow_create.rs`). This module converts that spec into the
// add-only op list #415 already knows how to validate and diff, then runs it
// through the exact same pure functions — so the operator reviews a proposed
// workflow with the machinery that already exists rather than a second one.
//
// ## Everything renders "added"
//
// A built proposal is a brand-new graph, so it is diffed against the empty
// graph: every step and every connection is an addition, with each step's whole
// payload (`kind`, `schedule`, `requiresApproval`, `destination`, …) shown — the
// same guarantee #415 makes about a proposed new node, and for the same reason:
// a scheduled auto-approving node must not reach Apply off a row that showed
// only a name.
//
// ## It refuses rather than crashes
//
// `ops` is opaque on the client and comes from a model-authored graph, so this
// is defensive: a spec that is not an object, lists no steps, reuses a step id,
// connects to a step that is not there, or carries a field the diff cannot
// render comes back as a stated `reason` (the panel shows it and withholds
// Apply), never as a thrown error. The refusals are `validateProposal`'s own —
// the host stays the authority on whether Apply actually succeeds.

import {
  applyProposal,
  diffGraphs,
  validateProposal,
  type GraphDiff,
} from "@/api/workflow-proposal";
import type { WorkflowGraph } from "@/api/workflows";

/** The graph a built proposal is diffed against: nothing, so all of it is new. */
const EMPTY_GRAPH: WorkflowGraph = {
  id: "",
  name: "",
  nodes: [],
  edges: [],
  // No saved body, so no token (issue #1013 makes `version` explicit).
  version: null,
};

/** Either the all-added diff, or the one sentence to show instead of Apply. */
export type TaskProposalDiff = { diff: GraphDiff } | { reason: string };

/**
 * Convert a stored `TaskWorkflowProposal.ops` (a `WorkflowGraphSpec`) into the
 * reviewable diff, or a reason it cannot be shown.
 *
 * The spec's `nodes` become `addNode` ops and its `edges` become `addEdge` ops,
 * in that order so an edge may reference a node the same spec adds. The
 * assembled op list is handed to {@link validateProposal} against the empty
 * graph — which is the single place that decides a duplicate id, an edge to an
 * absent step, or an unrenderable field is a refusal — and, if it passes,
 * applied and diffed by the same pure functions the copilot uses.
 */
export function taskProposalDiff(ops: unknown): TaskProposalDiff {
  if (!ops || typeof ops !== "object" || Array.isArray(ops)) {
    return { reason: "This proposal's workflow could not be read." };
  }
  const spec = ops as { nodes?: unknown; edges?: unknown; summary?: unknown };
  const nodes = Array.isArray(spec.nodes) ? spec.nodes : [];
  const edges = Array.isArray(spec.edges) ? spec.edges : [];

  // The shape `validateProposal` reads: a non-empty summary and an op list. The
  // summary is a placeholder — the panel leads with the proposal's own summary,
  // not this — it exists only to satisfy the shape check, which is right to
  // refuse a summary-less copilot proposal but has no bearing on a built one.
  const candidate = {
    summary: "workflow",
    ops: [
      ...nodes.map((node) => ({ op: "addNode", node })),
      ...edges.map((edge) => ({
        op: "addEdge",
        ...(edge && typeof edge === "object" && !Array.isArray(edge) ? edge : {}),
      })),
    ],
  };

  const validated = validateProposal(candidate, EMPTY_GRAPH);
  if ("reason" in validated) return { reason: validated.reason };

  const proposal = {
    ...validated,
    basedOnVersion: EMPTY_GRAPH.version ?? undefined,
  };
  return { diff: diffGraphs(EMPTY_GRAPH, applyProposal(EMPTY_GRAPH, proposal)) };
}
