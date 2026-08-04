import { Fragment, useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  AlertTriangle,
  ArrowLeft,
  ArrowUp,
  Brain,
  Building2,
  ChevronDown,
  ChevronRight,
  CornerUpRight,
  Loader2,
  Pause,
  PenSquare,
  Send,
  SquareKanban,
  Wrench,
  X,
} from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import { ApiError, type TurnStep, type TurnStepKind } from "@/api/types";
import {
  createTask,
  listInflight,
  steerTask,
  type InflightRun,
  type SteerAction,
} from "@/api/tasks";
import { Markdown } from "@/components/markdown";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { cn } from "@/lib/utils";
import { type ChatMessage, makeMessage, titleFromMessage } from "@/lib/chat";
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
  /** Bumped on every task-lifecycle SSE event, so the in-flight strip refetches. */
  taskEventTick?: number;
  /**
   * The live in-flight tool timeline per thread, built from the transient
   * `tool_call`/`tool_result` SSE frames while a turn runs. Rendered under the
   * typing indicator and cleared by the parent when the final reply lands.
   */
  liveStepsByThread?: Record<string, TurnStep[]>;
  /** Marks a thread's chat POST as in flight (parent suppresses the SSE echo). */
  onSendStart?: (threadId: string) => void;
  /** Clears the in-flight mark + live timeline once the POST resolves. */
  onSendEnd?: (threadId: string) => void;
}

/** Consecutive messages from one sender within this window group together. */
const GROUP_WINDOW_MS = 5 * 60 * 1000;

/** WhatsApp-style two-pane chat: a thread list on the left, transcript right. */
export function Conversation({ client, company, threads, activeId, onSelect, setMessages, onReply, taskEventTick, liveStepsByThread, onSendStart, onSendEnd }: Props) {
  const active = threads.find((t) => t.id === activeId) ?? threads[0];
  // On mobile, the list and the chat share the pane — track which is showing.
  const [mobilePane, setMobilePane] = useState<"list" | "chat">("chat");

  return (
    <div className="flex min-h-0 flex-1 overflow-hidden">
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
        taskEventTick={taskEventTick}
        liveSteps={liveStepsByThread?.[active.id] ?? []}
        onSendStart={onSendStart}
        onSendEnd={onSendEnd}
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
    <aside className={cn("min-h-0 w-full shrink-0 flex-col border-r bg-card/40 md:w-80", className)}>
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
  taskEventTick,
  liveSteps,
  onSendStart,
  onSendEnd,
  onOpenList,
  className,
}: {
  client: OpenCompanyClient;
  company: string | null;
  thread: Thread;
  setMessages: (threadId: string, updater: (m: ChatMessage[]) => ChatMessage[]) => void;
  onReply?: () => void;
  taskEventTick?: number;
  liveSteps?: TurnStep[];
  onSendStart?: (threadId: string) => void;
  onSendEnd?: (threadId: string) => void;
  onOpenList: () => void;
  className?: string;
}) {
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);
  /** The message whose "Add to board" create is in flight (issue #246). */
  const [addingId, setAddingId] = useState<string | null>(null);
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
    onSendStart?.(thread.id);
    try {
      // Address the active desk thread (issue #53). "main" and any id the
      // company doesn't define fall to the orchestrator on the backend.
      const reply = await client.chat(text, company, thread.id);
      const replies = reply.responses.length
        ? reply.responses.map((r) =>
            // `taskId` (issue #246): when the turn opened a board card, the
            // bubble says so immediately. The same id is journaled onto the
            // reply, so the chip is still there after a transcript reload.
            makeMessage("company", r.text, {
              channel: r.channel,
              steps: r.steps,
              taskId: r.taskId,
            }),
          )
        : [makeMessage("system", "(no reply)")];
      setMessages(thread.id, (m) => [...m, ...replies]);
      onReply?.();
    } catch (err) {
      const msg = err instanceof ApiError ? err.message : "something went wrong";
      setMessages(thread.id, (m) => [...m, makeMessage("system", `Couldn't send — ${msg}`)]);
    } finally {
      setSending(false);
      onSendEnd?.(thread.id);
    }
  }

  /**
   * Turns one transcript message into a board card (issue #246).
   *
   * Deliberately goes through the REST create rather than asking the responder
   * to call `spawn_task`: only the orchestrator carries the delegation tools,
   * so a toolbelt route would work on the main thread and silently do nothing
   * on a desk or DM thread. Going through REST is what makes the action true on
   * *every* thread — which is the whole point — without widening the v1
   * depth-1 delegation design.
   *
   * `column` is omitted on purpose. Dropping a card into `in_progress` is what
   * dispatches an agent turn, so letting the server's intake default decide
   * keeps the human drag as the only thing that spends money. `assignee` is
   * omitted for the same reason: an unassigned card asks nothing of anyone.
   *
   * The composer draft is untouched on both paths — a failure surfaces as a
   * toast and nothing the operator typed is cleared.
   */
  const addToBoard = useCallback(
    async (message: ChatMessage) => {
      const title = titleFromMessage(message.text);
      if (!title || addingId) return;
      setAddingId(message.id);
      try {
        const created = await createTask(client, company, {
          title,
          // The full text as the note, so nothing is lost to the title's cap.
          note: message.text,
          originChatId: thread.id,
        });
        setMessages(thread.id, (all) =>
          all.map((m) => (m.id === message.id ? { ...m, taskId: created.id } : m)),
        );
        toast.success(`Added to the board — ${created.title}`);
      } catch (e) {
        toast.error(e instanceof Error ? e.message : "could not add this to the board");
      } finally {
        setAddingId(null);
      }
    },
    [addingId, client, company, setMessages, thread.id],
  );

  function onKeyDown(e: React.KeyboardEvent<HTMLTextAreaElement>) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      void send();
    }
  }

  return (
    <section className={cn("min-h-0 flex-1 flex-col overflow-hidden", className)}>
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
            <MessageGroup
              key={g.key}
              group={g}
              prev={groups[i - 1]}
              onAddToBoard={addToBoard}
              addingId={addingId}
            />
          ))}
          {sending && (
            <>
              {/* Live tool timeline — the running/done rows stream in over SSE as
                  the turn works, before the final reply lands (issue: tool calls
                  weren't visible until the turn finished). */}
              {liveSteps && liveSteps.length > 0 && <StepTimeline steps={liveSteps} />}
              <TypingIndicator contact={thread.contact} />
            </>
          )}
        </div>
      </div>

      {/* In-flight steer strip (issue #111) */}
      <InflightStrip client={client} company={company} taskEventTick={taskEventTick} />

      {/* Composer */}
      <div className="border-t bg-background/80 backdrop-blur">
        <div className="mx-auto w-full max-w-3xl px-4 py-3">
          <div
            data-tour="chat-composer"
            className="relative flex items-end gap-2 rounded-xl border bg-card p-2 shadow-sm focus-within:ring-2 focus-within:ring-ring/50"
          >
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

/* ---- in-flight steer strip (issue #111) ---- */

/** Past-tense badge copy while a steer of the given verb is in flight. */
const PENDING_LABEL: Record<string, string> = {
  pause: "pausing…",
  cancel: "cancelling…",
  redirect: "redirecting…",
};

/**
 * A strip above the composer listing the company's in-flight runs, so the
 * operator can steer them (issue #111) without leaving company chat: pause,
 * redirect, or cancel a dispatched task; cancel a sub-agent delegation. Reads
 * {@link listInflight} on mount and refetches on any successful steer and on
 * each task-lifecycle SSE tick. Renders nothing when nothing is in flight (or
 * when the host has no inflight route), so it stays out of the way.
 */
function InflightStrip({
  client,
  company,
  taskEventTick,
}: {
  client: OpenCompanyClient;
  company: string | null;
  taskEventTick?: number;
}) {
  const [runs, setRuns] = useState<InflightRun[]>([]);
  const mounted = useRef(true);

  const refresh = useCallback(async () => {
    try {
      const rows = await listInflight(client, company);
      if (mounted.current) setRuns(rows);
    } catch {
      // Best-effort surface: a host without the inflight route (404) just means
      // no strip. Clear rather than surface an error into the chat.
      if (mounted.current) setRuns([]);
    }
  }, [client, company]);

  useEffect(() => {
    mounted.current = true;
    void refresh();
    return () => {
      mounted.current = false;
    };
  }, [refresh]);

  // Live refetch when a task-lifecycle event rides the SSE stream.
  useEffect(() => {
    if (taskEventTick !== undefined) void refresh();
  }, [taskEventTick, refresh]);

  if (runs.length === 0) return null;

  return (
    <div className="border-t bg-muted/30">
      <div className="mx-auto w-full max-w-3xl px-4 py-2">
        <p className="mb-1.5 px-1 text-[11px] font-medium uppercase tracking-wide text-muted-foreground">
          In flight · {runs.length}
        </p>
        <div className="flex flex-col gap-1.5">
          {runs.map((run) => (
            <InflightRow key={run.key} run={run} onSteer={refresh} client={client} company={company} />
          ))}
        </div>
      </div>
    </div>
  );
}

function InflightRow({
  run,
  onSteer,
  client,
  company,
}: {
  run: InflightRun;
  onSteer: () => Promise<void> | void;
  client: OpenCompanyClient;
  company: string | null;
}) {
  const [busy, setBusy] = useState(false);
  const [redirecting, setRedirecting] = useState(false);
  const [instruction, setInstruction] = useState("");

  // A pending server-side steer, or an optimistic local one, freezes the row.
  const pending = run.pendingAction ?? null;
  const disabled = busy || pending !== null;

  async function steer(action: SteerAction, opts?: { instruction?: string; confirm?: boolean }) {
    setBusy(true);
    try {
      await steerTask(client, company, run.key, { action, ...opts });
      setRedirecting(false);
      setInstruction("");
      await onSteer();
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "could not steer the task");
    } finally {
      setBusy(false);
    }
  }

  function onCancel() {
    // Cancel is destructive — the backend also requires `confirm: true`.
    if (!window.confirm(`Cancel “${run.title}”? This stops the run.`)) return;
    void steer("cancel", { confirm: true });
  }

  function onRedirect() {
    const text = instruction.trim();
    if (!text) return;
    void steer("redirect", { instruction: text });
  }

  const isTask = run.kind === "task";

  return (
    <div className="rounded-lg border bg-card px-2.5 py-1.5">
      <div className="flex items-center gap-2">
        <div className="min-w-0 flex-1">
          <p className="truncate text-xs font-medium">{run.title}</p>
          <p className="truncate text-[11px] text-muted-foreground">
            {run.kind === "delegation" ? "Delegation" : "Task"} · {run.agentId}
          </p>
        </div>

        {pending !== null ? (
          <span className="shrink-0 rounded-full bg-muted px-2 py-0.5 text-[10px] font-medium text-muted-foreground">
            {PENDING_LABEL[pending] ?? "steering…"}
          </span>
        ) : (
          <div className="flex shrink-0 items-center gap-1">
            {busy && <Loader2 className="size-3.5 animate-spin text-muted-foreground" />}
            {isTask && (
              <>
                <Button
                  variant="ghost"
                  size="sm"
                  className="h-7 px-2 text-xs"
                  disabled={disabled}
                  onClick={() => void steer("pause")}
                >
                  <Pause className="mr-1 size-3.5" />
                  Pause
                </Button>
                <Button
                  variant="ghost"
                  size="sm"
                  className="h-7 px-2 text-xs"
                  disabled={disabled}
                  aria-pressed={redirecting}
                  onClick={() => setRedirecting((r) => !r)}
                >
                  <CornerUpRight className="mr-1 size-3.5" />
                  Redirect
                </Button>
              </>
            )}
            <Button
              variant="ghost"
              size="sm"
              className="h-7 px-2 text-xs text-destructive hover:text-destructive"
              disabled={disabled}
              onClick={onCancel}
            >
              <X className="mr-1 size-3.5" />
              Cancel
            </Button>
          </div>
        )}
      </div>

      {isTask && redirecting && pending === null && (
        <div className="mt-1.5 flex items-center gap-1.5">
          <Input
            value={instruction}
            onChange={(e) => setInstruction(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                onRedirect();
              }
            }}
            placeholder="New instruction for this task…"
            aria-label={`New instruction for ${run.title}`}
            className="h-7 flex-1 text-xs"
            autoFocus
          />
          <Button
            size="icon"
            className="size-7 shrink-0"
            disabled={disabled || !instruction.trim()}
            onClick={onRedirect}
            aria-label="Send redirect"
          >
            <Send className="size-3.5" />
          </Button>
        </div>
      )}
    </div>
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

function MessageGroup({
  group,
  prev,
  onAddToBoard,
  addingId,
}: {
  group: Group;
  prev?: Group;
  /** Turns one message into a board card (issue #246). */
  onAddToBoard: (message: ChatMessage) => void;
  /** The message whose create is in flight, if any. */
  addingId: string | null;
}) {
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
              {/* The bubble and its hover action share a row so the action can
                  sit outside the bubble without overlapping the text. `group`
                  scopes the reveal to this one message. */}
              <div
                className={cn(
                  "group/msg flex max-w-full items-center gap-1",
                  mine ? "flex-row-reverse" : "flex-row",
                )}
              >
                <Bubble message={m} mine={mine} last={i === group.messages.length - 1} />
                <AddToBoardAction
                  message={m}
                  busy={addingId === m.id}
                  disabled={addingId !== null && addingId !== m.id}
                  onAdd={onAddToBoard}
                />
              </div>
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
      <span
        className={cn(
          "float-right ml-2 translate-y-1 select-none text-[10px]",
          mine ? "text-primary-foreground/70" : "text-muted-foreground",
        )}
      >
        {formatTime(message.at)}
      </span>
      {mine ? (
        // User-typed bubbles stay plain text so literal asterisks/underscores a
        // person types aren't reinterpreted as markdown.
        <span className="whitespace-pre-wrap break-words align-bottom">{message.text}</span>
      ) : (
        // Company/agent replies render markdown so **bold**, lists, and links
        // show formatted instead of leaking raw markup. Trim the first/last
        // block margins so a reply stays flush inside the tight bubble padding.
        <Markdown className="[&>:first-child]:mt-0 [&>:last-child]:mb-0">{message.text}</Markdown>
      )}
      {message.taskId && <CardChip taskId={message.taskId} mine={mine} />}
    </div>
  );
}

/**
 * The "this message has a card" chip (issue #246), linking to the card's detail
 * screen.
 *
 * Two provenances, one render. On a company reply it means the turn opened a
 * card by itself — that id is journaled onto the reply, so this chip survives a
 * transcript reload. On your own message it means you pressed "Add to board";
 * that link lives in the session only, because the durable record of it is
 * `originChatId` on the card rather than anything on the operator message.
 *
 * `clear-both` because the bubble floats its timestamp right; without it the
 * chip tucks under the time instead of starting a fresh line.
 */
function CardChip({ taskId, mine }: { taskId: string; mine: boolean }) {
  return (
    <a
      href={`#/tasks/${encodeURIComponent(taskId)}`}
      className={cn(
        "mt-1.5 flex w-fit clear-both items-center gap-1 rounded-full px-2 py-0.5 text-[11px] font-medium transition-opacity hover:opacity-80",
        mine
          ? "bg-primary-foreground/15 text-primary-foreground"
          : "bg-accent text-accent-foreground",
      )}
    >
      <SquareKanban className="size-3 shrink-0" />
      {mine ? "Added to the board" : "Card opened"}
    </a>
  );
}

/**
 * The per-message "Add to board" affordance (issue #246).
 *
 * On every message in every thread — desk, DM and orchestrator — because it
 * creates through REST rather than through the responder's toolbelt, which only
 * the orchestrator carries.
 *
 * Renders nothing once the message already has a card, so a second press cannot
 * open a duplicate; and nothing for a message with no text to title a card
 * from. Revealed on hover on pointer devices, but always present in the DOM and
 * focusable, so it is reachable by keyboard and on touch.
 */
function AddToBoardAction({
  message,
  busy,
  disabled,
  onAdd,
}: {
  message: ChatMessage;
  busy: boolean;
  disabled: boolean;
  onAdd: (message: ChatMessage) => void;
}) {
  if (message.taskId || !titleFromMessage(message.text)) return null;
  return (
    <Button
      variant="ghost"
      size="icon"
      className="size-7 shrink-0 text-muted-foreground opacity-0 transition-opacity focus-visible:opacity-100 group-hover/msg:opacity-100"
      onClick={() => onAdd(message)}
      disabled={busy || disabled}
      title="Add to board"
      aria-label="Add to board"
    >
      {busy ? (
        <Loader2 className="size-3.5 animate-spin" />
      ) : (
        <SquareKanban className="size-3.5" />
      )}
    </Button>
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
