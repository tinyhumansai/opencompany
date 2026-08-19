import { describe, expect, it } from "vitest";

import type { Person as HostPerson } from "@/api/auth";
import type { Task } from "@/api/tasks";
import type { DeskDto } from "@/api/types";
import type { TeamMember } from "@/lib/team";
import { adapt, deskDepartments, deskOfMember, UNPLACED } from "@/views/overview/kg/adapter";
import { buildKnowledgeGraph, SELF_ID } from "@/views/overview/kg/model";
import { radialRestLayout } from "@/views/overview/kg/tree-layout";
import { ownedBy } from "@/views/overview/pulse";

/**
 * Ring 1 of the Overview graph (issue #486).
 *
 * The graph used to guess this ring: `assignDepartment` keyword-matched a role
 * string into one of five hardcoded buckets and fell back to Operations. It now
 * reads the company's desks — the one structure the company declares.
 *
 * Every failure this guards is silent on screen. A wrong ring 1 still draws a
 * clean five-ring sunburst; it just describes a company that does not exist:
 *
 * - a teammate placed by their job title rather than their desk looks correctly
 *   filed, and contradicts the org chart two clicks away;
 * - a human dealt onto a real desk asserts a membership that desk's own member
 *   list denies;
 * - a teammate on no desk given a department is the exact fabrication this
 *   issue removed, and it is invisible once drawn;
 * - workflows keyed to the deleted `dept-*` ids silently draw nothing, emptying
 *   two of the five rings with no error anywhere.
 */

function member(id: string, role: string, name = id): TeamMember {
  // `effectiveTools` and `desks` are what the roster read now carries for every
  // teammate (issue #601). Empty here on purpose: these cases are about ring 1,
  // and a teammate holding no grants must still be placed by their desk.
  return {
    id,
    name,
    role,
    description: "",
    tone: "a",
    inboxEnabled: false,
    effectiveTools: [],
    desks: [],
  };
}

/**
 * Deliberately adversarial: every teammate's role points at a DIFFERENT
 * department than the desk that seats them, so a test can only pass by reading
 * the desk. `hedy` is a "Designer" seated on Engineering; `ada` is a "Backend
 * Engineer" seated on Front of house.
 */
const ROSTER: TeamMember[] = [
  member("hedy", "Designer"),
  member("ada", "Backend Engineer"),
  member("grace", "Growth Marketer"),
];

const ENGINEERING: DeskDto = {
  id: "engineering",
  name: "Engineering",
  description: "Ships the product",
  members: ["hedy"],
};

const FRONT_OF_HOUSE: DeskDto = {
  id: "front-of-house",
  name: "Front of house",
  members: ["ada"],
};

const DESKS = [ENGINEERING, FRONT_OF_HOUSE];

const BASE = {
  tasks: [] as Task[],
  skills: [],
  servers: [],
  toolsByServer: {},
  people: [] as HostPerson[],
  // The company's saved flows (issue #601). None by default: these cases are
  // about ring 1, and a company with no flows must still draw its desks.
  workflows: [],
  ownedBy,
};

function person(id: string, role: HostPerson["role"] = "member"): HostPerson {
  return {
    id,
    email: `${id}@example.com`,
    role,
    status: "active",
    hasPassword: true,
    mustChangePassword: false,
    createdAtMillis: 0,
  } as HostPerson;
}

describe("ring 1 is the company's desks", () => {
  it("names the pillars after the desks, in the host's order", () => {
    // Served deliberately out of alphabetical order: the host's order is the
    // hierarchy (`lib/org.ts` reads seats the same way), so a re-sort here
    // would silently reorder the wheel. Sorted by name this would read
    // Engineering first.
    const depts = deskDepartments([FRONT_OF_HOUSE, ENGINEERING]);
    expect(depts.map((d) => d.name)).toEqual(["Front of house", "Engineering"]);
    // The order field is what puts a pillar on the wheel.
    expect(depts.map((d) => d.order)).toEqual([0, 1]);
    expect(depts[1].tagline).toBe("Ships the product");
  });

  it("namespaces desk ids so a desk cannot collide with the unplaced sentinel", () => {
    const [dept] = deskDepartments([{ id: UNPLACED, name: "Unplaced", members: [] }]);
    expect(dept.id).not.toBe(UNPLACED);
    expect(dept.id).toBe(`desk:${UNPLACED}`);
  });

  it("seats a teammate by their desk, not by their job title", () => {
    // The whole point: hedy is a Designer on the Engineering desk. The old
    // keyword table put her in Design on the strength of the word "Designer".
    const { agents, departments } = adapt({ ...BASE, members: ROSTER, desks: DESKS });
    const hedy = agents.find((a) => a.id === "hedy")!;
    expect(hedy.departmentId).toBe("desk:engineering");
    expect(departments.find((d) => d.id === hedy.departmentId)!.name).toBe("Engineering");

    const ada = agents.find((a) => a.id === "ada")!;
    expect(ada.departmentId).toBe("desk:front-of-house");
  });

  it("gives a teammate on several desks the first one, and never two", () => {
    const both: DeskDto = { id: "both", name: "Both", members: ["hedy"] };
    // Host order decides: engineering is served first, so engineering wins.
    expect(deskOfMember("hedy", [ENGINEERING, both])).toBe("desk:engineering");
    expect(deskOfMember("hedy", [both, ENGINEERING])).toBe("desk:both");
  });

  it("draws no pillar for a desk nobody is on", () => {
    const empty: DeskDto = { id: "empty", name: "Empty desk", members: [] };
    const { departments } = adapt({ ...BASE, members: ROSTER, desks: [...DESKS, empty] });
    expect(departments.map((d) => d.id)).not.toContain("desk:empty");
  });
});

describe("nobody is given a position the company did not declare", () => {
  it("leaves a teammate on no desk unplaced rather than in Operations", () => {
    // grace is on neither desk. The old code read "Growth Marketer" and filed
    // her under Growth; a role matching nothing at all fell back to Operations.
    const { agents, departments } = adapt({ ...BASE, members: ROSTER, desks: DESKS });
    const grace = agents.find((a) => a.id === "grace")!;
    expect(grace.departmentId).toBe(UNPLACED);
    // And no pillar was conjured to hold her.
    expect(departments.map((d) => d.id)).not.toContain(UNPLACED);
  });

  it("leaves every human unplaced — desks staff agents", () => {
    const { people } = adapt({
      ...BASE,
      members: ROSTER,
      desks: DESKS,
      people: [person("root", "admin"), person("member")],
    });
    expect(people).toHaveLength(2);
    for (const p of people) expect(p.departmentId).toBe(UNPLACED);
  });

  it("drops an unplaced teammate's card rather than parking it on a desk", () => {
    const card: Task = {
      id: "t1",
      title: "Write the launch post",
      column: "doing",
      priority: "high",
      assignee: "grace",
      updatedAt: 0,
    };
    const { tasks } = adapt({ ...BASE, members: ROSTER, desks: DESKS, tasks: [card] });
    expect(tasks).toHaveLength(0);

    // Seat her, and the same card comes back under that desk — proving it is
    // the missing desk that drops it, not the card.
    const seated = adapt({
      ...BASE,
      members: ROSTER,
      desks: [...DESKS, { id: "growth", name: "Growth", members: ["grace"] }],
      tasks: [card],
    });
    expect(seated.tasks.map((t) => t.departmentId)).toEqual(["desk:growth"]);
  });

  it("draws no pillars at all for a company that declares no desks", () => {
    const { departments, agents, workflows } = adapt({ ...BASE, members: ROSTER, desks: [] });
    expect(departments).toEqual([]);
    expect(workflows).toEqual([]);
    for (const a of agents) expect(a.departmentId).toBe(UNPLACED);
  });
});

describe("the workflow ring survives desks replacing the hardcoded departments", () => {
  // This case used to assert "hangs one routine off every drawn desk": one
  // templated routine was dealt to each desk by position, so the count of flows
  // was the count of desks and every flow carried an `agentIds` list. Issue #601
  // deleted the templates — a workflow is one of the company's saved graphs now,
  // so a company with no saved flows draws no flow ring, and `agentIds` is gone
  // with the round-robin that consumed it.
  //
  // The concern the case was written for still holds and is what is checked
  // here: when ring 1 changed under it, the flow ring must not silently empty.
  it("hangs a saved flow off the desk of the teammate it runs through", () => {
    const { workflows, departments } = adapt({
      ...BASE,
      members: ROSTER,
      desks: DESKS,
      workflows: [
        {
          id: "wf-nightly",
          name: "Nightly digest",
          version: null,
          // `hedy` is seated on Engineering by the desk, not by her job title.
          nodes: [
            { id: "t", kind: "trigger", name: "22:00 UTC" },
            { id: "w", kind: "agent", name: "Write it", agent: "hedy" },
          ],
          edges: [{ from: "t", to: "w" }],
        },
      ],
    });

    expect(workflows).toHaveLength(1);
    expect(workflows[0].departmentId).toBe("desk:engineering");
    // And it landed on a desk that is actually drawn, so the flow is reachable
    // on the wheel rather than pointing at a pillar nobody built.
    expect(departments.map((d) => d.id)).toContain(workflows[0].departmentId);
  });
});

describe("the graph model places the unplaced without inventing a pillar", () => {
  it("hangs an unplaced worker off the core, not off a team", () => {
    const { agents, departments, people } = adapt({
      ...BASE,
      members: ROSTER,
      desks: DESKS,
      people: [person("root", "admin")],
    });
    const graph = buildKnowledgeGraph(agents, departments, people);

    // No team node was created for the sentinel.
    expect(graph.nodes.filter((n) => n.kind === "team").map((n) => n.id)).toEqual([
      "team:desk:engineering",
      "team:desk:front-of-house",
    ]);

    // grace and the human are on the graph — visible, not absent — and their
    // one structural edge points at the company itself.
    for (const nodeId of ["emp:grace", "person:root"]) {
      expect(graph.nodes.some((n) => n.id === nodeId)).toBe(true);
      const edges = graph.edges.filter((e) => e.source === nodeId);
      expect(edges).toEqual([{ source: nodeId, target: SELF_ID, kind: "member" }]);
    }

    // A seated teammate still reaches their own desk, not the core.
    expect(graph.edges).toContainEqual({
      source: "emp:hedy",
      target: "team:desk:engineering",
      kind: "member",
    });
  });
});

describe("the rest layout gives the unplaced a sector of their own", () => {
  const pillars = [
    { teamId: "team:a", taskIds: [], workerIds: ["emp:x"], toolIds: [] },
    { teamId: "team:b", taskIds: [], workerIds: ["emp:y"], toolIds: [] },
  ];
  const ringR = [0, 100, 150, 200, 250];
  const at = (id: string, positions: Map<string, { x: number; y: number }>) => {
    const p = positions.get(id)!;
    return Math.round(Math.hypot(p.x - 500, p.y - 300));
  };

  it("rests them on the worker ring with no pillar above them", () => {
    const { positions } = radialRestLayout({
      selfId: "self",
      pillars,
      unplacedIds: ["emp:lost", "person:root"],
      ringR,
      cx: 500,
      cy: 300,
    });
    // On ring 3, the worker ring — the same radius as a seated worker.
    expect(at("emp:lost", positions)).toBe(ringR[3]);
    expect(at("person:root", positions)).toBe(ringR[3]);
    // Their sector has nothing at ring 1: no pillar was placed for them.
    const ring1 = [...positions.entries()].filter(
      ([, p]) => Math.round(Math.hypot(p.x - 500, p.y - 300)) === ringR[1],
    );
    expect(ring1.map(([id]) => id).sort()).toEqual(["team:a", "team:b"]);
  });

  it("does not move the pillars when there is nobody unplaced", () => {
    const withNone = radialRestLayout({ selfId: "self", pillars, ringR, cx: 500, cy: 300 });
    const withEmpty = radialRestLayout({
      selfId: "self",
      pillars,
      unplacedIds: [],
      ringR,
      cx: 500,
      cy: 300,
    });
    expect(withEmpty.positions.get("team:a")).toEqual(withNone.positions.get("team:a"));
    expect(withEmpty.positions.get("team:b")).toEqual(withNone.positions.get("team:b"));
  });

  it("separates the unplaced sector from every pillar's wedge", () => {
    const { positions } = radialRestLayout({
      selfId: "self",
      pillars,
      unplacedIds: ["emp:lost"],
      ringR,
      cx: 500,
      cy: 300,
    });
    // Not sitting on top of either desk's worker.
    for (const seated of ["emp:x", "emp:y"]) {
      const a = positions.get("emp:lost")!;
      const b = positions.get(seated)!;
      expect(Math.hypot(a.x - b.x, a.y - b.y)).toBeGreaterThan(20);
    }
  });
});
