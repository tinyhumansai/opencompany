// The record shapes the knowledge graph is built from.
//
// The graph, its layout, and its detail cards were written against a five-ring
// org model — departments, written-out SOP tasks, the one worker who does each,
// and that worker's tools. This console's host does not serve that model, so
// `adapter.ts` derives it; these are the types both sides agree on.

export type AgentStatus = "active" | "idle" | "paused";
export type AgentTier = "lead" | "worker";

export interface Department {
  id: string;
  name: string;
  slug: string;
  tagline: string;
  color: string;
  order: number;
}

export interface Agent {
  id: string;
  departmentId: string;
  name: string;
  role: string;
  status: AgentStatus;
  tier: AgentTier;
  description: string;
  model: string;
  tools: string[];
  parentId: string | null;
  instance: string;
}

export interface Person {
  id: string;
  departmentId: string;
  name: string;
  role: string;
  tools: string[];
}

export type SopAssigneeKind = "agent" | "person";

export interface SopTask {
  id: string;
  departmentId: string;
  /** The job, stated as work. */
  title: string;
  summary: string;
  /** The written-out checklist. */
  steps: string[];
  assigneeKind: SopAssigneeKind;
  assigneeId: string;
}

export interface AgentRun {
  id: string;
  agentId: string;
  startedAt: string;
  finishedAt: string;
  ok: boolean;
  summary: string;
  model?: string | null;
  tokensIn?: number | null;
  tokensOut?: number | null;
  costUsd?: number | null;
}

export interface RosterClient {
  id: string;
  name: string;
  venture: string;
  status: string;
  amountUsd: number | null;
  source: "attio" | "funnel";
}
