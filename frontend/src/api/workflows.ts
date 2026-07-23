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
