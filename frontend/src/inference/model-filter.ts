// Narrowing a provider's model catalog to what the operator is typing.
//
// The matcher is **`search/rank.ts`**, unchanged and not reimplemented. It
// already ranks exact above prefix above substring, already returns offsets into
// the original string so a highlight needs no HTML built from typed input, and
// already folds Unicode properly — NFD, combining marks stripped, and a
// folded→original offset map, with a Hangul regression pinned. A fresh
// `.toLowerCase().includes()` here would re-earn every bug that module already
// fixed, and would sort `anthracite-org/magnum-v4-72b` above
// `anthropic/claude-fable-5` for the term `claude`.

import { score } from "@/search/rank";

/**
 * How many matches are rendered.
 *
 * OpenRouter publishes around four hundred ids. Rendering all of them is four
 * hundred DOM nodes for a list nobody scrolls to the end of — and the ranking
 * above means the answer is in the first few or it is not in the list at all,
 * in which case the operator types it in full and takes the row that offers it.
 *
 * The cap is **said out loud** where it bites, never silently applied: a list
 * that quietly stops at fifty is a list that lies about what the provider
 * serves.
 */
export const MODEL_LIMIT = 50;

/** What the filtered list holds. */
export interface ModelMatches {
  /** The ids to render, best first, capped at [`MODEL_LIMIT`]. */
  shown: string[];
  /** How many matched in total, so a caller can say what the cap hid. */
  total: number;
  /**
   * The typed term, when it matched nothing and is worth offering as an id
   * in its own right.
   *
   * This is what unifies the filter box and the free-text escape hatch:
   * `openai/gpt-6-astra-pro` typed in full is simultaneously a search that finds
   * nothing and a perfectly good answer. Offering it as a row means one control
   * does both jobs, rather than a filter that dead-ends into a toggle.
   */
  offerTyped: string | null;
}

/**
 * The models matching `term`, best first.
 *
 * An empty term is not a filter — it returns the list in catalog order (already
 * sorted host-side), capped. Sorting an unfiltered list by `score` would be
 * sorting by nothing, since every entry scores the same.
 *
 * A term that matches nothing is offered **as itself**, so long as it could
 * plausibly be an id: an operator typing a model the catalog has not heard of is
 * the Azure case and the stale-catalog case at once, and both end with them
 * wanting exactly what they typed.
 */
export function filterModels(models: readonly string[], term: string): ModelMatches {
  const trimmed = term.trim();
  if (!trimmed) {
    return {
      shown: models.slice(0, MODEL_LIMIT),
      total: models.length,
      offerTyped: null,
    };
  }

  const ranked = models
    .map((id) => ({ id, rank: score(id, trimmed) }))
    .filter((entry) => entry.rank > 0)
    // Descending by rank, and by id within a rank so the order is stable across
    // renders rather than depending on how the engine happened to sort.
    .sort((a, b) => b.rank - a.rank || a.id.localeCompare(b.id))
    .map((entry) => entry.id);

  return {
    shown: ranked.slice(0, MODEL_LIMIT),
    total: ranked.length,
    // Not offered when it is already in the list under some other ranking — the
    // operator would then have two rows meaning one thing.
    offerTyped: ranked.includes(trimmed) ? null : trimmed,
  };
}

/** What to say under a filtered list that the cap has cut short. */
export function cappedNote(matches: ModelMatches): string | null {
  if (matches.total <= matches.shown.length) return null;
  return `Showing the ${matches.shown.length} closest of ${matches.total}. Keep typing to narrow it.`;
}
