import { useCallback, useEffect, useState } from "react";

/** The two ways a ledger can render its entries. */
export type LedgerViewMode = "board" | "list";

/** Hash query key that keeps the selected ledger rendering in browser history. */
export const LEDGER_VIEW_PARAM = "view";

/**
 * Reads a valid ledger view from a hash, defaulting to `fallback` when the hash
 * does not name one. Board is the fallback callers that do not say default to —
 * the tasks ledger's dispatch board.
 */
export function readLedgerViewMode(
  hash = window.location.hash,
  fallback: LedgerViewMode = "board",
): LedgerViewMode {
  const [, query = ""] = hash.split("?");
  const view = new URLSearchParams(query).get(LEDGER_VIEW_PARAM);
  return view === "list" || view === "board" ? view : fallback;
}

/**
 * Keeps the Board/List choice beside the ledger route (`?view=list`).
 *
 * This is navigation state, rather than component state: changing it pushes a
 * history entry and a browser Back returns to the rendering the operator left.
 *
 * `fallback` is the mode the ledger on screen opens in when the address names
 * no view — the per-ledger default (`defaultLedgerMode` in `LedgersView`).
 * Board when the caller does not say, which is the tasks ledger's default.
 */
export function useLedgerViewMode(
  fallback: LedgerViewMode = "board",
): [LedgerViewMode, (mode: LedgerViewMode) => void] {
  const [mode, setMode] = useState(() =>
    readLedgerViewMode(undefined, fallback),
  );

  useEffect(() => {
    const onHashChange = () => setMode(readLedgerViewMode(undefined, fallback));
    window.addEventListener("hashchange", onHashChange);
    return () => window.removeEventListener("hashchange", onHashChange);
  }, [fallback]);

  // A changed fallback (a different ledger on screen) is re-applied only when
  // the address still names no view; an explicit `?view=list` keeps winning.
  useEffect(() => {
    setMode(readLedgerViewMode(undefined, fallback));
  }, [fallback]);

  const set = useCallback(
    (next: LedgerViewMode) => {
      const [path, query = ""] = window.location.hash.replace(/^#/, "").split("?");
      const params = new URLSearchParams(query);
      // The fallback is the mode the ledger opens in when the address names no
      // view, so switching to it deletes the parameter. Any other mode is an
      // explicit override and must be serialized — in particular `view=board`
      // on a declared ledger whose default is rows, which would otherwise
      // round-trip back to the fallback on the next `hashchange` and make the
      // board unreachable after one toggle (issue #1397).
      if (next === fallback) params.delete(LEDGER_VIEW_PARAM);
      else params.set(LEDGER_VIEW_PARAM, next);
      const suffix = params.toString().replace(/=(?=&|$)/g, "");
      const nextHash = `#${path}${suffix ? `?${suffix}` : ""}`;
      if (window.location.hash !== nextHash) window.location.hash = nextHash;
      setMode(next);
    },
    [fallback],
  );

  return [mode, set];
}
