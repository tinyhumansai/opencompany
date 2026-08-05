import { type DragEvent, useCallback, useEffect, useRef, useState } from "react";
import {
  AlertTriangle,
  CircleHelp,
  ClipboardList,
  FileText,
  ListTree,
  Loader2,
  Paperclip,
  Play,
  Plus,
  ScrollText,
} from "lucide-react";

import {
  createTask,
  listTasks,
  patchTask,
  type Task,
  type TaskPlan,
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
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { Textarea } from "@/components/ui/textarea";
import { cn } from "@/lib/utils";
import { ADD_TASK_COLUMN, PRIORITY_STYLES, TASK_COLUMNS } from "@/lib/tasks-sample";
import {
  extraOutputCount,
  primaryLink,
  readTaskFocus,
  type TaskFocus,
  type TaskLink,
} from "@/lib/task-output";
import { toast } from "sonner";
import { TaskDetailView } from "./TaskDetailView";
import { tallyPrerequisites } from "./TaskPlanBrief";

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

/**
 * The MIME type a dragged card stamps its id onto (issue #334).
 *
 * The drop used to read the dragged id out of React state alone. That is a
 * silent single point of failure: a drag that began on anything other than a
 * card leaves the state null, and the drop handler then returned without a
 * word. Putting the id on the drag itself makes a drop self-describing.
 *
 * Filling the data store at all matters for a second reason: a drag whose store
 * is empty is aborted outright by Firefox and Safari, so the board's one
 * documented gesture never even started there.
 *
 * Every read and write of it is optional-chained. A real drag always carries a
 * `dataTransfer`; a synthesized `DragEvent` — which is how the e2e suite drives
 * these handlers — does not, and the `dragId` fallback covers that case.
 */
const CARD_MIME = "application/x-opencompany-task";

/**
 * How near the board's left or right edge a drag has to come before the board
 * starts scrolling itself, and how fast it goes once hard against that edge.
 *
 * The board is a horizontal scroller and, at six columns, wider than an
 * ordinary window: the last column sits off the right edge. HTML5
 * drag-and-drop does not scroll a nested scroll container on its own — a drag
 * parked on the edge moves it zero pixels — so without this the far column
 * cannot be reached by the very gesture the board's own hint recommends.
 */
const EDGE_BAND_PX = 72;
const EDGE_SPEED_PX = 16;

function priorityStyle(priority: string): string {
  return PRIORITY_STYLES[priority as keyof typeof PRIORITY_STYLES] ?? PRIORITY_STYLES.low;
}

/** A column's board label, for messages the operator reads. */
function columnLabel(id: string): string {
  return TASK_COLUMNS.find((c) => c.id === id)?.label ?? id;
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
  // What the address asks the detail screen to open (issue #339): a pinned
  // artifact, or an attempt's trace. Empty for a plain `#/tasks/<id>`, which is
  // the ordinary "open the card" navigation and lands on the default tab.
  const [focus, setFocus] = useState<TaskFocus>(() =>
    readTaskFocus(window.location.hash),
  );
  const [creating, setCreating] = useState(false);
  const mounted = useRef(true);
  // A real HTML5 drag fires a trailing click; suppress it so a drag never also
  // opens the detail dialog.
  const dragged = useRef(false);
  // The horizontal scroller holding the columns, so a drag near its edge can
  // scroll it (issue #334).
  const boardRef = useRef<HTMLDivElement | null>(null);
  // Pixels per frame the board is currently scrolling itself by, and the frame
  // that is doing it. Both refs rather than state: this is driven by dragover
  // at pointer rate and must not re-render the board on every move.
  const edgeSpeed = useRef(0);
  const edgeFrame = useRef<number | null>(null);

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
    const onHash = () => {
      setDetailId(readTaskDetailId());
      // Re-read the focus too, so a card link clicked while the detail is
      // already open (a `+N more` row, or a second card's link) actually moves
      // the screen rather than only changing the address bar.
      setFocus(readTaskFocus(window.location.hash));
    };
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  const stopEdgeScroll = useCallback(() => {
    edgeSpeed.current = 0;
    if (edgeFrame.current !== null) {
      cancelAnimationFrame(edgeFrame.current);
      edgeFrame.current = null;
    }
  }, []);

  /**
   * Aims the board's self-scroll at wherever the drag currently is: full speed
   * hard against an edge, easing to nothing at the band's inner lip, and off
   * entirely across the middle of the board.
   */
  const edgeScrollTo = useCallback(
    (clientX: number) => {
      const board = boardRef.current;
      if (!board) return;
      const { left, right } = board.getBoundingClientRect();
      const ramp = (depth: number) => Math.min(1, Math.max(0, 1 - depth / EDGE_BAND_PX));
      let speed = 0;
      if (clientX < left + EDGE_BAND_PX) speed = -EDGE_SPEED_PX * ramp(clientX - left);
      else if (clientX > right - EDGE_BAND_PX) speed = EDGE_SPEED_PX * ramp(right - clientX);
      if (speed === 0) {
        stopEdgeScroll();
        return;
      }
      edgeSpeed.current = speed;
      if (edgeFrame.current !== null) return;
      const step = () => {
        const el = boardRef.current;
        if (!el || edgeSpeed.current === 0) {
          edgeFrame.current = null;
          return;
        }
        el.scrollLeft += edgeSpeed.current;
        edgeFrame.current = requestAnimationFrame(step);
      };
      edgeFrame.current = requestAnimationFrame(step);
    },
    [stopEdgeScroll],
  );

  // A drag interrupted by a view change (the detail screen replaces the board
  // in place) must not leave a frame running against a detached node.
  useEffect(() => stopEdgeScroll, [stopEdgeScroll]);

  const openDetail = useCallback((id: string) => {
    window.location.hash = `/tasks/${encodeURIComponent(id)}`;
    setDetailId(id);
    // An ordinary open carries no focus, and must clear any it inherited —
    // otherwise opening a second card would re-apply the first card's artifact
    // pin to a screen that has never heard of it.
    setFocus({});
  }, []);
  const closeDetail = useCallback(() => {
    window.location.hash = "/tasks";
    setDetailId(null);
    setFocus({});
  }, []);

  /**
   * Lands a dropped card in `column`. `dropped` is the id the drag carried on
   * its dataTransfer, which is authoritative; `dragId` is the fallback for a
   * drop that arrives without one.
   *
   * Every exit from here now says something (issue #334). A drop that goes
   * nowhere and reports nothing is indistinguishable from a frozen app, and
   * that is what made one failed gesture read as "there is no way to do this".
   * The one deliberate silence is a card landing back in its own column: that
   * is a no-op, not a refusal.
   */
  async function moveTo(column: string, dropped: string | null) {
    const id = dropped || dragId;
    setDragId(null);
    setOverCol(null);
    if (!id) {
      toast.error("That drop did not carry a card, so nothing moved.");
      return;
    }
    const current = tasks.find((t) => t.id === id);
    if (!current) {
      toast.error("That card is no longer on the board. Reloading it now.");
      void refresh();
      return;
    }
    if (current.column === column) return;
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
      // The card has already snapped back by the time this is read, so the
      // message carries the whole story: which card, where it was going, and
      // the host's own words for why it would not go. The board validates
      // nothing itself — `BOARD_COLUMNS` is the host's list — so the reason for
      // a refusal only ever exists in the response.
      toast.error(`Could not move "${current.title}" to ${columnLabel(column)}.`, {
        description: e instanceof Error ? e.message : "the host refused the move",
      });
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
        focus={focus}
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

      <div
        ref={boardRef}
        onDragOver={(e) => {
          // The columns preventDefault as well; this handler also covers the
          // pixels between and around them, so the board keeps scrolling while
          // a drag crosses a gap on its way to a far column.
          e.preventDefault();
          edgeScrollTo(e.clientX);
        }}
        onDragLeave={(e) => {
          // Only when the pointer has genuinely left the board, not while it
          // moves between two of the board's own children.
          if (!e.currentTarget.contains(e.relatedTarget as Node | null)) stopEdgeScroll();
        }}
        onDrop={(e) => {
          // Columns claim their own drops and stop them here, so anything that
          // reaches this handler landed on dead board pixels: the gap between
          // two columns, the leading padding, the trailing gutter. Those used
          // to swallow the whole gesture without a word (issue #334).
          e.preventDefault();
          stopEdgeScroll();
          const id = e.dataTransfer?.getData(CARD_MIME) || dragId;
          setDragId(null);
          setOverCol(null);
          if (id) toast.error("Drop the card on a column to move it.");
        }}
        className="flex min-h-0 flex-1 gap-4 overflow-x-auto py-4 pl-4"
      >
        {TASK_COLUMNS.map((col) => {
          const items = tasks.filter((t) => t.column === col.id);
          return (
            <div
              key={col.id}
              onDragOver={(e) => {
                e.preventDefault();
                // Runs before the board's own dragover (this is the inner
                // handler), and the board leaves it alone — so the cursor says
                // "move" over a column and nothing over the dead pixels.
                if (e.dataTransfer) e.dataTransfer.dropEffect = "move";
                setOverCol(col.id);
              }}
              onDragLeave={() => setOverCol((c) => (c === col.id ? null : c))}
              onDrop={(e) => {
                e.preventDefault();
                // Claim the drop, so anything still reaching the board's own
                // handler is known to have missed every column.
                e.stopPropagation();
                stopEdgeScroll();
                void moveTo(col.id, e.dataTransfer?.getData(CARD_MIME) ?? null);
              }}
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
                      onDragStart={(e) => {
                        dragged.current = true;
                        setDragId(t.id);
                        if (e.dataTransfer) {
                          e.dataTransfer.effectAllowed = "move";
                          // The id is what the drop reads back. The
                          // `text/plain` copy is what makes the drag
                          // well-formed for the browsers that abort one
                          // carrying no data at all.
                          e.dataTransfer.setData(CARD_MIME, t.id);
                          e.dataTransfer.setData("text/plain", t.title);
                        }
                      }}
                      onDragEnd={() => {
                        setDragId(null);
                        setOverCol(null);
                        stopEdgeScroll();
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
  /** Takes the event so the card can stamp its id onto the drag (issue #334). */
  onDragStart: (e: DragEvent<HTMLDivElement>) => void;
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
      {task.plan && <PlanBadgeRow plan={task.plan} />}
      {SHOWS_OUTPUT_LINK.has(task.column) && <OutputLinkRow task={task} />}
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

/**
 * The columns whose cards show what they produced (issue #339).
 *
 * Done **and In review**, which is a correction to how the epic is worded. A
 * clean success no longer lands in Done — it stops in In review, and Done is
 * reached only when a person accepts it. So a card that has produced something
 * spends most of its visible life in In review, and showing the link only in
 * Done would hide it during exactly the stretch where somebody is deciding
 * whether to accept the work and needs to read it.
 *
 * Not the earlier columns: a card in To-do or In progress either has no output
 * yet or has one from a superseded attempt, and advertising that mid-run would
 * suggest the work in flight is already finished.
 */
const SHOWS_OUTPUT_LINK = new Set(["in_review", "done"]);

/**
 * What a planned card carries, in one line on the board (issue #337).
 *
 * Shown on **every** column a plan survives into rather than a chosen set, and
 * that is the difference from {@link SHOWS_OUTPUT_LINK} above. An output is
 * only meaningful once there is one, so it earns a column filter; a plan is
 * only ever present because a person deliberately asked for one, so hiding it
 * anywhere would be second-guessing that request.
 *
 * The blocked case is the one that has to be loud. A pass that could not clear
 * a card returns it to To-do, where it sits looking exactly like work nobody
 * has picked up — and the difference between "not started" and "cannot start"
 * is the whole point of having planned it. So blockers get the destructive
 * treatment and a count; a clear plan gets a quiet step count and stays out of
 * the way.
 *
 * `needsApproval` and `unknown` are deliberately not counted here. Neither
 * stops the card host-side, and a badge that counted them would tell an
 * operator to go fix something that is not blocking anything.
 */
function PlanBadgeRow({ plan }: { plan: TaskPlan }) {
  const { blocking, approval, unchecked } = tallyPrerequisites(plan);
  if (blocking > 0) {
    return (
      <div className="mt-2 flex items-center gap-1.5 text-[11px] font-medium text-destructive">
        <AlertTriangle className="size-3 shrink-0" />
        <span>
          Planned — needs {blocking} thing{blocking === 1 ? "" : "s"}
        </span>
      </div>
    );
  }
  // Nothing blocking, but not necessarily all-clear either — the same three-way
  // distinction the brief's headline makes, kept in step with it so the board
  // and the card can never disagree about whether a plan is settled. A count
  // here is a prompt to open the card, where the rows say which is which.
  const unresolved = approval + unchecked;
  if (unresolved > 0) {
    return (
      <div className="mt-2 flex items-center gap-1.5 text-[11px] text-amber-600 dark:text-amber-400">
        <CircleHelp className="size-3 shrink-0" />
        <span>
          Planned — {unresolved} to be aware of
        </span>
      </div>
    );
  }
  return (
    <div className="mt-2 flex items-center gap-1.5 text-[11px] text-muted-foreground">
      <ClipboardList className="size-3 shrink-0" />
      <span>
        Planned
        {plan.steps.length > 0 && ` · ${plan.steps.length} step${plan.steps.length === 1 ? "" : "s"}`}
      </span>
    </div>
  );
}

function LinkIcon({ kind }: { kind: TaskLink["kind"] }) {
  const className = "size-3.5 shrink-0";
  if (kind === "artifact") return <Paperclip className={className} />;
  if (kind === "workflow") return <ListTree className={className} />;
  if (kind === "trace") return <ScrollText className={className} />;
  return <FileText className={className} />;
}

/**
 * One line on a finished card: *here is the thing this task produced*.
 *
 * Every card in these columns gets one, including the ones that produced no
 * file — for those the link opens the attempt's trace, which is the deliverable
 * when there is no document. A card that recorded no attempt at all links to
 * itself, which is honest rather than absent.
 *
 * The anchor stops its own click from bubbling: the whole card is a button that
 * opens the detail screen, and without this a click on the link would both
 * follow the href and fire the card's `onOpen`, racing two navigations.
 */
function OutputLinkRow({ task }: { task: Task }) {
  const link = primaryLink(task);
  const extra = extraOutputCount(task);
  return (
    <div className="mt-3 flex items-center gap-2 border-t pt-2 text-xs">
      <a
        href={link.href}
        title={link.hint}
        onClick={(e) => e.stopPropagation()}
        className="flex min-w-0 items-center gap-1.5 text-muted-foreground hover:text-foreground hover:underline"
      >
        <LinkIcon kind={link.kind} />
        <span className="truncate">{link.label}</span>
      </a>
      {extra > 0 && (
        <a
          href={`#/tasks/${encodeURIComponent(task.id)}`}
          title="Open the task to see everything it produced."
          onClick={(e) => e.stopPropagation()}
          className="shrink-0 text-muted-foreground hover:text-foreground hover:underline"
        >
          +{extra} more
        </a>
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
