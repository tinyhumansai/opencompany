import { describe, expect, it } from "vitest";

import { clampText, formatBytes, present, TEXT_LIMIT } from "@/views/observatory/clamp";

describe("clampText", () => {
  it("leaves a short body alone", () => {
    expect(clampText("hello")).toEqual({ shown: "hello", hidden: 0, truncated: false });
  });

  it("keeps a body exactly at the limit whole", () => {
    const exact = "x".repeat(TEXT_LIMIT);
    expect(clampText(exact).truncated).toBe(false);
  });

  it("clamps one character over", () => {
    const over = "x".repeat(TEXT_LIMIT + 1);
    const r = clampText(over);
    expect(r.truncated).toBe(true);
    expect(r.hidden).toBe(1);
    expect([...r.shown]).toHaveLength(TEXT_LIMIT);
  });

  it("counts characters, not code units", () => {
    // Ten astral-plane characters are twenty code units. Slicing by code unit
    // would cut a surrogate pair in half and render a replacement glyph.
    const body = "🙂".repeat(10);
    const r = clampText(body, 5);
    expect([...r.shown]).toHaveLength(5);
    expect(r.shown.endsWith("🙂")).toBe(true);
    expect(r.hidden).toBe(5);
  });

  it("reports exactly how much is withheld", () => {
    const r = clampText("y".repeat(100), 40);
    expect(r.hidden).toBe(60);
  });
});

describe("formatBytes", () => {
  it("scales its unit", () => {
    expect(formatBytes(840)).toBe("840 B");
    expect(formatBytes(12_595)).toBe("12.3 KB");
    expect(formatBytes(1_258_291)).toBe("1.2 MB");
  });
});

describe("present", () => {
  it("treats blank as absent", () => {
    // A host may send "" where it means nothing; an empty pane under a heading
    // reads as a bug rather than an absence.
    expect(present("")).toBe(false);
    expect(present("   ")).toBe(false);
    expect(present(null)).toBe(false);
    expect(present(undefined)).toBe(false);
    expect(present("x")).toBe(true);
  });
});
