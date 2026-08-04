import { lazy, Suspense, useCallback, useEffect, useMemo, useState } from "react";
import { ChevronRight, Flag, MessagesSquare, ShieldCheck, Sparkles } from "lucide-react";

import { listTasks, type Task } from "@/api/tasks";
import type { OpenCompanyClient } from "@/api/client";
import { listSkills, type Skill } from "@/api/skills";
import { Button } from "@/components/ui/button";
import { StatusPill } from "@/components/status-pill";
import type { View } from "@/components/app-shell";
import type { CompanyFeed } from "@/hooks/use-company";
import { fromDto, starterTeam, type TeamMember } from "@/lib/team";
import { cn } from "@/lib/utils";
import { CompanyMap } from "./overview/CompanyMap";
import { FocusPanel } from "./overview/FocusPanel";
import { TONE_TEXT } from "./overview/palette";
import {
  activityByDay,
  boardShape,
  greeting,
  isOpen,
  mapMembers,
  stateOfWorld,
  tickerItems,
} from "./overview/pulse";
import { Meter, Segments, Spark, Tile } from "./overview/Tile";
import { Ticker } from "./overview/Ticker";
import type { MapFocus } from "./overview/types";

/** How far back the activity chart and its sparks look. */
const WINDOW_DAYS = 14;

// Recharts is ~400 kB and every one of its users is below the fold. Overview is
// the landing view, so pulling it into the entry chunk would put that weight in
// front of first paint for every operator; the Usage and Finances views split
// it out for the same reason. The tiles' own sparks are hand-drawn SVG and stay
// in the entry chunk, so the top of the page is never waiting on this.
const ActivityChart = lazy(() =>
  import("./overview/Charts").then((m) => ({ default: m.ActivityChart })),
);
const BoardShapeChart = lazy(() =>
  import("./overview/Charts").then((m) => ({ default: m.BoardShapeChart })),
);

interface Props {
  feed: CompanyFeed;
  client: OpenCompanyClient;
  company: string | null;
  onNavigate: (view: View) => void;
  onFlag: () => void;
}

/**
 * The command centre: the whole company on one screen.
 *
 * Three layers, in the order an operator actually reads them — what needs you
 * (the state line and the pulse row), what just happened (the live strip), and
 * how the company is shaped (the map you can dive into, plus the two charts).
 *
 * Everything is built from surfaces the host already serves. Where one isn't
 * there — a host without a roster route — this falls back exactly as the Team
 * page does rather than inventing a company.
 */
export function Overview({ feed, client, company, onNavigate, onFlag }: Props) {
  const { status, approvals, now } = feed;

  const [tasks, setTasks] = useState<Task[]>([]);
  const [team, setTeam] = useState<TeamMember[]>(starterTeam);
  const [skills, setSkills] = useState<Skill[]>([]);
  const [focus, setFocus] = useState<MapFocus>({ kind: "company" });

  // Reset the dive whenever the operator switches company — a member id from
  // the last company means nothing in this one.
  useEffect(() => setFocus({ kind: "company" }), [company]);

  useEffect(() => {
    let live = true;
    void (async () => {
      const [board, roster, skillSet] = await Promise.all([
        listTasks(client, company).catch(() => [] as Task[]),
        client.listTeam(company).catch(() => null),
        listSkills(client, company).catch(() => [] as Skill[]),
      ]);
      if (!live) return;
      setTasks(board);
      if (roster?.length) setTeam(roster.map(fromDto));
      setSkills(skillSet);
    })();
    return () => {
      live = false;
    };
  }, [client, company]);

  const open = useMemo(() => tasks.filter(isOpen), [tasks]);
  const inProgress = useMemo(() => tasks.filter((t) => t.column === "in_progress"), [tasks]);
  const activity = useMemo(() => activityByDay(tasks, WINDOW_DAYS, now), [tasks, now]);
  const columns = useMemo(() => boardShape(tasks), [tasks]);
  const members = useMemo(() => mapMembers(tasks, team), [tasks, team]);
  const ticker = useMemo(() => tickerItems(tasks, approvals), [tasks, approvals]);
  const busy = useMemo(() => members.filter((m) => m.open > 0).length, [members]);
  const enabledSkills = useMemo(() => skills.filter((s) => s.enabled).length, [skills]);

  const state = useMemo(
    () =>
      stateOfWorld({
        lifecycle: status.lifecycle,
        pendingApprovals: status.pending_approvals,
        openTasks: open.length,
        inProgress: inProgress.length,
        members: team.length,
        enabledSkills,
      }),
    [status, open.length, inProgress.length, team.length, enabledSkills],
  );

  const focused = useMemo(() => {
    if (focus.kind === "member") return members.find((m) => m.id === focus.id) ?? null;
    if (focus.kind === "task") {
      return members.find((m) => m.tasks.some((t) => t.id === focus.id)) ?? null;
    }
    return null;
  }, [focus, members]);

  const diveOut = useCallback(() => setFocus({ kind: "company" }), []);

  return (
    <div className="flex-1 overflow-y-auto">
      <div className="mx-auto w-full max-w-6xl space-y-5 px-4 py-6">
        {/* Who, and how things stand in one line */}
        <header className="flex flex-wrap items-start justify-between gap-3">
          <div className="space-y-1.5">
            <p className="font-mono text-[10px] uppercase tracking-[0.18em] text-muted-foreground">
              {greeting(now)} · command centre
            </p>
            <h2 className="text-2xl font-semibold tracking-tight">{status.name}</h2>
            <p className="flex flex-wrap items-center gap-x-2 gap-y-1 font-mono text-xs">
              {state.map((chip, i) => (
                <span key={chip.text} className="flex items-center gap-2">
                  {i > 0 && <span className="text-border">·</span>}
                  <span className={TONE_TEXT[chip.tone]}>{chip.text}</span>
                </span>
              ))}
            </p>
          </div>
          <StatusPill lifecycle={status.lifecycle} />
        </header>

        {/* Pulse row */}
        <section className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
          <Tile
            label="Needs you"
            value={String(status.pending_approvals)}
            unit={status.pending_approvals === 0 ? "all clear" : "parked decisions"}
            onDive={() => onNavigate("approvals")}
          >
            <Meter
              value={status.pending_approvals}
              max={Math.max(status.pending_approvals, 5)}
              tone={status.pending_approvals > 0 ? "warn" : "ok"}
            />
          </Tile>
          <Tile
            label="In flight"
            value={String(inProgress.length)}
            unit={`of ${open.length} open`}
            onDive={() => onNavigate("tasks")}
          >
            <Spark values={activity.map((d) => d.value)} />
          </Tile>
          <Tile
            label="Team busy"
            value={String(busy)}
            unit={`of ${team.length} on the roster`}
            onDive={() => onNavigate("team")}
          >
            <Segments lit={busy} total={team.length} />
          </Tile>
          <Tile
            label="Skills"
            value={String(enabledSkills)}
            unit={skills.length ? `of ${skills.length} equipped` : "none installed"}
            onDive={() => onNavigate("skills")}
          >
            <Meter value={enabledSkills} max={Math.max(skills.length, 1)} tone="ok" />
          </Tile>
        </section>

        <Ticker items={ticker} />

        {/* The map, and whatever it is centred on */}
        <section className="space-y-3">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <nav aria-label="Map depth" className="flex items-center gap-1 font-mono text-xs">
              <Crumb label="Company" active={focus.kind === "company"} onClick={diveOut} />
              {focused && (
                <>
                  <ChevronRight className="size-3 text-muted-foreground" />
                  <Crumb
                    label={focused.name}
                    active={focus.kind === "member"}
                    onClick={() => setFocus({ kind: "member", id: focused.id })}
                  />
                </>
              )}
              {focus.kind === "task" && (
                <>
                  <ChevronRight className="size-3 text-muted-foreground" />
                  <Crumb label="Card" active onClick={() => undefined} />
                </>
              )}
            </nav>
            {focus.kind !== "company" && (
              <Button variant="ghost" size="sm" onClick={diveOut}>
                Dive out <kbd className="ml-1 font-mono text-[10px] text-muted-foreground">esc</kbd>
              </Button>
            )}
          </div>

          <div className="grid gap-3 lg:grid-cols-[1.4fr_1fr]">
            <div className="h-[420px] overflow-hidden rounded-xl border bg-card">
              <CompanyMap
                companyName={status.name}
                lifecycle={status.lifecycle}
                members={members}
                focus={focus}
                onFocus={setFocus}
              />
            </div>
            <FocusPanel
              focus={focus}
              members={members}
              companyName={status.name}
              lifecycle={status.lifecycle}
              openTasks={open.length}
              approvals={approvals}
              now={now}
              onFocus={setFocus}
              onOpenTasks={() => onNavigate("tasks")}
              onOpenApprovals={() => onNavigate("approvals")}
              onOpenTeam={() => onNavigate("team")}
            />
          </div>
        </section>

        {/* How the work is trending, and where it is piled up */}
        <section className="grid gap-3 lg:grid-cols-2">
          <Suspense fallback={<ChartSkeleton />}>
            <ActivityChart series={activity} />
          </Suspense>
          <Suspense fallback={<ChartSkeleton />}>
            <BoardShapeChart columns={columns} />
          </Suspense>
        </section>

        {/* Ways in */}
        <section className="grid gap-3 sm:grid-cols-3">
          <Action
            icon={MessagesSquare}
            label="Talk to your company"
            onClick={() => onNavigate("conversation")}
          />
          <Action icon={ShieldCheck} label="Review approvals" onClick={() => onNavigate("approvals")} />
          <Action icon={Flag} label="Flag something" onClick={onFlag} />
        </section>
      </div>
    </div>
  );
}

/** Holds the chart's footprint while its chunk arrives, so nothing jumps. */
function ChartSkeleton() {
  return <div className="h-[292px] rounded-xl border bg-card" />;
}

function Crumb({ label, active, onClick }: { label: string; active: boolean; onClick: () => void }) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={cn(
        "max-w-[14rem] truncate rounded-md px-2 py-1 transition-colors",
        active ? "bg-accent text-foreground" : "text-muted-foreground hover:text-foreground",
      )}
    >
      {label}
    </button>
  );
}

function Action({
  icon: Icon,
  label,
  onClick,
}: {
  icon: typeof Sparkles;
  label: string;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className="flex items-center gap-3 rounded-xl border bg-card px-4 py-3 text-left text-sm font-medium transition-colors hover:bg-accent/40"
    >
      <span className="grid size-8 shrink-0 place-items-center rounded-lg bg-muted">
        <Icon className="size-4" />
      </span>
      {label}
    </button>
  );
}
