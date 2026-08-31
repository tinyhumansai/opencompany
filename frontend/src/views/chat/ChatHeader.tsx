import { useState, type Ref } from "react";
import { Check, CircleDot, Copy, Hash, Lock, PanelLeft, PanelRight, Users } from "lucide-react";

import { AgentAvatarButton } from "@/components/agent-profile-sheet";
import { TeammateAvatar } from "@/components/teammate-avatar";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { channelSubtitle, channelTitle, dmFace, type Channel } from "./model";

interface Props {
  channel: Channel;
  /** Shown as a facepile count beside the title on a channel. */
  memberCount: number;
  membersOpen: boolean;
  onToggleMembers: () => void;
  /** Only rendered below `lg`, where the full rail shares the pane. */
  onOpenRail?: () => void;
  channelsCollapsed: boolean;
  onToggleChannels: () => void;
  /**
   * Where the shell restores focus after this header's own toggle unmounts the
   * compact rail's expand button (issue #1340). The header toggle is mounted on
   * both density states, so it is the one control the shell can count on to
   * carry focus across the switch.
   */
  channelsToggleRef?: Ref<HTMLButtonElement>;
}

/**
 * The bar above a channel's timeline.
 *
 * It is deliberately thin — a kind mark (the teammate's avatar on a DM), the
 * name, and a muted second line past the divider — with the two things you
 * actually reach for on the right: the member pane and a copy of the channel
 * name. The copy button only appears on hover so the title reads clean at rest.
 *
 * That second line is [`channelSubtitle`], which answers `null` when the only
 * thing it could say is what the title already says. The whole `<span>` goes
 * with it: it carries the `border-l` that draws the divider, so rendering it
 * empty would leave a rule hanging beside the name with nothing after it.
 */
export function ChatHeader({
  channel,
  memberCount,
  membersOpen,
  onToggleMembers,
  onOpenRail,
  channelsCollapsed,
  onToggleChannels,
  channelsToggleRef,
}: Props) {
  const [copied, setCopied] = useState(false);
  const title = channelTitle(channel);
  const subtitle = channelSubtitle(channel);

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
          className="size-8 lg:hidden"
          onClick={onOpenRail}
          aria-label="Show channels"
        >
          <PanelLeft className="size-4" />
        </Button>
      )}

      <Button
        ref={channelsToggleRef}
        variant="ghost"
        size="icon"
        className="hidden size-8 lg:inline-flex"
        onClick={onToggleChannels}
        aria-label={channelsCollapsed ? "Expand channels" : "Collapse channels"}
        title={channelsCollapsed ? "Expand channels" : "Collapse channels"}
      >
        {channelsCollapsed ? <PanelRight className="size-4" /> : <PanelLeft className="size-4" />}
      </Button>

      <div className="group/title flex min-w-0 flex-1 items-center gap-1.5">
        <KindIcon channel={channel} />
        <h1
          className="min-w-0 truncate text-base font-semibold tracking-tight"
          title={subtitle ?? undefined}
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
        {subtitle && (
          <span className="hidden min-w-0 truncate border-l pl-2 text-xs text-muted-foreground sm:block">
            {subtitle}
          </span>
        )}
      </div>

      {/* Issue #1757: the Operator system channel is a read-only report feed
          with no members, so it offers no teammate pane. */}
      {!channel.system && (
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
          <span className="sr-only">{membersOpen ? "Hide" : "Show"} teammates</span>
        </Button>
      )}
    </header>
  );
}

/**
 * What sits to the left of the title: a channel's kind, or a DM's teammate.
 *
 * A channel has no face, so `#` and `Lock` are the honest marks for one. A DM
 * has exactly one person on the other end, and drawing a glyph for them while
 * the rail row a few pixels away drew their mascot gave one teammate two faces
 * on one screen (issue #1170). Both now seed off [`dmFace`], so they cannot
 * disagree about who you are talking to.
 *
 * `size-6` — 24px — rather than the rail's 20px: this header has the room, and
 * 24px is the floor `TeammateAvatar`'s `markOnly` draws for a mascot being legible
 * rather than a smudge, so the drawing is worth rendering at all here.
 */
function KindIcon({ channel }: { channel: Channel }) {
  const cls = "size-4 shrink-0 text-muted-foreground";
  if (channel.kind === "dm") {
    const face = dmFace(channel);
    // A DM with no roster entry behind it has nobody to draw — the rail falls
    // back to the same glyph rather than inventing a face for a stranger.
    return face ? (
      // The teammate this line is *with* — clicking their face here opens who
      // they are (issue #1653), same as clicking it in the transcript below.
      <AgentAvatarButton agentId={channel.member?.id} name={channel.name}>
        <TeammateAvatar {...face} className="size-6" />
      </AgentAvatarButton>
    ) : (
      <CircleDot className={cls} aria-hidden />
    );
  }
  if (channel.private) return <Lock className={cls} aria-hidden />;
  return <Hash className={cls} aria-hidden />;
}
