/**
 * Who was working, when, and how much of it overlapped.
 *
 * All the arithmetic lives in `waterfall.ts`; this file is placement and tone.
 * The concurrency strip beneath the lanes is the point of the whole view: a
 * hivemind that is really a relay race looks identical to one that is really
 * parallel until you can see the overlap.
 */

import { useMemo } from "react";

import { cn } from "@/lib/utils";
import {
  concurrencyProfile,
  formatOffset,
  lanesFrom,
  peakConcurrency,
  ticks,
  windowFor,
  type Span,
  type SpanState,
} from "./waterfall";

/** The tone tokens, keyed by the same words `run-health` uses. */
const TONE: Record<SpanState, string> = {
  done: "bg-[var(--status-done)]",
  failed: "bg-[var(--status-failed)]",
  blocked: "bg-[var(--status-blocked)]",
  running: "bg-[var(--status-running)]",
  // A declined attempt (issue #1809) — neutral, never green.
  idle: "bg-[var(--status-idle)]",
};

interface Props {
  spans: Span[];
  /** The clock an open span is measured against. */
  nowMs: number;
  /** The span currently selected, if any. */
  selectedId?: string | null;
  onSelect?: (span: Span) => void;
}

export function WaterfallLens({ spans, nowMs, selectedId, onSelect }: Props) {
  const { lanes, marks, profile, peak, window } = useMemo(() => {
    const window = windowFor(spans, nowMs);
    return {
      lanes: lanesFrom(spans, nowMs),
      marks: ticks(window, 4),
      profile: concurrencyProfile(spans, nowMs),
      peak: peakConcurrency(spans, nowMs),
      window,
    };
  }, [spans, nowMs]);

  if (spans.length === 0) {
    return (
      <p className="text-muted-foreground px-3 py-6 text-sm">
        No agent activity recorded for this run.
      </p>
    );
  }

  const totalMs = Math.max(1, window.endMs - window.startMs);

  return (
    <div className="flex flex-col gap-2">
      {/* The scale, labelled as elapsed time: the question here is "how long
          did this take", and a column of timestamps answers a different one. */}
      <div className="text-muted-foreground relative h-4 text-3xs">
        {marks.map((tick) => (
          <span
            key={tick.offset}
            className="absolute -translate-x-1/2 tabular-nums"
            style={{ left: `${tick.offset * 100}%` }}
          >
            {tick.label}
          </span>
        ))}
      </div>

      <div className="flex flex-col gap-1">
        {lanes.map((lane) => (
          <div key={lane.agentId} className="flex items-start gap-2">
            <span
              className="text-muted-foreground w-28 shrink-0 truncate pt-0.5 text-xs"
              title={lane.agentId}
            >
              {lane.agentId}
            </span>
            <div className="flex min-w-0 flex-1 flex-col gap-0.5">
              {lane.rows.map((row, index) => (
                <div key={index} className="bg-muted/40 relative h-4 rounded">
                  {row.map((span) => (
                    <button
                      key={span.id}
                      type="button"
                      onClick={() => onSelect?.(span)}
                      title={`${span.label} · ${formatOffset(
                        span.resolvedEndMs - span.startMs,
                      )}`}
                      className={cn(
                        "absolute inset-y-0 rounded transition-opacity",
                        TONE[span.state],
                        span.state === "running" && "animate-pulse",
                        selectedId === span.id
                          ? "ring-foreground/60 ring-2"
                          : "hover:opacity-80",
                      )}
                      style={{
                        left: `${span.offset * 100}%`,
                        width: `${span.width * 100}%`,
                      }}
                    >
                      <span className="sr-only">
                        {span.label} on {lane.agentId}
                      </span>
                    </button>
                  ))}
                </div>
              ))}
            </div>
          </div>
        ))}
      </div>

      {/* The one visual that answers whether these agents actually ran
          together. A step area, because concurrency is a step function — it
          changes at an instant and holds until the next one. */}
      <div className="flex items-center gap-2 pt-1">
        <span className="text-muted-foreground w-28 shrink-0 text-xs">
          concurrency
        </span>
        <div className="bg-muted/30 relative h-6 min-w-0 flex-1 overflow-hidden rounded">
          {profile.map((point, index) => {
            const next = profile[index + 1];
            const from = (point.atMs - window.startMs) / totalMs;
            const to = next ? (next.atMs - window.startMs) / totalMs : 1;
            if (point.n <= 0) return null;
            return (
              <div
                key={`${point.atMs}-${index}`}
                className="bg-primary/35 absolute bottom-0"
                style={{
                  left: `${Math.max(0, from) * 100}%`,
                  width: `${Math.max(0, to - from) * 100}%`,
                  height: `${(point.n / Math.max(1, peak)) * 100}%`,
                }}
              />
            );
          })}
        </div>
        <span className="text-muted-foreground w-16 shrink-0 text-right text-xs tabular-nums">
          peak {peak}
        </span>
      </div>
    </div>
  );
}
