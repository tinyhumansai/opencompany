// The uploaded-avatar object-URL cache is bounded, and eviction revokes.
//
// `blobUrls` in `src/lib/avatar.ts` is module-level and cached for the life of
// the tab, which is the right trade for faces that recur on every page — but a
// cache that never gave anything back would pin one blob per distinct uploaded
// node ever viewed, and that set is unbounded (every face change mints a new
// node, and the host in the key multiplies the set across connections). Past
// the cap the oldest entry must be dropped *and* its object URL revoked, or
// the backing blob stays alive even though the cache entry is gone.
//
// Two corollaries of "bounded" are pinned here too. Eviction must never pick
// an entry whose fetch is still in flight: such an entry pins no blob, so
// dropping it saves nothing and throws away a fetch a mounted tile is waiting
// on. And when a workspace node is deleted, `forgetAvatarNode` drops its face
// on the spot rather than letting the cache keep drawing a file that no longer
// exists until the cap or a reload.

// @vitest-environment node

import { describe, expect, it, vi } from "vitest";

import { OpenCompanyClient } from "@/api/client";

/** The cache is module-level, so each test re-imports `avatar` fresh. */
async function freshAvatar() {
  vi.resetModules();
  return import("@/lib/avatar");
}

/** Node ids must be minted per host and unique — distinct faces in the tests. */
function node(i: number): string {
  return `01J8Z5Q9YQ${String(i).padStart(14, "0")}`;
}

/** Stub the URL APIs with distinct URLs so revocation is observable per face. */
function stubUrlApi() {
  let n = 0;
  const createObjectURL = vi.fn(() => `blob:face-${n++}`);
  const revokeObjectURL = vi.fn();
  (URL as { createObjectURL?: unknown }).createObjectURL = createObjectURL;
  (URL as { revokeObjectURL?: unknown }).revokeObjectURL = revokeObjectURL;
  return { createObjectURL, revokeObjectURL };
}

/** Stub `fetch` to answer any request with a tiny blob, recording the URLs. */
function stubFetch() {
  const requested: string[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: string | URL | Request) => {
      requested.push(String(input));
      return {
        ok: true,
        status: 200,
        blob: async () => new Blob([String(input)]),
      } as unknown as Response;
    }),
  );
  return requested;
}

function client() {
  return new OpenCompanyClient({
    baseUrl: "https://host",
    company: null,
    operatorToken: null,
    sessionHeader: null,
  });
}

describe("resolveAvatarSrc bounded cache", () => {
  it("revokes the oldest object URL and refetches the face once past the cap", async () => {
    const { resolveAvatarSrc } = await freshAvatar();
    const { revokeObjectURL } = stubUrlApi();
    const requested = stubFetch();

    // `MAX_BLOB_URLS` is 64, so a 65th distinct face evicts the first.
    const ids = Array.from({ length: 65 }, (_, i) => node(i));
    for (const id of ids) {
      await resolveAvatarSrc(client(), "acme", `blob:${id}`);
    }

    // The first entry was evicted, and its object URL revoked — not silently
    // dropped, which would keep the blob alive with no way to reach it.
    expect(revokeObjectURL).toHaveBeenCalledTimes(1);

    // The evicted face is a cache miss again: asking for it fetches once more
    // rather than answering from the hoard.
    const before = requested.length;
    await resolveAvatarSrc(client(), "acme", `blob:${ids[0]}`);
    expect(requested).toHaveLength(before + 1);

    vi.unstubAllGlobals();
  });

  it("never evicts a face whose fetch is still in flight", async () => {
    const { resolveAvatarSrc } = await freshAvatar();
    const { revokeObjectURL } = stubUrlApi();

    // The 65th request stays in flight until released, so it has no URL yet
    // when the 66th and 67th faces push the map over the cap.
    let release!: () => void;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    const requested: string[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: string | URL | Request) => {
        requested.push(String(input));
        if (requested.length === 65) await gate;
        return {
          ok: true,
          status: 200,
          blob: async () => new Blob([String(input)]),
        } as unknown as Response;
      }),
    );

    const ids = Array.from({ length: 67 }, (_, i) => node(i));
    for (const id of ids.slice(0, 64)) {
      await resolveAvatarSrc(client(), "acme", `blob:${id}`);
    }
    // 65th starts and is held in flight; 66th and 67th resolve and each
    // evicts the oldest *resolved* face — skipping the pending 65th.
    const inFlight = resolveAvatarSrc(client(), "acme", `blob:${ids[64]}`);
    for (const id of ids.slice(65)) {
      await resolveAvatarSrc(client(), "acme", `blob:${id}`);
    }
    // The cap is enforced while the 65th is held — faces 0, 1 and 2 get
    // evicted at the 65th, 66th and 67th insertions — but never the held
    // 64th, which is still in flight and therefore skipped.
    expect(revokeObjectURL).toHaveBeenCalledTimes(3);

    release();
    const url = await inFlight;

    // The held face was not thrown away by eviction: it resolves to a live
    // URL rather than `null`, and that URL was never revoked.
    expect(url).not.toBeNull();
    expect(revokeObjectURL).not.toHaveBeenCalledWith(url);

    vi.unstubAllGlobals();
  });

  it("forgetAvatarNode revokes the URL and makes the face a cache miss", async () => {
    const { resolveAvatarSrc, forgetAvatarNode } = await freshAvatar();
    const { revokeObjectURL } = stubUrlApi();
    const requested = stubFetch();

    // One face, resolved and cached.
    const id = node(0);
    const url = (await resolveAvatarSrc(client(), "acme", `blob:${id}`)) as string;

    // Deleting the node revokes its object URL — the backing blob is released
    // rather than pinned by a cache entry nothing will read again.
    forgetAvatarNode(client(), "acme", id);
    expect(revokeObjectURL).toHaveBeenCalledWith(url);

    // And the next resolve is a miss: it fetches again instead of answering
    // from the hoard (in the real world that re-fetch 404s and the caller
    // draws the tone tile).
    const before = requested.length;
    await resolveAvatarSrc(client(), "acme", `blob:${id}`);
    expect(requested).toHaveLength(before + 1);

    vi.unstubAllGlobals();
  });

  it("forgetAvatarNode fires subscribers so a mounted tile re-resolves", async () => {
    const { resolveAvatarSrc, forgetAvatarNode, subscribeAvatarNode } = await freshAvatar();
    // Stubbed for its side effect: the URL APIs have to be observable for the
    // revoke assertions in the sibling tests, and this test only watches the
    // subscription side of a forget.
    stubUrlApi();
    const requested = stubFetch();
    const c = client();
    const id = node(0);

    // A mounted tile has resolved the face and subscribes for its node. The
    // tile keeps the decoded pixels after a revoke, so only the subscription
    // can tell it to re-resolve.
    await resolveAvatarSrc(c, "acme", `blob:${id}`);
    const notified: string[] = [];
    const unsubscribe = subscribeAvatarNode(c, "acme", id, () => notified.push(id));

    // Deleting the node reaches the mounted tile...
    forgetAvatarNode(c, "acme", id);
    expect(notified).toHaveLength(1);

    // ...and the tile that re-resolves gets a fresh fetch (a 404 in the real
    // world) rather than a revoked URL, so it draws the tone tile.
    const before = requested.length;
    await resolveAvatarSrc(c, "acme", `blob:${id}`);
    expect(requested).toHaveLength(before + 1);

    // An unmounted tile has unsubscribed; a later forget of the same node no
    // longer reaches it, so the registry does not accumulate dead callbacks.
    unsubscribe();
    forgetAvatarNode(c, "acme", id);
    expect(notified).toHaveLength(1);

    vi.unstubAllGlobals();
  });

  it("hands a face out uncached when every cache entry is pinned", async () => {
    const { resolveAvatarSrc, retainAvatar, releaseAvatar } = await freshAvatar();
    const { revokeObjectURL } = stubUrlApi();
    const requested = stubFetch();
    const c = client();
    const ids = Array.from({ length: 65 }, (_, i) => node(i));

    // A roster of mounted custom avatars: every entry is pinned, so past the
    // cap nothing is evictable.
    for (const id of ids.slice(0, 64)) {
      retainAvatar(c, "acme", id);
      await resolveAvatarSrc(c, "acme", `blob:${id}`);
    }
    retainAvatar(c, "acme", ids[64]);
    const url = (await resolveAvatarSrc(c, "acme", `blob:${ids[64]}`)) as string;
    expect(url).not.toBeNull();

    // While the tile holds it, a second resolve shares the URL (no refetch).
    const before = requested.length;
    await expect(resolveAvatarSrc(c, "acme", `blob:${ids[64]}`)).resolves.toBe(url);
    expect(requested).toHaveLength(before);

    // Releasing the tile revokes it — it was component-owned, not cache-owned.
    releaseAvatar(c, "acme", ids[64]);
    expect(revokeObjectURL).toHaveBeenCalledWith(url);

    // The cache entry went with it: the next resolve fetches again instead of
    // answering a URL that is about to be dead.
    const after = requested.length;
    await resolveAvatarSrc(c, "acme", `blob:${ids[64]}`);
    expect(requested).toHaveLength(after + 1);

    vi.unstubAllGlobals();
  });

  it("does not publish a superseded fetch under a newer request's key", async () => {
    const { resolveAvatarSrc, forgetAvatarNode } = await freshAvatar();
    const { revokeObjectURL } = stubUrlApi();
    const requested: string[] = [];
    let releaseA!: () => void;
    const gateA = new Promise<void>((resolve) => {
      releaseA = resolve;
    });
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: string | URL | Request) => {
        requested.push(String(input));
        if (requested.length === 1) await gateA;
        return {
          ok: true,
          status: 200,
          blob: async () => new Blob([String(input)]),
        } as unknown as Response;
      }),
    );

    const c = client();
    const id = node(0);
    // Request A starts and is held in flight.
    const a = resolveAvatarSrc(c, "acme", `blob:${id}`);
    // Its entry is deleted, then request B installs a fresh entry for the same
    // node before A completes — the ABA shape where the key coming back is not
    // proof the completing promise is still the map's.
    forgetAvatarNode(c, "acme", id);
    const b = resolveAvatarSrc(c, "acme", `blob:${id}`);
    const bUrl = (await b) as string;
    releaseA();
    const aUrl = await a;

    // A saw the map holding B's promise, not its own: it revoked its orphan
    // URL and answered null rather than publishing under B's key.
    expect(aUrl).toBeNull();
    expect(revokeObjectURL).toHaveBeenCalledTimes(1);
    expect(revokeObjectURL).not.toHaveBeenCalledWith(bUrl);

    // B's URL owns the key: resolving again is a hit, not another fetch.
    const before = requested.length;
    await expect(resolveAvatarSrc(c, "acme", `blob:${id}`)).resolves.toBe(bUrl);
    expect(requested).toHaveLength(before);

    vi.unstubAllGlobals();
  });
});
