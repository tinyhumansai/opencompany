// The live task-board API: the console's Kanban reads and writes real cards
// through the host's `…/tasks` routes (REST, camelCase over the wire). Replaces
// the client-side `tasks-sample` illustrative data.

import type { OpenCompanyClient } from "./client";

/** A board card as the host returns it. */
export interface Task {
  id: string;
  title: string;
  note?: string;
  column: string;
  priority: string;
  /** The desk/teammate label that owns it (a roster agent id routes a turn). */
  assignee: string;
  updatedAt: number;
}

/** The create body; the host defaults column→`backlog`, priority→`medium`. */
export interface CreateTask {
  title: string;
  note?: string;
  column?: string;
  priority?: string;
  assignee?: string;
}

/** A partial update; any omitted field is left as-is. A drag sends `{column}`. */
export interface PatchTask {
  title?: string;
  note?: string;
  column?: string;
  priority?: string;
  assignee?: string;
}

export function listTasks(client: OpenCompanyClient, company: string | null): Promise<Task[]> {
  return client.get<Task[]>(`${client.scopeFor(company)}/tasks`);
}

export function createTask(
  client: OpenCompanyClient,
  company: string | null,
  body: CreateTask,
): Promise<Task> {
  return client.post<Task>(`${client.scopeFor(company)}/tasks`, body);
}

export function patchTask(
  client: OpenCompanyClient,
  company: string | null,
  id: string,
  body: PatchTask,
): Promise<Task> {
  return client.patch<Task>(`${client.scopeFor(company)}/tasks/${encodeURIComponent(id)}`, body);
}

export function deleteTask(
  client: OpenCompanyClient,
  company: string | null,
  id: string,
): Promise<void> {
  return client.del<void>(`${client.scopeFor(company)}/tasks/${encodeURIComponent(id)}`);
}

/** A steer verb the operator can apply to an in-flight run (issue #111). */
export type SteerAction = "pause" | "cancel" | "redirect";

/**
 * One in-flight run the operator can steer (issue #111): a dispatched board
 * task (`kind: "task"`) or a sub-agent delegation (`kind: "delegation"`).
 * `key` is the steer identifier — for a board task it equals the task id.
 * `pendingAction` is non-null while a steer of that verb is already in flight
 * for this run, so the console can badge + disable its row.
 */
export interface InflightRun {
  taskId: string | null;
  key: string;
  kind: "task" | "delegation";
  title: string;
  agentId: string;
  startedAt: number;
  pendingAction: string | null;
}

/** The steer body. `redirect` requires `instruction`; `cancel` requires `confirm`. */
export interface SteerInput {
  action: SteerAction;
  instruction?: string;
  confirm?: boolean;
}

/** The runs currently in flight for a company, steerable from company chat. */
export function listInflight(
  client: OpenCompanyClient,
  company: string | null,
): Promise<InflightRun[]> {
  return client.get<InflightRun[]>(`${client.scopeFor(company)}/tasks/inflight`);
}

/**
 * Steer an in-flight run by its `key`: pause it, cancel it (requires
 * `confirm: true`), or redirect it with a fresh `instruction`. The host answers
 * 202 with no body; callers should refetch {@link listInflight} afterwards.
 */
export function steerTask(
  client: OpenCompanyClient,
  company: string | null,
  key: string,
  body: SteerInput,
): Promise<void> {
  return client.post<void>(
    `${client.scopeFor(company)}/tasks/${encodeURIComponent(key)}/steer`,
    body,
  );
}
