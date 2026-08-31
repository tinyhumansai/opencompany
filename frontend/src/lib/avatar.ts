// Which face a teammate or a person wears.
//
// Everybody has one whether or not they chose it: a stable id is hashed into
// one of the mascots shipped in `public/avatars/`, so an untouched roster reads
// as a set of individuals rather than a column of grey squares. This module is
// the other half — what happens when somebody *does* choose.
//
// The stored reference grammar mirrors `src/company/avatar.rs` exactly; see
// `docs/spec/runtime/avatars.md` for why it is closed rather than "store a URL".

import type { OpenCompanyClient } from "@/api/client";

/**
 * The mascots shipped in `public/avatars/blob-<flavour>.webp`.
 *
 * **Must stay in step with `TINY_FLAVOURS` in `src/company/avatar.rs`**, which
 * is what the host validates against: a flavour one side accepts and the other
 * has no file for renders as a broken image on every surface at once.
 *
 * Eleven rather than the eight tone keys, deliberately — the tones are a hue
 * circle that avoids amber, green and red so they cannot be confused with run
 * status, and the mascots are under no such constraint.
 */
export const TINY_FLAVOURS = [
  "amber",
  "blue",
  "clay",
  "cloud",
  "ember",
  "graphite",
  "green",
  "indigo",
  "rose",
  "teal",
  "violet",
] as const;

export type TinyFlavour = (typeof TINY_FLAVOURS)[number];

/** The image types an uploaded avatar may be — the `accept` a file input wants. */
export const AVATAR_ACCEPT = "image/png,image/jpeg,image/webp,image/gif";

/**
 * The host's upload ceiling, in whole megabytes — for saying so before somebody
 * picks a file, not for enforcing it.
 *
 * The enforcement is `MAX_AVATAR_BYTES` in `src/company/avatar.rs`, and it stays
 * there: a limit checked in the browser is a limit anybody can skip. This is the
 * number the picker prints, so the copy and the refusal name the same figure.
 */
export const MAX_AVATAR_MB = 4;

/**
 * Picks a mascot from a seed, for whoever has not chosen one.
 *
 * A hash rather than a random draw, for the reason that matters to an operator:
 * a teammate keeps the same face across reloads, browsers and machines with
 * nothing persisted anywhere. Drawing randomly at creation would need a stored
 * field; drawing randomly at render would give the same teammate a new face
 * every time the page reloaded.
 *
 * Seeded on the id wherever there is one, so renaming somebody does not change
 * their face.
 */
export function hashedFlavour(seed: string): TinyFlavour {
  let hash = 0;
  for (let i = 0; i < seed.length; i++) hash = (hash * 31 + seed.charCodeAt(i)) | 0;
  return TINY_FLAVOURS[Math.abs(hash) % TINY_FLAVOURS.length];
}

/**
 * The reference to draw for somebody: what they chose, else the hashed default.
 *
 * `chosen` is the host's field, absent when nobody has chosen — which is not the
 * same as "no face", and is exactly why the host skips the key rather than
 * defaulting it.
 */
export function avatarRef(chosen: string | undefined, seed: string): string {
  const trimmed = chosen?.trim();
  return trimmed || `tiny:${hashedFlavour(seed)}`;
}

/** Where a mascot lives on disk. */
export function tinySrc(flavour: string): string {
  return `/avatars/blob-${flavour}.webp`;
}

/** The workspace node id a `blob:` reference names, or `null` for any other form. */
export function blobNodeId(ref: string): string | null {
  const trimmed = ref.trim();
  return trimmed.startsWith("blob:") ? trimmed.slice("blob:".length) : null;
}

/**
 * The `src` for a reference that needs no fetch — a mascot — or `null` for one
 * that does.
 *
 * Split from {@link resolveAvatarSrc} so the common case stays synchronous: the
 * overwhelming majority of faces on any screen are mascots, and making every
 * avatar await a promise would flash an empty gutter on every mount for the sake
 * of the few that are uploads.
 */
export function staticAvatarSrc(ref: string): string | null {
  const trimmed = ref.trim();
  if (trimmed.startsWith("tiny:")) return tinySrc(trimmed.slice("tiny:".length));
  // An unrecognised reference is drawn as nothing rather than as itself. The
  // host refuses to store anything but the two forms, so this can only be
  // version skew — and putting an unknown string into a `src=` is the one thing
  // the closed grammar exists to prevent.
  return null;
}

/**
 * Object URLs for uploaded avatars, keyed by host, company and node id.
 *
 * Module-level, and deliberately *cached* rather than revoked on unmount: an
 * avatar is drawn in dozens of places on one screen — chat gutters, facepiles,
 * the members pane, the org chart — and the same faces recur on every page the
 * operator visits. Revoking on unmount would mean refetching a teammate's face
 * each time it scrolled out of a list and back, and per-component caching would
 * fetch the same bytes once per component.
 *
 * Cached is not unbounded, though — see [`MAX_BLOB_URLS`]. The bound is what
 * keeps the "keep them" choice honest: the number of distinct uploaded nodes a
 * long-lived session can touch is not bounded by the company's *current* faces.
 * Every face change mints a new node (the old one stays in the workspace), and
 * the host in the key means visiting other hosts multiplies the same set — and
 * each entry pins a blob of up to 4 MB for as long as it is kept. Past the cap
 * the oldest entry is dropped and its object URL revoked; a face that was
 * evicted is simply fetched again the next time it scrolls into view. An entry
 * still in flight is never evicted — it pins no blob, so dropping it would save
 * nothing and would throw away a fetch a mounted tile is waiting on — and a
 * face whose node is deleted is dropped on the spot by {@link forgetAvatarNode}
 * rather than kept drawing a file that no longer exists.
 *
 * The host is part of the key because the map outlives a connection switch: the
 * desktop console remounts `AppShell` when it changes hosts, but this module
 * does not reload, so a key that named only company and node would let the
 * second host draw the first host's bytes when two hosts hold the same
 * company/node ids — a cloned or restored company, say. Node ids are minted per
 * host, so the collision is exactly the case this prefix exists for.
 *
 * The promise is cached, not the URL, so N components mounting at once share one
 * request instead of racing N.
 */
const blobUrls = new Map<string, Promise<string | null>>();
/** The resolved URL for a cache key, kept separately so eviction can revoke it. */
const blobUrlValues = new Map<string, string>();
/** Mounted avatar consumers by cache key; pinned URLs cannot be evicted. */
const blobUrlRefs = new Map<string, number>();
/**
 * URLs handed to a mounted tile when the cache is full of pinned entries.
 *
 * A cache-owned URL is released by eviction; these are the other kind — the
 * cache could not make room, so the face is owned by the tile that drew it
 * and [`releaseAvatar`] revokes it when the last such tile unmounts. Keeping
 * them apart is what lets the bound ([`MAX_BLOB_URLS`]) stay honest: it counts
 * cache-owned URLs, and an over-capacity screen's extra faces are released
 * with their tiles instead of being hoarded past the cap.
 */
const componentUrls = new Map<string, string>();
/**
 * Mounted consumers of a specific uploaded face, by cache key.
 *
 * Revoking an object URL does not unpaint an `<img>` that already decoded it,
 * so `forgetAvatarNode` deleting the cache cannot redraw a tile that has
 * already resolved — the tile has to be told. These callbacks are that
 * channel: a mounted consumer subscribes for its node, and a forget fires the
 * callbacks so the tiles re-resolve (to the tone tile, once the deleted bytes
 * 404). The registry is bounded by mounted blob-avatar consumers; every
 * subscription removes itself on unmount, and forget clears the whole set.
 */
const forgetListeners = new Map<string, Set<() => void>>();

/**
 * The most distinct uploaded faces the cache keeps at once.
 *
 * Far above what any one screen draws; the point of the number is that the
 * cache is *bounded*, not how big the bound is. Past it, the oldest entry is
 * dropped and its object URL revoked, so a session that keeps viewing new faces
 * costs bounded memory and re-fetches a cold face on return — one request per
 * face, rather than a slowly-growing hoard.
 */
const MAX_BLOB_URLS = 64;

/**
 * Drops the oldest *resolved* entry, revoking its object URL.
 *
 * An entry still in flight is deliberately skipped: it pins no blob yet, so
 * evicting it would save nothing and would throw away a fetch a mounted tile
 * is waiting on. The pending set is bounded by how many faces a single screen
 * can mount at once; every one of them lands in `blobUrlValues` on success,
 * which is where the cap actually bites.
 *
 * Returns whether anything was evicted — `false` when every resolved entry is
 * pinned by a mounted tile, which is the signal that a new face must be handed
 * out without becoming cache-owned.
 */
function evictOldestBlobUrl(): boolean {
  for (const key of blobUrls.keys()) {
    const url = blobUrlValues.get(key);
    if (url === undefined || (blobUrlRefs.get(key) ?? 0) > 0) continue;
    URL.revokeObjectURL(url);
    blobUrlValues.delete(key);
    blobUrls.delete(key);
    return true;
  }
  return false;
}

function avatarCacheKey(client: OpenCompanyClient, company: string | null, nodeId: string): string {
  return `${client.baseUrl}|${company ?? ""}|${nodeId}`;
}

/** Pin an uploaded avatar while its component is mounted. */
export function retainAvatar(client: OpenCompanyClient, company: string | null, nodeId: string): void {
  const key = avatarCacheKey(client, company, nodeId);
  blobUrlRefs.set(key, (blobUrlRefs.get(key) ?? 0) + 1);
}

/** Release a mounted avatar's cache pin. */
export function releaseAvatar(client: OpenCompanyClient, company: string | null, nodeId: string): void {
  const key = avatarCacheKey(client, company, nodeId);
  const count = blobUrlRefs.get(key) ?? 0;
  if (count <= 1) {
    blobUrlRefs.delete(key);
    // A component-owned URL (minted when the cache was full of pinned
    // entries) has no cache owner to evict it — this is the release that
    // must revoke it, or the face would pin its blob for the life of the tab.
    // The cache entry goes with it: the resolved promise answers with a URL
    // that is about to be dead, so a later mount must fetch again.
    const url = componentUrls.get(key);
    if (url) {
      URL.revokeObjectURL(url);
      componentUrls.delete(key);
      blobUrls.delete(key);
    }
  } else {
    blobUrlRefs.set(key, count - 1);
  }
}

/**
 * The `src` for any reference, fetching an uploaded one through the client.
 *
 * A plain `<img src="…/workspace/blob/{id}">` would not work: the route needs
 * the credential the client holds, and an image element cannot carry one. So the
 * bytes are fetched through the authenticated client and wrapped in an object
 * URL the element can point at — the same shape `fetchBlobUrl` uses for the
 * workspace viewer, but cached, because a face is drawn far more often than a
 * document is opened.
 *
 * Resolves to `null` when the reference names nothing this host holds any more
 * (an avatar whose node was deleted from the Files tab). Callers draw the tone
 * tile they were already drawing underneath, so a deleted image degrades to a
 * coloured square rather than to a broken-image glyph.
 */
export function resolveAvatarSrc(
  client: OpenCompanyClient,
  company: string | null,
  ref: string,
): string | Promise<string | null> {
  const staticSrc = staticAvatarSrc(ref);
  if (staticSrc) return staticSrc;
  const node = blobNodeId(ref);
  if (!node) return Promise.resolve(null);
  const key = avatarCacheKey(client, company, node);
  let pending = blobUrls.get(key);
  if (!pending) {
    pending = client
      .getBlob(`${client.scopeFor(company)}/workspace/blob/${encodeURIComponent(node)}`)
      .then((blob) => {
        // This request's entry is gone. Two ways that happens, both of which
        // mean the bytes must not be published under a key this request no
        // longer answers for: the key was evicted while the fetch was in
        // flight (nothing reads a URL cached under a dropped key), or a newer
        // request for the same node superseded it after `forgetAvatarNode`
        // removed the old entry — the ABA race where the map merely *having*
        // the key again is not proof it is this promise. Compare identity,
        // not presence.
        if (blobUrls.get(key) !== pending) {
          const orphan = URL.createObjectURL(blob);
          URL.revokeObjectURL(orphan);
          return null;
        }
        const url = URL.createObjectURL(blob);
        // Make room before caching. If every resolved entry is pinned by a
        // mounted tile — a screen drawing more custom faces than the cap —
        // nothing can be evicted, and the bound wins: the URL is handed to
        // the caller but is not cache-owned, so the cache does not grow. It
        // is revoked by `releaseAvatar` when the last tile unmounts; the key
        // stays in `blobUrls` so concurrent mounts of the same face share
        // this URL rather than racing another fetch. With no mounted tile
        // waiting and nothing evictable, the face is simply revoked on the
        // spot — nobody would ever read it.
        if (blobUrlValues.size < MAX_BLOB_URLS || evictOldestBlobUrl()) {
          blobUrlValues.set(key, url);
        } else if ((blobUrlRefs.get(key) ?? 0) > 0) {
          componentUrls.set(key, url);
        } else {
          URL.revokeObjectURL(url);
          blobUrls.delete(key);
        }
        return url;
      })
      .catch(() => {
        // Not cached as a failure: a face that 404s because the workspace was
        // mid-write should be retried on the next mount, not remembered as
        // missing for the life of the tab. A stale failure — this request's
        // entry was superseded while it was in flight — must not delete the
        // newer entry, so the identity is checked before the maps are touched.
        if (blobUrls.get(key) !== pending) return null;
        blobUrls.delete(key);
        blobUrlValues.delete(key);
        return null;
      });
    blobUrls.set(key, pending);
    if (blobUrls.size > MAX_BLOB_URLS) evictOldestBlobUrl();
  }
  return pending;
}

/**
 * Drops one face from the cache and revokes its object URL, so a node deleted
 * from the workspace stops being drawn on the next render.
 *
 * A chosen face references a workspace node (`blob:<nodeId>`); without this
 * the cache would keep drawing a face whose bytes were just deleted until the
 * cap evicted it or the tab reloaded. The workspace view calls it for every
 * node it deletes — including the contents of a deleted folder — and the next
 * resolve for that node 404s and falls back to the tone tile, which is the
 * degrade {@link resolveAvatarSrc} documents. A face deleted while still in
 * flight is handled the same way: the pending entry is dropped, and when its
 * fetch resolves the guard inside `resolveAvatarSrc` revokes the URL and
 * returns `null`.
 *
 * A tile that already drew the face keeps its pixels even after the URL is
 * revoked, so the drop also fires the node's {@link subscribeAvatarNode}
 * subscribers: a mounted tile re-resolves and draws the tone tile instead of
 * holding the deleted face until it unmounts.
 */
export function forgetAvatarNode(
  client: OpenCompanyClient,
  company: string | null,
  nodeId: string,
): void {
  const key = avatarCacheKey(client, company, nodeId);
  const url = blobUrlValues.get(key) ?? componentUrls.get(key);
  if (url) URL.revokeObjectURL(url);
  blobUrlValues.delete(key);
  componentUrls.delete(key);
  blobUrls.delete(key);
  forgetListeners.get(key)?.forEach((notify) => notify());
  forgetListeners.delete(key);
}

/**
 * Every workspace node id the cache holds a face for, within one scope.
 *
 * The hook that lets a mount revalidate the cache against an authoritative
 * tree. A view that deletes a node calls {@link forgetAvatarNode} directly,
 * and a mounted view diffs its previous tree against the new one — but a node
 * deleted while the view was unmounted is invisible to both, because the
 * view's own state started empty. Its face is still sitting in this cache,
 * though, so this is how a fresh mount finds and forgets it again.
 */
export function cachedAvatarNodeIds(
  client: OpenCompanyClient,
  company: string | null,
): string[] {
  const prefix = `${client.baseUrl}|${company ?? ""}|`;
  const ids: string[] = [];
  for (const key of blobUrls.keys()) {
    if (key.startsWith(prefix)) ids.push(key.slice(prefix.length));
  }
  return ids;
}

/**
 * Subscribes to a node's face being forgotten, for a tile that already drew it.
 *
 * Revoking an object URL does not unpaint an `<img>` that has decoded it, so a
 * mounted avatar that resolved before the node was deleted has no reason to
 * re-resolve — its reference is unchanged and its cache entry is gone. This is
 * how such a tile learns to: when {@link forgetAvatarNode} drops the node, the
 * callback fires and the tile re-resolves (the deleted bytes 404, so it draws
 * the tone tile it was already drawing underneath). Returns an unsubscribe;
 * call it on unmount, or the registry leaks one entry per mounted tile.
 */
export function subscribeAvatarNode(
  client: OpenCompanyClient,
  company: string | null,
  nodeId: string,
  notify: () => void,
): () => void {
  const key = avatarCacheKey(client, company, nodeId);
  let listeners = forgetListeners.get(key);
  if (!listeners) {
    listeners = new Set();
    forgetListeners.set(key, listeners);
  }
  const set = listeners;
  set.add(notify);
  return () => {
    set.delete(notify);
    if (set.size === 0) forgetListeners.delete(key);
  };
}

/** What `POST …/avatars` answers with. */
export interface UploadedAvatar {
  /** The reference to store — `blob:<nodeId>`. */
  avatar: string;
  nodeId: string;
  /** The type the host **sniffed** from the bytes, not the one the browser declared. */
  mime: string;
  size: number;
}

/**
 * Uploads an image and returns the reference to store.
 *
 * Nothing is worn by uploading: the caller then saves the reference onto a
 * teammate or onto themselves. Keeping the two steps apart is what lets a picker
 * preview an image before it is committed.
 */
export async function uploadAvatar(
  client: OpenCompanyClient,
  company: string | null,
  file: File,
): Promise<UploadedAvatar> {
  const form = new FormData();
  form.append("file", file, file.name);
  return client.postForm<UploadedAvatar>(`${client.scopeFor(company)}/avatars`, form);
}
