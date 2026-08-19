// Issue #415: the copilot proposes an edit the operator reviews and applies.
//
// The read-only copilot (#303) could describe a change; turning that description
// into canvas edits was the operator's job, by hand. This module is the round
// trip: a reply may carry a **proposal**, the console turns it into a candidate
// graph, shows the operator what would change, and applies it through the same
// `updateWorkflow` call the editor uses.
//
// ## The unit of a proposal is an operation list, not a replacement graph
//
// A whole replacement graph is trivial to apply and hostile to review: the
// operator has to diff two documents to find out what the model actually
// changed, and a model that silently drops a node it did not mention gets away
// with it. So a proposal is a short list of node/edge operations, each naming
// its target. That is what makes the review honest — the diff shown to the
// operator is derived from the ops, so a change nobody proposed cannot appear in
// it, and a proposal that touches something it never mentioned cannot exist.
//
// ## Validation here is a courtesy; the host is the authority
//
// Everything below is *shape* checking: the JSON parses, the ops are known, and
// they refer to nodes that exist. It exists so an obviously malformed proposal
// is refused with a stated reason instead of being sent to the host to fail,
// and so the operator is never shown a diff the console could not compute. It
// is deliberately NOT the graph's rulebook: applying goes through
// `updateWorkflow`, which is the same validated, versioned write the canvas
// editor performs, so a proposal can never produce a graph the editor could not
// have produced. If the two disagree, the host wins and the operator sees why.
//
// ## Staleness is refused, never rebased
//
// A proposal is reasoned about ONE version of the graph. The version is stamped
// by the console when the reply is parsed — never read from the model, which
// has no business asserting what it was looking at — and apply refuses if the
// graph has moved since. Two layers, and both are load-bearing: this one gives
// the operator a sentence they can act on, and `expectedVersion` on the write
// makes the host refuse a race this check cannot see.

import { nodeKindConfigProblem } from "@/lib/workflow-node-config";
import { WORKFLOW_NODE_KINDS } from "./workflows";
import type { WorkflowEdge, WorkflowGraph, WorkflowNode } from "./workflows";

/**
 * The fence a proposal arrives in. The composed copilot message asks for
 * exactly this, and nothing else in a reply is parsed as a proposal — prose
 * that merely describes a change stays prose.
 */
export const PROPOSAL_FENCE = "workflow-proposal";

/** One reviewable change to the graph. */
export type ProposalOp =
  | { op: "addNode"; node: WorkflowNode }
  /** Field-level, so the diff can say which fields moved rather than "changed". */
  | { op: "updateNode"; id: string; set: Partial<WorkflowNode> }
  | { op: "removeNode"; id: string }
  | { op: "addEdge"; from: string; to: string; label?: string }
  | { op: "removeEdge"; from: string; to: string };

/** A parsed, shape-checked proposal, bound to the graph it was reasoned about. */
export interface WorkflowProposal {
  /** The model's one-line description of the change, for the card's header. */
  summary: string;
  ops: ProposalOp[];
  /**
   * The `version` of the graph this was proposed against, stamped by the
   * console at parse time.
   *
   * `undefined` when the graph carried none — a host predating the version
   * token (#259). Apply still goes through, unconditionally, exactly as a
   * canvas edit does on such a host: the protection does not exist there, and
   * pretending otherwise by refusing every proposal would be a different lie.
   */
  basedOnVersion?: string;
}

/** Why a reply's proposal block could not be used. */
export interface ProposalProblem {
  reason: string;
}

/** What {@link readProposal} found in one reply. */
export interface ReadProposal {
  /** The reply with the proposal block removed, so the card is not shown twice. */
  prose: string;
  proposal?: WorkflowProposal;
  /** Present when a block was found but could not be used. */
  problem?: ProposalProblem;
}

const FENCE_RE = new RegExp(
  "```" + PROPOSAL_FENCE + "\\s*\\n([\\s\\S]*?)\\n?```",
  // First block only: a reply carrying two proposals is one the operator cannot
  // review as a unit, so the first is read and the rest stays visible as prose.
  "",
);

/**
 * Splits a copilot reply into prose and (at most) one proposal.
 *
 * A reply with no block is returned unchanged with no proposal — the ordinary
 * case, and the whole read-only surface still works exactly as it did.
 */
export function readProposal(reply: string, graph: WorkflowGraph): ReadProposal {
  const match = FENCE_RE.exec(reply);
  if (!match) return { prose: reply };

  const prose = (reply.slice(0, match.index) + reply.slice(match.index + match[0].length)).trim();
  let parsed: unknown;
  try {
    parsed = JSON.parse(match[1]);
  } catch {
    return { prose, problem: { reason: "The proposed change wasn't valid JSON." } };
  }

  const result = validateProposal(parsed, graph);
  if ("reason" in result) return { prose, problem: result };
  // `version` is now `string | null` (issue #1013); `basedOnVersion` is the
  // token this proposal was based on, so a non-editable graph's `null` means "no
  // base" — coerce it to `undefined`.
  return {
    prose,
    proposal: { ...result, basedOnVersion: graph.version ?? undefined },
  };
}

/** The known node fields an `updateNode` may set. */
const UPDATABLE: readonly (keyof WorkflowNode)[] = [
  "kind",
  "name",
  "summary",
  "agent",
  "schedule",
  "config",
  "onError",
  "retry",
  "requiresApproval",
  "destination",
];

/**
 * Shape-checks a parsed block against the graph, returning either the proposal
 * or the one sentence the operator is shown instead.
 *
 * Exported for the tests, which are the reason each refusal names what was
 * wrong rather than reporting "invalid proposal": a copilot that says "I
 * proposed something but it was invalid" is a dead end, and the reason is what
 * lets the operator ask again usefully.
 */
export function validateProposal(
  parsed: unknown,
  graph: WorkflowGraph,
): Omit<WorkflowProposal, "basedOnVersion"> | ProposalProblem {
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    return { reason: "The proposed change wasn't an object." };
  }
  const body = parsed as { summary?: unknown; ops?: unknown };
  const summary = typeof body.summary === "string" ? body.summary.trim() : "";
  if (!summary) return { reason: "The proposed change didn't say what it does." };
  if (!Array.isArray(body.ops) || body.ops.length === 0) {
    return { reason: "The proposed change listed no edits." };
  }

  // Tracked as the ops are read, so an edge may reference a node the same
  // proposal adds, and may not reference one the same proposal removes.
  const ids = new Set(graph.nodes.map((n) => n.id));
  // The nodes as they stand, so an `updateNode`'s kind↔config coherence can be
  // judged on the node it WOULD produce (existing merged with `set`), not on the
  // change in isolation — a kind-switch that leaves the node without the config
  // its new kind needs is exactly the "wrong kind when applied" the host rejects
  // on write, and this catches it before the operator is shown a diff for it.
  const byId = new Map(graph.nodes.map((n) => [n.id, n]));
  const edges = new Set(graph.edges.map(edgeKey));
  const ops: ProposalOp[] = [];

  for (const raw of body.ops) {
    if (!raw || typeof raw !== "object" || Array.isArray(raw)) {
      return { reason: "One of the proposed edits wasn't an object." };
    }
    const op = raw as Record<string, unknown>;
    switch (op.op) {
      case "addNode": {
        const node = op.node;
        if (!node || typeof node !== "object" || Array.isArray(node)) {
          return { reason: "A proposed new step carried no node." };
        }
        const candidate = node as Record<string, unknown>;
        const id = typeof candidate.id === "string" ? candidate.id.trim() : "";
        const kind = typeof candidate.kind === "string" ? candidate.kind.trim() : "";
        const name = typeof candidate.name === "string" ? candidate.name.trim() : "";
        if (!id || !kind || !name) {
          return { reason: "A proposed new step is missing its id, kind or name." };
        }
        if (ids.has(id)) {
          return { reason: `A proposed new step reuses the existing id \`${id}\`.` };
        }
        // Refused rather than dropped. A key this console does not know is a
        // key the review card cannot render — an added step's diff line shows
        // its id and name — so silently keeping it would put something in the
        // candidate graph the operator never saw, and silently discarding it
        // would apply a step that is not the one the copilot described. Both
        // break the same guarantee: the diff is what would happen.
        const stray = Object.keys(candidate).find(
          (field) => !["id", "kind", "name"].includes(field) && !isNodeField(field),
        );
        if (stray) {
          return {
            reason: `A proposed new step sets \`${stray}\`, which isn't a step field.`,
          };
        }
        // A kind the host would reject on write (`WORKFLOW_NODE_KINDS` in
        // `src/company/workflow_file.rs`) is refused here, with the same actionable
        // shape as an unknown field: offering a diff for a step Apply cannot write
        // is the ungrounded-proposal failure issue #783 exists to close.
        if (!WORKFLOW_NODE_KINDS.includes(kind)) {
          return {
            reason: `A proposed new step \`${id}\` has an unknown kind \`${kind}\` — use one of: ${WORKFLOW_NODE_KINDS.join(", ")}.`,
          };
        }
        const built: WorkflowNode = { ...(candidate as unknown as WorkflowNode), id, kind, name };
        // The console mirror of the host's kind↔config rules: a `tool_call` with
        // no `config.slug`, an `agent` naming no teammate, and the rest. Refused
        // with the host's own actionable sentence rather than sent on to fail.
        const coherence = nodeKindConfigProblem(built);
        if (coherence) return { reason: coherence };
        ids.add(id);
        // Kept in step with `ids` so a later op in the same proposal judges
        // against the graph as these edits build it, not the original — an
        // `updateNode` on a node this proposal just added must see it.
        byId.set(id, built);
        ops.push({ op: "addNode", node: built });
        break;
      }
      case "updateNode": {
        const id = typeof op.id === "string" ? op.id.trim() : "";
        if (!ids.has(id)) {
          return { reason: `A proposed change targets \`${id || "?"}\`, which isn't in this workflow.` };
        }
        const set = op.set;
        if (!set || typeof set !== "object" || Array.isArray(set)) {
          return { reason: `The proposed change to \`${id}\` set no fields.` };
        }
        const fields = Object.keys(set as object);
        if (fields.length === 0) {
          return { reason: `The proposed change to \`${id}\` set no fields.` };
        }
        // `id` is deliberately not updatable: an id keys the node's edges, so a
        // rename through the side door would silently orphan them. Renaming is
        // an add plus a remove, which the diff then shows as what it is.
        const unknown = fields.find((f) => !UPDATABLE.includes(f as keyof WorkflowNode));
        if (unknown) {
          return { reason: `The proposed change to \`${id}\` sets \`${unknown}\`, which isn't a step field.` };
        }
        // A `kind` that is present but not a string slips past the string check
        // below (`typeof … === "string"` is false), passes `nodeKindConfigProblem`
        // on a node whose kind is `null`/an object, and applies a graph the host
        // rejects on write. Reject it here with the same actionable shape.
        if ("kind" in (set as object)) {
          const proposedKind = (set as Record<string, unknown>).kind;
          if (typeof proposedKind !== "string" || proposedKind.trim() === "") {
            return {
              reason: `The proposed change to \`${id}\` sets an invalid kind — it must name a node kind, one of: ${WORKFLOW_NODE_KINDS.join(", ")}.`,
            };
          }
        }
        // Judge the node this update WOULD produce, the way `applyProposal` will
        // build it — existing merged with `set`. A change that switches a node's
        // kind, or replaces its `config`, can leave it incoherent for its kind;
        // the host rejects that on write, so it is caught here before the diff.
        const existing = byId.get(id) as WorkflowNode;
        const merged = { ...existing, ...(set as Partial<WorkflowNode>) };
        if (typeof merged.kind === "string" && !WORKFLOW_NODE_KINDS.includes(merged.kind)) {
          return {
            reason: `The proposed change to \`${id}\` sets an unknown kind \`${merged.kind}\` — use one of: ${WORKFLOW_NODE_KINDS.join(", ")}.`,
          };
        }
        const updated = nodeKindConfigProblem(merged);
        if (updated) return { reason: updated };
        // Kept in step so a further op in the same proposal sees this change.
        byId.set(id, merged as WorkflowNode);
        ops.push({ op: "updateNode", id, set: set as Partial<WorkflowNode> });
        break;
      }
      case "removeNode": {
        const id = typeof op.id === "string" ? op.id.trim() : "";
        if (!ids.has(id)) {
          return { reason: `A proposed removal targets \`${id || "?"}\`, which isn't in this workflow.` };
        }
        ids.delete(id);
        byId.delete(id);
        ops.push({ op: "removeNode", id });
        break;
      }
      case "addEdge": {
        const from = typeof op.from === "string" ? op.from.trim() : "";
        const to = typeof op.to === "string" ? op.to.trim() : "";
        if (!ids.has(from) || !ids.has(to)) {
          return {
            reason: `A proposed connection joins \`${from || "?"}\` to \`${to || "?"}\`, and one of those steps isn't there.`,
          };
        }
        // A connection that already exists would be appended a second time and
        // then diff to NOTHING — the diff compares sets keyed by `from`/`to` —
        // so the operator would apply a duplicated edge the review never
        // showed. Refused for the same reason `removeEdge` refuses a
        // connection that is not there: the graph, not the request, decides
        // whether an op means anything.
        if (edges.has(edgeKey({ from, to }))) {
          return {
            reason: `A proposed connection joins \`${from}\` to \`${to}\`, which is already connected.`,
          };
        }
        const label = typeof op.label === "string" && op.label.trim() ? op.label.trim() : undefined;
        edges.add(edgeKey({ from, to }));
        ops.push({ op: "addEdge", from, to, label });
        break;
      }
      case "removeEdge": {
        const from = typeof op.from === "string" ? op.from.trim() : "";
        const to = typeof op.to === "string" ? op.to.trim() : "";
        if (!edges.has(edgeKey({ from, to }))) {
          return {
            reason: `A proposed disconnection names \`${from || "?"}\` to \`${to || "?"}\`, which isn't a connection in this workflow.`,
          };
        }
        edges.delete(edgeKey({ from, to }));
        ops.push({ op: "removeEdge", from, to });
        break;
      }
      default:
        return { reason: `The proposal used an edit this console doesn't know: \`${String(op.op)}\`.` };
    }
  }

  return { summary, ops };
}

/**
 * The graph a proposal would produce.
 *
 * **Pure.** It never touches the graph it was given, which is what makes the
 * preview safe: the operator is looking at a candidate, and rejecting one
 * changes nothing because there was nothing to change back.
 */
export function applyProposal(graph: WorkflowGraph, proposal: WorkflowProposal): WorkflowGraph {
  let nodes = graph.nodes.map((n) => ({ ...n }));
  let edges = graph.edges.map((e) => ({ ...e }));

  for (const op of proposal.ops) {
    switch (op.op) {
      case "addNode":
        nodes = [...nodes, { ...op.node }];
        break;
      case "updateNode":
        nodes = nodes.map((n) => (n.id === op.id ? { ...n, ...op.set } : n));
        break;
      case "removeNode":
        nodes = nodes.filter((n) => n.id !== op.id);
        // A removed step takes its connections with it. Leaving them would
        // produce a graph naming a node that is gone, which the host would
        // refuse — and the operator would be told the proposal was invalid
        // rather than that it removed a step.
        edges = edges.filter((e) => e.from !== op.id && e.to !== op.id);
        break;
      case "addEdge":
        edges = [...edges, { from: op.from, to: op.to, label: op.label }];
        break;
      case "removeEdge":
        edges = edges.filter((e) => !(e.from === op.from && e.to === op.to));
        break;
    }
  }

  return { ...graph, nodes, edges };
}

/** One step's place in the diff. */
export interface NodeChange {
  id: string;
  name: string;
  change: "added" | "removed" | "changed";
  /** For a `changed` step: the fields that moved, `before` → `after`. */
  fields: { field: string; before: string; after: string }[];
}

/** One connection's place in the diff. */
export interface EdgeChange {
  from: string;
  to: string;
  change: "added" | "removed";
}

/** What the operator is shown before applying. */
export interface GraphDiff {
  nodes: NodeChange[];
  edges: EdgeChange[];
}

/**
 * The change between the saved graph and a candidate, as the review card renders
 * it.
 *
 * Computed from the two graphs rather than from the op list on purpose: the ops
 * say what was *asked for*, and this says what would actually happen. An op that
 * sets a field to the value it already holds shows up as no change, which is the
 * honest answer and stops a proposal from looking bigger than it is.
 */
export function diffGraphs(current: WorkflowGraph, candidate: WorkflowGraph): GraphDiff {
  const before = new Map(current.nodes.map((n) => [n.id, n]));
  const after = new Map(candidate.nodes.map((n) => [n.id, n]));
  const nodes: NodeChange[] = [];

  for (const [id, node] of after) {
    const was = before.get(id);
    if (!was) {
      // Every field the new step carries, not just its id and name. A proposed
      // step arrives with a `kind`, and often a `config`, a `schedule` or a
      // `requiresApproval` — all of which change what the workflow DOES. An
      // added row that showed only a name would let an operator apply a
      // scheduled, auto-approving node believing they had reviewed it, which is
      // the same guarantee this module makes about changed steps and had not
      // been keeping about new ones.
      nodes.push({ id, name: node.name, change: "added", fields: addedFields(node) });
      continue;
    }
    const fields = changedFields(was, node);
    if (fields.length > 0) {
      nodes.push({ id, name: node.name, change: "changed", fields });
    }
  }
  for (const [id, node] of before) {
    if (!after.has(id)) nodes.push({ id, name: node.name, change: "removed", fields: [] });
  }

  const beforeEdges = new Set(current.edges.map(edgeKey));
  const afterEdges = new Set(candidate.edges.map(edgeKey));
  const edges: EdgeChange[] = [];
  for (const edge of candidate.edges) {
    if (!beforeEdges.has(edgeKey(edge))) {
      edges.push({ from: edge.from, to: edge.to, change: "added" });
    }
  }
  for (const edge of current.edges) {
    if (!afterEdges.has(edgeKey(edge))) {
      edges.push({ from: edge.from, to: edge.to, change: "removed" });
    }
  }

  return { nodes, edges };
}

/** Whether a diff would change anything at all. */
export function isEmptyDiff(diff: GraphDiff): boolean {
  return diff.nodes.length === 0 && diff.edges.length === 0;
}

/**
 * Whether a proposal may still be applied to `graph`.
 *
 * `stale` covers both halves of the issue's "must be refused or rebased, never
 * silently applied": a graph that moved under a live proposal, and a proposal
 * replayed from the journal on a later visit, whose `basedOnVersion` was never
 * ours to know.
 */
export function proposalIsStale(proposal: WorkflowProposal, graph: WorkflowGraph): boolean {
  // `null` (issue #1013, a current host's honest "no token" for a non-editable
  // graph) and `undefined` (a host predating #259 entirely) both mean the same
  // thing here: nothing to compare against, so nothing to call stale.
  if (graph.version == null) return false;
  return proposal.basedOnVersion !== graph.version;
}

function edgeKey(edge: Pick<WorkflowEdge, "from" | "to">): string {
  // JSON, not `${from} ${to}`: a space delimiter collides — `("a b", "c")` and
  // `("a", "b c")` both key as `a b c`. The host does not forbid a space in a
  // node id, and the cost of the collision is not cosmetic: two different edges
  // would compare equal, so validation could refuse a genuinely new connection
  // and `diffGraphs` could hide a real one from the review.
  return JSON.stringify([edge.from, edge.to]);
}

/** Whether `field` is a step field this console can render in a diff. */
function isNodeField(field: string): boolean {
  return (UPDATABLE as readonly string[]).includes(field);
}

/**
 * Everything a NEW step carries, rendered as diff lines against "not set".
 *
 * `name` is omitted because the row's own header already shows it; every other
 * persisted field is listed, so what the operator reviews is the whole node the
 * apply would write.
 */
function addedFields(node: WorkflowNode): { field: string; before: string; after: string }[] {
  const out: { field: string; before: string; after: string }[] = [];
  for (const field of UPDATABLE) {
    if (field === "name") continue;
    const value = node[field];
    if (value === undefined || value === null) continue;
    out.push({ field, before: "not set", after: render(value) });
  }
  return out;
}

/** The fields that differ between two versions of one step, rendered for review. */
function changedFields(
  before: WorkflowNode,
  after: WorkflowNode,
): { field: string; before: string; after: string }[] {
  const out: { field: string; before: string; after: string }[] = [];
  for (const field of UPDATABLE) {
    const was = render(before[field]);
    const now = render(after[field]);
    if (was !== now) out.push({ field, before: was, after: now });
  }
  return out;
}

/** A field value as one short reviewable string. */
function render(value: unknown): string {
  if (value === undefined || value === null) return "not set";
  if (typeof value === "string") return value;
  return JSON.stringify(value);
}
