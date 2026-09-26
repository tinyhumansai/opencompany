import { useEffect, useState, type CSSProperties } from "react";
import { Check } from "lucide-react";
import { RadioGroup } from "@base-ui/react/radio-group";
import { Radio } from "@base-ui/react/radio";
import { Slider } from "@base-ui/react/slider";

import { cn } from "@/lib/utils";
import {
  accentRampStepToOklch,
  generateCustomRamp,
  normalizeHue,
} from "@/lib/accent-ramp";
import { evaluateAccentRamp } from "@/lib/accent-contrast";
import {
  ACCENT_PRESETS,
  CUSTOM_ACCENT_PRESET_ID,
  DEFAULT_ACCENT_PRESET,
  setAccentPreset,
  useAccentPreset,
  useCustomHue,
  type AccentPreset,
} from "@/lib/accent-presets";

/**
 * What the Default swatch paints itself with.
 *
 * `DEFAULT_ACCENT_PRESET` deliberately has no `[data-accent-preset="default"]`
 * block in `index.css` (`architecture.md` §2, `accent-presets-registry.test.ts`
 * pins that it never gains one) — selecting it means "remove the attribute",
 * not "apply a block named default". A swatch carrying
 * `data-accent-preset="default"` would therefore have nothing to resolve
 * against and silently inherit whatever preset happens to be active on
 * `<html>` — wrong for exactly the same reason `bg-primary` is wrong (below).
 *
 * `"violet"` is the fix: its block is authored to hold the *exact* values
 * `:root` itself declares (see its comment in `index.css`), so painting the
 * Default swatch with it is indistinguishable from painting it with the true
 * default — without needing a block that the registry test says must not
 * exist.
 */
const DEFAULT_SWATCH_PRESET_ID = "violet";

/**
 * The accent-preset picker — a `role="radiogroup"` of named swatches, one per
 * `AccentPreset`, plus a trailing "Custom" tile (issue #2493 follow-on,
 * `docs/issues/accent-theme-presets/theme-system-decision.md`'s constrained
 * hue picker). See `docs/issues/accent-theme-presets/architecture.md` §10 for
 * the original nine.
 *
 * # Why each swatch paints with `bg-brand-500`, never `bg-primary`
 *
 * `--primary: var(--brand-500)` resolves against whatever `data-accent-preset`
 * `<html>` currently carries — that is the whole point of the feature — so a
 * swatch styled with `bg-primary` would show the *page's* preset on every
 * swatch instead of its own (`roadblocks.md` R4). `bg-brand-500` emits
 * `var(--brand-500)` directly, which each swatch's own `data-accent-preset`
 * attribute (set on itself, not inherited from `<html>`) resolves correctly —
 * the Custom tile does the same thing with an inline `--brand-500`/`--brand-400`
 * instead, since its ramp has no CSS block to attach a `data-accent-preset` to.
 *
 * # Why `RadioGroup`/`Radio` from `@base-ui/react` rather than hand-rolled
 *
 * Arrow-key roving focus, `aria-checked`, and `role="radiogroup"` /
 * `role="radio"` all come from the primitive rather than being re-implemented
 * here — the same library `Switch` (`components/ui/switch.tsx`) already uses.
 * The Custom tile is a `Radio.Root` like every other one, so it inherits the
 * exact same keyboard/roving-focus semantics without special-casing.
 */
export function AccentPresetPicker() {
  const current = useAccentPreset();

  return (
    <div className="flex flex-col gap-3">
      <RadioGroup
        aria-label="Accent preset"
        value={current}
        onValueChange={(value) => setAccentPreset(value as string)}
        className="grid grid-cols-4 gap-2 sm:grid-cols-11"
      >
        {ACCENT_PRESETS.map((preset) => (
          <PresetSwatch key={preset.id} preset={preset} />
        ))}
        <CustomSwatch />
      </RadioGroup>
      {current === CUSTOM_ACCENT_PRESET_ID ? <CustomHueControl /> : null}
    </div>
  );
}

function PresetSwatch({ preset }: { preset: AccentPreset }) {
  const swatchPresetId = preset.id === DEFAULT_ACCENT_PRESET ? DEFAULT_SWATCH_PRESET_ID : preset.id;
  return (
    <Radio.Root
      value={preset.id}
      data-accent-preset={swatchPresetId}
      className="group/swatch flex flex-col items-center gap-1.5 rounded-lg p-1.5 outline-none focus-visible:ring-3 focus-visible:ring-ring/50"
    >
      <span
        className={cn(
          "relative flex size-9 items-center justify-center rounded-full bg-brand-500 ring-1 ring-inset ring-black/10 transition-transform group-data-checked/swatch:scale-110 dark:bg-brand-400 dark:ring-white/10",
        )}
      >
        {/* `data-checked` comes from `Radio.Root`'s own state, not from a
            second read of `current` — one source of truth for "is this the
            selected preset". */}
        <Check
          className="size-4 text-white opacity-0 transition-opacity group-data-checked/swatch:opacity-100 dark:text-black"
          aria-hidden="true"
        />
      </span>
      {/* Colour must not be the only cue (WCAG 1.4.1) — the label is always
          visible text, never a tooltip-only affordance. */}
      <span className="text-2xs text-muted-foreground group-data-checked/swatch:text-foreground">
        {preset.label}
      </span>
    </Radio.Root>
  );
}

/**
 * The tenth tile. Unlike `PresetSwatch`, there is no `[data-accent-preset="custom"]`
 * CSS block to point the swatch at — a custom ramp is user-generated data, not
 * an authored asset (`theme-system-decision.md`) — so the swatch's own
 * `--brand-500`/`--brand-400` are set inline, from the *stored* hue (falling
 * back to whatever `generateCustomRamp` would produce for it) so the tile
 * shows the operator's actual saved colour even while some other preset is
 * currently active on `<html>`.
 */
function CustomSwatch() {
  const hue = useCustomHue();
  const ramp = generateCustomRamp(hue);
  const style = {
    "--brand-500": accentRampStepToOklch(ramp[500]),
    "--brand-400": accentRampStepToOklch(ramp[400]),
  } as CSSProperties;

  return (
    <Radio.Root
      value={CUSTOM_ACCENT_PRESET_ID}
      style={style}
      className="group/swatch flex flex-col items-center gap-1.5 rounded-lg p-1.5 outline-none focus-visible:ring-3 focus-visible:ring-ring/50"
    >
      <span className="relative flex size-9 items-center justify-center rounded-full bg-brand-500 ring-1 ring-inset ring-black/10 transition-transform group-data-checked/swatch:scale-110 dark:bg-brand-400 dark:ring-white/10">
        <Check
          className="size-4 text-white opacity-0 transition-opacity group-data-checked/swatch:opacity-100 dark:text-black"
          aria-hidden="true"
        />
      </span>
      <span className="text-2xs text-muted-foreground group-data-checked/swatch:text-foreground">
        Custom
      </span>
    </Radio.Root>
  );
}

/**
 * The single hue slider the Custom tile opens — no saturation/lightness
 * control, per the decision doc: those stay locked to the curated ramp's own
 * cadence (`@/lib/accent-ramp`), so no drag of this slider can produce a
 * combination outside the already-proven shape.
 *
 * Applies live on every change, but only when `evaluateAccentRamp` says the
 * resulting ramp clears the same gamut/contrast bars the curated presets do
 * — `setAccentPreset` itself enforces this and reports back whether it
 * applied, so a refused hue never touches `<html>` or storage and the slider
 * can be dragged straight through a bad stretch of the hue circle without
 * losing the last good colour.
 */
function CustomHueControl() {
  const storedHue = useCustomHue();
  const [hue, setHue] = useState(storedHue);
  const [refused, setRefused] = useState(false);

  // Stay in sync with a hue applied elsewhere (another tab, or a reload).
  useEffect(() => {
    setHue(storedHue);
    setRefused(false);
  }, [storedHue]);

  const previewRamp = generateCustomRamp(hue);
  const evaluation = evaluateAccentRamp(previewRamp);

  function handleValueChange(value: number | readonly number[]) {
    const next = normalizeHue(Array.isArray(value) ? value[0] : (value as number));
    setHue(next);
    const applied = setAccentPreset(CUSTOM_ACCENT_PRESET_ID, next);
    setRefused(!applied);
  }

  return (
    <div className="flex flex-col gap-2 rounded-lg border border-border p-3">
      <div className="flex items-center justify-between gap-3">
        <span className="text-sm font-medium">Hue</span>
        <span
          className="size-6 shrink-0 rounded-full ring-1 ring-inset ring-black/10 dark:ring-white/10"
          style={{ backgroundColor: accentRampStepToOklch(previewRamp[500]) }}
          aria-hidden="true"
        />
      </div>
      {/* `max={359}`, not `360`: `normalizeHue` wraps 360 to 0
          (`accent-ramp.ts`'s `hue % 360`), so a slider that could reach 360
          would render an equivalent colour but then snap the controlled
          thumb back to the start on the very next render — a picker-state
          jump, not a colour bug. 359 is the last value the thumb can sit at
          before that wrap would occur. */}
      <Slider.Root min={0} max={359} step={1} value={hue} onValueChange={handleValueChange}>
        <Slider.Control className="flex w-full items-center py-2">
          <Slider.Track className="relative h-1.5 w-full rounded-full bg-muted">
            <Slider.Indicator className="absolute h-full rounded-full bg-brand-500 dark:bg-brand-400" />
            <Slider.Thumb
              getAriaLabel={() => "Custom accent hue"}
              aria-valuetext={`${Math.round(hue)} degrees`}
              className="size-4 rounded-full bg-brand-500 outline-none ring-1 ring-inset ring-black/10 focus-visible:ring-3 focus-visible:ring-ring/50 dark:bg-brand-400 dark:ring-white/10"
            />
          </Slider.Track>
        </Slider.Control>
      </Slider.Root>
      <div className="flex items-center justify-between text-2xs text-muted-foreground">
        <span>{Math.round(hue)}°</span>
        {/* `aria-live` rather than a layout-shifting error banner: the message
            appears and disappears as the slider moves without ever grabbing
            focus away from it. */}
        <span role="status" aria-live="polite" className={refused ? "text-destructive" : undefined}>
          {refused
            ? "This hue doesn't clear the contrast bar — try another."
            : evaluation.ok
              ? "Applied"
              : ""}
        </span>
      </div>
    </div>
  );
}
