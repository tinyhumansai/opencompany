import { useState } from "react";
import { Check, CircleDot, Copy, Hash, Lock, PanelLeft, Users } from "lucide-react";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { channelTitle, type Channel } from "./model";

interface Props {
  channel: Channel;
  /** Shown as a facepile count beside the title on a channel. */
  memberCount: number;
  membersOpen: boolean;
  onToggleMembers: () => void;
  /** Only rendered below `md`, where the rail shares the pane. */
  onOpenRail?: () => void;
}

/**
 * The bar above a channel's timeline.
 *
 * It is deliberately thin — a kind icon, the name, and the purpose in the
 * tooltip — with the two things you actually reach for on the right: the
 * member pane and a copy of the channel name. The copy button only appears on
 * hover so the title reads clean at rest.
 */
export function ChatHeader({
  channel,
  memberCount,
  membersOpen,
  onToggleMembers,
  onOpenRail,
}: Props) {
  const [copied, setCopied] = useState(false);
  const title = channelTitle(channel);

  async function copyName() {
    try {
      await navigator.clipboard.writeText(title);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1200);
    } catch {
      // Clipboard is permission-gated; a failed copy is not worth an error.
    }
  }

  return (
    <header className="flex h-15.5 shrink-0 items-center gap-2 border-b px-6">
      {onOpenRail && (
        <Button
          variant="ghost"
          size="icon"
          className="size-8 md:hidden"
          onClick={onOpenRail}
          aria-label="Show channels"
        >
          <PanelLeft className="size-4" />
        </Button>
      )}

      <div className="group/title flex min-w-0 flex-1 items-center gap-1.5">
        <KindIcon channel={channel} />
        <h1
          className="min-w-0 truncate text-base font-semibold tracking-tight"
          title={channel.purpose}
        >
          {channel.name}
        </h1>
        <Button
          variant="ghost"
          size="icon"
          className="size-6 shrink-0 text-muted-foreground opacity-0 transition-opacity focus-visible:opacity-100 group-hover/title:opacity-100"
          onClick={() => void copyName()}
          aria-label={`Copy channel name: ${title}`}
          title="Copy channel name"
        >
          {copied ? <Check className="size-3.5" /> : <Copy className="size-3.5" />}
        </Button>
        <span className="hidden min-w-0 truncate border-l pl-2 text-xs text-muted-foreground sm:block">
          {channel.purpose}
        </span>
      </div>

      <Button
        variant="ghost"
        size="sm"
        className={cn(
          "h-7 gap-1.5 rounded-full border px-2.5 text-xs",
          membersOpen ? "bg-accent" : "bg-muted/60",
        )}
        onClick={onToggleMembers}
        aria-pressed={membersOpen}
      >
        <Users className="size-3.5" />
        <span className="tabular-nums">{memberCount}</span>
        <span className="sr-only">{membersOpen ? "Hide" : "Show"} members</span>
      </Button>
    </header>
  );
}

function KindIcon({ channel }: { channel: Channel }) {
  const cls = "size-4 shrink-0 text-muted-foreground";
  if (channel.kind === "dm") return <CircleDot className={cls} aria-hidden />;
  if (channel.private) return <Lock className={cls} aria-hidden />;
  return <Hash className={cls} aria-hidden />;
}
