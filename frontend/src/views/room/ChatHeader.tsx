import { useState } from "react";
import {
  Braces,
  Check,
  CircleDot,
  Copy,
  Hash,
  Lock,
  PanelLeft,
  Users,
} from "lucide-react";

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
  /**
   * Opens the channel list on a phone, where the sidebar holding it is a sheet
   * over the whole screen. Absent from `md` up, where the sidebar is a column
   * and the list is already beside the transcript — the rule this codebase
   * follows for a control that would do nothing.
   */
  onOpenRail?: () => void;
  /**
   * Whether this conversation can be shown as the agent's raw turns.
   *
   * A DM has exactly one teammate on the other end, so "the raw turns" names
   * something. A `#channel` has several and a system feed has none, so the
   * control is absent there rather than present and ambiguous — the same rule
   * the member pane follows for the Operator feed.
   */
  rawAvailable?: boolean;
  /** Whether the raw view is the one currently on screen. */
  raw?: boolean;
  onToggleRaw?: () => void;
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
  rawAvailable = false,
  raw = false,
  onToggleRaw,
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
          className="size-8 md:hidden"
          onClick={onOpenRail}
          aria-label="Show channels"
        >
          <PanelLeft className="size-4" />
        </Button>
      )}

      {/* No density toggle here any more.

          It used to collapse the chat rail, which was this view's own column.
          The rail is a section of the app sidebar now, so collapsing it IS
          collapsing the sidebar — and the console already has one control for
          that, on the content card's leading seam, forty pixels to the left of
          where this button sat (`SidebarCollapseButton`, issue #1177). Two
          buttons doing one job, side by side, is worse than either. */}

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

      {/* The raw turns for the teammate on the other end of this DM: what it
          was handed, what it said, and every tool call unfolded.

          It sits here rather than only on the teammate's Session tab because
          this is where you are when the question occurs to you. "Why did it
          answer that" is asked mid-conversation, and a control for it that
          lives two navigations away is a control nobody finds. */}
      {rawAvailable && onToggleRaw && (
        <Button
          variant="ghost"
          size="sm"
          className={cn(
            "h-7 gap-1.5 rounded-full border px-2.5 text-xs",
            raw ? "bg-accent" : "bg-muted/60",
          )}
          onClick={onToggleRaw}
          aria-pressed={raw}
          data-testid="chat-raw-toggle"
        >
          <Braces className="size-3.5" aria-hidden />
          <span>Raw turns</span>
        </Button>
      )}

      {/* Issue #1757: the Operator system channel is a read-only report feed
          with no members, so it offers no agent pane. */}
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
          <span className="sr-only">{membersOpen ? "Hide" : "Show"} agents</span>
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
