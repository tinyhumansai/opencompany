// SPDX-License-Identifier: GPL-3.0-or-later

// Our host's data, shaped into the knowledge graph's five-ring org model.
//
// ## What is real, and what is not
//
// The graph reads `company → department → SOP task → the worker who does it →
// that worker's tools`. Every one of those is a value the host answers for:
//
// | Edge                  | Source                                        |
// |-----------------------|-----------------------------------------------|
// | task → worker         | `task.assignee` on the board                  |
// | teammate → department | the desk that seats it (`GET {scope}/desks`)  |
// | worker → tools        | its resolved grants (`GET …/team`)            |
// | workflow → stages     | the saved graph's nodes (`GET …/workflows/…`) |
// | stage → worker        | the `agent` node's own agent id               |
//
// The one thing left that is this console's reading rather than the company's
// declaration is **where a workflow hangs on the wheel** — see `DERIVED_NOTICE`.
//
// Ring 1 used to be invented too: `assignDepartment` keyword-matched a role
// string into one of five hardcoded buckets, falling back to Operations. It is
// gone (issue #486). A desk — a `[[group_chat]]` or an operator-created overlay
// desk — is the one place the company actually declares how it is organised, so
// the departments are its desks and a teammate's department is the desk they
// are seated on.
//
// The consequence is that the graph now has to answer a question the invention
// let it dodge: **what about somebody the company declares no desk for?** They
// are not placed — see `UNPLACED`. Nothing here invents a position for anyone.
//
// The per-agent tool list used to be derived too: `assignTools` dealt each
// teammate a slice of the company-wide `[tools] allow` list, because the roster
// **list** carried no grants and fetching every agent's detail on page load was
// worse. That DTO gap is closed (issue #601) — a roster row now carries the
// grants the host resolved for that teammate, from the same server-side
// constructor the per-agent detail read uses, so the graph and the agent card
// cannot disagree about who holds what (the rule issue #264 established).
//
// An agent's **tier** was the same gap with a louder symptom (issue #643): the
// list carried none, so every node was stamped a literal `worker` and a company
// declaring `tier = "orchestrator"` read back as a worker on its own graph. The
// tier and the orchestrator marker are both host-answered now, through the same
// constructor the detail card reads — and they are two questions, not one: the
// tier is the declaration verbatim, the marker is the host's roster rule, and
// nothing here re-derives the second from the first.
//
// Workflows were the last invention: `WORKFLOW_ROUTINES` dealt one made-up
// routine per desk by position, and `model.ts` dealt its stages round-robin
// across that desk's agents. Both are gone (issue #601). A workflow is one of
// the company's saved graphs, its stages are that graph's nodes in run order,
// and a stage hands off to the agent the flow itself names.

import type { Person as HostPerson } from "@/api/auth";
import type { Task } from "@/api/tasks";
import type { MemoryEntry } from "@/api/memory";
import type { DeskDto } from "@/api/types";
import type { WorkflowGraph } from "@/api/workflows";
import type { TeamMember } from "@/lib/team";
import { humanizeStatus } from "@/lib/board-columns";
import type { BrainGraphEdge, BrainGraphNode, MemoryGraph } from "./memory-core";
import { distillMemoryGraph } from "./memory-core";
import { personName } from "@/lib/person";
import { isOpen } from "../pulse";
import type { Agent, Department, Person, SopTask, Workflow, WorkflowStage } from "./schemas";

/**
 * The one caveat left on this surface.
 *
 * Everything the graph draws is something the company declared — except which
 * pillar a workflow hangs off, because the host scopes a flow to the *company*
 * and nothing links a flow to a desk. It is drawn on the desk of the first
 * teammate it runs through, which is a real relationship (that teammate really
 * does perform that stage, and really does sit on that desk) read as a
 * placement it was never declared to be.
 */
export const DERIVED_NOTICE =
  "A workflow is drawn on the desk of the first teammate it runs through — the company scopes flows to itself, not to a desk. Everything else on this graph is declared.";

/**
 * The department a worker gets when the company declares no desk for them.
 *
 * Not a department: no `team:` node is ever built for it, so a worker carrying
 * it draws no pillar and hangs off the company core instead of a desk. This is
 * the graph's version of the org chart's "Not on a desk" section — `lib/org.ts`
 * keeps the same people out of its tree for the same reason, and says so:
 * inventing a position for them "is exactly the mistake the Overview graph
 * makes when it keyword-matches a role into a department".
 *
 * Prefixed so it can never collide with a real desk: desk ids become
 * `desk:<id>`, so a manifest desk literally named `unplaced` is `desk:unplaced`
 * and stays distinct from this.
 */
export const UNPLACED = "unplaced";

/** A desk's department id. Namespaced so no desk id can collide with `UNPLACED`. */
export function departmentIdOfDesk(deskId: string): string {
  return `desk:${deskId}`;
}

/**
 * The pillar colours, in order — dealt to desks by position. A company with
 * more desks than hues wraps, which is a repeated tint rather than a wrong
 * one.
 *
 * These now *are* the console's chart hues, which the comment here has always
 * claimed. They were a stale copy of them in hex: five values that no longer
 * matched `--chart-*` and could not follow the theme, so the org graph drifted
 * a little further from the rest of the console with every palette change.
 */
const PILLAR_COLORS = [
  "var(--chart-1)",
  "var(--chart-2)",
  "var(--chart-3)",
  "var(--chart-4)",
  "var(--chart-5)",
];

/** `Product Design` → `product-design`, for the department's slug. */
function slugify(s: string): string {
  return s.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "");
}

/**
 * Ring 1: the company's desks, in the order the host serves them.
 *
 * **Declared, not derived.** A desk is a `[[group_chat]]` in the manifest or an
 * operator-created overlay desk; either way the company said it exists and said
 * who is on it. The order is the host's own — never re-sorted here, the same
 * rule `lib/org.ts` follows for seats.
 */
export function deskDepartments(desks: DeskDto[]): Department[] {
  return desks.map((desk, i) => ({
    id: departmentIdOfDesk(desk.id),
    name: desk.name,
    slug: slugify(desk.name) || slugify(desk.id) || `desk-${i}`,
    tagline: desk.description ?? "",
    color: PILLAR_COLORS[i % PILLAR_COLORS.length],
    order: i,
  }));
}

/**
 * Which desk a teammate is seated on, or `UNPLACED`.
 *
 * A teammate can sit on several desks — desks are flat, and nothing stops a
 * manifest seating one agent twice. The graph's model gives an agent exactly
 * one `departmentId`, so the **first** desk that names them wins, in the host's
 * desk order. That under-reports the structure (the org chart shows both
 * seats); it never invents one, which is the property that matters here.
 */
export function deskOfMember(id: string, desks: DeskDto[]): string {
  const desk = desks.find((d) => d.members.includes(id));
  return desk ? departmentIdOfDesk(desk.id) : UNPLACED;
}

/**
 * A saved workflow graph, written out as an ordered list of stages.
 *
 * The nodes are sorted into run order (Kahn's algorithm over the graph's own
 * edges, ties broken by declared order) rather than taken as declared: the file
 * lists nodes, the edges say what follows what, and a flow read out of order is
 * a flow described wrongly. A cycle — or a node no edge reaches — cannot be
 * ordered that way, so anything left over is appended in declared order rather
 * than dropped.
 *
 * `agentId` rides along from the graph's own `agent` nodes, so the "who performs
 * this stage" edge is the flow's own answer rather than a deal across a desk.
 */
export function orderStages(graph: Pick<WorkflowGraph, "nodes" | "edges">): WorkflowStage[] {
  const rank = new Map(graph.nodes.map((n, i) => [n.id, i]));
  const indegree = new Map(graph.nodes.map((n) => [n.id, 0]));
  const next = new Map<string, string[]>();
  // A repeated edge needs no special case: it raises the target's in-degree
  // twice and is then followed twice, so the two cancel and the target is
  // still released exactly once. An edge naming a node this graph does not
  // contain is skipped, so it can never hold a real node back.
  for (const edge of graph.edges) {
    if (!rank.has(edge.from) || !rank.has(edge.to)) continue;
    indegree.set(edge.to, (indegree.get(edge.to) ?? 0) + 1);
    next.set(edge.from, [...(next.get(edge.from) ?? []), edge.to]);
  }

  const ready = graph.nodes.filter((n) => indegree.get(n.id) === 0).map((n) => n.id);
  const order: string[] = [];
  const placed = new Set<string>();
  while (ready.length) {
    // Lowest declared index first, so a fan-out reads in file order and the
    // same graph always produces the same list.
    ready.sort((a, b) => (rank.get(a) ?? 0) - (rank.get(b) ?? 0));
    const id = ready.shift()!;
    if (placed.has(id)) continue;
    placed.add(id);
    order.push(id);
    for (const to of next.get(id) ?? []) {
      const left = (indegree.get(to) ?? 0) - 1;
      indegree.set(to, left);
      if (left === 0) ready.push(to);
    }
  }
  for (const node of graph.nodes) if (!placed.has(node.id)) order.push(node.id);

  const byId = new Map(graph.nodes.map((n) => [n.id, n]));
  return order.map((id) => {
    const node = byId.get(id)!;
    return {
      name: node.name,
      // Only an `agent` node names a teammate; every other kind performs no
      // agent's work and must not be attached to one.
      ...(node.kind === "agent" && node.agent ? { agentId: node.agent } : {}),
    };
  });
}

// `assignHumanDepartment` was deleted with `assignDepartment` (issue #486),
// and the reason is worth keeping: it did not merely stay derived, it got
// *worse* when ring 1 became real.
//
// It spread humans deterministically across the departments that existed. While
// those were invented buckets that was internally consistent fiction — a made-up
// person-to-made-up-bucket edge. Now the departments are the company's actual
// desks, with membership the company declared. Dealing a human onto one would
// claim they sit on a real desk whose real member list does not name them, and
// the graph would contradict the org chart on a fact the operator can check.
//
// So a human is `UNPLACED`, which is what the org chart already decided for the
// same people: "Desks staff agents, so the company declares no desk for a
// person, and this chart does not guess one."

export interface AdaptInput {
  members: TeamMember[];
  /**
   * The company's desks — ring 1, and the only declared statement of how this
   * company is organised. An empty list is a company that declares no
   * structure: it draws no pillars and everyone is unplaced, which is the true
   * answer rather than a guessed one.
   */
  desks: DeskDto[];
  tasks: Task[];
  /**
   * The company's saved workflow graphs, whole — nodes and edges, not just
   * names. The list read serves summaries only, so the caller fetches each
   * graph; see `Overview.tsx` for why that is bounded and what a flow with no
   * saved graph falls back to.
   */
  workflows: WorkflowGraph[];
  /** The humans who can sign in to this company. */
  people: HostPerson[];
  /** Matches a board card to a roster member; the one real assignment edge. */
  ownedBy: (task: Task, member: TeamMember) => boolean;
}

export interface Adapted {
  departments: Department[];
  agents: Agent[];
  people: Person[];
  workflows: Workflow[];
  tasks: SopTask[];
  /** Tool slug → display label, for the detail cards. */
  toolLabels: Record<string, string>;
}

/** Shape the host's data into the graph's org model. */
export function adapt(input: AdaptInput): Adapted {
  const agents: Agent[] = input.members.map((member) => ({
    id: member.id,
    departmentId: deskOfMember(member.id, input.desks),
    name: member.name,
    role: member.role,
    status: "active",
    // The company's own answer, verbatim (issue #643). This was a literal
    // `"worker"` on every node, so a company declaring `tier = "orchestrator"`
    // read back as a worker on its own graph. `null` means the company declared
    // no tier, which the card draws as "not declared" rather than as a default.
    tier: member.tier ?? null,
    // The host's roster rule, read — never re-derived from `tier`. The two are
    // different questions: an untagged company still has an orchestrator, and a
    // second agent tagged with that tier is not one.
    isOrchestrator: member.isOrchestrator ?? false,
    description: member.description,
    // Still a placeholder, deliberately. Resolving it console-side would mean a
    // second copy of the host's `model_for_tier` — the #264 drift hazard this
    // file keeps closing — and resolving it host-side is not available today:
    // `src/harness` is feature-gated out of the default build that `src/server`
    // compiles in, so the roster read has nothing to ask.
    model: "—",
    // What the host says this teammate actually holds, straight through: its
    // own `[[agent]].tools` line narrowed by the company's `[tools] allow`,
    // resolved server-side. The agent detail card renders the same list from
    // the same resolution, which is the whole point of issue #601.
    tools: member.effectiveTools,
    parentId: null,
    instance: "builtin",
  }));

  // A tool slug is a grant glob now — the literal `[tools] allow` entry, like
  // `composio`, `mcp:*` or `workspace.*` — so it is its own best label. The map
  // is kept because the detail card takes one, and mapping each grant to itself
  // is what stops the card title-casing `workspace.*` into something an
  // operator cannot grep for.
  const toolLabels: Record<string, string> = {};
  for (const agent of agents) for (const grant of agent.tools) toolLabels[grant] = grant;

  // A board card becomes an SOP task owned by the teammate it is assigned to —
  // the one edge here that the host actually records. Cards nobody owns are
  // dropped rather than parked under an invented owner, and a closed one
  // (`pulse.ts`'s `isOpen`) is dropped too: the graph reads as work owed, not
  // an archive, so a card already in Done should not still show as active.
  const tasks: SopTask[] = [];
  for (const task of input.tasks) {
    if (!isOpen(task)) continue;
    const member = input.members.find((m) => input.ownedBy(task, m));
    if (!member) continue;
    // A task hangs off its owner's desk. An unplaced owner has none, and ring 2
    // hangs off ring 1 — so their card is dropped rather than parked under a
    // desk they are not on. That loses a real card from the graph, which is a
    // genuine cost and the reason it is stated here rather than left to fall
    // out of a guard in `model.ts`: the alternative is asserting a desk
    // membership the company never declared, and the board still shows the
    // card. Seat the teammate on a desk and it comes back.
    const departmentId = deskOfMember(member.id, input.desks);
    if (departmentId === UNPLACED) continue;
    tasks.push({
      id: task.id,
      departmentId,
      title: task.title,
      summary: task.note ?? "",
      steps: [
        // Humanised rather than read off the ledger: this adapter is a pure
        // function over what the graph was handed, and threading an async
        // column read through it would buy "In progress" instead of "In
        // progress" — the two agree on every column but `todo`.
        `Column: ${humanizeStatus(task.column)}`,
        `Priority: ${task.priority}`,
        `Owner: ${task.assignee}`,
      ],
      assigneeKind: "agent",
      assigneeId: member.id,
    });
  }

  // Only desks that ended up with somebody on them. A declared desk with no
  // members draws no pillar — `buildKnowledgeGraph` skips a department nobody
  // claims, and matching that here keeps the two from disagreeing.
  const used = new Set(agents.map((a) => a.departmentId));
  const departments = deskDepartments(input.desks).filter((d) => used.has(d.id));

  const people: Person[] = input.people.map((p) => ({
    id: p.id,
    // Never a desk: the company staffs desks with agents and declares no desk
    // for a person. See the note where `assignHumanDepartment` used to be.
    departmentId: UNPLACED,
    // Falling back to a name derived from the address keeps a real name off
    // the graph only when nobody has set one — the same derivation every other
    // surface uses, so one person is not two names on two screens.
    name: personName(p),
    role: p.role === "admin" ? "Admin" : "Member",
    // Humans get no derived tool shelf: an operator's tools are their browser,
    // and inventing an MCP loadout for a person would read as a claim.
    tools: [],
  }));

  // Every workflow the company has saved, written out. Placement is the one
  // reading here (see `DERIVED_NOTICE`): a flow hangs off the desk of the first
  // teammate it runs through.
  //
  // A flow that runs through nobody seated — a pure trigger/HTTP routine, one
  // whose agents are all on no desk, or one the host lists with no saved graph
  // behind it — comes out `UNPLACED`. Unlike an unplaced teammate's board card,
  // which is dropped above, the flow is still drawn: `model.ts` hangs it off the
  // company core, the same answer it gives an unplaced worker. The company
  // really does declare the flow, so the choice is between drawing it somewhere
  // honest and hiding it, not between two placements.
  const deskOfAgent = new Map(agents.map((a) => [a.id, a.departmentId]));
  const workflows: Workflow[] = input.workflows.map((graph) => {
    const stages = orderStages(graph);
    const firstSeated = stages
      .map((stage) => (stage.agentId ? deskOfAgent.get(stage.agentId) : undefined))
      .find((desk) => desk !== undefined && desk !== UNPLACED);
    return {
      id: graph.id,
      departmentId: firstSeated ?? UNPLACED,
      name: graph.name,
      summary: graph.description ?? "",
      stages,
    };
  });

  return { departments, agents, people, workflows, tasks, toolLabels };
}

/**
 * Which folder a memory row hangs off.
 *
 * An operator-authored fact carries its own taxonomy (`preference`, `person`,
 * …); an agent's runtime chunk carries none, so it is bucketed by where it came
 * from instead. Mirrors `MemoryView`'s type filter, so the graph and the Brain
 * page group the same rows the same way.
 */
function folderOf(entry: MemoryEntry): string {
  return entry.origin === "fact" ? (entry.kind ?? "fact") : entry.origin;
}

/**
 * The memory constellation, in the shape the core distils from.
 *
 * Each entry is a page, each memory kind is its folder hub. Entries of a kind
 * are linked to each other so the force layout has structure to pull on —
 * a `similar` edge here means "same kind", which is the only similarity this
 * console can honestly claim.
 */
export function buildMemoryGraph(entries: MemoryEntry[]): MemoryGraph {
  const nodes: BrainGraphNode[] = [];
  const edges: BrainGraphEdge[] = [];
  const kinds = [...new Set(entries.map(folderOf))];

  kinds.forEach((kind, k) => {
    const angle = (k / Math.max(1, kinds.length)) * Math.PI * 2;
    nodes.push({
      id: `folder:${kind}`,
      type: "folder",
      label: kind,
      folder: kind,
      kind,
      excerpt: "",
      wordCount: 0,
      tags: [],
      agents: [],
      vx: Math.cos(angle) * 0.4,
      vy: Math.sin(angle) * 0.4,
      vector: [],
      chunks: 0,
    });
  });

  const byKind = new Map<string, string[]>();
  entries.forEach((entry) => {
    const folder = folderOf(entry);
    const k = kinds.indexOf(folder);
    const angle = (k / Math.max(1, kinds.length)) * Math.PI * 2;
    // Seed each page near its folder, jittered deterministically by id so the
    // layout starts spread rather than stacked.
    const jitter = hash(entry.id) % 1000;
    const spread = 0.18 + (jitter / 1000) * 0.22;
    const spin = ((jitter % 360) * Math.PI) / 180;
    nodes.push({
      id: entry.id,
      type: "page",
      label: entry.title,
      folder,
      kind: folder,
      excerpt: entry.body,
      wordCount: entry.body.split(/\s+/).filter(Boolean).length,
      tags: [entry.source],
      agents: [],
      vx: clamp(Math.cos(angle) * 0.4 + Math.cos(spin) * spread),
      vy: clamp(Math.sin(angle) * 0.4 + Math.sin(spin) * spread),
      vector: [],
      chunks: 1,
    });
    edges.push({ source: entry.id, target: `folder:${folder}`, type: "member" });
    const siblings = byKind.get(folder);
    if (siblings) {
      edges.push({ source: entry.id, target: siblings[siblings.length - 1], type: "similar" });
      siblings.push(entry.id);
    } else {
      byKind.set(folder, [entry.id]);
    }
  });

  return distillMemoryGraph({ nodes, edges });
}

const clamp = (n: number) => Math.max(-1, Math.min(1, n));

function hash(s: string): number {
  let h = 0;
  for (const ch of s) h = (h * 31 + ch.charCodeAt(0)) >>> 0;
  return h;
}
