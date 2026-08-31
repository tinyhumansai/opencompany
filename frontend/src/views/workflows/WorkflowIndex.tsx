// Issue #303: the company's workflows as CARDS or as a LIST.
//
// Issue #1110 promoted it from a panel that opened over the canvas to the body
// of `#/workflows` itself, which is why it no longer draws a header: the tab's
// own toolbar carries the title, the count, the Cards/List toggle and New
// workflow, and this renders the list under it.
//
// Why this exists at all: before #303 the only way to see the company's
// workflows was the `<Select>` in the toolbar — a combobox shows one workflow at
// a time and carries nothing but a name, so "which of these is failing?" had no
// answer short of selecting each in turn and reading its chip.
//
// WHAT A CARD IS ALLOWED TO SAY is the whole design constraint here. Two
// requests back this surface — `GET …/workflows` (including schedule and node
// count on current hosts) and `GET …/workflows/runs` (the company-wide run
// journal) — and a card renders strictly what those two carry. The widened
// summary avoids an N+1 full-graph request per card. An older host omits both
// fields; omission renders nothing rather than inventing "manual" or zero steps.
//
// It does say whether a schedule is OFF (issue #1209), because `enabled` is on
// the list wire and needs no second request. The two are different claims: "it
// runs at 02:15" needs the graph, "it will not start on its own" does not. The
// host disarms every workflow created with a schedule (#276's rule), so a
// company's scheduled workflows arrive paused — and without this badge an
// operator who authored one is shown a row indistinguishable from the ones that
// do fire. The detail header has said it since #276 (`workflow-paused-badge`);
// this is the same reading, on the surface an operator meets first.

import type { WorkflowRunOutcome, WorkflowSummary } from "@/api/workflows";
import { Badge } from "@/components/ui/badge";
import { Skeleton } from "@/components/ui/skeleton";

import { failedNodeOf } from "./graph";
import {
  pendingCount,
  relativeTime,
  runSummaryLine,
  runTone,
  undeliveredCount,
} from "./run-health";

/** Which rendering the index is showing. Persisted by the caller. */
export type IndexMode = "cards" | "list";

/** The one sentence a paused row is allowed to say, and the way out of it.
 * Named once because a card and a list row both carry it (the same reason
 * {@link NO_RUNS_LABEL} is a constant) and two copies would drift. */
const PAUSED_TITLE =
  "This workflow's schedule is off, so it won't start on its own. " +
  "Open it and press Resume to arm it.";

/**
 * The "this will not fire" badge, or `null` when the workflow is armed.
 *
 * Issue #276's rule, repeated exactly as the `editable` checks beside it
 * repeat #259's: only an explicit `false` is off. A host predating #276 sends no
 * field, and `undefined` must not render every workflow as paused.
 */
function PausedBadge({ enabled }: { enabled: boolean | undefined }) {
  if (enabled !== false) return null;
  return (
    <Badge
      variant="outline"
      className="h-4 shrink-0 px-1.5 text-3xs font-normal border-status-blocked/40 bg-status-blocked-soft"
      title={PAUSED_TITLE}
      data-testid="workflow-index-paused"
    >
      Paused
    </Badge>
  );
}

const WEEKDAY_NAMES: Record<string, string> = {
  "0": "Sunday",
  "7": "Sunday",
  SUN: "Sunday",
  "1": "Monday",
  MON: "Monday",
  "2": "Tuesday",
  TUE: "Tuesday",
  "3": "Wednesday",
  WED: "Wednesday",
  "4": "Thursday",
  THU: "Thursday",
  "5": "Friday",
  FRI: "Friday",
  "6": "Saturday",
  SAT: "Saturday",
};

/**
 * The index's prose reading of a summary schedule.
 *
 * `null` is current-host knowledge that the workflow is manual. `undefined` is
 * no knowledge at all from an older host, and therefore renders nothing.
 */
export function workflowTriggerLine(schedule: string | null | undefined): string | null {
  if (schedule === undefined) return null;
  if (schedule === null || !schedule.trim()) return "Runs on request";

  const cron = schedule.trim().replace(/\s+/g, " ");
  const [minute, hour, dayOfMonth, month, dayOfWeek] = cron.split(" ");
  const number = (value: string | undefined, ceiling: number) => {
    if (!value || !/^\d+$/.test(value)) return null;
    const parsed = Number(value);
    return parsed >= 0 && parsed <= ceiling ? parsed : null;
  };
  const minuteNumber = number(minute, 59);
  const hourNumber = number(hour, 23);
  const clock =
    minuteNumber !== null && hourNumber !== null
      ? `${String(hourNumber).padStart(2, "0")}:${String(minuteNumber).padStart(2, "0")}`
      : null;

  if (minute === "*" && hour === "*" && dayOfMonth === "*" && month === "*" && dayOfWeek === "*") {
    return "Runs every minute";
  }
  if (
    minuteNumber !== null &&
    hour === "*" &&
    dayOfMonth === "*" &&
    month === "*" &&
    dayOfWeek === "*"
  ) {
    return minuteNumber === 0
      ? "Runs hourly on the hour"
      : `Runs hourly at ${String(minuteNumber).padStart(2, "0")} minutes past`;
  }
  if (clock && dayOfMonth === "*" && month === "*" && dayOfWeek === "*") {
    return `Runs daily at ${clock} UTC`;
  }
  const weekday = dayOfWeek ? WEEKDAY_NAMES[dayOfWeek.toUpperCase()] : undefined;
  if (clock && dayOfMonth === "*" && month === "*" && weekday) {
    return `Runs every ${weekday} at ${clock} UTC`;
  }
  const monthDay = number(dayOfMonth, 31);
  if (clock && monthDay !== null && monthDay > 0 && month === "*" && dayOfWeek === "*") {
    return `Runs monthly on day ${monthDay} at ${clock} UTC`;
  }
  return "Runs automatically on a custom schedule";
}

/** Facts shared verbatim by card and list layouts. */
function WorkflowFacts({ workflow }: { workflow: WorkflowSummary }) {
  const trigger = workflowTriggerLine(workflow.schedule);
  const steps =
    workflow.nodeCount === undefined
      ? null
      : `${workflow.nodeCount} ${workflow.nodeCount === 1 ? "step" : "steps"}`;
  if (!trigger && !steps) return null;
  return (
    <p
      className="truncate text-2xs text-muted-foreground"
      data-testid="workflow-index-facts"
      title={[trigger, steps].filter(Boolean).join(" · ")}
    >
      {trigger}
      {trigger && steps && " · "}
      {steps}
    </p>
  );
}

/** How many recent runs the health strip shows per workflow. */
const STRIP_RUNS = 5;

/** The two "no run to report" readings, named once because a card and a list
 * row both say them (issue #1136) and two copies of a sentence this careful
 * drift apart. What each one means is argued in {@link HealthLine}. */
const NO_RUNS_LABEL = "No recent runs";
const NO_RUNS_TITLE =
  "No runs in the recent company-wide page. Open the workflow to read its own run history.";
const LOADING_RUNS_LABEL = "Loading runs…";

export function WorkflowIndex({
  workflows,
  runsByWorkflow,
  onSelect,
  mode,
  loading,
  runsLoaded,
}: {
  workflows: WorkflowSummary[];
  /** The recent runs of each workflow, newest first, from the company-wide page. */
  runsByWorkflow: Map<string, WorkflowRunOutcome[]>;
  /**
   * Open one workflow. Issue #1110: there is no `selectedId` counterpart, and
   * that is the point — selecting IS leaving this surface, so nothing here is
   * ever "the selected one". The cards used to carry `aria-pressed`, which
   * announced every one of them to a screen reader as a toggle that was off;
   * they are links to a page, and they read as buttons that do something now.
   */
  onSelect: (id: string) => void;
  /**
   * Cards or list. Owned and rendered by the caller (issue #1110): the index is
   * the whole body of `#/workflows` now, so its heading and the tab's heading
   * were the same bar drawn twice — "Workflows 7" over "All workflows 7", with
   * the toggle stranded under the duplicate. The control moved up into the one
   * toolbar; this component reads the answer and renders it.
   */
  mode: IndexMode;
  loading: boolean;
  /**
   * Whether the run page has come back yet.
   *
   * Load-bearing for honesty: until it has, every workflow legitimately has no
   * runs to show, and rendering "No recent runs" would state as fact something
   * we have not asked about yet. The health line stays blank instead.
   */
  runsLoaded: boolean;
}) {
  const sortedWorkflows = [...workflows].sort((a, b) => {
    const aLastRun = runsByWorkflow.get(a.id)?.[0]?.atMillis ?? Number.NEGATIVE_INFINITY;
    const bLastRun = runsByWorkflow.get(b.id)?.[0]?.atMillis ?? Number.NEGATIVE_INFINITY;
    return aLastRun === bLastRun ? 0 : bLastRun > aLastRun ? 1 : -1;
  });

  return (
    <div className="h-full overflow-auto p-4" data-testid="workflow-index">
      {loading ? (
        <div className="grid gap-3 sm:grid-cols-2 xl:grid-cols-3">
          {[0, 1, 2].map((i) => (
            <Skeleton key={i} className="h-28 rounded-xl" />
          ))}
        </div>
      ) : workflows.length === 0 ? (
        <p className="text-sm text-muted-foreground">
          This company has no saved workflows yet.
        </p>
      ) : mode === "cards" ? (
        <div className="grid gap-3 sm:grid-cols-2 xl:grid-cols-3">
          {sortedWorkflows.map((w) => (
            <WorkflowCard
              key={w.id}
              workflow={w}
              runs={runsByWorkflow.get(w.id) ?? []}
              runsLoaded={runsLoaded}
              onSelect={() => onSelect(w.id)}
            />
          ))}
        </div>
      ) : (
        // `@container`: the row's breakpoint is the LIST's width, not the
        // window's — the tab sits beside a sidebar, so a viewport breakpoint
        // would drop the description while there was still room for it, and
        // keep it past the point where there wasn't (issue #1136).
        <div className="@container divide-y rounded-xl border">
          {sortedWorkflows.map((w) => (
            <WorkflowRow
              key={w.id}
              workflow={w}
              runs={runsByWorkflow.get(w.id) ?? []}
              runsLoaded={runsLoaded}
              onSelect={() => onSelect(w.id)}
            />
          ))}
        </div>
      )}
    </div>
  );
}

/** One workflow as a card: what it is, whether the console can change it, and
 * how its recent runs went. */
function WorkflowCard({
  workflow,
  runs,
  runsLoaded,
  onSelect,
}: {
  workflow: WorkflowSummary;
  runs: WorkflowRunOutcome[];
  runsLoaded: boolean;
  onSelect: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onSelect}
      data-testid="workflow-card"
      className="flex flex-col gap-2 rounded-xl border bg-card/60 p-3 text-left transition hover:bg-accent/40"
    >
      <div className="flex items-start justify-between gap-2">
        <span className="min-w-0 truncate text-sm font-semibold">{workflow.name}</span>
        <span className="flex shrink-0 items-center gap-1">
          {/* Paused first: it is the one that changes whether this workflow
              does anything, and "in source" only qualifies who may edit it. */}
          <PausedBadge enabled={workflow.enabled} />
          {/* Issue #259's rule, repeated exactly: only an explicit `false` is a
              refusal — a host predating it sends no field, and `undefined` must
              not render as "you can't edit this". */}
          {workflow.editable === false && (
            <Badge
              variant="outline"
              className="h-4 shrink-0 px-1.5 text-3xs font-normal"
              title="Defined by a file in the company source tree, so it can't be changed or removed from the console."
            >
              in source
            </Badge>
          )}
        </span>
      </div>

      {workflow.description ? (
        <p className="line-clamp-2 text-xs text-muted-foreground">{workflow.description}</p>
      ) : (
        <p className="text-xs italic text-muted-foreground">No description.</p>
      )}

      <WorkflowFacts workflow={workflow} />

      <div className="mt-auto space-y-1.5 pt-1">
        <HealthLine runs={runs} runsLoaded={runsLoaded} />
        <RunStrip runs={runs} />
      </div>
    </button>
  );
}

/** One workflow as a list row — the same facts, laid out for scanning down a
 * column rather than across a grid.
 *
 * WHY EVERY COLUMN BUT ONE IS A FIXED LENGTH (issue #1136). A row is its own
 * element, so a grid drawn here is a grid of ONE row: `auto` and `fr` tracks are
 * measured against that row's own content and nothing else. Sized that way the
 * columns land wherever each row's own text happens to end, which is exactly the
 * ragged edge the flex version had — the description's left edge zigzagging by a
 * couple of hundred pixels and the status dots never forming a line. Fixed
 * lengths are what a shared vertical edge is made of; the single `1fr` is the
 * description, and it is the same width on every row because everything around
 * it is.
 *
 * NARROW VIEWPORTS: the description is the column that yields. Below `@3xl`
 * (48rem of *list* width — a container query, because the sidebar means the
 * viewport is not the width this list gets) the three fixed tracks plus a
 * readable description no longer fit, and the description would be truncated to
 * a word or two. It is dropped entirely there and the name takes the space: a
 * name identifies the row, a five-character description fragment identifies
 * nothing. `hidden` rather than a conditional render on purpose — a
 * `display: none` cell is not a grid item, so the remaining three cells fall
 * into the three-track template with no second code path. */
function WorkflowRow({
  workflow,
  runs,
  runsLoaded,
  onSelect,
}: {
  workflow: WorkflowSummary;
  runs: WorkflowRunOutcome[];
  runsLoaded: boolean;
  onSelect: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onSelect}
      data-testid="workflow-list-row"
      className="grid w-full grid-cols-[minmax(0,1fr)_13rem_4.5rem] items-center gap-x-3 px-3 py-2 text-left transition hover:bg-accent/40 @3xl:grid-cols-[17.5rem_minmax(0,1fr)_13rem_4.5rem]"
    >
      <span className="min-w-0">
        <span className="flex min-w-0 items-center gap-2">
          {/* `title` because the column is fixed: a truncated name is unreadable
              without one, and this is the only place the row says which workflow
              it is. */}
          <span className="truncate text-sm font-medium" title={workflow.name}>
            {workflow.name}
          </span>
          {/* Issue #1136: inside the name cell, not floating between columns —
              the badges qualify the name, so they travel with it. */}
          <PausedBadge enabled={workflow.enabled} />
          {workflow.editable === false && (
            <Badge
              variant="outline"
              className="h-4 shrink-0 px-1.5 text-3xs font-normal"
              title="Defined by a file in the company source tree, so it can't be changed or removed from the console."
            >
              in source
            </Badge>
          )}
        </span>
        <WorkflowFacts workflow={workflow} />
      </span>
      <span
        className="hidden min-w-0 truncate text-xs text-muted-foreground @3xl:block"
        title={workflow.description ?? undefined}
      >
        {workflow.description ?? ""}
      </span>
      <RowHealth runs={runs} runsLoaded={runsLoaded} />
    </button>
  );
}

/** The last two cells of a list row: how the most recent run went, and when.
 *
 * The same reading {@link HealthLine} gives a card, split across two grid cells
 * (issue #1136) — a fragment, so the row's grid places each one itself. The
 * status text starts at the left edge of its own fixed column, which is what
 * puts the dots in a vertical line; the time sits right-aligned in a column
 * narrow enough that "21h ago" and "7d ago" end on the same pixel.
 *
 * Both are single-line and truncate: a row that wraps is a row that is taller
 * than its neighbours, and the alignment this is all for is the first thing an
 * uneven row height destroys. */
function RowHealth({ runs, runsLoaded }: { runs: WorkflowRunOutcome[]; runsLoaded: boolean }) {
  const last = runs[0];

  // Nothing to show, and the two reasons for that are NOT the same thing — see
  // {@link HealthLine}, which owns the wording both surfaces use. The time cell
  // is simply absent; it is the last column, so nothing follows it to shift.
  if (!last) {
    return runsLoaded ? (
      <span className="truncate text-2xs text-muted-foreground" title={NO_RUNS_TITLE}>
        {NO_RUNS_LABEL}
      </span>
    ) : (
      <span className="truncate text-2xs text-muted-foreground">{LOADING_RUNS_LABEL}</span>
    );
  }

  const tone = runTone(last);
  const label = runSummaryLine(last, tone.label, last.error ? failedNodeOf(last) : null);

  return (
    <>
      <span className="flex min-w-0 items-center gap-1.5 text-2xs" title={label}>
        <span className={`size-1.5 shrink-0 rounded-full ${tone.dot}`} />
        <span className="truncate text-muted-foreground">{label}</span>
      </span>
      <span className="truncate text-right text-2xs text-muted-foreground">
        {relativeTime(last.atMillis)}
      </span>
    </>
  );
}

/** How the most recent run went, in one line.
 *
 * The three terminal readings issue #383 separated are all distinguishable
 * here — failed, stopped by an operator, and finished — because {@link runTone}
 * is the same function the history rows use. */
function HealthLine({ runs, runsLoaded }: { runs: WorkflowRunOutcome[]; runsLoaded: boolean }) {
  const last = runs[0];

  if (!last) {
    // Nothing yet, and the two reasons for that are NOT the same thing.
    if (!runsLoaded) {
      return <span className="text-2xs text-muted-foreground">{LOADING_RUNS_LABEL}</span>;
    }
    // "No recent runs", never "never run". The company-wide run page is cut by
    // a limit, so a workflow whose last run has scrolled off it is
    // indistinguishable from one that has never run — and claiming the stronger
    // of the two would be exactly the false negative issue #228 was about.
    // Selecting the workflow re-reads its history scoped server-side, which
    // does answer the stronger question.
    return (
      <span className="text-2xs text-muted-foreground" title={NO_RUNS_TITLE}>
        {NO_RUNS_LABEL}
      </span>
    );
  }

  const tone = runTone(last);
  const undelivered = undeliveredCount(last.deliveries);
  const pending = pendingCount(last.deliveries);
  const failedNode = last.error ? failedNodeOf(last) : null;

  return (
    <span className="flex flex-wrap items-center gap-1.5 text-2xs">
      {/* `runTone` owns the running reading too, so this reads the same way as
          the last-run chip and the history rows rather than being a second
          opinion that can drift from them. */}
      <span className={`size-1.5 rounded-full ${tone.dot}`} />
      <span className="text-muted-foreground">
        {last.scheduled ? "Scheduled" : "Manual"} run {tone.label}
        {failedNode ? ` at “${failedNode}”` : ""}
      </span>
      <span className="text-muted-foreground">· {relativeTime(last.atMillis)}</span>
      {undelivered > 0 && (
        <Badge
          variant="outline"
          className="h-4 px-1.5 text-3xs font-normal border-status-failed/40 bg-status-failed-soft"
        >
          {undelivered} not delivered
        </Badge>
      )}
      {pending > 0 && (
        <Badge
          variant="outline"
          className="h-4 px-1.5 text-3xs font-normal border-status-blocked/40 bg-status-blocked-soft"
        >
          {pending} awaiting approval
        </Badge>
      )}
    </span>
  );
}

/** The last few runs as dots, newest first — the "is this flaky or is this
 * broken?" reading a single last-run status cannot give.
 *
 * Renders nothing when there are no runs: an empty row of placeholder dots
 * would imply runs we do not have. */
function RunStrip({ runs }: { runs: WorkflowRunOutcome[] }) {
  const recent = runs.slice(0, STRIP_RUNS);
  if (recent.length === 0) return null;
  return (
    <span className="flex items-center gap-1" data-testid="workflow-card-strip">
      {recent.map((run) => {
        const tone = runTone(run);
        return (
          <span
            key={run.seq}
            className={`size-1.5 rounded-full ${tone.dot}`}
            title={`${run.scheduled ? "Scheduled" : "Manual"} run — ${tone.label} · ${relativeTime(
              run.atMillis,
            )}`}
          />
        );
      })}
      <span className="ml-0.5 text-3xs text-muted-foreground">
        last {recent.length}
      </span>
    </span>
  );
}
