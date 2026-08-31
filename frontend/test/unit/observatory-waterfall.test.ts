import { describe, expect, it } from "vitest";

import {
  concurrencyProfile,
  formatOffset,
  lanesFrom,
  peakConcurrency,
  place,
  ticks,
  windowFor,
  type Span,
} from "@/views/observatory/waterfall";

const NOW = 10_000;

function span(over: Partial<Span> & Pick<Span, "id" | "lane" | "startMs">): Span {
  return {
    endMs: null,
    state: "done",
    label: over.id,
    ...over,
  };
}

describe("windowFor", () => {
  it("is empty for no spans rather than infinite", () => {
    // `Math.min()` of nothing is Infinity; a window of Infinity would make every
    // offset NaN and blank the lane.
    const w = windowFor([], NOW);
    expect(Number.isFinite(w.startMs)).toBe(true);
    expect(Number.isFinite(w.endMs)).toBe(true);
  });

  it("extends to now for an open span, so a live run grows", () => {
    const w = windowFor([span({ id: "a", lane: "x", startMs: 0 })], NOW);
    expect(w.endMs).toBe(NOW);
  });

  it("covers the earliest start and latest end", () => {
    const w = windowFor(
      [
        span({ id: "a", lane: "x", startMs: 500, endMs: 900 }),
        span({ id: "b", lane: "y", startMs: 100, endMs: 700 }),
      ],
      NOW,
    );
    expect(w).toEqual({ startMs: 100, endMs: 900 });
  });
});

describe("place", () => {
  const window = { startMs: 0, endMs: 1000 };

  it("maps a span onto its fraction of the window", () => {
    const p = place(span({ id: "a", lane: "x", startMs: 250, endMs: 750 }), window, NOW);
    expect(p.offset).toBeCloseTo(0.25);
    expect(p.width).toBeCloseTo(0.5);
  });

  it("clamps an open span to now", () => {
    const p = place(span({ id: "a", lane: "x", startMs: 0 }), { startMs: 0, endMs: 400 }, 400);
    expect(p.resolvedEndMs).toBe(400);
  });

  it("gives a zero-length span a visible hairline", () => {
    // A tool call that returned in under a millisecond still happened.
    const p = place(span({ id: "a", lane: "x", startMs: 10, endMs: 10 }), window, NOW);
    expect(p.width).toBeGreaterThan(0);
  });

  it("clamps a skewed end rather than drawing backwards", () => {
    // The host stamps the start and the browser supplies `now`, so the two
    // clocks really can disagree. A negative width would render as a bar
    // growing leftwards across the lane.
    const p = place(span({ id: "a", lane: "x", startMs: 800, endMs: 200 }), window, NOW);
    expect(p.resolvedEndMs).toBe(800);
    expect(p.width).toBeGreaterThan(0);
    expect(p.offset).toBeGreaterThanOrEqual(0);
  });

  it("never places outside the window", () => {
    const p = place(span({ id: "a", lane: "x", startMs: -500, endMs: 5000 }), window, NOW);
    expect(p.offset).toBeGreaterThanOrEqual(0);
    expect(p.offset + p.width).toBeLessThanOrEqual(1.0001);
  });
});

describe("lanesFrom", () => {
  it("gives disjoint spans one row", () => {
    const lanes = lanesFrom(
      [
        span({ id: "a", lane: "solver", startMs: 0, endMs: 100 }),
        span({ id: "b", lane: "solver", startMs: 200, endMs: 300 }),
      ],
      NOW,
    );
    expect(lanes).toHaveLength(1);
    expect(lanes[0].rows).toHaveLength(1);
    expect(lanes[0].rows[0]).toHaveLength(2);
  });

  it("stacks overlapping spans so neither is drawn over the other", () => {
    const lanes = lanesFrom(
      [
        span({ id: "a", lane: "solver", startMs: 0, endMs: 500 }),
        span({ id: "b", lane: "solver", startMs: 100, endMs: 600 }),
      ],
      NOW,
    );
    expect(lanes[0].rows).toHaveLength(2);
  });

  it("keeps one lane per agent", () => {
    const lanes = lanesFrom(
      [
        span({ id: "a", lane: "theorist", startMs: 0, endMs: 100 }),
        span({ id: "b", lane: "programmer", startMs: 0, endMs: 100 }),
      ],
      NOW,
    );
    expect(lanes.map((l) => l.agentId).sort()).toEqual(["programmer", "theorist"]);
  });

  it("orders lanes by who started first", () => {
    const lanes = lanesFrom(
      [
        span({ id: "b", lane: "programmer", startMs: 500, endMs: 900 }),
        span({ id: "a", lane: "theorist", startMs: 100, endMs: 400 }),
      ],
      NOW,
    );
    expect(lanes.map((l) => l.agentId)).toEqual(["theorist", "programmer"]);
  });

  it("returns nothing for no spans", () => {
    expect(lanesFrom([], NOW)).toEqual([]);
  });
});

describe("concurrencyProfile", () => {
  it("counts overlap", () => {
    const spans = [
      span({ id: "a", lane: "x", startMs: 0, endMs: 100 }),
      span({ id: "b", lane: "y", startMs: 50, endMs: 150 }),
    ];
    expect(peakConcurrency(spans, NOW)).toBe(2);
  });

  it("reads a hand-off as one, never two", () => {
    // b starts exactly as a ends. Ordering ends before starts at the same
    // instant is what keeps a relay race from looking parallel.
    const spans = [
      span({ id: "a", lane: "x", startMs: 0, endMs: 100 }),
      span({ id: "b", lane: "y", startMs: 100, endMs: 200 }),
    ];
    expect(peakConcurrency(spans, NOW)).toBe(1);
  });

  it("catches a burst that a sampled grid would miss", () => {
    // Two long spans and one 1ms overlap between them.
    const spans = [
      span({ id: "a", lane: "x", startMs: 0, endMs: 5000 }),
      span({ id: "b", lane: "y", startMs: 4999, endMs: 9000 }),
    ];
    expect(peakConcurrency(spans, NOW)).toBe(2);
  });

  it("returns a step series that ends at zero", () => {
    const profile = concurrencyProfile(
      [span({ id: "a", lane: "x", startMs: 0, endMs: 100 })],
      NOW,
    );
    expect(profile[0]).toEqual({ atMs: 0, n: 1 });
    expect(profile[profile.length - 1].n).toBe(0);
  });

  it("is empty for no spans", () => {
    expect(concurrencyProfile([], NOW)).toEqual([]);
    expect(peakConcurrency([], NOW)).toBe(0);
  });
});

describe("formatOffset", () => {
  it("reads as elapsed time at every scale", () => {
    expect(formatOffset(0)).toBe("0ms");
    expect(formatOffset(450)).toBe("450ms");
    expect(formatOffset(1200)).toBe("1.2s");
    expect(formatOffset(45_000)).toBe("45s");
    expect(formatOffset(120_000)).toBe("2m");
    expect(formatOffset(130_000)).toBe("2m 10s");
  });

  it("never reports a negative", () => {
    expect(formatOffset(-5)).toBe("0ms");
  });
});

describe("ticks", () => {
  it("spans the window inclusively", () => {
    const t = ticks({ startMs: 0, endMs: 4000 }, 4);
    expect(t).toHaveLength(5);
    expect(t[0].offset).toBe(0);
    expect(t[4].offset).toBe(1);
    expect(t[4].label).toBe("4s");
  });

  it("survives a zero-length window", () => {
    // A run with one instantaneous node: every tick collapses onto 0 rather
    // than dividing by zero.
    const t = ticks({ startMs: 5, endMs: 5 }, 4);
    expect(t.every((tick) => Number.isFinite(tick.atMs))).toBe(true);
  });
});
