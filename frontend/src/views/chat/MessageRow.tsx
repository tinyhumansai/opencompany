import { MessageSquareReply } from "lucide-react";

import { Button } from "@/components/ui/button";
import type { ChatMessage } from "@/lib/chat";
import { cn } from "@/lib/utils";
import { Avatar } from "./Avatar";
import {
  formatTime,
  QUICK_REACTIONS,
  type Sender,
  type TimelineEntry,
} from "./model";

interface Props {
  entry: TimelineEntry;
  /** True when the thread panel is showing this row's replies. */
  threadOpen: boolean;
  onOpenThread: (messageId: string) => void;
  onReact: (messageId: string, emoji: string) => void;
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
        <p className="whitespace-pre-wrap break-words text-sm leading-6">{message.text}</p>

        {message.reactions && (
          <Reactions reactions={message.reactions} onReact={(e) => onReact(message.id, e)} />
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

function Reactions({
  reactions,
  onReact,
}: {
  reactions: Record<string, number>;
  onReact: (emoji: string) => void;
}) {
  return (
    <div className="mt-1 flex flex-wrap gap-1">
      {Object.entries(reactions).map(([emoji, count]) => (
        <button
          key={emoji}
          type="button"
          onClick={() => onReact(emoji)}
          className="flex items-center gap-1 rounded-full border border-primary/40 bg-primary/10 px-2 py-0.5 text-xs transition-colors hover:bg-primary/20"
          aria-label={`Remove ${emoji} reaction`}
        >
          <span aria-hidden>{emoji}</span>
          <span className="tabular-nums text-[11px] font-medium">{count}</span>
        </button>
      ))}
    </div>
  );
}

/** The hover-revealed react/reply strip, pinned to the row's top-right. */
function ActionBar({
  onReply,
  onReact,
}: {
  onReply: () => void;
  onReact: (emoji: string) => void;
}) {
  return (
    <div className="absolute -top-3 right-4 z-10 hidden items-center gap-0.5 rounded-lg border bg-popover p-0.5 shadow-sm group-hover/message:flex group-focus-within/message:flex">
      {QUICK_REACTIONS.map((emoji) => (
        <button
          key={emoji}
          type="button"
          onClick={() => onReact(emoji)}
          className="flex size-7 items-center justify-center rounded-md text-sm transition-colors hover:bg-accent"
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
        onClick={onReply}
        aria-label="Reply in thread"
        title="Reply in thread"
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
