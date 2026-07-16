import { useCallback, useEffect, useState } from "react";

const readHash = (): string => window.location.hash.replace(/^#\/?/, "").split(/[/?]/)[0];

/**
 * A tiny hash router: keeps the active view in `location.hash` (e.g.
 * `#/conversation`) so views are linkable, survive refresh, and honor
 * back/forward — without pulling in a full router or disturbing the app's
 * boot phases. Falls back to `fallback` for unknown/empty hashes.
 */
export function useHashView<T extends string>(
  valid: readonly T[],
  fallback: T,
): [T, (view: T) => void] {
  const resolve = useCallback(
    (): T => {
      const h = readHash();
      return (valid as readonly string[]).includes(h) ? (h as T) : fallback;
    },
    [valid, fallback],
  );

  const [view, setView] = useState<T>(resolve);

  // Reflect the initial view into the URL if it arrived without a hash.
  useEffect(() => {
    if (!readHash()) window.location.replace(`#/${view}`);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Follow browser back/forward and manual hash edits.
  useEffect(() => {
    const onHash = () => setView(resolve());
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, [resolve]);

  const navigate = useCallback((next: T) => {
    if (readHash() !== next) window.location.hash = `/${next}`;
    setView(next);
  }, []);

  return [view, navigate];
}
