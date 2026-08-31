// The uploaded-avatar cache must key on the host, not only on company and node.
//
// The `blobUrls` map in `src/lib/avatar.ts` is module-level and survives a
// desktop console switching connections — `AppShell` remounts, the module does
// not reload. Two hosts that happen to hold the same company and node ids (a
// cloned or restored company, say) therefore must not share a cache entry: the
// second host would draw the first host's bytes, fetched through the first
// host's client, without ever asking its own.

// @vitest-environment node

import { describe, expect, it, vi } from "vitest";

import { OpenCompanyClient } from "@/api/client";
import { resolveAvatarSrc } from "@/lib/avatar";

describe("resolveAvatarSrc cache key", () => {
  it("does not reuse one host's object URL for another host", async () => {
    const createObjectURL = vi.fn(() => "blob:mock");
    // Node has no `createObjectURL`; the stub stands in for the browser's.
    (URL as { createObjectURL?: unknown }).createObjectURL = createObjectURL;

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

    const clientA = new OpenCompanyClient({
      baseUrl: "https://host-a",
      company: null,
      operatorToken: null,
      sessionHeader: null,
    });
    const clientB = new OpenCompanyClient({
      baseUrl: "https://host-b",
      company: null,
      operatorToken: null,
      sessionHeader: null,
    });

    await resolveAvatarSrc(clientA, "acme", "blob:01J8Z5Q9YQ0000000000000000");
    await resolveAvatarSrc(clientB, "acme", "blob:01J8Z5Q9YQ0000000000000000");

    // One fetch per host: the second host must not hit the cache entry the
    // first host populated for the same company+node.
    expect(requested).toHaveLength(2);

    vi.unstubAllGlobals();
  });
});
