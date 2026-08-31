// Which host's console the address names, and how a switch reaches history
// (issue #1358).
//
// ## Why the hash carries a host at all
//
// Host selection used to be plain React state — `onSelect: setSelected` in
// `App` — while the hash named a page without saying whose. Two defects
// followed, and they compounded into a third that was invisible:
//
//  1. **Back could not undo a switch.** Nothing was pushed, so the most natural
//     recovery from "I picked the wrong host and it is down" did nothing to the
//     selection. The only way back was to find the switcher again.
//  2. **The address lied.** While a failed host's "Can't connect" screen was
//     up, the bar went on naming a page of the host just left. Copying it
//     handed somebody a Tasks board rather than the failure being looked at.
//  3. **So Back silently spent the other host's place.** A Back pressed on the
//     error screen popped an entry belonging to the *working* host. Nothing on
//     screen changed — the failed host's console does not read the route — so
//     it read as inert, and two presses later switching back landed on Overview
//     instead of the Tasks board it had been rewound from.
//
// Making the host part of the address fixes all three: `#/ledgers/tasks?host=…`
// says whose page it is, a switch becomes an ordinary push that Back undoes,
// and no Back is inert any more because the entry it returns to names a host.
//
// ## Why a query companion rather than a path segment
//
// `useHashView`'s segment parsing already strips everything from `?` onward
// (`readSegments` in `use-hash-view.ts`), so a query companion coexists with
// the view/sub routing untouched — the same seam `useHashFlag` uses for `?new`.
// A path segment would instead rewrite every address the console has emitted.
//
// The scope is not a transient flag, though, and that is the one way it differs
// from `useHashFlag`: a view change carries it forward, where `?new` is meant
// to be dropped by the next navigation. Every writer that replaces the hash
// wholesale therefore has to carry it — see {@link withHostParam} — and
// {@link useHostAddress} re-asserts it after the ones that cannot.
//
// ## Only when there is something to disambiguate
//
// A console holding one host writes no `?host=`. There is nothing ambiguous
// about its address, and connection ids are minted per browser
// (`registry.mintId`), so an opaque id in every copied link means nothing to
// whoever receives it. The scope appears the moment a second host is
// registered, and `selectHost` stamps the entry being left on the way out — so
// even the first switch, made from an era of the history stack that predates
// the second host, is undoable.

import { useCallback, useEffect, useState } from "react";

import type { ConnectionId } from "@/connections/types";

/** The hash query key naming the connection whose console is on screen. */
export const HOST_PARAM = "host";

/** The current hash, split into its path and its query. */
function readHash(): { path: string; params: URLSearchParams } {
  const [path = "", query = ""] = window.location.hash.replace(/^#/, "").split("?");
  return { path, params: new URLSearchParams(query) };
}

/**
 * Serialises a hash back, leaving value-less flags bare.
 *
 * `URLSearchParams` always writes a `=`, and `useHashFlag` deliberately writes
 * `?new` rather than `?new=`. Round-tripping the query to change one key must
 * not quietly rewrite the flags standing next to it.
 */
function formatHash(path: string, params: URLSearchParams): string {
  const query = params.toString().replace(/=(?=&|$)/g, "");
  // An address with no path yet — a bare `/` landing, before the shell's router
  // canonicalizes it — still gets a readable one rather than `#?host=…`.
  return `#${path || "/"}${query ? `?${query}` : ""}`;
}

/** The host the address currently names, if it names one. */
export function readHostParam(): ConnectionId | null {
  return readHash().params.get(HOST_PARAM) || null;
}

/** The current hash with the host scope set to `id`, or removed when `null`. */
export function hashWithHost(id: ConnectionId | null): string {
  const { path, params } = readHash();
  if (id) params.set(HOST_PARAM, id);
  else params.delete(HOST_PARAM);
  return formatHash(path, params);
}

/**
 * `#/<path>` with the host scope carried over from the current address.
 *
 * `path` is the hash path with no `#` and no leading `/` — `"ledgers/tasks"`.
 *
 * For the writers that replace the hash wholesale rather than editing it. The
 * host rides along because it is a scope and not a page; every other query key
 * is dropped, which is what `useHashFlag`'s flags want — `?new` belongs to the
 * screen it was opened over, not to the next one.
 *
 * `replaceState` callers need this most: replacing fires no `hashchange`, so a
 * scope dropped there has no event for {@link useHostAddress} to repair it on.
 */
export function withHostParam(
  path: string,
  query: Readonly<Record<string, string | null>> = {},
): string {
  const host = readHostParam();
  const params = new URLSearchParams(host ? `${HOST_PARAM}=${host}` : "");
  for (const [key, value] of Object.entries(query)) {
    if (value === null) params.delete(key);
    else params.set(key, value);
  }
  return formatHash(`/${path}`, params);
}

export interface HostRoute {
  /** The host the address names, or the fallback when it names none. */
  selected: ConnectionId | null;
  /**
   * Puts another host's console on screen.
   *
   * A navigation, so it pushes and Back undoes it. `from` is the host actually
   * on screen — not `selected`, which is `null` on a desktop that has never
   * been asked — and is what the entry being left gets stamped with.
   */
  selectHost: (id: ConnectionId | null, from: ConnectionId | null) => void;
  /**
   * Moves the selection without touching history.
   *
   * For a host that stopped existing: forgetting one is not somewhere Back
   * should be able to return to, and the entry left behind would name a
   * connection this client no longer holds.
   */
  resettleHost: (id: ConnectionId | null) => void;
}

/**
 * The host whose console is on screen, read from and written to the address.
 *
 * `fallback` is what to open when the address names no host: the bootstrap
 * connection in a browser, `null` on a desktop that has not yet heard back
 * about its own.
 */
export function useHostRoute(fallback: ConnectionId | null): HostRoute {
  const [selected, setSelected] = useState<ConnectionId | null>(
    () => readHostParam() ?? fallback,
  );

  // Follow Back, Forward, and a host typed into the address bar.
  //
  // An address carrying no scope leaves the selection alone rather than
  // resetting it. That is what an ordinary view navigation looks like for the
  // moment before the scope is stamped back on, and what every address in a
  // one-host console looks like permanently — treating it as "no host chosen"
  // would bounce the console back to the bootstrap connection on every click.
  useEffect(() => {
    const onHash = () => {
      const named = readHostParam();
      if (named) setSelected(named);
    };
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  const selectHost = useCallback((id: ConnectionId | null, from: ConnectionId | null) => {
    // Stamp the entry being left with the host it was showing, before pushing
    // the one being opened. A console that held one host until a moment ago
    // wrote no scope at all, so without this the entry Back returns to names
    // nobody — and Back is inert for exactly the first switch, which is the
    // gesture issue #1358 was reported from.
    if (from && !readHostParam()) {
      window.history.replaceState(null, "", hashWithHost(from));
    }
    const next = hashWithHost(id);
    // A plain assignment, not `replaceState`: a real entry the Back button pops
    // is the entire point.
    if (next !== window.location.hash) window.location.hash = next;
    setSelected(id);
  }, []);

  const resettleHost = useCallback((id: ConnectionId | null) => setSelected(id), []);

  return { selected, selectHost, resettleHost };
}

/**
 * Keeps the address naming the host actually on screen.
 *
 * A standing repair rather than a one-off write, because two things move
 * underneath it. The console has writers that replace the hash wholesale — a
 * company switch clearing a stale entity id, a workflow reconciling itself off
 * a graph that was deleted — and while those carry the scope themselves, the
 * ones that merely *assign* the hash do not, and only an event puts it back.
 * And the roster moves: a host forgotten from Manage hosts leaves its id in a
 * bar that now names nothing.
 *
 * **Never overwrites a scope that names a live host.** That entry is the
 * authority — it is what a Back just restored and what `useHostRoute` is
 * following — so a rewrite here would fight the browser and undo the Back.
 *
 * `connectionIds` is the joined id list rather than the connection array for
 * the reason `App`'s probe effect joins it too: every status change emits a
 * fresh array, and depending on the array would re-register this listener on
 * each one.
 */
export function useHostAddress(
  activeId: ConnectionId | null,
  connectionIds: string,
  scoped: boolean,
): void {
  useEffect(() => {
    const known = new Set(connectionIds.split(",").filter(Boolean));
    const assert = () => {
      const named = readHostParam();
      if (named && known.has(named)) return;
      const next = hashWithHost(scoped ? activeId : null);
      if (next !== window.location.hash) window.history.replaceState(null, "", next);
    };
    assert();
    window.addEventListener("hashchange", assert);
    return () => window.removeEventListener("hashchange", assert);
  }, [activeId, connectionIds, scoped]);
}
