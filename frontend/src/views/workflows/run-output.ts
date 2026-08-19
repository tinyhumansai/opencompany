// Defensive parsing of a workflow run's engine output into readable per-node
// results (issue #596).
//
// Extracted from `RunResultPanel.tsx` so BOTH the live run drawer and the
// durable run inspector (a past run reopened from History, read through
// `workflowRunOutput`) share one parse — and so the pre-publish approvals card
// can render the same upstream content. A live run and a past run carry the
// identical `{ nodes: { "<id>": { items: [...] } } }` shape (the durable record
// persists exactly the engine's `output["nodes"]`), so one parser serves all
// three surfaces.
//
// Pure logic only (no JSX): callers render the returned `NodeResult` / message
// list however they like (markdown, raw JSON behind a toggle).

import type { WorkflowGraph } from "@/api/workflows";

/** A single agent reply extracted from a node's `items[].json`. */
export interface NodeMessage {
  text: string | null;
  agentRef: string | null;
}

/**
 * The state of a single node's output as the inspector renders it (issue #596).
 *
 * A live run's clicked node is `present` immediately (from the in-memory
 * result); a past run's node is `loading` while its durable snapshot is fetched,
 * then `present` or `unavailable`. `unavailable` covers a 404 (the run predates
 * capture, was a dry run, or was hard-aborted) and a run that simply produced no
 * output for this node — the inspector renders one honest empty state for both.
 */
export type NodeOutputView =
  | { state: "loading" }
  | { state: "unavailable" }
  | {
      state: "present";
      value: unknown;
      truncated: boolean;
      /**
       * Whether the snapshot this node came from is a PARTIAL capture — the run
       * failed or blocked rather than settling cleanly (issue #1008). Drives a
       * badge that distinguishes "run stopped early, here is what it reached"
       * from a clean result. Optional so a caller that has no partial signal
       * (a live run's in-memory result) can omit it — absent reads as `false`.
       */
      partial?: boolean;
    };

/** One node's readable, shape-checked result, ready to render. */
export interface NodeResult {
  id: string;
  name: string;
  /** The condition branch taken (`null` when the node isn't a branch point). */
  port: string | null;
  messages: NodeMessage[];
}

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

/** A non-empty trimmed string, else `null` (defensive against non-strings). */
export function nonEmptyString(value: unknown): string | null {
  return typeof value === "string" && value.trim() ? value : null;
}

/**
 * Pull a field from an item's `json`, preferring the OUTERMOST value and falling
 * back to the NESTED `json.json.<key>` the engine sometimes emits. Handles the
 * observed shape where `json` carries both a top-level `text` and a nested
 * `json.json.text` — the outer one wins.
 */
export function readNested(json: unknown, key: string): string | null {
  if (!isRecord(json)) return null;
  const outer = nonEmptyString(json[key]);
  if (outer) return outer;
  const inner = json.json;
  if (isRecord(inner)) return nonEmptyString(inner[key]);
  return null;
}

/**
 * Parse ONE node's raw output value (`{ items: [...], port? }`) into its
 * readable messages and branch port. The building block both `parseRunNodes` and
 * the single-node inspector use, so a live node and a past node parse identically.
 */
export function parseNodeMessages(raw: unknown): NodeMessage[] {
  const items = isRecord(raw) && Array.isArray(raw.items) ? raw.items : [];
  return items
    .map((item) => {
      const json = isRecord(item) ? item.json : undefined;
      return {
        text: readNested(json, "text"),
        agentRef: readNested(json, "agent_ref"),
      };
    })
    .filter((m) => m.text || m.agentRef);
}

/** The branch a node routed to, when it is a branch point. */
export function parseNodePort(raw: unknown): string | null {
  return isRecord(raw) ? nonEmptyString(raw.port) : null;
}

/**
 * Parse a single node's output into a `NodeResult`, given its display name.
 * Used by the run inspector, which fetches a whole run's output and renders one
 * node at a time.
 */
export function parseSingleNode(id: string, name: string, raw: unknown): NodeResult {
  return {
    id,
    name,
    port: parseNodePort(raw),
    messages: parseNodeMessages(raw),
  };
}

/**
 * Safely parse the engine's run output into per-node results, ordered by the
 * loaded graph when available (falling back to the map's insertion order).
 * Returns `null` when `output` doesn't match the expected `{ nodes: {…} }`
 * shape, signalling the caller to fall back to the raw JSON dump. Every access is
 * guarded — `output` is typed `unknown` and older/edge runs may be a plain
 * string, missing `nodes`, or carry malformed node values.
 */
export function parseRunNodes(
  output: unknown,
  graph: WorkflowGraph | null,
): NodeResult[] | null {
  if (!isRecord(output) || !isRecord(output.nodes)) {
    console.debug(
      "[run-output] run output missing a `nodes` map; showing raw JSON",
      output,
    );
    return null;
  }
  const nodes = output.nodes;

  // Order by the graph's node order when we have it, then append any node ids
  // present in the output but not in the graph (in the map's insertion order).
  const graphOrder = graph?.nodes.map((n) => n.id) ?? [];
  const orderedIds = [
    ...graphOrder.filter((id) => id in nodes),
    ...Object.keys(nodes).filter((id) => !graphOrder.includes(id)),
  ];

  const nameById = new Map(graph?.nodes.map((n) => [n.id, n.name]) ?? []);

  return orderedIds.map((id) =>
    parseSingleNode(id, nameById.get(id) ?? id, nodes[id]),
  );
}

/**
 * The raw output value for one node id out of a whole run's `output` (or a
 * durable record's `nodes` map). Returns `undefined` when the run has no such
 * node — the inspector renders an explicit "produced none" state.
 *
 * Accepts either the full run `output` (`{ nodes: {…} }`) or the durable
 * record's already-unwrapped `nodes` map, so the same helper serves the live and
 * the past-run surfaces.
 */
export function nodeOutputFor(output: unknown, nodeId: string): unknown {
  if (!isRecord(output)) return undefined;
  const map = isRecord(output.nodes) ? output.nodes : output;
  return isRecord(map) ? map[nodeId] : undefined;
}
