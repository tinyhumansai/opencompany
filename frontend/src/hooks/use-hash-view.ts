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
    const view = (valid as readonly string[]).includes(head) ? (head as T) : fallback;
    return [view, sub ?? null];
  }, [valid, fallback]);

  const [route, setRoute] = useState<[T, string | null]>(resolve);

  // Reflect the initial view into the URL if it arrived without a hash.
  useEffect(() => {
    if (readSegments().length === 0) window.location.replace(`#/${route[0]}`);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Follow browser back/forward and manual hash edits.
  useEffect(() => {
    const onHash = () => setRoute(resolve());
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, [resolve]);

  const navigate = useCallback((next: T, nextSub?: string) => {
    const path = nextSub ? `${next}/${nextSub}` : next;
    if (readSegments().join("/") !== path) window.location.hash = `/${path}`;
    setRoute([next, nextSub ?? null]);
  }, []);

  return [route[0], route[1], navigate];
}
