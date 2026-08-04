// The Task Detail screen (v1, #184): the epic's capstone read surface. One
// `GET …/tasks/{id}` (#185/#190) drives a header, the lineage rail, the event
// timeline, and a controls bar; a 4s visibility-gated poll keeps it live while
// the card is open. Cost/₹ is intentionally absent everywhere. The Artifacts tab is
// its own self-fetching surface (#306, over #187's routes); Discussion stays an
// honest stub pending its own backend.

import { useCallback, useEffect, useMemo, useState, type ReactElement } from "react";
import {
  AlertCircle,
  ArrowLeft,
  Ban,
  Brain,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  Clock,
  CornerDownRight,
  CornerUpLeft,
  CornerUpRight,
  Hourglass,
  Layers,
  Loader2,
  MessageSquare,
  MessagesSquare,
  Pencil,
  Play,
  Send,
  ShieldCheck,
  Square,
  StickyNote,
  UserCog,
  Wrench,
} from "lucide-react";

import {
  getTaskDetail,
  listInflight,
  patchTask,
  steerTask,
  type InflightRun,
  type StepStatus,
  type SteerAction,
  type Task,
  type TaskDetail,
  type TimelineEntry,
  type TimelineKind,
} from "@/api/tasks";
import {
  getRun,
  isRunOpen,
  runElapsedMillis,
  RUN_STATUS_LABEL,
  type RunDetail,
  type RunStatus,
  type RunSummary,
} from "@/api/runs";
import { ApiError } from "@/api/types";
import type { OpenCompanyClient } from "@/api/client";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "@/components/ui/alert-dialog";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Skeleton } from "@/components/ui/skeleton";
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
} from "@/components/ui/sheet";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { cn } from "@/lib/utils";
import { PRIORITY_STYLES, TASK_COLUMNS } from "@/lib/tasks-sample";
import { toast } from "sonner";
import { ArtifactsTab } from "./ArtifactsTab";
import { AssigneeSelect } from "./AssigneeSelect";
import { TaskEditDialog } from "./TaskEditDialog";

/** How often to re-poll the detail while the screen is open (visibility-gated). */
const POLL_MS = 4000;

function priorityStyle(priority: string): string {
  return PRIORITY_STYLES[priority as keyof typeof PRIORITY_STYLES] ?? PRIORITY_STYLES.low;
}

/** The column id → human label ("in_progress" → "In progress"), tolerating unknowns. */
function columnLabel(column: string): string {
  return TASK_COLUMNS.find((c) => c.id === column)?.label ?? column;
}

/**
 * Sums the task's "worked" spans from its timeline: each `dispatched` opens a
 * window that its matching `completed` closes. Re-dispatch reopens a fresh
 * window, so multiple spans accumulate. A `completed` with no open window
 * (legacy cards journaled before dispatch anchors existed) is skipped rather
 * than counted from zero. When a window is still open, its running time to
 * `now` is included and `live` is true — the caller ticks it every second.
 */
function workedMillis(timeline: TimelineEntry[], now: number): { millis: number; live: boolean } {
  let total = 0;
  let openAt: number | null = null;
  for (const e of timeline) {
    if (e.kind === "dispatched") {
      openAt = e.atMillis;
    } else if (e.kind === "completed" && openAt !== null) {
      total += Math.max(0, e.atMillis - openAt);
      openAt = null;
    }
  }
  const live = openAt !== null;
  if (openAt !== null) total += Math.max(0, now - openAt);
  return { millis: total, live };
}

/**
 * The task's waiting-on-a-human spans, merged and summed (#305).
 *
 * Each resolved approval carries `waitedMillis` — the host's exact park→resolve
 * span, already clamped to the run window — so a span is reconstructed as
 * `[atMillis - waitedMillis, atMillis]` rather than inferred from gaps between
 * rows. A still-parked approval has no resolution event yet, so the live wait
 * arrives separately as `waitingSince` and runs to `now`.
 *
 * Spans are interval-merged before summing. Two approvals can be parked at once
 * — the company is waiting *once* over that overlap, not twice — and without
 * the merge the waiting figure could exceed the elapsed time it is subtracted
 * from, which would read as a bug to anyone doing the arithmetic.
 */
function waitingMillis(
  timeline: TimelineEntry[],
  waitingSince: number | undefined,
  now: number,
): { millis: number; live: boolean } {
  const spans: Array<[number, number]> = [];
  for (const e of timeline) {
    if (e.kind !== "approval") continue;
    const waited = e.waitedMillis;
    // `undefined` means the host could not recover the park instant; `0` is a
    // real (instant) sign-off. Neither contributes a span.
    if (waited === undefined || waited <= 0) continue;
    spans.push([e.atMillis - waited, e.atMillis]);
  }
  const live = waitingSince !== undefined;
  if (waitingSince !== undefined) spans.push([waitingSince, Math.max(waitingSince, now)]);

  spans.sort((a, b) => a[0] - b[0]);
  let total = 0;
  let cursor: [number, number] | null = null;
  for (const span of spans) {
    if (cursor && span[0] <= cursor[1]) {
      cursor[1] = Math.max(cursor[1], span[1]);
    } else {
      if (cursor) total += cursor[1] - cursor[0];
      cursor = [span[0], span[1]];
    }
  }
  if (cursor) total += cursor[1] - cursor[0];
  return { millis: Math.max(0, total), live };
}

/**
 * The pixel height of a waiting band for a span of `millis` (#305).
 *
 * Sub-linear on purpose. The point of the band is that a four-hour wait and a
 * four-second wait must not look alike, but a linear scale makes the short one
 * invisible and the long one taller than the screen. A log curve with a floor
 * and a cap keeps both on the page: ~14px at four seconds, ~112px at four
 * hours. Past the cap the printed duration inside the band carries the
 * precision the height no longer can.
 */
export function waitingBandHeight(millis: number): number {
  const minutes = Math.max(0, millis) / 60_000;
  const raw = 12 + 26 * Math.log2(1 + minutes);
  return Math.round(Math.min(112, Math.max(12, raw)));
}

/** `1h 04m 09s` / `4m 09s` / `9s`. */
function formatDuration(millis: number): string {
  const s = Math.floor(millis / 1000);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  if (h > 0) return `${h}h ${String(m).padStart(2, "0")}m ${String(sec).padStart(2, "0")}s`;
  if (m > 0) return `${m}m ${String(sec).padStart(2, "0")}s`;
  return `${sec}s`;
}

function timeOf(at: number): string {
  return new Date(at).toLocaleTimeString(undefined, {
    hour: "numeric",
    minute: "2-digit",
    second: "2-digit",
  });
}

export function TaskDetailView({
  client,
  company,
  taskId,
  onBack,
  onNavigate,
  onOpenThread,
  onSaved,
  onDeleted,
}: {
  client: OpenCompanyClient;
  company: string | null;
  taskId: string;
  /** Return to the board. */
  onBack: () => void;
  /** Navigate the detail to a neighbouring (lineage) task. */
  onNavigate: (id: string) => void;
  /** Open the chat thread this card was created from (issue #246). */
  onOpenThread?: (threadId: string) => void;
  /** Hand a saved card back to the board for reconciliation. */
  onSaved: (t: Task) => void;
  /** Tell the board a card was deleted. */
  onDeleted: (id: string) => void;
}) {
  const [detail, setDetail] = useState<TaskDetail | null>(null);
  const [inflight, setInflight] = useState<InflightRun | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [notFound, setNotFound] = useState(false);
  const [editing, setEditing] = useState(false);
  const [now, setNow] = useState(() => Date.now());

  // `isActive` is a per-effect-run token, not a shared ref: a superseded run
  // (e.g. taskId A→B) flips its own token to false so an in-flight `load()` from
  // run A can never apply task A's data while the screen is showing task B.
  const load = useCallback(
    async (isActive: () => boolean = () => true) => {
      try {
        const [d, runs] = await Promise.all([
          getTaskDetail(client, company, taskId),
          listInflight(client, company).catch(() => [] as InflightRun[]),
        ]);
        if (!isActive()) return;
        setDetail(d);
        setInflight(runs.find((r) => r.taskId === taskId) ?? null);
        setNotFound(false);
        setError(null);
      } catch (e) {
        if (!isActive()) return;
        if (e instanceof ApiError && e.status === 404) {
          setNotFound(true);
          setError(null);
        } else {
          setError(e instanceof Error ? e.message : "could not load the task");
        }
      } finally {
        if (isActive()) setLoading(false);
      }
    },
    [client, company, taskId],
  );

  // 4s poll, paused while the tab is hidden and resumed (with an immediate
  // fetch) when it returns to the foreground. Re-keys on `taskId`, so a lineage
  // navigation reloads the screen for the new card.
  useEffect(() => {
    let cancelled = false;
    const isActive = () => !cancelled;
    setLoading(true);
    setDetail(null);
    void load(isActive);
    let timer: number | undefined;
    const stop = () => {
      if (timer !== undefined) {
        window.clearInterval(timer);
        timer = undefined;
      }
    };
    const start = () => {
      if (timer === undefined) timer = window.setInterval(() => void load(isActive), POLL_MS);
    };
    const onVisibility = () => {
      if (document.visibilityState === "hidden") {
        stop();
      } else {
        void load(isActive);
        start();
      }
    };
    if (document.visibilityState !== "hidden") start();
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      cancelled = true;
      stop();
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [load]);

  const worked = useMemo(
    () => (detail ? workedMillis(detail.timeline, now) : null),
    [detail, now],
  );
  const waiting = useMemo(
    () => (detail ? waitingMillis(detail.timeline, detail.waitingSince, now) : null),
    [detail, now],
  );

  // Only tick the 1s clock while something is actually running: a dispatch
  // window is open, the task is parked on an operator right now, or an attempt
  // has not settled (#242 — a parked attempt's clock is still running, which is
  // why this asks the run's phase and not its finish time).
  const ticking =
    Boolean(worked?.live) ||
    Boolean(waiting?.live) ||
    Boolean(detail?.runs.some(isRunOpen));
  useEffect(() => {
    if (!ticking) return;
    const timer = window.setInterval(() => {
      if (document.visibilityState !== "hidden") setNow(Date.now());
    }, 1000);
    return () => window.clearInterval(timer);
  }, [ticking]);

  if (notFound) {
    return (
      <div className="flex flex-1 flex-col items-center justify-center gap-3 p-8 text-center">
        <p className="text-sm font-medium">This task no longer exists.</p>
        <p className="max-w-sm text-xs text-muted-foreground">
          It may have been deleted. Head back to the board to pick another card.
        </p>
        <Button variant="outline" size="sm" onClick={onBack}>
          <ArrowLeft className="mr-1.5 size-4" />
          Back to board
        </Button>
      </div>
    );
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-hidden">
      <div className="flex items-center gap-2 border-b px-4 py-3">
        <Button variant="ghost" size="sm" className="-ml-2 h-8 px-2" onClick={onBack}>
          <ArrowLeft className="mr-1.5 size-4" />
          Board
        </Button>
        <span className="text-xs text-muted-foreground">Task detail</span>
      </div>

      {error && (
        <div className="px-4 pt-3">
          <Alert variant="destructive">
            <AlertDescription>{error}</AlertDescription>
          </Alert>
        </div>
      )}

      {loading && !detail ? (
        <div className="space-y-4 p-4">
          <Skeleton className="h-24 rounded-xl" />
          <Skeleton className="h-10 w-64 rounded-lg" />
          <Skeleton className="h-48 rounded-xl" />
        </div>
      ) : detail ? (
        <ScrollArea className="min-h-0 flex-1">
          <div className="mx-auto w-full max-w-3xl space-y-5 p-4">
            <DetailHeader task={detail.task} worked={worked} waiting={waiting} />

            <ControlBar
              task={detail.task}
              inflight={inflight}
              client={client}
              company={company}
              onChanged={load}
              onSaved={onSaved}
              onEdit={() => setEditing(true)}
            />

            <OriginThreadRow
              originChatId={detail.task.originChatId}
              onOpenThread={onOpenThread}
            />

            <LineageRail lineage={detail.lineage} onNavigate={onNavigate} />

            <Tabs defaultValue="timeline">
              <TabsList>
                <TabsTrigger value="timeline">Timeline</TabsTrigger>
                <TabsTrigger value="attempts">
                  Attempts
                  {detail.runs.length > 0 && (
                    <span className="ml-1.5 tabular-nums text-muted-foreground">
                      {detail.runs.length}
                    </span>
                  )}
                </TabsTrigger>
                <TabsTrigger value="approvals">Approvals</TabsTrigger>
                <TabsTrigger value="artifacts">Artifacts</TabsTrigger>
                <TabsTrigger value="discussion">Discussion</TabsTrigger>
              </TabsList>

              <TabsContent value="timeline" className="mt-4">
                <TimelineList
                  entries={detail.timeline}
                  waitingSince={detail.waitingSince}
                  now={now}
                />
              </TabsContent>

              <TabsContent value="attempts" className="mt-4">
                <AttemptsTab
                  client={client}
                  company={company}
                  runs={detail.runs}
                  now={now}
                />
              </TabsContent>

              <TabsContent value="approvals" className="mt-4">
                <ApprovalsTab entries={detail.timeline} />
              </TabsContent>

              <TabsContent value="artifacts" className="mt-4">
                <ArtifactsTab client={client} company={company} taskId={detail.task.id} />
              </TabsContent>

              <TabsContent value="discussion" className="mt-4">
                {/* No backend for per-task discussion yet — honest empty state,
                    no fake threads. */}
                <EmptyState
                  title="No discussion yet"
                  body="A per-task discussion thread has no backend yet."
                />
              </TabsContent>
            </Tabs>
          </div>
        </ScrollArea>
      ) : null}

      <TaskEditDialog
        task={editing && detail ? detail.task : null}
        onClose={() => setEditing(false)}
        onSaved={(t) => {
          onSaved(t);
          setEditing(false);
          void load();
        }}
        onDeleted={(id) => {
          onDeleted(id);
          onBack();
        }}
        client={client}
        company={company}
      />
    </div>
  );
}

function DetailHeader({
  task,
  worked,
  waiting,
}: {
  task: Task;
  worked: { millis: number; live: boolean } | null;
  waiting: { millis: number; live: boolean } | null;
}) {
  const hasDispatch = worked !== null && (worked.millis > 0 || worked.live);
  // `worked` is the whole elapsed run window; waiting sits *inside* it, so
  // working time is the remainder. Clamped at zero because the two figures come
  // from different sources (event log vs journal join) and a clock skew between
  // them must never render a negative duration.
  const waitedMs = waiting?.millis ?? 0;
  const workingMs = Math.max(0, (worked?.millis ?? 0) - waitedMs);
  // The acceptance line: a task that never waited shows no waiting figure at
  // all, not a "Waiting 0s".
  const showWaiting = waitedMs > 0 || Boolean(waiting?.live);
  return (
    <div className="rounded-xl border bg-card p-4">
      <div className="flex items-start justify-between gap-3">
        <h2 className="text-lg font-semibold leading-snug">{task.title}</h2>
        <Badge
          variant="outline"
          className={cn("shrink-0 capitalize", priorityStyle(task.priority))}
        >
          {task.priority}
        </Badge>
      </div>

      <div className="mt-3 flex flex-wrap items-center gap-x-4 gap-y-2 text-xs text-muted-foreground">
        <span className="inline-flex items-center gap-1.5">
          <span className="font-medium text-foreground">Status</span>
          <Badge variant="secondary" className="font-normal">
            {columnLabel(task.column)}
          </Badge>
        </span>
        {task.assignee && (
          <span className="inline-flex items-center gap-1.5">
            <span
              className="flex size-5 items-center justify-center rounded-full bg-muted text-[10px] font-semibold"
              aria-hidden
            >
              {initials(task.assignee)}
            </span>
            {task.assignee}
          </span>
        )}
        <span className="inline-flex items-center gap-1.5">
          <Clock className="size-3.5" />
          {hasDispatch ? (
            <>
              <span className="font-medium text-foreground">
                Worked {formatDuration(workingMs)}
              </span>
              {worked!.live && (
                <span className="inline-flex items-center gap-1 text-emerald-600 dark:text-emerald-400">
                  <span className="size-1.5 animate-pulse rounded-full bg-current" aria-hidden />
                  live
                </span>
              )}
            </>
          ) : (
            <span>Not yet dispatched</span>
          )}
        </span>
        {showWaiting && (
          <span className="inline-flex items-center gap-1.5">
            <Hourglass className="size-3.5 text-amber-600 dark:text-amber-400" />
            <span className="font-medium text-amber-700 dark:text-amber-400">
              Waiting {formatDuration(waitedMs)}
            </span>
            {waiting!.live && (
              <span className="inline-flex items-center gap-1 text-amber-600 dark:text-amber-400">
                <span className="size-1.5 animate-pulse rounded-full bg-current" aria-hidden />
                on you
              </span>
            )}
          </span>
        )}
      </div>

      {task.note && (
        <p className="mt-3 whitespace-pre-wrap border-t pt-3 text-xs text-muted-foreground">
          {task.note}
        </p>
      )}

      {/* Each waiting span itself is exact — a real park instant to a real
          resolution. What stays approximate is which task an approval belonged
          to: parked effects carry no task id, so they are correlated to the run
          window. Said plainly rather than left for a reader to discover. */}
      {showWaiting && (
        <p className="mt-2 text-[11px] text-muted-foreground/70">
          Waiting is correlated to this task&rsquo;s run window — approvals are not linked
          per-task.
        </p>
      )}
    </div>
  );
}

function ControlBar({
  task,
  inflight,
  client,
  company,
  onChanged,
  onSaved,
  onEdit,
}: {
  task: Task;
  inflight: InflightRun | null;
  client: OpenCompanyClient;
  company: string | null;
  onChanged: () => Promise<void> | void;
  onSaved: (t: Task) => void;
  onEdit: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [redirecting, setRedirecting] = useState(false);
  const [instruction, setInstruction] = useState("");
  const [reassigning, setReassigning] = useState(false);
  const [assignee, setAssignee] = useState(task.assignee);

  const pending = inflight?.pendingAction ?? null;
  const steerDisabled = busy || pending !== null;

  async function steer(action: SteerAction, opts?: { instruction?: string; confirm?: boolean }) {
    if (!inflight) return;
    setBusy(true);
    try {
      await steerTask(client, company, inflight.key, { action, ...opts });
      setRedirecting(false);
      setInstruction("");
      await onChanged();
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "could not steer the task");
    } finally {
      setBusy(false);
    }
  }

  async function patchColumn() {
    setBusy(true);
    try {
      const saved = await patchTask(client, company, task.id, { column: "in_progress" });
      onSaved(saved);
      await onChanged();
      toast.success("Dispatched — the assignee is working on it.");
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "could not dispatch the task");
    } finally {
      setBusy(false);
    }
  }

  async function saveAssignee() {
    const next = assignee.trim();
    // Only an unchanged value is a no-op. Blank used to short-circuit here too,
    // which made the one deliberate way to hand a card back to the orchestrator
    // unreachable from this screen — the host accepts it happily
    // (`resolve("") -> Unassigned -> canonical ""`), the row just never sent it.
    if (next === task.assignee) {
      setReassigning(false);
      return;
    }
    setBusy(true);
    try {
      const saved = await patchTask(client, company, task.id, { assignee: next });
      onSaved(saved);
      await onChanged();
      setReassigning(false);
      toast.success("Reassigned.");
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "could not reassign the task");
    } finally {
      setBusy(false);
    }
  }

  const resumeLabel = task.column === "paused" ? "Resume" : "Retry";

  return (
    <div className="rounded-xl border bg-card/40 p-3">
      <div className="flex flex-wrap items-center gap-2">
        {inflight ? (
          <>
            <span className="mr-1 inline-flex items-center gap-1.5 text-xs font-medium text-emerald-600 dark:text-emerald-400">
              <span className="size-1.5 animate-pulse rounded-full bg-current" aria-hidden />
              In flight
            </span>
            {pending !== null ? (
              <span className="rounded-full bg-muted px-2 py-0.5 text-[11px] font-medium text-muted-foreground">
                {pending}…
              </span>
            ) : (
              <>
                <Button
                  variant="outline"
                  size="sm"
                  className="h-8"
                  disabled={steerDisabled}
                  onClick={() => void steer("pause")}
                >
                  <Square className="mr-1.5 size-3.5" />
                  Stop
                </Button>
                <Button
                  variant="outline"
                  size="sm"
                  className="h-8"
                  disabled={steerDisabled}
                  aria-pressed={redirecting}
                  onClick={() => setRedirecting((r) => !r)}
                >
                  <CornerUpRight className="mr-1.5 size-3.5" />
                  Redirect
                </Button>
                <ConfirmButton
                  trigger={
                    <Button variant="outline" size="sm" className="h-8" disabled={steerDisabled}>
                      <Ban className="mr-1.5 size-3.5" />
                      Cancel
                    </Button>
                  }
                  title={`Cancel “${task.title}”?`}
                  description="This stops the run. It can be retried afterwards from the board or here."
                  confirmLabel="Cancel run"
                  destructive
                  onConfirm={() => void steer("cancel", { confirm: true })}
                />
              </>
            )}
          </>
        ) : (
          <Button variant="outline" size="sm" className="h-8" disabled={busy} onClick={() => void patchColumn()}>
            <Play className="mr-1.5 size-3.5" />
            {resumeLabel}
          </Button>
        )}

        <div className="ml-auto flex items-center gap-2">
          {busy && <Loader2 className="size-4 animate-spin text-muted-foreground" />}
          <Button
            variant="ghost"
            size="sm"
            className="h-8"
            disabled={busy}
            aria-pressed={reassigning}
            onClick={() => {
              setAssignee(task.assignee);
              setReassigning((r) => !r);
            }}
          >
            <UserCog className="mr-1.5 size-3.5" />
            Reassign
          </Button>
          <Button variant="ghost" size="sm" className="h-8" disabled={busy} onClick={onEdit}>
            <Pencil className="mr-1.5 size-3.5" />
            Edit
          </Button>
        </div>
      </div>

      {redirecting && (
        <div className="mt-2 flex items-center gap-2">
          <Input
            autoFocus
            value={instruction}
            placeholder="New instruction for the run…"
            className="h-8"
            disabled={steerDisabled}
            onChange={(e) => setInstruction(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && instruction.trim()) void steer("redirect", { instruction: instruction.trim() });
            }}
          />
          <Button
            size="sm"
            className="h-8"
            disabled={steerDisabled || !instruction.trim()}
            onClick={() => void steer("redirect", { instruction: instruction.trim() })}
          >
            <Send className="mr-1.5 size-3.5" />
            Send
          </Button>
        </div>
      )}

      {reassigning && (
        <div className="mt-2 flex items-center gap-2">
          {/* Issue #263: the same roster picker the edit dialog uses. Since
              #301 this is the *only* place a card gets an assignee — the create
              box asks for a prompt and nothing else, so the host's default
              (unassigned → orchestrator) stands until somebody picks here.
              Unassigned is a row here rather than an empty field, so handing the
              card back to the orchestrator is something you can see and choose. */}
          <AssigneeSelect
            client={client}
            company={company}
            value={assignee}
            onChange={setAssignee}
            disabled={busy}
            className="h-8 min-w-0 flex-1"
          />
          <Button size="sm" className="h-8" disabled={busy} onClick={() => void saveAssignee()}>
            Save
          </Button>
        </div>
      )}
    </div>
  );
}

/**
 * "Opened from a conversation" — the card → chat half of issue #246.
 *
 * `originChatId` has been on the record since #151, but nothing ever read it
 * back, so a card created out of a conversation looked exactly like one typed
 * onto the board. Renders nothing for a card with no conversation behind it,
 * which is every card the `+` button creates.
 *
 * Falls back to plain text when no navigation callback is supplied: stating the
 * origin is the part that must always work; the jump is a convenience the host
 * screen may or may not be able to offer.
 */
function OriginThreadRow({
  originChatId,
  onOpenThread,
}: {
  originChatId?: string;
  onOpenThread?: (threadId: string) => void;
}) {
  if (!originChatId) return null;
  const label = "Opened from chat";
  if (!onOpenThread) {
    return (
      <div className="flex items-center gap-2 rounded-xl border bg-card/40 px-3 py-2 text-xs text-muted-foreground">
        <MessagesSquare className="size-3.5 shrink-0" />
        <span className="truncate">{label}</span>
      </div>
    );
  }
  return (
    <button
      className="flex w-full items-center gap-2 rounded-xl border bg-card/40 px-3 py-2 text-left text-xs transition-colors hover:bg-accent"
      onClick={() => onOpenThread(originChatId)}
    >
      <MessagesSquare className="size-3.5 shrink-0 text-muted-foreground" />
      <span className="min-w-0 flex-1 truncate">{label}</span>
      <span className="shrink-0 text-muted-foreground">Open the conversation</span>
    </button>
  );
}

function LineageRail({
  lineage,
  onNavigate,
}: {
  lineage: TaskDetail["lineage"];
  onNavigate: (id: string) => void;
}) {
  if (!lineage.parent && lineage.children.length === 0) return null;
  return (
    <div className="rounded-xl border bg-card/40 p-3">
      <p className="mb-2 text-[11px] font-medium uppercase tracking-wide text-muted-foreground">
        Lineage
      </p>
      <div className="space-y-1.5">
        {lineage.parent && (
          <button
            className="flex w-full items-center gap-2 rounded-lg border bg-card px-2.5 py-1.5 text-left text-xs transition-colors hover:bg-accent"
            onClick={() => onNavigate(lineage.parent!.id)}
          >
            <CornerUpLeft className="size-3.5 shrink-0 text-muted-foreground" />
            <span className="min-w-0 flex-1 truncate">{lineage.parent.title}</span>
            <Badge variant="secondary" className="shrink-0 font-normal">
              {columnLabel(lineage.parent.column)}
            </Badge>
          </button>
        )}
        <div className="px-2.5 py-1 text-[11px] font-medium text-muted-foreground">This task</div>
        {lineage.children.map((child) => (
          <button
            key={child.id}
            className="flex w-full items-center gap-2 rounded-lg border bg-card px-2.5 py-1.5 pl-5 text-left text-xs transition-colors hover:bg-accent"
            onClick={() => onNavigate(child.id)}
          >
            <CornerDownRight className="size-3.5 shrink-0 text-muted-foreground" />
            <span className="min-w-0 flex-1 truncate">{child.title}</span>
            <Badge variant="secondary" className="shrink-0 font-normal">
              {columnLabel(child.column)}
            </Badge>
          </button>
        ))}
      </div>
    </div>
  );
}

/** One rendered timeline row — a single entry, or a run of grouped failures. */
interface TimelineGroup {
  key: string;
  kind: TimelineKind;
  /** A run step's outcome (#242); absent for journal-derived entries. */
  status?: StepStatus;
  label: string;
  count: number;
  entries: TimelineEntry[];
}

/**
 * One item in the rendered timeline: an event row, or a waiting band (#305).
 *
 * The band is not an event — nothing is journaled while the company waits — so
 * it cannot be a `TimelineGroup`. Making the list a union keeps the band's
 * variable height out of the row renderer entirely.
 */
type TimelineItem =
  | { row: "group"; key: string; group: TimelineGroup }
  | { row: "wait"; key: string; millis: number; live: boolean };

/**
 * Folds a timeline into rows, coalescing consecutive same-label `tool_failed`
 * entries into one `×N` row. Every other kind is its own row.
 *
 * Waiting bands (#305) are spliced in *before* the approval row that ended the
 * wait — the band is the pause that led to the decision, so it reads in that
 * order — and a live band is appended at the foot when the task is parked on an
 * operator right now. Approvals are never coalesced, so no band can land inside
 * a `×N` group.
 */
function groupTimeline(
  entries: TimelineEntry[],
  waitingSince?: number,
  now: number = Date.now(),
): TimelineItem[] {
  const groups: TimelineGroup[] = [];
  for (const e of entries) {
    const last = groups[groups.length - 1];
    if (
      isFailureRow(e) &&
      last &&
      last.kind === e.kind &&
      last.label === e.label &&
      isFailureRow(last.entries[last.entries.length - 1])
    ) {
      last.count += 1;
      last.entries.push(e);
    } else {
      groups.push({
        key: String(e.seq),
        kind: e.kind,
        status: e.status,
        label: e.label,
        count: 1,
        entries: [e],
      });
    }
  }

  const items: TimelineItem[] = [];
  for (const g of groups) {
    const waited = g.kind === "approval" ? g.entries[0].waitedMillis : undefined;
    if (waited !== undefined && waited > 0) {
      // `wait-` prefixed so a band can never collide with a row key, which is
      // the bare sequence number.
      items.push({ row: "wait", key: `wait-${g.entries[0].seq}`, millis: waited, live: false });
    }
    items.push({ row: "group", key: g.key, group: g });
  }
  if (waitingSince !== undefined) {
    items.push({
      row: "wait",
      key: "wait-live",
      millis: Math.max(0, now - waitingSince),
      live: true,
    });
  }
  return items;
}

const KIND_ICON: Record<TimelineKind, ReactElement> = {
  dispatched: <Play className="size-3.5" />,
  reply: <MessageSquare className="size-3.5" />,
  tool_failed: <AlertCircle className="size-3.5" />,
  approval: <ShieldCheck className="size-3.5" />,
  completed: <CheckCircle2 className="size-3.5" />,
  // The run-trace kinds (#242). Same renderer, three more icon rows.
  tool_call: <Wrench className="size-3.5" />,
  thinking: <Brain className="size-3.5" />,
  note: <StickyNote className="size-3.5" />,
};

/**
 * The icon for a row. A run step's **outcome** outranks its kind (#242): a
 * failed tool call reads as a failure, and one still in flight reads as a
 * spinner — which is the honest render of a step the trace recorded as
 * `running` because the host died mid-call, not an error.
 *
 * Task-timeline entries carry no `status`, so they fall through to the kind
 * icon exactly as before.
 */
function rowIcon(kind: TimelineKind, status?: StepStatus): ReactElement {
  if (status === "running") return <Loader2 className="size-3.5 animate-spin" />;
  if (status === "error") return <AlertCircle className="size-3.5" />;
  return KIND_ICON[kind];
}

function kindTone(kind: TimelineKind, status?: StepStatus): string {
  // Outcome first, for the same reason `rowIcon` reads it first.
  if (status === "running") return "text-sky-600 dark:text-sky-400";
  if (status === "error") return "text-rose-600 dark:text-rose-400";
  switch (kind) {
    case "completed":
      return "text-emerald-600 dark:text-emerald-400";
    case "tool_failed":
      return "text-rose-600 dark:text-rose-400";
    case "approval":
      return "text-amber-600 dark:text-amber-400";
    default:
      return "text-muted-foreground";
  }
}

/**
 * Whether a row is a failure, from either surface: the journal's `tool_failed`
 * kind, or a run step whose recorded outcome was an error (#242). This is what
 * the `×N` coalescing keys on, so a tool that failed six times in a row reads
 * the same in a run's trace as it does on the task timeline.
 */
function isFailureRow(entry: TimelineEntry): boolean {
  return entry.kind === "tool_failed" || entry.status === "error";
}

function TimelineList({
  entries,
  waitingSince,
  now,
}: {
  entries: TimelineEntry[];
  waitingSince?: number;
  now: number;
}) {
  const items = useMemo(
    () => groupTimeline(entries, waitingSince, now),
    [entries, waitingSince, now],
  );
  if (items.length === 0) {
    return (
      <EmptyState
        title="Nothing has happened yet"
        body="Dispatch this task from the board to start its timeline."
      />
    );
  }
  return (
    <ol className="space-y-1.5">
      {items.map((item) =>
        item.row === "wait" ? (
          <WaitingBand key={item.key} millis={item.millis} live={item.live} />
        ) : (
          <TimelineRow key={item.key} group={item.group} />
        ),
      )}
    </ol>
  );
}

/**
 * A waiting period, rendered as space rather than as another uniform row (#305).
 *
 * This is the acceptance criterion the timeline exists for: a four-hour wait and
 * a four-second wait must not look alike. The height carries the comparison at a
 * glance; the printed duration carries the exact figure, including past the
 * point the height saturates.
 */
function WaitingBand({ millis, live }: { millis: number; live: boolean }) {
  // Height is quantised to the 4s poll while the band is live, so a 1s text tick
  // does not relayout the list underneath the reader's cursor every second.
  const height = waitingBandHeight(live ? Math.round(millis / 4000) * 4000 : millis);
  return (
    <li
      className={cn(
        "flex items-center justify-center gap-1.5 rounded-lg border border-dashed",
        "border-amber-400/60 bg-amber-50/60 text-[11px] text-amber-700",
        "dark:border-amber-500/40 dark:bg-amber-950/20 dark:text-amber-400",
        live && "animate-pulse",
      )}
      style={{ minHeight: height }}
      aria-label={`Waiting on a human for ${formatDuration(millis)}`}
    >
      <Hourglass className="size-3.5 shrink-0" aria-hidden />
      <span className="font-medium tabular-nums">
        {live ? `Waiting on you · ${formatDuration(millis)}` : `Waited ${formatDuration(millis)}`}
      </span>
    </li>
  );
}

function TimelineRow({ group }: { group: TimelineGroup }) {
  const [open, setOpen] = useState(false);
  const details = group.entries.filter((e) => e.detail);
  const expandable = details.length > 0 || group.count > 1;
  const first = group.entries[0];

  return (
    <li className="rounded-lg border bg-card">
      <button
        className={cn(
          "flex w-full items-center gap-2 px-3 py-2 text-left text-xs",
          expandable ? "cursor-pointer" : "cursor-default",
        )}
        disabled={!expandable}
        onClick={() => expandable && setOpen((o) => !o)}
      >
        <span className={cn("shrink-0", kindTone(group.kind, group.status))}>
          {rowIcon(group.kind, group.status)}
        </span>
        <span className="min-w-0 flex-1 truncate font-medium">{group.label}</span>
        {group.count > 1 && (
          <Badge variant="outline" className="shrink-0 font-normal">
            ×{group.count}
          </Badge>
        )}
        {/* A run step's own duration (#242); journal entries carry none. */}
        {group.count === 1 && first.elapsedMs !== undefined && (
          <span className="shrink-0 text-[11px] tabular-nums text-muted-foreground">
            {first.elapsedMs < 1000 ? `${first.elapsedMs}ms` : formatDuration(first.elapsedMs)}
          </span>
        )}
        <span className="shrink-0 text-[11px] tabular-nums text-muted-foreground">
          {timeOf(first.atMillis)}
        </span>
        {expandable &&
          (open ? (
            <ChevronDown className="size-3.5 shrink-0 text-muted-foreground" />
          ) : (
            <ChevronRight className="size-3.5 shrink-0 text-muted-foreground" />
          ))}
      </button>
      {open && expandable && (
        <div className="space-y-2 border-t px-3 py-2">
          {group.count > 1
            ? group.entries.map((e) => (
                <div key={e.seq} className="text-[11px]">
                  <div className="mb-0.5 text-muted-foreground">{timeOf(e.atMillis)}</div>
                  {e.detail && (
                    <pre className="whitespace-pre-wrap break-words font-mono text-[11px] text-muted-foreground">
                      {e.detail}
                    </pre>
                  )}
                </div>
              ))
            : details.map((e) => (
                <pre
                  key={e.seq}
                  className="whitespace-pre-wrap break-words font-mono text-[11px] text-muted-foreground"
                >
                  {e.detail}
                </pre>
              ))}
        </div>
      )}
    </li>
  );
}

// ---------------------------------------------------------------------------
// Attempts (#242)
// ---------------------------------------------------------------------------

/**
 * The tone of a run's status chip. `waiting_approval` and `paused` share the
 * amber "parked" tone the waiting band already uses — they differ in *who*
 * unblocks them, not in whether the company is stuck.
 */
function runStatusTone(status: RunStatus): string {
  switch (status) {
    case "succeeded":
      return "border-emerald-500/40 text-emerald-700 dark:text-emerald-400";
    case "failed":
      return "border-rose-500/40 text-rose-700 dark:text-rose-400";
    case "cancelled":
      return "border-muted-foreground/30 text-muted-foreground";
    case "waiting_approval":
    case "paused":
      return "border-amber-500/40 text-amber-700 dark:text-amber-400";
    default:
      return "border-sky-500/40 text-sky-700 dark:text-sky-400";
  }
}

/**
 * The card's recorded attempts (#242) — the thing that makes a task which
 * failed twice and succeeded on the third try look different from one that
 * succeeded immediately.
 *
 * Cost is deliberately absent: epic #184 scopes this screen with the
 * cost/currency dimension removed (no per-line cost, no total-cost header), and
 * a per-attempt USD figure is exactly a per-line cost. The usage totals are on
 * the wire for the surfaces that own them.
 */
function AttemptsTab({
  client,
  company,
  runs,
  now,
}: {
  client: OpenCompanyClient;
  company: string | null;
  runs: RunSummary[];
  now: number;
}) {
  const [openId, setOpenId] = useState<string | null>(null);
  const open = useMemo(() => runs.find((r) => r.id === openId) ?? null, [runs, openId]);

  if (runs.length === 0) {
    return (
      <EmptyState
        title="No recorded attempts"
        body="Dispatch this card to record one. Cards dispatched before attempts were recorded show none — they were never backfilled, because inventing them would fabricate a record."
      />
    );
  }

  return (
    <div className="space-y-3">
      <p className="text-xs text-muted-foreground">
        Every dispatch of this card, newest first. A card can enter review more than once, so
        several waits on one attempt is the expected record, not a fault.
      </p>
      <ol className="space-y-1.5">
        {runs.map((run) => (
          <AttemptRow key={run.id} run={run} now={now} onOpen={() => setOpenId(run.id)} />
        ))}
      </ol>
      <RunDrawer
        client={client}
        company={company}
        run={open}
        now={now}
        onClose={() => setOpenId(null)}
      />
    </div>
  );
}

/**
 * What to say about an attempt's step count, which is **written on the settle**
 * and so reads `0` for the whole of a live run.
 *
 * Three honest cases rather than one misleading number:
 * - never started (`pending`) — nothing has been traced, and nothing is being
 *   traced either, so it does not claim to be recording;
 * - open but unsettled — steps *are* landing incrementally (the drawer shows
 *   them), the count just has not been written yet;
 * - settled — the real figure, marked `+` when it is a capped high-water
 *   ordinal rather than a total.
 */
function stepSummary(run: RunSummary): string {
  if (run.startedAtMillis === undefined) return "not started";
  if (isRunOpen(run) && run.stepCount === 0) return "recording…";
  const n = run.stepCount;
  return `${n}${run.stepCountCapped ? "+" : ""} step${n === 1 ? "" : "s"}`;
}

function AttemptRow({
  run,
  now,
  onOpen,
}: {
  run: RunSummary;
  now: number;
  onOpen: () => void;
}) {
  const elapsed = runElapsedMillis(run, now);
  return (
    <li className="rounded-lg border bg-card">
      <button
        className="flex w-full cursor-pointer flex-col gap-1 px-3 py-2 text-left"
        onClick={onOpen}
      >
        <div className="flex w-full items-center gap-2 text-xs">
          <Layers className="size-3.5 shrink-0 text-muted-foreground" />
          <span className="shrink-0 font-medium">Attempt {run.attempt}</span>
          <Badge variant="outline" className={cn("shrink-0 font-normal", runStatusTone(run.status))}>
            {RUN_STATUS_LABEL[run.status]}
          </Badge>
          <span className="min-w-0 flex-1 truncate text-muted-foreground">{run.agentId}</span>
          {elapsed !== null && (
            <span
              className={cn(
                "shrink-0 tabular-nums text-[11px] text-muted-foreground",
                isRunOpen(run) && "text-foreground",
              )}
            >
              {formatDuration(elapsed)}
              {isRunOpen(run) && " …"}
            </span>
          )}
          <ChevronRight className="size-3.5 shrink-0 text-muted-foreground" />
        </div>
        <div className="flex w-full items-center gap-2 pl-5 text-[11px] text-muted-foreground">
          <span className="tabular-nums">{timeOf(run.createdAtMillis)}</span>
          <span aria-hidden>·</span>
          <span>{stepSummary(run)}</span>
          {run.stepCountCapped && <span>(trace capped)</span>}
        </div>
        {run.error && (
          <p className="w-full truncate pl-5 text-[11px] text-rose-600 dark:text-rose-400">
            {run.error}
          </p>
        )}
      </button>
    </li>
  );
}

/**
 * One attempt's persisted step trace, in a side drawer (#242).
 *
 * **Refresh-on-read.** Steps land in the store *as the turn executes*, so
 * re-reading an open attempt shows the progress made since — which is why this
 * re-fetches on the same cadence as the screen while the run has not settled,
 * and stops once it has. A live stream would mean widening the harness turn
 * stream for something a re-read already answers.
 */
function RunDrawer({
  client,
  company,
  run,
  now,
  onClose,
}: {
  client: OpenCompanyClient;
  company: string | null;
  run: RunSummary | null;
  now: number;
  onClose: () => void;
}) {
  const [detail, setDetail] = useState<RunDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const runId = run?.id ?? null;
  // Re-fetch while the attempt is unsettled. Read from the *summary* the parent
  // poll refreshes, so the drawer stops polling as soon as the run settles even
  // if its own last read still showed it open.
  const live = run !== null && isRunOpen(run);

  useEffect(() => {
    if (runId === null) {
      setDetail(null);
      setError(null);
      return;
    }
    let cancelled = false;
    const read = async () => {
      try {
        const next = await getRun(client, company, runId);
        if (!cancelled) {
          setDetail(next);
          setError(null);
        }
      } catch (e) {
        if (!cancelled) setError(e instanceof Error ? e.message : "could not load the attempt");
      }
    };
    void read();
    if (!live) return () => void (cancelled = true);
    const timer = window.setInterval(() => {
      if (document.visibilityState !== "hidden") void read();
    }, POLL_MS);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [client, company, runId, live]);

  const elapsed = run ? runElapsedMillis(run, now) : null;

  return (
    <Sheet open={run !== null} onOpenChange={(next) => !next && onClose()}>
      <SheetContent side="right" className="w-full sm:max-w-md">
        {run && (
          <>
            <SheetHeader className="border-b">
              <SheetTitle>Attempt {run.attempt}</SheetTitle>
              <SheetDescription className="flex flex-wrap items-center gap-1.5 text-xs">
                <Badge
                  variant="outline"
                  className={cn("font-normal", runStatusTone(run.status))}
                >
                  {RUN_STATUS_LABEL[run.status]}
                </Badge>
                <span>{run.agentId}</span>
                {elapsed !== null && (
                  <>
                    <span aria-hidden>·</span>
                    <span className="tabular-nums">{formatDuration(elapsed)}</span>
                  </>
                )}
              </SheetDescription>
            </SheetHeader>
            <ScrollArea className="min-h-0 flex-1">
              <div className="space-y-3 px-4 pb-4">
                {run.error && (
                  <Alert variant="destructive">
                    <AlertDescription className="text-xs">{run.error}</AlertDescription>
                  </Alert>
                )}
                {error && (
                  <Alert variant="destructive">
                    <AlertDescription className="text-xs">{error}</AlertDescription>
                  </Alert>
                )}
                {run.stepCountCapped && (
                  <p className="text-[11px] text-muted-foreground">
                    This attempt hit the per-run trace ceiling, so what follows is the start of
                    the run, not all of it.
                  </p>
                )}
                {detail === null && error === null ? (
                  <div className="space-y-1.5 pt-1">
                    <Skeleton className="h-9 rounded-lg" />
                    <Skeleton className="h-9 rounded-lg" />
                    <Skeleton className="h-9 rounded-lg" />
                  </div>
                ) : detail && detail.steps.length === 0 ? (
                  <EmptyState
                    title="No steps recorded"
                    body={
                      isRunOpen(run)
                        ? "Steps appear here as the attempt runs."
                        : "This attempt settled without producing a traceable step."
                    }
                  />
                ) : detail ? (
                  /* The same grouped-timeline renderer the task timeline uses —
                     `kind` simply widens to the three step words. */
                  <TimelineList entries={detail.steps} now={now} />
                ) : null}
              </div>
            </ScrollArea>
          </>
        )}
      </SheetContent>
    </Sheet>
  );
}

function ApprovalsTab({ entries }: { entries: TimelineEntry[] }) {
  const approvals = useMemo(() => entries.filter((e) => e.kind === "approval"), [entries]);
  return (
    <div className="space-y-3">
      <p className="text-xs text-muted-foreground">
        Approvals resolved <span className="font-medium text-foreground">during this run</span> —
        correlated to the dispatch window, not linked per-task. Each wait is measured from the
        moment the effect actually parked.
      </p>
      {approvals.length === 0 ? (
        <EmptyState
          title="No approvals in this run"
          body="Sign-offs resolved while this task ran will appear here."
        />
      ) : (
        <ol className="space-y-1.5">
          {approvals.map((e) => (
            <li
              key={e.seq}
              className="flex items-center gap-2 rounded-lg border bg-card px-3 py-2 text-xs"
            >
              <ShieldCheck className="size-3.5 shrink-0 text-amber-600 dark:text-amber-400" />
              <span className="min-w-0 flex-1 truncate font-medium">{e.label}</span>
              {e.waitedMillis !== undefined && (
                <span className="shrink-0 text-[11px] tabular-nums text-amber-700 dark:text-amber-400">
                  waited {formatDuration(e.waitedMillis)}
                </span>
              )}
              <span className="shrink-0 text-[11px] tabular-nums text-muted-foreground">
                {timeOf(e.atMillis)}
              </span>
            </li>
          ))}
        </ol>
      )}
    </div>
  );
}

function EmptyState({ title, body }: { title: string; body: string }) {
  return (
    <div className="rounded-xl border border-dashed py-10 text-center">
      <p className="text-sm font-medium">{title}</p>
      <p className="mx-auto mt-1 max-w-xs text-xs text-muted-foreground">{body}</p>
    </div>
  );
}

/** An AlertDialog-gated button, mirroring the confirm pattern used elsewhere. */
function ConfirmButton({
  trigger,
  title,
  description,
  confirmLabel,
  destructive,
  onConfirm,
}: {
  trigger: ReactElement;
  title: string;
  description: string;
  confirmLabel: string;
  destructive?: boolean;
  onConfirm: () => void;
}) {
  return (
    <AlertDialog>
      <AlertDialogTrigger render={trigger} />
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>{title}</AlertDialogTitle>
          <AlertDialogDescription>{description}</AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel>Keep running</AlertDialogCancel>
          <AlertDialogAction
            onClick={onConfirm}
            className={destructive ? "bg-destructive text-white hover:bg-destructive/90" : undefined}
          >
            {confirmLabel}
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
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
