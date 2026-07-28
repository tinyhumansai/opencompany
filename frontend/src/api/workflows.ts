// The live workflow API: the console's Workflows canvas reads the company's
// saved graphs through the host's `…/workflows` routes (REST, camelCase over
// the wire) and runs one via `…/workflows/{wid}/run`. Replaces the client-side
// `workflow-sample` illustrative data.

import type { OpenCompanyClient } from "./client";

/** A one-line workflow entry, as the picker lists it. */
export interface WorkflowSummary {
  id: string;
  name: string;
  description?: string;
}

/** A single graph node. `kind` is one of the tinyflows node kinds. */
export interface WorkflowNode {
  id: string;
  /**
   * `trigger` | `agent` | `tool_call` | `http_request` | `condition` |
   * `output` | `switch` | `merge` | `split_out` | `transform` |
   * `output_parser` | `sub_workflow`.
   */
  kind: string;
  name: string;
  summary?: string;
  /** The roster agent id — only present on `agent` nodes. */
  agent?: string;
  /** Kind-specific configuration (a slug, URL, case labels, schema, …). */
  config?: unknown;
  /** How the engine handles an error on this node, when set. */
  onError?: string;
  /** The node's retry policy, when set. */
  retry?: {
    maxAttempts?: number;
    backoffMs?: number;
    backoff?: string;
  };
  /** Whether the node pauses for a human approval before proceeding. */
  requiresApproval?: boolean;
}

/** A directed edge between two node ids, with an optional branch label. */
export interface WorkflowEdge {
  from: string;
  to: string;
  label?: string;
}

/** The full graph the canvas renders. */
export interface WorkflowGraph {
  id: string;
  name: string;
  description?: string;
  nodes: WorkflowNode[];
  edges: WorkflowEdge[];
}

/** The result of a run: the engine's final state and any pending approvals. */
export interface WorkflowRunResult {
  /** The engine's final run state — a nested JSON payload. */
  output: unknown;
  /** Node ids left waiting on a human approval, if any. */
  pendingApprovals: string[];
}

export function listWorkflows(
  client: OpenCompanyClient,
  company: string | null,
): Promise<WorkflowSummary[]> {
  return client.get<WorkflowSummary[]>(`${client.scopeFor(company)}/workflows`);
}

export function getWorkflow(
  client: OpenCompanyClient,
  company: string | null,
  wid: string,
): Promise<WorkflowGraph> {
  return client.get<WorkflowGraph>(
    `${client.scopeFor(company)}/workflows/${encodeURIComponent(wid)}`,
  );
}

export function runWorkflow(
  client: OpenCompanyClient,
  company: string | null,
  wid: string,
  input?: unknown,
): Promise<WorkflowRunResult> {
  return client.post<WorkflowRunResult>(
    `${client.scopeFor(company)}/workflows/${encodeURIComponent(wid)}/run`,
    { input: input ?? {} },
  );
}

/**
 * Authors a new workflow graph (issue #69): the console's form creator posts
 * the same shape `getWorkflow` returns, and the host writes it to
 * `workflows/{id}.toml`. Rejections carry a prosumer-language `ApiError`
 * message (bad id, duplicate id, an edge or `agent` node the graph can't
 * support, no writable source directory on this deployment).
 */
export function createWorkflow(
  client: OpenCompanyClient,
  company: string | null,
  graph: WorkflowGraph,
): Promise<WorkflowGraph> {
  return client.post<WorkflowGraph>(`${client.scopeFor(company)}/workflows`, graph);
}

/**
 * The node kinds the form creator's palette offers. These are the kinds that
 * are meaningful to author from a bare form — no per-node config required to do
 * something useful: `merge` fans several inputs into one stream and `transform`
 * passes items through (a config-less `set` is an identity pass-through).
 *
 * Deliberately withheld until the P4 config forms land: `tool_call`,
 * `http_request`, `switch`, `output_parser`, and `sub_workflow` all need config
 * (a slug, a URL, case labels, a schema, a `workflow_id`) to run — creating one
 * from a bare palette would silently produce a node that errors at run time — so
 * the creator doesn't offer them yet. All of these kinds still render on the
 * canvas and can be authored by hand in `workflows/<id>.toml`.
 */
export const CREATABLE_NODE_KINDS: { value: string; label: string }[] = [
  { value: "trigger", label: "Trigger — starts the workflow" },
  { value: "agent", label: "Agent — a teammate performs a step" },
  { value: "condition", label: "Condition — branches on something" },
  { value: "merge", label: "Merge — combines several inputs into one" },
  { value: "transform", label: "Transform — reshapes the data" },
  { value: "output", label: "Output — reports the result back" },
];
