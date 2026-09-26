import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

const here = dirname(fileURLToPath(import.meta.url));
const read = (rel: string) => readFileSync(resolve(here, "../../src", rel), "utf8");

describe("operator control names (issue #1395)", () => {
  it("names the ledger search and status filter independently of their values", () => {
    const ledgers = read("views/LedgersView.tsx");

    expect(ledgers).toContain('aria-label="Search ledger entries"');
    expect(ledgers).toContain('aria-label="Filter by status"');
  });

  it("names the memory search and type filter independently of their placeholders", () => {
    const memory = read("views/MemoryView.tsx");

    expect(memory).toContain('aria-label="Search memory"');
    expect(memory).toContain('aria-label="Filter by memory type"');
  });

  it("names the usage range filter", () => {
    expect(read("views/UsageView.tsx")).toContain('aria-label="Usage date range"');
  });

  it("keeps the composer named after its placeholder disappears", () => {
    const composer = read("views/room/MessageComposer.tsx");

    expect(composer).toContain("aria-label={placeholder}");
  });

  it("names the accent preset picker (issue #2493)", () => {
    // `role="radiogroup"` / `role="radio"` and `aria-checked` come from the
    // `@base-ui/react` primitives at runtime rather than appearing literally in
    // source, so they are not grepped here — `test/e2e/accent-preset.spec.ts`
    // E3/E4 verify the live semantics with `getByRole("radio", { name })`
    // instead. This checks the one thing that IS source text: the group's name
    // and each swatch's visible label, so colour is never the only cue.
    const picker = read("components/accent-preset-picker.tsx");

    expect(picker).toContain('aria-label="Accent preset"');
    expect(picker).toContain("{preset.label}");
  });
});
