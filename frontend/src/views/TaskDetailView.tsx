// The Task Detail screen (v1, #184): the epic's capstone read surface. One
// `GET …/tasks/{id}` (#185/#190) drives a header, the lineage rail, the event
// timeline, and a controls bar; a 4s visibility-gated poll keeps it live while
// the card is open. The Artifacts tab is
// its own self-fetching surface (#306, over #187's routes); Discussion stays an
// honest stub pending its own backend. Export (#352) is a host-rendered
// document the controls bar downloads — the console lays out none of it.

import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactElement,
} from "react";
import {
  AlertCircle,
  ArrowLeft,
  Ban,
  ChevronRight,
  ClipboardList,
  Clock,
  CornerDownRight,
  CornerUpLeft,
  CornerUpRight,
  Download,
  Hourglass,
  Layers,
  Loader2,
  MessagesSquare,
  Pencil,
  Play,
  Send,
  Square,
  Trash2,
  UserCog,
  Workflow,
} from "lucide-react";

import {
  exportTaskRecord,
  getTaskDetail,
  listInflight,
  patchTask,
  postTaskDiscussion,
  redactTaskDiscussion,
  steerTask,
  type DiscussionMessage,
  type InflightRun,
  type IrreversibleEffect,
  type SteerAction,
  type Task,
  type TaskApproval,
  type TaskDetail,
  type TaskPlan,
} from "@/api/tasks";
import {
  getRun,
  isRunOpen,
  runElapsedMillis,
  RUN_STATUS_LABEL,
  type RunDetail,
  type RunSummary,
} from "@/api/runs";
import {
  ApiError,
  type ApprovalSummary,
  type GrantScope,
  type Verdict,
} from "@/api/types";
import type { OpenCompanyClient } from "@/api/client";
import {
  isTaskTab,
  tabForFocus,
  type TaskFocus,
  type TaskTab,
} from "@/lib/task-output";
import { useAskerNames } from "@/components/approval-card";
import { ApprovalRow } from "@/views/chat/ApprovalRow";
import type { DecidedApproval } from "@/views/chat/model";
import {
  blockingTaskApprovals,
  decidingForTask,
  pendingApprovalWait,
  taskApprovalRows,
} from "@/lib/task-approvals";
import { formatDuration, timeOf } from "@/lib/timeline-format";
import { TimelineList, runStatusTone } from "@/views/runs/RunTimeline";
import { startVisiblePolling } from "@/lib/visible-poll";
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
import { PageHeader } from "@/components/page-header";
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
import { Textarea } from "@/components/ui/textarea";
import { cn } from "@/lib/utils";
import { formatUsdCost } from "@/lib/cost";
import { effectDone } from "@/lib/language";

import { labelFor, PRIORITY_STYLES, type TaskColumn } from "@/lib/board-columns";
import { useBoardColumns } from "@/hooks/use-board-columns";
import { toast } from "sonner";
import { ArtifactsTab } from "./ArtifactsTab";
import { TaskPlanBrief, tallyPrerequisites } from "./TaskPlanBrief";
import { AssigneeSelect } from "./AssigneeSelect";
import { TaskEditDialog } from "./TaskEditDialog";
import { TaskWorkflowProposalPanel } from "./TaskWorkflowProposalPanel";

/** How often to re-poll the detail while the screen is open (visibility-gated). */
const POLL_MS = 4000;

function priorityStyle(priority: string): string {
  return (
    PRIORITY_STYLES[priority as keyof typeof PRIORITY_STYLES] ??
    PRIORITY_STYLES.low
  );
}

/**
 * The column id → human label ("working" → "Working").
 *
 * Takes the columns because they come from the `tasks` ledger now rather than
 * from a list this module could read at import time. `labelFor` humanises an
 * id the host has not named — which since issue #1512 includes every *stage*
 * word, since the ledger declares only the three phases — so a card's stage
 * still reads as words when it is passed through here.
 */
function columnLabel(columns: TaskColumn[], column: string): string {
  return labelFor(columns, column);
}

/**
 * The columns a card can sit in without any agent having run it yet — the only
 * ones where "Not yet dispatched" is a true statement (issue #465).
 *
 * The worked figure is reconstructed from journaled `TaskDispatched` events, so
 * a card that never went through the board's dispatch edge has no window to
 * measure however much work it saw. That is most obviously the #442 card opened
 * *by construction* for work handed straight to an agent: it is written through
 * the task store rather than the dispatch path precisely so it cannot re-fire
 * dispatch, so it is legitimately never "dispatched" — and it was worked all the
 * same. Reading a missing window as "not yet dispatched" put that claim beside a
 * status of In review, which is how the reported card managed to contradict
 * itself in a single line.
 *
 * Past these columns the honest answer is that there is no timing to show, not
 * that nothing has happened, so the slot is omitted rather than guessed at. The
 * waiting figure beside it is unaffected and still says what the card is
 * blocked on.
 */
const UNSTARTED_COLUMNS = new Set(["todo", "planning"]);

/**
 * Whether a card has not been dispatched yet, on the stage vocabulary.
 *
 * Reads {@link Task.stage} and falls back to the phase, because since issue
 * #1512 `column` is `pending`/`working`/`done`: `pending` is unstarted, a
 * working card is unstarted only while it is `planning`, and matching on the
 * phase alone would call every in-flight card never-dispatched.
 */
function neverDispatched(task: Task): boolean {
  const stage = task.stage ?? task.column;
  return stage === "pending" || UNSTARTED_COLUMNS.has(stage);
}

/**
 * Extends a host-computed duration to `now` while its span is still open.
 *
 * The worked/waiting arithmetic used to live here *and* in the exporter
 * (`src/server/ops/task_export.rs`), so the screen and the exported record of
 * the same task could disagree with nothing failing. The host now computes the
 * totals once in `TaskDurations` and hands them to whoever reads the task; this
 * is all that is left client-side.
 *
 * The extension is exact rather than an approximation, which is why the merge
 * does not have to be repeated here: every closed span ends in the past, so past
 * `asOf` the only interval still growing is the open one, and it grows second
 * for second. `Math.max(0, …)` guards a client clock behind the host's.
 */
function extend(
  total: number,
  live: boolean,
  asOf: number,
  now: number,
): { millis: number; live: boolean } {
  return { millis: live ? total + Math.max(0, now - asOf) : total, live };
}

// `waitingBandHeight` moved to `@/lib/timeline-format` with the timeline it
// sizes (issue #1573), and is re-exported here so its existing importers — the
// unit tests that pin the curve — keep their path.
export { waitingBandHeight } from "@/lib/timeline-format";

/**
 * A stable empty default for the `parked` prop (issue #883). A `[]` literal in
 * the parameter list is a new array every render, which would re-run the memo
 * that reads it on a screen with a 1s clock.
 */
const EMPTY_PARKED: readonly ApprovalSummary[] = [];
/** Stable defaults, so a screen rendered without the decide bundle does not
 *  churn the row's props on every poll (#1891). */
const EMPTY_DECIDING: ReadonlyMap<string, Verdict> = new Map();
const EMPTY_VERDICTS: Record<string, Verdict> = {};
const EMPTY_DECIDED: Readonly<Record<string, DecidedApproval>> = {};
const EMPTY_FAILED: Record<string, string> = {};

/**
 * The count that rides the Plan tab's trigger, and the colour it wears (#337).
 *
 * Two different signals, never merged into one number. Red is *blocking*: only
 * `missing` earns it, because only `missing` stopped the card. Amber is
 * *unresolved but not blocking* — a prerequisite that will stop for an operator
 * approval, or one the host could not check at all. Showing a single total
 * would either send someone to fix a non-problem or hide a real one behind a
 * colour that says it is fine.
 *
 * Returns `null` for a plan with nothing to report, so the trigger stays a
 * plain word.
 */
function planTabCount(plan: TaskPlan): { count: number; tone: string } | null {
  const { blocking, approval, unchecked } = tallyPrerequisites(plan);
  if (blocking > 0) return { count: blocking, tone: "text-destructive" };
  const unresolved = approval + unchecked;
  if (unresolved > 0) {
    return { count: unresolved, tone: "text-status-blocked-text" };
  }
  return null;
}

export function TaskDetailView({
  client,
  company,
  taskId,
  attemptEventTick,
  focus,
  onTabChange,
  parked = EMPTY_PARKED,
  deciding = EMPTY_DECIDING,
  decided = EMPTY_DECIDED,
  failed = EMPTY_FAILED,
  onDecide,
  onBack,
  onNavigate,
  onOpenThread,
  onSaved,
  onDeleted,
}: {
  client: OpenCompanyClient;
  company: string | null;
  taskId: string;
  /**
   * Bumped on every `run_status_changed` (issue #1015) — the push half of this
   * screen's liveness, beside the poll below rather than instead of it.
   */
  attemptEventTick?: number;
  /**
   * The company's parked approvals, for naming what this card is waiting on
   * (issue #883). Optional and defaulting to empty, which renders the pre-#883
   * row — this screen's own read is what decides *whether* it is waiting.
   */
  parked?: readonly ApprovalSummary[];
  /**
   * The console-wide decision state this screen decides through (#1891).
   *
   * The same bundle the board and the run drawer receive, so a verdict given on
   * any of them settles on the others without a reload. `onDecide` absent means
   * a read-only screen: it renders what the card is waiting on and no controls,
   * on `RunResultPanel`'s precedent.
   */
  deciding?: ReadonlyMap<string, Verdict>;
  decided?: Readonly<Record<string, DecidedApproval>>;
  failed?: Record<string, string>;
  onDecide?: (approval: ApprovalSummary, verdict: Verdict, scope: GrantScope) => void;
  /**
   * What the address asked this screen to open (issue #339): a pinned artifact
   * or an attempt's trace. Empty — the ordinary "open the card" navigation —
   * lands on the default tab, exactly as before.
   */
  focus?: TaskFocus;
  /** Writes an always-visible tab into the task detail's address. */
  onTabChange?: (tab: TaskTab) => void;
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
  // The board's columns, so this screen and the board label a status the same
  // way without either keeping a list. See `lib/board-columns`.
  const columns = useBoardColumns(client, company);
  /**
   * Who asked for each parked approval (#1891). Over the whole company queue,
   * not this card's slice: `useAskerNames` keys on the set of asker ids, so the
   * roster read is shared with every other surface asking the same question.
   */
  const askerNames = useAskerNames(client, company, [...parked]);
  const [detail, setDetail] = useState<TaskDetail | null>(null);
  const [inflight, setInflight] = useState<InflightRun | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [notFound, setNotFound] = useState(false);
  const [editing, setEditing] = useState(false);
  const [now, setNow] = useState(() => Date.now());
  // Issue #339: the tab became controlled so a link can land on it. It still
  // defaults to Timeline for every ordinary open — `tabForFocus` returns that
  // whenever the address asked for nothing.
  const [tab, setTab] = useState<string>(() => tabForFocus(focus));

  // Follow the address after mount: a card link clicked while this screen is
  // already open, a back/forward between two links on the same card, or a
  // lineage hop to a neighbouring card. Keyed on the focus's own identity
  // rather than the object, because the parent rebuilds it on every hash event
  // and an object dependency would yank the operator back to the linked tab on
  // every re-render.
  //
  // `taskId` is a dependency because a lineage hop writes a plain
  // `#/tasks/<id>` — no tab addressed — while this component instance survives.
  // Without the reset the screen would keep showing the previous card's tab
  // while its address claimed the default, so copying or reloading that URL
  // opened a different view than the one on screen.
  const focusKey = `${focus?.tab ?? ""}|${focus?.artifactId ?? ""}|${focus?.version ?? ""}|${focus?.runId ?? ""}`;
  useEffect(() => {
    setTab(tabForFocus(focus));
    // `focus` is read through `focusKey`, which is what actually changed.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [focusKey, taskId]);

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
    const dispose = startVisiblePolling(() => void load(isActive), POLL_MS);
    return () => {
      cancelled = true;
      dispose();
    };
  }, [load]);

  // Issue #1015: the push half. Re-read the detail the moment the host says an
  // attempt moved, rather than up to `POLL_MS` later — and at all, which the
  // poll above deliberately does not do while the tab is hidden.
  //
  // Its own effect rather than a dependency of the one above, for the reason the
  // board screen gave for the same split (issue #464): folding the tick in there
  // would tear down and restart the fallback timer on every frame, so a busy
  // company would keep resetting the interval and the fallback would effectively
  // stop existing on exactly the companies that need it most.
  //
  // The poll STAYS. A frame can be missed — the stream can drop, and the store
  // swallows an append that fails rather than failing the transition it
  // describes (`runtime::run_events`) — so the timer is the floor under a
  // liveness this does not otherwise guarantee.
  const seenAttemptTick = useRef(attemptEventTick);
  useEffect(() => {
    if (attemptEventTick === undefined || attemptEventTick === seenAttemptTick.current) return;
    // Gated like the poll it complements (issue #581), or the visibility gate
    // buys nothing on a busy company. Deliberately NOT consumed on the way out:
    // marking it seen here would drop it, since this effect does not re-run on a
    // visibility change.
    if (document.visibilityState === "hidden") return;
    seenAttemptTick.current = attemptEventTick;
    let cancelled = false;
    void load(() => !cancelled);
    return () => {
      cancelled = true;
    };
  }, [attemptEventTick, load]);

  const worked = useMemo(
    () =>
      detail
        ? extend(
            detail.durations.workedMillis,
            detail.durations.workedLive,
            detail.durations.asOfMillis,
            now,
          )
        : null,
    [detail, now],
  );
  // This remains an input to the worked-time calculation below, but is not a
  // second current-wait presentation. Pending approvals own that display.
  const waiting = useMemo(
    () =>
      detail
        ? extend(
            detail.durations.waitingMillis,
            detail.durations.waitingLive,
            detail.durations.asOfMillis,
            now,
          )
        : null,
    [detail, now],
  );
  // Only tick the 1s clock while something is actually running: a dispatch
  // window is open, an attempt has not settled, or this task still owns a
  // pending approval. Pending approvals are the one source for the current
  // wait clock, including after the run that created one has settled.
  const awaitingApproval = Boolean(detail?.approvals.some((a) => a.status === "pending"));
  const ticking =
    Boolean(worked?.live) ||
    Boolean(detail?.runs.some(isRunOpen)) ||
    awaitingApproval;
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
        {/*
          `#/tasks/<deleted-id>` had no `h1` at all (codex review, #1785): this
          pane's only heading is the card's own title, and a deleted card has
          none. `hidden`, because the recovery message *is* the pane.
        */}
        <PageHeader title="Task not found" hidden />
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
      {/*
        This pane's heading is the card's own title, inside `DetailHeader`,
        which needs a loaded `detail`. So a cold `#/tasks/<id>` was unnamed
        while the read was in flight, and stayed unnamed if it failed with
        anything other than a 404 — `detail` is left null and nothing retries
        (codex review, #1785).

        "Task", not the id: an id is a string the operator did not choose and
        cannot read out, and announcing one would be worse than announcing the
        kind of page. It disappears the moment the real title exists, so the
        two are never both on screen.
      */}
      {!detail && <PageHeader title="Task" hidden />}
      <div className="flex items-center gap-2 border-b px-4 py-3">
        <Button
          variant="ghost"
          size="sm"
          className="-ml-2 h-8 px-2"
          onClick={onBack}
        >
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
          <div className="w-full space-y-4 p-4">
            <section className="overflow-hidden rounded-xl border bg-card">
              <DetailHeader
                task={detail.task}
                worked={worked}
                waiting={waiting}
                columns={columns}
              />

              <ControlBar
                task={detail.task}
                inflight={inflight}
                irreversible={detail.irreversibleEffects}
                historyIncomplete={detail.historyIncomplete}
                client={client}
                company={company}
                onChanged={load}
                onSaved={onSaved}
                onEdit={() => setEditing(true)}
              />

              <AwaitingApprovalRow
                approvals={detail.approvals}
                parked={parked}
                taskId={detail.task.id}
                now={now}
                askerNames={askerNames}
                deciding={deciding}
                decided={decided}
                failed={failed}
                onDecide={onDecide}
              />
            </section>

            <OriginThreadRow
              originChatId={detail.task.originChatId}
              onOpenThread={onOpenThread}
            />

            <LineageRail
              lineage={detail.lineage}
              onNavigate={onNavigate}
              columns={columns}
            />

            {/* Issue #580: the built workflow awaiting approval, shown only while
                the card sits In Review with a proposal. Apply creates the
                workflow and moves the card to Done (#339 link); reject returns
                it to Pending. */}
            {detail.task.stage === "in_review" && detail.task.workflowProposal && (
              <TaskWorkflowProposalPanel
                client={client}
                company={company}
                task={detail.task}
                onSaved={onSaved}
                onReload={load}
              />
            )}

            <Tabs
              value={tab}
              onValueChange={(next) => {
                const selected = String(next);
                setTab(selected);
                if (isTaskTab(selected)) onTabChange?.(selected);
              }}
            >
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
                {/*
                  * Issue #337: only for a card somebody planned. A tab that is
                  * empty on every card until Planning is used would be clutter
                  * on the four screens out of five that never see one.
                  *
                  * The blocker count rides the trigger, in the Attempts count's
                  * shape, because it is the one thing worth seeing without a
                  * click: a card sitting in To-do with "Plan 2" in red answers
                  * "why didn't this start?" from the tab bar.
                  */}
                {detail.task.plan && (
                  <TabsTrigger value="plan">
                    Plan
                    {(() => {
                      const badge = planTabCount(detail.task.plan!);
                      return badge ? (
                        <span className={cn("ml-1.5 tabular-nums", badge.tone)}>
                          {badge.count}
                        </span>
                      ) : null;
                    })()}
                  </TabsTrigger>
                )}
                <TabsTrigger value="artifacts">Artifacts</TabsTrigger>
                <TabsTrigger value="discussion">Discussion</TabsTrigger>
              </TabsList>

              <TabsContent value="timeline" className="mt-4">
                <TimelineList
                  empty={
                    <EmptyState
                      title="Nothing has happened yet"
                      body="Dispatch this task from the board to start its timeline."
                    />
                  }
                  entries={detail.timeline}
                />
              </TabsContent>

              <TabsContent value="attempts" className="mt-4">
                <AttemptsTab
                  client={client}
                  company={company}
                  runs={detail.runs}
                  now={now}
                  openRunId={focus?.runId}
                />
              </TabsContent>

              {detail.task.plan && (
                <TabsContent value="plan" className="mt-4">
                  <TaskPlanBrief
                    plan={detail.task.plan}
                    // Issue #1106: answering the brief's ownership question is
                    // the same write the reassign row makes, through the same
                    // route — so the host validates the pick against the roster
                    // once, for both, and a candidate the planner named cannot
                    // reach the card by a path that skips that check.
                    onPick={async (id) => {
                      try {
                        const saved = await patchTask(client, company, detail.task.id, {
                          assignee: id,
                        });
                        onSaved(saved);
                        await load();
                        toast.success(`Assigned to ${id}.`);
                      } catch (e) {
                        toast.error(
                          e instanceof Error ? e.message : "could not assign the task",
                        );
                      }
                    }}
                  />
                </TabsContent>
              )}

              <TabsContent value="artifacts" className="mt-4">
                <ArtifactsTab
                  client={client}
                  company={company}
                  taskId={detail.task.id}
                  openArtifactId={focus?.artifactId}
                  openVersion={focus?.version}
                />
              </TabsContent>

              <TabsContent value="discussion" className="mt-4">
                <DiscussionTab
                  // Keyed by card: a different task is a different thread, and
                  // the tab accumulates the one it is shown.
                  key={detail.task.id}
                  messages={detail.discussion}
                  hasMore={detail.discussionHasMore}
                  taskId={detail.task.id}
                  client={client}
                  company={company}
                  onPosted={load}
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
  columns,
}: {
  task: Task;
  worked: { millis: number; live: boolean } | null;
  waiting: { millis: number; live: boolean } | null;
  /** The board's columns, for the status badge's label. Passed rather than
      read here: they come from the `tasks` ledger, so the one component that
      can fetch them is the one that already holds the client. */
  columns: TaskColumn[];
}) {
  const hasDispatch = worked !== null && (worked.millis > 0 || worked.live);
  // Issue #465: "Not yet dispatched" is only sayable where it can still be true.
  // A card past To-do/Planning has been worked whether or not a dispatch window
  // was journaled for it, so claiming otherwise beside a settled status is the
  // self-contradiction the report caught.
  const neverStarted = !hasDispatch && neverDispatched(task);
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
    <div className="p-4">
      <div className="flex items-start justify-between gap-3">
        <h1 className="text-lg font-semibold leading-snug">{task.title}</h1>
        <Badge
          variant="outline"
          className={cn("shrink-0 capitalize", priorityStyle(task.priority))}
        >
          {task.priority}
        </Badge>
      </div>

      <div className="mt-3 flex flex-wrap items-center gap-x-4 gap-y-2 text-xs text-muted-foreground">
        {formatUsdCost(task.cost, "total") && (
          <span className="font-medium tabular-nums text-foreground">
            {formatUsdCost(task.cost, "total")}
            {task.cost?.amountUsd !== undefined && " total"}
          </span>
        )}
        <span className="inline-flex items-center gap-1.5">
          <span className="font-medium text-foreground">Status</span>
          <Badge variant="secondary" className="font-normal">
            {columnLabel(columns, task.column)}
          </Badge>
          {/* The stage beside the phase, never instead of it (issue #1512).
              The board has three columns; what a working card is *waiting on*
              is a property of the card, and this is where it is read. */}
          {task.stage && (
            <Badge variant="outline" className="font-normal">
              {columnLabel(columns, task.stage)}
            </Badge>
          )}
        </span>
        {/* Issue #580: this card builds a reusable workflow rather than doing
            the work once. Stated on the detail header the same as the board
            card's chip, so the deliverable reads the same from either surface. */}
        {task.deliverable === "workflow" && (
          <span className="inline-flex items-center gap-1.5">
            <Badge variant="outline" className="gap-1 font-normal">
              <Workflow className="size-3" aria-hidden />
              Workflow
            </Badge>
          </span>
        )}
        {task.assignee && (
          <span className="inline-flex items-center gap-1.5">
            <span
              className="flex size-5 items-center justify-center rounded-full bg-muted text-3xs font-semibold"
              aria-hidden
            >
              {initials(task.assignee)}
            </span>
            {task.assignee}
          </span>
        )}
        {(hasDispatch || neverStarted) && (
          <span className="inline-flex items-center gap-1.5">
            <Clock className="size-3.5" />
            {hasDispatch ? (
              <>
                <span className="font-medium text-foreground">
                  Worked {formatDuration(workingMs)}
                </span>
                {worked!.live && (
                  <span className="inline-flex items-center gap-1 text-status-done-text">
                    <span
                      className="size-1.5 animate-pulse rounded-full bg-current"
                      aria-hidden
                    />
                    live
                  </span>
                )}
              </>
            ) : (
              <span>Not yet dispatched</span>
            )}
          </span>
        )}
      </div>

      {task.note && (
        <p className="mt-3 whitespace-pre-wrap border-t pt-3 text-xs text-muted-foreground">
          {task.note}
        </p>
      )}

      {/* Each waiting span is exact — a real park instant to a real resolution
          — and since #333 so is the card it is charged to: an approval is
          journaled with the task that parked it. Only a sign-off parked by a
          host older than #333 has no link and still falls back to the run
          window. Said plainly rather than left for a reader to discover. */}
      {showWaiting && (
        <p className="mt-2 text-2xs text-muted-foreground">
          Waiting counts this task&rsquo;s own approvals; sign-offs parked before they carried a
          task id fall back to its run window.
        </p>
      )}
    </div>
  );
}

function ControlBar({
  task,
  inflight,
  irreversible,
  historyIncomplete,
  client,
  company,
  onChanged,
  onSaved,
  onEdit,
}: {
  task: Task;
  inflight: InflightRun | null;
  /**
   * What a retry would do again (#351). Empty means Retry stays one click —
   * unless `historyIncomplete` says the empty is a gap rather than an all-clear.
   */
  irreversible: IrreversibleEffect[];
  /** Whether the journal holds executed history it cannot describe (#351). */
  historyIncomplete: boolean;
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

  async function steer(
    action: SteerAction,
    opts?: { instruction?: string; confirm?: boolean },
  ) {
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
      const saved = await patchTask(client, company, task.id, {
        // The phase word, not the stage: the host resolves `working` to
        // `in_progress`, which is what dispatches (issue #1512).
        column: "working",
      });
      onSaved(saved);
      await onChanged();
      toast.success("Dispatched — the assignee is working on it.");
    } catch (e) {
      toast.error(
        e instanceof Error ? e.message : "could not dispatch the task",
      );
    } finally {
      setBusy(false);
    }
  }

  /**
   * Buy one planning pass before any work is dispatched (issue #1512).
   *
   * The gesture the Planning **column** used to be. Collapsing the board to
   * three columns took the drop target away, and the pass is not something to
   * lose with it: it is the one way to have a card turned into a brief — and to
   * have its hard prerequisites checked — before an agent turn is paid for.
   *
   * So it becomes a control on the card, which is where it belonged anyway. A
   * column is a *state*, and "plan this first" is an act. It writes the
   * `planning` stage directly, because there is no phase word that means it:
   * dropping into Working means dispatch, and always has.
   *
   * It spends money, exactly as the drag into Planning did, which is why it is
   * a deliberate second button rather than something folded into Dispatch.
   */
  async function planFirst() {
    setBusy(true);
    try {
      const saved = await patchTask(client, company, task.id, {
        column: "planning",
      });
      onSaved(saved);
      await onChanged();
      toast.success("Planning — a brief is being written for this card.");
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "could not plan the task");
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
      const saved = await patchTask(client, company, task.id, {
        assignee: next,
      });
      onSaved(saved);
      await onChanged();
      setReassigning(false);
      toast.success("Reassigned.");
    } catch (e) {
      toast.error(
        e instanceof Error ? e.message : "could not reassign the task",
      );
    } finally {
      setBusy(false);
    }
  }

  const resumeLabel = task.stage === "paused" ? "Resume" : "Retry";

  return (
    <div className="border-t bg-card/40 p-3">
      <div className="flex flex-wrap items-center gap-2">
        {inflight ? (
          <>
            <span className="mr-1 inline-flex items-center gap-1.5 text-xs font-medium text-status-done-text">
              <span
                className="size-1.5 animate-pulse rounded-full bg-current"
                aria-hidden
              />
              In flight
            </span>
            {pending !== null ? (
              <span className="rounded-full bg-muted px-2 py-0.5 text-2xs font-medium text-muted-foreground">
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
                    <Button
                      variant="outline"
                      size="sm"
                      className="h-8"
                      disabled={steerDisabled}
                    >
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
          <>
            <RetryButton
              label={resumeLabel}
              title={task.title}
              irreversible={irreversible}
              historyIncomplete={historyIncomplete}
              disabled={busy}
              onConfirm={() => void patchColumn()}
            />
            {/* Only before anything has started: planning a card that has
                already been worked would write a brief for work that exists,
                which is the one shape the pass has nothing useful to say
                about (issue #1512). */}
            {task.column === "pending" && (
              <Button
                variant="outline"
                size="sm"
                className="h-8"
                disabled={busy}
                onClick={() => void planFirst()}
              >
                <ClipboardList className="mr-1.5 size-3.5" />
                Plan first
              </Button>
            )}
          </>
        )}

        <div className="ml-auto flex items-center gap-2">
          {busy && (
            <Loader2 className="size-4 animate-spin text-muted-foreground" />
          )}
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
          <Button
            variant="ghost"
            size="sm"
            className="h-8"
            disabled={busy}
            onClick={onEdit}
          >
            <Pencil className="mr-1.5 size-3.5" />
            Edit
          </Button>
          <ExportButton task={task} client={client} company={company} />
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
              if (e.key === "Enter" && instruction.trim())
                void steer("redirect", { instruction: instruction.trim() });
            }}
          />
          <Button
            size="sm"
            className="h-8"
            disabled={steerDisabled || !instruction.trim()}
            onClick={() =>
              void steer("redirect", { instruction: instruction.trim() })
            }
          >
            <Send className="mr-1.5 size-3.5" />
            Send
          </Button>
        </div>
      )}

      {reassigning && (
        <div className="mt-2 flex items-center gap-2">
          {/* Issue #263: the same roster picker the edit dialog uses, and since
              #1106 the same one the create dialog offers up front. This is the
              row that can reach the *whole* roster after the fact — the create
              dialog pre-empts, and the plan brief's candidate list narrows to
              what the planner named, so a card that needs anyone else is
              reassigned here. Unassigned is a row rather than an empty field, so
              handing the card back to the orchestrator is something you can see
              and choose. */}
          <AssigneeSelect
            client={client}
            company={company}
            value={assignee}
            onChange={setAssignee}
            disabled={busy}
            className="h-8 min-w-0 flex-1"
          />
          <Button
            size="sm"
            className="h-8"
            disabled={busy}
            onClick={() => void saveAssignee()}
          >
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
      <span className="shrink-0 text-muted-foreground">
        Open the conversation
      </span>
    </button>
  );
}

function LineageRail({
  lineage,
  onNavigate,
  columns,
}: {
  lineage: TaskDetail["lineage"];
  onNavigate: (id: string) => void;
  columns: TaskColumn[];
}) {
  if (!lineage.parent && lineage.children.length === 0) return null;
  return (
    <div className="rounded-xl border bg-card/40 p-3">
      <p className="mb-2 text-2xs font-medium uppercase tracking-wide text-muted-foreground">
        Lineage
      </p>
      <div className="space-y-1.5">
        {lineage.parent && (
          <button
            className="flex w-full items-center gap-2 rounded-lg border bg-card px-2.5 py-1.5 text-left text-xs transition-colors hover:bg-accent"
            onClick={() => onNavigate(lineage.parent!.id)}
          >
            <CornerUpLeft className="size-3.5 shrink-0 text-muted-foreground" />
            <span className="min-w-0 flex-1 truncate">
              {lineage.parent.title}
            </span>
            {formatUsdCost(lineage.parent.cost, "total") && (
              <span className="shrink-0 tabular-nums text-muted-foreground">
                {formatUsdCost(lineage.parent.cost, "total")}
              </span>
            )}
            <Badge variant="secondary" className="shrink-0 font-normal">
              {columnLabel(columns, lineage.parent.column)}
            </Badge>
          </button>
        )}
        <div className="px-2.5 py-1 text-2xs font-medium text-muted-foreground">
          This task
        </div>
        {lineage.children.map((child) => (
          <button
            key={child.id}
            className="flex w-full items-center gap-2 rounded-lg border bg-card px-2.5 py-1.5 pl-5 text-left text-xs transition-colors hover:bg-accent"
            onClick={() => onNavigate(child.id)}
          >
            <CornerDownRight className="size-3.5 shrink-0 text-muted-foreground" />
            <span className="min-w-0 flex-1 truncate">{child.title}</span>
            {formatUsdCost(child.cost, "total") && (
              <span className="shrink-0 tabular-nums text-muted-foreground">
                {formatUsdCost(child.cost, "total")}
              </span>
            )}
            <Badge variant="secondary" className="shrink-0 font-normal">
              {columnLabel(columns, child.column)}
            </Badge>
          </button>
        ))}
      </div>
    </div>
  );
}

export { groupTimeline } from "@/views/runs/RunTimeline";

// Timeline rendering lives in `@/views/runs/RunTimeline`; the task and attempt
// surfaces share the same grouping, waiting bands, and step-state treatment.

// ---------------------------------------------------------------------------
// Attempts (#242)
// ---------------------------------------------------------------------------

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
  openRunId,
}: {
  client: OpenCompanyClient;
  company: string | null;
  runs: RunSummary[];
  now: number;
  /**
   * An attempt the address asked to open (issue #339) — the *"this task
   * produced no file"* link, whose deliverable is the trace itself.
   */
  openRunId?: string;
}) {
  const [openId, setOpenId] = useState<string | null>(null);
  const open = useMemo(
    () => runs.find((r) => r.id === openId) ?? null,
    [runs, openId],
  );
  // Apply a requested attempt once per distinct id. Once, because the parent
  // re-renders this tab on every four-second poll and re-applying would reopen
  // a drawer the operator has just closed; per distinct id, so a second link to
  // a different attempt still lands.
  const appliedRun = useRef<string | null>(null);
  useEffect(() => {
    if (!openRunId) {
      // Forget the last application when the address stops naming one, so
      // returning to the same link later opens it again.
      appliedRun.current = null;
      return;
    }
    if (appliedRun.current === openRunId) return;
    // Runs arrive with the detail read, so wait for the row to exist rather
    // than opening a drawer onto nothing.
    if (!runs.some((r) => r.id === openRunId)) return;
    appliedRun.current = openRunId;
    setOpenId(openRunId);
  }, [openRunId, runs]);

  if (runs.length === 0) {
    return (
      <EmptyState
        title="No recorded attempts"
        body="Dispatch this card to record one. Cards dispatched before attempts were recorded show none — they were never backfilled, because inventing them would fabricate a record."
      />
    );
  }

  // A link naming an attempt this card does not have: say so rather than
  // silently showing the list, so a stale link reads as stale instead of as a
  // broken screen.
  const missing = Boolean(openRunId && !runs.some((r) => r.id === openRunId));

  return (
    <div className="space-y-3">
      <p className="text-xs text-muted-foreground">
        Every dispatch of this card, newest first. A card can enter review more
        than once, so several waits on one attempt is the expected record, not a
        fault.
      </p>
      {missing && (
        <p className="rounded-md border border-status-blocked/40 bg-status-blocked-soft px-3 py-2 text-xs text-muted-foreground">
          The attempt that link points at is no longer in this card's history.
          Its other attempts are below.
        </p>
      )}
      <ol className="space-y-1.5">
        {runs.map((run) => (
          <AttemptRow
            key={run.id}
            run={run}
            now={now}
            onOpen={() => setOpenId(run.id)}
          />
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
          <Badge
            variant="outline"
            className={cn("shrink-0 font-normal", runStatusTone(run.status))}
          >
            {RUN_STATUS_LABEL[run.status]}
          </Badge>
          <span className="min-w-0 flex-1 truncate text-muted-foreground">
            {run.agentId}
          </span>
          {elapsed !== null && (
            <span
              className={cn(
                "shrink-0 tabular-nums text-2xs text-muted-foreground",
                isRunOpen(run) && "text-foreground",
              )}
            >
              {formatDuration(elapsed)}
              {isRunOpen(run) && " …"}
            </span>
          )}
          <ChevronRight className="size-3.5 shrink-0 text-muted-foreground" />
        </div>
        <div className="flex w-full items-center gap-2 pl-5 text-2xs text-muted-foreground">
          <span className="tabular-nums">{timeOf(run.createdAtMillis)}</span>
          <span aria-hidden>·</span>
          <span>{stepSummary(run)}</span>
          {run.stepCountCapped && <span>(trace capped)</span>}
        </div>
        {run.error && (
          <p className="w-full truncate pl-5 text-2xs text-status-failed-text">
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
        if (!cancelled)
          setError(
            e instanceof Error ? e.message : "could not load the attempt",
          );
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
                    <span className="tabular-nums">
                      {formatDuration(elapsed)}
                    </span>
                  </>
                )}
              </SheetDescription>
            </SheetHeader>
            <ScrollArea className="min-h-0 flex-1">
              <div className="space-y-3 px-4 pb-4">
                {run.error && (
                  <Alert variant="destructive">
                    <AlertDescription className="text-xs">
                      {run.error}
                    </AlertDescription>
                  </Alert>
                )}
                {error && (
                  <Alert variant="destructive">
                    <AlertDescription className="text-xs">
                      {error}
                    </AlertDescription>
                  </Alert>
                )}
                {run.stepCountCapped && (
                  <p className="text-2xs text-muted-foreground">
                    This attempt hit the per-run trace ceiling, so what follows
                    is the start of the run, not all of it.
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
                     `kind` simply widens to the three step words. The zero-step
                     case is handled by the guard above, so no `empty` copy is
                     needed here — the task-card dispatch sentence would be
                     wrong for a run's trace anyway. */
                  <TimelineList entries={detail.steps} />
                ) : null}
              </div>
            </ScrollArea>
          </>
        )}
      </SheetContent>
    </Sheet>
  );
}

/**
 * What this card is waiting on, decided here (#468, #1891).
 *
 * This replaced an Approvals tab that listed every sign-off this task ever
 * asked for. The tab was removed because it could not do the one thing an
 * operator wanted from it — decide — and a second surface beside the real one
 * was a maintenance cost that only ever ended in a link. The row that took its
 * place kept the *signal*: a card stalled behind a sign-off has to say so, or
 * the screen that exists to answer "why is this stuck" silently stops
 * answering it.
 *
 * #1891 closes the loop the tab could not: the sign-off is decided **here**,
 * through the same `ApprovalRow` the Approvals page, the chat transcript and
 * the workflow run drawer resolve with. That is not the tab coming back. The
 * tab was a *list of history* that ended in a link; this is the live queue for
 * one card, and it disappears the moment nothing is pending.
 *
 * Still says nothing about *decided* approvals. Those are resolutions, already
 * on the timeline as `approval` entries with their own waited span, and
 * repeating them here would rebuild the tab one row at a time.
 *
 * ## Two sources, and only one of them is the truth about waiting
 *
 * `approvals` is this screen's own read, and it decides **whether** the card is
 * waiting — the host computed it with an ownership rule (`approval_owner`)
 * carrying an attempt-level key this side cannot see. `parked` is the company
 * queue, and it is what makes a pending approval *decidable*: it is where the
 * payload, the deadline and the id live.
 *
 * They can disagree for a poll, and the honest rendering of that is the point.
 * An approval the host counts and the feed has not delivered yet is still
 * counted — it is named in the residual line rather than dropped, because a
 * decide surface that quietly showed three of four rows would tell an operator
 * they had cleared a card that is still stopped.
 *
 * Exported for `test/unit/task-detail-approvals.test.ts` (#1891), on the same
 * grounds `TaskItem` and the chat card are: the claims that matter here — that
 * a decidable row appears, that deciding one does not freeze its siblings, and
 * above all that an undelivered approval is *said* rather than swallowed — only
 * exist at the rendered row.
 */
export function AwaitingApprovalRow({
  approvals,
  parked,
  taskId,
  now,
  askerNames,
  deciding,
  decided,
  failed,
  onDecide,
}: {
  approvals: TaskApproval[];
  /**
   * The company's parked queue (issue #883). Deliberately **not** the source of
   * truth for whether the card is waiting: `approvals` is, because the host
   * computed it with an ownership rule (`approval_owner`) that has an
   * attempt-level key this side cannot see. Rows are matched into it by id, so
   * an approval the host counts and the feed has not caught up on is still
   * counted — it goes undecidable for one poll rather than disappearing.
   */
  parked: readonly ApprovalSummary[];
  taskId: string;
  now: number;
  askerNames: Map<string, string>;
  deciding: ReadonlyMap<string, Verdict>;
  decided: Readonly<Record<string, DecidedApproval>>;
  failed: Record<string, string>;
  onDecide?: (approval: ApprovalSummary, verdict: Verdict, scope: GrantScope) => void;
}) {
  const pending = pendingApprovalWait(approvals, now);
  /**
   * This card's rows, as the queue has them.
   *
   * One row per approval, unlike the board card's single consolidated batch,
   * and for the same reason the Approvals page itemises: this screen is where
   * an operator studies a request. There is room for each payload and each
   * grant-scope choice, and a card whose turn parked a fetch *and* a payment
   * should be able to take the first and refuse the second here.
   */
  const rows = useMemo(() => {
    // Intersected with the host's own answer, never taken from the queue alone
    // (#1895 review). `approvalsForTask` can only match on the park's task
    // link, while `approval_owner` decides ownership with an attempt-level
    // `run_id` this side cannot see — so an approval linked to this card but
    // belonging to another attempt is excluded from `approvals` by the host and
    // would still have been pulled in here, giving this screen a row to resolve
    // that the host does not consider part of this card. The header already
    // says `approvals` is the authority on what this card is waiting on; this
    // is that rule applied to the rows and not only to the count.
    const owned = new Set(
      approvals.filter((a) => a.status === "pending").map((a) => a.id),
    );
    return taskApprovalRows(parked, decided, taskId).filter((r) =>
      owned.has(r.approval.id),
    );
  }, [approvals, parked, decided, taskId]);
  const blocking = useMemo(() => blockingTaskApprovals(rows), [rows]);
  /**
   * Pending approvals the host counts that this screen can neither show nor
   * account for.
   *
   * The honest residual, and the reason it is stated rather than swallowed: the
   * two reads land separately, so for a poll this screen can hold four pending
   * approvals and three decidable rows. Deciding those three would leave the
   * card stopped, and a surface that said nothing would have just told the
   * operator they were finished.
   *
   * **A set difference, not a subtraction** (#1895 review). Counting
   * `pending.count - blocking.length` mixes two clocks: `pending` refreshes on
   * this screen's own 4s poll while `blocking` drops a row the instant a
   * verdict is witnessed, so for up to a poll after the operator decides the
   * last approval the arithmetic said one was undelivered and the screen
   * announced "still loading it" about a request that had just been settled by
   * the person reading it. Naming the ids instead cannot drift: an approval is
   * unaccounted for only if the host still calls it pending, this screen has no
   * row for it, **and** this console has not decided it.
   */
  const undelivered = useMemo(() => {
    const shown = new Set(rows.map((r) => r.approval.id));
    return approvals.filter(
      (a) => a.status === "pending" && !shown.has(a.id) && !decided[a.id],
    ).length;
  }, [approvals, rows, decided]);
  if (!pending) return null;
  const { waited } = pending;
  const href = `#/approvals/${encodeURIComponent(taskId)}`;

  return (
    <div className="border-t border-status-blocked/30 bg-status-blocked-soft px-4 py-3">
      <div className="flex items-center gap-2 text-xs">
        <Hourglass className="size-3.5 shrink-0 text-status-blocked-text" />
        <span className="min-w-0 flex-1 text-status-blocked-text">
          {pending.count === 1
            ? "Waiting on your approval"
            : `Waiting on ${pending.count} approvals`}{" "}
          <span className="tabular-nums">for {formatDuration(waited)}</span>
        </span>
        {/* Kept even now that the rows are decidable here. The page is where an
            operator cleans up across cards, and it is also the whole answer when
            the feed has delivered none of this card's rows yet. */}
        <a
          href={href}
          className="shrink-0 font-medium text-status-blocked-text underline-offset-2 hover:underline"
          aria-label={
            pending.count === 1
              ? "Review this task's pending approval on the Approvals page"
              : `Review this task's ${pending.count} pending approvals on the Approvals page`
          }
        >
          Review
        </a>
      </div>
      {onDecide && blocking.length > 0 && (
        <div className="-mx-4 mt-1" data-testid="task-approvals">
          {blocking.map((row) => (
            <ApprovalRow
              key={row.approval.id}
              approvals={[row.approval]}
              now={now}
              askerNames={askerNames}
              // Narrowed to this row: a decision in flight on a sibling
              // approval of the same card is not this row's business, and
              // `ApprovalRow` reads a non-empty map as "I am busy" (#373).
              deciding={decidingForTask([row], deciding)}
              decided={EMPTY_VERDICTS}
              failed={failed}
              onDecide={onDecide}
            />
          ))}
        </div>
      )}
      {undelivered > 0 && (
        <p className="mt-1 text-2xs text-muted-foreground">
          {/* Never "and that is all of them". See `undelivered` above. */}
          {blocking.length === 0
            ? `Still loading ${undelivered === 1 ? "it" : "them"} — decide on the Approvals page in the meantime.`
            : `${undelivered} more not loaded yet — this card stays stopped until ${undelivered === 1 ? "it is" : "they are"} decided too.`}
        </p>
      )}
    </div>
  );
}

/**
 * Exports the task's record as a document (#352).
 *
 * The host renders it — one implementation, so a scripted export gets the same
 * file — and this is delivery only: fetch the document through the authenticated
 * client, then hand it to the browser as a download. A plain `<a href>` would be
 * simpler but would drop the `Authorization` header the platform-token
 * deployment needs, so the bytes come back through `getDocument` — with the
 * host's own filename — and go out through an object URL.
 *
 * Read-only by construction: the button triggers a GET, and the screen does not
 * reload after it, because nothing about the task changed.
 */
function ExportButton({
  task,
  client,
  company,
}: {
  task: Task;
  client: OpenCompanyClient;
  company: string | null;
}) {
  const [busy, setBusy] = useState(false);

  async function download() {
    setBusy(true);
    try {
      const { text, filename } = await exportTaskRecord(
        client,
        company,
        task.id,
      );
      const url = URL.createObjectURL(new Blob([text], { type: "text/html" }));
      const link = document.createElement("a");
      link.href = url;
      // The host already named the file, and a blob download does not honour
      // `Content-Disposition` on its own — so carry the host's name across
      // rather than deriving a second one here, which could disagree with what
      // a `curl -OJ` of the same route saves.
      link.download = filename ?? `task-${task.id}.html`;
      document.body.appendChild(link);
      link.click();
      link.remove();
      // Freed on the next tick: revoking synchronously can beat the click in
      // some browsers and save an empty file.
      setTimeout(() => URL.revokeObjectURL(url), 0);
      toast.success(
        "Record downloaded. Open it in any browser, or print it to PDF.",
      );
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "could not export the task");
    } finally {
      setBusy(false);
    }
  }

  return (
    <Button
      variant="ghost"
      size="sm"
      className="h-8"
      disabled={busy}
      title="Download this task's record as a document"
      onClick={() => void download()}
    >
      {busy ? (
        <Loader2 className="mr-1.5 size-3.5 animate-spin" />
      ) : (
        <Download className="mr-1.5 size-3.5" />
      )}
      Export
    </Button>
  );
}

/**
 * The Task Detail **Discussion** tab (#335): the card's own message thread.
 *
 * A task discussion is a thread of its own, not the company chat filtered to a
 * card — so a message posted here is about *this* work and is read by whoever
 * opens the card next. It is served on the parent's single `GET …/tasks/{id}`
 * (#185) and therefore rides its 4s poll: a colleague's post lands here without
 * a reload, which is the whole point of putting the conversation on the card.
 *
 * Operator-only in v1. Posting deliberately runs no agent turn — nothing here
 * dispatches work or spends money, which stays behind the board's column drag.
 * There is no edit: the thread is journal-backed and append-only, so what was
 * said stays said.
 *
 * A message can be **withdrawn** (#358), which is not the same as deleted. The
 * row keeps its place, its author and its time, and its text is replaced by the
 * host — on this screen, on the exported record, and in the company bundle, so
 * a pasted credential stops being readable and stops travelling. Withdrawing is
 * the one thing on this tab that needs a confirmation, because it is the one
 * thing that cannot be undone.
 *
 * The poll carries only the newest page of the thread (the host caps it so a
 * long discussion is not re-sent every 4s). Older messages are pulled on demand
 * and kept here, so walking back through a thread survives the next poll.
 */
function DiscussionTab({
  messages,
  hasMore,
  taskId,
  client,
  company,
  onPosted,
}: {
  messages: DiscussionMessage[];
  hasMore: boolean;
  taskId: string;
  client: OpenCompanyClient;
  company: string | null;
  onPosted: () => Promise<void>;
}) {
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  /**
   * Every message this tab has been shown, oldest first, deduped by `seq` — the
   * journal key, and the only identity a message has.
   *
   * The poll carries a *sliding* page, so a new post pushes the oldest message
   * of that page out of it; rendering the page directly would make a message
   * disappear from under someone mid-read. Accumulating means the thread on
   * screen only ever grows: the poll adds new posts, "load earlier" adds old
   * ones, and the `201` echo adds your own before the poll comes round.
   *
   * Mounted per card (the parent keys this component by task id), so this never
   * holds another card's thread.
   */
  const [thread, setThread] = useState<DiscussionMessage[]>([]);
  /**
   * Whether anything remains before the oldest message held. `null` until an
   * older page has been pulled, when that page's own flag is the answer.
   */
  const [earlierHasMore, setEarlierHasMore] = useState<boolean | null>(null);
  const [loadingEarlier, setLoadingEarlier] = useState(false);

  const absorb = useCallback((rows: DiscussionMessage[]) => {
    setThread((prev) => {
      const bySeq = new Map<number, DiscussionMessage>(
        prev.map((m) => [m.seq, m] as const),
      );
      let added = false;
      for (const m of rows) {
        const held = bySeq.get(m.seq);
        if (!held) {
          bySeq.set(m.seq, m);
          added = true;
          continue;
        }
        // A message this thread already holds is normally identical to the copy
        // the poll brought, and re-setting it would churn the render for
        // nothing. A WITHDRAWAL is the exception (#358): the row arrives again
        // with its text replaced, and it is the one update that must land —
        // ignoring it would leave the original text on screen for as long as
        // this tab stays open, which is precisely the reader the withdrawal was
        // for.
        if (m.redacted && !held.redacted) {
          bySeq.set(m.seq, m);
          added = true;
        }
      }
      // Same list back when the poll brought nothing new — the common case, and
      // the one that must not churn the render.
      return added ? [...bySeq.values()].sort((a, b) => a.seq - b.seq) : prev;
    });
  }, []);

  useEffect(() => {
    absorb(messages);
  }, [messages, absorb]);

  const moreBefore = earlierHasMore ?? hasMore;

  async function loadEarlier() {
    const oldest = thread[0]?.seq;
    if (oldest === undefined || loadingEarlier) return;
    setLoadingEarlier(true);
    try {
      const page = await getTaskDetail(client, company, taskId, oldest);
      absorb(page.discussion);
      setEarlierHasMore(page.discussionHasMore);
    } catch (e) {
      toast.error(
        e instanceof Error ? e.message : "could not load earlier messages",
      );
    } finally {
      setLoadingEarlier(false);
    }
  }

  /**
   * Withdraws one message (#358).
   *
   * The `200` carries the row as every reader now sees it — placeholder text,
   * `redacted`, and who removed it — and `absorb` collapses it onto the copy on
   * screen by `seq`, so the thread updates in place rather than waiting out the
   * 4s poll. `onPosted` then re-reads the card for the same reason posting
   * does: the parent holds the authoritative page.
   */
  async function redact(seq: number) {
    try {
      const row = await redactTaskDiscussion(client, company, taskId, seq);
      absorb([row]);
      await onPosted();
    } catch (e) {
      toast.error(
        e instanceof Error ? e.message : "could not remove the message",
      );
    }
  }

  async function post() {
    const body = text.trim();
    if (!body || busy) return;
    setBusy(true);
    try {
      const posted = await postTaskDiscussion(client, company, taskId, body);
      // Shown straight away rather than after the poll: the host journaled it
      // and handed the stored row back, so waiting up to four seconds to show
      // an operator their own message buys nothing. It carries the journaled
      // `seq` and stamp, so the poll's copy collapses onto it.
      absorb([posted]);
      // Cleared only after the host accepted it, so a failed post leaves the
      // operator's words in the box rather than losing them.
      setText("");
      await onPosted();
    } catch (e) {
      toast.error(
        e instanceof Error ? e.message : "could not post the message",
      );
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="space-y-3">
      {thread.length === 0 ? (
        <EmptyState
          title="No discussion yet"
          body="Post the first message to start this card's thread."
        />
      ) : (
        <ol className="space-y-1.5">
          {moreBefore ? (
            <li className="pb-1 text-center">
              <Button
                variant="ghost"
                size="sm"
                className="h-7 text-xs"
                disabled={loadingEarlier}
                onClick={() => void loadEarlier()}
              >
                {loadingEarlier ? (
                  <Loader2 className="mr-1.5 size-3.5 animate-spin" />
                ) : null}
                Load earlier messages
              </Button>
            </li>
          ) : null}
          {thread.map((m) => (
            <li
              key={m.seq}
              data-testid="discussion-message"
              data-redacted={m.redacted ? "true" : undefined}
              className={cn(
                "rounded-lg border px-3 py-2",
                m.redacted ? "border-dashed bg-muted/40" : "bg-card",
              )}
            >
              <div className="flex items-center gap-2 text-2xs text-muted-foreground">
                <MessagesSquare className="size-3.5 shrink-0" aria-hidden />
                <span className="min-w-0 flex-1 truncate font-medium text-foreground">
                  {m.author}
                </span>
                <span
                  className="shrink-0 tabular-nums"
                  title={new Date(m.atMillis).toLocaleString()}
                >
                  {timeOf(m.atMillis)}
                </span>
                {/* Issue #358. Offered only on a message that still has text to
                    withdraw: a second removal changes nothing, so a button for
                    it would be a control that does nothing. */}
                {!m.redacted && (
                  <ConfirmButton
                    trigger={
                      <Button
                        variant="ghost"
                        size="icon-sm"
                        className="-mr-1 size-6 shrink-0 text-muted-foreground hover:text-destructive"
                        aria-label={`Remove the message from ${m.author}`}
                        data-testid="discussion-redact"
                      >
                        <Trash2 className="size-3.5" />
                      </Button>
                    }
                    title="Remove this message?"
                    description="The message stops being readable here, on the exported record, and in this company's bundle. Its place in the thread stays, showing that you removed it. This cannot be undone, and if the message contained a credential you still need to rotate it."
                    confirmLabel="Remove it"
                    cancelLabel="Keep it"
                    destructive
                    onConfirm={() => void redact(m.seq)}
                  />
                )}
              </div>
              {m.redacted ? (
                <p className="mt-1 text-xs italic text-muted-foreground">
                  {m.text}
                  {m.redactedBy ? ` Removed by ${m.redactedBy}.` : null}
                </p>
              ) : (
                <p className="mt-1 whitespace-pre-wrap break-words text-xs">
                  {m.text}
                </p>
              )}
            </li>
          ))}
        </ol>
      )}

      <div className="flex items-start gap-2">
        <Textarea
          value={text}
          placeholder="Write a message about this task…"
          rows={2}
          className="min-h-16 text-xs"
          disabled={busy}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            // Enter posts; Shift+Enter is a newline. A note about a task is
            // usually one line, and the mouse trip for every one of them is
            // what stops people writing them down at all.
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              void post();
            }
          }}
        />
        <Button
          size="sm"
          className="h-8 shrink-0"
          disabled={busy || !text.trim()}
          onClick={() => void post()}
        >
          {busy ? (
            <Loader2 className="mr-1.5 size-3.5 animate-spin" />
          ) : (
            <Send className="mr-1.5 size-3.5" />
          )}
          Post
        </Button>
      </div>
    </div>
  );
}

function EmptyState({ title, body }: { title: string; body: string }) {
  return (
    <div className="rounded-xl border border-dashed py-10 text-center">
      <p className="text-sm font-medium">{title}</p>
      <p className="mx-auto mt-1 max-w-xs text-xs text-muted-foreground">
        {body}
      </p>
    </div>
  );
}

/**
 * Retry / Resume (#351): one click when the previous attempt did nothing that
 * cannot be taken back, and a confirmation naming what it did when it did.
 *
 * Re-entering a run re-runs its effects. Cancel on this same bar has always
 * been gated, while Retry — the control that can send a second payment or
 * submit a second filing — was a bare `patchColumn()`. The gate is conditional
 * on purpose: a dialog that appears every time is a dialog that gets clicked
 * through, and most retries are of a task that only read and replied.
 *
 * Nothing here re-derives risk from the timeline. The host answers with the
 * effects its journal recorded as executed, and an empty list means the journal
 * has nothing irreversible against this card — but only when
 * `historyIncomplete` is false. That flag is the honest half: a journal written
 * before #351 holds executed keys with no description, so "empty" there means
 * "cannot say", and the dialog opens anyway to say exactly that rather than
 * pass a gap off as an all-clear.
 *
 * Each row is what the runtime *committed to run*. The record is written before
 * the effect is performed — that ordering is what makes effects at-most-once —
 * and nothing ever re-attempts it, so an interrupted one still belongs on this
 * list. The footnote says so; the sentences stay in the past tense because
 * "must be assumed to have happened" is what an operator has to act on.
 *
 * Scope: **Task Detail only.** The board's own re-dispatch — dragging a card
 * back into `in_progress` — has the same shape and the same read available to
 * it, and is deliberately left for a follow-up rather than half-done here.
 */
function RetryButton({
  label,
  title,
  irreversible,
  historyIncomplete,
  disabled,
  onConfirm,
}: {
  label: string;
  title: string;
  irreversible: IrreversibleEffect[];
  historyIncomplete: boolean;
  disabled: boolean;
  onConfirm: () => void;
}) {
  // Nothing to warn about and nothing unaccounted for: the plain button, wired
  // straight through, exactly as it behaved before this existed.
  if (irreversible.length === 0 && !historyIncomplete) {
    return (
      <Button variant="outline" size="sm" className="h-8" disabled={disabled} onClick={onConfirm}>
        <Play className="mr-1.5 size-3.5" />
        {label}
      </Button>
    );
  }

  const named = irreversible.length;

  return (
    <ConfirmButton
      trigger={
        <Button variant="outline" size="sm" className="h-8" disabled={disabled}>
          <Play className="mr-1.5 size-3.5" />
          {label}
        </Button>
      }
      title={`${label} “${title}”?`}
      description={
        named === 0
          ? "Running this again may repeat whatever the last attempt did."
          : named === 1
            ? "This task already did something that cannot be undone. Running it again may do it a second time."
            : `This task already did ${named} things that cannot be undone. Running it again may do them a second time.`
      }
      details={
        <div className="space-y-2 text-left">
          {named > 0 && (
            <ul className="space-y-1.5 rounded-lg border bg-muted/40 p-3 text-xs">
              {irreversible.map((e, i) => (
                // Two effects of the same kind can land in the same millisecond,
                // so the index carries the uniqueness the pair cannot.
                <li key={`${e.kind}-${e.atMillis}-${i}`} className="flex items-start gap-2">
                  <AlertCircle
                    className="mt-px size-3.5 shrink-0 text-status-blocked-text"
                    aria-hidden
                  />
                  <span className="min-w-0 flex-1">{effectDone(e.kind, e.amountUsd)}</span>
                  <span className="shrink-0 tabular-nums text-muted-foreground">
                    {timeOf(e.atMillis)}
                  </span>
                </li>
              ))}
            </ul>
          )}
          {historyIncomplete && (
            // The list is short, not wrong. Say which, rather than let a
            // truncated list read as the whole story.
            <p className="text-xs text-muted-foreground">
              Some of this company’s earlier activity was recorded before it kept a description, so
              {named > 0 ? " this list may be incomplete." : " nothing here can be listed."}
            </p>
          )}
          {named > 0 && (
            <p className="text-2xs text-muted-foreground">
              Each is recorded the moment it was committed, so one that was interrupted still
              appears — nothing is ever retried on its own.
            </p>
          )}
        </div>
      }
      confirmLabel={`${label} anyway`}
      cancelLabel="Leave it alone"
      onConfirm={onConfirm}
    />
  );
}

/** An AlertDialog-gated button, mirroring the confirm pattern used elsewhere. */
function ConfirmButton({
  trigger,
  title,
  description,
  details,
  confirmLabel,
  cancelLabel = "Keep running",
  destructive,
  onConfirm,
}: {
  trigger: ReactElement;
  title: string;
  description: string;
  /**
   * Block content rendered under the description (#351) — a list of what
   * already happened. A sibling of the description rather than a child of it:
   * the description renders a `<p>`, and a `<ul>` inside one is invalid HTML.
   */
  details?: ReactElement;
  confirmLabel: string;
  cancelLabel?: string;
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
        {details}
        <AlertDialogFooter>
          <AlertDialogCancel>{cancelLabel}</AlertDialogCancel>
          <AlertDialogAction
            onClick={onConfirm}
            className={
              destructive
                ? "bg-destructive text-white hover:bg-destructive/90"
                : undefined
            }
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
