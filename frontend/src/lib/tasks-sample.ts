// Sample Kanban data for the Tasks board. Client-side illustrative data — the
// console has no live task API yet, so the board is a local working surface.

export type TaskPriority = "low" | "medium" | "high";

export interface TaskCard {
  id: string;
  title: string;
  note?: string;
  column: string;
  priority: TaskPriority;
  /** Which desk owns it — matches the conversation thread tones. */
  assignee: { name: string; tone: string };
}

export interface TaskColumn {
  id: string;
  label: string;
}

export const TASK_COLUMNS: TaskColumn[] = [
  { id: "backlog", label: "Backlog" },
  { id: "in_progress", label: "In progress" },
  { id: "in_review", label: "In review" },
  { id: "done", label: "Done" },
];

const STRATEGY = { name: "Strategy desk", tone: "sky" };
const CREATIVE = { name: "Creative studio", tone: "violet" };
const FRONT = { name: "Front desk", tone: "amber" };

let n = 0;
const id = () => `task-${n++}`;

export function sampleTasks(): TaskCard[] {
  return [
    { id: id(), title: "Q2 campaign brief", note: "Turn the client brief into an angle", column: "backlog", priority: "high", assignee: STRATEGY },
    { id: id(), title: "Competitor scan", note: "Pull three rival launches", column: "backlog", priority: "low", assignee: STRATEGY },
    { id: id(), title: "Newsletter refresh", note: "New template + segments", column: "backlog", priority: "medium", assignee: FRONT },
    { id: id(), title: "Spring launch taglines", note: "Draft three options", column: "in_progress", priority: "high", assignee: CREATIVE },
    { id: id(), title: "Landing hero image", note: "Generate + retouch", column: "in_progress", priority: "medium", assignee: CREATIVE },
    { id: id(), title: "Invoice March retainer", column: "in_review", priority: "medium", assignee: FRONT },
    { id: id(), title: "Brand voice guide", note: "One-pager for the team", column: "done", priority: "low", assignee: STRATEGY },
    { id: id(), title: "Welcome email flow", column: "done", priority: "medium", assignee: FRONT },
  ];
}

export const PRIORITY_STYLES: Record<TaskPriority, string> = {
  high: "border-rose-500/30 bg-rose-500/10 text-rose-600 dark:text-rose-400",
  medium: "border-amber-500/30 bg-amber-500/10 text-amber-600 dark:text-amber-400",
  low: "border-border bg-muted text-muted-foreground",
};
