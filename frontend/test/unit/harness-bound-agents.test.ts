import { describe, expect, it } from "vitest";

import { boundAgents, boundLabel } from "@/lib/harnesses";

/**
 * Which teammates a harness actually serves, resolved on the client from the
 * roster read's `harness` field.
 *
 * The rule has to match the host's own (`harness/lanes.rs`'s `agents_on`),
 * because the number this produces is a claim about what the runtime will do:
 * a teammate that declares nothing is served by the **default** harness, and by
 * no other. Counting it nowhere would under-report the default; counting it
 * everywhere would report a binding nobody made.
 */

function member(id: string, harness?: string) {
  return harness === undefined ? { id } : { id, harness };
}

describe("boundAgents", () => {
  it("counts a teammate that declares nothing toward the default harness", () => {
    const roster = [member("ceo"), member("writer", "laptop")];
    expect(boundAgents(roster, "main", "main").map((m) => m.id)).toEqual(["ceo"]);
  });

  it("counts that same teammate toward no other harness", () => {
    const roster = [member("ceo"), member("writer", "laptop")];
    expect(boundAgents(roster, "laptop", "main").map((m) => m.id)).toEqual(["writer"]);
  });

  it("resolves an explicit binding to the default harness like any other", () => {
    // An agent that names the default explicitly is bound to it exactly as one
    // that named nothing is — the distinction is on the wire, not in the lane.
    const roster = [member("ceo", "main"), member("writer")];
    expect(boundAgents(roster, "main", "main").map((m) => m.id)).toEqual(["ceo", "writer"]);
  });

  it("resolves an overlay teammate's binding, which arrives on the same field", () => {
    const roster = [member("ceo"), member("jamie", "laptop")];
    expect(boundAgents(roster, "laptop", "main").map((m) => m.id)).toEqual(["jamie"]);
  });

  it("binds an undeclared teammate nowhere when the company names no default", () => {
    // A host that sends no `default = true` harness cannot say where an
    // undeclared teammate lands, and guessing a row would be inventing one.
    const roster = [member("ceo"), member("writer", "laptop")];
    expect(boundAgents(roster, "laptop", undefined).map((m) => m.id)).toEqual(["writer"]);
    expect(boundAgents(roster, "main", undefined)).toEqual([]);
  });
});

describe("boundLabel", () => {
  it("says bound rather than connected, because nothing here is connected", () => {
    // "Connected" is the Providers page's own word for a credential this
    // company holds. A harness holds nothing: some number of teammates
    // individually picked it, and borrowing the word would imply a shared
    // persisted state that does not exist.
    for (const count of [0, 1, 2, 17]) {
      expect(boundLabel(count)).not.toMatch(/connect/i);
    }
  });

  it("counts in whole teammates, singular at one", () => {
    expect(boundLabel(0)).toBe("No agents bound");
    expect(boundLabel(1)).toBe("1 agent bound");
    expect(boundLabel(2)).toBe("2 agents bound");
  });
});
