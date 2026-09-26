/**
 * The operator's accent preset — a curated hue for `--brand-*`, independent of
 * light/dark mode (issue #2493).
 *
 * # Why "preset" and never "accent" alone
 *
 * `--accent` already names something else in this codebase (the neutral
 * hover/rest tint under menu rows, `index.css:236-242`), and `--sidebar-accent`
 * and `.oc-kg`'s own `--accent` name two more. So everywhere in code this
 * feature is the **accent preset**: this module, the `data-accent-preset`
 * attribute, the `oc.appearance.accentPreset` storage key. The user-facing
 * label can still say "Accent" — `AppearanceView.tsx` does — because an
 * operator never sees the other three.
 *
 * # No colour lives here
 *
 * Every value is in `frontend/src/index.css`'s `ACCENT PRESETS` section, one
 * `[data-accent-preset="<id>"] { --brand-50: …; … }` block per preset. This
 * file carries only ids and labels, on purpose: `scripts/ci/assert-design-
 * tokens.sh` bans raw hex and Tailwind palette classes from `.ts`/`.tsx`, and
 * keeping colour out entirely means this file needs no exemption to pass it —
 * unlike `frontend/src/lib/connections.ts`, whose third-party brand hexes have
 * nowhere else to live. See `docs/issues/accent-theme-presets/architecture.md`
 * §5 and §11 for the reasoning in full, and §9 for why every preset here is
 * measured against the same contrast bars the default ramp meets
 * (`frontend/test/unit/accent-presets-contrast.test.ts`).
 */

import { useSyncExternalStore } from "react";

import {
  accentRampStepToOklch,
  ACCENT_STEPS,
  computeTintedNeutrals,
  CURATED_PRESET_HUE,
  generateCustomRamp,
  normalizeHue,
} from "@/lib/accent-ramp";
import { evaluateAccentRamp } from "@/lib/accent-contrast";

/** One curated accent preset. Colours live in `index.css`, never here. */
export interface AccentPreset {
  /**
   * Stable id: the `data-accent-preset` attribute value and the exact string
   * persisted to `localStorage`. Ids are never renamed — like the `TEAM_TONES`
   * slot names (`docs/design-system/color.md`, "Legacy slot names"), an id
   * already sitting in someone's browser has to keep resolving. Retiring a
   * preset means removing its entry and its CSS block; a stored id that no
   * longer matches any entry falls back to the default (`readStoredAccentPreset`).
   */
  readonly id: string;
  /** What the picker shows. */
  readonly label: string;
}

/**
 * The id that means "no override" — `:root`'s own ramp, whatever it declares.
 * Selecting it **removes** `data-accent-preset` rather than setting it to this
 * string; it has no CSS block of its own. See `architecture.md` §2.
 */
export const DEFAULT_ACCENT_PRESET = "default";

/**
 * The curated set, target 6–8 per `open-questions.md` Q5. "Violet" is a
 * deliberate twin of whatever `:root` currently declares (`architecture.md`
 * §2's Q1 follow-up: naming today's default now keeps it choosable later, if
 * a future brand decision ever moves the default away from violet). Six more
 * are spread around the hue circle, and "Graphite" and "Onyx" are the
 * deliberate exceptions: chroma zero at every step, for the premium
 * monochrome look requested alongside this feature from the start — a hue
 * choice would defeat that, so these are the two presets with no hue at all.
 * "Onyx" anchors much darker than "Graphite" (near-black rather than
 * mid-gray) and, unlike every other preset, does not hold the shared
 * lightness cadence either — see its comment in `index.css` for why a
 * near-black 500 makes that impossible to combine with the contrast bars
 * below. Every preset here is individually verified against every pair in
 * `docs/design-system/color.md`'s
 * contrast table — see the `ACCENT PRESETS` section of `index.css` for the
 * measured ratios in each preset's leading comment, and
 * `accent-presets-contrast.test.ts` for the test that keeps them honest.
 */
export const ACCENT_PRESETS: readonly AccentPreset[] = [
  { id: DEFAULT_ACCENT_PRESET, label: "Default" },
  { id: "violet", label: "Violet" },
  { id: "indigo", label: "Indigo" },
  { id: "blue", label: "Blue" },
  { id: "teal", label: "Teal" },
  { id: "green", label: "Green" },
  { id: "amber", label: "Amber" },
  { id: "rose", label: "Rose" },
  { id: "graphite", label: "Graphite" },
  { id: "onyx", label: "Onyx" },
];

/** `oc.<area>.<name>` — the convention `oc.connections.v1`, `oc.presence.override`
 *  and `oc.workspace.listWidth` already use. Deliberately not `next-themes`'
 *  own `"theme"` key: the two axes (mode, accent) are independent and must
 *  never collide. */
export const ACCENT_PRESET_STORAGE_KEY = "oc.appearance.accentPreset";

/**
 * The id that means "generate the ramp from a single operator-chosen hue"
 * (issue #2493 follow-on, `theme-system-decision.md`'s constrained
 * "Customize" tab). Deliberately never added to `ACCENT_PRESETS`:
 * `accent-presets-registry.test.ts` requires every non-default entry there to
 * have exactly one `[data-accent-preset="…"]` block in `index.css`, and this
 * id has none on purpose — its ramp is generated client-side
 * (`@/lib/accent-ramp`) and applied as inline custom properties, never
 * authored as CSS. `isKnownPreset` special-cases it instead.
 */
export const CUSTOM_ACCENT_PRESET_ID = "custom";

/** `oc.appearance.customHue` — the second key `theme-system-decision.md` calls
 *  for, read only when `oc.appearance.accentPreset` is `"custom"`. */
export const CUSTOM_HUE_STORAGE_KEY = "oc.appearance.customHue";

/** The hue the "Custom" tile opens to before an operator has ever chosen one —
 *  violet's own hue (index.css `:root`'s `--brand-500`), so the very first
 *  Custom selection previews as indistinguishable from the shipped default. */
export const DEFAULT_CUSTOM_HUE = 285.51;

function isKnownPreset(id: string): boolean {
  return id === CUSTOM_ACCENT_PRESET_ID || ACCENT_PRESETS.some((p) => p.id === id);
}

/** The stored custom hue, or `DEFAULT_CUSTOM_HUE` when nothing valid is
 *  stored — same never-throws contract as `readStoredAccentPreset`. */
export function readStoredCustomHue(): number {
  try {
    const stored = window.localStorage.getItem(CUSTOM_HUE_STORAGE_KEY);
    if (stored === null) return DEFAULT_CUSTOM_HUE;
    const parsed = Number.parseFloat(stored);
    return Number.isFinite(parsed) ? normalizeHue(parsed) : DEFAULT_CUSTOM_HUE;
  } catch {
    return DEFAULT_CUSTOM_HUE;
  }
}

/** The ten `--brand-*` custom-property names, computed once. */
const CUSTOM_BRAND_PROPERTIES = ACCENT_STEPS.map((step) => `--brand-${step}`);

/** Removes every inline `--brand-*` override this module may have set. A
 *  curated preset's CSS block cannot out-rank an inline style
 *  (`roadblocks.md` R2's specificity trap is about selectors, not inline —
 *  inline always wins), so switching away from `"custom"` without clearing
 *  these first would pin the previous custom ramp underneath whatever
 *  preset id gets set next. */
function clearInlineAccentRamp(root: HTMLElement): void {
  for (const property of CUSTOM_BRAND_PROPERTIES) root.style.removeProperty(property);
}

/** The canvas/chrome/accent custom-property names `applyTintedNeutrals` sets. */
const TINTED_NEUTRAL_PROPERTIES = [
  "--canvas-tint-light",
  "--canvas-tint-dark",
  "--chrome-tint-light",
  "--chrome-tint-dark",
  "--accent-tint-light",
  "--accent-tint-dark",
] as const;

/** Removes every inline canvas/chrome/accent override, so `index.css`'s own
 *  `:root` values — today's exact, authored pixels — show through
 *  unapproximated. Used for `"default"` only: `CURATED_PRESET_HUE.default`
 *  (violet's hue, 285.51°) is close to but not identical to these anchors'
 *  own authored hues (`--surface-light-bg`'s is 286.28°, dark's are further
 *  still at 262.8°/264.46°, and `--surface-light-active`/`-dark-active` sit
 *  at 293.15°/284.87°) — substituting it would be a small but real pixel
 *  change, not the zero-pixel-change `theme-system-decision-addendum.md`/
 *  `-addendum-2.md` promise for Default. Same reasoning `clearInlineAccentRamp`
 *  already applies to `--brand-*` for the same id. */
function clearTintedNeutrals(root: HTMLElement): void {
  for (const property of TINTED_NEUTRAL_PROPERTIES) root.style.removeProperty(property);
}

/** Sets a computed canvas/chrome tint for every id except `"default"`, which
 *  clears instead (see `clearTintedNeutrals`). See `index.css`'s
 *  `--canvas-tint-*`/`--chrome-tint-*` primitives and
 *  `theme-system-decision-addendum.md`. */
function applyTintedNeutrals(root: HTMLElement, hue: number | null): void {
  const tint = computeTintedNeutrals(hue);
  root.style.setProperty("--canvas-tint-light", tint.canvasLight);
  root.style.setProperty("--canvas-tint-dark", tint.canvasDark);
  root.style.setProperty("--chrome-tint-light", tint.chromeLight);
  root.style.setProperty("--chrome-tint-dark", tint.chromeDark);
  root.style.setProperty("--accent-tint-light", tint.accentLight);
  root.style.setProperty("--accent-tint-dark", tint.accentDark);
}

/** Graphite and Onyx have no hue by design (`ACCENT_PRESETS`'s comment) —
 *  their canvas/chrome/accent tints go fully achromatic to match, per
 *  `theme-system-decision-addendum.md`. Every other known id, including
 *  `"default"`, resolves through `CURATED_PRESET_HUE`. */
function neutralTintHueForPreset(id: string): number | null {
  if (id === "graphite" || id === "onyx") return null;
  return CURATED_PRESET_HUE[id] ?? CURATED_PRESET_HUE[DEFAULT_ACCENT_PRESET];
}

/** The stored preset id, or the default when nothing valid is stored.
 *  Never throws: a private window or blocked site data makes the
 *  `localStorage` getter itself throw (`crash-fallback.tsx` makes the same
 *  point), and an unknown id — a preset retired since it was chosen — must
 *  fall back rather than leave a dead attribute on `<html>`. */
export function readStoredAccentPreset(): string {
  try {
    const stored = window.localStorage.getItem(ACCENT_PRESET_STORAGE_KEY);
    return stored && isKnownPreset(stored) ? stored : DEFAULT_ACCENT_PRESET;
  } catch {
    return DEFAULT_ACCENT_PRESET;
  }
}

/**
 * Sets `data-accent-preset` on `<html>` — the same element `:root` and
 * `next-themes`' `.dark` both match (`architecture.md` §3). The default
 * removes the attribute rather than setting it to `"default"`, so a browser
 * with nothing stored and one that has explicitly chosen "Default" render
 * identically: `:root`'s own values, un-overridden.
 *
 * Never a semantic utility, and never on `#root` or `<body>`: a custom
 * property that references another (`--primary: var(--brand-500)`) resolves
 * on the element where the *reference* is declared, not where it is read, so
 * the attribute has to sit on the same element as the semantic layer itself
 * (`roadblocks.md` R4).
 *
 * `id === CUSTOM_ACCENT_PRESET_ID` is the one branch that can refuse: the
 * generated ramp (`@/lib/accent-ramp`) is only applied — as inline
 * `--brand-*` custom properties, since it is user-generated data rather than
 * an authored CSS block — when `evaluateAccentRamp` (`@/lib/accent-contrast`)
 * says it clears every gamut and contrast bar the curated presets already
 * do. A refused hue leaves `<html>` completely untouched (not even the
 * dataset attribute changes), so the last good state — custom or curated —
 * keeps rendering; it never reverts to the default ramp just because a drag
 * passed through a bad hue. Returns whether the hue was applied, so the
 * picker can tell the operator when it was not.
 */
export function applyAccentPreset(id: string, customHue?: number): boolean {
  const root = document.documentElement;

  if (id === CUSTOM_ACCENT_PRESET_ID) {
    const hue = normalizeHue(customHue ?? readStoredCustomHue());
    const ramp = generateCustomRamp(hue);
    if (!evaluateAccentRamp(ramp).ok) return false;
    clearInlineAccentRamp(root); // no-op unless a previous custom ramp was applied
    for (const step of ACCENT_STEPS) {
      root.style.setProperty(`--brand-${step}`, accentRampStepToOklch(ramp[step]));
    }
    root.dataset.accentPreset = CUSTOM_ACCENT_PRESET_ID;
    applyTintedNeutrals(root, hue);
    return true;
  }

  clearInlineAccentRamp(root); // switching off "custom" must drop its inline override
  const resolvedId = id === DEFAULT_ACCENT_PRESET || !isKnownPreset(id) ? DEFAULT_ACCENT_PRESET : id;
  if (resolvedId === DEFAULT_ACCENT_PRESET) {
    delete root.dataset.accentPreset;
    clearTintedNeutrals(root); // today's exact `:root` pixels, not an approximation of them
  } else {
    root.dataset.accentPreset = resolvedId;
    applyTintedNeutrals(root, neutralTintHueForPreset(resolvedId));
  }
  return true;
}

/** Reads storage and applies it — the one call `main.tsx` makes, synchronously
 *  and before `mount()`, so the chosen preset is already on `<html>` for
 *  React's first commit. No inline `<script>`: React makes a client-rendered
 *  one inert (`roadblocks.md` R1), so `next-themes`' own anti-flash trick does
 *  not work in this SPA and copying it here would silently do nothing. No
 *  static `public/` file either: those are served `immutable` for a year
 *  (`roadblocks.md` R3). This function lives in the hashed app bundle instead,
 *  which is what makes it reach a returning browser at all.
 *
 *  Reads `oc.appearance.customHue` too, but only when the stored preset id is
 *  `"custom"` — same "custom" pre-mount path the curated ids already had, so
 *  a stored custom hue paints on the very first frame with zero flash, same
 *  as any other preset. */
export function applyStoredAccentPreset(): void {
  const id = readStoredAccentPreset();
  applyAccentPreset(id, id === CUSTOM_ACCENT_PRESET_ID ? readStoredCustomHue() : undefined);
}

/**
 * In-memory fallback for the custom hue, updated immediately after a
 * successful `applyAccentPreset` call for `"custom"` — regardless of whether
 * the `localStorage` write that follows it succeeds. Without this, a failed
 * write (private browsing, quota) leaves `readStoredCustomHue()` returning
 * whatever was there before: the ramp on `<html>` is correct (the write
 * failure never reaches `applyAccentPreset`), but `useCustomHue()` — and so
 * the picker's own swatch and slider position — would keep reporting the
 * stale hue instead of the one actually applied. `null` means "nothing
 * applied in this tab yet"; `getCustomHueSnapshot` falls back to storage in
 * that case, same as before this existed.
 */
let customHueSnapshot: number | null = null;

/** The hue `useCustomHue` should currently report: the in-memory snapshot the
 *  moment one exists, else whatever storage holds. Read by `useCustomHue`
 *  and by `CustomSwatch`/`CustomHueControl` indirectly through it — never by
 *  `applyAccentPreset` or `setAccentPreset` themselves, which already carry
 *  the hue as a parameter. */
function getCustomHueSnapshot(): number {
  return customHueSnapshot ?? readStoredCustomHue();
}

let listeners: Array<() => void> = [];

function emit(): void {
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void): () => void {
  listeners.push(listener);
  return () => {
    listeners = listeners.filter((l) => l !== listener);
  };
}

/** The id `useAccentPreset` should currently report: the live `dataset`
 *  attribute when one is set, `"default"` otherwise. Reading the DOM rather
 *  than a module-level variable is what keeps this correct even though
 *  `applyStoredAccentPreset` runs before any React state exists to seed it. */
function getSnapshot(): string {
  return document.documentElement.dataset.accentPreset ?? DEFAULT_ACCENT_PRESET;
}

/** Same value on the server as the client would compute before hydration —
 *  irrelevant here (this app has no SSR pass), kept only because
 *  `useSyncExternalStore` requires a third argument. */
function getServerSnapshot(): string {
  return DEFAULT_ACCENT_PRESET;
}

/**
 * Persists `id`, applies it, and notifies every `useAccentPreset()` caller —
 * including ones in other tabs, via the `storage` event listener armed below.
 * A `localStorage` write failing (storage disabled, quota) still applies the
 * choice for this tab; it just will not survive a reload.
 *
 * For `CUSTOM_ACCENT_PRESET_ID`, `customHue` is the hue to try; a refused hue
 * (`applyAccentPreset` returns `false`) writes nothing to storage and leaves
 * whatever was showing alone — the picker's slider can be dragged through a
 * bad hue without persisting it or losing the last good one. Returns whether
 * the hue (or preset) was applied, so a caller can show the refusal.
 */
export function setAccentPreset(id: string, customHue?: number): boolean {
  if (id === CUSTOM_ACCENT_PRESET_ID) {
    const hue = normalizeHue(customHue ?? readStoredCustomHue());
    const applied = applyAccentPreset(CUSTOM_ACCENT_PRESET_ID, hue);
    if (!applied) return false;
    // Update the in-memory fallback before the storage write, which can
    // still throw below — the ramp is already on `<html>`, so `useCustomHue`
    // must report this hue regardless of whether the write lands.
    customHueSnapshot = hue;
    try {
      window.localStorage.setItem(ACCENT_PRESET_STORAGE_KEY, CUSTOM_ACCENT_PRESET_ID);
      window.localStorage.setItem(CUSTOM_HUE_STORAGE_KEY, String(hue));
    } catch {
      // Storage refused; the choice still holds for this tab via customHueSnapshot.
    }
    emit();
    return true;
  }

  try {
    if (id === DEFAULT_ACCENT_PRESET) {
      window.localStorage.removeItem(ACCENT_PRESET_STORAGE_KEY);
    } else {
      window.localStorage.setItem(ACCENT_PRESET_STORAGE_KEY, id);
    }
  } catch {
    // Storage refused; the choice still holds for this tab.
  }
  applyAccentPreset(id);
  emit();
  return true;
}

if (typeof window !== "undefined") {
  // Cross-tab sync, the same shape `next-themes` uses for its own key: a
  // `storage` event only fires in tabs that did NOT make the write, and only
  // for the one key it names — a `"theme"` change must never re-apply an
  // accent preset, and vice versa. Both accent keys are watched: a hue-only
  // change (the preset id stays `"custom"`) only touches
  // `CUSTOM_HUE_STORAGE_KEY`, and would otherwise never re-apply here.
  window.addEventListener("storage", (event) => {
    if (event.key !== ACCENT_PRESET_STORAGE_KEY && event.key !== CUSTOM_HUE_STORAGE_KEY) return;
    // A `storage` event only fires for a write that *succeeded* (in another
    // tab), so it is always fresher than any snapshot this tab is holding
    // only because its own write failed — drop the snapshot rather than let
    // it shadow the value `applyStoredAccentPreset` is about to read.
    if (event.key === CUSTOM_HUE_STORAGE_KEY) customHueSnapshot = null;
    applyStoredAccentPreset();
    emit();
  });
}

/** The live accent preset id, reactive across this tab and every other one on
 *  the same origin. `useSyncExternalStore` rather than `useState`: the true
 *  state is the DOM attribute `applyStoredAccentPreset` already set before
 *  React existed, and mirroring it into a second, React-owned copy is exactly
 *  the kind of drift a module-level store (`src/connections/registry.ts` uses
 *  the same pattern) avoids. */
export function useAccentPreset(): string {
  return useSyncExternalStore(subscribe, getSnapshot, getServerSnapshot);
}

/** Same reactivity contract as `useAccentPreset`, for the stored custom hue —
 *  the Custom tile's slider reads this to seed its position, including after
 *  a cross-tab `storage` event re-applies a hue chosen elsewhere. Reads
 *  `getCustomHueSnapshot` rather than storage directly (unlike `getSnapshot`,
 *  which reads the DOM): the hue is meaningful even while `"custom"` is not
 *  the active preset, the ramp on `<html>` carries no independent record of
 *  the hue it was generated from, and a `localStorage` write can fail after
 *  the ramp has already applied — the in-memory snapshot is what keeps this
 *  hook honest in that case. */
export function useCustomHue(): number {
  return useSyncExternalStore(subscribe, getCustomHueSnapshot, () => DEFAULT_CUSTOM_HUE);
}
