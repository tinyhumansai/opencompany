import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import { ACCENT_PRESETS, DEFAULT_ACCENT_PRESET } from "@/lib/accent-presets";

/**
 * Shape and order of the `index.css` accent-preset blocks (issue #2493,
 * test-plan U3). Each rule here guards a specific failure mode named in
 * `docs/issues/accent-theme-presets/roadblocks.md`:
 *
 *   - a preset declaring something other than the ten `--brand-*` primitives
 *     unqualified would leak into the other theme (R2's specificity trap);
 *   - the section sitting before `.dark` would let a `[data-accent-preset="x"]
 *     .dark {` block's substring `.dark {` fool a naive string search (R14),
 *     and would also change *which* rule wins the cascade tie (R2 again).
 */

const indexCss = readFileSync(
  resolve(dirname(fileURLToPath(import.meta.url)), "../../src/index.css"),
  "utf8",
);

const BRAND_STEPS = [50, 100, 200, 300, 400, 500, 600, 700, 800, 900] as const;

function blockBody(openIndex: number): string {
  let depth = 0;
  for (let i = indexCss.indexOf("{", openIndex); i < indexCss.length; i += 1) {
    if (indexCss[i] === "{") depth += 1;
    else if (indexCss[i] === "}") {
      depth -= 1;
      if (depth === 0) return indexCss.slice(openIndex, i);
    }
  }
  throw new Error(`unterminated block starting at ${openIndex}`);
}

function presetBlock(id: string): string {
  const open = indexCss.indexOf(`[data-accent-preset="${id}"] {`);
  if (open < 0) throw new Error(`no unqualified block for preset "${id}"`);
  return blockBody(open);
}

const presetIds = ACCENT_PRESETS.filter((p) => p.id !== DEFAULT_ACCENT_PRESET).map((p) => p.id);

describe("accent preset CSS shape", () => {
  it("declares all ten --brand-{50..900} primitives, and nothing else, unqualified", () => {
    for (const id of presetIds) {
      const body = presetBlock(id);
      const withoutComments = body.replace(/\/\*[\s\S]*?\*\//g, "");
      const declared = [...withoutComments.matchAll(/--([a-z0-9-]+):/g)].map((m) => m[1]);
      const brandSteps = declared.filter((name) => /^brand-(50|[1-9]00)$/.test(name));
      const other = declared.filter((name) => !/^brand-(50|[1-9]00)$/.test(name));

      expect(new Set(brandSteps).size, `preset "${id}" missing a brand step`).toBe(
        BRAND_STEPS.length,
      );
      expect(other, `preset "${id}" declares a non-ramp token unqualified`).toEqual([]);
    }
  });

  it("every declared value is an oklch() literal with a trailing hex comment", () => {
    for (const id of presetIds) {
      const body = presetBlock(id);
      for (const step of BRAND_STEPS) {
        const line = new RegExp(`--brand-${step}:\\s*(oklch\\([^)]+\\));\\s*/\\*\\s*(#[0-9a-fA-F]{6})`).exec(
          body,
        );
        expect(line, `preset "${id}" step ${step} is not \`oklch(...); /* #hex */\``).not.toBeNull();
      }
    }
  });

  it("declares nothing that reaches `--brand-discord`, `--brand-chargebee` or `--brand-paypal-*`", () => {
    // roadblocks.md R6: those share the `--brand-` prefix but are third-party
    // colours, and must never be swept in by a completeness check like this one.
    for (const id of presetIds) {
      const body = presetBlock(id);
      expect(body).not.toMatch(/--brand-(discord|chargebee|paypal)/);
    }
  });

  it("keeps the violet preset byte-identical to :root's own ramp", () => {
    // accent-preset-picker.tsx paints the Default swatch (which has no CSS
    // block of its own — see the registry test) with the "violet" preset's
    // block instead, on the strength of this exact invariant. If violet ever
    // drifts from :root, the Default swatch silently starts lying.
    const rootBlock = blockBody(indexCss.indexOf(":root {"));
    const violetBlock = presetBlock("violet");
    // Numeric comparison, not string equality: the file's own formatting drops
    // a trailing zero inconsistently (`0.841` vs `0.8410`), which is the same
    // number and not a drift worth failing on.
    const oklchNumbers = (body: string, step: number) => {
      const m = new RegExp(`--brand-${step}:\\s*oklch\\(([\\d.]+)\\s+([\\d.]+)\\s+([\\d.]+)\\)`).exec(body);
      if (!m) return null;
      return [Number(m[1]), Number(m[2]), Number(m[3])];
    };
    for (const step of BRAND_STEPS) {
      expect(oklchNumbers(violetBlock, step), `--brand-${step} missing from the violet block`).toEqual(
        oklchNumbers(rootBlock, step),
      );
    }
  });

  it("the ACCENT PRESETS section starts after the last top-level :root and .dark block", () => {
    const sectionStart = indexCss.indexOf("ACCENT PRESETS");
    expect(sectionStart).toBeGreaterThan(0);

    const lastTopLevelBlockStart = (selector: string): number => {
      let last = -1;
      let from = 0;
      for (;;) {
        const open = indexCss.indexOf(`${selector} {`, from);
        if (open < 0 || open > sectionStart) break;
        last = open;
        from = open + 1;
      }
      return last;
    };

    expect(lastTopLevelBlockStart(":root")).toBeLessThan(sectionStart);
    expect(lastTopLevelBlockStart(".dark")).toBeLessThan(sectionStart);
  });
});
