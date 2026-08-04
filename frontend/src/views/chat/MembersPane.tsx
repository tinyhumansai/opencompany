import { Mail, MessageSquare, MoreHorizontal, UserPlus, Wallet } from "lucide-react";

import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuCheckboxItem,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Skeleton } from "@/components/ui/skeleton";
import type { TeamMember } from "@/lib/team";
import { cn } from "@/lib/utils";
import { Avatar } from "./Avatar";

interface Props {
  members: TeamMember[];
  loading: boolean;
  /** True when the roster came from the host rather than the starter set. */
  fromHost: boolean;
  /**
   * Give this teammate an inbox, or take it away. Whether they have one is read
   * from the roster (`member.inboxEnabled`), never guessed client-side, so this
   * pane and the Inbox page agree on the same host state (issue #173).
   */
  onToggleInbox: (member: TeamMember) => void;
  onRemove: (id: string) => void;
  onAdd: () => void;
  onMessage: (member: TeamMember) => void;
  /**
   * Whether to offer the daily-budget controls at all (issue #360, ported
   * from the retired Team page): admin-only, and only for a host-backed
   * teammate — a starter-roster row is a local placeholder with no budget
   * record to edit.
   */
  canEditBudget: boolean;
  onEditBudget: (member: TeamMember) => void;
  onRemoveCap: (member: TeamMember) => void;
  onResetBudget: (member: TeamMember) => void;
  /** Who set a teammate's cap override, resolved to a display label. */
  setByLabel: (member: TeamMember) => string | undefined;
}

/**
 * The right-hand member pane — the company's roster, where a chat workspace
 * expects it.
 *
 * This replaces the standalone Team page: everything that page could do lives
 * on a row here (give an agent an inbox, drop them from the roster) or on the
 * Add button, and a row now also opens that teammate's DM, which the page
 * could not do at all.
 */
export function MembersPane({
  members,
  loading,
  fromHost,
  onToggleInbox,
  onRemove,
  onAdd,
  onMessage,
  canEditBudget,
  onEditBudget,
  onRemoveCap,
  onResetBudget,
  setByLabel,
}: Props) {
  return (
    <aside className="flex w-72 shrink-0 flex-col border-l bg-background">
      <header className="flex h-13 shrink-0 items-center gap-2 border-b px-3">
        <div className="min-w-0 flex-1">
          <h2 className="text-sm font-semibold tracking-tight">Team</h2>
          <p className="truncate text-xs text-muted-foreground">
            {loading
              ? "Loading…"
              : `${members.length} ${members.length === 1 ? "agent" : "agents"} · ${
                  fromHost ? "defined by this company" : "starter roster"
                }`}
          </p>
        </div>
        <Button
          variant="ghost"
          size="icon"
          className="size-8"
          onClick={onAdd}
          aria-label="Add member"
          title="Add member"
        >
          <UserPlus className="size-4" />
        </Button>
      </header>

      <div className="flex-1 overflow-y-auto p-2">
        {loading ? (
          <div className="space-y-2 p-1">
            {Array.from({ length: 6 }).map((_, i) => (
              <Skeleton key={i} className="h-12 rounded-lg" />
            ))}
          </div>
        ) : (
          <ul className="flex flex-col gap-px">
            {members.map((m) => (
              <li key={m.id}>
                <MemberRow
                  member={m}
                  inboxOn={m.inboxEnabled}
                  onToggleInbox={() => onToggleInbox(m)}
                  onRemove={() => onRemove(m.id)}
                  onMessage={() => onMessage(m)}
                  canEditBudget={canEditBudget}
                  onEditBudget={() => onEditBudget(m)}
                  onRemoveCap={() => onRemoveCap(m)}
                  onResetBudget={() => onResetBudget(m)}
                  setByLabel={setByLabel(m)}
                />
              </li>
            ))}
          </ul>
        )}
      </div>
    </aside>
  );
}

function MemberRow({
  member,
  inboxOn,
  onToggleInbox,
  onRemove,
  onMessage,
  canEditBudget,
  onEditBudget,
  onRemoveCap,
  onResetBudget,
  setByLabel,
}: {
  member: TeamMember;
  inboxOn: boolean;
  onToggleInbox: () => void;
  onRemove: () => void;
  onMessage: () => void;
  canEditBudget: boolean;
  onEditBudget: () => void;
  onRemoveCap: () => void;
  onResetBudget: () => void;
  setByLabel?: string;
}) {
  const capped = member.budgetUsdDaily !== undefined;
  const overridden = member.budgetSetBy !== undefined;

  return (
    <div className="group/member flex items-center gap-2.5 rounded-lg px-2 py-1.5 transition-colors hover:bg-accent/60">
      <button
        type="button"
        onClick={onMessage}
        className="flex min-w-0 flex-1 items-center gap-2.5 text-left"
        title={member.description || member.role}
      >
        <Avatar name={member.name} tone={member.tone} className="size-8" />
        <span className="min-w-0">
          <span className="flex items-center gap-1">
            <span className="truncate text-sm font-medium">{member.name}</span>
            {inboxOn && (
              <Mail className="size-3 shrink-0 text-muted-foreground" aria-label="Has an inbox" />
            )}
          </span>
          <span className="block truncate text-xs text-muted-foreground">{member.role}</span>
          <DailyBudgetLine member={member} setByLabel={setByLabel} />
        </span>
      </button>

      <DropdownMenu>
        <DropdownMenuTrigger
          render={
            <Button
              variant="ghost"
              size="icon"
              className="size-7 shrink-0 opacity-0 transition-opacity focus-visible:opacity-100 group-hover/member:opacity-100"
              aria-label={`Actions for ${member.name}`}
            />
          }
        >
          <MoreHorizontal className="size-4" />
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end">
          <DropdownMenuItem onClick={onMessage}>
            <MessageSquare className="size-4" /> Message
          </DropdownMenuItem>
          <DropdownMenuCheckboxItem checked={inboxOn} onCheckedChange={onToggleInbox}>
            Give this agent an inbox
          </DropdownMenuCheckboxItem>
          {canEditBudget && (
            <>
              <DropdownMenuSeparator />
              <DropdownMenuItem onClick={onEditBudget} data-testid="team-budget-edit">
                <Wallet className="size-4" />
                {capped ? "Change daily budget…" : "Set daily budget…"}
              </DropdownMenuItem>
              {capped && (
                <DropdownMenuItem onClick={onRemoveCap} data-testid="team-budget-remove">
                  Remove cap
                </DropdownMenuItem>
              )}
              {overridden && (
                <DropdownMenuItem onClick={onResetBudget} data-testid="team-budget-reset">
                  Reset to company default
                </DropdownMenuItem>
              )}
            </>
          )}
          <DropdownMenuSeparator />
          <DropdownMenuItem variant="destructive" onClick={onRemove}>
            Remove from roster
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
    </div>
  );
}

/**
 * The teammate's daily spend line, ported from the retired Team page (issue
 * #360). Renders nothing at all for an uncapped teammate: the host omits the
 * fields entirely rather than sending zeros, so absence means "spends freely"
 * and must not be drawn as "$0.00/day". Once spend reaches the cap the line
 * turns destructive — that teammate's dispatch is paused until 00:00 UTC.
 */
function DailyBudgetLine({ member, setByLabel }: { member: TeamMember; setByLabel?: string }) {
  const cap = member.budgetUsdDaily;
  const attribution =
    setByLabel && member.budgetSetAtMillis !== undefined ? (
      <span data-testid="team-budget-attribution" className="block truncate text-[10px] text-muted-foreground">
        {cap === undefined ? "Uncapped by" : "Set by"} {setByLabel} ·{" "}
        {new Date(member.budgetSetAtMillis).toLocaleDateString()}
      </span>
    ) : null;

  // No cap: render nothing but the attribution, if a human deliberately
  // removed one. "Uncapped by Ana" and "nobody ever capped this" are
  // different facts, and only the first has a line.
  if (cap === undefined) return attribution;

  const spent = member.spentTodayUsd ?? 0;
  const overBudget = spent >= cap;
  const usd = (n: number) => `$${n.toFixed(2)}`;
  return (
    <span className="block">
      <span
        data-testid="team-budget"
        className={cn("block truncate text-[10px]", overBudget ? "text-destructive" : "text-muted-foreground")}
      >
        {usd(cap)}/day · {usd(spent)} spent today
        {overBudget && " · paused"}
      </span>
      {attribution}
    </span>
  );
}
