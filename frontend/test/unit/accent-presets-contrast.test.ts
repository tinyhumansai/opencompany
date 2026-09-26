import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import { ACCENT_CONTRAST_BAR, evaluateAccentRamp, evaluateTintedNeutrals } from "@/lib/accent-contrast";
import { ACCENT_STEPS, computeTintedNeutrals, generateCustomRamp, type AccentRamp } from "@/lib/accent-ramp";

/**
 * The contrast gate (issue #2493, test-plan U4). Every curated accent preset —
 * and the default ramp in `:root` — must clear the same five pairs
 * `docs/design-system/color.md`'s "Accent presets" table documents, at 4.5:1.
 * This is the gate, not a reviewer's eye: a preset that cannot pass belongs in
 * `open-questions.md`, not in `index.css`.
 *
 * The colour math and the five pairs live in one place now,
 * `@/lib/accent-contrast`'s `evaluateAccentRamp` — imported here rather than
 * reimplemented, per `docs/issues/accent-theme-presets/theme-system-decision.md`'s
 * requirement that the test and the live "Customize" picker share a single
 * implementation, so a hue the picker refuses is refused for the same reason
 * this test would have failed it.
 *
 * Deliberately NOT asserted: `--brand-500` on `--accent` or `--chrome`. The
 * default ramp already misses both (4.20:1, 4.22:1) — a pre-existing gap this
 * test does not widen. See `roadblocks.md` R12 and
 * `docs/design-system/color.md`'s "Accent presets" section.
 */

const indexCss = readFileSync(
  resolve(dirname(fileURLToPath(import.meta.url)), "../../src/index.css"),
  "utf8",
);

/** `--brand-<step>: oklch(L C H); …` -> { L, C, H }, for the given block body. */
function ramp(body: string): AccentRamp {
  const out: Record<number, { L: number; C: number; H: number }> = {};
  for (const step of ACCENT_STEPS) {
    const m = new RegExp(`--brand-${step}:\\s*oklch\\(([\\d.]+)\\s+([\\d.]+)\\s+([\\d.]+)\\)`).exec(body);
    if (!m) throw new Error(`--brand-${step} not found in block`);
    out[step] = { L: Number(m[1]), C: Number(m[2]), H: Number(m[3]) };
  }
  return out as AccentRamp;
}

function blockBody(marker: string): string {
  const open = indexCss.indexOf(marker);
  if (open < 0) throw new Error(`marker not found: ${marker}`);
  const braceOpen = indexCss.indexOf("{", open);
  let depth = 0;
  for (let i = braceOpen; i < indexCss.length; i += 1) {
    if (indexCss[i] === "{") depth += 1;
    else if (indexCss[i] === "}") {
      depth -= 1;
      if (depth === 0) return indexCss.slice(braceOpen, i);
    }
  }
  throw new Error(`unterminated block: ${marker}`);
}

const rampsToCheck: Array<{ name: string; body: string }> = [
  { name: "default (:root)", body: blockBody(":root {") },
  ...["violet", "indigo", "blue", "teal", "green", "amber", "rose", "graphite", "onyx"].map((id) => ({
    name: id,
    body: blockBody(`[data-accent-preset="${id}"] {`),
  })),
];

describe.each(rampsToCheck)("accent preset contrast: $name", ({ body }) => {
  const evaluation = evaluateAccentRamp(ramp(body));

  it("keeps every ramp step in sRGB gamut", () => {
    expect(evaluation.inGamut).toBe(true);
  });

  it("clears 4.5:1 white text on 500", () => {
    expect(evaluation.ratios.whiteOn500).toBeGreaterThanOrEqual(ACCENT_CONTRAST_BAR);
  });

  it("clears 4.5:1 for 500 on the light canvas", () => {
    expect(evaluation.ratios.c500OnLightCanvas).toBeGreaterThanOrEqual(ACCENT_CONTRAST_BAR);
  });

  it("clears 4.5:1 for 400 on the dark canvas", () => {
    expect(evaluation.ratios.c400OnDarkCanvas).toBeGreaterThanOrEqual(ACCENT_CONTRAST_BAR);
  });

  it("clears 4.5:1 for 700 on 100 (the active nav row)", () => {
    expect(evaluation.ratios.c700On100).toBeGreaterThanOrEqual(ACCENT_CONTRAST_BAR);
  });

  it("clears 4.5:1 for 300 on the dark active rung", () => {
    expect(evaluation.ratios.c300OnDarkActiveRung).toBeGreaterThanOrEqual(ACCENT_CONTRAST_BAR);
  });
});

/**
 * The "Customize" hue ramp (`@/lib/accent-ramp`'s `generateCustomRamp`), fuzzed
 * across the hue circle rather than checked at the 9 fixed presets above —
 * `theme-system-decision.md`'s explicit ask, since a custom hue is exactly
 * the input this gate did not used to see.
 */
describe("custom hue ramp", () => {
  const CURATED_ANCHORS: Array<{ name: string; hue: number }> = [
    { name: "rose", hue: 15.0 },
    { name: "amber", hue: 55.0 },
    { name: "green", hue: 145.0 },
    { name: "teal", hue: 195.0 },
    { name: "blue", hue: 255.0 },
    { name: "indigo", hue: 268.0 },
    { name: "violet", hue: 285.51 },
  ];

  describe.each(CURATED_ANCHORS)("reproduces the curated $name ramp at its own hue", ({ name, hue }) => {
    const curated = ramp(blockBody(`[data-accent-preset="${name}"] {`));
    const generated = generateCustomRamp(hue);

    it("matches every step's L, C and H within rounding tolerance", () => {
      for (const step of ACCENT_STEPS) {
        expect(generated[step].L, `step ${step} L`).toBeCloseTo(curated[step].L, 3);
        expect(generated[step].C, `step ${step} C`).toBeCloseTo(curated[step].C, 3);
        expect(generated[step].H, `step ${step} H`).toBeCloseTo(curated[step].H, 1);
      }
    });

    it("still clears every gamut and contrast bar the curated preset does", () => {
      expect(evaluateAccentRamp(generated).ok).toBe(true);
    });
  });

  // `violet` is also `:root`'s own ramp, so its exact hue is the one every
  // fresh "Custom" tile opens to (`DEFAULT_CUSTOM_HUE`, `accent-presets.ts`) —
  // pinned here too, redundantly with the anchor check above, because a
  // regression here is exactly what would put a visible flash into the very
  // first time an operator opens the slider.
  it("previews as violet at hue 285.51, violet's own hue", () => {
    const violet = ramp(blockBody('[data-accent-preset="violet"] {'));
    const generated = generateCustomRamp(285.51);
    expect(generated[500].L).toBeCloseTo(violet[500].L, 3);
    expect(generated[500].C).toBeCloseTo(violet[500].C, 3);
  });

  it("refuses a hue from the yellow/yellow-green band (90°) — the true sRGB gamut boundary there dips well below what a straight interpolation between amber (55°) and green (145°) assumes", () => {
    const evaluation = evaluateAccentRamp(generateCustomRamp(90));
    expect(evaluation.ok).toBe(false);
  });

  it("accepts a hue close to a curated anchor (rose-adjacent, 10°)", () => {
    const evaluation = evaluateAccentRamp(generateCustomRamp(10));
    expect(evaluation.ok).toBe(true);
  });

  it("accepts hues right at the 0°/360° wrap, with no discontinuity", () => {
    const at0 = evaluateAccentRamp(generateCustomRamp(0));
    const at360 = evaluateAccentRamp(generateCustomRamp(360));
    const at359 = evaluateAccentRamp(generateCustomRamp(359.9));
    expect(at0.ok).toBe(true);
    expect(at360.ok).toBe(true);
    // 0 and 360 are the same hue; the ramp must be identical, not just both "ok".
    const ramp0 = generateCustomRamp(0);
    const ramp360 = generateCustomRamp(360);
    for (const step of ACCENT_STEPS) {
      expect(ramp360[step].L).toBeCloseTo(ramp0[step].L, 9);
      expect(ramp360[step].C).toBeCloseTo(ramp0[step].C, 9);
      expect(ramp360[step].H).toBeCloseTo(ramp0[step].H, 9);
    }
    expect(at359.ok).toBe(true);
  });

  it("sweeps the full hue circle without throwing, and is neither vacuously all-pass nor all-fail", () => {
    let passCount = 0;
    let failCount = 0;
    for (let hue = 0; hue < 360; hue += 1) {
      const evaluation = evaluateAccentRamp(generateCustomRamp(hue));
      if (evaluation.ok) passCount += 1;
      else failCount += 1;
    }
    expect(passCount + failCount).toBe(360);
    // A constrained customize control is expected to refuse a real portion of
    // the wheel (the decision doc's whole point — interpolation is not a
    // gamut solver) but must not refuse everything, and the violet/rose/blue
    // neighbourhood the brand already lives in must stay usable.
    expect(passCount).toBeGreaterThan(100);
    expect(failCount).toBeGreaterThan(50);
  });
});

/**
 * The canvas/chrome auto-tint (`theme-system-decision-addendum.md`): every
 * accent application — curated, default, custom, Graphite — also sets
 * `--canvas-tint-*`/`--chrome-tint-*`, and this is the gate on that, mirroring
 * `describe("custom hue ramp", …)` above: read the real `index.css` anchor
 * values rather than assume them, confirm the default reproduces today's
 * exact pixels, and sweep contrast across the hue circle.
 */
describe("canvas/chrome auto-tint", () => {
  function readVarOklch(name: string): { L: number; C: number; H: number } {
    const m = new RegExp(`${name}:\\s*oklch\\(([\\d.]+)\\s+([\\d.]+)\\s+([\\d.]+)\\)`).exec(indexCss);
    if (!m) throw new Error(`${name} not found in index.css`);
    return { L: Number(m[1]), C: Number(m[2]), H: Number(m[3]) };
  }

  // `index.css`'s own six anchor hues (286.28°, 262.8°, 286.17°, 264.46°,
  // 293.15°, 284.87°) are close to, but NOT identical to, violet's brand hue
  // (285.51° — `CURATED_PRESET_HUE.default`) — dark mode's in particular
  // differs by over 20°. Substituting the accent hue for "Default" would
  // therefore be a small but real pixel change, not the zero-pixel-change
  // `theme-system-decision-addendum.md`/`-addendum-2.md` promises.
  // `applyAccentPreset` resolves this the same way it already does for
  // `--brand-*`: "Default" clears the inline override entirely
  // (`clearTintedNeutrals`) rather than computing an approximation, so
  // `:root`'s own authored values — asserted to still be exactly what they
  // were before this feature, below — show through unmodified. That DOM
  // behaviour is exercised by `test/e2e/accent-preset.spec.ts` (a real
  // `getComputedStyle`, not a node approximation of one); this test only pins
  // the anchor *source values* Default falls back to, so a future edit to
  // one of them is caught here.
  it("keeps index.css's own anchor values as the fallback \"default\" reproduces (via clearing, not approximation)", () => {
    expect(readVarOklch("--canvas-tint-light")).toEqual({ L: 0.9776, C: 0.0066, H: 286.28 });
    expect(readVarOklch("--canvas-tint-dark")).toEqual({ L: 0.1395, C: 0.0048, H: 262.8 });
    expect(readVarOklch("--chrome-tint-light")).toEqual({ L: 0.9427, C: 0.012, H: 286.17 });
    expect(readVarOklch("--chrome-tint-dark")).toEqual({ L: 0.1865, C: 0.0044, H: 264.46 });
    expect(readVarOklch("--accent-tint-light")).toEqual({ L: 0.942, C: 0.0256, H: 293.15 });
    expect(readVarOklch("--accent-tint-dark")).toEqual({ L: 0.2396, C: 0.019, H: 284.87 });
  });

  it("computeTintedNeutrals at violet's hue is a close but not exact approximation of the anchors (documents why Default clears instead)", () => {
    const tint = computeTintedNeutrals(285.51);
    const parse = (v: string) => {
      const m = /oklch\(([-\d.]+) ([-\d.]+) ([-\d.]+)\)/.exec(v)!;
      return { H: Number(m[3]) };
    };
    // Close (light) ...
    expect(Math.abs(parse(tint.canvasLight).H - 286.28)).toBeLessThan(1);
    // ... but not exact, and dark drifts much further — this is exactly the
    // gap that would have made Default a pixel change, not a reproduction.
    expect(Math.abs(parse(tint.canvasDark).H - 262.8)).toBeGreaterThan(15);
  });

  it("goes fully achromatic for Graphite (hue: null), never an arbitrary hue at zero chroma", () => {
    const tint = computeTintedNeutrals(null);
    for (const value of [
      tint.canvasLight,
      tint.canvasDark,
      tint.chromeLight,
      tint.chromeDark,
      tint.accentLight,
      tint.accentDark,
    ]) {
      expect(value).toContain(" 0 0)"); // chroma 0, hue 0
    }
    expect(evaluateTintedNeutrals(null).contrastOk).toBe(true);
  });

  it("clears every documented pair at the default hue", () => {
    expect(evaluateTintedNeutrals(285.51).contrastOk).toBe(true);
  });

  it("sweeps the full hue circle and always clears the documented pairs — hue never affects contrast at this chroma", () => {
    for (let hue = 0; hue < 360; hue += 5) {
      const evaluation = evaluateTintedNeutrals(hue);
      expect(evaluation.contrastOk, `hue ${hue}: ${JSON.stringify(evaluation.ratios)}`).toBe(true);
    }
  });
});
