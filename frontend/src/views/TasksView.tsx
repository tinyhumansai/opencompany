import { useCallback, useEffect, useRef, useState } from "react";
import { Loader2, Play, Plus } from "lucide-react";

import { createTask, listTasks, patchTask, type Task } from "@/api/tasks";
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
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { Textarea } from "@/components/ui/textarea";
import { cn } from "@/lib/utils";
import { ADD_TASK_COLUMN, PRIORITY_STYLES, TASK_COLUMNS } from "@/lib/tasks-sample";
import { toast } from "sonner";
import { TaskDetailView } from "./TaskDetailView";

/**
 * Reads the `#/tasks/<id>` sub-hash, or null on the bare board. The app shell's
 * `useHashView` only inspects the first path segment (`tasks`), so this second
 * segment is ours to own — no app-shell change needed to route the detail.
 */
function readTaskDetailId(): string | null {
  try {
    const parts = window.location.hash.replace(/^#\/?/, "").split(/[/?]/);
    return parts[0] === "tasks" && parts[1] ? decodeURIComponent(parts[1]) : null;
  } catch {
    // Malformed percent-encoding (e.g. `#/tasks/%`) throws URIError — fall back
    // to the bare board instead of blowing up the render. Covers both the
    // useState initializer and the hashchange handler, since both call here.
    return null;
  }
}

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
  onOpenThread,
}: {
  client: OpenCompanyClient;
  company: string | null;
  /**
   * Opens the chat thread a card came from (issue #246). Absent when the board
   * is rendered somewhere with no conversation pane to jump to, in which case
   * the detail screen states the origin without offering the jump.
   */
  onOpenThread?: (threadId: string) => void;
}) {
  const [tasks, setTasks] = useState<Task[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [dragId, setDragId] = useState<string | null>(null);
  const [overCol, setOverCol] = useState<string | null>(null);
  // The open card's id, mirrored in `#/tasks/<id>` so the detail survives a
  // refresh and honors back/forward.
  const [detailId, setDetailId] = useState<string | null>(readTaskDetailId);
  const [creating, setCreating] = useState(false);
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

  // Follow browser back/forward and manual edits of the `#/tasks/<id>` sub-hash.
  useEffect(() => {
    const onHash = () => setDetailId(readTaskDetailId());
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  const openDetail = useCallback((id: string) => {
    window.location.hash = `/tasks/${encodeURIComponent(id)}`;
    setDetailId(id);
  }, []);
  const closeDetail = useCallback(() => {
    window.location.hash = "/tasks";
    setDetailId(null);
  }, []);

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

  // Re-dispatch a paused card (issue #111): a Resume moves it back into
  // "In progress", which is what hands it to its assignee again. Optimistic,
  // reconciled against the server echo — the same shape as a drag-move.
  async function resume(task: Task) {
    setTasks((ts) => ts.map((t) => (t.id === task.id ? { ...t, column: "in_progress" } : t)));
    try {
      const saved = await patchTask(client, company, task.id, { column: "in_progress" });
      setTasks((ts) => ts.map((t) => (t.id === task.id ? saved : t)));
      toast.success("Resumed — the assignee is working on it.");
      // The turn runs server-side; poll a touch sooner so the result shows.
      setTimeout(() => void refresh(), 1500);
    } catch (e) {
      setTasks((ts) => ts.map((t) => (t.id === task.id ? { ...t, column: task.column } : t)));
      toast.error(e instanceof Error ? e.message : "could not resume the card");
    }
  }

  function openCard(task: Task) {
    if (dragged.current) {
      dragged.current = false;
      return;
    }
    openDetail(task.id);
  }

  // The detail screen replaces the board in place; the board keeps polling
  // underneath so its state is reconciled by the time we return.
  if (detailId) {
    return (
      <TaskDetailView
        client={client}
        company={company}
        taskId={detailId}
        onBack={closeDetail}
        onNavigate={openDetail}
        onOpenThread={onOpenThread}
        onSaved={(saved) => setTasks((ts) => ts.map((t) => (t.id === saved.id ? saved : t)))}
        onDeleted={(id) => setTasks((ts) => ts.filter((t) => t.id !== id))}
      />
    );
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-hidden">
      <div className="flex items-center justify-between border-b px-4 py-3">
        <div className="flex items-center gap-2">
          <h2 className="text-sm font-semibold">Board</h2>
          <Badge variant="secondary">{tasks.length}</Badge>
          <Button
            size="sm"
            variant="outline"
            className="ml-1 h-7"
            onClick={() => setCreating(true)}
          >
            <Plus className="size-4" />
            Add task
          </Button>
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

      <div className="flex min-h-0 flex-1 gap-4 overflow-x-auto py-4 pl-4">
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
                "flex min-h-0 w-72 shrink-0 flex-col rounded-xl border bg-card/40 transition-colors",
                overCol === col.id && "border-primary/40 bg-accent/40",
              )}
            >
              {/* New work enters the board in one place only (issue #206), and
                  that entry point now lives in the board header rather than on
                  this column. */}
              <div className="flex items-center gap-2 px-3 py-2.5">
                <span className="text-sm font-medium">{col.label}</span>
                <span className="text-xs text-muted-foreground">{items.length}</span>
              </div>
              <div className="flex min-h-0 flex-1 flex-col gap-2 overflow-y-auto px-2 pb-2">
                {loading && items.length === 0 ? (
                  <Skeleton className="h-20 rounded-lg" />
                ) : (
                  items.map((t) => (
                    <TaskItem
                      key={t.id}
                      task={t}
                      dragging={dragId === t.id}
                      onOpen={() => openCard(t)}
                      onResume={() => void resume(t)}
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
        {/* Trailing gutter: flex scroll containers drop their padding-inline-end,
            so this spacer keeps ~16px of breathing room past the last column. */}
        <div aria-hidden className="w-4 shrink-0" />
      </div>

      <CreateTaskDialog
        open={creating}
        onClose={() => setCreating(false)}
        onCreated={(created) => {
          setTasks((ts) => [created, ...ts]);
          setCreating(false);
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
  onResume,
  onDragStart,
  onDragEnd,
}: {
  task: Task;
  dragging: boolean;
  onOpen: () => void;
  onResume: () => void;
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
        if (e.target !== e.currentTarget) return;
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
      {task.column === "paused" && (
        <Button
          variant="outline"
          size="sm"
          className="mt-3 h-7 w-full"
          onClick={(e) => {
            // Don't let the click bubble to the card's open handler.
            e.stopPropagation();
            onResume();
          }}
        >
          <Play className="mr-1.5 size-3.5" />
          Resume
        </Button>
      )}
    </div>
  );
}

/** How long a derived title may run before the full prompt moves to the note. */
const TITLE_CAP = 80;

/**
 * Splits a prompt into the card's `{title, note}` (issue #301).
 *
 * The dialog asks for one thing — what needs doing — so the card's two text
 * fields are derived rather than collected. The rule mirrors the host's own
 * chat task-intent derivation (`src/server/operator.rs`) and `delegate_to_desk`'s
 * `first_line(…, 80)`: the title is the prompt's first line, capped; the note
 * carries the **full** prompt only when the title was shortened from it, so a
 * one-liner does not duplicate itself onto its own card.
 *
 * The invariant that matters: the operator's full text always survives on the
 * card, in the title or the note. Epic #183 §4's planner reads it from there.
 */
export function derivePromptCard(prompt: string): { title: string; note?: string } {
  const full = prompt.trim();
  const firstLine = full.split("\n")[0].trim();
  const title =
    firstLine.length > TITLE_CAP ? `${firstLine.slice(0, TITLE_CAP).trimEnd()}…` : firstLine;
  return { title, note: title === full ? undefined : full };
}

/**
 * New work enters the board through one prompt box (issue #301).
 *
 * Title/Note/Priority/Assignee used to be collected up front. They are not gone,
 * only moved: priority and assignee default on the host (`medium`, unassigned →
 * orchestrator) and are edited on the card afterwards, where #278 put the
 * picker. `column` is omitted on purpose so the *server's* intake default
 * decides where the card lands — the same spend gate the transcript's "Add to
 * board" relies on, keeping the human drag into In progress the only thing that
 * spends an agent turn.
 */
function CreateTaskDialog({
  open,
  onClose,
  onCreated,
  client,
  company,
}: {
  open: boolean;
  onClose: () => void;
  onCreated: (t: Task) => void;
  client: OpenCompanyClient;
  company: string | null;
}) {
  const [prompt, setPrompt] = useState("");
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (open) setPrompt("");
  }, [open]);

  if (!open) return null;

  async function create() {
    const { title, note } = derivePromptCard(prompt);
    if (!title) return;
    setBusy(true);
    try {
      const created = await createTask(client, company, { title, note });
      onCreated(created);
      toast.success("Task created.");
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "could not create the task");
    } finally {
      setBusy(false);
    }
  }

  const columnLabel =
    TASK_COLUMNS.find((c) => c.id === ADD_TASK_COLUMN)?.label ?? ADD_TASK_COLUMN;

  return (
    <Dialog open={open} onOpenChange={(o) => !o && onClose()}>
      {/* `sm:` — DialogContent's own `sm:max-w-sm` beats an unprefixed width. */}
      <DialogContent className="sm:max-w-3xl">
        <DialogHeader>
          <DialogTitle>New task</DialogTitle>
          <DialogDescription>Added to “{columnLabel}”.</DialogDescription>
        </DialogHeader>

        <div className="grid gap-1.5">
          <Label htmlFor="new-prompt">What needs doing?</Label>
          <Textarea
            id="new-prompt"
            autoFocus
            // Textarea is `field-sizing-content`, so `rows` is inert — a
            // min-height is what actually gives the box room.
            className="min-h-32 resize-y"
            value={prompt}
            onChange={(e) => setPrompt(e.target.value)}
            placeholder="Describe the work. The first line becomes the card's title."
          />
        </div>

        <DialogFooter>
          <Button variant="outline" onClick={onClose} disabled={busy}>
            Cancel
          </Button>
          <Button onClick={() => void create()} disabled={busy || !prompt.trim()}>
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
