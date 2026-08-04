// SPDX-License-Identifier: GPL-3.0-or-later

import type { Agent, Department, Person, SopTask, Workflow } from './schemas';

/**
 * The company's operating graph: who it is, how it is divided, what it runs,
 * who does that work, and what they reach for.
 *
 * Five concentric rings, read outward from the centre:
 *
 * | Ring | What |
 * |---|---|
 * | 0 | the company itself |
 * | 1 | its departments, each tinted with its own colour |
 * | 2 | the written-out SOP tasks, and the workflows a department runs |
 * | 3 | the workers — AI agents and humans — plus each workflow's stages |
 * | 4 | the tools those workers use |
 *
 * An SOP task is done by exactly one worker; a workflow passes through several,
 * one per stage. Pure data — icons live in the component, and only nodes with a
 * colour of their own carry one.
 */
export type KGNodeKind = 'self' | 'team' | 'workflow' | 'task' | 'step' | 'employee' | 'person' | 'tool';

export type KGNode = {
  id: string;
  kind: KGNodeKind;
  label: string;
  ring: number; // 0 = the company at the core → 4 = the outer tool ring
  color?: string; // the department's own colour (teams only)
  /** A department's place on the wheel; absent on every other kind. */
  order?: number;
};

export type KGEdgeKind = 'pillar' | 'flow' | 'stage' | 'next' | 'runs' | 'sop' | 'does' | 'member' | 'uses' | 'reports';

export type KGEdge = {
  source: string;
  target: string;
  kind: KGEdgeKind;
};

export type KnowledgeGraph = { nodes: KGNode[]; edges: KGEdge[] };

// Workflows share ring 2 with SOP tasks — both are work a department runs — and
// a flow's stages share ring 3 with the workers, because a stage is where the
// flow meets the worker who performs it.
const RING: Record<KGNodeKind, number> = {
  self: 0, team: 1, workflow: 2, task: 2, step: 3, employee: 3, person: 3, tool: 4,
};

export const SELF_ID = 'self';

/**
 * Where each department sits on the wheel.
 *
 * Read off the `order` the department itself declares, carried onto its node
 * when the graph is built. Anything without one sorts to the end, alphabetically
 * among its peers, so the wheel is stable whatever it is handed.
 */
export function orderGraphDepartments<T>(items: T[], deptIdOf: (item: T) => string): T[] {
  const rank = (item: T) => {
    const node = item as { order?: number };
    return typeof node.order === 'number' ? node.order : Number.MAX_SAFE_INTEGER;
  };
  return [...items].sort((a, b) => rank(a) - rank(b) || deptIdOf(a).localeCompare(deptIdOf(b)));
}

/** 'design-review' → 'Design Review' */
function prettify(slug: string): string {
  return slug
    .split(/[-_]/)
    .map((w) => w.charAt(0).toUpperCase() + w.slice(1))
    .join(' ');
}

/** Graph node id for a task's worker: agents are `emp:`, humans are `person:`. */
export function workerNodeId(kind: SopTask['assigneeKind'], assigneeId: string): string {
  return kind === 'agent' ? `emp:${assigneeId}` : `person:${assigneeId}`;
}

/**
 * Tool node id → tool slug. A tool reached from several departments is split
 * into one node per department (`tool:slack@dept-growth`) so no line has to
 * cross the whole wheel to a far-away shelf; a single-department tool keeps
 * the plain `tool:slug` id.
 */
export function toolSlugOf(nodeId: string): string {
  return nodeId.replace(/^tool:/, '').split('@')[0];
}

/**
 * Build the five-ring graph from the adapted org model: the company at the
 * core, each department that actually has somebody in it, their SOP tasks and
 * workflows, the workers who run them, and the tools those workers reach for.
 * Deterministic given the same inputs — nothing here depends on render order
 * or wall-clock time.
 */
export function buildKnowledgeGraph(
  agents: Agent[],
  departments: Department[],
  people: Person[] = [],
  tasks: SopTask[] = [],
  workflows: Workflow[] = [],
  /** What to call the node at the centre. */
  selfLabel = 'This company',
): KnowledgeGraph {
  const nodes: KGNode[] = [];
  const edges: KGEdge[] = [];

  // The company at the core — every department hangs off it.
  nodes.push({ id: SELF_ID, kind: 'self', label: selfLabel, ring: RING.self });

  // Departments (ring 1) — only the ones that actually have somebody in them,
  // each tinted with its own colour.
  //
  // `usedDepts` names every department id an agent or person *claims*; `drawn`
  // is the subset that actually got a `team:<id>` node below. A caller can
  // hand in a department id that isn't in `departments` at all (an admin
  // person forced onto a department the roster doesn't have, say) — edges
  // below must check `drawn`, not `usedDepts`, or they point `forceLink` at a
  // node that was never created and it throws.
  const usedDepts = new Set([...agents.map((a) => a.departmentId), ...people.map((p) => p.departmentId)]);
  const drawn = new Set<string>();
  for (const d of departments) {
    if (!usedDepts.has(d.id)) continue;
    // The pillar's tint comes from the department record itself.
    const color = d.color;
    nodes.push({ id: `team:${d.id}`, kind: 'team', label: d.name, ring: RING.team, color, order: d.order });
    edges.push({ source: SELF_ID, target: `team:${d.id}`, kind: 'pillar' });
    drawn.add(d.id);
  }

  // SOP tasks (ring 2) — the written-out jobs. Each hangs off its department
  // and hands the chain to exactly one worker below.
  const assignedWorkers = new Set<string>();
  for (const t of tasks) {
    if (!drawn.has(t.departmentId)) continue;
    nodes.push({ id: `task:${t.id}`, kind: 'task', label: t.title, ring: RING.task });
    edges.push({ source: `team:${t.departmentId}`, target: `task:${t.id}`, kind: 'sop' });
    const worker = workerNodeId(t.assigneeKind, t.assigneeId);
    edges.push({ source: `task:${t.id}`, target: worker, kind: 'does' });
    assignedWorkers.add(worker);
  }

  // Workflows (ring 2) — the multi-step routines a department runs. Unlike an
  // SOP task, a flow passes through several workers, so it draws one `runs`
  // edge per agent it touches.
  const agentIds = new Set(agents.map((a) => a.id));
  for (const f of workflows) {
    if (!drawn.has(f.departmentId)) continue;
    nodes.push({ id: `flow:${f.id}`, kind: 'workflow', label: f.name, ring: RING.workflow });
    edges.push({ source: `team:${f.departmentId}`, target: `flow:${f.id}`, kind: 'flow' });

    // Each stage is its own node, chained to the next so the flow reads as a
    // sequence rather than a bag, and handed to the agent who performs it.
    const runners = f.agentIds.filter((id) => agentIds.has(id));
    let prevStep: string | null = null;
    f.stages.forEach((stage, i) => {
      const stepId = `step:${f.id}:${i}`;
      nodes.push({ id: stepId, kind: 'step', label: stage, ring: RING.step });
      edges.push({ source: `flow:${f.id}`, target: stepId, kind: 'stage' });
      if (prevStep) edges.push({ source: prevStep, target: stepId, kind: 'next' });
      prevStep = stepId;
      // Stages are dealt round-robin across the department's agents: the flow
      // knows its order, not who owns which step.
      if (runners.length) {
        edges.push({ source: stepId, target: `emp:${runners[i % runners.length]}`, kind: 'runs' });
      }
    });
  }

  // First pass: which departments touch each tool? A tool used from several
  // departments is DUPLICATED — one copy per department — so its lines stay
  // local instead of crossing the wheel — no messy long edges.
  const deptsOfTool = new Map<string, Set<string>>();
  const workerRows: { nodeId: string; kind: 'employee' | 'person'; label: string; deptId: string; tools: string[] }[] = [
    ...agents.map((a) => ({ nodeId: `emp:${a.id}`, kind: 'employee' as const, label: a.name, deptId: a.departmentId, tools: a.tools })),
    ...people.map((p) => ({ nodeId: `person:${p.id}`, kind: 'person' as const, label: p.name, deptId: p.departmentId, tools: p.tools })),
  ];
  for (const w of workerRows) {
    for (const slug of w.tools) {
      (deptsOfTool.get(slug) ?? deptsOfTool.set(slug, new Set()).get(slug)!).add(w.deptId);
    }
  }
  const toolNodeId = (slug: string, deptId: string) =>
    (deptsOfTool.get(slug)?.size ?? 0) > 1 ? `tool:${slug}@${deptId}` : `tool:${slug}`;

  // Workers (ring 3): AI agents and the humans in the process.
  for (const w of workerRows) {
    nodes.push({ id: w.nodeId, kind: w.kind, label: w.label, ring: RING[w.kind] });
    // Workers reach their team through their task; only a worker with no task
    // (a data gap the seed tests forbid) falls back to a direct member edge.
    if (!assignedWorkers.has(w.nodeId) && drawn.has(w.deptId)) {
      edges.push({ source: w.nodeId, target: `team:${w.deptId}`, kind: 'member' });
    }
    for (const slug of w.tools) {
      edges.push({ source: w.nodeId, target: toolNodeId(slug, w.deptId), kind: 'uses' });
    }
  }
  for (const a of agents) {
    if (a.parentId && agentIds.has(a.parentId)) {
      edges.push({ source: `emp:${a.id}`, target: `emp:${a.parentId}`, kind: 'reports' });
    }
  }

  // Software tools (outer ring) — one node per (tool, department-that-uses-it)
  // for shared tools, a single node otherwise.
  for (const slug of [...deptsOfTool.keys()].sort()) {
    const depts = [...deptsOfTool.get(slug)!].sort();
    if (depts.length > 1) {
      for (const deptId of depts) {
        nodes.push({ id: `tool:${slug}@${deptId}`, kind: 'tool', label: prettify(slug), ring: RING.tool });
      }
    } else {
      nodes.push({ id: `tool:${slug}`, kind: 'tool', label: prettify(slug), ring: RING.tool });
    }
  }

  return { nodes, edges };
}
