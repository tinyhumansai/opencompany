// Avatar cache lifecycle invariants: deleted nodes are revalidated after remount,
// and object URLs used by mounted tiles cannot be evicted.

// @vitest-environment node

import { describe, expect, it, vi } from "vitest";
import { OpenCompanyClient } from "@/api/client";

function client() {
  return new OpenCompanyClient({ baseUrl: "https://host", company: null, operatorToken: null, sessionHeader: null });
}

function node(i: number) { return `01J8Z5Q9YQ${String(i).padStart(14, "0")}`; }

function setup() {
  vi.resetModules();
  let sequence = 0;
  const revoked: string[] = [];
  vi.stubGlobal("URL", {
    createObjectURL: vi.fn(() => `blob:url-${sequence++}`),
    revokeObjectURL: vi.fn((url: string) => revoked.push(url)),
  });
  const requested: string[] = [];
  vi.stubGlobal("fetch", vi.fn(async (input: string | URL | Request) => {
    requested.push(String(input));
    return { ok: true, status: 200, blob: async () => new Blob([String(input)]) } as unknown as Response;
  }));
  return { requested, revoked };
}

describe("avatar cache lifecycle", () => {
  it("revalidates a forgotten node after an unmount and remount", async () => {
    const { resolveAvatarSrc, forgetAvatarNode } = await import("@/lib/avatar");
    const { requested } = setup();
    const id = node(1);
    await resolveAvatarSrc(client(), "acme", `blob:${id}`);
    forgetAvatarNode(client(), "acme", id);
    await resolveAvatarSrc(client(), "acme", `blob:${id}`);
    expect(requested).toHaveLength(2);
    vi.unstubAllGlobals();
  });

  it("does not revoke a URL while its consumer remains mounted", async () => {
    const { resolveAvatarSrc, retainAvatar, releaseAvatar } = await import("@/lib/avatar");
    const { revoked } = setup();
    const id = node(2);
    const url = await resolveAvatarSrc(client(), "acme", `blob:${id}`) as string;
    retainAvatar(client(), "acme", id);
    for (let i = 0; i < 65; i++) await resolveAvatarSrc(client(), "acme", `blob:${node(i + 10)}`);
    expect(revoked).not.toContain(url);
    releaseAvatar(client(), "acme", id);
    vi.unstubAllGlobals();
  });

  it("enumerates cached node ids only within the caller's scope", async () => {
    const { resolveAvatarSrc, forgetAvatarNode, cachedAvatarNodeIds } = await import("@/lib/avatar");
    const { } = setup();
    const id = node(3);
    await resolveAvatarSrc(client(), "acme", `blob:${id}`);
    // The cached face is visible to its own scope and to no other.
    expect(cachedAvatarNodeIds(client(), "acme")).toEqual([id]);
    expect(cachedAvatarNodeIds(client(), "other")).toEqual([]);
    expect(cachedAvatarNodeIds(new OpenCompanyClient({ baseUrl: "https://elsewhere", company: null, operatorToken: null, sessionHeader: null }), "acme")).toEqual([]);
    // Forgetting it empties the enumeration — the fresh-mount revalidation
    // path converges to nothing on the next load.
    forgetAvatarNode(client(), "acme", id);
    expect(cachedAvatarNodeIds(client(), "acme")).toEqual([]);
    vi.unstubAllGlobals();
  });
});
