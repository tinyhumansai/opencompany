// WhatsApp-style two-pane chat with the company: a thread list on the left, the
// transcript on the right.
//
// The transcript half runs on **assistant-ui** — `@assistant-ui/react` — via an
// external-store runtime, so the console keeps owning the messages while the
// library owns the composer, the viewport and the run state. See
// `conversation/runtime.ts` for why that is the runtime this surface uses, and
// `conversation/model.ts` for how a `ChatMessage` crosses the boundary.
//
// The pieces around the transcript stay this product's own: the thread list is
// the company's desks and teammates rather than an assistant's saved
// conversations, and the strip above the composer steers named in-flight runs
// (issue #111) rather than cancelling the last reply.

import { useEffect, useState } from "react";
import { ArrowLeft } from "lucide-react";
import { AssistantRuntimeProvider } from "@assistant-ui/react";

import type { OpenCompanyClient } from "@/api/client";
import type { TurnStep } from "@/api/types";
import { PageHeader } from "@/components/page-header";
import { Button } from "@/components/ui/button";
import { useIsMobile } from "@/hooks/use-mobile";
import type { ChatMessage } from "@/lib/chat";
import type { OpenTurn } from "@/lib/live-reply";
import type { Thread } from "@/lib/threads";
import { cn } from "@/lib/utils";
import { InflightStrip } from "@/views/conversation/InflightStrip";
import { ContactAvatar } from "@/views/conversation/parts";
import { ThreadList } from "@/views/conversation/ThreadList";
import { Transcript } from "@/views/conversation/Transcript";
import { useConversationRuntime } from "@/views/conversation/runtime";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  threads: Thread[];
  activeId: string;
  onSelect: (id: string) => void;
  /**
   * Reports the active thread as viewed, with the ids of its loaded messages.
   *
   * The mention badge is keyed to the Chat rail's channels, but the `main`
   * thread (and every desk thread) renders here too — and a company's main
   * conversation lives *only* here once it has real desks, never in a rail
   * channel. So this surface has to report its own views, or a mention whose
   * subject sits in the main thread could never be cleared: the rail's channel
   * it badges never renders that thread. The loaded ids gate the clear exactly
   * the way ChatView's do — the mention clears only once the message it names
   * is actually on screen.
   */
  onThreadViewed?: (threadId: string, loadedMessageIds: ReadonlySet<string>) => void;
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
  /**
   * Marks a thread's chat POST as in flight (parent suppresses the SSE echo).
   * Returns the generation the shell stamped this send's receipt with (issue
   * #1935 review, codex 3892702774) — forwarded straight through to
   * `ChatPane`/`useConversationRuntime`, which threads it to whichever
   * terminal callback below the POST reaches.
   */
  onSendStart?: (threadId: string) => number | undefined;
  /** Clears the in-flight mark + live timeline once the POST resolves. */
  onSendEnd?: (threadId: string, gen?: number) => void;
  /**
   * The host accepted the turn and answered `202` rather than the reply
   * (issue #983). Unlike `onSendEnd` this does NOT end the turn — it only ends
   * the POST, so the parent keeps the working row up and stops suppressing the
   * live reply frame, which in this mode is the delivery path.
   */
  onSendDetached?: (threadId: string, turnId?: string, gen?: number) => void;
  /**
   * The chat POST **threw** (issue #1000). The third outcome, and not
   * `onSendEnd`: that one promises the parent the reply is already on screen,
   * which licenses it to drop the live frame it was holding. A throw rendered
   * nothing and the turn usually outlives the request, so that frame is the
   * only copy of the answer.
   */
  onSendFailed?: (threadId: string, gen?: number) => void;
  /** Turns accepted but not settled, by thread id — survives a reload (#983). */
  openTurns?: Record<string, OpenTurn[]>;
}

export function Conversation({
  client,
  company,
  threads,
  activeId,
  onSelect,
  setMessages,
  onReply,
  taskEventTick,
  liveStepsByThread,
  onSendStart,
  onSendEnd,
  onSendDetached,
  onSendFailed,
  openTurns,
  onThreadViewed,
}: Props) {
  const active = threads.find((t) => t.id === activeId) ?? threads[0];
  // On mobile, the list and the chat share the pane — track which is showing.
  const [mobilePane, setMobilePane] = useState<"list" | "chat">("chat");
  // The transcript is on screen on desktop (both panes render side by side)
  // and on mobile only while the chat pane is the active one. A view report
  // from a hidden pane — the operator opened the thread list, or resized down
  // after selecting one — would clear a mention whose text was never visible.
  const isMobile = useIsMobile();
  const chatVisible = !isMobile || mobilePane === "chat";

  // A thread view is the mention-badge's read path on this surface (see the
  // prop doc): report it when the thread changes and as its transcript grows,
  // so a mention whose subject just loaded can clear the moment it is on
  // screen rather than waiting for another visit. Never report a thread whose
  // transcript is hidden (Codex P1).
  useEffect(() => {
    if (!chatVisible) return;
    onThreadViewed?.(active.id, new Set(active.messages.map((m) => m.id)));
  }, [active.id, active.messages.length, onThreadViewed, chatVisible]);

  return (
    <div className="flex min-h-0 flex-1 overflow-hidden">
      <PageHeader hidden title="Conversation" />
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
        // A fresh runtime per thread. Keying here rather than resetting inside
        // is what keeps the composer draft, the scroll position and the run
        // state from following the operator into a different conversation.
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
        onSendDetached={onSendDetached}
        onSendFailed={onSendFailed}
        openTurn={openTurns?.[active.id]?.[0]}
        onOpenList={() => setMobilePane("list")}
        className={cn("md:flex", mobilePane === "chat" ? "flex" : "hidden")}
      />
    </div>
  );
}

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
  onSendDetached,
  onSendFailed,
  openTurn,
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
  onSendStart?: (threadId: string) => number | undefined;
  onSendEnd?: (threadId: string, gen?: number) => void;
  onSendDetached?: (threadId: string, turnId?: string, gen?: number) => void;
  onSendFailed?: (threadId: string, gen?: number) => void;
  /** This thread's turn, when one is accepted but not settled (#983). */
  openTurn?: OpenTurn;
  onOpenList: () => void;
  className?: string;
}) {
  const [sending, setSending] = useState(false);
  const { runtime, addToBoard, addingId, dismissCard, dismissingCardId } = useConversationRuntime({
    client,
    company,
    thread,
    setMessages,
    onReply,
    onSendStart,
    onSendEnd,
    onSendDetached,
    onSendFailed,
    // A detached turn keeps the thread running after its POST resolved, so the
    // composer stays gated on the turn and not merely on the request.
    running: sending || !!openTurn,
    setSending,
  });

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

      <AssistantRuntimeProvider runtime={runtime}>
        <Transcript
          contact={thread.contact}
          readOnly={thread.readOnly}
          onAddToBoard={(m) => void addToBoard(m)}
          addingId={addingId}
          onDismissCard={(id) => void dismissCard(id)}
          dismissingCardId={dismissingCardId}
          sending={sending}
          openTurn={openTurn}
          liveSteps={liveSteps}
          footer={
            <InflightStrip client={client} company={company} taskEventTick={taskEventTick} />
          }
        />
      </AssistantRuntimeProvider>
    </section>
  );
}
