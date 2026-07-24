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
  /** `trigger` | `agent` | `tool_call` | `http_request` | `condition` | `output`. */
  kind: string;
  name: string;
  summary?: string;
  /** The roster agent id — only present on `agent` nodes. */
  agent?: string;
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
 * The node kinds the form creator's palette offers. `tool_call` and
 * `http_request` are real graph kinds the engine stores and the canvas
 * renders, but the runtime only executes `trigger`/`agent`/`condition`/
 * `output` today — the other two are `Unwired*` stubs that error at run time
 * (see `src/workflows/caps.rs`). Creating one from scratch would silently
 * produce a workflow that can never finish, so the creator doesn't offer them.
 */
export const CREATABLE_NODE_KINDS: { value: string; label: string }[] = [
  { value: "trigger", label: "Trigger — starts the workflow" },
  { value: "agent", label: "Agent — a teammate performs a step" },
  { value: "condition", label: "Condition — branches on something" },
  { value: "output", label: "Output — reports the result back" },
];
