// Pure derivations behind the Overview's chrome — the state line and the live
// strip. The graph's own derivations live in `graph.ts`.
//
// Nothing here fetches, reads a clock, or randomises. `now` is injected so a
// render is reproducible and the values can be asserted directly.

import type { Task } from "@/api/tasks";
import type { ApprovalSummary } from "@/api/types";
import type { TeamMember } from "@/lib/team";

import type { StateChip, TickerItem } from "./types";

/** Columns whose cards are finished work rather than work in flight. */
const CLOSED_COLUMNS = new Set(["done"]);

/** Whether a card still counts as work the company owes someone. */
export function isOpen(task: Task): boolean {
  return !CLOSED_COLUMNS.has(task.column);
}

/**
 * Whether a card belongs to a roster member.
 *
 * The board stores `assignee` as a free label, so a card may name the agent id,
 * the display name, or the role. All three are accepted rather than dropping
 * work on the floor because the operator typed the role.
 */
export function ownedBy(task: Task, member: TeamMember): boolean {
  const label = task.assignee?.trim().toLowerCase();
  if (!label) return false;
  return (
    label === member.id.toLowerCase() ||
    label === member.name.toLowerCase() ||
    label === member.role.toLowerCase()
  );
}

/**
 * The honest one-liner beside the company name: what is actually true right
 * now, strongest signal first. Nothing is padded — a quiet company gets a short
 * line, not a fabricated one.
 */
export function stateOfWorld(input: {
  lifecycle: string;
  pendingApprovals: number;
  openTasks: number;
  inProgress: number;
  members: number;
  enabledSkills: number;
}): StateChip[] {
  const chips: StateChip[] = [];

  if (input.pendingApprovals > 0) {
    chips.push({ tone: "warn", text: `${input.pendingApprovals} waiting on you` });
  }

  if (input.lifecycle === "running") {
    chips.push({
      tone: input.inProgress > 0 ? "busy" : "ok",
      text: input.inProgress > 0 ? `${input.inProgress} in flight` : "idle, ready for work",
    });
  } else {
    chips.push({ tone: "dim", text: `company ${input.lifecycle}` });
  }

  if (input.openTasks > 0) {
    chips.push({ tone: "dim", text: `${input.openTasks} open on the board` });
  }

  chips.push({
    tone: "dim",
    text: `${input.members} on the team · ${input.enabledSkills} skills equipped`,
  });

  return chips;
}

/**
 * The live ticker: the most recent real movements, newest first.
 *
 * Approvals lead — they are the only items that block a human — then the
 * board's most recently touched cards.
 */
export function tickerItems(
  tasks: Task[],
  approvals: ApprovalSummary[],
  limit = 10,
): TickerItem[] {
  const fromApprovals: TickerItem[] = approvals.map((a) => ({
    id: `approval-${a.id}`,
    mark: "WAITING",
    tone: "warn" as const,
    subject: a.kind,
    detail: a.amount_usd != null ? `$${a.amount_usd.toLocaleString()}` : "needs your decision",
    atMillis: a.at_millis,
  }));

  const fromTasks: TickerItem[] = [...tasks]
    .sort((a, b) => b.updatedAt - a.updatedAt)
    .map((t) => ({
      id: `task-${t.id}`,
      mark: isOpen(t) ? "MOVED" : "DONE",
      tone: isOpen(t) ? ("busy" as const) : ("ok" as const),
      subject: t.assignee || "unassigned",
      detail: t.title,
      atMillis: t.updatedAt,
    }));

  return [...fromApprovals, ...fromTasks]
    .sort((a, b) => b.atMillis - a.atMillis)
    .slice(0, limit);
}
