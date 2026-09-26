/**
 * The "Customize" hue ramp (issue #2493 follow-on,
 * `docs/issues/accent-theme-presets/theme-system-decision.md`).
 *
 * # What this generates, and from what
 *
 * The eight curated, non-Graphite presets in `index.css`'s `ACCENT PRESETS`
 * section (violet, indigo, blue, teal, green, amber, rose — violet is also
 * `:root`'s own ramp) are not eight independently-authored palettes: every one
 * of them holds the *same* per-step lightness ratio, the *same* per-step
 * chroma ratio (both relative to that preset's own 500 step), and the *same*
 * per-step hue drift (relative to that preset's own 500 hue) — only the base
 * L, C and H at step 500 differ, tuned per hue to clear
 * `accent-presets-contrast.test.ts`'s five pairs. Dividing every preset's
 * per-step value by its own 500 value confirms the ratios below are identical
 * (to rounding) across all seven hues; they are written out here from
 * `violet`'s block, the one whose L/C/H is also `:root`'s.
 *
 * A custom hue therefore never invents a new shape: it picks a continuous L500
 * and C500 by interpolating between the seven curated anchors (clamped to the
 * range they already span) and reuses these exact fixed ratios/drift for
 * every other step — "substituting only the hue channel" into an
 * already-proven cadence, per the decision doc. Whether the result actually
 * clears contrast and stays in the sRGB gamut is decided separately by
 * `evaluateAccentRamp` (`@/lib/accent-contrast`) — this module only shapes the
 * ramp, it never judges it.
 *
 * # Why some hues are simply unreachable
 *
 * Linear interpolation between two verified-safe anchor points is
 * deliberately *not* the same as solving for the true oklch/sRGB gamut
 * boundary at every hue in between — the decision doc rules out "inventing
 * new chroma math". Around the widest gaps between anchors (amber→green is
 * 90°, teal→blue is 60°) the true gamut boundary dips well below a straight
 * line connecting them, particularly in the yellow/yellow-green band. The
 * live check in `accent-contrast.ts` is what catches that, at the exact hue a
 * user picks, rather than this module trying to predict it.
 */

export const ACCENT_STEPS = [50, 100, 200, 300, 400, 500, 600, 700, 800, 900] as const;

/** One `--brand-*` rung. */
export type AccentStep = (typeof ACCENT_STEPS)[number];

/** One ramp step's OKLCH channels — not yet a CSS value (see `accentRampStepToOklch`). */
export interface AccentRampStep {
  readonly L: number;
  readonly C: number;
  readonly H: number;
}

/** A full ten-rung `--brand-{50..900}` ramp, keyed by step. */
export type AccentRamp = Readonly<Record<AccentStep, AccentRampStep>>;

/**
 * Per-step lightness, relative to the 500 step — e.g. step 50's L is always
 * `1.6856 * L500`. Derived by dividing every violet step's `L` by violet's own
 * `L500` (0.5649); confirmed identical (to four decimal places) against
 * indigo, blue, teal, green, amber and rose in `index.css`.
 */
const STEP_LIGHTNESS_RATIO: Readonly<Record<AccentStep, number>> = {
  50: 1.6856080722251727,
  100: 1.615153124446805,
  200: 1.488759072402195,
  300: 1.325544344131705,
  400: 1.170826694990264,
  500: 1,
  600: 0.9100725792175607,
  700: 0.7882811117011861,
  800: 0.6532129580456718,
  900: 0.5234554788458134,
};

/** Same idea as `STEP_LIGHTNESS_RATIO`, for chroma relative to the 500 step's C. */
const STEP_CHROMA_RATIO: Readonly<Record<AccentStep, number>> = {
  50: 0.10286225402504473,
  100: 0.1909660107334526,
  200: 0.3550983899821109,
  300: 0.5755813953488372,
  400: 0.7911449016100179,
  500: 1,
  600: 0.9338103756708409,
  700: 0.817531305903399,
  800: 0.669051878354204,
  900: 0.5107334525939177,
};

/**
 * Per-step hue drift *in degrees*, relative to the 500 step's H — e.g. step
 * 50's hue is always `H500 + 10.20`. Derived the same way as the ratios
 * above: violet's per-step H minus violet's H500, confirmed identical (to two
 * decimal places — index.css's own precision) across every other curated
 * preset.
 */
const STEP_HUE_DRIFT: Readonly<Record<AccentStep, number>> = {
  50: 10.2,
  100: 9.97,
  200: 9.23,
  300: 7.24,
  400: 4.22,
  500: 0,
  600: -0.54,
  700: -0.63,
  800: -0.19,
  900: 1.0,
};

interface HueAnchor {
  /** OKLCH hue, degrees. */
  readonly hue: number;
  /** That preset's own `--brand-500` lightness. */
  readonly l500: number;
  /** That preset's own `--brand-500` chroma. */
  readonly c500: number;
}

/**
 * The seven curated non-Graphite presets' own `--brand-500` values, sorted by
 * hue ascending. Graphite is excluded on purpose — it is chroma-zero at every
 * step by design (`index.css`'s comment on that block), not a point on this
 * hue curve. Each entry cites the `index.css` block it is read from; these
 * are read values, not tuned ones.
 */
const HUE_ANCHORS: readonly HueAnchor[] = [
  { hue: 15.0, l500: 0.57, c500: 0.185 }, // rose
  { hue: 55.0, l500: 0.56, c500: 0.13 }, // amber
  { hue: 145.0, l500: 0.54, c500: 0.12 }, // green
  { hue: 195.0, l500: 0.54, c500: 0.085 }, // teal
  { hue: 255.0, l500: 0.55, c500: 0.17 }, // blue
  { hue: 268.0, l500: 0.56, c500: 0.23 }, // indigo
  { hue: 285.51, l500: 0.5649, c500: 0.2236 }, // violet (== :root's own ramp)
];

/** Wraps any real number into `[0, 360)`. */
export function normalizeHue(hue: number): number {
  const wrapped = hue % 360;
  return wrapped < 0 ? wrapped + 360 : wrapped;
}

/**
 * The base (500-step) lightness and chroma for an arbitrary hue: piecewise
 * linear interpolation around the hue circle between the two `HUE_ANCHORS`
 * that bracket it. Because every anchor's own l500/c500 already clears the
 * contrast and gamut bars at its own hue, and interpolation stays within the
 * pair of values it walks between, this can only ever propose a value inside
 * the range the curated set already spans — never outside it, and never a
 * value nobody has seen. Whether a given interpolated point still clears the
 * bars at *its* hue is exactly what `evaluateAccentRamp` decides.
 */
export function interpolateAccentBase(hue: number): { l500: number; c500: number } {
  const h = normalizeHue(hue);
  for (let i = 0; i < HUE_ANCHORS.length; i += 1) {
    const a = HUE_ANCHORS[i];
    const b = HUE_ANCHORS[(i + 1) % HUE_ANCHORS.length];
    let bHue = b.hue;
    if (bHue <= a.hue) bHue += 360; // the wrap-around segment, violet -> rose
    let hh = h;
    if (hh < a.hue) hh += 360;
    if (hh >= a.hue && hh <= bHue) {
      const t = bHue === a.hue ? 0 : (hh - a.hue) / (bHue - a.hue);
      return {
        l500: a.l500 + (b.l500 - a.l500) * t,
        c500: a.c500 + (b.c500 - a.c500) * t,
      };
    }
  }
  // Unreachable: HUE_ANCHORS' wrap-around segment covers the full circle.
  throw new Error(`accent-ramp: hue ${hue} matched no anchor interval`);
}

/**
 * The full ten-step `--brand-*` ramp for a custom hue: the interpolated base
 * at step 500, carried to every other step by the fixed ratio/drift tables
 * above. Never judges gamut or contrast — call `evaluateAccentRamp` on the
 * result before applying it.
 */
export function generateCustomRamp(hue: number): AccentRamp {
  const { l500, c500 } = interpolateAccentBase(hue);
  const ramp = {} as Record<AccentStep, AccentRampStep>;
  for (const step of ACCENT_STEPS) {
    ramp[step] = {
      L: l500 * STEP_LIGHTNESS_RATIO[step],
      C: Math.max(0, c500 * STEP_CHROMA_RATIO[step]),
      H: normalizeHue(hue + STEP_HUE_DRIFT[step]),
    };
  }
  return ramp;
}

/** Renders one ramp step as the exact CSS `oklch()` value `--brand-*` expects. */
export function accentRampStepToOklch(step: AccentRampStep): string {
  return `oklch(${step.L} ${step.C} ${step.H})`;
}

/**
 * Each curated preset's own hue, for the canvas/chrome tint below — not for
 * `--brand-*` itself, which curated presets already carry as an authored
 * `index.css` block and never touch this module for
 * (`theme-system-decision-addendum.md`). `"default"` maps to violet's own
 * hue, matching `DEFAULT_CUSTOM_HUE` in `accent-presets.ts` and reproducing
 * today's authored `--canvas-tint-*`/`--chrome-tint-*` values exactly.
 * `"graphite"` has none — see `computeTintedNeutrals`.
 */
export const CURATED_PRESET_HUE: Readonly<Record<string, number>> = {
  default: 285.51,
  violet: 285.51,
  indigo: 268.0,
  blue: 255.0,
  teal: 195.0,
  green: 145.0,
  amber: 55.0,
  rose: 15.0,
};

/** A tinted canvas/chrome/accent pair, one value per theme mode. */
export interface TintedNeutrals {
  readonly canvasLight: string;
  readonly canvasDark: string;
  readonly chromeLight: string;
  readonly chromeDark: string;
  readonly accentLight: string;
  readonly accentDark: string;
}

/**
 * The anchor lightness/chroma pairs `--canvas-tint-*`/`--chrome-tint-*`/
 * `--accent-tint-*` hold in `index.css` today — read from that file, not
 * invented. `accentLight`/`accentDark` are `--surface-light-active`'s and
 * `--surface-dark-active`'s own values (`oklch(0.942 0.0256 293.15)`,
 * `oklch(0.2396 0.019 284.87)`) — `--accent` was always meant to be
 * brand-tinted but was wired straight to those fixed primitives instead of
 * the ramp, so it silently never followed a preset
 * (`theme-system-decision-addendum-2.md`, a bug fix, not new scope). Kept
 * apart from `ACCENT_STEPS`'s ratio tables above: these are independent
 * points, not a ten-step ramp, and carry no per-step drift.
 */
const NEUTRAL_ANCHORS = {
  canvasLight: { L: 0.9776, C: 0.0066 },
  canvasDark: { L: 0.1395, C: 0.0048 },
  chromeLight: { L: 0.9427, C: 0.012 },
  chromeDark: { L: 0.1865, C: 0.0044 },
  accentLight: { L: 0.942, C: 0.0256 },
  accentDark: { L: 0.2396, C: 0.019 },
} as const;

/**
 * The canvas/chrome/accent tint for a given hue — `null` for Graphite, whose
 * whole point is chroma-zero at every step (`accent-presets.ts`'s comment on
 * `ACCENT_PRESETS`); tinting with *some* hue at zero chroma would be a no-op
 * anyway, but passing `null` here makes that a chroma-zero (fully achromatic)
 * render rather than an arbitrary, meaningless hue number sitting unused in
 * the value. Every other hue substitutes into the anchors above, lightness
 * and chroma untouched — selecting the shipped default
 * (`hue = CURATED_PRESET_HUE.default`) reproduces the exact current
 * `index.css` values.
 */
export function computeTintedNeutrals(hue: number | null): TintedNeutrals {
  const h = hue === null ? 0 : normalizeHue(hue);
  const c = (chroma: number) => (hue === null ? 0 : chroma);
  return {
    canvasLight: `oklch(${NEUTRAL_ANCHORS.canvasLight.L} ${c(NEUTRAL_ANCHORS.canvasLight.C)} ${h})`,
    canvasDark: `oklch(${NEUTRAL_ANCHORS.canvasDark.L} ${c(NEUTRAL_ANCHORS.canvasDark.C)} ${h})`,
    chromeLight: `oklch(${NEUTRAL_ANCHORS.chromeLight.L} ${c(NEUTRAL_ANCHORS.chromeLight.C)} ${h})`,
    chromeDark: `oklch(${NEUTRAL_ANCHORS.chromeDark.L} ${c(NEUTRAL_ANCHORS.chromeDark.C)} ${h})`,
    accentLight: `oklch(${NEUTRAL_ANCHORS.accentLight.L} ${c(NEUTRAL_ANCHORS.accentLight.C)} ${h})`,
    accentDark: `oklch(${NEUTRAL_ANCHORS.accentDark.L} ${c(NEUTRAL_ANCHORS.accentDark.C)} ${h})`,
  };
}
