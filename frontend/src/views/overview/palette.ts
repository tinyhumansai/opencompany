// The Overview's colour vocabulary.
//
// Same hues the Usage and Finances charts already use, so the console reads as
// one system: slot 1 blue for the primary series, slot 3 aqua for the second,
// green for finished work, yellow reserved for "a human is blocked". Each tone
// carries a shape or a label alongside it — colour is never the only signal.
//
// Both modes are stepped for their own surface rather than flipped, which is
// why every entry names a light and a dark value.

import type { ChartConfig } from "@/components/ui/chart";
import type { Tone } from "./types";

/** Text colour per tone, for the state line and ticker marks. */
export const TONE_TEXT: Record<Tone, string> = {
  ok: "text-[#008300] dark:text-[#008300]",
  warn: "text-[#a87400] dark:text-[#eda100]",
  busy: "text-[#2a78d6] dark:text-[#3987e5]",
  dim: "text-muted-foreground",
};

/** Fill/stroke colour per tone, for inline SVG meters drawn with currentColor. */
export const TONE_MARK: Record<Tone, string> = {
  ok: "text-[#008300] dark:text-[#008300]",
  warn: "text-[#c98500] dark:text-[#eda100]",
  busy: "text-[#2a78d6] dark:text-[#3987e5]",
  dim: "text-muted-foreground/60",
};

/** Board columns on the company map. Each node is direct-labelled as well. */
export const COLUMN_MARK: Record<string, string> = {
  backlog: "text-muted-foreground",
  in_progress: "text-[#2a78d6] dark:text-[#3987e5]",
  in_review: "text-[#1baf7a] dark:text-[#199e70]",
  done: "text-[#008300] dark:text-[#008300]",
};

/** Recharts series config for the Overview's two charts. Single series each. */
export const CHART_CONFIG = {
  value: { label: "Cards touched", theme: { light: "#2a78d6", dark: "#3987e5" } },
  count: { label: "Cards", theme: { light: "#2a78d6", dark: "#3987e5" } },
} satisfies ChartConfig;
