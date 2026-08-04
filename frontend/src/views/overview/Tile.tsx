// The pulse row: four tiles that each answer one question and dive straight
// into the surface that answers it in full.

import { ArrowUpRight } from "lucide-react";

import { cn } from "@/lib/utils";
import { sparkPath } from "./pulse";
import { TONE_MARK } from "./palette";
import type { Tone } from "./types";

interface TileProps {
  label: string;
  value: string;
  /** The denominator or unit, kept small beside the value. */
  unit: string;
  /** Where clicking lands. Omitted for a tile with nowhere deeper to go. */
  onDive?: () => void;
  /** The footer visual — a spark, a meter, or segments. */
  children: React.ReactNode;
}

/** One pulse tile. Clickable when there is a surface to dive into. */
export function Tile({ label, value, unit, onDive, children }: TileProps) {
  const body = (
    <>
      <div className="flex items-center justify-between">
        <span className="font-mono text-[10px] uppercase tracking-[0.16em] text-muted-foreground">
          {label}
        </span>
        {onDive && (
          <ArrowUpRight className="size-3.5 text-muted-foreground opacity-0 transition-opacity group-hover:opacity-100" />
        )}
      </div>
      <div className="flex items-baseline gap-2">
        <span className="font-mono text-[26px] font-semibold tracking-tight tabular-nums">
          {value}
        </span>
        <span className="text-xs text-muted-foreground">{unit}</span>
      </div>
      <div className="pt-0.5">{children}</div>
    </>
  );

  const shell =
    "group flex flex-col gap-2 rounded-xl border bg-card px-4 py-4 text-left transition-colors";

  if (!onDive) return <div className={shell}>{body}</div>;
  return (
    <button type="button" onClick={onDive} className={cn(shell, "hover:bg-accent/40")}>
      {body}
    </button>
  );
}

/** A trend line over a day series. Silent when there is no trend to show. */
export function Spark({ values, tone = "busy" }: { values: number[]; tone?: Tone }) {
  const w = 96;
  const h = 22;
  const d = sparkPath(values, w, h - 2);
  if (!d) {
    return <span className="block h-[22px] text-[10px] text-muted-foreground">no history yet</span>;
  }
  return (
    <svg width={w} height={h} viewBox={`0 0 ${w} ${h}`} className={TONE_MARK[tone]} aria-hidden>
      <path d={d} fill="none" stroke="currentColor" strokeWidth={2} strokeLinejoin="round" />
    </svg>
  );
}

/** A filled bar showing `value` of `max`. Reads as a proportion, not a trend. */
export function Meter({ value, max, tone = "busy" }: { value: number; max: number; tone?: Tone }) {
  const w = 96;
  const pct = max <= 0 ? 0 : Math.max(0, Math.min(1, value / max));
  return (
    <svg width={w} height={22} viewBox={`0 0 ${w} 22`} className={TONE_MARK[tone]} aria-hidden>
      <rect x="0" y="8.5" width={w} height="5" rx="2.5" className="fill-border" />
      <rect x="0" y="8.5" width={(w * pct).toFixed(1)} height="5" rx="2.5" fill="currentColor" />
    </svg>
  );
}

/**
 * One notch per item, lit for the ones that count.
 *
 * An honest stand-in where there is state but no history — connector uptime,
 * a roster's busy/idle split — rather than a line implying a trend we never
 * recorded.
 */
export function Segments({ lit, total, tone = "busy" }: { lit: number; total: number; tone?: Tone }) {
  const w = 96;
  const h = 22;
  const gap = 2;
  const n = Math.max(1, Math.min(total, 16));
  const bw = Math.max(2, (w - gap * (n - 1)) / n);
  return (
    <svg width={w} height={h} viewBox={`0 0 ${w} ${h}`} className={TONE_MARK[tone]} aria-hidden>
      {Array.from({ length: n }, (_, i) => {
        const on = i < lit;
        return (
          <rect
            key={i}
            x={(i * (bw + gap)).toFixed(1)}
            y={on ? 3 : 12}
            width={bw.toFixed(1)}
            height={on ? h - 6 : 7}
            rx="1.5"
            fill="currentColor"
            className={on ? undefined : "fill-border"}
          />
        );
      })}
    </svg>
  );
}
