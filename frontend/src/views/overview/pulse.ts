// Pure derivations behind the Overview command centre.
//
// Every function here is a plain transform of data the host already returned —
// no fetching, no clock reads, no randomness. `nowMillis` and `todayMillis` are
// injected so a render is reproducible and the values can be asserted directly.

import type { Task } from "@/api/tasks";
import type { ApprovalSummary } from "@/api/types";
import { TASK_COLUMNS } from "@/lib/tasks-sample";
import type { TeamMember } from "@/lib/team";

import type { ColumnCount, DayPoint, MapMember, StateChip, TickerItem, Workload } from "./types";

const DAY_MS = 86_400_000;

/** Columns whose cards are finished work rather than work in flight. */
const CLOSED_COLUMNS = new Set(["done"]);

/** Whether a card still counts as work the company owes someone. */
export function isOpen(task: Task): boolean {
  return !CLOSED_COLUMNS.has(task.column);
}

/** Time-of-day greeting. Split out so the header is testable without a clock. */
export function greeting(nowMillis: number): string {
  const hour = new Date(nowMillis).getHours();
  if (hour < 5) return "Still up";
  if (hour < 12) return "Good morning";
  if (hour < 18) return "Good afternoon";
  return "Good evening";
}

/**
 * The honest one-liner under the title: what is actually true right now,
 * strongest signal first. Nothing is padded — a quiet company gets a short
 * line, not a fabricated one.
 */
export function stateOfWorld(input: {
  lifecycle: string;
  pendingApprovals: number;
  openTasks: number;
  inProgress: number;
  members: number;
  enabledSkills: number;
}): StateChip[] {
  const chips: StateChip[] = [];

  if (input.pendingApprovals > 0) {
    chips.push({
      tone: "warn",
      text: `${input.pendingApprovals} waiting on you`,
    });
  }

  if (input.lifecycle === "running") {
    chips.push({
      tone: input.inProgress > 0 ? "busy" : "ok",
      text: input.inProgress > 0 ? `${input.inProgress} in flight` : "idle, ready for work",
    });
  } else {
    chips.push({ tone: "dim", text: `company ${input.lifecycle}` });
  }

  if (input.openTasks > 0) {
    chips.push({ tone: "dim", text: `${input.openTasks} open on the board` });
  }

  chips.push({
    tone: "dim",
    text: `${input.members} on the team · ${input.enabledSkills} skills equipped`,
  });

  return chips;
}

/** Card counts per board column, in board order. */
export function boardShape(tasks: Task[]): ColumnCount[] {
  return TASK_COLUMNS.map((col) => ({
    id: col.id,
    label: col.label,
    count: tasks.filter((t) => t.column === col.id).length,
  }));
}

/**
 * Cards touched per day over the trailing `days` window.
 *
 * The board records only `updatedAt`, so this is honestly "activity" — a card
 * moved, edited, or created that day — not a completion count. Days with no
 * activity are present as zeroes so the axis stays even.
 */
export function activityByDay(tasks: Task[], days: number, todayMillis: number): DayPoint[] {
  const buckets = new Map<string, number>();
  for (let i = days - 1; i >= 0; i--) {
    buckets.set(isoDay(todayMillis - i * DAY_MS), 0);
  }
  for (const task of tasks) {
    const key = isoDay(task.updatedAt);
    const seen = buckets.get(key);
    if (seen !== undefined) buckets.set(key, seen + 1);
  }
  return [...buckets].map(([date, value]) => ({ date, value }));
}

/** Per-teammate load, heaviest first, for the workload bars. */
export function workloads(tasks: Task[], members: TeamMember[]): Workload[] {
  const rows = members.map((m) => {
    const own = tasks.filter((t) => ownedBy(t, m));
    return {
      assignee: m.name,
      name: m.name,
      open: own.filter(isOpen).length,
      done: own.length - own.filter(isOpen).length,
      total: own.length,
    };
  });
  return rows.sort((a, b) => b.open - a.open || b.total - a.total);
}

/**
 * Whether a card belongs to a roster member.
 *
 * The board stores `assignee` as a free label, so a card may name the agent id,
 * the display name, or the role. All three are accepted rather than dropping
 * work on the floor because the operator typed the role.
 */
export function ownedBy(task: Task, member: TeamMember): boolean {
  const label = task.assignee?.trim().toLowerCase();
  if (!label) return false;
  return (
    label === member.id.toLowerCase() ||
    label === member.name.toLowerCase() ||
    label === member.role.toLowerCase()
  );
}

/** The map's orbit: every teammate with the cards they hold. */
export function mapMembers(tasks: Task[], members: TeamMember[]): MapMember[] {
  return members.map((m) => {
    const own = tasks.filter((t) => ownedBy(t, m));
    return {
      id: m.id,
      name: m.name,
      role: m.role,
      description: m.description,
      tone: m.tone,
      tasks: own,
      open: own.filter(isOpen).length,
    };
  });
}

/**
 * The live ticker: the most recent real movements, newest first.
 *
 * Approvals lead — they are the only items that block a human — then the
 * board's most recently touched cards.
 */
export function tickerItems(
  tasks: Task[],
  approvals: ApprovalSummary[],
  limit = 10,
): TickerItem[] {
  const fromApprovals: TickerItem[] = approvals.map((a) => ({
    id: `approval-${a.id}`,
    mark: "WAITING",
    tone: "warn" as const,
    subject: a.kind,
    detail: a.amount_usd != null ? `$${a.amount_usd.toLocaleString()}` : "needs your decision",
    atMillis: a.at_millis,
  }));

  const fromTasks: TickerItem[] = [...tasks]
    .sort((a, b) => b.updatedAt - a.updatedAt)
    .map((t) => ({
      id: `task-${t.id}`,
      mark: isOpen(t) ? "MOVED" : "DONE",
      tone: isOpen(t) ? ("busy" as const) : ("ok" as const),
      subject: t.assignee || "unassigned",
      detail: t.title,
      atMillis: t.updatedAt,
    }));

  return [...fromApprovals, ...fromTasks]
    .sort((a, b) => b.atMillis - a.atMillis)
    .slice(0, limit);
}

/**
 * An SVG path through `values`, scaled to fill `width` × `height`.
 *
 * Returns an empty string for fewer than two points — a one-point sparkline is
 * a dot, not a trend, and drawing it would imply a shape that isn't there.
 */
export function sparkPath(values: number[], width: number, height: number): string {
  if (values.length < 2) return "";
  const max = Math.max(...values, 1);
  const step = width / (values.length - 1);
  return values
    .map((v, i) => {
      const x = i * step;
      const y = height - (v / max) * height;
      return `${i === 0 ? "M" : "L"}${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .join(" ");
}

function isoDay(millis: number): string {
  return new Date(millis).toISOString().slice(0, 10);
}
