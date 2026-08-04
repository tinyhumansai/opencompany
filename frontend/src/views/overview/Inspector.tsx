// The panel beside the graph. It always describes whatever the camera is on,
// so diving reads as one gesture rather than two surfaces to reconcile.

import { ArrowRight, ChevronLeft, ShieldCheck } from "lucide-react";

import type { Task } from "@/api/tasks";
import type { Skill } from "@/api/skills";
import type { ApprovalSummary, CompanyStatus } from "@/api/types";
import type { View } from "@/components/app-shell";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { approvalSummary, money, timeAgo } from "@/lib/language";
import type { McpServer, McpTool } from "@/lib/mcp";
import { KIND_STYLES, type MemoryEntry } from "@/lib/memory";
import { PRIORITY_STYLES, type TaskPriority } from "@/lib/tasks-sample";
import { initials, TEAM_TONES, type TeamMember } from "@/lib/team";
import { cn } from "@/lib/utils";
import { KIND_ICON } from "./AgentGraph";
import { BRANCH_MARK } from "./palette";
import { BRANCH_OF, COMPANY_ID, type Graph, type GraphNode, type NodeKind } from "./graph";

/** Which console surface each kind belongs to, for the "open in full" button. */
const SURFACE: Record<NodeKind, { view: View; label: string }> = {
  company: { view: "approvals", label: "Approvals" },
  memory: { view: "memory", label: "Open memory" },
  desk: { view: "team", label: "Open team" },
  card: { view: "tasks", label: "Open board" },
  capability: { view: "skills", label: "Open skills" },
  skill: { view: "skills", label: "Open skills" },
  server: { view: "mcp", label: "Open MCP servers" },
  tool: { view: "mcp", label: "Open MCP servers" },
};

interface Props {
  graph: Graph;
  focusId: string;
  status: CompanyStatus;
  approvals: ApprovalSummary[];
  openCards: number;
  now: number;
  onFocus: (id: string) => void;
  onNavigate: (view: View) => void;
}

export function Inspector(props: Props) {
  const node = props.graph.byId.get(props.focusId);
  if (!node || node.id === COMPANY_ID) return <CompanyPane {...props} />;
  return <NodePane {...props} node={node} />;
}

function CompanyPane({ graph, status, approvals, openCards, now, onFocus, onNavigate }: Props) {
  const hubs = graph.hubs.map((id) => graph.byId.get(id)!).filter(Boolean);
  const memories = graph.nodes.filter((n) => n.kind === "memory");

  return (
    <Shell
      kind="company"
      title={status.name}
      sub={`${graph.nodes.length} nodes · ${status.lifecycle}`}
    >
      <dl className="grid grid-cols-2 gap-2">
        <Fact label="Open cards" value={String(openCards)} />
        <Fact label="Waiting on you" value={String(status.pending_approvals)} />
      </dl>

      {memories.length > 0 && (
        <Section label={`Memory core · ${memories.length}`}>
          <NodeList nodes={memories.slice(0, 6)} onFocus={onFocus} />
          <Button variant="outline" size="sm" className="w-full" onClick={() => onNavigate("memory")}>
            Open memory <ArrowRight className="size-4" />
          </Button>
        </Section>
      )}

      <Section label="Parked for your decision">
        {approvals.length === 0 ? (
          <p className="flex items-center gap-2 rounded-lg border border-dashed p-2.5 text-xs text-muted-foreground">
            <ShieldCheck className="size-3.5 shrink-0 text-[#008300]" />
            All clear — nothing needs your approval.
          </p>
        ) : (
          <>
            <ul className="divide-y">
              {approvals.slice(0, 3).map((a) => (
                <li key={a.id} className="flex items-center gap-2 py-1.5 text-xs first:pt-0">
                  <span className="min-w-0 flex-1 truncate">{approvalSummary(a)}</span>
                  <span className="shrink-0 text-[11px] text-muted-foreground">
                    {a.amount_usd != null && (
                      <span className="font-medium text-foreground">{money(a.amount_usd)} · </span>
                    )}
                    {timeAgo(a.at_millis, now)}
                  </span>
                </li>
              ))}
            </ul>
            <Button variant="outline" size="sm" className="w-full" onClick={() => onNavigate("approvals")}>
              Review all approvals <ArrowRight className="size-4" />
            </Button>
          </>
        )}
      </Section>

      <Section label="Directory">
        <NodeList nodes={hubs} onFocus={onFocus} />
      </Section>

      <p className="text-[11px] leading-relaxed text-muted-foreground">
        Hover a node to light its chain. Click to dive in — Escape, or the empty
        field, dives back out.
      </p>
    </Shell>
  );
}

function NodePane({ graph, node, now, onFocus, onNavigate }: Props & { node: GraphNode }) {
  const children = (graph.children.get(node.id) ?? [])
    .map((id) => graph.byId.get(id)!)
    .filter(Boolean);
  const parent = node.parent ? graph.byId.get(node.parent) : undefined;
  const surface = SURFACE[node.kind];

  return (
    <Shell kind={node.kind} title={node.label} sub={node.sub} avatar={avatarFor(node)}>
      {parent && (
        <button
          type="button"
          onClick={() => onFocus(parent.id)}
          className="-mt-1 flex items-center gap-1 self-start text-[11px] text-muted-foreground transition-colors hover:text-foreground"
        >
          <ChevronLeft className="size-3" />
          {parent.label}
        </button>
      )}

      <Detail node={node} now={now} />

      {children.length > 0 && (
        <Section label={childLabel(node.kind, children.length)}>
          <NodeList nodes={children} onFocus={onFocus} />
        </Section>
      )}

      <Button variant="outline" size="sm" className="w-full" onClick={() => onNavigate(surface.view)}>
        {surface.label} <ArrowRight className="size-4" />
      </Button>
    </Shell>
  );
}

/** The kind-specific body: whatever the host actually recorded about it. */
function Detail({ node, now }: { node: GraphNode; now: number }) {
  switch (node.kind) {
    case "desk": {
      const member = node.payload as TeamMember;
      return member.description ? (
        <p className="text-xs text-muted-foreground">{member.description}</p>
      ) : null;
    }
    case "card": {
      const task = node.payload as Task;
      return (
        <div className="space-y-2">
          {task.note && <p className="text-xs text-muted-foreground">{task.note}</p>}
          <div className="flex flex-wrap gap-1.5">
            <Badge
              variant="outline"
              className={cn("text-[10px]", PRIORITY_STYLES[task.priority as TaskPriority])}
            >
              {task.priority} priority
            </Badge>
            <Badge variant="secondary" className="text-[10px]">
              updated {timeAgo(task.updatedAt, now)}
            </Badge>
          </div>
        </div>
      );
    }
    case "memory": {
      const entry = node.payload as MemoryEntry;
      return (
        <div className="space-y-2">
          <p className="text-xs leading-relaxed text-muted-foreground">{entry.body}</p>
          <div className="flex flex-wrap gap-1.5">
            <Badge variant="outline" className={cn("text-[10px]", KIND_STYLES[entry.kind])}>
              {entry.kind}
            </Badge>
            <Badge variant="secondary" className="text-[10px]">
              captured by {entry.source}
            </Badge>
            <Badge variant="secondary" className="text-[10px]">
              updated {timeAgo(entry.updatedAt, now)}
            </Badge>
          </div>
        </div>
      );
    }
    case "skill": {
      const skill = node.payload as Skill;
      return (
        <div className="space-y-2">
          <p className="text-xs text-muted-foreground">{skill.description}</p>
          <Badge variant="secondary" className="text-[10px]">
            from {skill.source}
          </Badge>
        </div>
      );
    }
    case "server": {
      const server = node.payload as McpServer;
      return (
        <div className="space-y-2">
          <p className="font-mono text-[11px] text-muted-foreground">{server.transport}</p>
          {server.last_error && (
            <p className="rounded-lg border border-dashed p-2 text-[11px] text-muted-foreground">
              {server.last_error}
            </p>
          )}
        </div>
      );
    }
    case "tool": {
      const tool = node.payload as McpTool;
      return tool.description ? (
        <p className="text-xs text-muted-foreground">{tool.description}</p>
      ) : null;
    }
    default:
      return null;
  }
}

function NodeList({ nodes, onFocus }: { nodes: GraphNode[]; onFocus: (id: string) => void }) {
  return (
    <ul className="max-h-64 space-y-1 overflow-y-auto pr-1">
      {nodes.map((child) => {
        const Icon = KIND_ICON[child.kind];
        return (
          <li key={child.id}>
            <button
              type="button"
              onClick={() => onFocus(child.id)}
              className="flex w-full items-center gap-2 rounded-lg border px-2.5 py-1.5 text-left text-xs transition-colors hover:bg-accent/40"
            >
              <Icon className={cn("size-3.5 shrink-0", BRANCH_MARK[BRANCH_OF[child.kind]])} />
              <span className="min-w-0 flex-1 truncate">{child.label}</span>
              <span className="shrink-0 font-mono text-[10px] text-muted-foreground">
                {child.sub}
              </span>
            </button>
          </li>
        );
      })}
    </ul>
  );
}

function Shell({
  kind,
  title,
  sub,
  avatar,
  children,
}: {
  kind: NodeKind;
  title: string;
  sub: string;
  avatar?: React.ReactNode;
  children: React.ReactNode;
}) {
  const Icon = KIND_ICON[kind];
  return (
    <div className="flex max-h-full w-72 flex-col gap-3 overflow-y-auto rounded-xl border bg-card/90 p-3.5 backdrop-blur">
      <div className="flex items-start gap-2.5">
        {avatar ?? (
          <span className="grid size-8 shrink-0 place-items-center rounded-lg border">
            <Icon className={cn("size-4", BRANCH_MARK[BRANCH_OF[kind]])} />
          </span>
        )}
        <div className="min-w-0 space-y-0.5">
          <h3 className="truncate text-sm font-semibold tracking-tight">{title}</h3>
          <p className="truncate font-mono text-[10px] text-muted-foreground">{sub}</p>
        </div>
      </div>
      {children}
    </div>
  );
}

function Section({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="space-y-1.5">
      <h4 className="font-mono text-[10px] uppercase tracking-[0.16em] text-muted-foreground">
        {label}
      </h4>
      {children}
    </div>
  );
}

function Fact({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded-lg border px-2.5 py-1.5">
      <dt className="font-mono text-[9.5px] uppercase tracking-[0.16em] text-muted-foreground">
        {label}
      </dt>
      <dd className="truncate text-sm font-semibold tabular-nums">{value}</dd>
    </div>
  );
}

function avatarFor(node: GraphNode): React.ReactNode {
  if (node.kind !== "desk") return undefined;
  const member = node.payload as TeamMember;
  return (
    <span
      className={cn(
        "grid size-8 shrink-0 place-items-center rounded-full text-[11px] font-semibold",
        TEAM_TONES[member.tone] ?? TEAM_TONES.sky,
      )}
    >
      {initials(member.name)}
    </span>
  );
}

function childLabel(kind: NodeKind, count: number): string {
  const noun =
    kind === "desk" ? "cards" : kind === "capability" ? "skills" : kind === "server" ? "tools" : "children";
  return `${count} ${noun}`;
}
