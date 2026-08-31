import { useCallback, useEffect, useState } from "react";

import { withHostParam } from "@/hooks/use-host-route";

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
 *
 * `rewrite` is how a retired address keeps working. It sees the raw segments
 * before anything else does and may name a different view to resolve to;
 * returning `null` — what it does for every ordinary address — leaves the
 * resolution exactly as it was. It runs *before* the validity check on purpose,
 * so an address whose view no longer exists can be sent somewhere real instead
 * of silently collapsing onto `fallback`.
 *
 * The redirect lands through `canonicalize` below, which replaces rather than
 * pushes — and that is not an implementation detail. A retired address that
 * *pushed* its replacement would sit one Back away, bounce the operator forward
 * again on arrival, and trap them in a loop they cannot leave.
 */
export function useHashView<T extends string>(
  valid: readonly T[],
  fallback: T,
  rewrite?: (head: string, sub: string | null) => [T, string | null] | null,
): [
  T,
  string | null,
  (
    view: T,
    sub?: string,
    /** Query state that belongs to the destination, beside its route. */
    query?: Readonly<Record<string, string | null>>,
  ) => void,
] {
  const resolve = useCallback((): [T, string | null] => {
    const [head, sub] = readSegments();
    const rewritten = rewrite?.(head ?? "", sub ?? null);
    if (rewritten) return rewritten;
    // An unknown head takes its sub-page with it: the sub-page names a page of
    // a view that isn't on screen, so carrying it onto `fallback` would point
    // the fallback view at a sub-page it doesn't have.
    if (!(valid as readonly string[]).includes(head)) return [fallback, null];
    return [head as T, sub ?? null];
  }, [valid, fallback, rewrite]);

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
   *
   * `withHostParam` carries the connection scope across the rewrite. The host
   * is part of the address (`use-host-route.ts`), and a `replaceState` fires no
   * `hashchange` — so a scope dropped here has nothing to put it back, and the
   * console would go on rendering one host under an address naming none.
   */
  const canonicalize = useCallback((next: [T, string | null]) => {
    const path = next[1] ? `${next[0]}/${next[1]}` : next[0];
    if (readSegments().join("/") === path) return;
    window.history.replaceState(null, "", withHostParam(path));
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

  // The host scope rides along; every other query key is dropped, which is what
  // `useHashFlag`'s flags want — `?new` belongs to the screen it was opened
  // over, not to the one being navigated to.
  const navigate = useCallback((next: T, nextSub?: string, query?: Readonly<Record<string, string | null>>) => {
    const path = nextSub ? `${next}/${nextSub}` : next;
    const nextHash = withHostParam(path, query);
    // A navigation without an explicit query changes only the route. Preserve
    // query state when the destination path is unchanged so durable link state
    // (for example, a focused workflow run) remains represented by the URL.
    const currentPath = window.location.hash.split("?")[0];
    const nextPath = nextHash.split("?")[0];
    const currentQuery = window.location.hash.includes("?")
      ? window.location.hash.slice(window.location.hash.indexOf("?") + 1)
      : "";
    const destinationHash =
      query === undefined && currentPath === nextPath && currentQuery
        ? `${nextPath}?${currentQuery}`
        : nextHash;
    if (window.location.hash !== destinationHash) {
      window.location.hash = destinationHash;
    }
    setRoute([next, nextSub ?? null]);
  }, []);

  return [route[0], route[1], navigate];
}
