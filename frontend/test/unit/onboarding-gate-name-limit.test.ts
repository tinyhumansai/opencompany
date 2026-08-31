import { describe, expect, it } from "vitest";

import { clampToCompanyNameLimit } from "@/onboarding/OnboardingGate";

/**
 * PR #1875 review finding: the company-name field's native `maxLength`
 * attribute counted UTF-16 code units, not the Unicode scalar values the
 * host counts with `chars().count()` (`src/server/ops/company_profile.rs`).
 * An astral character (most emoji, some scripts) is one scalar value but two
 * UTF-16 units, so a name built from 101-200 of them passed the host's
 * 200-character limit but could not be typed past 100 of them through the
 * old `maxLength={200}` attribute.
 *
 * These pin `clampToCompanyNameLimit` against exactly that gap: it must
 * agree with the host's scalar-value definition of "character", not the
 * DOM's UTF-16 one.
 */
describe("clampToCompanyNameLimit", () => {
  it("does not truncate a name at or under the limit", () => {
    const name = "Acme Inc.";
    expect(clampToCompanyNameLimit(name)).toBe(name);
  });

  it("truncates a plain-ASCII name over the limit to exactly the limit", () => {
    const over = "a".repeat(250);
    const clamped = clampToCompanyNameLimit(over);
    expect(clamped).toHaveLength(200);
    expect(clamped).toBe("a".repeat(200));
  });

  it("allows 200 astral characters (e.g. emoji) — the exact gap this closes", () => {
    // U+1F600 GRINNING FACE is one Unicode scalar value but a UTF-16 surrogate
    // pair (two code units). The old `maxLength={200}` attribute would refuse
    // input past 100 of these even though the host's `chars().count()` limit
    // allows all 200.
    const astral = "\u{1F600}".repeat(200);
    expect(Array.from(astral)).toHaveLength(200);
    expect(astral.length).toBe(400); // UTF-16 length — what `maxLength` used to count.
    expect(clampToCompanyNameLimit(astral)).toBe(astral);
  });

  it("truncates an over-limit astral name to exactly 200 scalar values, never splitting a surrogate pair", () => {
    const astral = "\u{1F600}".repeat(210);
    const clamped = clampToCompanyNameLimit(astral);
    expect(Array.from(clamped)).toHaveLength(200);
    expect(clamped).toBe("\u{1F600}".repeat(200));
    // A naive UTF-16 slice (`astral.slice(0, 200)`) would cut mid-pair and
    // produce a lone surrogate; scalar-aware truncation never does.
    expect(clamped.length).toBe(400);
  });

  it("counts a mixed ASCII+astral name by scalar value, matching the host", () => {
    const mixed = "a".repeat(199) + "\u{1F600}".repeat(2);
    // 201 scalar values total — one over the limit.
    expect(Array.from(mixed)).toHaveLength(201);
    const clamped = clampToCompanyNameLimit(mixed);
    expect(Array.from(clamped)).toHaveLength(200);
    expect(clamped).toBe("a".repeat(199) + "\u{1F600}");
  });
});
