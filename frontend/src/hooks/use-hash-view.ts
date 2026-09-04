import { useCallback, useEffect, useRef, useState } from "react";

import { withHostParam } from "@/hooks/use-host-route";

/**
 * How many dirty forms are currently claiming the right to answer a hash
 * navigation themselves, rather than let it land (Codex review, PR #2054).
 *
 * Module-level and plain, not React state or context: every `useHashView`
 * instance below registers its OWN `window` `hashchange` listener at its own
 * mount time — the app shell's top-level router, `WorkspaceView`,
 * `WorkflowsView`, `LedgersView`, `CompanyView`, `OrgChartView`, and more —
 * so there is no single listener whose registration order a later-mounting
 * guard (a dialog opened well after the shell) could arrange to run before.
 * DOM listeners on one target fire in registration order regardless of the
 * `capture` flag once the target has no ancestors (`window` is both target
 * and the whole propagation path for `hashchange`), so a guard that only
 * reacts to the SAME event can never win a listener-order race against a
 * router that was already listening. This has to be checked as the very
 * first thing inside every router's own listener, synchronously, before any
 * of them reads `location.hash` or calls `setState` — a value read from
 * context or props is already too late, since it is only current once a
 * render has happened, and the whole point is to act before any of this
 * event's several listeners have caused one.
 *
 * The bug this exists to fix: `WorkflowCreateDialog`'s own unsaved-work guard
 * (issue #1006, defect B-081) restores a changed hash and raises a
 * confirmation from its OWN `hashchange` listener — but that listener
 * necessarily mounts, and therefore registers, after the app shell's router
 * already has. On Back or a manual hash edit, the shell's router listener ran
 * first, read the NEW hash before the dialog's restoration touched anything,
 * and queued a route change. React 18 batches that together with the
 * dialog's own `setState` (raising the confirmation) into one commit — and
 * since the route change unmounts `WorkflowCreateDialog`, the confirmation's
 * state died with it in the very same commit that would have shown it. The
 * operator's draft was gone with no confirmation ever rendered, on the exact
 * path B-081 was written to protect.
 */
let activeHashNavigationGuards = 0;

/**
 * Claims (or releases) the right to answer the next hash navigation, for as
 * long as `active` is true. See {@link activeHashNavigationGuards}'s doc for
 * why this has to be a plain synchronous counter rather than React state.
 *
 * A form calls this with its own "would this navigation lose work" flag —
 * `WorkflowCreateDialog` passes `dirty`, the same flag that already gates its
 * `beforeunload` and `hashchange` guards, so there is one fact about whether
 * an exit is dangerous rather than three that could drift. Registering as
 * soon as the form BECOMES dirty, rather than only inside a `hashchange`
 * listener, is what closes the race: by the time any navigation is
 * attempted, the claim already exists, so every `useHashView`'s own listener
 * below sees it on its very first line and returns without touching its
 * route — the claiming form's own listener is the only one left to act, and
 * it is no longer racing anybody.
 */
export function useHashNavigationGuard(active: boolean): () => void {
  // Whether THIS instance currently holds a claim — not just `active`, but
  // "has the effect below actually incremented the counter yet". Lets the
  // returned `release` and the effect's own cleanup share one guard against
  // double-releasing, whichever fires first.
  const heldRef = useRef(false);

  // CodeRabbit review, PR #2054: an approved navigation still raced the
  // guard's release. `WorkflowCreateDialog`'s discard confirm answers
  // `onOpenChange(false)` and *then* replays the hash change — but
  // `onOpenChange(false)` only makes `dirty` false on the NEXT render, and
  // this hook's own cleanup (below) runs as a passive effect, which React
  // flushes on its own schedule rather than synchronously inside that click
  // handler. The replayed `hashchange` — fired from a plain assignment to
  // `window.location.hash`, a macrotask — could reach every `useHashView`'s
  // listener before that cleanup ran, so the still-active guard swallowed
  // the very navigation the operator had just approved: the address bar
  // moved, nothing else did. `release` lets the approving code drop the
  // claim synchronously, in the same click handler, before it touches
  // `location.hash` again — no waiting on React's own schedule.
  const release = useCallback(() => {
    if (!heldRef.current) return;
    heldRef.current = false;
    activeHashNavigationGuards -= 1;
  }, []);

  useEffect(() => {
    if (!active) return;
    heldRef.current = true;
    activeHashNavigationGuards += 1;
    return release;
  }, [active, release]);

  return release;
}

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
 * `#/chat`, or `#/settings/people` for a view with sub-pages) so views
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
      // A dirty form elsewhere has claimed this navigation — see
      // `useHashNavigationGuard`'s doc. Its own listener will restore the
      // hash and ask; this router leaves its route alone until then, so
      // there is nothing here for the restoration to race.
      if (activeHashNavigationGuards > 0) return;
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
    // CodeRabbit review, PR #2054: the guard above only protected the
    // `hashchange` LISTENER path (Back/Forward, a manual address edit) — this
    // function is the OTHER way a route changes, called directly by sidebar
    // links and every other in-app "go to this view" affordance, and it used
    // to call `setRoute` synchronously in the same breath as the assignment
    // above, bypassing the listener (and its guard check) entirely. A sidebar
    // click while `WorkflowCreateDialog` was dirty unmounted it before its own
    // listener ever got a chance to run.
    //
    // The hash assignment above still happens unconditionally — that is what
    // gives the guard-holder's OWN `hashchange` listener something to react
    // to, restore, and ask about, exactly as it already does for a Back
    // press. Only `setRoute` is withheld here: this router's rendered view
    // stays put until the guard clears, the same outcome its own `onHash`
    // listener would reach reacting to the very `hashchange` event this
    // assignment is about to raise.
    if (activeHashNavigationGuards > 0) return;
    setRoute([next, nextSub ?? null]);
  }, []);

  return [route[0], route[1], navigate];
}
