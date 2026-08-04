import { X } from "lucide-react";

import { Button } from "@/components/ui/button";
import type { ChatMessage } from "@/lib/chat";
import { Avatar } from "./Avatar";
import { MessageComposer } from "./MessageComposer";
import { channelTitle, formatTime, senderOf, type Channel } from "./model";

interface Props {
  channel: Channel;
  /** The message the thread hangs off. */
  parent: ChatMessage;
  replies: ChatMessage[];
  sending: boolean;
  onSend: (text: string) => void;
  onClose: () => void;
}

/**
 * The thread panel.
 *
 * Replies live here rather than inline, so a busy exchange never pushes the
 * channel apart. The parent message sits at the top under a rule, and the
 * panel carries its own composer scoped to the thread.
 */
export function ThreadPanel({ channel, parent, replies, sending, onSend, onClose }: Props) {
  return (
    <aside className="flex w-96 shrink-0 flex-col border-l bg-background">
      <header className="flex h-13 shrink-0 items-center gap-2 border-b px-3">
        <div className="min-w-0 flex-1">
          <h2 className="text-sm font-semibold tracking-tight">Thread</h2>
          <p className="truncate text-xs text-muted-foreground">{channelTitle(channel)}</p>
        </div>
        <Button variant="ghost" size="icon" className="size-8" onClick={onClose} aria-label="Close thread">
          <X className="size-4" />
        </Button>
      </header>

      <div className="flex-1 overflow-y-auto">
        <Line channel={channel} message={parent} />
        <div className="flex items-center gap-2 px-4 py-2">
          <span className="text-xs font-medium text-muted-foreground">
            {replies.length} {replies.length === 1 ? "reply" : "replies"}
          </span>
          <span className="h-px flex-1 bg-border" aria-hidden />
        </div>
        {replies.map((r) => (
          <Line key={r.id} channel={channel} message={r} />
        ))}
      </div>

      <MessageComposer
        compact
        placeholder="Reply…"
        disabled={sending}
        onSend={onSend}
      />
    </aside>
  );
}

function Line({ channel, message }: { channel: Channel; message: ChatMessage }) {
  const sender = senderOf(message, channel);

  if (sender.kind === "system") {
    return (
      <p className="px-4 py-1 text-center text-xs text-muted-foreground">{message.text}</p>
    );
  }

  return (
    <div className="flex gap-2.5 px-4 py-2">
      <Avatar
        name={sender.name}
        tone={sender.tone}
        company={sender.kind === "company"}
        className="size-8"
      />
      <div className="min-w-0 flex-1">
        <div className="flex items-baseline gap-2">
          <span className="truncate text-sm font-semibold tracking-tight">{sender.name}</span>
          <span className="shrink-0 text-[11px] text-muted-foreground tabular-nums">
            {formatTime(message.at)}
          </span>
        </div>
        <p className="whitespace-pre-wrap break-words text-sm leading-6">{message.text}</p>
      </div>
    </div>
  );
}
