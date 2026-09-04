// The Add-teammate dialog's branch, and the derivation the reduced one depends
// on (issue #1989).
//
// The branch is the thing this redesign lives or dies on, and its failure is
// silent in one direction: answer `form` on a company whose copilot works and
// the dialog looks exactly as it did before, so nothing anywhere reports that
// the reduction never shipped. A rendered test can only prove the cases
// somebody thought to render; the decision is a pure function so every input
// can be.

import { describe, expect, it } from "vitest";

import type { CognitionPath } from "@/api/inference";
import {
  addTeammateSurface,
  describedTeammateFields,
  roleFromDescription,
} from "@/lib/team-add-surface";

/** Every cognition path the host can report, so a new one is never silently untested. */
const PATHS: CognitionPath[] = ["harness", "hosted", "sidecar", "echo", "custom", "test"];

describe("addTeammateSurface", () => {
  it("shows the reduced dialog for every cognition path that is not the offline brain", () => {
    for (const cognition of PATHS.filter((p) => p !== "echo")) {
      expect(
        addTeammateSurface({ cognition, roleUnderivable: false }),
        `cognition=${cognition} can draft, so the dialog must be the reduced one`,
      ).toBe("describe");
    }
  });

  it("shows the reduced dialog while the cognition read has not landed", () => {
    // `null` is both "in flight" and "this host has no /inference route". Issue
    // #753 leaves the copilot ENABLED in that case rather than refusing because
    // it could not confirm, and this follows it: guessing `describe` wrong is
    // corrected out loud on the detail page, where guessing `form` wrong is
    // corrected by nothing at all.
    expect(addTeammateSurface({ cognition: null, roleUnderivable: false })).toBe("describe");
  });

  it("shows the full form on the offline brain", () => {
    // The operator's decision on #1988, applied here: the can't-draft path keeps
    // today's form, so a company with no model is never locked out of writing a
    // description that nothing downstream could draft for it.
    expect(addTeammateSurface({ cognition: "echo", roleUnderivable: false })).toBe("form");
  });

  it("hands over the full form once the sentence yielded no role", () => {
    // The reduced dialog's one dead end. A blank role must never be written, so
    // the operator gets every field rather than a Create that cannot work.
    expect(addTeammateSurface({ cognition: "harness", roleUnderivable: true })).toBe("form");
  });

  it("keeps the full form on the offline brain even before any Create", () => {
    // Both reasons at once must not cancel out.
    expect(addTeammateSurface({ cognition: "echo", roleUnderivable: true })).toBe("form");
  });
});

describe("roleFromDescription", () => {
  it("takes the first clause, which is where a job description says what the job is", () => {
    expect(roleFromDescription("Runs paid acquisition and reports on ROAS.")).toBe(
      "Runs paid acquisition and reports on ROAS",
    );
    expect(roleFromDescription("Runs paid acquisition, and reports on ROAS.")).toBe(
      "Runs paid acquisition",
    );
    expect(roleFromDescription("Writes the weekly digest; emails it on Monday.")).toBe(
      "Writes the weekly digest",
    );
  });

  it("capitalises and collapses whitespace", () => {
    expect(roleFromDescription("  growth   marketer  ")).toBe("Growth marketer");
    expect(roleFromDescription("owns\nthe backlog")).toBe("Owns");
  });

  it("caps a rambling clause rather than minting a 400-character job title", () => {
    const long = `${"a".repeat(200)}`;
    const role = roleFromDescription(long);
    // 60 characters plus the ellipsis that says it was cut.
    expect(role).toHaveLength(61);
    expect(role.endsWith("…")).toBe(true);
    expect(role.startsWith("A")).toBe(true);
  });

  it("answers empty when the sentence has nothing usable", () => {
    // The caller MUST treat this as "no role derived" and hand over the form. A
    // blank role breaks the teammate's own system prompt (`persona_prompt`
    // interpolates it unguarded), empties the orchestrator's Team block, and
    // switches off the copilot on the very page the create redirects to.
    expect(roleFromDescription("")).toBe("");
    expect(roleFromDescription("   ")).toBe("");
    expect(roleFromDescription(".,;")).toBe("");
    expect(roleFromDescription("\n\n")).toBe("");
  });
});

describe("describedTeammateFields", () => {
  it("derives the role and trims what the operator typed", () => {
    expect(
      describedTeammateFields({
        name: "  Nova  ",
        description: "  Runs paid acquisition, reports on ROAS.  ",
      }),
    ).toEqual({
      name: "Nova",
      role: "Runs paid acquisition",
      description: "Runs paid acquisition, reports on ROAS.",
    });
  });

  it("refuses a teammate with no name", () => {
    // The id is slugged from the name host-side (`mint_agent_id`), and nothing
    // in this path can derive one: `DraftableField` excludes `name` on purpose,
    // so there is no model to ask.
    expect(
      describedTeammateFields({ name: "   ", description: "Runs paid acquisition." }),
    ).toBeNull();
  });

  it("refuses a teammate with no description", () => {
    expect(describedTeammateFields({ name: "Nova", description: "  " })).toBeNull();
  });

  it("refuses a teammate whose description yields no role", () => {
    // The case the hand-over exists for: a real name, a non-empty description,
    // and nothing in it that survives the clause split.
    expect(describedTeammateFields({ name: "Nova", description: "🎉🎉" })).not.toBeNull();
    expect(describedTeammateFields({ name: "Nova", description: "..." })).toBeNull();
    expect(describedTeammateFields({ name: "Nova", description: "!?!" })).toBeNull();
  });

  it("never returns a blank role", () => {
    // The invariant the whole module exists to hold. Anything that comes back
    // non-null is safe to POST.
    for (const description of ["Nova", "a", "🎉", "Runs ads.", "  x  ", "...", "", "!?"]) {
      const fields = describedTeammateFields({ name: "Nova", description });
      if (fields) expect(fields.role.trim()).not.toBe("");
    }
  });
});
