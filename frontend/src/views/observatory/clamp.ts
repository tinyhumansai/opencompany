/**
 * Bounding what a turn card renders.
 *
 * A single `shell` step can return tens of kilobytes. The cost that matters is
 * not painting it — it is *parsing* it: a markdown renderer handed 84 KB does
 * that work on every render. So text is clamped here, before anything downstream
 * sees it, and the full body is only materialised when a reader asks.
 */

/** Characters shown before a body is folded behind "Show all". */
export const TEXT_LIMIT = 2_000;

export interface Clamped {
  shown: string;
  /** Characters withheld. Zero when nothing was. */
  hidden: number;
  truncated: boolean;
}

/**
 * Clamps `text` to `limit` characters.
 *
 * Counts **characters, not code units**, so a body of emoji or CJK is cut where
 * a reader would say it was cut, and never in the middle of a surrogate pair —
 * which would render as a replacement glyph.
 */
export function clampText(text: string, limit = TEXT_LIMIT): Clamped {
  const chars = [...text];
  if (chars.length <= limit) {
    return { shown: text, hidden: 0, truncated: false };
  }
  return {
    shown: chars.slice(0, limit).join(""),
    hidden: chars.length - limit,
    truncated: true,
  };
}

/** `840 B`, `12.3 KB`, `1.2 MB` — for saying what is behind a fold. */
export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

/**
 * Whether a string is worth rendering at all.
 *
 * A host may send `""` where it means "nothing", and an empty pane with a
 * heading reads as a bug rather than as an absence.
 */
export function present(text: string | null | undefined): text is string {
  return typeof text === "string" && text.trim().length > 0;
}
