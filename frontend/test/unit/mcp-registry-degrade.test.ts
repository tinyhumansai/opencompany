import { describe, expect, it } from "vitest";

import { expectCatalogue } from "@/api/mcp-registry";
import { ApiError } from "@/api/types";
import { missingEnvKeys, REGISTRY_UNWIRED_NOTICE, registryOutage } from "@/lib/mcp-registry";

/**
 * Issue #1270 — the directory half must not be able to take the tab down with
 * it.
 *
 * The catalogue is two network hops away (the host federates Smithery and the
 * official MCP registry) and the routes serving it are behind the host's `mcp`
 * Cargo feature, so a build without it answers `404 not_wired` on every one of
 * them. Neither is a reason for the company's installed servers to stop
 * rendering: a dead directory is an empty search result with a reason, and a
 * missing build feature is a sentence about the build.
 *
 * The property that makes that structural is that classification is **total** —
 * every rejection the browse surface can see becomes a notice, and nothing
 * escapes as an exception into the section rendering List A.
 */
describe("registryOutage", () => {
  it("reads not_wired as a missing feature, not an error", () => {
    const outage = registryOutage(new ApiError(404, "not_wired", "mcp registry is not wired"));

    expect(outage).toEqual({ kind: "unwired" });
    // The operator gets a sentence about the build. Nothing here is their fault
    // and nothing here is actionable from the console.
    expect(REGISTRY_UNWIRED_NOTICE).toContain("isn't enabled in this build");
    // …and it says the rest of the tab is fine, because it is.
    expect(REGISTRY_UNWIRED_NOTICE).toContain("servers above still work");
  });

  it("keeps a not_wired 404 distinct from any other 404", () => {
    // The section already treats a bare 404 on `GET …/mcp/servers` as "this
    // host has no MCP surface". A directory 404 that is not `not_wired` is a
    // failure of this route, and saying "the feature isn't in this build" would
    // be a claim about the deployment we cannot make.
    expect(registryOutage(new ApiError(404, "not_found", "no such entry"))).toEqual({
      kind: "error",
      message: "no such entry",
    });
  });

  it("carries the host's own sentence when it sent one", () => {
    const outage = registryOutage(new ApiError(502, "upstream", "Smithery didn't answer."));

    expect(outage).toEqual({ kind: "error", message: "Smithery didn't answer." });
  });

  it("classifies every failure, including ones that are not ApiErrors", () => {
    // Totality is the point: whatever a rejected fetch hands back becomes a
    // notice inside the browse panel. A throw escaping from here is the one
    // outcome that would blank the server list.
    for (const thrown of [undefined, null, "boom", new Error("boom"), { status: 500 }, 0]) {
      const outage = registryOutage(thrown);
      expect(outage.kind).toBe("error");
      expect(outage.kind === "error" && outage.message.length).toBeGreaterThan(0);
    }
  });

  it("never shows a harness error's internal wording", () => {
    const outage = registryOutage(
      new ApiError(
        500,
        "harness_error",
        "harness error: mcp registry search failed: mcp transport failure for `https://registry.modelcontextprotocol.io`: error sending request",
      ),
    );
    expect(outage).toEqual({
      kind: "error",
      message: "The MCP directory didn't answer. Try again in a moment.",
    });
  });

  it("passes the host's typed directory sentences through", () => {
    for (const code of ["registry_timeout", "registry_unavailable"]) {
      const outage = registryOutage(
        new ApiError(504, code, "The MCP directory is taking too long to search right now."),
      );
      expect(outage).toEqual({
        kind: "error",
        message: "The MCP directory is taking too long to search right now.",
      });
    }
  });

  it("reads the console's own request deadline as a slow directory", () => {
    const outage = registryOutage(
      new ApiError(0, "timeout", "the company host at http://x did not respond in time"),
    );
    expect(outage.kind === "error" && outage.message).toContain("taking too long");
  });

  it("falls back to its own sentence when the host's envelope was blank", () => {
    expect(registryOutage(new ApiError(500, "boom", "   "))).toEqual({
      kind: "error",
      message: "The MCP directory didn't answer. Try again in a moment.",
    });
  });
});

/**
 * `client.get<T>` casts an unparsed body straight to `T`, so a declared type is
 * a claim about the host and never a check on it. Issue #414 is what that costs
 * when the claim is wrong: the bad value flows on and throws at render, several
 * frames from the fetch that caused it.
 */
describe("expectCatalogue", () => {
  it("passes a page through", () => {
    const body = {
      servers: [{ qualifiedName: "@org/git" }],
      page: 2,
      totalPages: 7,
    };

    expect(expectCatalogue(body)).toEqual(body);
  });

  it("accepts an empty page — a directory with no matches is not a failure", () => {
    expect(expectCatalogue({ servers: [], page: 1, totalPages: 0 })).toEqual({
      servers: [],
      page: 1,
      totalPages: 0,
    });
  });

  it("defaults the paging numbers rather than putting NaN on the screen", () => {
    const page = expectCatalogue({ servers: [] });

    expect(page.page).toBe(1);
    expect(page.totalPages).toBe(0);
  });

  it("rejects a body whose servers are not a list", () => {
    for (const body of [undefined, null, [], {}, { servers: null }, { servers: "git" }]) {
      expect(() => expectCatalogue(body)).toThrow(ApiError);
    }
  });

  it("marks a bad shape as not the host refusing", () => {
    try {
      expectCatalogue({ servers: {} });
      expect.unreachable();
    } catch (err) {
      const api = err as ApiError;
      expect(api.code).toBe("unexpected_shape");
      // Nothing was refused — the answer was unusable.
      expect(api.status).toBe(0);
      expect(api.fromHost).toBe(false);
    }
  });
});

/**
 * An entry declares the env keys it needs and the install form's fields are
 * exactly those. A key left blank is a key the operator has not filled in — and
 * an install that stored it anyway would come back `unauthorized` while the row
 * reported a credential as configured.
 */
describe("missingEnvKeys", () => {
  it("names every key still without a value", () => {
    expect(missingEnvKeys(["GITHUB_TOKEN", "GITHUB_ORG"], { GITHUB_ORG: "acme" })).toEqual([
      "GITHUB_TOKEN",
    ]);
  });

  it("treats whitespace as unfilled", () => {
    expect(missingEnvKeys(["TOKEN"], { TOKEN: "   " })).toEqual(["TOKEN"]);
  });

  it("is satisfied by every key having a value", () => {
    expect(missingEnvKeys(["TOKEN"], { TOKEN: "abc" })).toEqual([]);
  });

  it("asks for nothing when the entry declared nothing", () => {
    // A directory entry with no credentials installs straight through.
    expect(missingEnvKeys([], {})).toEqual([]);
    // …and a value for a key the entry never declared does not make it required.
    expect(missingEnvKeys([], { STRAY: "x" })).toEqual([]);
  });
});
