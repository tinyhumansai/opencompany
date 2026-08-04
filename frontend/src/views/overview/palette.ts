// The Overview's colour vocabulary.
//
// Same hues the Usage and Finances charts already use, so the console reads as
// one system: slot 1 blue for the primary series, slot 3 aqua for the second,
// green for finished work, yellow reserved for "a human is blocked". Each tone
// carries a shape or a label alongside it — colour is never the only signal.
//
// Both modes are stepped for their own surface rather than flipped, which is
// why every entry names a light and a dark value.

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

/**
 * One hue per branch of the graph — work, capability, tools — taken in the
 * palette's fixed slot order (1 blue, 2 orange, 3 aqua). Three slots is
 * deliberate: those are the ones that clear the colour-blindness floors when
 * every pair can appear together, which on a sunburst they can.
 *
 * Depth is carried by size and weight rather than a fourth and fifth hue, so a
 * hub and its leaves read as one branch at a glance. Every node also carries an
 * icon and a label, and the legend names each kind — colour is never alone.
 */
export const BRANCH_MARK: Record<string, string> = {
  company: "text-foreground",
  // The core stays achromatic. It is not a fourth category competing with the
  // three branches — it is what they all sit around — and giving it a hue
  // would put a fourth colour on screen that no gate would pass beside the
  // other three.
  memory: "text-foreground",
  work: "text-[#2a78d6] dark:text-[#3987e5]",
  capability: "text-[#eb6834] dark:text-[#d95926]",
  tools: "text-[#1baf7a] dark:text-[#199e70]",
};
