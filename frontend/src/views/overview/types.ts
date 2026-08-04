// View models for the Overview command centre. Everything here is derived from
// what the host already serves — status, approvals, the task board, the roster,
// the skill set — so the surface never invents a number it cannot source.

import type { Task } from "@/api/tasks";

/** How a fragment of the state-of-the-world line should read. */
export type Tone = "ok" | "warn" | "busy" | "dim";

/** One clause of the honest one-line summary above the pulse row. */
export interface StateChip {
  text: string;
  tone: Tone;
}

/** A day bucket in a sparkline or area chart. */
export interface DayPoint {
  /** ISO day, e.g. `2026-08-04`. */
  date: string;
  value: number;
}

/** One column of the task board, with its live count. */
export interface ColumnCount {
  id: string;
  label: string;
  count: number;
}

/** What one teammate is carrying right now. */
export interface Workload {
  /** The assignee label exactly as the board stores it. */
  assignee: string;
  name: string;
  open: number;
  done: number;
  total: number;
}

/** One line of the live ticker. */
export interface TickerItem {
  id: string;
  /** Short status word rendered in the tone colour, e.g. `MOVED`, `WAITING`. */
  mark: string;
  tone: Tone;
  subject: string;
  detail: string;
  atMillis: number;
}

/**
 * What the company map is currently centred on.
 *
 * The map is a two-level dive: the company sits at the centre, teammates orbit
 * it, and a teammate's own cards orbit them once you dive in. `kind` is the
 * depth; `id` names the node at that depth.
 */
export type MapFocus =
  | { kind: "company" }
  | { kind: "member"; id: string }
  | { kind: "task"; id: string };

/** A teammate rendered on the map, with the work they hold. */
export interface MapMember {
  id: string;
  name: string;
  role: string;
  description: string;
  tone: string;
  tasks: Task[];
  open: number;
}
