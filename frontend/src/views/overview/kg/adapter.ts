// Our host's data, shaped into the knowledge graph's five-ring org model.
//
// ## What is real, and what is not
//
// The graph reads `company → department → SOP task → the worker who does it →
// that worker's tools`. This host serves only some of those edges:
//
// | Edge                | Source                        | Real? |
// |---------------------|-------------------------------|-------|
// | task → worker       | `task.assignee` on the board  | yes   |
// | category → skill    | `skill.category`              | yes   |
// | server → tool       | what the server advertises    | yes   |
// | teammate → department | nothing                     | **derived** |
// | worker → tools      | nothing (`[tools] allow` is company-wide) | **derived** |
// | human → department  | nothing (sign-in tells us who, not what) | **derived** |
// | department → workflow | nothing (no flow API yet)   | **derived** |
//
// The two derived edges are placeholders: a company manifest has no department
// field and no per-agent tool list, so this module invents a plausible
// structure rather than leaving three of the five rings empty. That is a
// deliberate, and temporary, lie — `DERIVED_NOTICE` is rendered in the UI so
// nobody reads the org chart as something the company declared. When
// `[[agent]]` grows `department` and `tools`, delete `assignDepartment` and
// `assignTools` and read them straight through.

import type { Person as HostPerson } from "@/api/auth";
import type { Skill } from "@/api/skills";
import type { Task } from "@/api/tasks";
import type { McpServer, McpTool } from "@/lib/mcp";
import type { MemoryEntry } from "@/lib/memory";
import type { TeamMember } from "@/lib/team";
import { TASK_COLUMNS } from "@/lib/tasks-sample";
import type { BrainGraphEdge, BrainGraphNode, MemoryGraph } from "./memory-core";
import { distillMemoryGraph } from "./memory-core";
import type { Agent, Department, Person, SopTask, Workflow } from "./schemas";

/** Shown wherever the derived structure is on screen. */
export const DERIVED_NOTICE =
  "Departments, workflows, and tool assignments are placeholders — this company doesn't declare them.";

/**
 * The department set. Keyed by the functional areas a small company actually
 * splits into, so the derived assignment lands somewhere plausible rather than
 * arbitrary. Colours are the console's chart hues.
 */
const DEPARTMENTS: Department[] = [
  { id: "dept-product", name: "Product", slug: "product", tagline: "What we build and why", color: "#2a78d6", order: 0 },
  { id: "dept-engineering", name: "Engineering", slug: "engineering", tagline: "Building and running it", color: "#1baf7a", order: 1 },
  { id: "dept-design", name: "Design", slug: "design", tagline: "How it looks and feels", color: "#eb6834", order: 2 },
  { id: "dept-growth", name: "Growth", slug: "growth", tagline: "Finding and keeping users", color: "#4a3aa7", order: 3 },
  { id: "dept-ops", name: "Operations", slug: "ops", tagline: "Keeping the machine running", color: "#eda100", order: 4 },
];

/**
 * Role keywords that place a teammate in a department, checked in order.
 *
 * Order matters where a title names two areas: "Product Designer" is a
 * designer, so Design is tested before Product, and Product's own words are
 * the ones no other area claims.
 */
const ROLE_HINTS: { department: string; words: string[] }[] = [
  { department: "dept-design", words: ["design", "ux", "ui", "brand", "creative"] },
  { department: "dept-engineering", words: ["engineer", "developer", "backend", "frontend", "qa", "devops", "security", "infrastructure"] },
  { department: "dept-growth", words: ["growth", "market", "sales", "writer", "content", "social", "campaign"] },
  { department: "dept-product", words: ["product", "pm", "strategy", "research", "analyst", "data", "roadmap"] },
  { department: "dept-ops", words: ["ops", "operation", "support", "finance", "legal", "front desk", "admin"] },
];

/**
 * Which department a teammate belongs to.
 *
 * **Derived.** Their role text is matched against the hint words above; a role
 * that matches nothing falls to Operations, which is where unclassified work
 * really does end up. Deterministic, so a teammate does not move between
 * renders.
 */
export function assignDepartment(member: TeamMember): string {
  const haystack = `${member.role} ${member.name}`.toLowerCase();
  for (const hint of ROLE_HINTS) {
    if (hint.words.some((word) => haystack.includes(word))) return hint.department;
  }
  return "dept-ops";
}

/**
 * Which tools a teammate uses.
 *
 * **Derived.** The host knows the company's tools but not who reaches for
 * which, so each teammate is given the tools of their department's slice — a
 * deterministic deal from the company-wide list, not a record of anything.
 */
// (member is not consulted: the assignment is positional, not personal)
export function assignTools(index: number, tools: string[]): string[] {
  if (tools.length === 0) return [];
  const take = Math.min(3, Math.max(1, Math.ceil(tools.length / 3)));
  return Array.from({ length: take }, (_, k) => tools[(index * 2 + k) % tools.length]).filter(
    (tool, k, all) => all.indexOf(tool) === k,
  );
}

/**
 * One standing routine per department.
 *
 * **Derived.** The console has no flow API — the Workflows canvas draws a
 * single hard-coded sample — so these are plausible routines for each area
 * rather than anything the company declared. They exist to show the shape a
 * real flow will take on the graph: a department runs a flow, and the flow
 * passes through several of that department's agents in turn.
 */
const WORKFLOW_TEMPLATES: Omit<Workflow, "agentIds">[] = [
  {
    id: "flow-discovery",
    departmentId: "dept-product",
    name: "Discovery loop",
    summary: "Turn a raw request into a specced, prioritised piece of work.",
    stages: ["Intake", "Research", "Spec", "Prioritise"],
  },
  {
    id: "flow-delivery",
    departmentId: "dept-engineering",
    name: "Ship it",
    summary: "Take a spec through build, review, and release.",
    stages: ["Plan", "Build", "Review", "Release"],
  },
  {
    id: "flow-brand",
    departmentId: "dept-design",
    name: "Make it look right",
    summary: "Draft, critique, and hand off the visuals for a piece of work.",
    stages: ["Brief", "Draft", "Critique", "Hand off"],
  },
  {
    id: "flow-campaign",
    departmentId: "dept-growth",
    name: "Campaign run",
    summary: "Angle to published post, with the numbers read back after.",
    stages: ["Angle", "Write", "Publish", "Measure"],
  },
  {
    id: "flow-triage",
    departmentId: "dept-ops",
    name: "Inbound triage",
    summary: "Sort what arrives, answer it, and escalate what needs a person.",
    stages: ["Receive", "Sort", "Answer", "Escalate"],
  },
];

/**
 * Which department a human belongs to.
 *
 * **Derived.** Signing in tells the console who someone is, not what they work
 * on. Admins sit with Operations — administering the company *is* operations —
 * and everyone else is spread deterministically across the departments that
 * exist, so humans read as part of the org rather than piled on one pillar.
 */
export function assignHumanDepartment(person: HostPerson, departments: Department[]): string {
  if (person.role === "admin") return "dept-ops";
  if (departments.length === 0) return "dept-ops";
  return departments[hash(person.email) % departments.length].id;
}

export interface AdaptInput {
  members: TeamMember[];
  tasks: Task[];
  skills: Skill[];
  servers: McpServer[];
  toolsByServer: Record<string, McpTool[]>;
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
  const toolLabels: Record<string, string> = {};
  const toolSlugs: string[] = [];

  for (const skill of input.skills) {
    const slug = `skill-${skill.id}`;
    toolLabels[slug] = skill.name;
    toolSlugs.push(slug);
  }
  for (const server of input.servers) {
    for (const tool of input.toolsByServer[server.server_id] ?? []) {
      const slug = `mcp-${server.server_id}-${tool.name}`;
      toolLabels[slug] = tool.name;
      toolSlugs.push(slug);
    }
  }

  const agents: Agent[] = input.members.map((member, i) => ({
    id: member.id,
    departmentId: assignDepartment(member),
    name: member.name,
    role: member.role,
    status: "active",
    tier: "worker",
    description: member.description,
    model: "—",
    tools: assignTools(i, toolSlugs),
    parentId: null,
    instance: "builtin",
  }));

  // A board card becomes an SOP task owned by the teammate it is assigned to —
  // the one edge here that the host actually records. Cards nobody owns are
  // dropped rather than parked under an invented owner.
  const tasks: SopTask[] = [];
  for (const task of input.tasks) {
    const member = input.members.find((m) => input.ownedBy(task, m));
    if (!member) continue;
    tasks.push({
      id: task.id,
      departmentId: assignDepartment(member),
      title: task.title,
      summary: task.note ?? "",
      steps: [
        `Column: ${TASK_COLUMNS.find((c) => c.id === task.column)?.label ?? task.column}`,
        `Priority: ${task.priority}`,
        `Owner: ${task.assignee}`,
      ],
      assigneeKind: "agent",
      assigneeId: member.id,
    });
  }

  // Only departments that ended up with somebody in them.
  const used = new Set(agents.map((a) => a.departmentId));
  const departments = DEPARTMENTS.filter((d) => used.has(d.id));

  const people: Person[] = input.people.map((p) => ({
    id: p.id,
    departmentId: assignHumanDepartment(p, departments),
    // Falling back to the local part of the address keeps a real name off the
    // graph only when nobody has set one.
    name: p.displayName?.trim() || p.email.split("@")[0],
    role: p.role === "admin" ? "Admin" : "Member",
    // Humans get no derived tool shelf: an operator's tools are their browser,
    // and inventing an MCP loadout for a person would read as a claim.
    tools: [],
  }));

  // A flow only makes sense where its department has agents to run it; each is
  // wired through that department's agents in roster order.
  const workflows: Workflow[] = WORKFLOW_TEMPLATES.filter((f) => used.has(f.departmentId)).map(
    (f) => ({
      ...f,
      agentIds: agents.filter((a) => a.departmentId === f.departmentId).map((a) => a.id),
    }),
  );

  return { departments, agents, people, workflows, tasks, toolLabels };
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
  const kinds = [...new Set(entries.map((e) => e.kind))];

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
    const k = kinds.indexOf(entry.kind);
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
      folder: entry.kind,
      kind: entry.kind,
      excerpt: entry.body,
      wordCount: entry.body.split(/\s+/).filter(Boolean).length,
      tags: [entry.source],
      agents: [],
      vx: clamp(Math.cos(angle) * 0.4 + Math.cos(spin) * spread),
      vy: clamp(Math.sin(angle) * 0.4 + Math.sin(spin) * spread),
      vector: [],
      chunks: 1,
    });
    edges.push({ source: entry.id, target: `folder:${entry.kind}`, type: "member" });
    const siblings = byKind.get(entry.kind);
    if (siblings) {
      edges.push({ source: entry.id, target: siblings[siblings.length - 1], type: "similar" });
      siblings.push(entry.id);
    } else {
      byKind.set(entry.kind, [entry.id]);
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
