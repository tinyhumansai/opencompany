import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { AlertTriangle, ArrowRight, CircleAlert, Clock3, MessageSquare, ShieldCheck } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import { listRuns, RUN_STATUS_LABEL, type RunSummary } from "@/api/runs";
import type { LocalScope } from "@/connections/types";
import type { CompanyFeed } from "@/hooks/use-company";
import { commitOverviewVisit, openOverviewVisit } from "@/lib/overview-visit";
import { chatHref } from "@/lib/run-source";
import { PageHeader } from "@/components/page-header";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  feed: Pick<CompanyFeed, "approvals" | "queue">;
  scope: LocalScope;
  /**
   * Bumped by the shell on every `run_status_changed` event — the same counter
   * the task-detail screen re-reads on (issue #1015). The run panels below
   * re-read when a live attempt parks or fails while this page stays open;
   * without it they would keep saying nothing stopped until the view remounted
   * or the page reloaded.
   */
  attemptEventTick?: number;
}

type RunLoad = "loading" | "ready" | "error";

/**
 * How many failed attempts the since-visit panel reads, and therefore what
 * "read through the boundary" can cover. The host clamps `?limit=` to
 * [`MAX_RUN_LIMIT`](server) (200), so this is the widest exhaustive window a
 * single creation-ordered page can give the `finishedAtMillis` boundary below.
 * The panel is honest about the ceiling: when a read comes back exactly full,
 * the empty state says so instead of claiming no failure was recorded.
 */
const FAILED_READ_LIMIT = 200;

/**
 * The operator's landing page (issue #1321).
 *
 * A graph remains available at `#/company/graph`; this page concentrates on
 * what needs a person, work that stopped, and durable failed runs since the
 * last time *this browser* opened it. The boundary is browser-local because
 * the host has no persisted company-wide event read cursor yet.
 */
export function OperatorOverview({
  client,
  company,
  feed,
  scope,
  attemptEventTick,
}: Props) {
  /**
  /**
   * The boundary the panel below compares against, for the current `scope`.
   *
   * Two fixes meet here and both are load-bearing, so neither side of this
   * merge could be taken whole.
   *
   * **From #1745, kept exactly:** the read happens during *render*, pinned in a
   * ref keyed on `scope` — which its owner ([`ConnectionConsole`]) memoizes on
   * `[connectionId, company]`, so reference identity is the right key. A
   * `[scope]` read effect paired with a `[scope]` write effect looks idempotent
   * and is not: StrictMode replays both in declaration order, so the replay's
   * read observes what the first pass's write just recorded. Reading before any
   * effect runs is what makes that replay harmless. A scope switch also gets
   * the new scope's boundary in the same render rather than one frame late.
   *
   * **From #1700, and why the functions changed:** a ref lives as long as one
   * component instance, and the shell mounts this view conditionally — every
   * trip to Chat and back is a fresh instance with a fresh ref, re-reading a
   * `localStorage` value the previous instance's write effect had already
   * advanced. That is the remount half of #1700, and no per-instance pin can
   * see it. [`openOverviewVisit`](overview-visit) pins the boundary in MODULE
   * state instead, which lives exactly as long as one page load — the lifetime
   * "since you last opened" is a claim about.
   *
   * `openOverviewVisit` records nothing, and `commitOverviewVisit` below is the
   * only durable write. A render React starts and never commits — a descendant
   * throws, the operator reloads out of the error boundary — is not a visit,
   * and must not become the boundary the next page load hides failures behind.
   * `commitOverviewVisit` is idempotent per page load per scope for the same
   * reason the read is, so StrictMode replaying the effect writes once.
   */
  const visitRef = useRef<{ scope: LocalScope; previousVisit: number | null }>();
  if (!visitRef.current || visitRef.current.scope !== scope) {
    visitRef.current = { scope, previousVisit: openOverviewVisit(scope) };
  }
  const previousVisit = visitRef.current.previousVisit;

  const [stoppedRuns, setStoppedRuns] = useState<RunSummary[]>([]);
  const [failedRuns, setFailedRuns] = useState<RunSummary[]>([]);
  const [runLoad, setRunLoad] = useState<RunLoad>("loading");

  /**
   * Which chat ids name a *desk* rather than a DM, from a best-effort desks
   * read.
   *
   * A chat-originated run's `chatId` is a host thread id, and the two kinds
   * address differently: a desk's channel id *is* its thread id, so the link is
   * `#/chat/<deskId>`, while a DM's thread id is the roster member's id, so the
   * link is `#/chat/dm:<id>`. The row below needs this to tell them apart.
   * Absent — a host without `…/desks`, or a read that failed — every chat run
   * degrades to the DM form, which is what [`runSource`](run-source) itself
   * falls back to.
   */
  const [deskIds, setDeskIds] = useState<ReadonlySet<string>>();
  useEffect(() => {
    let live = true;
    Promise.resolve()
      .then(() => client.listDesks(company))
      .then((desks) => {
        if (!live) return;
        if (Array.isArray(desks)) setDeskIds(new Set(desks.map((desk) => desk.id)));
      })
      .catch(() => {
        /* best-effort: the set stays empty and chat runs link as DMs */
      });
    return () => {
      live = false;
    };
  }, [client, company]);

  /**
   * The generation of the newest in-flight run read.
   *
   * Every read — the initial load and each `run_status_changed` re-read —
   * takes a ticket before it starts and only applies its answer if it still
   * holds the newest ticket. Without this, a re-read that returns while the
   * initial snapshot is still outstanding would be overwritten by that older
   * answer when it lands, leaving the panels stale until the next event.
   */
  const runReadGen = useRef(0);

  /**
   * The two run reads this page makes, kept separate on purpose.
   *
   * "Work that stopped" wants the newest parked-or-failed attempts of either
   * kind; "Since you last opened" is a claim about *failures*. Mixing them in
   * one capped page would let a run of newer paused attempts push an older
   * failed attempt that finished after the previous visit out of the answer —
   * and the since-visit panel's empty state would then print "No failed
   * attempts were recorded" while one existed. Failures get a page of their
   * own, read at the host's widest cap ([`FAILED_READ_LIMIT`]) so the boundary
   * filter below is as exhaustive as a single creation-ordered read can be.
   */
  const fetchRuns = useCallback(async () => {
    const [stopped, failed] = await Promise.all([
      listRuns(client, company, { status: ["failed", "paused"], limit: 12 }),
      listRuns(client, company, { status: ["failed"], limit: FAILED_READ_LIMIT }),
    ]);
    return { stopped, failed };
  }, [client, company]);

  useEffect(() => {
    let live = true;
    setRunLoad("loading");
    const gen = ++runReadGen.current;
    fetchRuns()
      .then(({ stopped, failed }) => {
        if (!live || gen !== runReadGen.current) return;
        setStoppedRuns(stopped);
        setFailedRuns(failed);
        setRunLoad("ready");
      })
      .catch(() => {
        if (live && gen === runReadGen.current) setRunLoad("error");
      });
    return () => {
      live = false;
    };
  }, [fetchRuns]);

  // Issue #1015: re-read (silently — no loading flash) when the shell reports a
  // run status change while this page stays open. The initial-load effect above
  // owns the "loading"/"error" states; a re-read that fails keeps the last good
  // lists rather than dropping a settled page to the error state mid-view. The
  // generation ticket means a slower *initial* read can no longer land after
  // this fresher answer and overwrite it.
  const seenRunTick = useRef(attemptEventTick);
  useEffect(() => {
    if (attemptEventTick === undefined || attemptEventTick === seenRunTick.current) return;
    seenRunTick.current = attemptEventTick;
    const gen = ++runReadGen.current;
    fetchRuns()
      .then(({ stopped, failed }) => {
        if (gen !== runReadGen.current) return;
        setStoppedRuns(stopped);
        setFailedRuns(failed);
        // If the initial read was still in flight, this is the freshest answer:
        // settle to ready rather than leaving the panel on its loading text.
        setRunLoad("ready");
      })
      .catch(() => {
        /* the current lists stay; the next event or reload re-reads */
      });
  }, [attemptEventTick, fetchRuns]);

  // Record that this browser opened this scope's overview. The only durable
  // side effect in the read/write pair — the read lives at render time above —
  // so StrictMode replaying it twice on mount is harmless twice over: the
  // boundary was already captured before this effect's first pass ran, and
  // `commitOverviewVisit` writes once per page load per scope anyway.
  //
  // In an effect rather than beside the read, because a render React starts and
  // discards is not a visit (review of PR #1752). It was `writeOverviewVisit`
  // straight from render on both sides of this merge, in different places.
  useEffect(() => {
    commitOverviewVisit(scope);
  }, [scope]);

  const stopped = useMemo(
    () => stoppedRuns.filter((run) => run.status === "paused" || run.status === "failed"),
    [stoppedRuns],
  );
  const failuresSinceVisit = useMemo(
    () =>
      previousVisit === null
        ? []
        : failedRuns.filter(
            (run) =>
              run.status === "failed" &&
              run.finishedAtMillis !== undefined &&
              run.finishedAtMillis >= previousVisit,
          ),
    [previousVisit, failedRuns],
  );
  /**
   * Whether the failed-only read came back at its cap. A full page means the
   * host may hold older created attempts than this read covers, so "no failed
   * attempts since the visit" is a claim about the newest [`FAILED_READ_LIMIT`]
   * only — the empty state below says exactly that instead of pretending the
   * history is exhausted.
   */
  const failedReadCapped = failedRuns.length >= FAILED_READ_LIMIT;

  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="operator-overview" data-tour="operator-overview">
      <PageHeader
        gutter="px-5 sm:px-8"
        title="Overview"
        width="5xl"
        actions={
          <a href="#/chat" className="inline-flex items-center justify-center gap-2 rounded-md bg-primary px-3 py-2 text-sm font-medium text-primary-foreground hover:bg-primary/90">
            <MessageSquare className="size-4" aria-hidden /> Start a conversation
          </a>
        }
      />
      <div className="mx-auto flex w-full min-h-0 max-w-5xl flex-1 flex-col gap-6 overflow-auto p-5 sm:p-8">

      <section aria-labelledby="overview-attention" className="rounded-xl border bg-card p-5 shadow-sm">
        <div className="flex items-start justify-between gap-4">
          <div>
            <h2 id="overview-attention" className="font-semibold">Needs your attention</h2>
            <p className="mt-1 text-sm text-muted-foreground">Approvals are decisions the company cannot make for itself.</p>
          </div>
          <ShieldCheck className="size-5 text-status-blocked-text" aria-hidden />
        </div>
        <ApprovalSummary feed={feed} />
      </section>

      <div className="grid gap-6 lg:grid-cols-2">
        <section aria-labelledby="overview-stopped" className="rounded-xl border bg-card p-5 shadow-sm">
          <div className="flex items-start justify-between gap-4">
            <div>
              <h2 id="overview-stopped" className="font-semibold">Work that stopped</h2>
              <p className="mt-1 text-sm text-muted-foreground">Paused and failed attempts that may need a closer look.</p>
            </div>
            <AlertTriangle className="size-5 text-status-failed-text" aria-hidden />
          </div>
          <RunRows state={runLoad} runs={stopped} deskIds={deskIds} empty="No work is paused or failed right now." />
        </section>

        <section aria-labelledby="overview-since" className="rounded-xl border bg-card p-5 shadow-sm">
          <div className="flex items-start justify-between gap-4">
            <div>
              <h2 id="overview-since" className="font-semibold">Since you last opened this browser</h2>
              <p className="mt-1 text-sm text-muted-foreground">
                {previousVisit === null
                  ? "There is no earlier visit in this browser to compare yet."
                  : "Failed attempts recorded after the previous visit."}
              </p>
            </div>
            <Clock3 className="size-5 text-muted-foreground" aria-hidden />
          </div>
          {previousVisit === null ? (
            <p className="mt-5 text-sm text-muted-foreground">Future visits will compare against this one. Company-wide activity history is not stored by the host yet.</p>
          ) : (
            <RunRows
              state={runLoad}
              runs={failuresSinceVisit}
              deskIds={deskIds}
              empty={
                failedReadCapped
                  ? `No failed attempts since your previous visit appear in the newest ${FAILED_READ_LIMIT} recorded — the host caps the read here.`
                  : "No failed attempts were recorded since the previous visit."
              }
            />
          )}
        </section>
      </div>

      <p className="text-xs text-muted-foreground">
        Looking for the company&apos;s structure? <a className="underline-offset-2 hover:underline" href="#/company/graph">Open the knowledge graph</a>.
      </p>
      </div>
    </div>
  );
}

function ApprovalSummary({ feed }: { feed: Pick<CompanyFeed, "approvals" | "queue"> }) {
  if (feed.queue === "loading") return <p className="mt-5 text-sm text-muted-foreground" aria-busy="true">Loading approvals…</p>;
  if (feed.queue === "error" && feed.approvals.length === 0) {
    return <p role="alert" className="mt-5 text-sm text-destructive">Couldn&apos;t read what needs your approval. Open Approvals to try again.</p>;
  }
  if (feed.approvals.length === 0) return <p className="mt-5 text-sm text-muted-foreground">Nothing is waiting for your approval.</p>;
  const count = feed.approvals.length;
  return (
    <div className="mt-5 flex items-center justify-between gap-3">
      <p className="text-sm font-medium">{count === 1 ? "1 decision is waiting" : `${count} decisions are waiting`}</p>
      <a href="#/approvals" className="inline-flex shrink-0 items-center gap-1 text-sm font-medium underline-offset-2 hover:underline">Review approvals <ArrowRight className="size-4" aria-hidden /></a>
    </div>
  );
}

function RunRows({
  state,
  runs,
  empty,
  deskIds,
}: {
  state: RunLoad;
  runs: RunSummary[];
  empty: string;
  deskIds?: ReadonlySet<string>;
}) {
  if (state === "loading") return <p className="mt-5 text-sm text-muted-foreground" aria-busy="true">Loading recent work…</p>;
  if (state === "error") return <p role="alert" className="mt-5 text-sm text-destructive">Couldn&apos;t read recent work from the company host.</p>;
  if (runs.length === 0) return <p className="mt-5 text-sm text-muted-foreground">{empty}</p>;
  return (
    <ul className="mt-5 divide-y">
      {runs.slice(0, 3).map((run) => (
        <li key={run.id} className="flex items-center justify-between gap-3 py-3 first:pt-0 last:pb-0">
          <div className="min-w-0">
            <p className="truncate text-sm font-medium">
              {run.taskId ? `Task ${run.taskId}` : run.chatId ? "Conversation work" : "Unattributed attempt"}
            </p>
            <p className="text-xs text-muted-foreground">{RUN_STATUS_LABEL[run.status]}{run.error ? ` — ${run.error}` : ""}</p>
          </div>
          {run.taskId ? (
            <a href={`#/tasks/${encodeURIComponent(run.taskId)}?run=${encodeURIComponent(run.id)}`} className="shrink-0 text-sm font-medium underline-offset-2 hover:underline">Open <ArrowRight className="inline size-3.5" aria-hidden /></a>
          ) : run.chatId ? (
            // A paused or failed operator-chat turn is investigated from the
            // thread it was raised in — the icon alone hid it (issue #1643).
            // The desk/DM form is the run-source rule: a known desk addresses
            // by id, anything else is a roster member's DM.
            <a href={chatHref(run.chatId, !deskIds?.has(run.chatId))} className="shrink-0 text-sm font-medium underline-offset-2 hover:underline">Open <ArrowRight className="inline size-3.5" aria-hidden /></a>
          ) : (
            <CircleAlert className="size-4 shrink-0 text-muted-foreground" aria-label="No task or conversation is attached to this attempt" />
          )}
        </li>
      ))}
    </ul>
  );
}
