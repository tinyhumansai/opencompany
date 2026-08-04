import { useCallback, useEffect, useState } from "react";

/** The hash split into path segments: `#/settings/people` → `["settings", "people"]`. */
function readSegments(): string[] {
  return window.location.hash
    .replace(/^#\/?/, "")
    .split("?")[0]
    .split("/")
    .filter(Boolean);
}

/**
 * A tiny hash router: keeps the active view in `location.hash` (e.g.
 * `#/conversation`, or `#/settings/people` for a view with sub-pages) so views
 * are linkable, survive refresh, and honor back/forward — without pulling in a
 * full router or disturbing the app's boot phases. Falls back to `fallback` for
 * unknown or empty hashes.
 *
 * The second segment comes back unvalidated: only the owning view knows which
 * of its sub-pages exist, so it does that check itself and falls back to its
 * own default.
 */
export function useHashView<T extends string>(
  valid: readonly T[],
  fallback: T,
): [T, string | null, (view: T, sub?: string) => void] {
  const resolve = useCallback((): [T, string | null] => {
    const [head, sub] = readSegments();
    // An unknown head takes its sub-page with it: the sub-page names a page of
    // a view that isn't on screen, so carrying it onto `fallback` would point
    // the fallback view at a sub-page it doesn't have.
    if (!(valid as readonly string[]).includes(head)) return [fallback, null];
    return [head as T, sub ?? null];
  }, [valid, fallback]);

  const [route, setRoute] = useState<[T, string | null]>(resolve);

  /**
   * Rewrite the URL so it names the view actually on screen. An empty hash and
   * an unknown one (`#/finances` after a surface is retired, a typo, a stale
   * bookmark) both resolve to `fallback`, and without this the address bar
   * keeps claiming a view that isn't rendered.
   *
   * Replace semantics, never push: pushing leaves the unknown hash in the
   * history stack, so Back returns to it, this rewrite bounces forward again,
   * and the operator is stuck in a ping-pong they cannot Back out of.
   */
  const canonicalize = useCallback((next: [T, string | null]) => {
    const path = next[1] ? `${next[0]}/${next[1]}` : next[0];
    if (readSegments().join("/") === path) return;
    window.history.replaceState(null, "", `#/${path}`);
  }, []);

  // Reflect the resolved view into the URL when the page arrived with no hash
  // or an unrecognized one.
  useEffect(() => {
    canonicalize(route);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Follow browser back/forward and manual hash edits.
  useEffect(() => {
    const onHash = () => {
      const next = resolve();
      setRoute(next);
      canonicalize(next);
    };
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, [resolve, canonicalize]);

  const navigate = useCallback((next: T, nextSub?: string) => {
    const path = nextSub ? `${next}/${nextSub}` : next;
    if (readSegments().join("/") !== path) window.location.hash = `/${path}`;
    setRoute([next, nextSub ?? null]);
  }, []);

  return [route[0], route[1], navigate];
}
