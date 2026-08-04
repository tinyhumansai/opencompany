import { Mail, MessageSquare, MoreHorizontal, UserPlus } from "lucide-react";

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
import { Avatar } from "./Avatar";

interface Props {
  members: TeamMember[];
  loading: boolean;
  /** True when the roster came from the host rather than the starter set. */
  fromHost: boolean;
  /** Member name → whether that agent has an inbox. */
  hasInbox: (name: string) => boolean;
  onToggleInbox: (name: string) => void;
  onRemove: (id: string) => void;
  onAdd: () => void;
  onMessage: (member: TeamMember) => void;
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
  hasInbox,
  onToggleInbox,
  onRemove,
  onAdd,
  onMessage,
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
                  inboxOn={hasInbox(m.name)}
                  onToggleInbox={() => onToggleInbox(m.name)}
                  onRemove={() => onRemove(m.id)}
                  onMessage={() => onMessage(m)}
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
}: {
  member: TeamMember;
  inboxOn: boolean;
  onToggleInbox: () => void;
  onRemove: () => void;
  onMessage: () => void;
}) {
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
          <DropdownMenuSeparator />
          <DropdownMenuItem variant="destructive" onClick={onRemove}>
            Remove from roster
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
    </div>
  );
}
