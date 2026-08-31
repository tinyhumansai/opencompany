/**
 * The concurrency lens: who was working, when, and how much of it overlapped.
 *
 * Pure, in the spirit of `views/workflows/graph.ts` — every decision here is a
 * function of its inputs, so the interesting cases (an open span, a clock skew,
 * two agents overlapping) are unit-testable without a browser.
 *
 * # Why this is not recharts
 *
 * recharts has no Gantt primitive. The usual substitute — a stacked `BarChart`
 * with a transparent offset bar — fights the axis, cannot draw two concurrent
 * bars in one lane, and cannot animate a "now" rule. All three are the point of
 * this view: a hivemind that is really a relay race looks identical to one that
 * is really parallel until you can see the overlap.
 */

/**
 * How a span should be tinted — the run-health vocabulary, not a new one.
 *
 * `idle` is the closed set's neutral word (docs/design-system/color.md): the
 * tone `run-health.ts` already gives `stopped` and `stranded` for "nothing is
 * happening and nothing went wrong". A by-design decline (issue #1809) reaches
 * for it too — it is a clean terminal outcome, but not a success, so it must
 * not share `done`'s green.
 */
export type SpanState = "done" | "failed" | "running" | "blocked" | "idle";

/** One bar: an agent doing one thing over an interval. */
export interface Span {
  id: string;
  /** The agent this belongs to — one lane per agent. */
  lane: string;
  startMs: number;
  /** `null` while it is still going; clamped to `now` when laid out. */
  endMs: number | null;
  state: SpanState;
  label: string;
}

/** A laid-out span, with its position as a fraction of the window. */
export interface PlacedSpan extends Span {
  /** Left edge, 0–1 of the window. */
  offset: number;
  /** Width, 0–1 of the window. A zero-length span still gets a hairline. */
  width: number;
  /** The resolved end, with an open span clamped to `now`. */
  resolvedEndMs: number;
}

/** One agent's row, packed so overlapping spans never collide. */
export interface Lane {
  agentId: string;
  /** Sub-rows. A lane with no overlap has exactly one. */
  rows: PlacedSpan[][];
}

/** The window every span is placed against. */
export interface Window {
  startMs: number;
  endMs: number;
}

/** The smallest fraction a span may occupy, so a fast call stays visible. */
const MIN_WIDTH = 0.004;

/**
 * The interval covering every span.
 *
 * An open span extends the window to `now`, which is what makes a running view
 * grow rather than pin itself to the last thing that finished.
 */
export function windowFor(spans: Span[], nowMs: number): Window {
  if (spans.length === 0) return { startMs: nowMs, endMs: nowMs };
  let startMs = Infinity;
  let endMs = -Infinity;
  for (const span of spans) {
    startMs = Math.min(startMs, span.startMs);
    endMs = Math.max(endMs, span.endMs ?? nowMs);
  }
  return { startMs, endMs: Math.max(endMs, startMs) };
}

/**
 * Places one span within `window`.
 *
 * Two clamps, both for real data rather than defensiveness:
 *
 * - an **open** span (`endMs === null`) resolves to `now`, the same rule
 *   `runElapsedMillis` and `runDuration` already use for a live run;
 * - an end *before* its start resolves to zero width. The host stamps
 *   `startedAtMillis` and the browser supplies `now`, so the two clocks are
 *   genuinely different — a skewed pair must render as a point, not as a bar
 *   growing leftwards across the lane.
 */
export function place(span: Span, window: Window, nowMs: number): PlacedSpan {
  const total = Math.max(1, window.endMs - window.startMs);
  const resolvedEndMs = Math.max(span.startMs, span.endMs ?? nowMs);
  const offset = (span.startMs - window.startMs) / total;
  const width = (resolvedEndMs - span.startMs) / total;
  return {
    ...span,
    resolvedEndMs,
    offset: Math.min(Math.max(offset, 0), 1),
    width: Math.min(Math.max(width, MIN_WIDTH), 1),
  };
}

/**
 * Groups spans into one lane per agent, packing overlaps onto sub-rows.
 *
 * Greedy first-fit over spans sorted by start: a span joins the first sub-row
 * whose last span ended before it began, else it opens a new one. That is what
 * makes concurrency *visible* — two overlapping turns by one agent become two
 * stacked bars rather than one bar drawn over the other.
 *
 * Lanes come back in first-start order, so the agent that opened the run reads
 * at the top and the reading order matches the run's order.
 */
export function lanesFrom(spans: Span[], nowMs: number): Lane[] {
  const window = windowFor(spans, nowMs);
  const byLane = new Map<string, Span[]>();
  for (const span of spans) {
    const bucket = byLane.get(span.lane);
    if (bucket) bucket.push(span);
    else byLane.set(span.lane, [span]);
  }

  const lanes: Lane[] = [];
  for (const [agentId, own] of byLane) {
    const sorted = [...own].sort((a, b) => a.startMs - b.startMs);
    const rows: PlacedSpan[][] = [];
    for (const span of sorted) {
      const placed = place(span, window, nowMs);
      const row = rows.find((candidate) => {
        const last = candidate[candidate.length - 1];
        return last === undefined || last.resolvedEndMs <= placed.startMs;
      });
      if (row) row.push(placed);
      else rows.push([placed]);
    }
    lanes.push({ agentId, rows });
  }

  lanes.sort((a, b) => {
    const first = (lane: Lane) =>
      Math.min(...lane.rows.map((row) => row[0]?.startMs ?? Infinity));
    return first(a) - first(b);
  });
  return lanes;
}

/** One point of the concurrency profile: how many spans were open at `atMs`. */
export interface Concurrency {
  atMs: number;
  n: number;
}

/**
 * How many spans were open at each moment something changed.
 *
 * A sweep over start/end events rather than a sampled grid, so a short burst of
 * parallelism cannot fall between two samples and vanish — which would make the
 * one question this chart answers ("did these agents actually run together?")
 * answerable wrongly.
 *
 * The returned series is a **step function**: each point is the count from that
 * instant until the next.
 */
export function concurrencyProfile(spans: Span[], nowMs: number): Concurrency[] {
  if (spans.length === 0) return [];
  const events: { atMs: number; delta: number }[] = [];
  for (const span of spans) {
    const end = Math.max(span.startMs, span.endMs ?? nowMs);
    events.push({ atMs: span.startMs, delta: 1 });
    events.push({ atMs: end, delta: -1 });
  }
  // Ends before starts at the same instant, so a span that ends exactly as
  // another begins reads as a hand-off rather than as a moment of overlap.
  events.sort((a, b) => a.atMs - b.atMs || a.delta - b.delta);

  const out: Concurrency[] = [];
  let n = 0;
  for (const event of events) {
    n += event.delta;
    const last = out[out.length - 1];
    if (last && last.atMs === event.atMs) last.n = n;
    else out.push({ atMs: event.atMs, n });
  }
  return out;
}

/** The highest number of spans open at once. */
export function peakConcurrency(spans: Span[], nowMs: number): number {
  return concurrencyProfile(spans, nowMs).reduce((peak, p) => Math.max(peak, p.n), 0);
}

/** An evenly-spaced tick, as a fraction of the window and a label. */
export interface Tick {
  atMs: number;
  offset: number;
  label: string;
}

/** `1.2s`, `45s`, `2m 10s` — the scale's own words, not a date. */
export function formatOffset(ms: number): string {
  if (ms < 1000) return `${Math.max(0, Math.round(ms))}ms`;
  const seconds = ms / 1000;
  if (seconds < 60) {
    // One decimal under ten seconds, where a tenth is the difference between a
    // fast tool call and a slow one — but never a bare `.0`, which reads as
    // spurious precision on a tick that is exactly four seconds.
    const shown = seconds < 10 ? seconds.toFixed(1).replace(/\.0$/, "") : Math.round(seconds);
    return `${shown}s`;
  }
  const minutes = Math.floor(seconds / 60);
  const rest = Math.round(seconds - minutes * 60);
  return rest === 0 ? `${minutes}m` : `${minutes}m ${rest}s`;
}

/**
 * `count` + 1 ticks across the window, labelled as elapsed time.
 *
 * Elapsed rather than wall-clock: the question a reader has here is "how long
 * did this take", and a column of timestamps answers a different one.
 */
export function ticks(window: Window, count = 4): Tick[] {
  const total = Math.max(1, window.endMs - window.startMs);
  const out: Tick[] = [];
  for (let i = 0; i <= count; i += 1) {
    const offset = i / count;
    out.push({
      atMs: window.startMs + total * offset,
      offset,
      label: formatOffset(total * offset),
    });
  }
  return out;
}
