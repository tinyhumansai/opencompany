// The panel beside the map. It always describes whatever the map is centred
// on, so diving in reads as one gesture rather than two surfaces to reconcile.

import { ArrowRight, ShieldCheck } from "lucide-react";

import type { ApprovalSummary } from "@/api/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { PRIORITY_STYLES, TASK_COLUMNS, type TaskPriority } from "@/lib/tasks-sample";
import { approvalSummary, money, timeAgo } from "@/lib/language";
import { TEAM_TONES, initials } from "@/lib/team";
import { cn } from "@/lib/utils";
import type { MapFocus, MapMember } from "./types";

interface Props {
  focus: MapFocus;
  members: MapMember[];
  companyName: string;
  lifecycle: string;
  openTasks: number;
  approvals: ApprovalSummary[];
  now: number;
  onFocus: (focus: MapFocus) => void;
  onOpenTasks: () => void;
  onOpenApprovals: () => void;
  onOpenTeam: () => void;
}

export function FocusPanel(props: Props) {
  const { focus, members } = props;

  if (focus.kind === "company") return <CompanyPane {...props} />;

  const member =
    focus.kind === "member"
      ? members.find((m) => m.id === focus.id)
      : members.find((m) => m.tasks.some((t) => t.id === focus.id));

  // A focus can outlive its subject across a poll — fall back rather than
  // rendering a blank pane.
  if (!member) return <CompanyPane {...props} />;

  if (focus.kind === "task") {
    const task = member.tasks.find((t) => t.id === focus.id);
    if (task) return <TaskPane {...props} member={member} task={task} />;
  }
  return <MemberPane {...props} member={member} />;
}

function CompanyPane({
  companyName,
  lifecycle,
  members,
  openTasks,
  approvals,
  now,
  onOpenApprovals,
}: Props) {
  return (
    <Pane title={companyName} sub={`Everything at once — ${lifecycle}.`}>
      <dl className="grid grid-cols-2 gap-3">
        <Fact label="Teammates" value={String(members.length)} />
        <Fact label="Open cards" value={String(openTasks)} />
        <Fact label="Waiting on you" value={String(approvals.length)} />
        <Fact label="Busiest" value={busiest(members)} />
      </dl>

      <div className="space-y-2">
        <h4 className="font-mono text-[10px] uppercase tracking-[0.16em] text-muted-foreground">
          Parked for your decision
        </h4>
        {approvals.length === 0 ? (
          <p className="flex items-center gap-2 rounded-lg border border-dashed p-3 text-sm text-muted-foreground">
            <ShieldCheck className="size-4 text-[#008300]" />
            All clear — nothing needs your approval.
          </p>
        ) : (
          <>
            <ul className="divide-y">
              {approvals.slice(0, 4).map((a) => (
                <li key={a.id} className="flex items-center gap-3 py-2 text-sm first:pt-0">
                  <span className="min-w-0 flex-1 truncate">{approvalSummary(a)}</span>
                  <span className="shrink-0 text-xs text-muted-foreground">
                    {a.amount_usd != null && (
                      <span className="font-medium text-foreground">{money(a.amount_usd)} · </span>
                    )}
                    {timeAgo(a.at_millis, now)}
                  </span>
                </li>
              ))}
            </ul>
            <Button variant="outline" size="sm" className="w-full" onClick={onOpenApprovals}>
              Review all approvals <ArrowRight className="size-4" />
            </Button>
          </>
        )}
      </div>

      <p className="text-xs text-muted-foreground">
        Click a teammate on the map to dive into their work. Escape dives back out.
      </p>
    </Pane>
  );
}

function MemberPane({ member, onFocus, onOpenTeam, onOpenTasks }: Props & { member: MapMember }) {
  const tone = TEAM_TONES[member.tone] ?? TEAM_TONES.sky;
  return (
    <Pane
      title={member.name}
      sub={member.role}
      avatar={
        <span className={cn("grid size-9 shrink-0 place-items-center rounded-full text-xs font-semibold", tone)}>
          {initials(member.name)}
        </span>
      }
    >
      {member.description && <p className="text-sm text-muted-foreground">{member.description}</p>}

      <dl className="grid grid-cols-2 gap-3">
        <Fact label="Open" value={String(member.open)} />
        <Fact label="Cards held" value={String(member.tasks.length)} />
      </dl>

      <div className="space-y-2">
        <h4 className="font-mono text-[10px] uppercase tracking-[0.16em] text-muted-foreground">
          Their cards
        </h4>
        {member.tasks.length === 0 ? (
          <p className="rounded-lg border border-dashed p-3 text-sm text-muted-foreground">
            Nothing assigned to them yet.
          </p>
        ) : (
          <ul className="space-y-1">
            {member.tasks.map((t) => (
              <li key={t.id}>
                <button
                  type="button"
                  onClick={() => onFocus({ kind: "task", id: t.id })}
                  className="flex w-full items-center gap-2 rounded-lg border px-3 py-2 text-left text-sm transition-colors hover:bg-accent/40"
                >
                  <span className="min-w-0 flex-1 truncate">{t.title}</span>
                  <Badge variant="secondary" className="shrink-0 text-[10px]">
                    {columnLabel(t.column)}
                  </Badge>
                </button>
              </li>
            ))}
          </ul>
        )}
      </div>

      <div className="flex gap-2">
        <Button variant="outline" size="sm" className="flex-1" onClick={onOpenTeam}>
          Team
        </Button>
        <Button variant="outline" size="sm" className="flex-1" onClick={onOpenTasks}>
          Board
        </Button>
      </div>
    </Pane>
  );
}

function TaskPane({
  member,
  task,
  now,
  onFocus,
  onOpenTasks,
}: Props & { member: MapMember; task: MapMember["tasks"][number] }) {
  return (
    <Pane title={task.title} sub={`${member.name} · ${columnLabel(task.column)}`}>
      {task.note && <p className="text-sm text-muted-foreground">{task.note}</p>}

      <div className="flex flex-wrap gap-2">
        <Badge variant="outline" className={cn("text-[11px]", PRIORITY_STYLES[task.priority as TaskPriority])}>
          {task.priority} priority
        </Badge>
        <Badge variant="secondary" className="text-[11px]">
          updated {timeAgo(task.updatedAt, now)}
        </Badge>
      </div>

      <div className="flex gap-2">
        <Button
          variant="outline"
          size="sm"
          className="flex-1"
          onClick={() => onFocus({ kind: "member", id: member.id })}
        >
          Back to {member.name}
        </Button>
        <Button variant="outline" size="sm" className="flex-1" onClick={onOpenTasks}>
          Open board
        </Button>
      </div>
    </Pane>
  );
}

function Pane({
  title,
  sub,
  avatar,
  children,
}: {
  title: string;
  sub: string;
  avatar?: React.ReactNode;
  children: React.ReactNode;
}) {
  return (
    <div className="flex h-full flex-col gap-4 rounded-xl border bg-card p-4">
      <div className="flex items-start gap-3">
        {avatar}
        <div className="min-w-0 space-y-0.5">
          <h3 className="truncate text-base font-semibold tracking-tight">{title}</h3>
          <p className="truncate text-xs text-muted-foreground">{sub}</p>
        </div>
      </div>
      {children}
    </div>
  );
}

function Fact({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded-lg border px-3 py-2">
      <dt className="font-mono text-[10px] uppercase tracking-[0.16em] text-muted-foreground">
        {label}
      </dt>
      <dd className="truncate text-sm font-semibold tabular-nums">{value}</dd>
    </div>
  );
}

/**
 * Who is carrying the most. Names nobody when nobody holds open work — an
 * idle board has no busiest teammate, and picking one would read as a claim.
 */
function busiest(members: MapMember[]): string {
  const top = members.reduce<MapMember | null>(
    (best, m) => (m.open > (best?.open ?? 0) ? m : best),
    null,
  );
  return top?.name ?? "—";
}

function columnLabel(column: string): string {
  return TASK_COLUMNS.find((c) => c.id === column)?.label ?? column;
}
