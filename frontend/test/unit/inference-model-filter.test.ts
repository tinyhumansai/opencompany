// Narrowing four hundred model ids to the one the operator wants.
//
// The matcher itself is `search/rank.ts` and is tested there — including the
// Unicode folding, which is the part a fresh implementation always gets wrong.
// What is tested here is the ordering it produces for THESE inputs, the cap, and
// the row that offers back what was typed.

import { describe, expect, it } from "vitest";

import { MODEL_LIMIT, cappedNote, filterModels } from "@/inference/model-filter";

const CATALOG = [
  "anthracite-org/magnum-v4-72b",
  "anthropic/claude-fable-5",
  "anthropic/claude-fable-5:batch",
  "openai/gpt-6",
  "openai/gpt-6:batch",
  "openai/gpt-6-mini",
];

describe("ranking, not just matching", () => {
  it("puts the provider you named above one that merely contains the letters", () => {
    // `claude` must not surface `anthracite-org/magnum-v4-72b` first just
    // because it sorts earlier. This is why the shared matcher is reused rather
    // than a fresh `.includes()`.
    const { shown } = filterModels(CATALOG, "claude");
    expect(shown[0]).toBe("anthropic/claude-fable-5");
    expect(shown).not.toContain("anthracite-org/magnum-v4-72b");
  });

  it("puts an exact id above its own variants", () => {
    // Typing `openai/gpt-6` in full should not bury it under twelve `:batch`
    // rows that also contain it.
    const { shown } = filterModels(CATALOG, "openai/gpt-6");
    expect(shown[0]).toBe("openai/gpt-6");
  });

  it("is stable across renders rather than engine-order dependent", () => {
    const once = filterModels(CATALOG, "anthropic").shown;
    const twice = filterModels(CATALOG, "anthropic").shown;
    expect(once).toEqual(twice);
  });
});

describe("the unfiltered list", () => {
  it("keeps catalog order rather than sorting by nothing", () => {
    // With no term every entry scores the same, so ranking would be arbitrary.
    // The host already sorted it.
    expect(filterModels(CATALOG, "").shown).toEqual(CATALOG);
    expect(filterModels(CATALOG, "   ").shown).toEqual(CATALOG);
  });
});

describe("the cap", () => {
  const many = Array.from({ length: 400 }, (_, i) => `vendor/model-${String(i).padStart(3, "0")}`);

  it("stops at the limit", () => {
    expect(filterModels(many, "").shown).toHaveLength(MODEL_LIMIT);
    expect(filterModels(many, "model").shown).toHaveLength(MODEL_LIMIT);
  });

  it("says what it hid rather than truncating silently", () => {
    // A list that quietly stops at fifty is a list that lies about what the
    // provider serves.
    const note = cappedNote(filterModels(many, "model"));
    expect(note).toContain("400");
    expect(note).toContain(String(MODEL_LIMIT));
  });

  it("says nothing when nothing was hidden", () => {
    expect(cappedNote(filterModels(CATALOG, ""))).toBeNull();
  });
});

describe("offering back what was typed", () => {
  it("offers an id the catalog has never heard of", () => {
    // The Azure case and the stale-catalog case at once: a search that finds
    // nothing is also a perfectly good answer, so one control does both jobs.
    const { shown, offerTyped } = filterModels(CATALOG, "my-deployment-name");
    expect(shown).toEqual([]);
    expect(offerTyped).toBe("my-deployment-name");
  });

  it("does not offer a row that is already in the list", () => {
    // Two rows meaning one thing is worse than one.
    expect(filterModels(CATALOG, "openai/gpt-6").offerTyped).toBeNull();
  });

  it("offers nothing when nothing was typed", () => {
    expect(filterModels(CATALOG, "").offerTyped).toBeNull();
  });
});
