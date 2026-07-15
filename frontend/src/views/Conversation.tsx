import { useEffect, useMemo, useRef, useState } from "react";
import { ArrowUp, Building2 } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import { ApiError } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import { cn } from "@/lib/utils";
import { type ChatMessage, makeMessage, senderOf } from "@/lib/chat";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  messages: ChatMessage[];
  setMessages: React.Dispatch<React.SetStateAction<ChatMessage[]>>;
  /** Called after a reply lands, so the parent can refresh approvals/status. */
  onReply?: () => void;
}

/** Consecutive messages from one sender within this window group together. */
const GROUP_WINDOW_MS = 5 * 60 * 1000;

interface Group {
  key: string;
  senderKey: string;
  name: string;
  from: ChatMessage["from"];
  at: number;
  messages: ChatMessage[];
}

/** The threaded conversation with the company — grouped by sender, chat-app style. */
export function Conversation({ client, company, messages, setMessages, onReply }: Props) {
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);
  const scroller = useRef<HTMLDivElement>(null);

  useEffect(() => {
    scroller.current?.scrollTo({ top: scroller.current.scrollHeight, behavior: "smooth" });
  }, [messages, sending]);

  const groups = useMemo(() => groupMessages(messages), [messages]);

  async function send() {
    const text = draft.trim();
    if (!text || sending) return;
    setDraft("");
    setMessages((m) => [...m, makeMessage("you", text)]);
    setSending(true);
    try {
      const reply = await client.chat(text, company);
      const replies = reply.responses.length
        ? reply.responses.map((r) => makeMessage("company", r.text, { channel: r.channel }))
        : [makeMessage("system", "(no reply)")];
      setMessages((m) => [...m, ...replies]);
      onReply?.();
    } catch (err) {
      const msg = err instanceof ApiError ? err.message : "something went wrong";
      setMessages((m) => [...m, makeMessage("system", `Couldn't send — ${msg}`)]);
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
        <div className="mx-auto flex w-full max-w-3xl flex-col gap-1.5 px-4 py-6">
          {messages.length === 0 && <EmptyConversation />}
          {groups.map((g, i) => (
            <MessageGroup key={g.key} group={g} prev={groups[i - 1]} />
          ))}
          {sending && <TypingIndicator />}
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

function MessageGroup({ group, prev }: { group: Group; prev?: Group }) {
  const showDay = !prev || !sameDay(prev.at, group.at);

  if (group.from === "system") {
    return (
      <>
        {showDay && <DaySeparator at={group.at} />}
        <div className="my-1 flex flex-col items-center gap-1">
          {group.messages.map((m) => (
            <div
              key={m.id}
              className="rounded-full bg-muted px-3 py-1 text-center text-xs text-muted-foreground"
            >
              {m.text}
            </div>
          ))}
        </div>
      </>
    );
  }

  const mine = group.from === "you";
  return (
    <>
      {showDay && <DaySeparator at={group.at} />}
      <div className={cn("mt-2 flex gap-2.5", mine ? "flex-row-reverse" : "flex-row")}>
        {!mine && <SenderAvatar senderKey={group.senderKey} name={group.name} />}
        <div className={cn("flex min-w-0 flex-col gap-1", mine ? "items-end" : "items-start")}>
          {!mine && (
            <div className="flex items-baseline gap-2 px-1">
              <span className="text-xs font-semibold">{group.name}</span>
              <span className="text-[11px] text-muted-foreground">{formatTime(group.at)}</span>
            </div>
          )}
          {group.messages.map((m, i) => (
            <Bubble key={m.id} message={m} mine={mine} last={i === group.messages.length - 1} />
          ))}
        </div>
      </div>
    </>
  );
}

function Bubble({ message, mine, last }: { message: ChatMessage; mine: boolean; last: boolean }) {
  return (
    <div
      className={cn(
        "group/bubble relative max-w-[85%] whitespace-pre-wrap px-3.5 py-2 text-sm leading-relaxed sm:max-w-[75%]",
        mine
          ? "rounded-2xl bg-primary text-primary-foreground"
          : "rounded-2xl border bg-card text-card-foreground",
        // Tuck the tail on the last bubble of a group.
        last && (mine ? "rounded-br-md" : "rounded-bl-md"),
      )}
    >
      {message.text}
      {mine && last && (
        <span className="mt-0.5 block text-right text-[10px] text-primary-foreground/70">
          {formatTime(message.at)}
        </span>
      )}
    </div>
  );
}

function SenderAvatar({ senderKey, name }: { senderKey: string; name: string }) {
  const isCompany = senderKey === "company";
  return (
    <div
      className={cn(
        "mt-5 flex size-8 shrink-0 items-center justify-center rounded-full text-xs font-semibold",
        isCompany ? "bg-primary text-primary-foreground" : avatarTone(senderKey),
      )}
      aria-hidden
    >
      {isCompany ? <Building2 className="size-4" /> : initials(name)}
    </div>
  );
}

function TypingIndicator() {
  return (
    <div className="mt-2 flex gap-2.5">
      <div className="mt-0.5 flex size-8 shrink-0 items-center justify-center rounded-full bg-primary text-primary-foreground">
        <Building2 className="size-4" />
      </div>
      <div className="flex items-center gap-1 rounded-2xl rounded-bl-md border bg-card px-3.5 py-3">
        <Dot />
        <Dot className="[animation-delay:150ms]" />
        <Dot className="[animation-delay:300ms]" />
      </div>
    </div>
  );
}

function DaySeparator({ at }: { at: number }) {
  return (
    <div className="my-3 flex items-center gap-3">
      <div className="h-px flex-1 bg-border" />
      <span className="text-[11px] font-medium text-muted-foreground">{formatDay(at)}</span>
      <div className="h-px flex-1 bg-border" />
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

/* ---- grouping + formatting ---- */

function groupMessages(messages: ChatMessage[]): Group[] {
  const groups: Group[] = [];
  for (const m of messages) {
    const sender = senderOf(m);
    const tail = groups[groups.length - 1];
    if (
      tail &&
      tail.senderKey === sender.key &&
      m.at - tail.at < GROUP_WINDOW_MS &&
      sameDay(tail.at, m.at)
    ) {
      tail.messages.push(m);
      tail.at = m.at;
    } else {
      groups.push({
        key: m.id,
        senderKey: sender.key,
        name: sender.name,
        from: m.from,
        at: m.at,
        messages: [m],
      });
    }
  }
  return groups;
}

const AVATAR_TONES = [
  "bg-sky-500/15 text-sky-600 dark:text-sky-400",
  "bg-violet-500/15 text-violet-600 dark:text-violet-400",
  "bg-amber-500/15 text-amber-600 dark:text-amber-400",
  "bg-emerald-500/15 text-emerald-600 dark:text-emerald-400",
  "bg-rose-500/15 text-rose-600 dark:text-rose-400",
  "bg-cyan-500/15 text-cyan-600 dark:text-cyan-400",
];

function avatarTone(key: string): string {
  let hash = 0;
  for (let i = 0; i < key.length; i++) hash = (hash * 31 + key.charCodeAt(i)) | 0;
  return AVATAR_TONES[Math.abs(hash) % AVATAR_TONES.length];
}

function initials(name: string): string {
  const parts = name.trim().split(/\s+/).slice(0, 2);
  return parts.map((p) => p.charAt(0).toUpperCase()).join("") || "?";
}

function formatTime(at: number): string {
  return new Date(at).toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });
}

function sameDay(a: number, b: number): boolean {
  return new Date(a).toDateString() === new Date(b).toDateString();
}

function formatDay(at: number): string {
  const d = new Date(at);
  const today = new Date();
  const yesterday = new Date(today);
  yesterday.setDate(today.getDate() - 1);
  if (d.toDateString() === today.toDateString()) return "Today";
  if (d.toDateString() === yesterday.toDateString()) return "Yesterday";
  return d.toLocaleDateString(undefined, { month: "short", day: "numeric", year: "numeric" });
}
