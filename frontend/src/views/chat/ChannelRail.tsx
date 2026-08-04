import { useState } from "react";
import { ChevronRight, CircleDot, Hash, Lock, SquarePen } from "lucide-react";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { Avatar } from "./Avatar";
import type { Channel, ChannelSection } from "./model";

interface Props {
  sections: ChannelSection[];
  activeId: string | null;
  /** Channel id → unread count. Absent or 0 reads as caught up. */
  unread: Record<string, number>;
  onSelect: (id: string) => void;
  className?: string;
}

/**
 * The workspace's channel list.
 *
 * Sections collapse, rows carry their own icon by kind (`#` for a channel, a
 * lock when private, the teammate's avatar for a DM), and an unread channel
 * goes bold with a count on the right. This is the second sidebar on the
 * screen — the app's own nav is to its left — so it stays visually quieter
 * than that one: no group headers in caps, no badges except unread.
 */
export function ChannelRail({ sections, activeId, unread, onSelect, className }: Props) {
  return (
    <aside
      className={cn(
        "w-64 shrink-0 flex-col overflow-y-auto border-r bg-sidebar/40 pb-3",
        className,
      )}
    >
      <div className="flex items-center justify-between gap-2 px-3 py-3">
        <h2 className="truncate text-sm font-semibold tracking-tight">Chat</h2>
        <Button
          variant="ghost"
          size="icon"
          className="size-7 text-muted-foreground"
          aria-label="New message"
          disabled
          title="New message — coming soon"
        >
          <SquarePen className="size-4" />
        </Button>
      </div>

      {sections.map((section) => (
        <Section
          key={section.id}
          section={section}
          activeId={activeId}
          unread={unread}
          onSelect={onSelect}
        />
      ))}
    </aside>
  );
}

function Section({
  section,
  activeId,
  unread,
  onSelect,
}: {
  section: ChannelSection;
  activeId: string | null;
  unread: Record<string, number>;
  onSelect: (id: string) => void;
}) {
  const [open, setOpen] = useState(true);
  const hiddenUnread = !open
    ? section.channels.reduce((n, c) => n + (unread[c.id] ?? 0), 0)
    : 0;

  return (
    <section className="group/section select-none px-2 pt-2">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
        className="flex w-full items-center gap-1 rounded-md px-1.5 py-1 text-left text-xs font-medium text-muted-foreground transition-colors hover:text-foreground"
      >
        <ChevronRight
          className={cn("size-3 shrink-0 transition-transform", open && "rotate-90")}
          aria-hidden
        />
        <span className="truncate">{section.label}</span>
        {hiddenUnread > 0 && (
          <span className="ml-auto rounded-full bg-primary px-1.5 text-[10px] font-semibold leading-4 text-primary-foreground">
            {hiddenUnread > 99 ? "99+" : hiddenUnread}
          </span>
        )}
      </button>

      {open && (
        <ul className="mt-0.5 flex flex-col gap-px">
          {section.channels.map((channel) => (
            <li key={channel.id}>
              <ChannelRow
                channel={channel}
                active={channel.id === activeId}
                unread={unread[channel.id] ?? 0}
                onSelect={onSelect}
              />
            </li>
          ))}
          {section.channels.length === 0 && (
            <li className="px-2 py-1 text-xs text-muted-foreground">Nothing here yet.</li>
          )}
        </ul>
      )}
    </section>
  );
}

function ChannelRow({
  channel,
  active,
  unread,
  onSelect,
}: {
  channel: Channel;
  active: boolean;
  unread: number;
  onSelect: (id: string) => void;
}) {
  const hasUnread = unread > 0 && !active;

  return (
    <button
      type="button"
      onClick={() => onSelect(channel.id)}
      aria-current={active ? "page" : undefined}
      title={channel.purpose}
      className={cn(
        "flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-sm transition-colors",
        active
          ? "bg-sidebar-accent font-medium text-sidebar-accent-foreground"
          : "text-muted-foreground hover:bg-sidebar-accent/50 hover:text-foreground",
        hasUnread && "font-semibold text-foreground",
      )}
    >
      <ChannelIcon channel={channel} />
      <span className="min-w-0 flex-1 truncate">{channel.name}</span>
      {hasUnread && (
        <span className="shrink-0 rounded-full bg-primary px-1.5 text-[10px] font-semibold leading-4 text-primary-foreground">
          {unread > 99 ? "99+" : unread}
        </span>
      )}
    </button>
  );
}

function ChannelIcon({ channel }: { channel: Channel }) {
  if (channel.kind === "dm") {
    return channel.member ? (
      <Avatar name={channel.name} tone={channel.tone} className="size-5 text-[9px]" />
    ) : (
      <CircleDot className="size-4 shrink-0" aria-hidden />
    );
  }
  const Icon = channel.private ? Lock : Hash;
  return <Icon className="size-4 shrink-0 opacity-70" aria-hidden />;
}
