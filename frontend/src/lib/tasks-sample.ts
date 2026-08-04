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

/**
 * The board's columns, left to right. Must stay in step with the backend's
 * `BOARD_COLUMNS` (`src/ports/tasks.rs`), which is the source of truth — and
 * which the REST write boundary rejects a card against, so a column that
 * exists only here is one the host will refuse.
 */
export const TASK_COLUMNS: TaskColumn[] = [
  // Everything not started (issue #301): work nobody has picked up yet, and
  // work the lifecycle returned needing another pass (a failed dispatch, a
  // cancellation, an orchestrator `revise` verdict) — the reason rides along on
  // the card's note. This replaces the old Backlog/To-do split (issue #206);
  // the host rewrites a stored `backlog` card to this column on read.
  //
  // It is also the board's manual-entry column: `ADD_TASK_COLUMN` below
  // restricts the "Add task" box to it, and it is what `POST …/tasks` defaults
  // to.
  { id: "todo", label: "To-do" },
  // Between intake and dispatch: the card is being turned into a plan. Inert
  // today — epic #183 §4's auto-advance is what will move cards through it, and
  // dragging one here fires nothing (only `in_progress` dispatches).
  { id: "planning", label: "Planning" },
  { id: "in_progress", label: "In progress" },
  // Where a steered-to-pause run parks (issue #111); a card here offers Resume.
  { id: "paused", label: "Paused" },
  { id: "in_review", label: "In review" },
  { id: "done", label: "Done" },
];

/**
 * The one column that offers the "+" add-task button (issue #206).
 *
 * New work enters the board in exactly one place. Offering `+` on every column
 * — as the board used to — let an operator create a card straight into
 * `in_progress`, `in_review`, or `done`, which either skips the dispatch edge
 * or fabricates a terminal state for work that never ran.
 */
export const ADD_TASK_COLUMN = "todo";

const STRATEGY = { name: "Strategy desk", tone: "sky" };
const CREATIVE = { name: "Creative studio", tone: "violet" };
const FRONT = { name: "Front desk", tone: "amber" };

let n = 0;
const id = () => `task-${n++}`;

export function sampleTasks(): TaskCard[] {
  return [
    { id: id(), title: "Q2 campaign brief", note: "Turn the client brief into an angle", column: "todo", priority: "high", assignee: STRATEGY },
    { id: id(), title: "Competitor scan", note: "Pull three rival launches", column: "todo", priority: "low", assignee: STRATEGY },
    { id: id(), title: "Newsletter refresh", note: "New template + segments", column: "planning", priority: "medium", assignee: FRONT },
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
