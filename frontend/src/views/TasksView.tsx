import { useCallback, useEffect, useRef, useState } from "react";
import { Loader2, Plus, Trash2 } from "lucide-react";

import {
  createTask,
  deleteTask,
  listTasks,
  patchTask,
  type PatchTask,
  type Task,
} from "@/api/tasks";
import type { OpenCompanyClient } from "@/api/client";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";
import { Textarea } from "@/components/ui/textarea";
import { cn } from "@/lib/utils";
import { PRIORITY_STYLES, TASK_COLUMNS } from "@/lib/tasks-sample";
import { toast } from "sonner";

const PRIORITIES = ["low", "medium", "high"] as const;

/** How often to re-poll the board, so a dispatched card's result appears. */
const POLL_MS = 4000;

function priorityStyle(priority: string): string {
  return PRIORITY_STYLES[priority as keyof typeof PRIORITY_STYLES] ?? PRIORITY_STYLES.low;
}

/**
 * The live Kanban board. Cards are read from and written to the host's
 * `…/tasks` routes; dragging a card into a column PATCHes it (moving one into
 * "In progress" is what dispatches it to its assignee on the embedded runtime),
 * and clicking a card opens its detail — where the agent's result shows up in
 * the note once the turn completes.
 */
export function TasksView({
  client,
  company,
}: {
  client: OpenCompanyClient;
  company: string | null;
}) {
  const [tasks, setTasks] = useState<Task[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [dragId, setDragId] = useState<string | null>(null);
  const [overCol, setOverCol] = useState<string | null>(null);
  const [selected, setSelected] = useState<Task | null>(null);
  const [creatingIn, setCreatingIn] = useState<string | null>(null);
  const mounted = useRef(true);
  // A real HTML5 drag fires a trailing click; suppress it so a drag never also
  // opens the detail dialog.
  const dragged = useRef(false);

  const refresh = useCallback(async () => {
    try {
      const rows = await listTasks(client, company);
      if (!mounted.current) return;
      setTasks(rows);
      setError(null);
    } catch (e) {
      if (!mounted.current) return;
      setError(e instanceof Error ? e.message : "could not load the board");
    } finally {
      if (mounted.current) setLoading(false);
    }
  }, [client, company]);

  useEffect(() => {
    mounted.current = true;
    void refresh();
    const timer = setInterval(() => void refresh(), POLL_MS);
    return () => {
      mounted.current = false;
      clearInterval(timer);
    };
  }, [refresh]);

  async function moveTo(column: string) {
    const id = dragId;
    setDragId(null);
    setOverCol(null);
    if (!id) return;
    const current = tasks.find((t) => t.id === id);
    if (!current || current.column === column) return;
    // Optimistic move; reconcile with the server's echo (and revert on error).
    setTasks((ts) => ts.map((t) => (t.id === id ? { ...t, column } : t)));
    try {
      const saved = await patchTask(client, company, id, { column });
      setTasks((ts) => ts.map((t) => (t.id === id ? saved : t)));
      if (column === "in_progress") {
        toast.success("Dispatched — the assignee is working on it.");
        // The turn runs server-side; poll a touch sooner so the result shows.
        setTimeout(() => void refresh(), 1500);
      }
    } catch (e) {
      setTasks((ts) => ts.map((t) => (t.id === id ? { ...t, column: current.column } : t)));
      toast.error(e instanceof Error ? e.message : "could not move the card");
    }
  }

  function openCard(task: Task) {
    if (dragged.current) {
      dragged.current = false;
      return;
    }
    setSelected(task);
  }

  return (
    <div className="flex flex-1 flex-col overflow-hidden">
      <div className="flex items-center justify-between border-b px-4 py-3">
        <div className="flex items-center gap-2">
          <h2 className="text-sm font-semibold">Board</h2>
          <Badge variant="secondary">{tasks.length}</Badge>
        </div>
        <p className="hidden text-xs text-muted-foreground sm:block">
          Drag a card to move it; drop into “In progress” to hand it to its assignee.
        </p>
      </div>

      {error && (
        <div className="px-4 pt-3">
          <Alert variant="destructive">
            <AlertDescription>{error}</AlertDescription>
          </Alert>
        </div>
      )}

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
              onDrop={() => void moveTo(col.id)}
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
                <button
                  className="text-muted-foreground hover:text-foreground"
                  aria-label={`Add task to ${col.label}`}
                  onClick={() => setCreatingIn(col.id)}
                >
                  <Plus className="size-4" />
                </button>
              </div>
              <div className="flex flex-1 flex-col gap-2 overflow-y-auto px-2 pb-2">
                {loading && items.length === 0 ? (
                  <Skeleton className="h-20 rounded-lg" />
                ) : (
                  items.map((t) => (
                    <TaskItem
                      key={t.id}
                      task={t}
                      dragging={dragId === t.id}
                      onOpen={() => openCard(t)}
                      onDragStart={() => {
                        dragged.current = true;
                        setDragId(t.id);
                      }}
                      onDragEnd={() => {
                        setDragId(null);
                        setOverCol(null);
                        // Clear the drag-suppression shortly after, so a genuine
                        // click that follows is honored.
                        setTimeout(() => (dragged.current = false), 0);
                      }}
                    />
                  ))
                )}
                {!loading && items.length === 0 && (
                  <div className="rounded-lg border border-dashed py-6 text-center text-xs text-muted-foreground">
                    Drop tasks here
                  </div>
                )}
              </div>
            </div>
          );
        })}
      </div>

      <TaskDetailDialog
        task={selected}
        onClose={() => setSelected(null)}
        onSaved={(saved) => {
          setTasks((ts) => ts.map((t) => (t.id === saved.id ? saved : t)));
          setSelected(null);
        }}
        onDeleted={(id) => {
          setTasks((ts) => ts.filter((t) => t.id !== id));
          setSelected(null);
        }}
        client={client}
        company={company}
      />

      <CreateTaskDialog
        column={creatingIn}
        onClose={() => setCreatingIn(null)}
        onCreated={(created) => {
          setTasks((ts) => [created, ...ts]);
          setCreatingIn(null);
        }}
        client={client}
        company={company}
      />
    </div>
  );
}

function TaskItem({
  task,
  dragging,
  onOpen,
  onDragStart,
  onDragEnd,
}: {
  task: Task;
  dragging: boolean;
  onOpen: () => void;
  onDragStart: () => void;
  onDragEnd: () => void;
}) {
  return (
    <div
      draggable
      onDragStart={onDragStart}
      onDragEnd={onDragEnd}
      onClick={onOpen}
      role="button"
      tabIndex={0}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onOpen();
        }
      }}
      className={cn(
        "cursor-grab rounded-lg border bg-card p-3 shadow-sm transition-shadow hover:shadow active:cursor-grabbing",
        dragging && "opacity-50",
      )}
    >
      <div className="flex items-start justify-between gap-2">
        <p className="text-sm font-medium leading-snug">{task.title}</p>
        <Badge variant="outline" className={cn("shrink-0 capitalize", priorityStyle(task.priority))}>
          {task.priority}
        </Badge>
      </div>
      {task.note && (
        <p className="mt-1 line-clamp-2 whitespace-pre-wrap text-xs text-muted-foreground">
          {task.note}
        </p>
      )}
      {task.assignee && (
        <div className="mt-3 flex items-center gap-2">
          <span
            className="flex size-6 items-center justify-center rounded-full bg-muted text-[10px] font-semibold text-muted-foreground"
            aria-hidden
          >
            {initials(task.assignee)}
          </span>
          <span className="truncate text-xs text-muted-foreground">{task.assignee}</span>
        </div>
      )}
    </div>
  );
}

function TaskDetailDialog({
  task,
  onClose,
  onSaved,
  onDeleted,
  client,
  company,
}: {
  task: Task | null;
  onClose: () => void;
  onSaved: (t: Task) => void;
  onDeleted: (id: string) => void;
  client: OpenCompanyClient;
  company: string | null;
}) {
  const [draft, setDraft] = useState<PatchTask>({});
  const [busy, setBusy] = useState(false);

  // Reset the edit draft each time a different card is opened.
  useEffect(() => {
    if (task) {
      setDraft({
        title: task.title,
        note: task.note ?? "",
        column: task.column,
        priority: task.priority,
        assignee: task.assignee,
      });
    }
  }, [task]);

  if (!task) return null;

  async function save() {
    if (!task) return;
    setBusy(true);
    try {
      const saved = await patchTask(client, company, task.id, draft);
      onSaved(saved);
      toast.success("Saved.");
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "could not save");
    } finally {
      setBusy(false);
    }
  }

  async function remove() {
    if (!task) return;
    setBusy(true);
    try {
      await deleteTask(client, company, task.id);
      onDeleted(task.id);
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "could not delete");
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog open={!!task} onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle>Task detail</DialogTitle>
          <DialogDescription>
            Edit the card, or drop it into “In progress” on the board to dispatch it.
          </DialogDescription>
        </DialogHeader>

        <div className="grid gap-3">
          <div className="grid gap-1.5">
            <Label htmlFor="task-title">Title</Label>
            <Input
              id="task-title"
              value={draft.title ?? ""}
              onChange={(e) => setDraft((d) => ({ ...d, title: e.target.value }))}
            />
          </div>

          <div className="grid gap-1.5">
            <Label htmlFor="task-note">Note / result</Label>
            <Textarea
              id="task-note"
              rows={8}
              className="font-mono text-xs"
              value={draft.note ?? ""}
              onChange={(e) => setDraft((d) => ({ ...d, note: e.target.value }))}
            />
          </div>

          <div className="grid grid-cols-3 gap-3">
            <div className="grid gap-1.5">
              <Label>Column</Label>
              <Select
                value={draft.column}
                onValueChange={(v) => setDraft((d) => ({ ...d, column: v ?? undefined }))}
              >
                <SelectTrigger>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {TASK_COLUMNS.map((c) => (
                    <SelectItem key={c.id} value={c.id}>
                      {c.label}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            <div className="grid gap-1.5">
              <Label>Priority</Label>
              <Select
                value={draft.priority}
                onValueChange={(v) => setDraft((d) => ({ ...d, priority: v ?? undefined }))}
              >
                <SelectTrigger>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {PRIORITIES.map((p) => (
                    <SelectItem key={p} value={p} className="capitalize">
                      {p}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            <div className="grid gap-1.5">
              <Label htmlFor="task-assignee">Assignee</Label>
              <Input
                id="task-assignee"
                value={draft.assignee ?? ""}
                placeholder="agent id"
                onChange={(e) => setDraft((d) => ({ ...d, assignee: e.target.value }))}
              />
            </div>
          </div>
        </div>

        <DialogFooter className="justify-between sm:justify-between">
          <Button variant="ghost" size="sm" onClick={() => void remove()} disabled={busy}>
            <Trash2 className="mr-1.5 size-4" />
            Delete
          </Button>
          <div className="flex gap-2">
            <Button variant="outline" onClick={onClose} disabled={busy}>
              Cancel
            </Button>
            <Button onClick={() => void save()} disabled={busy}>
              {busy && <Loader2 className="mr-1.5 size-4 animate-spin" />}
              Save
            </Button>
          </div>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function CreateTaskDialog({
  column,
  onClose,
  onCreated,
  client,
  company,
}: {
  column: string | null;
  onClose: () => void;
  onCreated: (t: Task) => void;
  client: OpenCompanyClient;
  company: string | null;
}) {
  const [title, setTitle] = useState("");
  const [note, setNote] = useState("");
  const [priority, setPriority] = useState("medium");
  const [assignee, setAssignee] = useState("");
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (column) {
      setTitle("");
      setNote("");
      setPriority("medium");
      setAssignee("");
    }
  }, [column]);

  if (!column) return null;

  async function create() {
    if (!title.trim()) return;
    setBusy(true);
    try {
      const created = await createTask(client, company, {
        title: title.trim(),
        note: note.trim() || undefined,
        column: column ?? undefined,
        priority,
        assignee: assignee.trim() || undefined,
      });
      onCreated(created);
      toast.success("Task created.");
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "could not create the task");
    } finally {
      setBusy(false);
    }
  }

  const columnLabel = TASK_COLUMNS.find((c) => c.id === column)?.label ?? column;

  return (
    <Dialog open={!!column} onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle>New task</DialogTitle>
          <DialogDescription>Added to “{columnLabel}”.</DialogDescription>
        </DialogHeader>

        <div className="grid gap-3">
          <div className="grid gap-1.5">
            <Label htmlFor="new-title">Title</Label>
            <Input
              id="new-title"
              autoFocus
              value={title}
              onChange={(e) => setTitle(e.target.value)}
              placeholder="What needs doing?"
            />
          </div>
          <div className="grid gap-1.5">
            <Label htmlFor="new-note">Note</Label>
            <Textarea
              id="new-note"
              rows={4}
              value={note}
              onChange={(e) => setNote(e.target.value)}
              placeholder="Any detail the assignee should act on."
            />
          </div>
          <div className="grid grid-cols-2 gap-3">
            <div className="grid gap-1.5">
              <Label>Priority</Label>
              <Select value={priority} onValueChange={(v) => setPriority(v ?? "medium")}>
                <SelectTrigger>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {PRIORITIES.map((p) => (
                    <SelectItem key={p} value={p} className="capitalize">
                      {p}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            <div className="grid gap-1.5">
              <Label htmlFor="new-assignee">Assignee</Label>
              <Input
                id="new-assignee"
                value={assignee}
                placeholder="agent id"
                onChange={(e) => setAssignee(e.target.value)}
              />
            </div>
          </div>
        </div>

        <DialogFooter>
          <Button variant="outline" onClick={onClose} disabled={busy}>
            Cancel
          </Button>
          <Button onClick={() => void create()} disabled={busy || !title.trim()}>
            {busy && <Loader2 className="mr-1.5 size-4 animate-spin" />}
            Create
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
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
