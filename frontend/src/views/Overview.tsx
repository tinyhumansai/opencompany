import { lazy, Suspense, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { RefreshCw } from "lucide-react";

import { listPeople, type Person } from "@/api/auth";
import type { OpenCompanyClient } from "@/api/client";
import { listMemory, type MemoryEntry } from "@/api/memory";
import { listTasks, type Task } from "@/api/tasks";
import type { DeskDto } from "@/api/types";
import { getWorkflow, listWorkflows, type WorkflowGraph } from "@/api/workflows";
import { fromDto, type TeamMember } from "@/lib/team";
import { PageHeader } from "@/components/page-header";
import { adapt, buildMemoryGraph } from "./overview/kg/adapter";
import { buildKnowledgeGraph } from "./overview/kg/model";
import { ownedBy } from "./overview/pulse";

// The graph carries the force simulation and every detail card with it. Its own
// chunk means a cold load paints the frame before the physics arrives.
const KnowledgeGraph = lazy(() =>
  import("./overview/kg/KnowledgeGraph").then((m) => ({ default: m.KnowledgeGraph })),
);

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  /**
   * The company's real name — `feed.status.name` in the shell, the same value
   * `HostSwitcher` is handed (issue #1219). Optional: falls back to `company`
   * (the slug) and then to `buildKnowledgeGraph`'s own placeholder, so the core
   * node never claims a name this component was not given.
   */
  companyName?: string;
}

/** Everything the graph is drawn from — a snapshot, taken on demand. */
interface Sources {
  tasks: Task[];
  team: TeamMember[];
  /**
   * The company's desks — ring 1 (issue #486).
   *
   * Best-effort like every other source here. A host that cannot serve them
   * draws a graph with no pillars, which is the same picture as a company that
   * declares no desks. The org chart treats a failed `/desks` as a hard error
   * because desks *are* that page; here they are one ring of five, and failing
   * the whole graph over them would take the real rings down with them.
   */
  desks: DeskDto[];
  /** The desks read answered. A rejected `/desks` draws no pillars but must
   *  not drive the "No desks yet" empty state — that claim is only true of a
   *  company a successful read found empty, not one whose request failed. */
  desksRead: boolean;
  people: Person[];
  memories: MemoryEntry[];
  /** The company's saved workflow graphs, whole — nodes and edges, not names. */
  workflows: WorkflowGraph[];
  /** When this snapshot was taken (epoch millis); `null` before the first read. */
  fetchedAt: number | null;
}

const EMPTY: Sources = {
  tasks: [],
  // Empty, not a fabricated roster. This is the pre-load state, and seeding it
  // with twelve invented agents drew a full org graph for a company that has
  // nobody in it (`docs/spec/runtime/company-setup.md`).
  team: [],
  desks: [],
  desksRead: false,
  people: [],
  memories: [],
  workflows: [],
  fetchedAt: null,
};

/**
 * The command centre: the company's knowledge graph, and nothing else.
 *
 * The page is the graph — no header or strip. The shell does not render a top
 * bar for any view; its remaining controls live in the sidebar. The company
 * sits at the core, its desks are the pillars, the jobs hang off each pillar,
 * the teammate who does each job sits above it, and their tools are the outer
 * ring.
 *
 * Every ring is **declared** now: the pillars are the company's own desks
 * (issue #486), the outer ring is the grants the host resolved for each
 * teammate, and the flows are its saved workflow graphs with their own stages
 * (issue #601). Nothing here is keyword-matched, dealt out or templated.
 *
 * The one thing this console decides for itself is which pillar a workflow
 * hangs off, because the host scopes a flow to the company rather than to a
 * desk — see `DERIVED_NOTICE` in `kg/adapter.ts`.
 *
 * ## Why this is a snapshot with a button rather than a live view
 *
 * One paint of this page is five reads — the board, the roster, the desks, the
 * people, the memory — plus the workflow list and one more read per saved
 * workflow. On a timer that is a standing cost per open tab, for a picture that
 * changes when an operator does something rather than on the clock. So the page
 * fetches once and **says** it fetched once, with a control that re-reads on
 * demand: the staleness is answered out loud instead of by omission, and an
 * operator is never reading an old wheel with no way to notice.
 */
export function Overview({ client, company, companyName }: Props) {
  const [sources, setSources] = useState<Sources>(EMPTY);
  // Bumped by the refresh control; re-runs the read below and nothing else.
  const [reload, setReload] = useState(0);
  const [loading, setLoading] = useState(true);
  const refresh = useCallback(() => setReload((n) => n + 1), []);
  // Set when every source below failed at once — a host that cannot be
  // reached, not a company with nothing in it. `null` the rest of the time,
  // including mid-load: a stale notice only clears once a new load actually
  // answers (matches `OrgChartView`'s `error`, which behaves the same way).
  const [loadError, setLoadError] = useState<string | null>(null);
  // Mirrors `sources.fetchedAt` outside the closure below, so a load that
  // fails outright can tell "this company has never once answered" from "this
  // is a snapshot going stale" without reading `sources` from a stale closure
  // (the effect does not depend on it).
  const fetchedAtRef = useRef<number | null>(null);

  // The outage overlay covers the graph rather than unmounting it, so the
  // snapshot underneath is still painted — but a covered graph must not stay
  // keyboard-focusable or exposed to a screen reader (issue #1314). `inert` is
  // the one attribute that removes both, and it is set imperatively because
  // React 18's types predate the boolean `inert` prop.
  const graphShellRef = useRef<HTMLDivElement>(null);
  const outageRef = useRef<HTMLDivElement>(null);
  // The status slot's Refresh control is the natural landing spot when an
  // outage dismisses — the overlay that held focus unmounts with the "Try
  // again" button, and dropping a keyboard user to <body> would restart their
  // whole tab order (issue #1314). The button is disabled while the retried
  // read is in flight, so the dismissal lands on the graph shell instead and
  // only hands off to Refresh once the load answers — and even then only if
  // the user has not moved focus themselves in the meantime.
  const refreshButtonRef = useRef<HTMLButtonElement>(null);
  // Set while the outage overlay is showing, so the dismissal branch of the
  // effect below can tell "the outage just went away" from "the page is
  // loading for the first time" — only the former must move focus.
  const outageWasShowingRef = useRef(false);
  // Set when the outage dismisses with the Refresh control still disabled;
  // consumed by the hand-off effect below once the retried load answers.
  const restoreFocusToRefreshRef = useRef(false);
  useEffect(() => {
    const shell = graphShellRef.current;
    if (!shell) return;
    if (loadError) {
      outageWasShowingRef.current = true;
      shell.setAttribute("inert", "");
      // The graph — and the Refresh button whose failed click just produced
      // this outage — are now inert, so the browser would drop focus to
      // <body>. Land it on the explanation instead, where the keyboard user
      // can read it and reach the retry control.
      outageRef.current?.focus();
    } else {
      shell.removeAttribute("inert");
      if (outageWasShowingRef.current) {
        outageWasShowingRef.current = false;
        // The overlay that held focus unmounts with the very render that
        // clears the outage, so — unless the user has already moved focus
        // somewhere deliberate — focus has just fallen to <body>. A user who
        // tabbed or clicked into the sidebar while the retry was in flight
        // made their own choice; reclaim focus only in the former case, never
        // override theirs (issue #1314).
        if (document.activeElement !== document.body) return;
        // Focus is back at <body>, so it must land somewhere real. When the
        // retried read already answered (its last await can batch with this
        // render), Refresh is enabled and is the natural landing spot. When it
        // is still in flight, Refresh is disabled — land on the graph shell,
        // stable and (with `inert` lifted) already interactive, and upgrade to
        // Refresh once the load answers below.
        if (refreshButtonRef.current?.disabled) {
          shell.focus();
          restoreFocusToRefreshRef.current = true;
        } else {
          refreshButtonRef.current?.focus();
        }
      }
    }
  }, [loadError]);

  // Runs after every commit; the ref flags keep it a no-op until an outage is
  // actually dismissed with the retried read still in flight. When that read
  // answers, Refresh is enabled and gets focus — but only if the user has not
  // already moved focus somewhere of their own during the read. Overriding a
  // focus the user set deliberately is worse than leaving the graph shell as
  // the landing spot.
  useEffect(() => {
    if (!loading && !loadError && restoreFocusToRefreshRef.current) {
      restoreFocusToRefreshRef.current = false;
      const active = document.activeElement;
      if (active === graphShellRef.current || active === document.body) {
        refreshButtonRef.current?.focus();
      }
    }
  });

  useEffect(() => {
    let live = true;
    setLoading(true);
    void (async () => {
      const results = await Promise.allSettled([
        listTasks(client, company),
        client.listTeam(company),
        // Ring 1 (issue #486). Best-effort: see `Sources.desks`.
        client.listDesks(company),
        // Only an admin may list people; a member just gets no humans on the
        // graph, which is the right amount of information for them to have.
        listPeople(client, company),
        // The company's real durable memory (issue #36). A host without the
        // surface draws no constellation rather than a seeded one — the graph
        // must never claim the company remembers something it doesn't.
        listMemory(client, company),
        // Ring 2 (issue #601). Summaries only — see the graph reads below.
        listWorkflows(client, company),
      ]);
      if (!live) return;

      const [tasksResult, rosterResult, desksResult, peopleResult, memoriesResult, flowListResult] =
        results;
      const failedCount = results.filter((r) => r.status === "rejected").length;

      // Six independent best-effort reads all failing at once is not six
      // coincidences (issue #1219) — it is one fact: the host could not be
      // reached. Drawing that as an empty company, freshly stamped, told an
      // operator their company had no desks, no teammates, no work and no
      // tools. So a total failure redraws nothing: the previous snapshot and
      // its time stay exactly as they were, and the outage is said out loud
      // instead.
      if (failedCount === results.length) {
        setLoadError(
          fetchedAtRef.current === null
            ? "Could not reach the company."
            : "Could not reach the company. Showing the last snapshot.",
        );
        setLoading(false);
        return;
      }
      // At least one source answered — the existing partial-degrade behaviour
      // applies (draw the rings that came back), and any stale outage notice
      // is retired.
      setLoadError(null);

      const tasks = tasksResult.status === "fulfilled" ? tasksResult.value : ([] as Task[]);
      const roster = rosterResult.status === "fulfilled" ? rosterResult.value : null;
      const desksRead = desksResult.status === "fulfilled";
      const desks = desksRead ? desksResult.value : ([] as DeskDto[]);
      const people = peopleResult.status === "fulfilled" ? peopleResult.value : ([] as Person[]);
      const memories =
        memoriesResult.status === "fulfilled"
          ? memoriesResult.value.items
          : ([] as MemoryEntry[]);
      const flowList = flowListResult.status === "fulfilled" ? flowListResult.value : [];

      // `listWorkflows` answers with `{id,name,description,editable,enabled}`
      // and no nodes or edges, so the stages ring needs one graph read per
      // workflow. That is bounded by how many flows the company has saved — not
      // an N+1 over the roster — and they go out together.
      const workflows = await Promise.all(
        flowList.map((summary) =>
          getWorkflow(client, company, summary.id).catch(
            // A name-only entry has no saved graph to fetch (and a read can
            // simply fail). The company still declares the flow, so it is drawn
            // with no stages rather than dropped: a flow missing from the wheel
            // would be a quieter lie than one drawn empty.
            (): WorkflowGraph => ({
              id: summary.id,
              name: summary.name,
              description: summary.description,
              nodes: [],
              edges: [],
              // No saved graph to fetch, so no token (issue #1013).
              version: null,
            }),
          ),
        ),
      );
      if (!live) return;

      const fetchedAt = Date.now();
      fetchedAtRef.current = fetchedAt;
      setSources({
        tasks,
        // The host's roster, or nobody. Never a fabricated stand-in: an operator
        // reading this graph is reading who works here, and inventing twelve
        // agents made an unstaffed company look busy.
        team: roster?.length ? roster.map(fromDto) : [],
        desks,
        desksRead,
        people,
        memories,
        workflows,
        fetchedAt,
      });
      setLoading(false);
    })();
    return () => {
      live = false;
    };
  }, [client, company, reload]);

  const adapted = useMemo(
    () =>
      adapt({
        members: sources.team,
        desks: sources.desks,
        tasks: sources.tasks,
        people: sources.people,
        workflows: sources.workflows,
        ownedBy,
      }),
    [sources],
  );

  const graph = useMemo(
    () =>
      buildKnowledgeGraph(
        adapted.agents,
        adapted.departments,
        adapted.people,
        adapted.tasks,
        adapted.workflows,
        // The company's real name (issue #1219), falling back to the slug and
        // then to the model's own placeholder — never an empty string, which
        // would otherwise beat the default and draw a blank core node.
        companyName || company || undefined,
      ),
    [adapted, companyName, company],
  );

  const memoryGraph = useMemo(() => buildMemoryGraph(sources.memories), [sources.memories]);

  /**
   * Whether a settled read has actually told us what this company has.
   *
   * Both claims below rest on it. Only a fulfilled `/desks` may say a company
   * has no desks — a rejected one draws the graph without pillars but must not
   * assert anything about the company, which is the lie issue #1313 was about,
   * pointed at the control instead of at the data.
   */
  const desksAnswered = !loading && sources.fetchedAt !== null && sources.desksRead;

  /**
   * The snapshot line: what the page is, when it was taken, and the way to
   * take another. The graph is not live, and says so rather than leaving an
   * operator to assume a stale wheel is the current company.
   *
   * Handed to the graph as a slot rather than positioned here (issue #1307).
   * This used to be an `absolute right-3 top-3 z-10` corner of its own, which
   * put it squarely underneath the graph's `z-30` detail rail: opening any
   * node's card hid the staleness signal, the Refresh control *and* the
   * outage alert, and left them unclickable behind an opaque panel. Only the
   * graph's shell knows how much of the right edge the rail is using, so the
   * shell is what places this. The outage alert no longer lives here at all:
   * a total failure is the page's state, so it is a full-page overlay over
   * the graph (issue #1314), not a corner detail.
   */
  const statusSlot = (
    <div className="flex items-center gap-1.5 rounded-md border bg-background/90 px-2 py-1 text-2xs text-muted-foreground shadow-sm backdrop-blur">
      {/* Volatile: the timestamp is `now` relative to the host's clock, so
          two runs of the same code land a minute apart and the label's
          glyphs change — the graph's settle time depends on machine speed.
          Masked via data-visual-volatile (visual.spec.ts) rather than
          frozen, because a frozen client clock turns "just now" labels in
          the list below into a distance that grows every day. */}
      <span className="truncate" data-visual-volatile>
        {sources.fetchedAt === null
          ? loading
            ? "Loading…"
            : "No snapshot yet"
          : `Snapshot ${new Date(sources.fetchedAt).toLocaleTimeString([], {
              hour: "2-digit",
              minute: "2-digit",
            })}`}
      </span>
      <button
        ref={refreshButtonRef}
        type="button"
        onClick={refresh}
        disabled={loading}
        title="This page is a snapshot, not a live view. Re-read the company."
        aria-label="Refresh the graph"
        className="inline-flex min-h-6 items-center gap-1 rounded px-1 py-0.5 hover:bg-muted disabled:opacity-50 md:min-h-0"
      >
        <RefreshCw className={`size-3 ${loading ? "animate-spin" : ""}`} aria-hidden />
        Refresh
      </button>
    </div>
  );

  return (
    // Fills whatever the shell gives it. It used to claim `h-svh` — the whole
    // viewport — which was true while the page ran edge to edge. It sits on the
    // inset content card now (issue #1178), a box shorter than the viewport by
    // the frame, and a child insisting on `100svh` inside it is laid out taller
    // than the box that clips it: the bottom band of the graph, legend included,
    // would be cropped away. `flex-1 min-h-0` takes the height the card has.
    <div
      className="oc-kg relative flex-1 min-h-0 w-full min-w-0 overflow-hidden"
      // The guided tour's Overview stop anchors here. It used to spotlight the
      // quick-action row this page had before it became the graph; the graph is
      // the page now, so the graph is what gets spotlighted.
      data-tour="overview-graph"
    >
      {/* The graph is the page and draws no visible title of its own (issue
          #1221) — this names it for a screen reader the same way every other
          view's title does. */}
      <PageHeader hidden title="Company overview" />
      {/* A complete read failure is the page's state, not a detail in the
          snapshot chrome. The opaque canvas keeps an unreachable host from
          looking like a genuinely empty company, and puts the retry beside the
          explanation instead of asking an operator to find it in a corner. */}
      {loadError && (
        <div
          ref={outageRef}
          tabIndex={-1}
          data-testid="overview-outage"
          className="absolute inset-0 z-50 grid place-items-center bg-os-bg/95 px-5 outline-none"
        >
          <div role="alert" className="max-w-md text-center">
            <p className="text-lg font-semibold text-os-text">{loadError}</p>
            <button
              type="button"
              onClick={refresh}
              disabled={loading}
              aria-label="Retry loading the company overview"
              className="mt-4 inline-flex items-center gap-2 rounded-md border border-os-border-strong bg-os-surface px-3 py-2 text-sm font-medium text-os-text shadow-sm transition-colors hover:bg-os-bg disabled:opacity-50"
            >
              <RefreshCw className={`size-4 ${loading ? "animate-spin" : ""}`} aria-hidden />
              Try again
            </button>
          </div>
        </div>
      )}
      {/* The graph stays mounted under the overlay so its snapshot is never
          torn down and rebuilt — but while the outage shows it must be inert:
          not focusable, not exposed to a screen reader (issue #1314). The
          attribute lives on this wrapper because it must cover the graph and
          the status slot but not the overlay itself. */}
      <div ref={graphShellRef} data-graph-shell tabIndex={-1} className="h-full w-full">
        <Suspense
          fallback={
            // The graph's chunk is still in flight, so there is no shell to slot
            // the snapshot line into yet — and no detail rail for it to dodge
            // either. It is drawn here at the same inset the shell will use, so
            // a cold load still says "Loading…" and the line does not appear to
            // pop into existence once the physics arrives.
            <div className="grid h-full place-items-center text-sm text-muted-foreground">
              <div className="absolute right-5 top-5 z-40 flex flex-col items-end gap-1.5">
                {statusSlot}
              </div>
              Drawing the graph…
            </div>
          }
        >
          <KnowledgeGraph
            graph={graph}
            agents={adapted.agents}
            departments={adapted.departments}
            people={adapted.people}
            tasks={adapted.tasks}
            memory={memoryGraph}
            toolLabels={adapted.toolLabels}
            statusSlot={statusSlot}
            covered={!!loadError}
            // A company with no desks still has a graph, and it is drawn.
            //
            // These used to be one flag, and it suppressed the canvas outright
            // — a company that had teammates, tools, saved workflows and a
            // memory constellation but no `[[group_chat]]` got a blank field
            // and a card telling it to create a desk. The model has never
            // needed a desk to draw: a worker the company declares no desk for
            // hangs off the core, and so does an unplaced workflow
            // (`model.ts`'s `UNPLACED` leg). It was only the view that refused.
            //
            // So the two facts are now separate. `noDesks` is a fact about the
            // company: no pillars, say so in the corner and offer the one
            // control that changes it. `emptyState` is a fact about the graph:
            // there is nothing beyond the core node to look at, which is the
            // only case where covering an empty field with an explanation is
            // better than leaving it bare.
            //
            // `graph.nodes` alone undercounts this: durable memory is passed
            // to `KnowledgeGraph` separately via the `memory` prop rather than
            // folded into `graph.nodes`, so a deskless company with a memory
            // constellation but no roster, tasks, or workflows still has
            // something to look at even though `graph.nodes` holds only the
            // core. Counting `memoryGraph.nodes` too keeps that constellation
            // reachable instead of covering it with "No desks yet".
            noDesks={desksAnswered && sources.desks.length === 0}
            emptyState={desksAnswered && graph.nodes.length <= 1 && memoryGraph.nodes.length === 0}
          />
        </Suspense>
      </div>
    </div>
  );
}
