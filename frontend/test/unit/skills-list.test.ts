import { describe, expect, it } from "vitest";

import {
  canEditSkill,
  canUninstallSkill,
  DEFAULT_SKILL_FILTERS,
  skillCategories,
  skillLastEditedLabel,
  skillSourceLabel,
  visibleSkills,
  type SkillListRow,
} from "@/lib/skills-list";

const NOW = 1_759_000_000_000;
const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

function row(over: Partial<SkillListRow> & { name: string }): SkillListRow {
  return {
    description: "",
    category: "Ops",
    source: "custom",
    enabled: true,
    updatedAtMillis: NOW,
    ...over,
  };
}

describe("skillSourceLabel", () => {
  it("names the three provenances the host serves", () => {
    expect(skillSourceLabel({ source: "company" })).toBe("Company");
    expect(skillSourceLabel({ source: "custom" })).toBe("Custom");
    expect(skillSourceLabel({ source: "registry" })).toBe("Registry");
  });

  it("carries the snapshotted revision on a registry install", () => {
    expect(skillSourceLabel({ source: "registry", version: "1.2" })).toBe("Registry v1.2");
  });

  it("does not double the v on an already-prefixed version", () => {
    expect(skillSourceLabel({ source: "registry", version: "v2.0.1" })).toBe("Registry v2.0.1");
  });

  // A version on a custom skill is its own frontmatter, not a library revision,
  // so showing it would imply a comparison against a library copy that does not
  // exist — the exact mislabel the host's install fix removed.
  it("never attaches a version to a company or custom skill", () => {
    expect(skillSourceLabel({ source: "custom", version: "9.9" })).toBe("Custom");
    expect(skillSourceLabel({ source: "company", version: "9.9" })).toBe("Company");
  });

  it("title-cases a source it does not recognise rather than rendering nothing", () => {
    expect(skillSourceLabel({ source: "marketplace" })).toBe("Marketplace");
    expect(skillSourceLabel({ source: "" })).toBe("Unknown");
  });

  // An upload response is folded into the list without a re-read, so a row can
  // arrive missing fields the type says are there. Costing that row its label
  // is fine; throwing inside render takes the whole tab down with it.
  it("survives a row the host served without a source", () => {
    expect(skillSourceLabel({ source: undefined as unknown as string })).toBe("Unknown");
  });
});

describe("canUninstallSkill / canEditSkill", () => {
  // Mirrors the host's uninstall arm, which admits Registry | Custom and
  // refuses everything else.
  it("allows uninstalling a registry install and a custom skill, never a company one", () => {
    expect(canUninstallSkill("registry")).toBe(true);
    expect(canUninstallSkill("custom")).toBe(true);
    expect(canUninstallSkill("company")).toBe(false);
  });

  it("offers Edit only for a skill the console authored", () => {
    expect(canEditSkill("custom")).toBe(true);
    expect(canEditSkill("registry")).toBe(false);
    expect(canEditSkill("company")).toBe(false);
  });
});

describe("skillLastEditedLabel", () => {
  it("says nothing was edited when the host reports no stamp", () => {
    expect(skillLastEditedLabel(null, NOW)).toBe("Never edited");
    expect(skillLastEditedLabel(undefined, NOW)).toBe("Never edited");
  });

  it("never renders a missing stamp as an epoch date", () => {
    expect(skillLastEditedLabel(undefined, NOW)).not.toContain("1970");
  });

  it("scales from seconds to a date", () => {
    expect(skillLastEditedLabel(NOW - 5_000, NOW)).toBe("Edited just now");
    expect(skillLastEditedLabel(NOW - MINUTE, NOW)).toBe("Edited 1 minute ago");
    expect(skillLastEditedLabel(NOW - 7 * MINUTE, NOW)).toBe("Edited 7 minutes ago");
    expect(skillLastEditedLabel(NOW - 3 * HOUR, NOW)).toBe("Edited 3 hours ago");
    expect(skillLastEditedLabel(NOW - 2 * DAY, NOW)).toBe("Edited 2 days ago");
    expect(skillLastEditedLabel(NOW - 400 * DAY, NOW, "en-US")).toMatch(/^Edited \w+ \d+, \d{4}$/);
  });

  // Host and browser clocks disagree by seconds routinely. "in -3 days" is a
  // bug report; "just now" is the truth to within the skew.
  it("reads a stamp from the future as just now", () => {
    expect(skillLastEditedLabel(NOW + 5 * MINUTE, NOW)).toBe("Edited just now");
  });
});

describe("skillCategories", () => {
  it("collects the categories actually present, sorted and deduplicated", () => {
    expect(
      skillCategories([
        row({ name: "a", category: "Ops" }),
        row({ name: "b", category: "Research" }),
        row({ name: "c", category: "Ops" }),
        row({ name: "d", category: "" }),
      ]),
    ).toEqual(["Ops", "Research"]);
  });
});

describe("visibleSkills", () => {
  const skills: SkillListRow[] = [
    row({ name: "Alpha", source: "company", enabled: true, updatedAtMillis: null }),
    row({ name: "Bravo", source: "registry", enabled: false, updatedAtMillis: NOW - DAY }),
    row({
      name: "Charlie",
      source: "custom",
      enabled: true,
      updatedAtMillis: NOW,
      category: "Research",
      description: "quarterly numbers",
    }),
  ];

  it("returns everything under the default filters", () => {
    expect(visibleSkills(skills, DEFAULT_SKILL_FILTERS, "name").map((s) => s.name)).toEqual([
      "Alpha",
      "Bravo",
      "Charlie",
    ]);
  });

  it("filters by source, enabled state and category independently", () => {
    const by = (over: Partial<typeof DEFAULT_SKILL_FILTERS>) =>
      visibleSkills(skills, { ...DEFAULT_SKILL_FILTERS, ...over }, "name").map((s) => s.name);

    expect(by({ source: "registry" })).toEqual(["Bravo"]);
    expect(by({ enabled: "disabled" })).toEqual(["Bravo"]);
    expect(by({ enabled: "enabled" })).toEqual(["Alpha", "Charlie"]);
    expect(by({ category: "Research" })).toEqual(["Charlie"]);
  });

  it("combines filters rather than replacing one with the next", () => {
    expect(
      visibleSkills(
        skills,
        { ...DEFAULT_SKILL_FILTERS, source: "registry", enabled: "enabled" },
        "name",
      ),
    ).toEqual([]);
  });

  it("matches the query against name and description", () => {
    const by = (query: string) =>
      visibleSkills(skills, { ...DEFAULT_SKILL_FILTERS, query }, "name").map((s) => s.name);

    expect(by("brav")).toEqual(["Bravo"]);
    expect(by("QUARTERLY")).toEqual(["Charlie"]);
    expect(by("  ")).toEqual(["Alpha", "Bravo", "Charlie"]);
  });

  // "Last edited" with the never-edited baseline on top would say the opposite
  // of what happened, and the baseline is the bulk of the list at the real cap.
  it("sorts newest edit first and sinks the never-edited rows to the bottom", () => {
    expect(visibleSkills(skills, DEFAULT_SKILL_FILTERS, "edited").map((s) => s.name)).toEqual([
      "Charlie",
      "Bravo",
      "Alpha",
    ]);
  });

  it("breaks an edited-time tie by name so rows do not swap between renders", () => {
    const tied = [
      row({ name: "Zulu", updatedAtMillis: NOW }),
      row({ name: "Kilo", updatedAtMillis: NOW }),
      row({ name: "Oscar", updatedAtMillis: null }),
      row({ name: "Echo", updatedAtMillis: null }),
    ];
    expect(visibleSkills(tied, DEFAULT_SKILL_FILTERS, "edited").map((s) => s.name)).toEqual([
      "Kilo",
      "Zulu",
      "Echo",
      "Oscar",
    ]);
  });

  it("filters and sorts a partial row instead of throwing on it", () => {
    const partial = [{ name: "Press Outreach" } as unknown as SkillListRow, ...skills];
    expect(() => visibleSkills(partial, DEFAULT_SKILL_FILTERS, "edited")).not.toThrow();
    expect(visibleSkills(partial, { ...DEFAULT_SKILL_FILTERS, query: "press" }, "name")).toHaveLength(
      1,
    );
    expect(skillCategories(partial)).toEqual(["Ops", "Research"]);
  });

  it("never mutates the array the view holds as state", () => {
    const held = [...skills];
    visibleSkills(held, DEFAULT_SKILL_FILTERS, "edited");
    expect(held.map((s) => s.name)).toEqual(["Alpha", "Bravo", "Charlie"]);
  });
});
