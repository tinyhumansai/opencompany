import { describe, expect, it } from "vitest";

import { ApiError } from "@/api/types";
import { classifyLoadFailure } from "@/lib/section-load";

/**
 * How a connections-section fetch failure is read (issues #1470, #2081).
 *
 * #1470's bug: five sections treated ANY failure as "this host has no such
 * thing" and unmounted, so a transient 500 was indistinguishable from a feature
 * the host never had.
 *
 * #2081's bug is the same mistake in the other direction. With only a 404
 * counted as absence, the host's permanent refusals — "Composio is not compiled
 * into this build", "no Composio credential is available" — landed in the
 * transient bucket, and the Apps page asked every operator on a default build
 * to reload a section no reload could ever fill.
 *
 * The rule these pin: the STATUS decides nothing beyond 404. The host's
 * structured `code` decides, and only when the host itself sent it.
 */
function apiError(status: number, code = "err", fromHost = true): ApiError {
  return new ApiError(status, code, `http ${status}`, fromHost);
}

describe("classifyLoadFailure", () => {
  it("reads a 404 as the surface being genuinely absent", () => {
    expect(classifyLoadFailure(apiError(404))).toBe("unavailable");
  });

  it("reads a host that says not_in_build as absent, not as a failed read", () => {
    // The #2081 regression guard. This is the default build's answer on
    // `GET …/composio/connections`, and it arrives as 409.
    expect(classifyLoadFailure(apiError(409, "not_in_build"))).toBe("unavailable");
  });

  it("reads a host that says not_configured as set-up-pending, not as a failed read", () => {
    expect(classifyLoadFailure(apiError(409, "not_configured"))).toBe("unconfigured");
  });

  /**
   * The guard that keeps this from becoming the opposite bug. 409 is the most
   * overloaded status the host has — a lost publish race, an optimistic-lock
   * version skew, a duplicate desk id, a paused company — and every one of them
   * is cleared by retrying or by sending something else. Reading the status as
   * permanent would retire sections the host still serves.
   */
  it("does NOT read a bare 409 as permanent", () => {
    expect(classifyLoadFailure(apiError(409, "conflict"))).toBe("error");
    expect(classifyLoadFailure(apiError(409, "lifecycle_conflict"))).toBe("error");
    expect(classifyLoadFailure(apiError(409, "restart_required"))).toBe("error");
  });

  it("reads any other status as the host failing to answer", () => {
    expect(classifyLoadFailure(apiError(500))).toBe("error");
    expect(classifyLoadFailure(apiError(401))).toBe("error");
    expect(classifyLoadFailure(apiError(503))).toBe("error");
  });

  /**
   * A gateway that synthesised the status never consulted the host, so its code
   * cannot speak for the capability. Only the host's own envelope votes.
   */
  it("ignores a permanent-looking code that did not come from the host", () => {
    expect(classifyLoadFailure(apiError(409, "not_in_build", false))).toBe("error");
    expect(classifyLoadFailure(apiError(409, "not_configured", false))).toBe("error");
  });

  it("reads a non-ApiError (offline, a thrown string) as an error, not absence", () => {
    // A dropped connection or a malformed body is unknown state, never a
    // confident "this host has no such feature".
    expect(classifyLoadFailure(new TypeError("fetch failed"))).toBe("error");
    expect(classifyLoadFailure("boom")).toBe("error");
    expect(classifyLoadFailure(undefined)).toBe("error");
  });
});
