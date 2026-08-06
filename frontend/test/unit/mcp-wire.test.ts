import { describe, expect, it } from "vitest";

import { expectList } from "@/api/mcp";
import { ApiError } from "@/api/types";

/**
 * Issue #414. `GET .../mcp/servers` answers a bare JSON array; the console once
 * declared it as `{ servers: McpServer[] }` and read `.servers` off it. Nothing
 * checked that claim — `client.get<T>` casts an unparsed body to `T` — so the
 * wrapper's `undefined` was stored as the server list and threw `Cannot read
 * properties of undefined (reading 'length')` at render, a stack trace with the
 * fetch nowhere in it.
 *
 * The view is now on the real surface, so that exact shape cannot come back.
 * These cover the guard that keeps the *next* wire disagreement from arriving
 * as a render-time TypeError: a body that isn't the promised list is a load
 * error raised at the fetch, not a value that flows on.
 */
describe("expectList", () => {
  it("passes a bare array through as the list", () => {
    const servers = [{ name: "notion" }, { name: "linear" }];
    expect(expectList<{ name: string }>(servers, "MCP server list")).toBe(servers);
  });

  it("accepts an empty array — a company with no servers is not a failure", () => {
    expect(expectList([], "MCP server list")).toEqual([]);
  });

  it("rejects the `{ servers }` wrapper the crash was built on", () => {
    expect(() => expectList({ servers: [{ name: "notion" }] }, "MCP server list")).toThrow(
      ApiError,
    );
  });

  it("rejects a body that is absent rather than treating it as empty", () => {
    for (const body of [undefined, null, "", 0]) {
      expect(() => expectList(body, "MCP server list")).toThrow(ApiError);
    }
  });

  it("names the route in the failure and marks it as not the host refusing", () => {
    try {
      expectList({}, "tool list for notion");
      expect.unreachable("a non-list body must throw");
    } catch (err) {
      expect(err).toBeInstanceOf(ApiError);
      const api = err as ApiError;
      expect(api.code).toBe("unexpected_shape");
      expect(api.message).toContain("tool list for notion");
      // Nothing was refused, so there is no HTTP status to report and the
      // message is ours, not the host's envelope.
      expect(api.status).toBe(0);
      expect(api.fromHost).toBe(false);
    }
  });
});
