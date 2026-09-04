/**
 * The run observatory: what a company's agents actually did.
 *
 * Three lenses over one set of attempts — a timeline of who worked when, the
 * transcript of what each of them did, and cross-run analytics. The DAG lives
 * in `WorkflowsView`, which owns the graph; this view is agent-centric, and
 * links back there for the canvas.
 *
 * # Live and replayable
 *
 * The snapshot is authority. It is refetched on a visible poll — faster while
 * something is running, slower when nothing is — and on the run-event tick the
 * shell already derives from SSE. A frame is never merged into the snapshot: it
 * only triggers a re-read, which is the discipline `graph.ts` documents for
 * frame loss (two frames collapsing inside one React batch still means "re-read"
 * exactly once, whereas two payloads collapsing loses one).
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import type { OpenCompanyClient } from "@/api/client";
import {
  fetchRecentRuns,
  fetchRun,
  fetchRunsForWorkflowRun,
  type ObservatoryRun,
} from "@/api/observatory";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { PageHeader } from "@/components/page-header";
import { classifyLoadFailure } from "@/lib/section-load";
import { startVisiblePolling } from "@/lib/visible-poll";
import { cn } from "@/lib/utils";
import { formatDuration, relativeTime } from "@/views/workflows/run-health";
import { formatUsdCost } from "@/lib/cost";
import { AnalyticsLens } from "./AnalyticsLens";
import { AttemptCard } from "./AttemptCard";
import { WaterfallLens } from "./WaterfallLens";
import { observatoryHref, readObservatoryHash, writeObservatoryQuery } from "./hash";
import { byWorkflowRun, runState, spansFromRuns, totals } from "./model";
import { peakConcurrency } from "./waterfall";

/** Refetch cadence while something is still running. */
const LIVE_POLL_MS = 4_000;
/** Refetch cadence when everything has settled. */
const IDLE_POLL_MS = 30_000;

interface Props {
  client: OpenCompanyClient;
  /** The active company, or `null` while the shell is between them. */
  company: string | null;
  /** The run named by the address, or `null` on the index. */
  runId: string | null;
  /** Bumped by the shell whenever an SSE run/workflow frame lands. */
  eventTick: number;
}

type Load =
  | { phase: "loading" }
  | { phase: "ready"; runs: ObservatoryRun[] }
  | { phase: "unavailable" }
  | { phase: "error"; message: string };

export function ObservatoryView({ client, company, runId, eventTick }: Props) {
  const [load, setLoad] = useState<Load>({ phase: "loading" });
  const [tab, setTab] = useState<"runs" | "analytics">(
    () => readObservatoryHash().tab,
  );
  const [nowMs, setNowMs] = useState(() => Date.now());
  // The address is the source of truth for the selection, but reading it on
  // every render would fight the poll; mirror it and let the writer update both.
  const [agent, setAgent] = useState<string | null>(() => readObservatoryHash().agent);
  // The attempt a deep link names, and the step within it. Mirrored in state,
  // like `agent`, so a link's `?turn=…&step=…` is honoured when the attempts
  // load — and re-read on `runId` change so it cannot go stale following a link
  // into another run.
  const [turn, setTurn] = useState<string | null>(() => readObservatoryHash().turn);
  const [focusStep, setFocusStep] = useState<number | null>(() => readObservatoryHash().step);
  // Bumped per `reload`; a response whose generation is stale (a company or
  // runId change happened while it was in flight) is discarded so a previous
  // route's attempts — including raw deep-trace bodies — cannot paint the
  // current route.
  const reloadGeneration = useRef(0);
  // The unredacted half of the attempts a reader has actually opened, keyed by
  // run id. The list read deliberately selects no deep bodies — they can hold
  // credentials and file contents — so a card's deep panes are fetched here,
  // and only here, when the card opens, and joined onto the freshest list
  // skeleton in the render below. A route change clears the map, and bumps
  // `deepGeneration`, so one company's raw bodies cannot outlive its route.
  const [deepByRun, setDeepByRun] = useState<Record<string, ObservatoryRun>>({});
  const deepGeneration = useRef(0);

  // Navigating between runs (following an Observatory link to another run)
  // changes `runId` while this view stays mounted, and such links deliberately
  // omit the old `agent` query parameter. Re-read the address so the filter,
  // the turn/step selection and the tab do not keep silently applying to the
  // newly fetched attempts.
  useEffect(() => {
    const next = readObservatoryHash();
    setAgent(next.agent);
    setTurn(next.turn);
    setFocusStep(next.step);
    setTab(next.tab);
  }, [runId]);

  const reload = useCallback(async () => {
    const generation = ++reloadGeneration.current;
    // GraphQL addresses a company by an explicit id, unlike the REST scope's
    // single-company alias — so there is nothing to ask for until one is
    // selected. An empty id would query for a company that cannot exist and
    // render its empty state as though the real one had no runs.
    if (!company) {
      setLoad({ phase: "ready", runs: [] });
      return;
    }
    try {
      const runs = runId
        ? await fetchRunsForWorkflowRun(client, company, runId)
        : await fetchRecentRuns(client, company, 100);
      if (generation !== reloadGeneration.current) return;
      setLoad({ phase: "ready", runs });
    } catch (err) {
      if (generation !== reloadGeneration.current) return;
      // A host that predates the GraphQL surface answers 404, and one built
      // without it says so in its code. Both are "unavailable", not "broken",
      // and get the honest empty state rather than an error with a retry the
      // operator can only watch fail.
      setLoad(
        classifyLoadFailure(err) !== "error"
          ? { phase: "unavailable" }
          : {
              phase: "error",
              message: err instanceof Error ? err.message : "the read failed",
            },
      );
    }
  }, [client, company, runId]);

  // The unredacted half is never fetched for a list; it loads per attempt, when
  // a reader opens its card. Each open re-reads, so a live attempt's deep panes
  // stay fresh alongside the polled skeleton. A response is discarded if the
  // route changed while it was in flight — see `deepGeneration` — and a failed
  // read degrades to "no deep half" for that open, exactly as the server-side
  // read degrades: the scrubbed trace is still the answer.
  const loadDeep = useCallback(
    async (id: string) => {
      if (!company) return;
      const generation = deepGeneration.current;
      try {
        const deepRun = await fetchRun(client, company, id);
        if (!deepRun) return;
        if (generation !== deepGeneration.current) return;
        setDeepByRun((prev) => ({ ...prev, [id]: deepRun }));
      } catch {
        // The scrubbed trace renders regardless; nothing else to do.
      }
    },
    [client, company],
  );

  useEffect(() => {
    setLoad({ phase: "loading" });
    // A route change (company or runId) invalidates the deep cache and bumps
    // the generation that in-flight deep reads check before landing.
    setDeepByRun({});
    deepGeneration.current += 1;
    void reload();
  }, [reload]);

  // The SSE tick: a counter, never a payload. See the header.
  useEffect(() => {
    if (eventTick === 0) return;
    void reload();
  }, [eventTick, reload]);

  const runs = load.phase === "ready" ? load.runs : [];
  const anyLive = runs.some((run) => runState(run) === "running");

  useEffect(() => {
    // The clock an open span is measured against. Ticking only while something
    // is live keeps a settled run from re-rendering forever.
    if (!anyLive) return;
    const timer = window.setInterval(() => setNowMs(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [anyLive]);

  useEffect(() => {
    return startVisiblePolling(
      () => void reload(),
      anyLive ? LIVE_POLL_MS : IDLE_POLL_MS,
    );
  }, [reload, anyLive]);

  const spans = useMemo(() => spansFromRuns(runs), [runs]);
  const summary = useMemo(() => totals(runs, nowMs), [runs, nowMs]);
  const peak = useMemo(() => peakConcurrency(spans, nowMs), [spans, nowMs]);
  const agents = useMemo(
    () => [...new Set(runs.map((r) => r.agentId))].sort(),
    [runs],
  );
  const shown = agent ? runs.filter((run) => run.agentId === agent) : runs;

  const selectAgent = (next: string | null) => {
    setAgent(next);
    writeObservatoryQuery({ agent: next });
  };

  const selectTab = (next: "runs" | "analytics") => {
    setTab(next);
    writeObservatoryQuery({ tab: next });
  };

  if (load.phase === "loading") {
    return (
      <div className="flex flex-col">
        <PageHeader title={runId ? "Run" : "Observatory"} />
        <p className="text-muted-foreground p-4 text-sm">Reading run history…</p>
      </div>
    );
  }

  if (load.phase === "unavailable") {
    return (
      <div className="flex flex-col">
        <PageHeader title={runId ? "Run" : "Observatory"} />
        <p className="text-muted-foreground max-w-prose p-4 text-sm">
          This host does not expose the run-observability read yet. Attempts and
          their step traces are still recorded — the Attempts tab on a card shows
          them one card at a time.
        </p>
      </div>
    );
  }

  if (load.phase === "error") {
    return (
      <div className="flex flex-col">
        <PageHeader title={runId ? "Run" : "Observatory"} />
        <div className="flex flex-col items-start gap-2 p-4">
          <p className="text-[var(--status-failed-text)] text-sm">{load.message}</p>
          <Button variant="outline" size="sm" onClick={() => void reload()}>
            Try again
          </Button>
        </div>
      </div>
    );
  }

  return (
    <div className="flex flex-col">
      {/*
        Issue #1763. This was a hand-rolled `h2` in a bare `header`, so
        Observatory was one of the two console pages a screen reader could not
        announce at all — it had no `h1` in any of its four states. Its shape
        was already the one `PageHeader` generalises (name on the left, tabs
        right-aligned on the same row), so it maps onto the component's own
        slots rather than needing a variant.
      */}
      <PageHeader
        title={runId ? "Run" : "Observatory"}
        trailing={
          runId && (
            <>
              <code className="text-muted-foreground text-xs">{runId}</code>
              <a
                href={observatoryHref()}
                className="text-muted-foreground text-xs underline underline-offset-2"
              >
                all runs
              </a>
            </>
          )
        }
        actions={
          <nav className="flex gap-1">
            {(["runs", "analytics"] as const).map((name) => (
              <Button
                key={name}
                variant={tab === name ? "secondary" : "ghost"}
                size="sm"
                className="h-7 px-3 text-xs capitalize"
                onClick={() => selectTab(name)}
              >
                {name}
              </Button>
            ))}
          </nav>
        }
      />
      <div className="flex flex-col gap-4 p-4">

      {runs.length === 0 ? (
        <p className="text-muted-foreground text-sm">
          {runId
            ? "This run recorded no agent attempts."
            : "No agent attempts recorded yet. Run a workflow and they will appear here."}
        </p>
      ) : tab === "analytics" ? (
        <AnalyticsLens runs={runs} />
      ) : (
        <>
          <dl className="text-muted-foreground flex flex-wrap gap-x-6 gap-y-1 text-xs">
            <span>
              <dt className="inline">agents</dt>{" "}
              <dd className="text-foreground inline tabular-nums">{summary.agents}</dd>
            </span>
            <span>
              <dt className="inline">attempts</dt>{" "}
              <dd className="text-foreground inline tabular-nums">{summary.attempts}</dd>
            </span>
            <span>
              <dt className="inline">steps</dt>{" "}
              <dd className="text-foreground inline tabular-nums">{summary.steps}</dd>
            </span>
            <span>
              <dt className="inline">elapsed</dt>{" "}
              <dd className="text-foreground inline tabular-nums">
                {formatDuration(summary.elapsedMs)}
              </dd>
            </span>
            <span>
              <dt className="inline">peak concurrency</dt>{" "}
              <dd className="text-foreground inline tabular-nums">{peak}</dd>
            </span>
            <span>
              <dt className="inline">tokens</dt>{" "}
              <dd className="text-foreground inline tabular-nums">
                {summary.tokens.toLocaleString()}
              </dd>
            </span>
            <span>
              <dt className="inline">cost</dt>{" "}
              <dd className="text-foreground inline tabular-nums">
                {formatUsdCost({ amountUsd: summary.costUsd }, "line") ?? "$0.00"}
              </dd>
            </span>
          </dl>

          <section className="bg-card rounded border p-3">
            <WaterfallLens spans={spans} nowMs={nowMs} />
          </section>

          {agents.length > 1 && (
            <div className="flex flex-wrap items-center gap-1">
              <Button
                variant={agent === null ? "secondary" : "ghost"}
                size="sm"
                className="h-7 px-3 text-xs"
                onClick={() => selectAgent(null)}
              >
                all
              </Button>
              {agents.map((id) => (
                <Button
                  key={id}
                  variant={agent === id ? "secondary" : "ghost"}
                  size="sm"
                  className="h-7 px-3 text-xs"
                  onClick={() => selectAgent(id)}
                >
                  {id}
                </Button>
              ))}
            </div>
          )}

          {!runId && <RunIndex runs={runs} />}

          <section className="flex flex-col gap-2">
            {shown.map((run) => (
              <AttemptCard
                key={run.id}
                run={withDeep(run, deepByRun[run.id])}
                nowMs={nowMs}
                turn={turn}
                focusStep={focusStep}
                onOpen={loadDeep}
              />
            ))}
          </section>
        </>
      )}
      </div>
    </div>
  );
}

/**
 * Joins a fetched deep half onto the freshest list skeleton of the same run.
 *
 * The list is the authority for which steps exist — it is refetched on the
 * visible poll, so it can be ahead of the open-time deep read. The deep read
 * contributes only each step's unredacted companion, matched by `seq`; a step
 * the deep read has not seen keeps `null`, which the renderer already treats as
 * "no deep trace for this step".
 */
function withDeep(list: ObservatoryRun, deep?: ObservatoryRun): ObservatoryRun {
  if (!deep) return list;
  const deepBySeq = new Map(deep.steps.map((s) => [s.seq, s.deep]));
  return {
    ...list,
    steps: list.steps.map((s) => ({ ...s, deep: deepBySeq.get(s.seq) ?? null })),
  };
}

/** On the index, group attempts by the workflow run that spawned them. */
function RunIndex({ runs }: { runs: ObservatoryRun[] }) {
  const grouped = useMemo(() => [...byWorkflowRun(runs).entries()], [runs]);
  if (grouped.length === 0) return null;
  return (
    <section className="flex flex-col gap-1">
      <h3 className="text-muted-foreground text-xs uppercase tracking-wide">
        Workflow runs
      </h3>
      <ul className="flex flex-col gap-1">
        {grouped.map(([workflowRunId, own]) => {
          const worst = own.some((r) => runState(r) === "failed")
            ? "failed"
            : own.some((r) => runState(r) === "blocked")
              ? "blocked"
              : own.some((r) => runState(r) === "running")
                ? "running"
                // A declined attempt (issue #1809) keeps the group's dot out
                // of "done" green even when nothing failed — the workflow
                // refused part of its own work, which is not the same claim
                // as every attempt succeeding.
                : own.some((r) => runState(r) === "idle")
                  ? "idle"
                  : "done";
          const started = Math.min(
            ...own.map((r) => r.startedAtMillis ?? r.createdAtMillis),
          );
          return (
            <li key={workflowRunId}>
              <a
                href={observatoryHref(workflowRunId)}
                className="hover:bg-muted/40 flex items-baseline gap-2 rounded px-2 py-1.5 text-sm"
              >
                <span
                  className={cn(
                    "size-2 shrink-0 rounded-full",
                    worst === "failed" && "bg-[var(--status-failed)]",
                    worst === "blocked" && "bg-[var(--status-blocked)]",
                    worst === "running" && "bg-[var(--status-running)]",
                    worst === "idle" && "bg-[var(--status-idle)]",
                    worst === "done" && "bg-[var(--status-done)]",
                  )}
                />
                <code className="truncate text-xs">{workflowRunId}</code>
                <Badge variant="secondary" className="shrink-0 text-3xs">
                  {own.length} {own.length === 1 ? "agent" : "agents"}
                </Badge>
                <span className="text-muted-foreground flex-1 truncate text-xs">
                  {[...new Set(own.map((r) => r.nodeId).filter(Boolean))].join(" → ")}
                </span>
                <span className="text-muted-foreground shrink-0 text-xs">
                  {relativeTime(started)}
                </span>
              </a>
            </li>
          );
        })}
      </ul>
    </section>
  );
}
