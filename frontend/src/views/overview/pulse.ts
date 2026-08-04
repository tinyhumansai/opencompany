// SPDX-License-Identifier: GPL-3.0-or-later

// The two predicates the graph adapter needs from the board.
//
// Pure: no fetching, no clock, no randomness — so the same cards always shape
// the same graph.

import type { Task } from "@/api/tasks";
import type { TeamMember } from "@/lib/team";

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
