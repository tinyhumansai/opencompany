import { useEffect, useRef } from "react";

import { cn } from "@/lib/utils";
import { Avatar } from "./Avatar";
import { MessageRow } from "./MessageRow";
import { channelTitle, type Channel, type TimelineEntry } from "./model";

interface Props {
  channel: Channel;
  entries: TimelineEntry[];
  /** The message whose thread is open, if any — that row stays highlighted. */
  openThreadId: string | null;
  /** Someone on the company side is composing a reply. */
  typing: boolean;
  onOpenThread: (messageId: string) => void;
  onReact: (messageId: string, emoji: string) => void;
}

/**
 * The scrolling body of a channel.
 *
 * Rows are bottom-anchored: the view sticks to the newest message, which is
 * what a chat log wants and what a plain scroll container does not do on its
 * own. Day dividers ride along as sticky pills so the date stays legible while
 * you scroll through it.
 */
export function MessageTimeline({
  channel,
  entries,
  openThreadId,
  typing,
  onOpenThread,
  onReact,
}: Props) {
  const scroller = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const el = scroller.current;
    if (!el) return;
    el.scrollTo({ top: el.scrollHeight, behavior: "smooth" });
  }, [entries.length, typing]);

  return (
    <div ref={scroller} className="flex-1 overflow-y-auto">
      <div className="flex min-h-full flex-col justify-end pb-4">
        <ChannelIntro channel={channel} empty={entries.length === 0} />
        {entries.map((entry) => (
          <div key={entry.message.id}>
            {entry.dayLabel && <DayDivider label={entry.dayLabel} />}
            <MessageRow
              entry={entry}
              threadOpen={entry.message.id === openThreadId}
              onOpenThread={onOpenThread}
              onReact={onReact}
            />
          </div>
        ))}
        {typing && <TypingRow channel={channel} />}
      </div>
    </div>
  );
}

function DayDivider({ label }: { label: string }) {
  return (
    <div
      aria-label={label}
      className="pointer-events-none sticky top-2 z-20 flex justify-center py-2"
    >
      <p className="rounded-full border bg-background px-2.5 py-1 text-[11px] font-medium tracking-wide text-muted-foreground">
        {label}
      </p>
    </div>
  );
}

/**
 * The block at the very top of a channel, explaining what it is for. It stays
 * above the first message rather than only showing when the channel is empty —
 * scrolling to the beginning of a channel should tell you where you are.
 */
function ChannelIntro({ channel, empty }: { channel: Channel; empty: boolean }) {
  return (
    <div className={cn("px-4 pb-3", empty ? "pt-16" : "pt-6")}>
      <Avatar
        name={channel.voice ?? channel.name}
        tone={channel.tone}
        company={channel.kind === "channel" && channel.id === "main"}
        className="mb-3 size-12 rounded-lg text-base"
      />
      <h2 className="text-xl font-semibold tracking-tight">{channelTitle(channel)}</h2>
      <p className="mt-1 max-w-prose text-sm text-muted-foreground">
        {channel.kind === "dm"
          ? `This is the start of your direct message with ${channel.name} — ${lower(channel.purpose)}.`
          : `This is the very beginning of ${channelTitle(channel)}. ${sentence(channel.purpose)}`}
      </p>
    </div>
  );
}

function TypingRow({ channel }: { channel: Channel }) {
  return (
    <div className="flex items-center gap-2.5 px-4 py-1">
      <Avatar
        name={channel.voice ?? channel.name}
        tone={channel.tone}
        company={channel.kind === "channel" && channel.id === "main"}
        className="size-9"
      />
      <span className="flex items-center gap-1 rounded-full bg-muted px-3 py-2">
        <Dot />
        <Dot className="[animation-delay:150ms]" />
        <Dot className="[animation-delay:300ms]" />
        <span className="sr-only">Replying…</span>
      </span>
    </div>
  );
}

function Dot({ className }: { className?: string }) {
  return (
    <span className={cn("size-1.5 animate-bounce rounded-full bg-muted-foreground", className)} />
  );
}

function lower(s: string): string {
  return s.charAt(0).toLowerCase() + s.slice(1);
}

function sentence(s: string): string {
  const t = s.trim();
  return /[.!?]$/.test(t) ? t : `${t}.`;
}
