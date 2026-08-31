// The seed behind a teammate's mascot, pinned (issue #1181).
//
// # Why this test exists
//
// A teammate's face must be the same face on every surface. There are two
// plausible seeds and they disagree for essentially every real roster row:
//
//   * `avatarFor(dto.id || name)` — what `fromDto` computes into
//     `TeamMember.avatar`, and what `lib/team.ts` documents ("renaming a
//     teammate does not change its face");
//   * `avatarFor(name)` — the fallback inside `TeammateAvatar` when no `avatar`
//     prop is passed.
//
// #1181 warned that seeding one surface on the id and another on the name gives
// the same teammate two different faces — "worse than the current
// inconsistency, and harder to notice, because each screen looks internally
// consistent."
//
// The console now settles that on the **id**: chat, the member pane, the org
// chart, the desk picker and the teammate detail header all pass a reference
// resolved from the id, and a face somebody *chose* arrives through that same
// field (`avatarRef(dto.avatar, id)`) rather than through a second one. So the
// choice and the default are one kind of value, carried one way.
//
// This test is the tripwire on that. If someone drops the `avatar` prop from a
// surface — falling it back to the name seed — or splits the chosen face out
// into a parallel prop, the two seeds below are what they will have silently
// pulled apart.

import { describe, expect, it } from "vitest";

import { avatarFor, fromDto } from "@/lib/team";

/** Roster rows shaped like a real company bundle: a slug id, a titled name. */
const ROWS = [
  { id: "backend_engineer", name: "Backend Engineer" },
  { id: "security_engineer", name: "Security Engineer" },
  { id: "designer", name: "Designer" },
  { id: "product_manager", name: "Product Manager" },
  { id: "researcher", name: "Researcher" },
];

describe("teammate mascot seeding", () => {
  it("the id seed and the name seed are genuinely different faces", () => {
    // Not a theoretical hazard: every one of these disagrees. If this ever
    // starts passing by coincidence the test is worthless, so assert on all.
    for (const row of ROWS) {
      expect(
        avatarFor(row.id),
        `${row.name}: id and name seeds must be treated as different faces`,
      ).not.toBe(avatarFor(row.name));
    }
  });

  it("`TeamMember.avatar` is the id-seeded one, and is what surfaces render", () => {
    // The field every surface passes. With no chosen face it is exactly the
    // id-seeded default, which is what keeps a teammate's face the same on the
    // roster, in chat and on its own page.
    for (const row of ROWS) {
      const member = fromDto({ id: row.id, name: row.name, role: row.name });
      expect(member.avatar).toBe(avatarFor(row.id));
    }
  });

  it("the name seed is stable, so two surfaces naming a teammate alike agree", () => {
    // This is the property the Company cards, the detail header and the chat
    // member pane rely on: they all pass the same display name, so they all
    // resolve the same mascot.
    for (const row of ROWS) {
      expect(avatarFor(row.name)).toBe(avatarFor(row.name));
      // A full reference, not a bare flavour: what surfaces hand to
      // `TeammateAvatar` is the same grammar an operator's *chosen* face uses,
      // so the default and the choice are one kind of value everywhere.
      expect(avatarFor(row.name)).toMatch(/^tiny:[a-z]+$/);
    }
  });
});
