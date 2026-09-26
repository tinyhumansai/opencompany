import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import { ACCENT_PRESETS, DEFAULT_ACCENT_PRESET } from "@/lib/accent-presets";

/**
 * Registry integrity (issue #2493, test-plan U2). Keeps the TypeScript
 * registry and the CSS section in `index.css` from drifting apart — an id
 * with no block would silently do nothing when chosen, and a block with no
 * id would be unreachable from the picker.
 */

const here = dirname(fileURLToPath(import.meta.url));
const indexCss = readFileSync(resolve(here, "../../src/index.css"), "utf8");
const registrySource = readFileSync(resolve(here, "../../src/lib/accent-presets.ts"), "utf8");

describe("accent preset registry", () => {
  it("has unique, non-empty ids matching the attribute-selector-safe pattern", () => {
    const ids = ACCENT_PRESETS.map((p) => p.id);
    expect(new Set(ids).size).toBe(ids.length);
    for (const id of ids) {
      expect(id.length).toBeGreaterThan(0);
      expect(id).toMatch(/^[a-z][a-z0-9-]*$/);
    }
  });

  it("has exactly one default entry, with no CSS block of its own", () => {
    const defaults = ACCENT_PRESETS.filter((p) => p.id === DEFAULT_ACCENT_PRESET);
    expect(defaults).toHaveLength(1);
    expect(indexCss).not.toContain(`[data-accent-preset="${DEFAULT_ACCENT_PRESET}"]`);
  });

  it("gives every non-default preset exactly one CSS block, and every CSS block a registry entry", () => {
    const nonDefaultIds = ACCENT_PRESETS.filter((p) => p.id !== DEFAULT_ACCENT_PRESET).map((p) => p.id);
    for (const id of nonDefaultIds) {
      const matches = indexCss.match(new RegExp(`\\[data-accent-preset="${id}"\\] \\{`, "g")) ?? [];
      expect(matches, `--brand-* block for preset "${id}"`).toHaveLength(1);
    }

    const blockIds = [...indexCss.matchAll(/\[data-accent-preset="([a-z][a-z0-9-]*)"\]\s*\{/g)].map(
      (m) => m[1],
    );
    for (const id of blockIds) {
      expect(nonDefaultIds, `CSS block "${id}" has no registry entry`).toContain(id);
    }
  });

  it("carries no colour literal — hex, oklch, rgb, or hsl — in the registry source", () => {
    // Covers the gap `roadblocks.md` R7 names: the CI token gate only greps
    // 6-digit hex in .ts/.tsx, so an oklch(...) literal would pass it silently.
    // Exactly 6 hex digits, the real convention — `#[0-9a-fA-F]{3,8}` would
    // false-positive on an issue reference like "#2493" in a doc comment.
    expect(registrySource).not.toMatch(/#[0-9a-fA-F]{6}\b/);
    expect(registrySource).not.toMatch(/\b(oklch|rgb|hsl)\(/);
  });
});
