// A per-costume+colorway cache of the mascot's *resting* look, captured once
// from a live Rive canvas and reused as a plain `<img>` everywhere except the
// true hero surfaces.
//
// # Why this exists
//
// A live `MascotAvatar` mounts its own `@rive-app/react-canvas` instance —
// a WASM runtime plus a running render loop — and that is cheap at the one or
// two hero spots it was built for (the profile sheet, the agent detail
// header). It stops being cheap once `AvatarTile` (`components/teammate-avatar.tsx`)
// started drawing a mascot at every surface a `TeammateAvatar` appears:
// measured live against a 30-message transcript from one `mascot:animated`
// teammate, 30 independent canvases mounted at once cost ~240 MB of JS heap
// (2026-09-26, Chromium, this repo's `companies/vending_machine_co` fixture) —
// for a screen where every one of those 30 faces is pixel-identical, since
// none of them wire up hover/replying reactivity (`AvatarTile` never passes a
// `state`, so every non-hero mascot renders its idle baseline and nothing
// else moves it). Thirty live WASM runtimes rendering one static picture
// thirty times over is pure waste, and the waste is unbounded: it scales with
// how many *messages* a transcript has, not how many distinct looks exist —
// nine costumes times six times six colors is 324 combinations, small and
// fixed, however many rows are on screen.
//
// # The shape
//
// `costumeKey` names a look. `getMascotSnapshot`/`publishMascotSnapshot` are a
// plain synchronous cache — same shape as `blobUrls` in `lib/avatar.ts`, one
// map instead of a moved-goalposts eviction scheme, because 324 data URLs at a
// few KB each is nowhere near the size that cache's `MAX_BLOB_URLS` bound
// exists for. `subscribeMascotSnapshot` is how an already-mounted, still-
// loading tile learns that *some other* tile for the same look finished
// first: every uncached mount races to capture (there is no cross-instance
// lock — see the module docs on `MascotWarmer` in `teammate-avatar.tsx` for
// why that is a deliberate simplification, not an oversight), and the instant
// one of them wins, every other racer for that key tears its own live canvas
// down rather than finishing a capture nobody needs.

/** A costume + skin + hand combination's cache key. Mode is deliberately not
 * part of it: `"static"`'s resting frame and `"animated"`'s idle baseline are
 * the same frame (`mascot-avatar.tsx`'s own docs), and nothing outside the
 * two hero surfaces ever requests anything but idle, so one snapshot serves
 * both modes everywhere this cache is consulted. */
export function mascotCostumeKey(
  costume: string | undefined,
  skinColor: string | undefined,
  handColor: string | undefined,
): string {
  return `${costume ?? "default"}|${skinColor ?? "default"}|${handColor ?? "default"}`;
}

const snapshots = new Map<string, string>();
const listeners = new Map<string, Set<(url: string) => void>>();

/** The cached data URL for a look, or `undefined` if nobody has captured it yet. */
export function getMascotSnapshot(key: string): string | undefined {
  return snapshots.get(key);
}

/**
 * Publishes a captured look, first-write-wins.
 *
 * Several racing warmers can call this for the same key (see the module
 * docs); only the first is kept; wakes every subscriber either way, so a
 * racer that lost still learns to stop.
 */
export function publishMascotSnapshot(key: string, url: string): void {
  if (!snapshots.has(key)) snapshots.set(key, url);
  const published = snapshots.get(key)!;
  listeners.get(key)?.forEach((notify) => notify(published));
  listeners.delete(key);
}

/**
 * Learn when a look becomes cached. Returns an unsubscribe; call it on
 * unmount or on seeing the cache already warm, or the registry leaks one
 * entry per mounted, still-loading tile.
 */
export function subscribeMascotSnapshot(key: string, notify: (url: string) => void): () => void {
  let set = listeners.get(key);
  if (!set) {
    set = new Set();
    listeners.set(key, set);
  }
  set.add(notify);
  return () => {
    set!.delete(notify);
    if (set!.size === 0) listeners.delete(key);
  };
}
