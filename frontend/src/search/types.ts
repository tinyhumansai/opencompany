// What a search answers with, independent of where it came from.
//
// One shape for every source, so the modal renders a list rather than four
// special cases, and so a new source (tasks, workflows) is a function that
// returns these rather than another branch in the component.

/** Which kind of thing a result is. Also the order its group is shown in. */
export type ResultKind = "channel" | "agent" | "message" | "file";

/**
 * The groups, in the order they appear.
 *
 * Sections rather than tabs, deliberately: a tab makes the operator choose
 * where the answer lives before they know, and the common case — "I half
 * remember a name" — is exactly the one where they cannot.
 */
export const RESULT_ORDER: readonly ResultKind[] = ["channel", "agent", "message", "file"];

/** What each group is called on screen. */
export const RESULT_LABEL: Record<ResultKind, string> = {
  channel: "Channels",
  agent: "Agents",
  message: "Messages",
  file: "Files",
};

/** A half-open `[start, end)` slice of a string that matched the term. */
export type MatchRange = readonly [number, number];

/** One thing the operator can act on. */
export interface SearchResult {
  kind: ResultKind;
  /** Unique within a result set; the list's React key and the highlight cursor. */
  id: string;
  /** The primary line — a channel name, an agent's name, a file's name. */
  title: string;
  /** Where the title matched, for highlighting without building HTML. */
  titleMatches: readonly MatchRange[];
  /** One line of context: a role, a description, a path, a channel and a date. */
  subtitle?: string;
  /** The matched text itself, for a message or a file's body. */
  excerpt?: string;
  /** Where the excerpt matched. */
  excerptMatches?: readonly MatchRange[];
  /**
   * Where activating this goes — a hash address this console already routes,
   * so a result is the same navigation as clicking the thing in the sidebar.
   */
  href: string;
  /**
   * What activating it will do, said plainly on the row.
   *
   * Load-bearing in a fused palette: `#engineering` navigates and `#engineering box`
   * searches, and the operator should not have to infer which from the shape
   * of what they typed.
   */
  action: string;
  /**
   * The token to write into the box when this result is *picked* as a scope,
   * where that differs from what the row reads.
   *
   * An agent's display name is not a token: "User Researcher" written back as
   * `@User Researcher ` parses as the scope `user` plus the term `researcher`,
   * which searches a different agent's DM for a word nobody typed. The id is
   * one word by construction, so it survives the round trip.
   */
  scopeName?: string;
  /** Higher sorts first within its group. Never compared across groups. */
  score: number;
}

/** A group of results, as the modal draws it. */
export interface ResultGroup {
  kind: ResultKind;
  label: string;
  results: SearchResult[];
}
