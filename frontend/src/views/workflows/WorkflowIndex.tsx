// Issue #303: browse the company's workflows as CARDS or as a LIST, with a
// toggle, instead of only through the toolbar's dropdown.
//
// Why this exists at all: before #303 the only way to see the company's
// workflows was the `<Select>` in the toolbar — a combobox shows one workflow at
// a time and carries nothing but a name, so "which of these is failing?" had no
// answer short of selecting each in turn and reading its chip.
//
// WHAT A CARD IS ALLOWED TO SAY is the whole design constraint here. Two
// requests back this surface — `GET …/workflows` (id, name, description,
// editable) and `GET …/workflows/runs` (the company-wide run journal) — and a
// card renders strictly what those two carry. In particular it does NOT show a
// schedule or a node count: both live only in the full graph, which is a
// per-workflow `GET …/workflows/{wid}`, and fetching one per card would be an
// N+1 on every render of this panel. Showing "no schedule" without having
// looked would be a lie, so the card says nothing at all about schedules.

import { LayoutGrid, List as ListIcon } from "lucide-react";

import type { WorkflowRunOutcome, WorkflowSummary } from "@/api/workflows";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";

import { failedNodeOf } from "./graph";
import { pendingCount, relativeTime, runTone, undeliveredCount } from "./run-health";

/** Which rendering the index is showing. Persisted by the caller. */
export type IndexMode = "cards" | "list";

/** How many recent runs the health strip shows per workflow. */
const STRIP_RUNS = 5;

export function WorkflowIndex({
  workflows,
  runsByWorkflow,
  selectedId,
  onSelect,
  mode,
  onModeChange,
  loading,
  runsLoaded,
}: {
  workflows: WorkflowSummary[];
  /** The recent runs of each workflow, newest first, from the company-wide page. */
  runsByWorkflow: Map<string, WorkflowRunOutcome[]>;
  selectedId: string | null;
  onSelect: (id: string) => void;
  mode: IndexMode;
  onModeChange: (mode: IndexMode) => void;
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
  return (
    <div className="flex h-full flex-col" data-testid="workflow-index">
      <div className="flex items-center justify-between gap-2 border-b px-4 py-2">
        <div className="flex items-center gap-2">
          <span className="text-sm font-medium">All workflows</span>
          <Badge variant="secondary">{workflows.length}</Badge>
        </div>
        <div className="flex items-center gap-1 rounded-lg border p-0.5">
          <Button
            size="sm"
            variant={mode === "cards" ? "secondary" : "ghost"}
            className="h-7 px-2"
            onClick={() => onModeChange("cards")}
            aria-pressed={mode === "cards"}
            data-testid="workflow-index-cards"
          >
            <LayoutGrid className="mr-1.5 size-3.5" />
            Cards
          </Button>
          <Button
            size="sm"
            variant={mode === "list" ? "secondary" : "ghost"}
            className="h-7 px-2"
            onClick={() => onModeChange("list")}
            aria-pressed={mode === "list"}
            data-testid="workflow-index-list"
          >
            <ListIcon className="mr-1.5 size-3.5" />
            List
          </Button>
        </div>
      </div>

      <div className="min-h-0 flex-1 overflow-auto p-4">
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
            {workflows.map((w) => (
              <WorkflowCard
                key={w.id}
                workflow={w}
                runs={runsByWorkflow.get(w.id) ?? []}
                runsLoaded={runsLoaded}
                selected={w.id === selectedId}
                onSelect={() => onSelect(w.id)}
              />
            ))}
          </div>
        ) : (
          <div className="divide-y rounded-xl border">
            {workflows.map((w) => (
              <WorkflowRow
                key={w.id}
                workflow={w}
                runs={runsByWorkflow.get(w.id) ?? []}
                runsLoaded={runsLoaded}
                selected={w.id === selectedId}
                onSelect={() => onSelect(w.id)}
              />
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

/** One workflow as a card: what it is, whether the console can change it, and
 * how its recent runs went. */
function WorkflowCard({
  workflow,
  runs,
  runsLoaded,
  selected,
  onSelect,
}: {
  workflow: WorkflowSummary;
  runs: WorkflowRunOutcome[];
  runsLoaded: boolean;
  selected: boolean;
  onSelect: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onSelect}
      aria-pressed={selected}
      data-testid="workflow-card"
      className={`flex flex-col gap-2 rounded-xl border bg-card/60 p-3 text-left transition hover:bg-accent/40 ${
        selected ? "ring-2 ring-primary/40" : ""
      }`}
    >
      <div className="flex items-start justify-between gap-2">
        <span className="min-w-0 truncate text-sm font-semibold">{workflow.name}</span>
        {/* Issue #259's rule, repeated exactly: only an explicit `false` is a
            refusal — a host predating it sends no field, and `undefined` must
            not render as "you can't edit this". */}
        {workflow.editable === false && (
          <Badge
            variant="outline"
            className="h-4 shrink-0 px-1.5 text-[10px] font-normal"
            title="Defined by a file in the company source tree, so it can't be changed or removed from the console."
          >
            in source
          </Badge>
        )}
      </div>

      {workflow.description ? (
        <p className="line-clamp-2 text-xs text-muted-foreground">{workflow.description}</p>
      ) : (
        <p className="text-xs italic text-muted-foreground/70">No description.</p>
      )}

      <div className="mt-auto space-y-1.5 pt-1">
        <HealthLine runs={runs} runsLoaded={runsLoaded} />
        <RunStrip runs={runs} />
      </div>
    </button>
  );
}

/** One workflow as a list row — the same facts, laid out for scanning down a
 * column rather than across a grid. */
function WorkflowRow({
  workflow,
  runs,
  runsLoaded,
  selected,
  onSelect,
}: {
  workflow: WorkflowSummary;
  runs: WorkflowRunOutcome[];
  runsLoaded: boolean;
  selected: boolean;
  onSelect: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onSelect}
      aria-pressed={selected}
      data-testid="workflow-list-row"
      className={`flex w-full flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2 text-left transition hover:bg-accent/40 ${
        selected ? "bg-accent/30" : ""
      }`}
    >
      <span className="min-w-0 flex-1 truncate text-sm font-medium">{workflow.name}</span>
      {workflow.editable === false && (
        <Badge
          variant="outline"
          className="h-4 shrink-0 px-1.5 text-[10px] font-normal"
          title="Defined by a file in the company source tree, so it can't be changed or removed from the console."
        >
          in source
        </Badge>
      )}
      <span className="hidden min-w-0 flex-1 truncate text-xs text-muted-foreground sm:block">
        {workflow.description ?? ""}
      </span>
      <span className="shrink-0">
        <HealthLine runs={runs} runsLoaded={runsLoaded} />
      </span>
    </button>
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
      return <span className="text-[11px] text-muted-foreground/60">Loading runs…</span>;
    }
    // "No recent runs", never "never run". The company-wide run page is cut by
    // a limit, so a workflow whose last run has scrolled off it is
    // indistinguishable from one that has never run — and claiming the stronger
    // of the two would be exactly the false negative issue #228 was about.
    // Selecting the workflow re-reads its history scoped server-side, which
    // does answer the stronger question.
    return (
      <span
        className="text-[11px] text-muted-foreground"
        title="No runs in the recent company-wide page. Open the workflow to read its own run history."
      >
        No recent runs
      </span>
    );
  }

  const tone = runTone(last);
  const undelivered = undeliveredCount(last.deliveries);
  const pending = pendingCount(last.deliveries);
  const failedNode = last.error ? failedNodeOf(last) : null;

  return (
    <span className="flex flex-wrap items-center gap-1.5 text-[11px]">
      {last.running ? (
        <>
          <span className="size-1.5 animate-pulse rounded-full bg-sky-500" />
          <span className="text-muted-foreground">Running now</span>
        </>
      ) : (
        <>
          <span className={`size-1.5 rounded-full ${tone.dot}`} />
          <span className="text-muted-foreground">
            {last.scheduled ? "Scheduled" : "Manual"} run {tone.label}
            {failedNode ? ` at “${failedNode}”` : ""}
          </span>
        </>
      )}
      <span className="text-muted-foreground/70">· {relativeTime(last.atMillis)}</span>
      {undelivered > 0 && (
        <Badge
          variant="outline"
          className="h-4 px-1.5 text-[10px] font-normal border-red-500/40 bg-red-500/10"
        >
          {undelivered} not delivered
        </Badge>
      )}
      {pending > 0 && (
        <Badge
          variant="outline"
          className="h-4 px-1.5 text-[10px] font-normal border-sky-500/40 bg-sky-500/10"
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
            className={`size-1.5 rounded-full ${run.running ? "animate-pulse bg-sky-500" : tone.dot}`}
            title={`${run.scheduled ? "Scheduled" : "Manual"} run — ${
              run.running ? "running" : tone.label
            } · ${relativeTime(run.atMillis)}`}
          />
        );
      })}
      <span className="ml-0.5 text-[10px] text-muted-foreground/70">
        last {recent.length}
      </span>
    </span>
  );
}
