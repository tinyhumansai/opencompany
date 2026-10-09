import type { ReactNode } from "react";

import { TeammateAvatar } from "@/components/teammate-avatar";
import type { TeamMemberDto } from "@/api/types";
import { avatarFor, teammateName } from "@/lib/team";

/** One teammate row in an access editor. */
export interface AgentAccessRow {
  id: string;
  /** The display name when the caller already has one; else read from `team`. */
  name?: string;
  /** Whether the box is ticked, draft included. */
  ticked: boolean;
  /** The box cannot move; `reason` says why. */
  locked?: boolean;
  reason?: ReactNode;
  /** Whether the teammate has access now, regardless of the draft. */
  holds: boolean;
  holdsLabel?: { yes: string; no: string };
  /** The trailing note, usually a link to the teammate's own page. */
  detail?: ReactNode;
}

/**
 * A per-teammate checkbox list shared by the skill page and the MCP server
 * page. Rendering only: the caller owns the draft and the save.
 */
export function AgentAccessList({
  rows,
  team,
  editable,
  disabled,
  listTestId,
  rowTestIdPrefix,
  onToggle,
}: {
  rows: readonly AgentAccessRow[];
  team: TeamMemberDto[] | null;
  editable: boolean;
  disabled: boolean;
  listTestId: string;
  /** `<prefix>-agent-toggle-<id>`, `<prefix>-agent-reach-<id>`, … */
  rowTestIdPrefix: string;
  onToggle: (id: string, on: boolean) => void;
}) {
  return (
    <div className="divide-y rounded-lg border" data-testid={listTestId}>
      {rows.map((row) => {
        const name = row.name ?? teammateName(row.id, team);
        const labels = row.holdsLabel ?? { yes: "Reached", no: "Not reached" };
        return (
          <div
            key={row.id}
            className={
              row.locked
                ? "flex flex-col gap-1 px-3 py-2 opacity-60"
                : "flex flex-col gap-1 px-3 py-2"
            }
            data-testid={`${rowTestIdPrefix}-agent-${row.id}`}
          >
            <div className="flex items-center justify-between gap-3">
              <label
                className="flex min-w-0 items-center gap-2"
                htmlFor={`${rowTestIdPrefix}-agent-input-${row.id}`}
              >
                {editable ? (
                  <input
                    type="checkbox"
                    id={`${rowTestIdPrefix}-agent-input-${row.id}`}
                    checked={row.ticked}
                    disabled={disabled || row.locked}
                    data-testid={`${rowTestIdPrefix}-agent-toggle-${row.id}`}
                    onChange={(e) => onToggle(row.id, e.target.checked)}
                  />
                ) : null}
                <TeammateAvatar
                  name={name}
                  avatar={avatarFor(row.id)}
                  className="size-6 shrink-0"
                />
                <span className="min-w-0 truncate text-sm">{name}</span>
              </label>
              <div className="flex shrink-0 items-center gap-2">
                <span
                  className={
                    row.holds
                      ? "text-2xs text-status-done-text"
                      : "text-2xs text-muted-foreground"
                  }
                  data-testid={`${rowTestIdPrefix}-agent-reach-${row.id}`}
                >
                  {row.holds ? labels.yes : labels.no}
                </span>
                {row.detail}
              </div>
            </div>
            {row.reason ? (
              <p
                className="pl-6 text-2xs text-muted-foreground"
                data-testid={`${rowTestIdPrefix}-agent-reason-${row.id}`}
              >
                {row.reason}
              </p>
            ) : null}
          </div>
        );
      })}
    </div>
  );
}
