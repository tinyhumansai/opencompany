import { useState } from "react";
import { Plus } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { cn } from "@/lib/utils";
import {
  PRIORITY_STYLES,
  sampleTasks,
  TASK_COLUMNS,
  type TaskCard,
} from "@/lib/tasks-sample";

const TONES: Record<string, string> = {
  sky: "bg-sky-500/15 text-sky-600 dark:text-sky-400",
  violet: "bg-violet-500/15 text-violet-600 dark:text-violet-400",
  amber: "bg-amber-500/15 text-amber-600 dark:text-amber-400",
  emerald: "bg-emerald-500/15 text-emerald-600 dark:text-emerald-400",
};

/** A built-in Kanban board. Drag cards between columns to move work along. */
export function TasksView() {
  const [tasks, setTasks] = useState<TaskCard[]>(sampleTasks);
  const [dragId, setDragId] = useState<string | null>(null);
  const [overCol, setOverCol] = useState<string | null>(null);

  function moveTo(column: string) {
    if (!dragId) return;
    setTasks((ts) => ts.map((t) => (t.id === dragId ? { ...t, column } : t)));
    setDragId(null);
    setOverCol(null);
  }

  return (
    <div className="flex flex-1 flex-col overflow-hidden">
      <div className="flex items-center justify-between border-b px-4 py-3">
        <div className="flex items-center gap-2">
          <h2 className="text-sm font-semibold">Board</h2>
          <Badge variant="secondary">{tasks.length}</Badge>
        </div>
        <p className="hidden text-xs text-muted-foreground sm:block">
          Drag a card to move it between columns.
        </p>
      </div>

      <div className="flex flex-1 gap-4 overflow-x-auto p-4">
        {TASK_COLUMNS.map((col) => {
          const items = tasks.filter((t) => t.column === col.id);
          return (
            <div
              key={col.id}
              onDragOver={(e) => {
                e.preventDefault();
                setOverCol(col.id);
              }}
              onDragLeave={() => setOverCol((c) => (c === col.id ? null : c))}
              onDrop={() => moveTo(col.id)}
              className={cn(
                "flex w-72 shrink-0 flex-col rounded-xl border bg-card/40 transition-colors",
                overCol === col.id && "border-primary/40 bg-accent/40",
              )}
            >
              <div className="flex items-center justify-between px-3 py-2.5">
                <div className="flex items-center gap-2">
                  <span className="text-sm font-medium">{col.label}</span>
                  <span className="text-xs text-muted-foreground">{items.length}</span>
                </div>
                <button className="text-muted-foreground hover:text-foreground" aria-label="Add task" disabled>
                  <Plus className="size-4" />
                </button>
              </div>
              <div className="flex flex-1 flex-col gap-2 overflow-y-auto px-2 pb-2">
                {items.map((t) => (
                  <TaskItem
                    key={t.id}
                    task={t}
                    dragging={dragId === t.id}
                    onDragStart={() => setDragId(t.id)}
                    onDragEnd={() => {
                      setDragId(null);
                      setOverCol(null);
                    }}
                  />
                ))}
                {items.length === 0 && (
                  <div className="rounded-lg border border-dashed py-6 text-center text-xs text-muted-foreground">
                    Drop tasks here
                  </div>
                )}
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}

function TaskItem({
  task,
  dragging,
  onDragStart,
  onDragEnd,
}: {
  task: TaskCard;
  dragging: boolean;
  onDragStart: () => void;
  onDragEnd: () => void;
}) {
  return (
    <div
      draggable
      onDragStart={onDragStart}
      onDragEnd={onDragEnd}
      className={cn(
        "cursor-grab rounded-lg border bg-card p-3 shadow-sm transition-shadow hover:shadow active:cursor-grabbing",
        dragging && "opacity-50",
      )}
    >
      <div className="flex items-start justify-between gap-2">
        <p className="text-sm font-medium leading-snug">{task.title}</p>
        <Badge variant="outline" className={cn("shrink-0 capitalize", PRIORITY_STYLES[task.priority])}>
          {task.priority}
        </Badge>
      </div>
      {task.note && <p className="mt-1 text-xs text-muted-foreground">{task.note}</p>}
      <div className="mt-3 flex items-center gap-2">
        <span
          className={cn(
            "flex size-6 items-center justify-center rounded-full text-[10px] font-semibold",
            TONES[task.assignee.tone] ?? "bg-muted text-muted-foreground",
          )}
          aria-hidden
        >
          {initials(task.assignee.name)}
        </span>
        <span className="truncate text-xs text-muted-foreground">{task.assignee.name}</span>
      </div>
    </div>
  );
}

function initials(name: string): string {
  return name
    .trim()
    .split(/\s+/)
    .slice(0, 2)
    .map((p) => p.charAt(0).toUpperCase())
    .join("");
}
