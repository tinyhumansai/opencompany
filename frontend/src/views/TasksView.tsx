// The task board — restored, and now driven by the `tasks` ledger.
//
// # What "ported to the ledger" actually changed
//
// The board used to carry its own column vocabulary: a `TASK_COLUMNS` literal
// that had to be kept in step with the host's `BOARD_COLUMNS` by hand, and whose
// own comment admitted a Rust test could not see it. It now asks. Columns, their
// order, their labels and which of them ends a card's life all come from the
// `tasks` ledger's statuses, which the host builds from one table
// (`src/ledger/board.rs`). A column added there appears here, correctly
// labelled, with no console release.
//
// What did **not** change is where a card's *content* comes from. A task is a
// `Task`: a priority, an assignee, a plan brief, a published output, a
// deliverable kind, a resume affordance for a paused run. None of that is a
// ledger field, and the ledger's projection of the board deliberately does not
// carry it — so this screen still reads `…/tasks` for its rows and renders them
// itself. The ledger supplies the *shape* of the board; the task store supplies
// what is on it.
//
// # And why the board itself is not here
//
// The columns, the counts, the drag mechanics and the three fixes behind issue
// #334 live in [`LedgerBoard`](./LedgerBoard), which this screen hands a
// `renderCard`. That is what stopped the Ledgers section growing a second,
// thinner board that lost all three the moment it was written — which is exactly
// what happened the first time.

import { useCallback, useEffect, useRef, useState } from "react";
import {
  AlertTriangle,
  CircleHelp,
  ClipboardList,
  FileText,
  Hourglass,
  ListTree,
  Paperclip,
  Play,
  Plus,
  ScrollText,
} from "lucide-react";
import { toast } from "sonner";

import {
  listTasks,
  patchTask,
  type Task,
  type TaskPlan,
} from "@/api/tasks";
import type { OpenCompanyClient } from "@/api/client";
import type { ApprovalSummary } from "@/api/types";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { formatUsdCost } from "@/lib/cost";
import { startVisiblePolling } from "@/lib/visible-poll";
import { labelFor, PRIORITY_STYLES } from "@/lib/board-columns";
import { approvalAction, timeAgo } from "@/lib/language";
import { taskApprovalBlock, type TaskApprovalBlock } from "@/lib/task-approvals";
import { useBoardColumns } from "@/hooks/use-board-columns";
import {
  extraOutputCount,
  primaryLink,
  readTaskFocus,
  type TaskFocus,
  type TaskLink,
} from "@/lib/task-output";
import { LedgerBoard } from "./LedgerBoard";
import { CreateTaskDialog } from "./CreateTaskDialog";
import { TaskDetailView } from "./TaskDetailView";
import { tallyPrerequisites } from "./TaskPlanBrief";

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

/**
 * The board's fallback refresh interval.
 *
 * Since issue #464 this is **no longer how the board stays current** — the
 * company SSE stream is, through `taskEventTick`. It stays as the degradation
 * path, and only that: a host that does not serve `{scope}/events` (a 404 the
 * events hook swallows by design) has no push, and without this the board there
 * would go back to being a snapshot of its own mount. It is the same stance
 * `use-events.ts` takes toward the 5s status poll.
 *
 * Left at its original cadence deliberately. Slowing it down would be paid for
 * entirely by the hosts that have no push, and speeding it up is the polling
 * answer #464 exists to replace.
 */
const POLL_MS = 4000;

/**
 * A stable empty default for {@link TasksView}'s `approvals` prop.
 *
 * A `[]` literal in the parameter list is a new array identity on every render.
 * Hoisting it keeps the default stable for anything downstream that compares by
 * reference, on a screen that re-renders on a 4s poll.
 */
const EMPTY_APPROVALS: readonly ApprovalSummary[] = [];

function priorityStyle(priority: string): string {
  return PRIORITY_STYLES[priority as keyof typeof PRIORITY_STYLES] ?? PRIORITY_STYLES.low;
}



/**
 * The live task board. Cards are read from and written to the host's `…/tasks`
 * routes; dragging a card into a column PATCHes it (moving one into
 * "In progress" is what hands it to its assignee).
 */
export function TasksView({
  client,
  company,
  taskEventTick,
  attemptEventTick,
  approvals = EMPTY_APPROVALS,
  now,
  onOpenThread,
  onReviewApprovals,
}: {
  client: OpenCompanyClient;
  company: string | null;
  /**
   * A counter the shell bumps on every task event off the company SSE stream
   * (issue #464) — a card opened, moved, settled, dispatched or steered. The
   * board re-reads itself whenever it changes, which is what makes *ask for
   * something in chat, watch it become work on the board* true without a
   * reload.
   */
  taskEventTick?: number;
  /**
   * Bumped on every `run_status_changed` (issue #1015), passed straight through
   * to the detail screen. The board itself does not react to it: an attempt
   * moving does not move a card, and folding it into `taskEventTick` would
   * refetch the whole board on every transition of every run.
   */
  attemptEventTick?: number;
  /**
   * The company's parked approvals (issue #883), from the shell's existing feed
   * poll. A paused card is blocked until every approval its turn parked has
   * been decided, and the board's own `…/tasks` read carries none of them — so
   * without this a paused card can only show a Resume button and no reason.
   * Defaults to empty, which renders exactly the pre-#883 card.
   */
  approvals?: readonly ApprovalSummary[];
  /** The feed's clock, for "blocked for 4m". Falls back to the browser's. */
  now?: number;
  /** Opens the chat thread a card came from (issue #246). */
  onOpenThread?: (threadId: string) => void;
  /** Opens the Approvals page filtered to one card (issue #883). */
  onReviewApprovals?: (taskId: string) => void;
}) {
  // The board's shape, from the host. Nothing here declares a column.
  const columns = useBoardColumns(client, company);
  // The clock the "blocked since" labels measure against (issue #883). The
  // feed's is preferred because it is stamped at the same read the approvals
  // came from, so a card cannot report a wait longer than the data behind it;
  // the browser's is the fallback for a caller that passes approvals without one.
  const clock = now ?? Date.now();
  const [tasks, setTasks] = useState<Task[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [detailId, setDetailId] = useState<string | null>(readTaskDetailId);
  const [focus, setFocus] = useState<TaskFocus>(() =>
    readTaskFocus(window.location.hash),
  );
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

  // Issue #581: the fallback poll runs only while the tab is being looked at.
  // Reading the whole board every four seconds is affordable for an operator
  // watching it and pure waste for a background tab, and the poller's
  // load-on-visible is what makes pausing safe — a tab coming back re-reads
  // immediately rather than showing a stale board for up to an interval.
  useEffect(() => {
    mounted.current = true;
    void refresh();
    const dispose = startVisiblePolling(() => void refresh(), POLL_MS);
    return () => {
      mounted.current = false;
      dispose();
    };
  }, [refresh]);

  // Issue #464: the push half. Re-read the board the moment the host says
  // something on it moved, rather than up to a poll interval later.
  //
  // Its own effect rather than a dependency of the one above, on purpose:
  // folding the tick in there would tear down and restart the fallback timer on
  // every event, so a busy company would keep resetting the interval and the
  // fallback would effectively stop existing on exactly the companies that need
  // it most.
  const seenTick = useRef(taskEventTick);
  useEffect(() => {
    if (taskEventTick === undefined || taskEventTick === seenTick.current) return;
    // Issue #581: the push half is gated too, or the poll gate above buys
    // nothing on a busy company. The tick is deliberately NOT consumed on the
    // way out — marking it seen here would drop it, since this effect does not
    // re-run on a visibility change.
    if (document.visibilityState === "hidden") return;
    seenTick.current = taskEventTick;
    void refresh();
  }, [taskEventTick, refresh]);

  // Follow browser back/forward and manual edits of the `#/tasks/<id>` sub-hash.
  useEffect(() => {
    const onHash = () => {
      setDetailId(readTaskDetailId());
      // Re-read the focus too, so a card link clicked while the detail is
      // already open actually moves the screen rather than only changing the
      // address bar.
      setFocus(readTaskFocus(window.location.hash));
    };
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  function openDetail(id: string) {
    window.location.hash = `#/tasks/${id}`;
  }

  function closeDetail() {
    window.location.hash = "#/tasks";
  }

  function openCard(task: Task) {
    if (dragged.current) {
      dragged.current = false;
      return;
    }
    openDetail(task.id);
  }

  /**
   * Moves a card into `column`.
   *
   * Optimistic, reconciled against the server echo, and **reverted out loud**.
   * Every exit says something (issue #334): a drop that goes nowhere and
   * reports nothing is indistinguishable from a frozen app, and that is what
   * made one failed gesture read as "there is no way to do this". The one
   * deliberate silence — a card landing back in its own column — never reaches
   * here, because the board filters a no-op before it calls.
   */
  async function moveTo(task: Task, column: string) {
    const was = task.column;
    setTasks((ts) => ts.map((t) => (t.id === task.id ? { ...t, column } : t)));
    try {
      const saved = await patchTask(client, company, task.id, { column });
      setTasks((ts) => ts.map((t) => (t.id === task.id ? saved : t)));
      if (column === "in_progress") {
        toast.success("Dispatched — the assignee is working on it.");
        // The turn runs server-side; poll a touch sooner so the result shows.
        setTimeout(() => void refresh(), 1500);
      }
    } catch (e) {
      setTasks((ts) => ts.map((t) => (t.id === task.id ? { ...t, column: was } : t)));
      // The card has already snapped back by the time this is read, so the
      // message carries the whole story: which card, where it was going, and
      // the host's own words for why it would not go. The board validates
      // nothing itself — the columns are the host's — so the reason for a
      // refusal only ever exists in the response.
      toast.error(`Could not move "${task.title}" to ${labelFor(columns, column)}.`, {
        description: e instanceof Error ? e.message : "the host refused the move",
      });
    }
  }

  // Re-dispatch a paused card (issue #111): a Resume moves it back into
  // "In progress", which is what hands it to its assignee again.
  //
  // Issue #883: this is not reached while the card is blocked on its own
  // undecided approvals — `TaskItem` disables the button from the same
  // `taskApprovalBlock` read, which is deliberately the *only* place the rule
  // lives. A second copy of it here would be a branch nothing can execute, and
  // therefore a branch nothing keeps true.
  async function resume(task: Task) {
    setTasks((ts) => ts.map((t) => (t.id === task.id ? { ...t, column: "in_progress" } : t)));
    try {
      const saved = await patchTask(client, company, task.id, { column: "in_progress" });
      setTasks((ts) => ts.map((t) => (t.id === task.id ? saved : t)));
      toast.success("Resumed — the assignee is working on it.");
      setTimeout(() => void refresh(), 1500);
    } catch (e) {
      setTasks((ts) => ts.map((t) => (t.id === task.id ? { ...t, column: task.column } : t)));
      toast.error(e instanceof Error ? e.message : "could not resume the card");
    }
  }

  // The detail screen replaces the board in place; the board keeps polling
  // underneath so its state is reconciled by the time we return.
  if (detailId) {
    return (
      <TaskDetailView
        attemptEventTick={attemptEventTick}
        client={client}
        company={company}
        taskId={detailId}
        focus={focus}
        // Issue #883: so the detail row can name the blocked call rather than
        // only counting it. The screen's own read still decides whether the
        // card is waiting; this only supplies the words.
        parked={approvals}
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
          <Button size="sm" className="ml-1 h-7" onClick={() => setCreating(true)}>
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

      <div className="flex min-h-0 flex-1 flex-col px-4 py-4">
        <LedgerBoard
          columns={columns}
          rows={tasks}
          statusOf={(task) => task.column}
          loading={loading}
          emptyHint="Drop tasks here"
          onMove={(task, column) => void moveTo(task, column)}
          onMiss={() => toast.error("Drop the card on a column to move it.")}
          renderCard={(task, dragging) => (
            <TaskItem
              task={task}
              dragging={dragging}
              // Issue #883: what this card is stopped behind, derived from the
              // shell's approvals feed. `null` for every card that is not
              // blocked, which is what keeps the pre-#883 card unchanged.
              block={taskApprovalBlock(approvals, task.id)}
              now={clock}
              onOpen={() => openCard(task)}
              onResume={() => void resume(task)}
              onReview={onReviewApprovals ? () => onReviewApprovals(task.id) : undefined}
            />
          )}
        />
      </div>

      <CreateTaskDialog
        open={creating}
        onClose={() => setCreating(false)}
        onCreated={(created) => {
          setCreating(false);
          setTasks((ts) => [created, ...ts]);
        }}
        client={client}
        company={company}
      />
    </div>
  );
}

/**
 * One card on the task board.
 *
 * It no longer carries the drag handlers: [`LedgerBoard`](./LedgerBoard) wraps
 * every card in the draggable element and owns the gesture, so this is purely
 * what a *task* looks like. That split is what lets one board serve both this
 * and a ledger a company declared — see that module's docs for why the card is
 * a slot rather than something built from field roles.
 *
 * Exported for `test/unit/task-blocked-card.test.ts` (issue #883). The
 * paused card's central claim — Resume is *disabled* while the card's own
 * approvals are undecided, because pressing it re-runs work that parks again —
 * exists only at the rendered button, so a pure test of the derivation cannot
 * reach it. Same exception `approval-batch-card.test.ts` earns, on the same
 * grounds: the thing under test is what reaches the operator's hand.
 */
export function TaskItem({
  task,
  dragging,
  block,
  now,
  onOpen,
  onResume,
  onReview,
}: {
  task: Task;
  dragging: boolean;
  /** What this card is stopped behind, or `null` when nothing (issue #883). */
  block: TaskApprovalBlock | null;
  /** The clock `block` was derived against, for its relative label. */
  now: number;
  onOpen: () => void;
  onResume: () => void;
  /** Opens the Approvals page filtered to this card (issue #883). */
  onReview?: () => void;
}) {
  return (
    <div
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
            className="flex size-6 items-center justify-center rounded-full bg-muted text-3xs font-semibold text-muted-foreground"
            aria-hidden
          >
            {initials(task.assignee)}
          </span>
          <span className="truncate text-xs text-muted-foreground">{task.assignee}</span>
        </div>
      )}
      {formatUsdCost(task.cost, "total") && (
        <div className="mt-2 text-2xs font-medium tabular-nums text-foreground">
          {formatUsdCost(task.cost, "total")}
        </div>
      )}
      {task.deliverable === "workflow" && (
        <div className="mt-2 inline-flex items-center gap-1 rounded-full border px-2 py-0.5 text-2xs text-muted-foreground">
          <ListTree className="size-3 shrink-0" />
          Workflow
        </div>
      )}
      {task.plan && <PlanBadgeRow plan={task.plan} />}
      {SHOWS_OUTPUT_LINK.has(task.column) && <OutputLinkRow task={task} />}
      {task.column === "paused" && (
        <>
          {block && <BlockedRow block={block} now={now} onReview={onReview} />}
          <Button
            variant="outline"
            size="sm"
            className={cn("h-7 w-full", block ? "mt-2" : "mt-3")}
            // Issue #883: the button is disabled rather than hidden while the
            // card is blocked. Hiding it would leave the card looking like it
            // had no next action at all, which is the ambiguity being fixed —
            // the operator has to be able to see that Resume is the wrong click
            // right now, not wonder where it went. `title` carries the reason
            // for a pointer; the row above carries it for everyone else.
            disabled={block !== null}
            title={
              block
                ? "Blocked — decide its approvals first; resuming re-runs the work from the start."
                : undefined
            }
            onClick={(e) => {
              // Don't let the click bubble to the card's open handler.
              e.stopPropagation();
              onResume();
            }}
          >
            <Play className="mr-1.5 size-3.5" />
            Resume
          </Button>
        </>
      )}
    </div>
  );
}

/**
 * Why a paused card is stopped, on the card itself (issue #883).
 *
 * The card used to carry a Resume button and nothing else, so "decided one of
 * five, still waiting on four" and "wedged" were the same pixels — and Resume
 * was the natural next click from both. It is the wrong click from the first:
 * the turn continues on its own when the last decision lands (#469), so
 * re-dispatching only re-runs the work and parks the same calls again.
 *
 * Names the calls, not the mechanism. One blocked call is quoted in full —
 * through {@link approvalAction}, the same function the Approvals page and the
 * chat card label their rows with, so all three say "Fetch a web page" rather
 * than three different things about one approval. Several are counted instead,
 * because five tool names is not something to read on a Kanban card; the count
 * plus the Review link is, and the page it links to lists them.
 */
function BlockedRow({
  block,
  now,
  onReview,
}: {
  block: TaskApprovalBlock;
  /** The same clock the block was derived against. */
  now: number;
  onReview?: () => void;
}) {
  const only = block.count === 1 ? block.approvals[0] : null;
  return (
    <div className="mt-2 rounded-md border border-status-blocked/30 bg-status-blocked-soft px-2 py-1.5">
      <div className="flex items-center gap-1.5 text-2xs font-medium text-status-blocked-text">
        <Hourglass className="size-3 shrink-0" />
        <span className="min-w-0 truncate">
          {only ? approvalAction(only) : `Blocked on ${block.count} approvals`}
        </span>
      </div>
      <div className="mt-0.5 flex items-center justify-between gap-2 text-2xs text-muted-foreground">
        <span className="truncate">
          Waiting for your approval · {timeAgo(block.since, now)}
        </span>
        {onReview && (
          <button
            type="button"
            className="shrink-0 font-medium text-status-blocked-text underline-offset-2 hover:underline"
            onClick={(e) => {
              // The card's own click handler opens task detail; this goes
              // somewhere else, so it must not also do that.
              e.stopPropagation();
              onReview();
            }}
          >
            Review
          </button>
        )}
      </div>
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
      <div className="mt-2 flex items-center gap-1.5 text-2xs font-medium text-destructive">
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
      <div className="mt-2 flex items-center gap-1.5 text-2xs text-status-blocked-text">
        <CircleHelp className="size-3 shrink-0" />
        <span>
          Planned — {unresolved} to be aware of
        </span>
      </div>
    );
  }
  return (
    <div className="mt-2 flex items-center gap-1.5 text-2xs text-muted-foreground">
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


function initials(name: string): string {
  return name
    .trim()
    .split(/\s+/)
    .slice(0, 2)
    .map((p) => p.charAt(0).toUpperCase())
    .join("");
}
