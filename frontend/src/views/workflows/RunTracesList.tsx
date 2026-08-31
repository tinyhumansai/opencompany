// The company-wide run traces list (issue #1697): every workflow's runs, in
// one table — workflow, trigger, started at, status — rather than scattered
// across each workflow's own history rail. Clicking a row opens its
// transcript in a side sheet (`RunTraceSheet`) instead of navigating into the
// graph editor, which is what the run's OTHER deep link (`?run=<id>`) still
// does.
//
// Reads the same company-wide run page the index's health strips already
// fetch (`WorkflowsView`'s `indexRuns`, capped at 200 rows) — sorting and
// filtering below are client-side over that one bounded page, not a new
// request per interaction.

import { useEffect, useMemo, useState } from "react";
import { ArrowDown, ArrowUp, ArrowUpDown, ListFilter } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuCheckboxItem,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Skeleton } from "@/components/ui/skeleton";
import type {
  WorkflowRunOutcome,
  WorkflowRunVerdict,
  WorkflowSummary,
} from "@/api/workflows";

import { useRunningClock } from "./RunHistoryPanel";
import {
  VERDICT_TONE,
  formatDuration,
  isRunning,
  relativeTime,
  runDuration,
  runTone,
  verdictOf,
} from "./run-health";

/** How many rows {@link listWorkflowRuns} was asked for — the same limit
 * `WorkflowsView` passes when it fetches the company-wide page this list
 * renders. Only used here to word the "more may exist" footnote honestly. */
const COMPANY_RUN_PAGE_LIMIT = 200;

/** A run's start, falling back to its finish for a row journaled before issue
 * #371 recorded one — the same fallback {@link runDuration} takes. Named once
 * because sorting, the time-range filter and the "Started at" cell all have to
 * agree on which timestamp a run's row stands for.
 *
 * Exported for the unit suite — see `run-traces-list.test.ts`. */
export function startedAt(run: WorkflowRunOutcome): number {
  return run.startedAtMillis ?? run.atMillis;
}

export type SortKey = "workflow" | "trigger" | "startedAt" | "status";
export type SortDir = "asc" | "desc";

/** The closed set of time windows the range filter offers, oldest cutoff last.
 * `null` is "All time" — no cutoff. */
const TIME_RANGES: { value: string; label: string; ms: number | null }[] = [
  { value: "6h", label: "Last 6h", ms: 6 * 60 * 60 * 1000 },
  { value: "24h", label: "Last 24h", ms: 24 * 60 * 60 * 1000 },
  { value: "7d", label: "Last 7d", ms: 7 * 24 * 60 * 60 * 1000 },
  { value: "all", label: "All time", ms: null },
];

/** The closed set of verdicts, in {@link VERDICT_TONE}'s own order — the same
 * severity-ish ordering the status column sorts by, so the filter list and a
 * status-sorted table read the same way. */
const VERDICTS = Object.keys(VERDICT_TONE) as WorkflowRunVerdict[];

/** What the three facets narrow a run against. `rangeMs: null` is "All time".
 * Pure and exported (issue #1697 review: this logic had no test coverage) so
 * the unit suite can pin the filter predicate without mounting the list. */
export interface RunTraceFilterState {
  now: number;
  rangeMs: number | null;
  workflowFilter: Set<string>;
  verdictFilter: Set<WorkflowRunVerdict>;
}

/** Whether one run survives the traces list's three filters. */
export function runMatchesFilters(
  run: WorkflowRunOutcome,
  filters: RunTraceFilterState,
): boolean {
  if (filters.rangeMs != null && filters.now - startedAt(run) > filters.rangeMs) {
    return false;
  }
  if (filters.workflowFilter.size > 0 && !filters.workflowFilter.has(run.workflowId)) {
    return false;
  }
  if (filters.verdictFilter.size > 0 && !filters.verdictFilter.has(verdictOf(run))) {
    return false;
  }
  return true;
}

/** The traces table's comparator, keyed by column. Exported for the same
 * reason as {@link runMatchesFilters} — this is where the "first click on
 * Status must put the most-severe verdicts first" rule actually lives, and it
 * is exactly the kind of one-line sign error a mount-and-screenshot test
 * would not have caught. */
export function compareRuns(
  a: WorkflowRunOutcome,
  b: WorkflowRunOutcome,
  sortKey: SortKey,
  sortDir: SortDir,
  nameById: Map<string, string>,
): number {
  const dir = sortDir === "asc" ? 1 : -1;
  const rank = (run: WorkflowRunOutcome) => VERDICTS.indexOf(verdictOf(run));
  switch (sortKey) {
    case "workflow":
      return (
        dir *
        (nameById.get(a.workflowId) ?? a.workflowId).localeCompare(
          nameById.get(b.workflowId) ?? b.workflowId,
        )
      );
    case "trigger":
      return dir * (Number(a.scheduled) - Number(b.scheduled));
    case "status":
      // `a`/`b` swapped rather than `dir` alone (issue #1697 review):
      // `VERDICTS` lists the states worth a person's attention FIRST
      // (`running`, `failed`, …) and `ok` last, so a plain rank difference
      // sorts `ok` to the top on the very first click, which is the one
      // direction a "descending" status sort must not read as. Swapping
      // puts the highest-attention rows first on that first click, and the
      // reverse — `ok` first — one more click away.
      return dir * (rank(b) - rank(a));
    case "startedAt":
    default:
      return dir * (startedAt(a) - startedAt(b));
  }
}

export function RunTracesList({
  runs,
  workflows,
  company,
  loading,
  onSelectRun,
}: {
  /** The company-wide run page, newest first — unscoped by workflow. */
  runs: WorkflowRunOutcome[];
  /** For resolving a run's `workflowId` to a name. */
  workflows: WorkflowSummary[];
  /** Company identity, used to reset company-scoped facets only on a switch. */
  company: string | null;
  loading: boolean;
  /** Open this run's transcript in the side sheet. Does NOT navigate. */
  onSelectRun: (run: WorkflowRunOutcome) => void;
}) {
  const nameById = useMemo(
    () => new Map(workflows.map((w) => [w.id, w.name])),
    [workflows],
  );
  const [sortKey, setSortKey] = useState<SortKey>("startedAt");
  const [sortDir, setSortDir] = useState<SortDir>("desc");
  const [timeRange, setTimeRange] = useState<string>("all");
  const [workflowFilter, setWorkflowFilter] = useState<Set<string>>(new Set());
  const [verdictFilter, setVerdictFilter] = useState<Set<WorkflowRunVerdict>>(
    new Set(),
  );

  const toggleSort = (key: SortKey) => {
    if (key === sortKey) {
      setSortDir((d) => (d === "asc" ? "desc" : "asc"));
    } else {
      setSortKey(key);
      // Newest/most-severe-first reads naturally on a first click for every
      // column here — nobody sorting a run table wants the oldest or the
      // quietest row on top by default.
      setSortDir("desc");
    }
  };

  // The two facets' option lists. Workflow options come from the runs
  // actually on this page — a checkbox for a workflow with zero runs here
  // would filter to nothing, which is not a choice worth offering. Verdicts
  // are the full closed set regardless: a filter that only shows the
  // verdicts already present would hide the option to look for one that
  // legitimately has none, which is indistinguishable from the filter itself
  // being broken.
  const workflowOptions = useMemo(() => {
    const ids = new Set(runs.map((r) => r.workflowId));
    return [...ids]
      .map((id) => ({ id, name: nameById.get(id) ?? id }))
      .sort((a, b) => a.name.localeCompare(b.name));
  }, [runs, nameById]);

  const rangeMs = TIME_RANGES.find((r) => r.value === timeRange)?.ms ?? null;
  const now = useRunningClock(runs.some(isRunning) || rangeMs !== null);

  useEffect(() => {
    setWorkflowFilter(new Set());
  }, [company]);

  const filtered = useMemo(
    () => runs.filter((run) => runMatchesFilters(run, { now, rangeMs, workflowFilter, verdictFilter })),
    [runs, rangeMs, now, workflowFilter, verdictFilter],
  );

  const sorted = useMemo(
    () => [...filtered].sort((a, b) => compareRuns(a, b, sortKey, sortDir, nameById)),
    [filtered, sortKey, sortDir, nameById],
  );

  const activeFilterCount =
    (rangeMs != null ? 1 : 0) + workflowFilter.size + verdictFilter.size;

  return (
    <div className="flex h-full flex-col gap-3 overflow-hidden p-4" data-testid="workflow-run-traces">
      <RunTraceFilters
        timeRange={timeRange}
        onTimeRange={setTimeRange}
        workflowOptions={workflowOptions}
        workflowFilter={workflowFilter}
        onWorkflowFilter={setWorkflowFilter}
        verdictFilter={verdictFilter}
        onVerdictFilter={setVerdictFilter}
      />
      <div className="min-h-0 flex-1 overflow-auto">
        {loading ? (
          <div className="space-y-2">
            {[0, 1, 2].map((i) => (
              <Skeleton key={i} className="h-11 rounded-lg" />
            ))}
          </div>
        ) : runs.length === 0 ? (
          <p className="text-sm text-muted-foreground">
            No workflow runs yet. Runs appear here once a workflow fires —
            including scheduled ones that run while you're away.
          </p>
        ) : sorted.length === 0 ? (
          <p className="text-sm text-muted-foreground" data-testid="workflow-run-traces-empty-filtered">
            No runs match the current filters.
          </p>
        ) : (
          <>
            {/* Wide rather than responsive-collapsing (issue #1697): a
                sortable header has to line up with its column at every width,
                which a breakpoint-swapped grid can't promise without
                duplicating the header too. Scrolls inside its own container
                instead — the page never scrolls sideways. */}
            <div className="min-w-[42rem] overflow-x-auto rounded-xl border">
              <RunTraceHeader sortKey={sortKey} sortDir={sortDir} onSort={toggleSort} />
              <div className="divide-y">
                {sorted.map((run) => (
                  <RunTraceRow
                    key={run.seq}
                    run={run}
                    workflowName={nameById.get(run.workflowId) ?? run.workflowId}
                    now={now}
                    onSelect={() => onSelectRun(run)}
                  />
                ))}
              </div>
            </div>
            {/* Honest about the cap (issue #1012's reasoning, restated here):
                the company-wide page is cut at a limit, so silence past it
                would read as "that's everything" when it may not be. Reads
                the unfiltered count — filters narrow what's SHOWN, not what
                was fetched. */}
            {runs.length >= COMPANY_RUN_PAGE_LIMIT && (
              <p className="mt-2 text-2xs text-muted-foreground">
                Showing the most recent {COMPANY_RUN_PAGE_LIMIT} runs across all
                workflows{activeFilterCount > 0 ? " before filtering" : ""}. Open
                a workflow's own history for its full trail.
              </p>
            )}
          </>
        )}
      </div>
    </div>
  );
}

const GRID_COLS =
  "grid grid-cols-[minmax(14rem,1fr)_7rem_11rem_9rem] items-center gap-x-3 px-3";

/** The sortable column headers. A `<div>` grid, not a `<table>`: the data
 * rows below are `<button>`s the whole row's width (the click target for
 * opening a transcript), and a `<button>` cannot sit inside a `<td>` without
 * the row/cell semantics fighting the click target — the same reason
 * `WorkflowIndex`'s list is a styled grid rather than a table. This header
 * shares {@link GRID_COLS} with the rows so the two can never drift apart. */
function RunTraceHeader({
  sortKey,
  sortDir,
  onSort,
}: {
  sortKey: SortKey;
  sortDir: SortDir;
  onSort: (key: SortKey) => void;
}) {
  const columns: { key: SortKey; label: string; align?: "right" }[] = [
    { key: "workflow", label: "Workflow" },
    { key: "trigger", label: "Trigger" },
    { key: "startedAt", label: "Started at" },
    { key: "status", label: "Status", align: "right" },
  ];
  return (
    <div
      className={`${GRID_COLS} border-b bg-muted/30 py-1.5`}
      data-testid="workflow-run-traces-header"
    >
      {columns.map(({ key, label, align }) => (
        <button
          key={key}
          type="button"
          onClick={() => onSort(key)}
          data-testid={`workflow-run-traces-sort-${key}`}
          aria-label={
            sortKey === key
              ? `${label}, sorted ${sortDir === "asc" ? "ascending" : "descending"}. Reverse the order.`
              : `${label}, not sorted. Sort by ${label}.`
          }
          className={`flex items-center gap-1 text-2xs font-medium text-muted-foreground hover:text-foreground ${
            align === "right" ? "justify-end" : ""
          }`}
        >
          {label}
          {sortKey === key ? (
            sortDir === "asc" ? (
              <ArrowUp className="size-3" />
            ) : (
              <ArrowDown className="size-3" />
            )
          ) : (
            <ArrowUpDown className="size-3 opacity-40" />
          )}
        </button>
      ))}
    </div>
  );
}

/** One run as a table row — the four facts a trace asks for: which workflow,
 * what fired it, when it started, and how it went — plus the duration folded
 * into the started-at cell rather than a fifth column. */
function RunTraceRow({
  run,
  workflowName,
  now,
  onSelect,
}: {
  run: WorkflowRunOutcome;
  workflowName: string;
  now: number;
  onSelect: () => void;
}) {
  const tone = runTone(run);
  const duration = runDuration(run, now);
  return (
    <button
      type="button"
      onClick={onSelect}
      data-testid="workflow-run-trace-row"
      className={`${GRID_COLS} w-full py-2 text-left transition hover:bg-accent/40`}
    >
      <span className="min-w-0 truncate text-sm font-medium" title={workflowName}>
        {workflowName}
      </span>
      <span className="truncate text-2xs text-muted-foreground">
        {run.scheduled ? "Scheduled" : "Manual"}
      </span>
      <span
        className="truncate text-2xs text-muted-foreground"
        title={new Date(startedAt(run)).toLocaleString()}
      >
        {relativeTime(startedAt(run))}
        {duration != null && (
          <>
            {" · "}
            {isRunning(run) ? "running " : ""}
            {formatDuration(duration)}
          </>
        )}
      </span>
      <span className="flex items-center justify-end gap-1.5 text-2xs">
        <span className={`size-1.5 shrink-0 rounded-full ${tone.dot}`} />
        <span className="truncate text-muted-foreground">{tone.label}</span>
      </span>
    </button>
  );
}

/** The filter toolbar: a time-range segmented control plus two checkbox-list
 * facets (workflow, status). All three narrow the same client-side page —
 * see the module header for why that's the right cost to pay here. */
function RunTraceFilters({
  timeRange,
  onTimeRange,
  workflowOptions,
  workflowFilter,
  onWorkflowFilter,
  verdictFilter,
  onVerdictFilter,
}: {
  timeRange: string;
  onTimeRange: (value: string) => void;
  workflowOptions: { id: string; name: string }[];
  workflowFilter: Set<string>;
  onWorkflowFilter: (next: Set<string>) => void;
  verdictFilter: Set<WorkflowRunVerdict>;
  onVerdictFilter: (next: Set<WorkflowRunVerdict>) => void;
}) {
  const toggle = <T,>(set: Set<T>, value: T): Set<T> => {
    const next = new Set(set);
    if (next.has(value)) next.delete(value);
    else next.add(value);
    return next;
  };

  return (
    <div className="flex flex-wrap items-center gap-2">
      <div className="flex items-center gap-1 rounded-lg border p-0.5">
        {TIME_RANGES.map(({ value, label }) => (
          <Button
            key={value}
            size="sm"
            variant={timeRange === value ? "secondary" : "ghost"}
            className="h-7 px-2 text-2xs"
            onClick={() => onTimeRange(value)}
            aria-pressed={timeRange === value}
            data-testid={`workflow-run-traces-range-${value}`}
          >
            {label}
          </Button>
        ))}
      </div>

      <DropdownMenu>
        <DropdownMenuTrigger
          render={
            <Button
              size="sm"
              variant="outline"
              className="h-7 gap-1.5 px-2 text-2xs"
              data-testid="workflow-run-traces-filter-workflow"
            />
          }
        >
          <ListFilter className="size-3.5" />
          Workflow
          {workflowFilter.size > 0 && (
            <Badge variant="secondary" className="h-4 px-1 text-3xs font-normal">
              {workflowFilter.size}
            </Badge>
          )}
        </DropdownMenuTrigger>
        <DropdownMenuContent align="start">
          <DropdownMenuGroup>
            <DropdownMenuLabel>Workflow</DropdownMenuLabel>
            {workflowOptions.length === 0 ? (
              <DropdownMenuItem disabled>No runs yet</DropdownMenuItem>
            ) : (
              workflowOptions.map((w) => (
                <DropdownMenuCheckboxItem
                  key={w.id}
                  checked={workflowFilter.has(w.id)}
                  onCheckedChange={() => onWorkflowFilter(toggle(workflowFilter, w.id))}
                >
                  {w.name}
                </DropdownMenuCheckboxItem>
              ))
            )}
          </DropdownMenuGroup>
          {workflowFilter.size > 0 && (
            <>
              <DropdownMenuSeparator />
              <DropdownMenuItem onClick={() => onWorkflowFilter(new Set())}>
                Clear
              </DropdownMenuItem>
            </>
          )}
        </DropdownMenuContent>
      </DropdownMenu>

      <DropdownMenu>
        <DropdownMenuTrigger
          render={
            <Button
              size="sm"
              variant="outline"
              className="h-7 gap-1.5 px-2 text-2xs"
              data-testid="workflow-run-traces-filter-status"
            />
          }
        >
          <ListFilter className="size-3.5" />
          Status
          {verdictFilter.size > 0 && (
            <Badge variant="secondary" className="h-4 px-1 text-3xs font-normal">
              {verdictFilter.size}
            </Badge>
          )}
        </DropdownMenuTrigger>
        <DropdownMenuContent align="start">
          <DropdownMenuGroup>
            <DropdownMenuLabel>Status</DropdownMenuLabel>
            {VERDICTS.map((v) => (
              <DropdownMenuCheckboxItem
                key={v}
                checked={verdictFilter.has(v)}
                onCheckedChange={() => onVerdictFilter(toggle(verdictFilter, v))}
              >
                <span className={`mr-1.5 inline-block size-1.5 rounded-full ${VERDICT_TONE[v].dot}`} />
                {VERDICT_TONE[v].label}
              </DropdownMenuCheckboxItem>
            ))}
          </DropdownMenuGroup>
          {verdictFilter.size > 0 && (
            <>
              <DropdownMenuSeparator />
              <DropdownMenuItem onClick={() => onVerdictFilter(new Set())}>
                Clear
              </DropdownMenuItem>
            </>
          )}
        </DropdownMenuContent>
      </DropdownMenu>
    </div>
  );
}
