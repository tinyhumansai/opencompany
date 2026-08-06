import { describe, expect, it } from "vitest";

import {
  addableTo,
  buildOrgTree,
  depthOf,
  MAX_DEPTH,
  reorderedIds,
  summarize,
} from "@/lib/org";
import type { DeskDto, TeamMemberDto } from "@/api/types";

/**
 * The org chart's model (issue #311).
 *
 * Everything here is derived from data the host already serves — #311 settles
 * that the tree is "a new reader over existing data, not a data change" — so
 * the failure modes are all *derivation* failures, and every one of them is
 * silent on screen:
 *
 * - a lead read by sorting instead of by position badges the wrong teammate,
 *   and the badge still looks right;
 * - a blueprint member rendered as removable offers a control the host always
 *   refuses;
 * - a seat pointing at nobody vanishes, and the operator never learns the desk
 *   is broken;
 * - a teammate on no desk disappears from a chart that claims to be the org.
 */

const ROSTER: TeamMemberDto[] = [
  { id: "ada", name: "Ada", role: "Backend Engineer" },
  { id: "grace", name: "Grace", role: "Frontend Engineer" },
  { id: "linus", name: "Linus", role: "QA Engineer" },
  { id: "hedy", name: "Hedy", role: "Designer" },
];

/** Lead first and deliberately NOT in roster order — see the lead test. */
const ENGINEERING: DeskDto = {
  id: "engineering",
  name: "Engineering",
  description: "Ships the product",
  members: ["grace", "ada"],
};

/** An operator-created desk with one blueprint founder and one added member. */
const GROWTH: DeskDto = {
  id: "growth",
  name: "Growth",
  members: ["linus", "hedy"],
  overlayMembers: ["hedy"],
  overlayCreated: true,
};

const tree = () => buildOrgTree("Acme", [ENGINEERING, GROWTH], ROSTER);

describe("buildOrgTree", () => {
  it("reads the lead from position, not from any ordering of its own", () => {
    // `members[0]` is the host's routing target. Grace is second on the roster
    // and first on the desk, so anything that sorted or matched by name here
    // would crown Ada.
    const engineering = tree().desks[0];
    expect(engineering.seats.map((s) => s.id)).toEqual(["grace", "ada"]);
    expect(engineering.seats[0].lead).toBe(true);
    expect(engineering.seats[1].lead).toBe(false);
  });

  it("gives an empty desk no lead at all rather than crowning nobody", () => {
    const empty = buildOrgTree("Acme", [{ id: "d", name: "D", members: [] }], ROSTER);
    expect(empty.desks[0].seats).toEqual([]);
  });

  it("marks desk provenance, because only an overlay desk can be deleted", () => {
    const [engineering, growth] = tree().desks;
    expect(engineering.provenance).toBe("blueprint");
    expect(growth.provenance).toBe("overlay");
  });

  it("marks seat provenance, because only an overlay member can be removed", () => {
    // Growth was operator-created with `linus` as a founder and gained `hedy`
    // afterwards, so the two rows carry different provenance on the same desk.
    const growth = tree().desks[1];
    expect(growth.seats.map((s) => [s.id, s.provenance])).toEqual([
      ["linus", "blueprint"],
      ["hedy", "overlay"],
    ]);
  });

  it("treats an omitted overlayMembers as none rather than as undefined", () => {
    // The host omits the field instead of sending `[]`, so a reader that
    // trusted it would throw or mark every seat overlay.
    expect(tree().desks[0].seats.every((s) => s.provenance === "blueprint")).toBe(true);
  });

  it("keeps a seat whose id is on no roster, flagged rather than dropped", () => {
    const ghosted = buildOrgTree(
      "Acme",
      [{ id: "engineering", name: "Engineering", members: ["ada", "nobody"] }],
      ROSTER,
    );
    const seats = ghosted.desks[0].seats;
    expect(seats.map((s) => s.id)).toEqual(["ada", "nobody"]);
    expect(seats[1].known).toBe(false);
    // It falls back to the raw id, which is the only true thing left to show.
    expect(seats[1].name).toBe("nobody");
    expect(seats[0].known).toBe(true);
  });

  it("still draws the structure when the roster fails to load", () => {
    // `/team` is best-effort. Losing it must cost names, not the whole chart.
    const nameless = buildOrgTree("Acme", [ENGINEERING], []);
    expect(nameless.desks[0].seats.map((s) => s.name)).toEqual(["grace", "ada"]);
    expect(nameless.desks[0].seats.every((s) => !s.known)).toBe(true);
  });

  it("lists teammates on no desk instead of losing them", () => {
    const partial = buildOrgTree("Acme", [ENGINEERING], ROSTER);
    expect(partial.unassigned.map((m) => m.id)).toEqual(["linus", "hedy"]);
    // And nobody is listed twice: a seated teammate is not also unassigned.
    expect(tree().unassigned).toEqual([]);
  });

  it("passes the company name through verbatim; the caller owns the fallback", () => {
    // `OrgChartView` resolves `status?.name || company || "This company"` and
    // hands the result down. The model must not second-guess that, or two
    // places would decide what an unnamed company is called.
    expect(buildOrgTree("Acme", [], []).companyName).toBe("Acme");
    expect(buildOrgTree("", [], []).companyName).toBe("");
  });
});

describe("the three-level cap", () => {
  it("is three levels: company, desk, seat", () => {
    expect(MAX_DEPTH).toBe(3);
    expect(depthOf(tree())).toBe(3);
  });

  it("cannot be exceeded, because a desk has no child desk to descend into", () => {
    // The structural claim, asserted rather than asserted-in-a-comment: walk
    // every node of a real tree and confirm nothing below a seat exists to
    // recurse into. A parent pointer on a desk is what would break this, and
    // that is the data change #311 rules out.
    const built = tree();
    for (const desk of built.desks) {
      for (const seat of desk.seats) {
        expect(Object.keys(seat)).not.toContain("seats");
        expect(Object.keys(seat)).not.toContain("children");
      }
      expect(Object.keys(desk)).not.toContain("parentId");
    }
    expect(depthOf(built)).toBeLessThanOrEqual(MAX_DEPTH);
  });

  it("is 1 with no desks and 2 when every desk is empty", () => {
    expect(depthOf(buildOrgTree("Acme", [], ROSTER))).toBe(1);
    expect(depthOf(buildOrgTree("Acme", [{ id: "d", name: "D", members: [] }], ROSTER))).toBe(2);
  });
});

describe("addableTo", () => {
  it("offers only teammates not already on that desk", () => {
    const built = tree();
    expect(addableTo(built, built.desks[0]).map((m) => m.id)).toEqual(["linus", "hedy"]);
    expect(addableTo(built, built.desks[1]).map((m) => m.id)).toEqual(["ada", "grace"]);
  });

  it("offers nobody when the desk already holds the whole roster", () => {
    const full = buildOrgTree(
      "Acme",
      [{ id: "all", name: "All", members: ROSTER.map((r) => r.id) }],
      ROSTER,
    );
    expect(addableTo(full, full.desks[0])).toEqual([]);
  });
});

describe("reorderedIds", () => {
  it("swaps one place and returns the whole order, which is what the host takes", () => {
    const engineering = tree().desks[0];
    // Moving the second seat up IS the change-of-lead: `members[0]` is the lead
    // and there is no separate call.
    expect(reorderedIds(engineering, 1, "up")).toEqual(["ada", "grace"]);
    expect(reorderedIds(engineering, 0, "down")).toEqual(["ada", "grace"]);
  });

  it("refuses a move that would run off either end", () => {
    const engineering = tree().desks[0];
    expect(reorderedIds(engineering, 0, "up")).toBeNull();
    expect(reorderedIds(engineering, 1, "down")).toBeNull();
  });

  it("refuses an index that is not a seat", () => {
    const engineering = tree().desks[0];
    expect(reorderedIds(engineering, -1, "down")).toBeNull();
    expect(reorderedIds(engineering, 9, "up")).toBeNull();
  });

  it("does not mutate the desk it was asked about", () => {
    const engineering = tree().desks[0];
    reorderedIds(engineering, 1, "up");
    expect(engineering.seats.map((s) => s.id)).toEqual(["grace", "ada"]);
  });
});

describe("summarize", () => {
  it("counts desks and seats, singular when there is one", () => {
    expect(summarize(tree())).toBe("2 desks · 4 seats");
    expect(summarize(buildOrgTree("Acme", [], ROSTER))).toBe("0 desks · 0 seats");
    expect(
      summarize(buildOrgTree("Acme", [{ id: "d", name: "D", members: ["ada"] }], ROSTER)),
    ).toBe("1 desk · 1 seat");
  });
});
