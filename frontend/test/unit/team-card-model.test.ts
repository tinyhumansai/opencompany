import { describe, expect, it } from "vitest";

import type { TeamMemberDto } from "@/api/types";
import { fromDto, modelSummary } from "@/lib/team";

/**
 * What a roster card is allowed to say about the model a teammate runs on.
 *
 * The roster read sends `model`, `provider` and `harness` **only when the
 * teammate declares them**. An absent key is not "no model" — it is "this
 * teammate declares none and inherits", and every teammate resolves to some
 * model in the end. The card has neither the company default nor the harness
 * catalogue to resolve that with, and fetching either per card would be an
 * N+1 over a grid.
 *
 * So the one thing pinned hardest below is the negative: the inherited case
 * renders words, never a model name. A resolved-looking name that came from
 * nowhere is worse on screen than an honest "inherits", because nothing
 * downstream can tell the two apart.
 */

function dto(fields: Partial<TeamMemberDto> = {}): TeamMemberDto {
  return { id: "ada", role: "Engineer", ...fields } as TeamMemberDto;
}

describe("modelSummary", () => {
  it("renders a declared pair as provider · model", () => {
    const summary = modelSummary({ provider: "anthropic", model: "claude-opus-5" });
    expect(summary.label).toBe("anthropic · claude-opus-5");
    expect(summary.inherited).toBe(false);
  });

  it("renders a model with no provider on its own", () => {
    const summary = modelSummary({ model: "gpt-5-codex", harness: "laptop" });
    expect(summary.label).toBe("gpt-5-codex");
    expect(summary.inherited).toBe(false);
    expect(summary.harness).toBe("laptop");
  });

  it("says the teammate inherits rather than naming a model it was not sent", () => {
    const summary = modelSummary({});
    expect(summary.label).toBe("Inherits the default");
    expect(summary.inherited).toBe(true);
    expect(summary.harness).toBeUndefined();
  });

  it("says the same thing when a harness is pinned and a model is not", () => {
    // One phrase for every unpinned teammate: choosing between the editor's
    // "Company default" and "Whatever the harness defaults to" needs the
    // harness `kind`, and a guess from the id alone claims the wrong source
    // for a teammate pinned to the built-in harness.
    const summary = modelSummary({ harness: "laptop" });
    expect(summary.label).toBe("Inherits the default");
    expect(summary.inherited).toBe(true);
    // The harness is named as a fact of its own; the model is not invented
    // from it.
    expect(summary.harness).toBe("laptop");
  });

  it("never renders a blank, and never a bare dash", () => {
    for (const member of [{}, { harness: "laptop" }, { provider: "anthropic" }]) {
      const { label } = modelSummary(member);
      expect(label.trim()).not.toBe("");
      expect(label).not.toBe("—");
      // …and never a model name conjured out of an absent pin.
      expect(label).not.toMatch(/claude|gpt|gemini/i);
    }
  });

  it("treats a provider with no model as inherited, not as half a pin", () => {
    // The host sends the pair together; a lone provider is a host that could
    // not answer, and the model is still not something to name.
    expect(modelSummary({ provider: "anthropic" }).inherited).toBe(true);
    expect(modelSummary({ provider: "anthropic" }).label).toBe("Inherits the default");
  });
});

describe("fromDto", () => {
  it("carries the binding through untouched", () => {
    const member = fromDto(
      dto({ harness: "laptop", provider: "anthropic", model: "claude-opus-5" }),
    );
    expect(member.harness).toBe("laptop");
    expect(member.provider).toBe("anthropic");
    expect(member.model).toBe("claude-opus-5");
  });

  it("leaves an undeclared binding undefined rather than defaulting it", () => {
    const member = fromDto(dto());
    expect(member.harness).toBeUndefined();
    expect(member.provider).toBeUndefined();
    expect(member.model).toBeUndefined();
    expect(modelSummary(member).inherited).toBe(true);
  });
});
