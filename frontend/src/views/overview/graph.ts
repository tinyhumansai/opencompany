// The company as a graph, and the sunburst it is drawn on.
//
// Everything here is pure. The model is built from what the host serves, the
// layout is deterministic (no force simulation, no animation frame, no clock),
// and the highlight is a set operation — so what the operator sees can be
// asserted directly, and the same inputs always draw the same picture.

import type { Task } from "@/api/tasks";
import type { Skill } from "@/api/skills";
import type { McpServer, McpTool } from "@/lib/mcp";
import type { MemoryEntry } from "@/lib/memory";
import { TASK_COLUMNS } from "@/lib/tasks-sample";
import type { TeamMember } from "@/lib/team";
import { isOpen, ownedBy } from "./pulse";

/**
 * What a node is.
 *
 * The chain reads outward from the company: a **branch** hub sits on ring 1
 * and its **leaves** on ring 2. Each branch is one honest relationship the
 * host actually records — a teammate owns cards, a category groups skills, a
 * server advertises tools. Nothing is joined across those branches, because
 * the host stores no edge that would justify it.
 */
export type NodeKind =
  | "company"
  | "memory"
  | "desk"
  | "card"
  | "capability"
  | "skill"
  | "server"
  | "tool";

/** The branch each kind belongs to. Drives hue: one hue per branch. */
export const BRANCH_OF: Record<NodeKind, "company" | "memory" | "work" | "capability" | "tools"> = {
  company: "company",
  memory: "memory",
  desk: "work",
  card: "work",
  capability: "capability",
  skill: "capability",
  server: "tools",
  tool: "tools",
};

/**
 * Memories hang off the company like hubs do, but they are not hubs: they live
 * in the core disc at the centre rather than on the ring, and nothing hangs off
 * them.
 */
export const isHubKind = (kind: NodeKind): boolean => kind !== "company" && kind !== "memory";

export interface GraphNode {
  id: string;
  kind: NodeKind;
  label: string;
  /** The second line under the label, and the tooltip's right half. */
  sub: string;
  /** Hub id, or null for the company itself. */
  parent: string | null;
  /** Finished or inactive — drawn dimmer, still present. */
  muted?: boolean;
  /** The source record, for the inspector to render in full. */
  payload?: unknown;
}

export interface Graph {
  nodes: GraphNode[];
  byId: Map<string, GraphNode>;
  /** Hub id → its leaves, in draw order. */
  children: Map<string, string[]>;
  /** Ring-1 hubs, in draw order. */
  hubs: string[];
}

export interface Placed {
  x: number;
  y: number;
  r: number;
  /** 0 company, 1 hub, 2 leaf. */
  ring: number;
  /** Where on the dial it sits, for the label's side. */
  angle: number;
}

export const COMPANY_ID = "company";

interface BuildInput {
  companyName: string;
  lifecycle: string;
  members: TeamMember[];
  tasks: Task[];
  skills: Skill[];
  servers: McpServer[];
  /** Tools per server id. Absent for a server that isn't connected. */
  toolsByServer: Record<string, McpTool[]>;
  memories: MemoryEntry[];
}

/**
 * Assemble the graph.
 *
 * Hubs are emitted branch by branch — desks, then skill categories, then MCP
 * servers — so the sunburst keeps related wedges adjacent instead of
 * interleaving them.
 */
export function buildGraph(input: BuildInput): Graph {
  const nodes: GraphNode[] = [
    {
      id: COMPANY_ID,
      kind: "company",
      label: input.companyName,
      sub: input.lifecycle,
      parent: null,
    },
  ];

  // The core: what the company remembers. These sit at the centre rather than
  // on the ring, so the middle of the graph shows its knowledge instead of an
  // opaque dot standing in for it.
  for (const entry of input.memories) {
    nodes.push({
      id: `memory:${entry.id}`,
      kind: "memory",
      label: entry.title,
      sub: entry.kind,
      parent: COMPANY_ID,
      payload: entry,
    });
  }

  for (const member of input.members) {
    const own = input.tasks.filter((t) => ownedBy(t, member));
    const open = own.filter(isOpen).length;
    nodes.push({
      id: `desk:${member.id}`,
      kind: "desk",
      label: member.name,
      sub: open > 0 ? `${open} open` : member.role,
      parent: COMPANY_ID,
      muted: own.length === 0,
      payload: member,
    });
    for (const task of own) {
      nodes.push({
        id: `card:${task.id}`,
        kind: "card",
        label: task.title,
        sub: columnLabel(task.column),
        parent: `desk:${member.id}`,
        muted: !isOpen(task),
        payload: task,
      });
    }
  }

  for (const [category, skills] of groupBy(input.skills, (s) => s.category || "Uncategorised")) {
    nodes.push({
      id: `capability:${category}`,
      kind: "capability",
      label: category,
      sub: `${skills.filter((s) => s.enabled).length} of ${skills.length} on`,
      parent: COMPANY_ID,
      payload: category,
    });
    for (const skill of skills) {
      nodes.push({
        id: `skill:${skill.id}`,
        kind: "skill",
        label: skill.name,
        sub: skill.enabled ? "enabled" : "off",
        parent: `capability:${category}`,
        muted: !skill.enabled,
        payload: skill,
      });
    }
  }

  for (const server of input.servers) {
    nodes.push({
      id: `server:${server.server_id}`,
      kind: "server",
      label: server.name,
      sub: server.status,
      parent: COMPANY_ID,
      muted: server.status !== "connected",
      payload: server,
    });
    for (const tool of input.toolsByServer[server.server_id] ?? []) {
      nodes.push({
        id: `tool:${server.server_id}:${tool.name}`,
        kind: "tool",
        label: tool.name,
        sub: server.name,
        parent: `server:${server.server_id}`,
        payload: tool,
      });
    }
  }

  return index(nodes);
}

/** Build the lookup maps and the hub list a `Graph` carries. */
function index(nodes: GraphNode[]): Graph {
  const byId = new Map(nodes.map((n) => [n.id, n]));
  const children = new Map<string, string[]>();
  for (const node of nodes) {
    if (!node.parent) continue;
    const list = children.get(node.parent);
    if (list) list.push(node.id);
    else children.set(node.parent, [node.id]);
  }
  const hubs = (children.get(COMPANY_ID) ?? []).filter((id) => isHubKind(byId.get(id)!.kind));
  return { nodes, byId, children, hubs };
}

export interface LayoutOptions {
  cx: number;
  cy: number;
  /** Radius of the hub ring and of the leaf ring. */
  hubRing: number;
  leafRing: number;
}

/**
 * The resting sunburst: company at the centre, hubs on ring 1, leaves balanced
 * inside their hub's wedge.
 *
 * Wedges are **density-weighted** — a hub's angular span is proportional to how
 * many leaves it holds, so a teammate carrying six cards gets room to spread
 * and one carrying none stays narrow. With every hub equally loaded this
 * degenerates to even spacing, which is what an empty company should look like.
 */
export function layoutGraph(graph: Graph, options: LayoutOptions): Map<string, Placed> {
  const { cx, cy, hubRing, leafRing } = options;
  const placed = new Map<string, Placed>();
  const polar = (r: number, a: number) => ({ x: cx + r * Math.cos(a), y: cy + r * Math.sin(a) });

  placed.set(COMPANY_ID, { x: cx, y: cy, r: 34, ring: 0, angle: 0 });

  const weights = graph.hubs.map((id) => Math.max(1, graph.children.get(id)?.length ?? 0));
  const total = weights.reduce((sum, w) => sum + w, 0) || 1;

  // Start at twelve o'clock so the first wedge is where the eye lands.
  let cursor = -Math.PI / 2;
  graph.hubs.forEach((hubId, i) => {
    const span = (weights[i] / total) * Math.PI * 2;
    const centre = cursor + span / 2;
    cursor += span;

    placed.set(hubId, {
      ...polar(hubRing, centre),
      r: 13 + Math.min(weights[i], 8),
      ring: 1,
      angle: centre,
    });

    const leaves = graph.children.get(hubId) ?? [];
    // Inset keeps neighbouring wedges from touching at the leaf ring, where
    // the arc length is longest and collisions actually happen.
    const usable = span * 0.82;
    leaves.forEach((leafId, j) => {
      const t = leaves.length === 1 ? 0.5 : j / (leaves.length - 1);
      const angle = centre - usable / 2 + usable * t;
      // Big enough to wear its icon and be clicked; small enough that a busy
      // hub's leaves do not merge into an arc.
      placed.set(leafId, { ...polar(leafRing, angle), r: 10, ring: 2, angle });
    });
  });

  return placed;
}

/**
 * The chain through `id`: the node, everything above it, everything below it.
 *
 * This is what lights up on hover. Highlighting neighbours alone would leave
 * the operator guessing which teammate a card belongs to — the whole point of
 * drawing it as a chain is that one hover answers that.
 */
export function chainOf(graph: Graph, id: string | null): Set<string> {
  if (!id || !graph.byId.has(id)) return new Set();
  const lit = new Set<string>([id]);

  for (let cursor = graph.byId.get(id)?.parent; cursor; cursor = graph.byId.get(cursor)?.parent) {
    lit.add(cursor);
  }
  const queue = [id];
  while (queue.length) {
    const next = queue.pop()!;
    for (const child of graph.children.get(next) ?? []) {
      lit.add(child);
      queue.push(child);
    }
  }
  return lit;
}

/** How many nodes of each kind the graph holds, for the legend. */
export function countsByKind(graph: Graph): Map<NodeKind, number> {
  const counts = new Map<NodeKind, number>();
  for (const node of graph.nodes) counts.set(node.kind, (counts.get(node.kind) ?? 0) + 1);
  return counts;
}

/**
 * The graph with some kinds hidden.
 *
 * Hiding a hub takes its leaves with it — a tool with no server on screen has
 * nothing to hang from, and leaving it floating would imply an edge to the
 * company that does not exist.
 */
export function withoutKinds(graph: Graph, hidden: Set<NodeKind>): Graph {
  if (hidden.size === 0) return graph;
  const keep = new Set<string>();
  for (const node of graph.nodes) {
    if (hidden.has(node.kind)) continue;
    if (node.parent && !keep.has(node.parent)) continue;
    keep.add(node.id);
  }
  return index(graph.nodes.filter((n) => keep.has(n.id)));
}

function columnLabel(column: string): string {
  return TASK_COLUMNS.find((c) => c.id === column)?.label ?? column;
}

function groupBy<T>(items: T[], key: (item: T) => string): Map<string, T[]> {
  const groups = new Map<string, T[]>();
  for (const item of items) {
    const k = key(item);
    const list = groups.get(k);
    if (list) list.push(item);
    else groups.set(k, [item]);
  }
  return groups;
}
