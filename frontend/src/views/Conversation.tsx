import { useEffect, useRef, useState } from "react";
import { ArrowUp, Building2 } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import { ApiError } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import { cn } from "@/lib/utils";
import { type ChatMessage, nextMessageId } from "@/lib/chat";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  messages: ChatMessage[];
  setMessages: React.Dispatch<React.SetStateAction<ChatMessage[]>>;
  /** Called after a reply lands, so the parent can refresh approvals/status. */
  onReply?: () => void;
}

/** The one-voice conversation with the company. */
export function Conversation({ client, company, messages, setMessages, onReply }: Props) {
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);
  const scroller = useRef<HTMLDivElement>(null);

  useEffect(() => {
    scroller.current?.scrollTo({ top: scroller.current.scrollHeight, behavior: "smooth" });
  }, [messages, sending]);

  async function send() {
    const text = draft.trim();
    if (!text || sending) return;
    setDraft("");
    setMessages((m) => [...m, { id: nextMessageId(), from: "you", text }]);
    setSending(true);
    try {
      const reply = await client.chat(text, company);
      const replies = reply.responses.length
        ? reply.responses.map((r) => ({ id: nextMessageId(), from: "company" as const, text: r.text }))
        : [{ id: nextMessageId(), from: "system" as const, text: "(no reply)" }];
      setMessages((m) => [...m, ...replies]);
      onReply?.();
    } catch (err) {
      const msg = err instanceof ApiError ? err.message : "something went wrong";
      setMessages((m) => [
        ...m,
        { id: nextMessageId(), from: "system", text: `Couldn't send — ${msg}` },
      ]);
    } finally {
      setSending(false);
    }
  }

  function onKeyDown(e: React.KeyboardEvent<HTMLTextAreaElement>) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      void send();
    }
  }

  return (
    <div className="flex flex-1 flex-col overflow-hidden">
      <div ref={scroller} className="flex-1 overflow-y-auto">
        <div className="mx-auto flex w-full max-w-3xl flex-col gap-4 px-4 py-6">
          {messages.length === 0 && <EmptyConversation />}
          {messages.map((m) => (
            <MessageBubble key={m.id} message={m} />
          ))}
          {sending && (
            <div className="flex items-center gap-2 self-start text-sm text-muted-foreground">
              <span className="flex gap-1">
                <Dot /> <Dot className="[animation-delay:150ms]" /> <Dot className="[animation-delay:300ms]" />
              </span>
              working on it
            </div>
          )}
        </div>
      </div>

      <div className="border-t bg-background/80 backdrop-blur">
        <div className="mx-auto w-full max-w-3xl px-4 py-3">
          <div className="relative flex items-end gap-2 rounded-xl border bg-card p-2 shadow-sm focus-within:ring-2 focus-within:ring-ring/50">
            <Textarea
              value={draft}
              onChange={(e) => setDraft(e.target.value)}
              onKeyDown={onKeyDown}
              placeholder="Message your company…"
              rows={1}
              className="max-h-40 min-h-9 flex-1 resize-none border-0 bg-transparent px-2 py-1.5 shadow-none focus-visible:ring-0"
            />
            <Button
              size="icon"
              className="size-9 shrink-0 rounded-lg"
              onClick={() => void send()}
              disabled={sending || !draft.trim()}
              aria-label="Send"
            >
              <ArrowUp className="size-4" />
            </Button>
          </div>
          <p className="mt-1.5 px-1 text-center text-xs text-muted-foreground">
            Enter to send · Shift+Enter for a new line
          </p>
        </div>
      </div>
    </div>
  );
}

function MessageBubble({ message }: { message: ChatMessage }) {
  if (message.from === "system") {
    return (
      <div className="self-center rounded-full bg-muted px-3 py-1 text-center text-xs text-muted-foreground">
        {message.text}
      </div>
    );
  }
  const mine = message.from === "you";
  return (
    <div className={cn("flex max-w-[85%] flex-col gap-1", mine ? "self-end items-end" : "self-start")}>
      {!mine && (
        <span className="flex items-center gap-1.5 px-1 text-xs font-medium text-muted-foreground">
          <Building2 className="size-3" /> Your company
        </span>
      )}
      <div
        className={cn(
          "whitespace-pre-wrap rounded-2xl px-4 py-2.5 text-sm leading-relaxed",
          mine
            ? "rounded-br-md bg-primary text-primary-foreground"
            : "rounded-bl-md border bg-card text-card-foreground",
        )}
      >
        {message.text}
      </div>
    </div>
  );
}

function EmptyConversation() {
  return (
    <div className="mt-16 flex flex-col items-center gap-3 text-center">
      <div className="flex size-12 items-center justify-center rounded-2xl bg-muted">
        <Building2 className="size-6 text-muted-foreground" />
      </div>
      <div className="space-y-1">
        <p className="font-medium">Talk to your company</p>
        <p className="max-w-sm text-sm text-muted-foreground">
          Say hello, ask for an update, or hand off a task. Your company handles the rest.
        </p>
      </div>
    </div>
  );
}

function Dot({ className }: { className?: string }) {
  return <span className={cn("size-1.5 animate-bounce rounded-full bg-muted-foreground", className)} />;
}
