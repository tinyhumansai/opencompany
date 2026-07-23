import { Fragment, useEffect, useMemo, useRef, useState } from "react";
import {
  AlertTriangle,
  ArrowLeft,
  ArrowUp,
  Brain,
  Building2,
  ChevronDown,
  ChevronRight,
  PenSquare,
  Wrench,
} from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import { ApiError, type TurnStep, type TurnStepKind } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import { cn } from "@/lib/utils";
import { type ChatMessage, makeMessage } from "@/lib/chat";
import type { Thread, ThreadContact } from "@/lib/threads";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  threads: Thread[];
  activeId: string;
  onSelect: (id: string) => void;
  setMessages: (threadId: string, updater: (m: ChatMessage[]) => ChatMessage[]) => void;
  /** Called after a reply lands, so the parent can refresh approvals/status. */
  onReply?: () => void;
}

/** Consecutive messages from one sender within this window group together. */
const GROUP_WINDOW_MS = 5 * 60 * 1000;

/** WhatsApp-style two-pane chat: a thread list on the left, transcript right. */
export function Conversation({ client, company, threads, activeId, onSelect, setMessages, onReply }: Props) {
  const active = threads.find((t) => t.id === activeId) ?? threads[0];
  // On mobile, the list and the chat share the pane — track which is showing.
  const [mobilePane, setMobilePane] = useState<"list" | "chat">("chat");

  return (
    <div className="flex flex-1 overflow-hidden">
      <ThreadList
        threads={threads}
        activeId={active.id}
        onSelect={(id) => {
          onSelect(id);
          setMobilePane("chat");
        }}
        className={cn("md:flex", mobilePane === "list" ? "flex" : "hidden")}
      />
      <ChatPane
        key={active.id}
        client={client}
        company={company}
        thread={active}
        setMessages={setMessages}
        onReply={onReply}
        onOpenList={() => setMobilePane("list")}
        className={cn("md:flex", mobilePane === "chat" ? "flex" : "hidden")}
      />
    </div>
  );
}

/* ---- left: the chat list ---- */

function ThreadList({
  threads,
  activeId,
  onSelect,
  className,
}: {
  threads: Thread[];
  activeId: string;
  onSelect: (id: string) => void;
  className?: string;
}) {
  return (
    <aside className={cn("w-full shrink-0 flex-col border-r bg-card/40 md:w-80", className)}>
      <div className="flex items-center justify-between px-4 py-3">
        <h2 className="text-sm font-semibold">Chats</h2>
        <Button variant="ghost" size="icon" className="size-8" aria-label="New chat" disabled>
          <PenSquare className="size-4" />
        </Button>
      </div>
      <div className="flex-1 overflow-y-auto px-2 pb-2">
        {threads.map((t) => {
          const last = t.messages[t.messages.length - 1];
          const preview = last ? previewOf(last) : t.blurb;
          return (
            <button
              key={t.id}
              onClick={() => onSelect(t.id)}
              className={cn(
                "flex w-full items-center gap-3 rounded-lg px-2 py-2.5 text-left transition-colors",
                t.id === activeId ? "bg-accent" : "hover:bg-accent/50",
              )}
            >
              <ContactAvatar contact={t.contact} className="size-10" />
              <div className="min-w-0 flex-1">
                <div className="flex items-baseline justify-between gap-2">
                  <span className="truncate text-sm font-medium">{t.contact.name}</span>
                  {last && (
                    <span className="shrink-0 text-[11px] text-muted-foreground">
                      {formatTime(last.at)}
                    </span>
                  )}
                </div>
                <p className="truncate text-xs text-muted-foreground">{preview}</p>
              </div>
            </button>
          );
        })}
      </div>
    </aside>
  );
}

/* ---- right: the active thread ---- */

function ChatPane({
  client,
  company,
  thread,
  setMessages,
  onReply,
  onOpenList,
  className,
}: {
  client: OpenCompanyClient;
  company: string | null;
  thread: Thread;
  setMessages: (threadId: string, updater: (m: ChatMessage[]) => ChatMessage[]) => void;
  onReply?: () => void;
  onOpenList: () => void;
  className?: string;
}) {
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);
  const scroller = useRef<HTMLDivElement>(null);

  const messages = thread.messages;
  const groups = useMemo(() => groupMessages(messages, thread.contact), [messages, thread.contact]);

  useEffect(() => {
    scroller.current?.scrollTo({ top: scroller.current.scrollHeight, behavior: "smooth" });
  }, [messages, sending]);

  async function send() {
    const text = draft.trim();
    if (!text || sending) return;
    setDraft("");
    setMessages(thread.id, (m) => [...m, makeMessage("you", text)]);
    setSending(true);
    try {
      // Address the active desk thread (issue #53). "main" and any id the
      // company doesn't define fall to the orchestrator on the backend.
      const reply = await client.chat(text, company, thread.id);
      const replies = reply.responses.length
        ? reply.responses.map((r) =>
            makeMessage("company", r.text, { channel: r.channel, steps: r.steps }),
          )
        : [makeMessage("system", "(no reply)")];
      setMessages(thread.id, (m) => [...m, ...replies]);
      onReply?.();
    } catch (err) {
      const msg = err instanceof ApiError ? err.message : "something went wrong";
      setMessages(thread.id, (m) => [...m, makeMessage("system", `Couldn't send — ${msg}`)]);
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
    <section className={cn("flex-1 flex-col overflow-hidden", className)}>
      {/* Contact header */}
      <div className="flex items-center gap-3 border-b px-4 py-2.5">
        <Button
          variant="ghost"
          size="icon"
          className="size-8 md:hidden"
          onClick={onOpenList}
          aria-label="Back to chats"
        >
          <ArrowLeft className="size-4" />
        </Button>
        <ContactAvatar contact={thread.contact} className="size-9" />
        <div className="min-w-0">
          <p className="truncate text-sm font-semibold">{thread.contact.name}</p>
          <p className="truncate text-xs text-muted-foreground">{thread.blurb}</p>
        </div>
      </div>

      {/* Transcript */}
      <div
        ref={scroller}
        className="flex-1 overflow-y-auto"
        style={{
          backgroundImage:
            "radial-gradient(color-mix(in oklab, var(--muted-foreground) 9%, transparent) 1px, transparent 1px)",
          backgroundSize: "22px 22px",
        }}
      >
        <div className="mx-auto flex w-full max-w-3xl flex-col gap-1.5 px-4 py-6">
          {messages.length === 0 && <EmptyConversation contact={thread.contact} />}
          {groups.map((g, i) => (
            <MessageGroup key={g.key} group={g} prev={groups[i - 1]} />
          ))}
          {sending && <TypingIndicator contact={thread.contact} />}
        </div>
      </div>

      {/* Composer */}
      <div className="border-t bg-background/80 backdrop-blur">
        <div className="mx-auto w-full max-w-3xl px-4 py-3">
          <div className="relative flex items-end gap-2 rounded-xl border bg-card p-2 shadow-sm focus-within:ring-2 focus-within:ring-ring/50">
            <Textarea
              value={draft}
              onChange={(e) => setDraft(e.target.value)}
              onKeyDown={onKeyDown}
              placeholder={`Message ${thread.contact.name}…`}
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
    </section>
  );
}

/* ---- message rendering ---- */

interface Sender {
  key: string;
  name: string;
  kind: "you" | "company" | "agent" | "system";
  tone?: string;
}

interface Group {
  key: string;
  sender: Sender;
  at: number;
  messages: ChatMessage[];
}

function MessageGroup({ group, prev }: { group: Group; prev?: Group }) {
  const showDay = !prev || !sameDay(prev.at, group.at);

  if (group.sender.kind === "system") {
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

  const mine = group.sender.kind === "you";
  return (
    <>
      {showDay && <DaySeparator at={group.at} />}
      <div className={cn("mt-2 flex gap-2.5", mine ? "flex-row-reverse" : "flex-row")}>
        {!mine && <SenderAvatar sender={group.sender} />}
        <div className={cn("flex min-w-0 flex-col gap-1", mine ? "items-end" : "items-start")}>
          {!mine && (
            <div className="px-1">
              <span className="text-xs font-semibold">{group.sender.name}</span>
            </div>
          )}
          {group.messages.map((m, i) => (
            <Fragment key={m.id}>
              {!mine && m.steps && m.steps.length > 0 && <StepTimeline steps={m.steps} />}
              <Bubble message={m} mine={mine} last={i === group.messages.length - 1} />
            </Fragment>
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
        "relative max-w-[85%] rounded-2xl px-3 py-1.5 text-sm leading-relaxed shadow-sm sm:max-w-[75%]",
        mine ? "bg-primary text-primary-foreground" : "border bg-card text-card-foreground",
        last && (mine ? "rounded-br-md" : "rounded-bl-md"),
      )}
    >
      <span className="whitespace-pre-wrap break-words align-bottom">{message.text}</span>
      <span
        className={cn(
          "float-right ml-2 translate-y-1 select-none text-[10px]",
          mine ? "text-primary-foreground/70" : "text-muted-foreground",
        )}
      >
        {formatTime(message.at)}
      </span>
    </div>
  );
}

/* ---- processing-step timeline (Activity-trace) ---- */

/**
 * The scrubbed processing steps behind a company reply, rendered above its
 * bubble. Collapsed by default to a one-line "N steps · M failed" summary; auto
 * expands when any step failed so a silent MCP failure is visible, not buried.
 * Renders nothing when there are no steps (a memory-served / tool-less reply).
 */
function StepTimeline({ steps }: { steps: TurnStep[] }) {
  const failed = steps.filter((s) => s.status === "error").length;
  const hasError = failed > 0;
  const [open, setOpen] = useState(hasError);

  if (steps.length === 0) return null;

  return (
    <div className="w-full max-w-[85%] sm:max-w-[75%]">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
        className={cn(
          "flex items-center gap-1 rounded-md px-1.5 py-0.5 text-[11px] font-medium transition-colors hover:bg-accent/60",
          hasError ? "text-destructive" : "text-muted-foreground",
        )}
      >
        {open ? <ChevronDown className="size-3" /> : <ChevronRight className="size-3" />}
        <span>
          {steps.length} step{steps.length === 1 ? "" : "s"}
          {failed > 0 && ` · ${failed} failed`}
        </span>
      </button>
      {open && (
        <ol className="mt-0.5 flex flex-col gap-1 rounded-lg border bg-card/60 px-2.5 py-1.5">
          {steps.map((step, i) => (
            <StepRow key={i} step={step} />
          ))}
        </ol>
      )}
    </div>
  );
}

function StepRow({ step }: { step: TurnStep }) {
  const error = step.status === "error";
  const Icon = stepIcon(step.kind);
  return (
    <li
      className={cn(
        "flex items-center gap-1.5 text-[11px] leading-relaxed",
        error ? "text-destructive" : "text-muted-foreground",
      )}
    >
      <Icon className={cn("size-3 shrink-0", step.status === "running" && "animate-pulse")} />
      <span className={cn("font-medium", !error && "text-foreground/80")}>{step.label}</span>
      {step.detail && <span className="min-w-0 truncate">— {step.detail}</span>}
      {typeof step.elapsedMs === "number" && (
        <span className="ml-auto shrink-0 tabular-nums opacity-70">
          {formatElapsed(step.elapsedMs)}
        </span>
      )}
    </li>
  );
}

function stepIcon(kind: TurnStepKind) {
  switch (kind) {
    case "tool_call":
      return Wrench;
    case "thinking":
      return Brain;
    case "note":
      return AlertTriangle;
    default:
      return Wrench;
  }
}

function formatElapsed(ms: number): string {
  return ms < 1000 ? `${ms}ms` : `${(ms / 1000).toFixed(1)}s`;
}

function SenderAvatar({ sender }: { sender: Sender }) {
  return (
    <div className="mt-5">
      <ContactAvatar
        contact={{ name: sender.name, kind: sender.kind === "company" ? "company" : "agent", tone: sender.tone }}
        className="size-8"
      />
    </div>
  );
}

function ContactAvatar({ contact, className }: { contact: ThreadContact; className?: string }) {
  if (contact.kind === "company") {
    return (
      <div
        className={cn(
          "flex shrink-0 items-center justify-center rounded-full bg-primary text-primary-foreground",
          className,
        )}
        aria-hidden
      >
        <Building2 className="size-1/2" />
      </div>
    );
  }
  return (
    <div
      className={cn(
        "flex shrink-0 items-center justify-center rounded-full text-xs font-semibold",
        toneClass(contact.tone),
        className,
      )}
      aria-hidden
    >
      {initials(contact.name)}
    </div>
  );
}

function TypingIndicator({ contact }: { contact: ThreadContact }) {
  return (
    <div className="mt-2 flex gap-2.5">
      <ContactAvatar contact={contact} className="mt-0.5 size-8" />
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

function EmptyConversation({ contact }: { contact: ThreadContact }) {
  return (
    <div className="mt-16 flex flex-col items-center gap-3 text-center">
      <ContactAvatar contact={contact} className="size-12" />
      <div className="space-y-1">
        <p className="font-medium">Message {contact.name}</p>
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

function groupMessages(messages: ChatMessage[], contact: ThreadContact): Group[] {
  const groups: Group[] = [];
  for (const m of messages) {
    const sender = senderOf(m, contact);
    const tail = groups[groups.length - 1];
    if (
      tail &&
      tail.sender.key === sender.key &&
      m.at - tail.at < GROUP_WINDOW_MS &&
      sameDay(tail.at, m.at)
    ) {
      tail.messages.push(m);
      tail.at = m.at;
    } else {
      groups.push({ key: m.id, sender, at: m.at, messages: [m] });
    }
  }
  return groups;
}

const COMPANY_VOICE = new Set(["operator", "console", "chat", "owner", ""]);

/** Resolve a message's sender within a thread: the company side wears the
 *  thread's contact identity unless the reply names a distinct channel. */
function senderOf(m: ChatMessage, contact: ThreadContact): Sender {
  if (m.from === "you") return { key: "you", name: "You", kind: "you" };
  if (m.from === "system") return { key: "system", name: "System", kind: "system" };
  const channel = m.channel?.trim().toLowerCase() ?? "";
  if (channel && !COMPANY_VOICE.has(channel)) {
    return { key: `agent:${channel}`, name: titleize(m.channel!), kind: "agent", tone: channel };
  }
  return { key: `contact:${contact.name}`, name: contact.name, kind: contact.kind, tone: contact.tone };
}

const TONES: Record<string, string> = {
  sky: "bg-sky-500/15 text-sky-600 dark:text-sky-400",
  violet: "bg-violet-500/15 text-violet-600 dark:text-violet-400",
  amber: "bg-amber-500/15 text-amber-600 dark:text-amber-400",
  emerald: "bg-emerald-500/15 text-emerald-600 dark:text-emerald-400",
  rose: "bg-rose-500/15 text-rose-600 dark:text-rose-400",
  cyan: "bg-cyan-500/15 text-cyan-600 dark:text-cyan-400",
};
const TONE_KEYS = Object.keys(TONES);

function toneClass(tone?: string): string {
  if (tone && TONES[tone]) return TONES[tone];
  const key = tone ?? "";
  let hash = 0;
  for (let i = 0; i < key.length; i++) hash = (hash * 31 + key.charCodeAt(i)) | 0;
  return TONES[TONE_KEYS[Math.abs(hash) % TONE_KEYS.length]];
}

function initials(name: string): string {
  const parts = name.trim().split(/\s+/).slice(0, 2);
  return parts.map((p) => p.charAt(0).toUpperCase()).join("") || "?";
}

function titleize(s: string): string {
  return s.replace(/[._-]+/g, " ").replace(/\w\S*/g, (w) => w.charAt(0).toUpperCase() + w.slice(1));
}

function previewOf(m: ChatMessage): string {
  const prefix = m.from === "you" ? "You: " : "";
  return `${prefix}${m.text}`;
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
