import { useEffect, useRef } from "react";

import type { ApprovalSummary, TurnStep, Verdict } from "@/api/types";
import { cn } from "@/lib/utils";
import { ApprovalRow } from "./ApprovalRow";
import { Avatar } from "./Avatar";
import { MessageRow } from "./MessageRow";
import { StepTimeline } from "./StepTimeline";
import { channelTitle, type Channel, type TimelineItem } from "./model";

interface Props {
  channel: Channel;
  /**
   * The channel's rows: messages, and the approvals raised in this channel
   * interleaved among them by time (#379). A card is its own kind rather than a
   * message — see {@link TimelineItem}.
   */
  items: TimelineItem[];
  /** The message whose thread is open, if any — that row stays highlighted. */
  openThreadId: string | null;
  /** Someone on the company side is composing a reply. */
  typing: boolean;
  /**
   * The tool rows of a turn running *right now* on this channel's thread, off
   * the transient `tool_call` / `tool_result` / `thinking` frames. Present
   * whether or not this console started the turn, which is the point — a turn
   * an inbound message kicked off shows its work here too (issue #367).
   */
  liveSteps?: TurnStep[];
  onOpenThread: (messageId: string) => void;
  onReact: (messageId: string, emoji: string) => void;
  /** Now, for the cards' "waiting N minutes" line. Owned by the shell's feed. */
  now?: number;
  /** Agent id → display name, for a card's "Asked by" line. */
  askerNames?: Map<string, string>;
  /** The verdict each inline card is currently waiting on. */
  decidingApprovals?: ReadonlyMap<string, Verdict>;
  onDecideApproval?: (approval: ApprovalSummary, verdict: Verdict) => void;
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
  items,
  openThreadId,
  typing,
  liveSteps,
  onOpenThread,
  onReact,
  now,
  askerNames,
  decidingApprovals,
  onDecideApproval,
}: Props) {
  const scroller = useRef<HTMLDivElement>(null);
  const liveStepCount = liveSteps?.length ?? 0;

  useEffect(() => {
    const el = scroller.current;
    if (!el) return;
    el.scrollTo({ top: el.scrollHeight, behavior: "smooth" });
    // Each new tool row grows the block at the bottom, so the scroll has to
    // follow it as the turn works, not only when the reply lands. A card
    // arriving counts too — it is the thing the operator has to act on.
  }, [items.length, typing, liveStepCount]);

  return (
    <div ref={scroller} className="flex-1 overflow-y-auto">
      <div className="flex min-h-full flex-col justify-end pb-4">
        <ChannelIntro channel={channel} empty={items.length === 0} />
        {items.map((item) =>
          item.kind === "message" ? (
            <div key={item.key}>
              {item.entry.dayLabel && <DayDivider label={item.entry.dayLabel} />}
              <MessageRow
                entry={item.entry}
                threadOpen={item.entry.message.id === openThreadId}
                onOpenThread={onOpenThread}
                onReact={onReact}
              />
            </div>
          ) : (
            <ApprovalRow
              key={item.key}
              approval={item.approval}
              now={now ?? Date.now()}
              askerNames={askerNames ?? EMPTY_NAMES}
              deciding={decidingApprovals?.get(item.approval.id) ?? null}
              decided={item.decided}
              onDecide={(verdict) => onDecideApproval?.(item.approval, verdict)}
            />
          ),
        )}
        {liveStepCount > 0 ? (
          <LiveTurnRow channel={channel} steps={liveSteps ?? []} />
        ) : (
          typing && <TypingRow channel={channel} />
        )}
      </div>
    </div>
  );
}

/** Stable identity so a missing `askerNames` cannot churn the card's props. */
const EMPTY_NAMES: Map<string, string> = new Map();

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

/**
 * What the company is doing right now, in place of the typing dots.
 *
 * Same avatar gutter as a message row so the work reads as coming from the
 * voice that will answer, and the same {@link StepTimeline} the finished reply
 * renders — so the rows do not re-draw differently the instant the turn ends.
 */
function LiveTurnRow({ channel, steps }: { channel: Channel; steps: TurnStep[] }) {
  return (
    <div className="flex items-start gap-2.5 px-4 py-1">
      <Avatar
        name={channel.voice ?? channel.name}
        tone={channel.tone}
        company={channel.kind === "channel" && channel.id === "main"}
        className="size-9 shrink-0"
      />
      <div className="min-w-0 flex-1">
        <StepTimeline steps={steps} defaultOpen />
        <span className="sr-only">Working…</span>
      </div>
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
