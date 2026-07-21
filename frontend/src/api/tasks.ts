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
