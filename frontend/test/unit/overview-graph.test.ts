import { describe, expect, it } from "vitest";

import type { Person as HostPerson } from "@/api/auth";
import type { Task } from "@/api/tasks";
import type { WorkflowGraph } from "@/api/workflows";
import type { DeskDto } from "@/api/types";
import type { TeamMember } from "@/lib/team";
import {
  adapt,
  type AdaptInput,
  DERIVED_NOTICE,
  orderStages,
  UNPLACED,
} from "@/views/overview/kg/adapter";
import { buildKnowledgeGraph, SELF_ID } from "@/views/overview/kg/model";
import { ownedBy } from "@/views/overview/pulse";

/**
 * The outer rings of the overview graph, drawn from the host rather than from a
 * plausible story (issue #601).
 *
 * Ring 1 is covered by `overview-ring1.test.ts`; this file is about the three
 * rings that were still invented after it: a teammate's tools, the company's
 * workflows, and which teammate performs a stage.
 *
 * Every assertion here exists because the honest value and the invented one
 * were indistinguishable on screen. A tool shelf dealt out of the company-wide
 * allow-list and one templated routine per desk drew a clean, convincing wheel;
 * it just described a company that does not exist, which is why it stayed wrong
 * long enough to need an issue.
 */

function member(over: Partial<TeamMember> & Pick<TeamMember, "id" | "name">): TeamMember {
  return {
    role: "Engineer",
    description: "",
    tone: "sky",
    avatar: "green",
    inboxEnabled: false,
    effectiveTools: [],
    desks: [],
    ...over,
  };
}

function desk(id: string, name: string, members: string[] = []): DeskDto {
  return { id, name, members };
}

function flow(over: Partial<WorkflowGraph> & Pick<WorkflowGraph, "id" | "name">): WorkflowGraph {
  return { version: null, nodes: [], edges: [], ...over };
}

function person(id: string, email: string, role: "admin" | "member"): HostPerson {
  return {
    id,
    email,
    role,
    status: "active",
    hasPassword: true,
    mustChangePassword: false,
    createdAtMillis: 0,
  };
}

const BASE = { tasks: [], people: [] as HostPerson[], workflows: [], ownedBy };

describe("adapt", () => {
  it("returns byte-identical graph data for repeated calls with one host snapshot", () => {
    // The order is deliberately not alphabetical. It is the order the host
    // served, and the graph must retain it rather than sorting its inputs to
    // manufacture stability.
    const input: AdaptInput = {
      members: [
        member({
          id: "grace",
          name: "Grace",
          effectiveTools: ["workspace.read", "mcp:calendar"],
        }),
        member({ id: "ada", name: "Ada", effectiveTools: ["composio"] }),
      ],
      desks: [desk("go-to-market", "Go to market", ["grace"]), desk("eng", "Engineering", ["ada"])],
      tasks: [
        {
          id: "launch",
          title: "Launch the campaign",
          note: "Publish after approval.",
          column: "working",
          priority: "high",
          assignee: "grace",
          updatedAt: 1,
        } satisfies Task,
      ],
      people: [{ ...person("operator", "operator@example.com", "admin"), displayName: "Operator" }],
      workflows: [
        flow({
          id: "digest",
          name: "Daily digest",
          description: "Summarise the day.",
          nodes: [
            { id: "send", kind: "output", name: "Send" },
            { id: "start", kind: "trigger", name: "Start" },
            { id: "write", kind: "agent", name: "Write", agent: "ada" },
          ],
          edges: [
            { from: "start", to: "write" },
            { from: "write", to: "send" },
          ],
        }),
      ],
      ownedBy,
    };
    const snapshot = JSON.stringify(input);
    const outputs = Array.from({ length: 3 }, () => JSON.stringify(adapt(input)));

    // `adapt` is the graph's pure host-data boundary. If a render changes the
    // serialized graph without receiving a new host snapshot, the wheel can
    // jump even though the company did not.
    expect(outputs).toEqual([outputs[0], outputs[0], outputs[0]]);
    expect(JSON.stringify(input)).toBe(snapshot);
  });
});

describe("a teammate's tools", () => {
  it("are the grants the host resolved, verbatim", () => {
    const { agents } = adapt({
      ...BASE,
      members: [
        member({ id: "ada", name: "Ada", effectiveTools: ["workspace.read", "composio"] }),
      ],
      desks: [desk("eng", "Engineering", ["ada"])],
    });

    // Not a slice, not a deal, not a reordering: the same list, in the same
    // order, that `GET .../team/{id}` shows on the agent's own card.
    expect(agents[0].tools).toEqual(["workspace.read", "composio"]);
  });

  it("are empty when the host granted none, rather than filled from somewhere", () => {
    const { agents, toolLabels } = adapt({
      ...BASE,
      members: [member({ id: "ada", name: "Ada", effectiveTools: [] })],
      desks: [],
    });

    expect(agents[0].tools).toEqual([]);
    expect(toolLabels).toEqual({});
  });

  it("label themselves with the literal grant, so an operator can grep for it", () => {
    const { toolLabels } = adapt({
      ...BASE,
      members: [member({ id: "ada", name: "Ada", effectiveTools: ["workspace.*", "*"] })],
      desks: [],
    });

    expect(toolLabels).toEqual({ "workspace.*": "workspace.*", "*": "*" });
  });

  it("reach the graph as the grant glob, not a title-cased rewrite of it", () => {
    const { agents, departments } = adapt({
      ...BASE,
      members: [member({ id: "ada", name: "Ada", effectiveTools: ["mcp:*", "workspace.read"] })],
      desks: [desk("eng", "Engineering", ["ada"])],
    });
    const graph = buildKnowledgeGraph(agents, departments);

    // The pre-fix UI showed things like "Mcp Deepwiki Read Wiki Contents"; the
    // truth is `mcp:*`, and it is the string in the company's `[tools] allow`.
    const labels = graph.nodes.filter((n) => n.kind === "tool").map((n) => n.label);
    expect(labels.sort()).toEqual(["mcp:*", "workspace.read"]);
  });

  it("never reach a human — the host resolves grants per agent", () => {
    const { people } = adapt({
      ...BASE,
      members: [member({ id: "ada", name: "Ada", effectiveTools: ["composio"] })],
      desks: [],
      people: [person("u1", "sam@example.com", "admin")],
    });

    expect(people[0].tools).toEqual([]);
  });
});

describe("orderStages", () => {
  it("reads the flow in edge order, not declaration order", () => {
    const stages = orderStages({
      nodes: [
        { id: "c", kind: "output", name: "Report" },
        { id: "a", kind: "trigger", name: "Every morning" },
        { id: "b", kind: "agent", name: "Draft", agent: "ada" },
      ],
      edges: [
        { from: "a", to: "b" },
        { from: "b", to: "c" },
      ],
    });

    expect(stages.map((s) => s.name)).toEqual(["Every morning", "Draft", "Report"]);
  });

  it("names the agent the flow itself names, and nobody else", () => {
    const stages = orderStages({
      nodes: [
        { id: "a", kind: "trigger", name: "Every morning" },
        { id: "b", kind: "agent", name: "Draft", agent: "ada" },
        // An `agent` field on a non-agent node is not a performer.
        { id: "c", kind: "http_request", name: "Post it", agent: "grace" },
      ],
      edges: [
        { from: "a", to: "b" },
        { from: "b", to: "c" },
      ],
    });

    expect(stages.map((s) => s.agentId)).toEqual([undefined, "ada", undefined]);
  });

  it("breaks a fan-out tie by declared order, so the same graph always reads the same", () => {
    const stages = orderStages({
      nodes: [
        { id: "root", kind: "trigger", name: "Start" },
        { id: "x", kind: "agent", name: "X", agent: "ada" },
        { id: "y", kind: "agent", name: "Y", agent: "grace" },
      ],
      edges: [
        { from: "root", to: "y" },
        { from: "root", to: "x" },
      ],
    });

    expect(stages.map((s) => s.name)).toEqual(["Start", "X", "Y"]);
  });

  it("keeps every node when the graph has a cycle", () => {
    const stages = orderStages({
      nodes: [
        { id: "a", kind: "agent", name: "A" },
        { id: "b", kind: "agent", name: "B" },
      ],
      edges: [
        { from: "a", to: "b" },
        { from: "b", to: "a" },
      ],
    });

    expect(stages.map((s) => s.name).sort()).toEqual(["A", "B"]);
  });

  // Documents behaviour rather than guarding a guard: a repeated edge balances
  // out (in-degree raised twice, followed twice), so this passes with or
  // without any explicit de-duplication. It is here because a saved graph can
  // carry the same edge twice and the stage list must not gain or lose a step.
  it("is not confused by a repeated edge", () => {
    const stages = orderStages({
      nodes: [
        { id: "a", kind: "trigger", name: "A" },
        { id: "b", kind: "agent", name: "B" },
      ],
      edges: [
        { from: "a", to: "b" },
        { from: "a", to: "b" },
      ],
    });

    expect(stages.map((s) => s.name)).toEqual(["A", "B"]);
  });
});

describe("workflows", () => {
  it("are the company's saved graphs, not templates", () => {
    const { workflows } = adapt({
      ...BASE,
      members: [member({ id: "ada", name: "Ada" })],
      desks: [desk("eng", "Engineering", ["ada"])],
      workflows: [
        flow({
          id: "wf-nightly",
          name: "Nightly digest",
          description: "Summarise the day.",
          nodes: [
            { id: "t", kind: "trigger", name: "22:00 UTC" },
            { id: "w", kind: "agent", name: "Write it", agent: "ada" },
          ],
          edges: [{ from: "t", to: "w" }],
        }),
      ],
    });

    expect(workflows).toHaveLength(1);
    expect(workflows[0].name).toBe("Nightly digest");
    expect(workflows[0].summary).toBe("Summarise the day.");
    expect(workflows[0].stages.map((s) => s.name)).toEqual(["22:00 UTC", "Write it"]);
    // Placement — the one thing this console decides. `ada` sits on `eng`.
    expect(workflows[0].departmentId).toBe("desk:eng");
  });

  it("draws no flow ring at all for a company that has saved none", () => {
    // The templates guaranteed a flow per desk, so this ring could never be
    // empty and an operator could never tell "no flows" from "not read".
    const { workflows } = adapt({
      ...BASE,
      members: [member({ id: "ada", name: "Ada" })],
      desks: [desk("eng", "Engineering", ["ada"])],
    });

    expect(workflows).toEqual([]);
  });

  it("is unplaced when it runs through nobody seated", () => {
    const { workflows } = adapt({
      ...BASE,
      members: [member({ id: "ada", name: "Ada" })],
      desks: [],
      workflows: [
        flow({
          id: "wf-ping",
          name: "Ping",
          nodes: [{ id: "h", kind: "http_request", name: "Call the API" }],
        }),
      ],
    });

    expect(workflows[0].departmentId).toBe(UNPLACED);
  });

  it("still draws a flow it could not place, hanging it off the core", () => {
    // A flow the host lists with no saved graph behind it falls back to an
    // empty stub, which names no agent and so cannot be placed. It must still
    // appear: the company declares it, and a flow missing from the wheel with
    // no error anywhere is the quieter lie.
    const { agents, departments, workflows } = adapt({
      ...BASE,
      members: [member({ id: "ada", name: "Ada" })],
      desks: [desk("eng", "Engineering", ["ada"])],
      workflows: [flow({ id: "wf-stub", name: "Unreadable flow" })],
    });
    const graph = buildKnowledgeGraph(agents, departments, [], [], workflows);

    expect(graph.nodes.some((n) => n.id === "flow:wf-stub")).toBe(true);
    expect(graph.edges).toContainEqual({
      source: SELF_ID,
      target: "flow:wf-stub",
      kind: "flow",
    });
    // Drawn stageless rather than given invented steps.
    expect(graph.nodes.filter((n) => n.kind === "step")).toEqual([]);
  });

  it("draws a stage only to the teammate the flow names", () => {
    const { agents, departments, workflows } = adapt({
      ...BASE,
      members: [
        member({ id: "ada", name: "Ada" }),
        member({ id: "grace", name: "Grace" }),
      ],
      desks: [desk("eng", "Engineering", ["ada", "grace"])],
      workflows: [
        flow({
          id: "wf",
          name: "Flow",
          nodes: [
            { id: "t", kind: "trigger", name: "Start" },
            { id: "d", kind: "agent", name: "Draft", agent: "ada" },
            // Names a teammate the roster does not carry: no edge, rather than
            // a link to a node that was never created.
            { id: "g", kind: "agent", name: "Ghost step", agent: "nobody" },
          ],
          edges: [
            { from: "t", to: "d" },
            { from: "d", to: "g" },
          ],
        }),
      ],
    });
    const graph = buildKnowledgeGraph(agents, departments, [], [], workflows);

    // Under the old round-robin every stage got an edge, dealt across the
    // department — including the two the flow never handed to anyone, and
    // `grace` collected one purely by being on the same desk.
    const runs = graph.edges.filter((e) => e.kind === "runs");
    expect(runs).toEqual([{ source: "step:wf:1", target: "emp:ada", kind: "runs" }]);
    expect(runs.some((e) => e.target === "emp:grace")).toBe(false);
  });
});

describe("DERIVED_NOTICE", () => {
  it("names flow placement and no longer claims departments or tools are invented", () => {
    expect(DERIVED_NOTICE).toMatch(/workflow/i);
    // The acceptance criterion for #601, asserted rather than assumed: the
    // notice must not still be telling operators that rings the host now
    // answers for are placeholders.
    expect(DERIVED_NOTICE).not.toMatch(/placeholder/i);
    expect(DERIVED_NOTICE).not.toMatch(/tool assignments/i);
    expect(DERIVED_NOTICE).not.toMatch(/templates/i);
  });
});
