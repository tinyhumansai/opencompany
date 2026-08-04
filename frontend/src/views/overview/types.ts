// View models for the Overview command centre's chrome. The graph itself has
// its own model in `graph.ts`.

/** How a fragment of the state-of-the-world line should read. */
export type Tone = "ok" | "warn" | "busy" | "dim";

/** One clause of the honest one-line summary beside the company name. */
export interface StateChip {
  text: string;
  tone: Tone;
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
