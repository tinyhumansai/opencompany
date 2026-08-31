// @vitest-environment jsdom
import { describe, expect, it } from "vitest";

import { VIEWS, type View } from "@/lib/console-routes";

/**
 * Against the REAL route table, never a copy.
 *
 * Duplicating the shell's table is exactly how issue #1311 went unnoticed for
 * four months: `#/pages` collapsed onto Overview while a test asserted a
 * verbatim copy that still listed it.
 */
describe("the observatory address", () => {
  it("is routable", () => {
    expect(VIEWS).toContain("observatory" as View);
  });

  it("is not the fallback view", () => {
    // If it ever became `VIEWS[0]`, every unknown address would land here and
    // the test above would still pass.
    expect(VIEWS[0]).not.toBe("observatory");
  });

  it("has no duplicate entries", () => {
    expect(new Set(VIEWS).size).toBe(VIEWS.length);
  });
});
