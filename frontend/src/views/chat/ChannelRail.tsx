import { useState } from "react";
import {
  ChevronRight,
  CircleDot,
  Hash,
  Lock,
  PanelRight,
  Plus,
  Radio,
  SquarePen,
} from "lucide-react";

import { Button } from "@/components/ui/button";
import { TeammateAvatar } from "@/components/teammate-avatar";
import { cn } from "@/lib/utils";
import { NewMessageDialog } from "./NewMessageDialog";
import { channelSubtitle, dmFace, type Channel, type ChannelSection } from "./model";

/**
 * What an unread badge actually claims (issue #364).
 *
 * The one thing on this rail that is still console-local: unread is derived
 * here from when this tab last looked at a channel, because the host has no
 * read-receipt surface. Transcripts, threads and reactions are all the host's
 * now — this is not, and it says so rather than letting an operator read the
 * badge as "unread by my team".
 */
const UNREAD_IS_LOCAL = "Estimated in this browser — unread is not tracked on the company.";

interface Props {
  sections: ChannelSection[];
  /**
   * Opens the channel creator (issue #1835) — rendered as a "+" on the
   * Channels section header. Absent (the rule for a control that would be
   * refused) when the roster cannot staff a channel yet.
   */
  onAddChannel?: () => void;
  activeId: string | null;
  /** Channel id → unread count. Absent or 0 reads as caught up. */
  unread: Record<string, number>;
  /** Channel id → how many unread mentions name this person there. */
  mentions?: Record<string, number>;
  onSelect: (id: string) => void;
  collapsed?: boolean;
  onExpand?: () => void;
  /** Controlled section-disclosure state, shared across the desktop and
   * sub-`lg` rail instances so crossing the breakpoint keeps the operator's
   * folds (codex P2 review). Falls back to instance-local state. */
  openSections?: Record<string, boolean>;
  onToggleSection?: (id: string) => void;
  directMessages?: Channel[];
  onStartDirectMessage?: (id: string) => void;
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
export function ChannelRail({
  sections,
  onAddChannel,
  activeId,
  unread,
  mentions,
  onSelect,
  collapsed = false,
  onExpand,
  openSections,
  onToggleSection,
  directMessages = [],
  onStartDirectMessage,
  className,
}: Props) {
  // Section disclosure lives here rather than inside `Section`, because the
  // collapsed branch below unmounts every `Section`. Held inside them, folding
  // a section and then collapsing the rail would reopen it on expand — the
  // density toggle must not discard the operator's organization. Absent means
  // "open": the default is a fully expanded list. `ChatView` passes the state
  // in so both rail instances share one fold set across the `lg` breakpoint;
  // a standalone rail (tests, other hosts) keeps it local to the instance.
  const [internalOpenSections, setInternalOpenSections] = useState<Record<string, boolean>>({});
  const resolvedOpenSections = openSections ?? internalOpenSections;
  const toggleSection = (id: string) => {
    if (onToggleSection) {
      onToggleSection(id);
    } else {
      setInternalOpenSections((prev) => ({ ...prev, [id]: !(prev[id] ?? true) }));
    }
  };

  if (collapsed) {
    return (
      <aside
        className={cn(
          "w-14 shrink-0 flex-col items-center overflow-y-auto border-r bg-sidebar/40 py-3",
          className,
        )}
      >
        <Button
          variant="ghost"
          size="icon"
          className="size-8 text-muted-foreground"
          onClick={onExpand}
          aria-label="Expand channels"
          title="Expand channels"
        >
          <PanelRight className="size-4" />
        </Button>
        <nav aria-label="Channels" className="mt-3 flex w-full flex-col items-center gap-1 px-2">
          {sections.flatMap((section) => section.channels).map((channel) => (
            <CompactChannelRow
              key={channel.id}
              channel={channel}
              active={channel.id === activeId}
              unread={unread[channel.id] ?? 0}
              mentions={mentions?.[channel.id] ?? 0}
              onSelect={onSelect}
            />
          ))}
        </nav>
      </aside>
    );
  }

  return (
    <aside
      className={cn(
        "w-64 shrink-0 flex-col overflow-y-auto border-r bg-sidebar/40 pb-3",
        className,
      )}
    >
      <div className="flex items-center justify-between px-3 py-3">
        <h2 className="truncate text-sm font-semibold tracking-tight">Chat</h2>
        {onStartDirectMessage && (
          <NewMessageDialog
            directMessages={directMessages}
            onSelect={onStartDirectMessage}
            trigger={
              <Button
                type="button"
                variant="ghost"
                size="icon"
                className="size-8"
                aria-label="New message"
                disabled={directMessages.length === 0}
                title="New message"
              >
                <SquarePen className="size-4" />
              </Button>
            }
          />
        )}
      </div>

      {sections.map((section) =>
        section.id === "operator" ? (
          <PinnedOperatorRow
            key={section.id}
            channel={section.channels[0]}
            active={section.channels[0]?.id === activeId}
            unread={section.channels[0] ? (unread[section.channels[0].id] ?? 0) : 0}
            onSelect={onSelect}
          />
        ) : (
          <Section
            key={section.id}
            section={section}
            onAdd={section.id === "channels" ? onAddChannel : undefined}
            activeId={activeId}
            unread={unread}
            mentions={mentions}
            onSelect={onSelect}
            open={resolvedOpenSections[section.id] ?? true}
            onToggle={() => toggleSection(section.id)}
          />
        ),
      )}
    </aside>
  );
}

/**
 * The Operator feed's row (issue #1757 rework): pinned below a divider,
 * outside every collapsible section, rather than folded into the Channels
 * list `Section` renders. No add door (channel creation stays scoped to the
 * Channels section's own `onAdd`), no member count, no mention badge — the
 * feed is a single read-only broadcast rather than an addressable,
 * multi-party line, so nobody is ever named in it.
 *
 * Unread IS shown (PR #1781 review, Codex P2): a workflow report can land
 * here while another channel is open, same as any other channel, and the
 * collapsed rail's `CompactChannelRow` already surfaced that (it flat-maps
 * every section, this one included, and was never taught to skip it) — this
 * expanded row was the one place unread silently dropped, so folding the
 * rail changed whether the pinned row could tell you something was waiting.
 */
function PinnedOperatorRow({
  channel,
  active,
  unread,
  onSelect,
}: {
  channel: Channel | undefined;
  active: boolean;
  unread: number;
  onSelect: (id: string) => void;
}) {
  if (!channel) return null;
  const hasUnread = unread > 0 && !active;
  return (
    <div className="mt-2 border-t px-2 pt-2">
      <button
        type="button"
        onClick={() => onSelect(channel.id)}
        aria-current={active ? "page" : undefined}
        title={channelSubtitle(channel) ?? undefined}
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
          <span
            data-testid="channel-unread"
            title={UNREAD_IS_LOCAL}
            className="shrink-0 rounded-full bg-primary px-1.5 text-3xs font-semibold leading-4 text-primary-foreground"
          >
            {unread > 99 ? "99+" : unread}
          </span>
        )}
      </button>
    </div>
  );
}

function CompactChannelRow({
  channel,
  active,
  unread,
  mentions,
  onSelect,
}: {
  channel: Channel;
  active: boolean;
  unread: number;
  mentions: number;
  onSelect: (id: string) => void;
}) {
  const hasUnread = unread > 0 && !active;
  const hasMentions = mentions > 0;

  return (
    <button
      type="button"
      onClick={() => onSelect(channel.id)}
      aria-current={active ? "page" : undefined}
      // The compact row renders unread as a bare dot, so the count has to live
      // in the accessible name — the expanded row says it in text, and
      // collapsing the rail must not strip the same fact from the screen-reader
      // tree. The dot itself stays a sighted-hover-only cue.
      aria-label={
        [
          channel.name,
          hasMentions && `${mentions > 99 ? "99+" : mentions} mention${mentions === 1 ? "" : "s"}`,
          hasUnread && `${unread > 99 ? "99+" : unread} unread`,
        ]
          .filter(Boolean)
          .join(", ")
      }
      title={channel.name}
      className={cn(
        "relative flex size-9 shrink-0 items-center justify-center rounded-md transition-colors",
        active
          ? "bg-sidebar-accent text-sidebar-accent-foreground"
          : "text-muted-foreground hover:bg-sidebar-accent/50 hover:text-foreground",
      )}
    >
      <ChannelIcon channel={channel} />
      {hasMentions && (
        <span
          data-testid="channel-mentions"
          title={`${mentions} ${mentions === 1 ? "mention" : "mentions"} of you here`}
          className="absolute -right-0.5 -top-0.5 size-2 rounded-full bg-destructive"
        />
      )}
      {hasUnread && (
        <span
          title={UNREAD_IS_LOCAL}
          className={cn(
            "absolute -right-0.5 size-2 rounded-full bg-primary",
            hasMentions ? "-bottom-0.5" : "-top-0.5",
          )}
        />
      )}
    </button>
  );
}

function Section({
  section,
  activeId,
  unread,
  mentions,
  onSelect,
  open,
  onToggle,
  onAdd,
}: {
  section: ChannelSection;
  activeId: string | null;
  unread: Record<string, number>;
  mentions?: Record<string, number>;
  onSelect: (id: string) => void;
  open: boolean;
  onToggle: () => void;
  /** Renders a "+" beside the header — the Channels section's create door. */
  onAdd?: () => void;
}) {
  const hiddenUnread = !open
    ? section.channels.reduce((n, c) => n + (unread[c.id] ?? 0), 0)
    : 0;
  const hiddenMentions = !open
    ? section.channels.reduce((n, c) => n + (mentions?.[c.id] ?? 0), 0)
    : 0;

  return (
    <section className="group/section select-none px-2 pt-2">
      <div className="flex items-center gap-0.5">
      <button
        type="button"
        onClick={onToggle}
        aria-expanded={open}
        className="flex w-full min-w-0 flex-1 items-center gap-1 rounded-md px-1.5 py-1 text-left text-xs font-medium text-muted-foreground transition-colors hover:text-foreground"
      >
        <ChevronRight
          className={cn("size-3 shrink-0 transition-transform", open && "rotate-90")}
          aria-hidden
        />
        <span className="truncate">{section.label}</span>
        {(hiddenMentions > 0 || hiddenUnread > 0) && (
          <span className="ml-auto flex items-center gap-1">
            {hiddenMentions > 0 && (
              <span
                data-testid="section-mentions"
                title={`${hiddenMentions} ${hiddenMentions === 1 ? "mention" : "mentions"} of you in this section`}
                className="rounded-full bg-destructive px-1.5 text-3xs font-semibold leading-4 text-destructive-foreground"
              >
                @{hiddenMentions > 99 ? "99+" : hiddenMentions}
              </span>
            )}
            {hiddenUnread > 0 && (
              <span
                title={UNREAD_IS_LOCAL}
                className="rounded-full bg-primary px-1.5 text-3xs font-semibold leading-4 text-primary-foreground"
              >
                {hiddenUnread > 99 ? "99+" : hiddenUnread}
              </span>
            )}
          </span>
        )}
      </button>
      {onAdd && (
        <button
          type="button"
          onClick={onAdd}
          title="New channel"
          aria-label="New channel"
          className="rounded-md p-1 text-muted-foreground hover:bg-accent hover:text-foreground"
        >
          <Plus className="size-3.5" aria-hidden />
        </button>
      )}
      </div>

      {open && (
        <ul className="mt-0.5 flex flex-col gap-px">
          {section.channels.map((channel) => (
            <li key={channel.id}>
              <ChannelRow
                channel={channel}
                active={channel.id === activeId}
                unread={unread[channel.id] ?? 0}
                mentions={mentions?.[channel.id] ?? 0}
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
  mentions,
  onSelect,
}: {
  channel: Channel;
  active: boolean;
  unread: number;
  mentions: number;
  onSelect: (id: string) => void;
}) {
  const hasUnread = unread > 0 && !active;
  const hasMentions = mentions > 0;

  return (
    <button
      type="button"
      onClick={() => onSelect(channel.id)}
      aria-current={active ? "page" : undefined}
      // The row's own label is `channel.name`, so a tooltip that resolves to
      // the same string is the header's issue-#1180 duplicate in a slower
      // form: you hover for a second fact and get the one already under the
      // cursor. No tooltip at all is the better answer, and `undefined` — not
      // `""` — is what suppresses the native bubble.
      title={channelSubtitle(channel) ?? undefined}
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
      {hasMentions && (
        <span
          data-testid="channel-mentions"
          title={mentions === 1 ? "1 mention of you here" : `${mentions} mentions of you here`}
          className="shrink-0 rounded-full bg-destructive px-1.5 text-3xs font-semibold leading-4 text-destructive-foreground"
        >
          @{mentions > 99 ? "99+" : mentions}
        </span>
      )}
      {hasUnread && (
        <span
          data-testid="channel-unread"
          // Issue #364: unread is derived in this browser from what this tab has
          // seen — the host keeps no read receipts. Two consoles will disagree,
          // and a badge that quietly means something narrower than it looks is
          // worse than one that says so.
          title={UNREAD_IS_LOCAL}
          className="shrink-0 rounded-full bg-primary px-1.5 text-3xs font-semibold leading-4 text-primary-foreground"
        >
          {unread > 99 ? "99+" : unread}
        </span>
      )}
    </button>
  );
}

function ChannelIcon({ channel }: { channel: Channel }) {
  if (channel.kind === "dm") {
    const face = dmFace(channel);
    return face ? (
      <TeammateAvatar {...face} className="size-5 text-3xs" />
    ) : (
      <CircleDot className="size-4 shrink-0" aria-hidden />
    );
  }
  // The Operator feed is a broadcast, not an addressable line — `#` implies a
  // channel you post into, which this one refuses (issue #1757 rework). A
  // distinct glyph is the honest mark, the same way `Lock` already distinguishes
  // a private channel from an ordinary one.
  if (channel.system) return <Radio className="size-4 shrink-0 opacity-70" aria-hidden />;
  const Icon = channel.private ? Lock : Hash;
  return <Icon className="size-4 shrink-0 opacity-70" aria-hidden />;
}
