import { MessageSquareReply } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Markdown } from "@/components/markdown";
import { isHostMessageId, type ChatMessage } from "@/lib/chat";
import { cn } from "@/lib/utils";
import { Avatar } from "./Avatar";
import {
  formatTime,
  hasReacted,
  QUICK_REACTIONS,
  reactionChips,
  type ReactionChip,
  type Sender,
  type TimelineEntry,
} from "./model";
import { CardChip, StepTimeline } from "./StepTimeline";

interface Props {
  entry: TimelineEntry;
  /** True when the thread panel is showing this row's replies. */
  threadOpen: boolean;
  onOpenThread: (messageId: string) => void;
  onReact: (messageId: string, emoji: string) => void;
}

/**
 * Why replying and reacting are unavailable on a row, or `undefined` when they
 * are not (issue #364).
 *
 * Derived from the row's own id rather than passed down, because the id *is*
 * the answer: a thread reply and a reaction both have to name a message the
 * host has journaled, and only an `h`-prefixed id names one. A row still
 * carrying its optimistic counter (the POST has not landed) or one from a host
 * with no durable ids at all cannot be named by either.
 *
 * The reason is a sentence, not a boolean, because a control that just stops
 * working reads as a bug. This is the on-screen label for the one thing about
 * this feature that is genuinely conditional on the host.
 */
function actionsUnavailableFor(message: ChatMessage): string | undefined {
  if (isHostMessageId(message.id)) return undefined;
  return message.from === "system"
    ? "Console-only line — there is nothing saved to reply to."
    : "Not saved yet — a reply or a reaction needs a message this company has stored.";
}

/**
 * One line in the channel.
 *
 * The layout is a fixed avatar gutter plus a flexible body, which is what
 * makes a run of messages from one voice line up: the first row shows the
 * avatar and the author, and every continuation row leaves the gutter empty
 * and reveals its timestamp there on hover instead. The action bar floats over
 * the top-right corner rather than taking layout space.
 */
export function MessageRow({ entry, threadOpen, onOpenThread, onReact }: Props) {
  const { message, sender, continuation, replies } = entry;
  const chips = reactionChips(message.reactions);
  const actionsUnavailable = actionsUnavailableFor(message);

  if (sender.kind === "system") {
    return (
      <div className="flex justify-center px-4 py-1">
        <p className="rounded-full bg-muted px-3 py-1 text-center text-xs text-muted-foreground">
          {message.text}
        </p>
      </div>
    );
  }

  return (
    <article
      data-message-id={message.id}
      className={cn(
        "group/message relative flex gap-2.5 px-4 transition-colors hover:bg-muted/40",
        continuation ? "py-0.5" : "pb-0.5 pt-2",
        threadOpen && "bg-muted/60",
      )}
    >
      <div className="w-9 shrink-0">
        {continuation ? (
          <span className="hidden pt-0.5 text-right text-[10px] leading-5 text-muted-foreground tabular-nums group-hover/message:block">
            {formatTime(message.at)}
          </span>
        ) : (
          <Avatar name={sender.name} tone={sender.tone} company={sender.kind === "company"} className="size-9" />
        )}
      </div>

      <div className="flex min-w-0 flex-1 flex-col">
        {!continuation && <AuthorLine sender={sender} at={message.at} />}
        <Markdown className="text-sm leading-6 break-words prose-p:my-0 prose-pre:my-1.5 prose-ul:my-1 prose-ol:my-1 prose-headings:my-1">{message.text}</Markdown>

        {message.steps && message.steps.length > 0 && <StepTimeline steps={message.steps} />}
        {message.taskId && <CardChip taskId={message.taskId} />}

        {chips.length > 0 && (
          <Reactions
            chips={chips}
            disabledReason={actionsUnavailable}
            onReact={(e) => onReact(message.id, e)}
          />
        )}

        {replies.length > 0 && (
          <button
            type="button"
            onClick={() => onOpenThread(message.id)}
            className="mt-1 flex w-fit items-center gap-2 rounded-md px-1.5 py-1 text-xs font-medium text-primary transition-colors hover:bg-accent"
          >
            <ReplyFacepile replies={replies} />
            {replies.length} {replies.length === 1 ? "reply" : "replies"}
            <span className="font-normal text-muted-foreground">
              Last reply {formatTime(replies[replies.length - 1].at)}
            </span>
          </button>
        )}
      </div>

      <ActionBar
        onReply={() => onOpenThread(message.id)}
        onReact={(emoji) => onReact(message.id, emoji)}
        reacted={(emoji) => hasReacted(message.reactions, emoji)}
        disabledReason={actionsUnavailable}
      />
    </article>
  );
}

function AuthorLine({ sender, at }: { sender: Sender; at: number }) {
  return (
    <div className="flex min-w-0 flex-wrap items-baseline gap-x-2 leading-5">
      <span className="truncate text-sm font-semibold tracking-tight">{sender.name}</span>
      <span className="shrink-0 text-[11px] text-muted-foreground tabular-nums">
        {formatTime(at)}
      </span>
    </div>
  );
}

/**
 * The reaction chips under a line.
 *
 * Each chip carries a count *and* names everyone behind it in its tooltip —
 * "who reacted" is most of what a reaction is for, and a bare count answers
 * none of it. The reader's own chip is highlighted, which is what makes it
 * legible as a toggle rather than a tally that only goes up.
 */
function Reactions({
  chips,
  disabledReason,
  onReact,
}: {
  chips: ReactionChip[];
  disabledReason?: string;
  onReact: (emoji: string) => void;
}) {
  return (
    <div className="mt-1 flex flex-wrap gap-1">
      {chips.map((chip) => (
        <button
          key={chip.emoji}
          type="button"
          disabled={!!disabledReason}
          onClick={() => onReact(chip.emoji)}
          title={disabledReason ?? `${chip.by.join(", ")} reacted with ${chip.emoji}`}
          className={cn(
            "flex items-center gap-1 rounded-full border px-2 py-0.5 text-xs transition-colors",
            chip.mine
              ? "border-primary/40 bg-primary/10 hover:bg-primary/20"
              : "border-border bg-muted/60 hover:bg-muted",
            disabledReason && "cursor-not-allowed opacity-60 hover:bg-inherit",
          )}
          aria-pressed={chip.mine}
          aria-label={`${chip.emoji} — ${chip.by.join(", ")}`}
        >
          <span aria-hidden>{chip.emoji}</span>
          <span className="tabular-nums text-[11px] font-medium">{chip.count}</span>
        </button>
      ))}
    </div>
  );
}

/** The hover-revealed react/reply strip, pinned to the row's top-right. */
function ActionBar({
  onReply,
  onReact,
  reacted,
  disabledReason,
}: {
  onReply: () => void;
  onReact: (emoji: string) => void;
  /** Whether the reader has already reacted with an emoji, to mark the button. */
  reacted: (emoji: string) => boolean;
  /** Why both actions are unavailable, shown as the tooltip when they are. */
  disabledReason?: string;
}) {
  const disabled = !!disabledReason;
  return (
    <div className="absolute -top-3 right-4 z-10 hidden items-center gap-0.5 rounded-lg border bg-popover p-0.5 shadow-sm group-hover/message:flex group-focus-within/message:flex">
      {QUICK_REACTIONS.map((emoji) => (
        <button
          key={emoji}
          type="button"
          disabled={disabled}
          onClick={() => onReact(emoji)}
          title={disabledReason}
          aria-pressed={reacted(emoji)}
          className={cn(
            "flex size-7 items-center justify-center rounded-md text-sm transition-colors hover:bg-accent",
            reacted(emoji) && "bg-primary/10",
            disabled && "cursor-not-allowed opacity-50 hover:bg-transparent",
          )}
          aria-label={`React with ${emoji}`}
        >
          <span aria-hidden>{emoji}</span>
        </button>
      ))}
      <span className="mx-0.5 h-4 w-px bg-border" aria-hidden />
      <Button
        variant="ghost"
        size="icon"
        className="size-7"
        disabled={disabled}
        onClick={onReply}
        aria-label="Reply in thread"
        title={disabledReason ?? "Reply in thread"}
      >
        <MessageSquareReply className="size-4" />
      </Button>
    </div>
  );
}

function ReplyFacepile({ replies }: { replies: ChatMessage[] }) {
  // At most three faces — beyond that the count carries the information.
  const shown = replies.slice(0, 3);
  return (
    <span className="flex -space-x-1.5" aria-hidden>
      {shown.map((r) => (
        <Avatar
          key={r.id}
          name={r.from === "you" ? "You" : (r.channel ?? "Company")}
          tone={r.channel}
          className="size-4 rounded-[3px] text-[7px] ring-1 ring-background"
        />
      ))}
    </span>
  );
}
