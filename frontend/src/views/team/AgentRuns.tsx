// A teammate's run history (issue #1573).
//
// Everything else on the Agent Detail page describes what a teammate *is* —
// its instructions, its toolbelt, its budget, its desks. This is the only
// section that says what it has actually *done*: every attempt it has made,
// where each one came from, and, one level down, every step it took getting
// there.
//
// # Master-detail, not a drawer
//
// The Task Detail screen shows an attempt's trace in a `max-w-md` side sheet,
// which is right there: the card, its timeline and its attempts are the
// content, and a trace is a detail you glance at without losing them. Here the
// trace *is* the content — the question this section exists to answer is "what
// did this teammate do", and the answer is a step list. A 448px sheet would put
// the one thing being read into the narrowest column on the page while the
// widest one showed a list the reader has already finished with. So opening a
// run replaces the list, full width, with one link back.
//
// # Two reads, and neither is allowed to fail the section
//
// The runs come from `GET …/runs?agent=`. The *sources* come from the board and
// the workflow list, which are separate reads that resolve a run's `taskId` into
// a card title and the workflow behind it (`lib/run-source.ts`). If either of
// those fails or the host is too old to answer it, every run still lists — with
// its source named by id. A history that refused to render because a card title
// could not be looked up would be withholding the very record it exists to show.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  Activity,
  ArrowLeft,
  Ban,
  CheckCircle2,
  ChevronRight,
  Hourglass,
  Layers,
  Loader2,
  MessageSquare,
  SquareKanban,
  Workflow,
  XCircle,
} from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import {
  getRun,
  isRunOpen,
  listRuns,
  runElapsedMillis,
  RUN_STATUS_LABEL,
  type RunDetail,
  type RunStatus,
  type RunSummary,
} from "@/api/runs";
import { listTasks, type Task } from "@/api/tasks";
import type { DeskDto } from "@/api/types";
import { deskFromDto } from "@/views/chat/model";
import { listWorkflows } from "@/api/workflows";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import { formatUsdCost } from "@/lib/cost";
import {
  runSource,
  RUN_SOURCE_LABEL,
  type RunSource,
  type RunSourceIndex,
  type RunSourceKind,
} from "@/lib/run-source";
import { formatDuration, timeOf } from "@/lib/timeline-format";
import { cn } from "@/lib/utils";
import { startVisiblePolling } from "@/lib/visible-poll";
import { TimelineList, runStatusTone } from "@/views/runs/RunTimeline";

/** How many attempts the history asks for. The host clamps its own ceiling. */
const RUN_PAGE = 50;

/** The list poll, matched to the Task Detail screen's. */
const POLL_MS = 4000;

/**
 * The status buckets the filter offers.
 *
 * Deliberately coarse. A seven-way status filter would be a faithful render of
 * the state machine and useless as a control — the questions an operator
 * actually arrives with are "is it working right now", "is it stuck on me", and
 * "what went wrong", and those are three buckets, not seven. `all` is first
 * because most visits are a scan rather than a search.
 */
const FILTERS: { key: string; label: string; statuses?: RunStatus[] }[] = [
  { key: "all", label: "All" },
  { key: "live", label: "Running", statuses: ["pending", "running"] },
  {
    key: "parked",
    label: "Waiting",
    statuses: ["waiting_approval", "paused", "blocked"],
  },
  { key: "failed", label: "Failed", statuses: ["failed", "cancelled"] },
];

const SOURCE_ICON: Record<RunSourceKind, typeof Workflow> = {
  workflow: Workflow,
  card: SquareKanban,
  chat: MessageSquare,
  unknown: Activity,
};

/** The chip tone for a source kind. Cool, and never competing with a status. */
const SOURCE_TONE: Record<RunSourceKind, string> = {
  workflow: "border-primary/30 text-primary",
  card: "border-muted-foreground/30 text-muted-foreground",
  chat: "border-muted-foreground/30 text-muted-foreground",
  unknown: "border-dashed border-muted-foreground/30 text-muted-foreground",
};

function statusIcon(status: RunStatus) {
  switch (status) {
    case "succeeded":
      return <CheckCircle2 className="size-4" />;
    case "failed":
      return <XCircle className="size-4" />;
    case "cancelled":
    // A by-design decline (issue #1809) is terminal and neutral, so it takes the
    // same quiet icon as a cancel — never the spinning `default`, which would
    // paint a settled attempt as still running.
    case "declined":
      return <Ban className="size-4" />;
    case "waiting_approval":
    case "paused":
    case "blocked":
      return <Hourglass className="size-4" />;
    default:
      return <Loader2 className="size-4 animate-spin" />;
  }
}

/**
 * What to say about an attempt's step count, which is **written on the settle**
 * and so reads `0` for the whole of a live run.
 *
 * The same three honest cases the Task Detail attempts list distinguishes: an
 * attempt that never started is not recording, one that is open but unsettled
 * is recording with the count not yet written, and a settled one carries the
 * real figure — marked `+` when it is a capped high-water ordinal rather than a
 * total.
 */
function stepSummary(run: RunSummary): string {
  if (run.startedAtMillis === undefined) return "not started";
  if (isRunOpen(run) && run.stepCount === 0) return "recording…";
  const n = run.stepCount;
  return `${n}${run.stepCountCapped ? "+" : ""} step${n === 1 ? "" : "s"}`;
}

/**
 * Runs `read`, and answers `null` however it fails.
 *
 * A plain `.catch()` would be *nearly* enough — and the gap is the whole
 * reason this exists. `.catch` handles a rejected promise; it does not handle
 * the call throwing before it returns one, which is what a client missing the
 * method does. Every read behind this helper is decoration on a section that
 * renders without it, so neither failure mode may reach the caller.
 */
async function best<T>(read: () => Promise<T>): Promise<T | null> {
  try {
    return await read();
  } catch {
    return null;
  }
}

/** A date heading, so a long history reads as days rather than as timestamps. */
function dayOf(at: number): string {
  return new Date(at).toLocaleDateString(undefined, {
    weekday: "short",
    month: "short",
    day: "numeric",
  });
}

/**
 * One teammate's attempts, and one attempt's steps.
 *
 * `agentId` is the roster id the host files runs under — the same id the board
 * uses as an assignee and the dispatch path stamps on the row.
 */
export function AgentRuns({
  client,
  company,
  agentId,
  agentName,
}: {
  client: OpenCompanyClient;
  company: string | null;
  agentId: string;
  agentName: string;
}) {
  const [runs, setRuns] = useState<RunSummary[] | null>(null);
  const [failed, setFailed] = useState(false);
  const [index, setIndex] = useState<RunSourceIndex>({});
  const [filter, setFilter] = useState("all");
  const [openId, setOpenId] = useState<string | null>(null);
  const openIdRef = useRef<string | null>(null);
  // Incremented whenever the list effect restarts (teammate switch, filter
  // change). A `read` that started before the change captures the old value
  // and discards its answer once it resolves, so rows fetched for one teammate
  // can never be committed beneath another's name (issue #1671).
  const generationRef = useRef(0);
  // A 1s clock, so a live attempt's elapsed time ticks rather than jumping on
  // the 4s poll. Only mounted while something is actually open — a settled
  // history has nothing that changes second to second.
  const [now, setNow] = useState(() => Date.now());

  // The statuses the active bucket asks for, `undefined` for "all". Sent to the
  // host, not applied here: a desk's history is fetched at `RUN_PAGE`, and
  // filtering a truncated page would make the empty state claim "no attempt
  // matches" while older matching runs sat past the cut (issue #1671).
  const wanted = FILTERS.find((f) => f.key === filter)?.statuses;

  const read = useCallback(async () => {
    const generation = generationRef.current;
    const rows = await listRuns(client, company, {
      agent: agentId,
      // Keep the open run in the refreshed answer so a live run that settles
      // remains inspectable instead of disappearing from its detail panel.
      ...(!openIdRef.current && wanted ? { status: wanted } : {}),
      limit: RUN_PAGE,
    });
    // A read that started before the operator switched teammates must not
    // commit after it: the rows were filtered against the old `agentId` and
    // would render beneath the new teammate's name until the newer read won.
    if (generation !== generationRef.current) return;
    // Belt and braces against a host that predates `?agent=` (and so, being
    // older still, `?status=`): an unrecognised selector is *ignored* rather
    // than refused, so such a host answers with the whole company's newest
    // attempts. Every row would be real and the page would still be a lie.
    // Filtering here costs nothing and makes the section under-report on that
    // host instead of misattributing — which is the right way round.
    const own = Array.isArray(rows)
      ? rows.filter((run) => run.agentId === agentId)
      : [];
    setRuns((prev) => {
      const openId = openIdRef.current;
      // The poll drops the status filter while a run is open so a live run
      // that settles stays inspectable — but a run the operator reached
      // through that filter can sit older than the newest page and fall out of
      // the unfiltered answer, which would close its detail panel on the next
      // refresh. Hold the previously-known summary for the open run instead.
      if (openId && !own.some((run) => run.id === openId)) {
        const held = prev?.find((run) => run.id === openId);
        if (held) return [...own, held];
      }
      return own;
    });
    setFailed(false);
  }, [client, company, agentId, wanted]);

  // The detail read is fresher than the summary the list poll last knew: a run
  // the operator reached through a filter can sit older than the newest page,
  // so its summary is *held* from before — and a held copy would keep showing
  // the status it had the moment the panel opened, settling attempts as live
  // forever. Lift the fresh summary back into the list so the panel's liveness
  // and timings follow the attempt (issue #1671).
  const onRunDetail = useCallback((fresh: RunSummary) => {
    setRuns((prev) =>
      prev
        ? prev.map((run) => (run.id === fresh.id ? fresh : run))
        : prev,
    );
  }, []);

  useEffect(() => {
    const generation = ++generationRef.current;
    setRuns(null);
    // `failed` belongs to the effect that owns the failure — a previous
    // teammate's or filter's error must not paint the new section before its
    // own read has resolved.
    setFailed(false);
    openIdRef.current = null;
    setOpenId(null);
    // A refresh superseded by a teammate switch must not mark the new
    // teammate's section failed on the old one's behalf — that state belongs
    // to the effect that owns the failure.
    const refresh = () =>
      void read().catch(() => {
        if (generation === generationRef.current) setFailed(true);
      });
    refresh();
    return startVisiblePolling(refresh, POLL_MS);
  }, [read]);

  // The source lists. Read once per teammate rather than per poll: a card
  // title and a workflow name do not change on the cadence an attempt's status
  // does, and re-reading the whole board every four seconds to keep a label
  // fresh would cost more than the label is worth.
  useEffect(() => {
    let live = true;
    void (async () => {
      const [tasks, workflows, desks] = await Promise.all([
        best(() => listTasks(client, company)),
        best(() => listWorkflows(client, company)),
        // The desks are what turn a chat run's `chatId` into `#front-desk`
        // rather than `front-desk`. A host that does not expose `…/desks`
        // 404s; the id then stands in for the name, which is what
        // `RunSource.resolved` is for.
        best(() => client.listDesks(company)),
      ]);
      if (!live) return;
      // `Array.isArray` rather than a null check: these reads are best-effort
      // and a *shape* that is not a list — an older host answering an object,
      // a proxy returning an error body with a 200 — must degrade to "no
      // sources" exactly as a rejection does. A throw here would escape into
      // an unhandled rejection and take nothing useful with it, because the
      // section renders every run perfectly well without either list.
      setIndex({
        tasks: Array.isArray(tasks)
          ? new Map<string, Task>(tasks.map((task) => [task.id, task]))
          : undefined,
        workflows: Array.isArray(workflows)
          ? new Map(workflows.map((flow) => [flow.id, flow.name]))
          : undefined,
        // Named the way Chat names them: `deskFromDto` is where the console's
        // channel slug is derived, so `#engineering-desk` here and in the
        // sidebar are the same string by construction rather than by two files
        // agreeing to slugify the same way. A desk's `name` is its *voice*
        // ("Engineering desk") and reads wrong behind a hash.
        chats: Array.isArray(desks)
          ? new Map(
              (desks as DeskDto[]).map((desk) => [
                desk.id,
                `#${deskFromDto(desk).channel}`,
              ]),
            )
          : undefined,
      });
    })();
    return () => {
      live = false;
    };
  }, [client, company]);

  const shown = useMemo(() => {
    if (!runs) return null;
    // Belt and braces on top of the host-side filter: a host that ignored
    // `?status=` (being older than it) answers unfiltered, and the empty-state
    // claim must still be honest on that host.
    return wanted ? runs.filter((run) => wanted.includes(run.status)) : runs;
  }, [runs, wanted]);

  const open = useMemo(
    () => runs?.find((run) => run.id === openId) ?? null,
    [runs, openId],
  );

  const ticking = Boolean(runs?.some(isRunOpen));
  useEffect(() => {
    if (!ticking) return;
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [ticking]);

  if (open) {
    return (
      <RunDetailPanel
        client={client}
        company={company}
        run={open}
        source={runSource(open, index)}
        now={now}
        onDetail={onRunDetail}
        onBack={() => {
          openIdRef.current = null;
          setOpenId(null);
        }}
      />
    );
  }

  return (
    <Card data-testid="agent-runs">
      <CardContent className="space-y-3">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div className="space-y-1">
            <h3 className="font-medium">Runs</h3>
            <p className="text-xs text-muted-foreground">
              Every attempt {agentName} has made, newest first — cards it was
              dispatched, messages it answered, and workflow steps it ran.
            </p>
          </div>
          {/* Visible while a filter is active even if its fetch came back empty —
              a desk that has run but has no *failed* attempts must not strand the
              reader on a filter they cannot turn off. */}
          {runs !== null && (runs.length > 0 || wanted) && (
            <div className="flex flex-wrap gap-1" role="group" aria-label="Filter runs">
              {FILTERS.map((option) => (
                <Button
                  key={option.key}
                  size="sm"
                  variant={filter === option.key ? "secondary" : "ghost"}
                  className="h-7 px-2 text-xs"
                  onClick={() => setFilter(option.key)}
                  data-testid={`agent-runs-filter-${option.key}`}
                >
                  {option.label}
                </Button>
              ))}
            </div>
          )}
        </div>

        {runs && runs.length > 0 && <RunTotals runs={runs} />}

        {runs === null && !failed && (
          <div className="space-y-1.5">
            <Skeleton className="h-14 rounded-lg" />
            <Skeleton className="h-14 rounded-lg" />
            <Skeleton className="h-14 rounded-lg" />
          </div>
        )}

        {failed && (
          <Note>
            The company host didn't answer for this teammate's runs. It may not
            record them yet, or the read may have failed — either way nothing
            here has been lost.
          </Note>
        )}

        {runs?.length === 0 && !failed && (
          <Note>
            {wanted
              ? "No attempt in this history matches that filter."
              : `${agentName} hasn't run yet. An attempt is recorded the first time a card is ` +
                `dispatched to this teammate, or the first time it answers a message.`}
          </Note>
        )}

        {shown?.length === 0 && runs !== null && runs.length > 0 && (
          <Note>No attempt in this history matches that filter.</Note>
        )}

        {shown && shown.length > 0 && (
          <RunList
            runs={shown}
            index={index}
            now={now}
            onOpen={(id) => {
              openIdRef.current = id;
              setOpenId(id);
            }}
          />
        )}
      </CardContent>
    </Card>
  );
}

/** A quiet inline note. Smaller than the page's `EmptyState` cards, which are
 * whole-screen states; this one sits inside a section that has other content. */
function Note({ children }: { children: React.ReactNode }) {
  return (
    <p className="rounded-lg border border-dashed px-3 py-4 text-center text-xs text-muted-foreground">
      {children}
    </p>
  );
}

/**
 * The history's totals.
 *
 * Counted over the attempts **on this page**, and the wording says so, because
 * the host clamps the read: "12 of the last 50" is a fact, "12" alone would be
 * a claim about all time that the console cannot make.
 */
function RunTotals({ runs }: { runs: RunSummary[] }) {
  const totals = useMemo(() => {
    let succeeded = 0;
    let failedCount = 0;
    let open = 0;
    let costUsd = 0;
    let tokens = 0;
    for (const run of runs) {
      if (run.status === "succeeded") succeeded += 1;
      if (run.status === "failed" || run.status === "cancelled") failedCount += 1;
      if (isRunOpen(run)) open += 1;
      costUsd += run.usage.costUsd;
      tokens += run.usage.input + run.usage.output;
    }
    return { succeeded, failedCount, open, costUsd, tokens };
  }, [runs]);

  return (
    <dl
      className="grid grid-cols-2 gap-2 sm:grid-cols-4"
      data-testid="agent-runs-totals"
    >
      <Stat label="Attempts" value={String(runs.length)} />
      <Stat
        label="Succeeded"
        value={String(totals.succeeded)}
        tone="text-status-done-text"
      />
      <Stat
        label={totals.open > 0 ? "Open" : "Failed"}
        value={String(totals.open > 0 ? totals.open : totals.failedCount)}
        tone={
          totals.open > 0 ? "text-status-running-text" : "text-status-failed-text"
        }
      />
      {/* Cost is settled-only, like the token figures it sits beside — a live
          attempt contributes nothing until it settles. Rendered as "—" rather
          than "$0.00" when the whole page is unsettled, because a zero here
          would read as free work rather than as unbilled-so-far. */}
      <Stat
        label="Cost"
        value={
          totals.costUsd > 0
            ? (formatUsdCost({ amountUsd: totals.costUsd }, "total") ?? "—")
            : "—"
        }
      />
    </dl>
  );
}

function Stat({
  label,
  value,
  tone,
}: {
  label: string;
  value: string;
  tone?: string;
}) {
  return (
    <div className="rounded-lg border bg-muted/30 px-3 py-2">
      <dt className="text-2xs uppercase tracking-wide text-muted-foreground">
        {label}
      </dt>
      <dd className={cn("text-lg font-semibold tabular-nums", tone)}>{value}</dd>
    </div>
  );
}

/** The attempts, grouped under a heading per calendar day. */
function RunList({
  runs,
  index,
  now,
  onOpen,
}: {
  runs: RunSummary[];
  index: RunSourceIndex;
  now: number;
  onOpen: (id: string) => void;
}) {
  const days = useMemo(() => {
    const out: { day: string; runs: RunSummary[] }[] = [];
    for (const run of runs) {
      const day = dayOf(run.createdAtMillis);
      const last = out[out.length - 1];
      if (last && last.day === day) last.runs.push(run);
      else out.push({ day, runs: [run] });
    }
    return out;
  }, [runs]);

  return (
    <div className="space-y-3">
      {days.map((group) => (
        <div key={group.day} className="space-y-1.5">
          <p className="text-2xs font-medium uppercase tracking-wide text-muted-foreground">
            {group.day}
          </p>
          <ol className="space-y-1.5">
            {group.runs.map((run) => (
              <RunRow
                key={run.id}
                run={run}
                source={runSource(run, index)}
                now={now}
                onOpen={() => onOpen(run.id)}
              />
            ))}
          </ol>
        </div>
      ))}
    </div>
  );
}

function SourceChip({ source }: { source: RunSource }) {
  const Icon = SOURCE_ICON[source.kind];
  return (
    <Badge
      variant="outline"
      className={cn("shrink-0 gap-1 font-normal", SOURCE_TONE[source.kind])}
    >
      <Icon className="size-3" aria-hidden /> {RUN_SOURCE_LABEL[source.kind]}
    </Badge>
  );
}

function RunRow({
  run,
  source,
  now,
  onOpen,
}: {
  run: RunSummary;
  source: RunSource;
  now: number;
  onOpen: () => void;
}) {
  const elapsed = runElapsedMillis(run, now);
  return (
    <li className="rounded-lg border bg-card">
      <button
        className="flex w-full cursor-pointer flex-col gap-1 px-3 py-2 text-left"
        onClick={onOpen}
        data-testid={`agent-run-${run.id}`}
      >
        <div className="flex w-full items-center gap-2 text-xs">
          <span className={cn("shrink-0", runStatusTone(run.status))}>
            {statusIcon(run.status)}
          </span>
          <SourceChip source={source} />
          <span
            className={cn(
              "min-w-0 flex-1 truncate font-medium",
              // An id standing in for a name is set in mono and never dressed
              // up as a title — see `RunSource.resolved`.
              !source.resolved && "font-mono text-2xs text-muted-foreground",
            )}
          >
            {source.label}
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
        <div className="flex w-full flex-wrap items-center gap-2 pl-6 text-2xs text-muted-foreground">
          <Badge
            variant="outline"
            className={cn("shrink-0 font-normal", runStatusTone(run.status))}
          >
            {RUN_STATUS_LABEL[run.status]}
          </Badge>
          <span className="tabular-nums">{timeOf(run.createdAtMillis)}</span>
          <span aria-hidden>·</span>
          <span>Attempt {run.attempt}</span>
          <span aria-hidden>·</span>
          <span>{stepSummary(run)}</span>
          {formatUsdCost({ amountUsd: run.usage.costUsd }, "line") && (
            <>
              <span aria-hidden>·</span>
              <span className="tabular-nums">
                {formatUsdCost({ amountUsd: run.usage.costUsd }, "line")}
              </span>
            </>
          )}
        </div>
        {source.detail && (
          <p className="w-full truncate pl-6 text-2xs text-muted-foreground">
            {source.detail}
          </p>
        )}
        {run.error && (
          <p className="w-full truncate pl-6 text-2xs text-status-failed-text">
            {run.error}
          </p>
        )}
      </button>
    </li>
  );
}

/**
 * One attempt, in full: what set it going, how it went, and every step it took.
 *
 * **Refresh-on-read.** Steps land in the store as the turn executes, so
 * re-reading an open attempt shows the progress made since. This re-fetches on
 * the list's own cadence while the run is unsettled and stops once it is —
 * reading liveness from the summary the list keeps fresh, not from its own last
 * answer, so the poll ends even if the detail read is lagging behind.
 */
function RunDetailPanel({
  client,
  company,
  run,
  source,
  now,
  onDetail,
  onBack,
}: {
  client: OpenCompanyClient;
  company: string | null;
  run: RunSummary;
  source: RunSource;
  now: number;
  /**
   * The freshest summary this attempt's detail read has seen. The panel itself
   * renders from the `run` prop (the list's copy, which a poll may have to
   * *hold* for a run older than the newest page); handing the fresh copy up
   * lets the list's holder replace the stale one.
   */
  onDetail?: (run: RunSummary) => void;
  onBack: () => void;
}) {
  const [detail, setDetail] = useState<RunDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const live = isRunOpen(run);
  const runId = run.id;

  useEffect(() => {
    let cancelled = false;
    const read = async () => {
      try {
        const next = await getRun(client, company, runId);
        if (cancelled) return;
        setDetail(next);
        setError(null);
        onDetail?.(next.run);
      } catch (e) {
        if (!cancelled)
          setError(e instanceof Error ? e.message : "could not load this attempt");
      }
    };
    void read();
    if (!live) return () => void (cancelled = true);
    const stop = startVisiblePolling(() => void read(), POLL_MS);
    return () => {
      cancelled = true;
      stop();
    };
  }, [client, company, runId, live, onDetail]);

  const elapsed = runElapsedMillis(run, now);
  const Icon = SOURCE_ICON[source.kind];

  return (
    <Card data-testid="agent-run-detail">
      <CardContent className="space-y-4">
        <Button
          variant="ghost"
          size="sm"
          className="-ml-2 h-7 px-2 text-muted-foreground"
          onClick={onBack}
          data-testid="agent-run-back"
        >
          <ArrowLeft className="size-3.5" /> All runs
        </Button>

        <div className="space-y-2">
          <div className="flex flex-wrap items-center gap-2">
            <span className={cn("shrink-0", runStatusTone(run.status))}>
              {statusIcon(run.status)}
            </span>
            <h3 className="min-w-0 flex-1 truncate text-lg font-medium">
              {RUN_SOURCE_LABEL[source.kind]} · Attempt {run.attempt}
            </h3>
            <Badge
              variant="outline"
              className={cn("font-normal", runStatusTone(run.status))}
            >
              {RUN_STATUS_LABEL[run.status]}
            </Badge>
          </div>
          {/* The source, as the one thing on this panel worth clicking through
              to. An unresolved source still renders — as its id, and without a
              claim about what it was named. */}
          <a
            href={source.href}
            className={cn(
              "flex items-center gap-2 rounded-lg border px-3 py-2 text-sm",
              source.href ? "hover:bg-muted/50" : "pointer-events-none",
            )}
            data-testid="agent-run-source"
          >
            <Icon className="size-4 shrink-0 text-muted-foreground" aria-hidden />
            <span className="min-w-0 flex-1">
              <span
                className={cn(
                  "block truncate",
                  source.resolved ? "font-medium" : "font-mono text-xs",
                )}
              >
                {source.label}
              </span>
              {source.detail && (
                <span className="block truncate text-xs text-muted-foreground">
                  {source.detail}
                </span>
              )}
            </span>
            {source.href && (
              <ChevronRight className="size-4 shrink-0 text-muted-foreground" />
            )}
          </a>
        </div>

        <dl className="grid grid-cols-2 gap-2 sm:grid-cols-4">
          <Stat label="Started" value={timeOf(run.createdAtMillis)} />
          <Stat
            label={run.phase === "terminal" ? "Took" : "Running for"}
            value={elapsed === null ? "—" : formatDuration(elapsed)}
          />
          <Stat label="Steps" value={stepSummary(run)} />
          <Stat
            label="Tokens"
            value={
              run.usage.input + run.usage.output > 0
                ? (run.usage.input + run.usage.output).toLocaleString()
                : "—"
            }
          />
        </dl>

        {run.error && (
          <p className="rounded-lg border border-status-failed/40 bg-status-failed-soft px-3 py-2 text-xs text-status-failed-text">
            {run.error}
          </p>
        )}
        {error && (
          <p className="rounded-lg border border-status-failed/40 bg-status-failed-soft px-3 py-2 text-xs text-status-failed-text">
            {error}
          </p>
        )}
        {run.stepCountCapped && (
          <p className="text-2xs text-muted-foreground">
            This attempt hit the per-run trace ceiling, so what follows is the
            start of the run, not all of it.
          </p>
        )}

        <div className="space-y-2">
          <p className="flex items-center gap-1.5 text-xs font-medium">
            <Layers className="size-3.5 text-muted-foreground" aria-hidden /> Turns
          </p>
          {detail === null && error === null ? (
            <div className="space-y-1.5">
              <Skeleton className="h-9 rounded-lg" />
              <Skeleton className="h-9 rounded-lg" />
              <Skeleton className="h-9 rounded-lg" />
            </div>
          ) : (
            detail && (
              <TimelineList
                entries={detail.steps}
                empty={
                  <Note>
                    {live
                      ? "Steps appear here as the attempt runs."
                      : "This attempt settled without producing a traceable step."}
                  </Note>
                }
              />
            )
          )}
        </div>
      </CardContent>
    </Card>
  );
}
