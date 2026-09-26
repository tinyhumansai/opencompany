import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

/**
 * Phase 1 of issue #2493: chart slot 1 and the knowledge graph's "AI agents"
 * mark are identity, not interaction, and must stay one fixed signature
 * violet regardless of an operator's accent preset. Guards that they resolve
 * through `--signature-*`, never `--brand-*`, and that no preset block can
 * reach them (`architecture.md` §4, `roadblocks.md` R10).
 *
 * `--kg-accent` sits in a **second** `.dark { … }` block — the knowledge-graph
 * bridge, `index.css` — so this file cannot reuse `shell-chrome-tokens.test.ts`'s
 * `block(".dark")`, which returns only the first. It finds every top-level
 * `.dark {` block instead.
 */

const indexCss = readFileSync(
  resolve(dirname(fileURLToPath(import.meta.url)), "../../src/index.css"),
  "utf8",
);

function firstBlock(selector: string): string {
  const open = indexCss.indexOf(`${selector} {`);
  if (open < 0) throw new Error(`no \`${selector} {\` block in index.css`);
  let depth = 0;
  for (let i = indexCss.indexOf("{", open); i < indexCss.length; i += 1) {
    if (indexCss[i] === "{") depth += 1;
    else if (indexCss[i] === "}") {
      depth -= 1;
      if (depth === 0) return indexCss.slice(open, i);
    }
  }
  throw new Error(`unterminated \`${selector}\` block`);
}

/** Every top-level `${selector} {` block, brace-matched, in source order. */
function allBlocks(selector: string): string[] {
  const blocks: string[] = [];
  let from = 0;
  for (;;) {
    const open = indexCss.indexOf(`${selector} {`, from);
    if (open < 0) break;
    let depth = 0;
    let end = -1;
    for (let i = indexCss.indexOf("{", open); i < indexCss.length; i += 1) {
      if (indexCss[i] === "{") depth += 1;
      else if (indexCss[i] === "}") {
        depth -= 1;
        if (depth === 0) {
          end = i;
          break;
        }
      }
    }
    if (end < 0) throw new Error(`unterminated \`${selector}\` block starting at ${open}`);
    blocks.push(indexCss.slice(open, end));
    from = end + 1;
  }
  return blocks;
}

function declaration(body: string, name: string): string | null {
  const withoutComments = body.replace(/\/\*[\s\S]*?\*\//g, "");
  const match = new RegExp(`(?:^|[;{\\s])--${name}:\\s*([^;]+);`).exec(withoutComments);
  return match ? match[1].trim() : null;
}

describe("signature tokens (issue #2493 Phase 1)", () => {
  it("declares --signature-500 and --signature-400 in :root", () => {
    const root = firstBlock(":root");
    expect(declaration(root, "signature-500")).not.toBeNull();
    expect(declaration(root, "signature-400")).not.toBeNull();
  });

  it("routes --chart-1 through the signature tokens, not the brand ramp", () => {
    const light = firstBlock(":root");
    const dark = firstBlock(".dark");
    expect(declaration(light, "chart-1")).toBe("var(--signature-500)");
    expect(declaration(dark, "chart-1")).toBe("var(--signature-400)");
  });

  it("routes --kg-accent through the signature tokens, in the knowledge-graph bridge's .dark block", () => {
    const darkBlocks = allBlocks(".dark");
    expect(darkBlocks.length).toBeGreaterThanOrEqual(2);
    const kgBridgeDark = darkBlocks.find((b) => declaration(b, "kg-accent") !== null);
    expect(kgBridgeDark, "no `.dark` block declares --kg-accent").toBeTruthy();
    expect(declaration(kgBridgeDark!, "kg-accent")).toBe("var(--signature-400)");
    const kgBridgeLight = allBlocks(":root").find((b) => declaration(b, "kg-accent") !== null);
    expect(declaration(kgBridgeLight!, "kg-accent")).toBe("var(--signature-500)");
  });

  it("no accent-preset block declares --signature-500 or --signature-400", () => {
    const presetBlocks = indexCss.slice(indexCss.indexOf("ACCENT PRESETS"));
    for (const preset of ["signature-500", "signature-400"]) {
      const re = new RegExp(`--${preset}:`);
      expect(re.test(presetBlocks), `a preset block declares --${preset}`).toBe(false);
    }
  });
});
