// The assistant-ui runtime for one conversation thread.
//
// assistant-ui offers several runtimes; this surface uses the **external
// store** one, and the choice is the whole design. The console's transcripts do
// not live in this component and must not: `AppShell` owns them, because a
// reply can arrive over SSE while this view is unmounted, and because a turn
// can outlive the request that started it (issue #983). An external-store
// runtime is the one runtime that takes the message array as an input rather
// than owning it — assistant-ui renders and drives the composer, the shell
// keeps being the single source of truth for what was said.
//
// What that buys, concretely: composer state, Enter/Shift+Enter, textarea
// autosize, focus handling, viewport auto-scroll with a scroll-to-bottom
// affordance, and a run state machine — none of which this file has to own or
// test. What it does NOT change: the transport. The host is still the Rust
// `POST /chat` and its event stream; nothing here speaks a data-stream
// protocol.

import { useCallback, useMemo, useState } from "react";
import { toast } from "sonner";
import {
  useExternalStoreRuntime,
  type AppendMessage,
  type AssistantRuntime,
} from "@assistant-ui/react";

import type { OpenCompanyClient } from "@/api/client";
import { createTask, deleteTask } from "@/api/tasks";
import { ApiError, isDetachedChat } from "@/api/types";
import { clearTaskCard, makeMessage, titleFromMessage, type ChatMessage } from "@/lib/chat";
import type { Thread } from "@/lib/threads";
import { decorate, soloLine, textOf, toThreadMessageLike } from "./model";

export interface ConversationRuntimeOptions {
  client: OpenCompanyClient;
  company: string | null;
  thread: Thread;
  setMessages: (threadId: string, updater: (m: ChatMessage[]) => ChatMessage[]) => void;
  /** Called after a reply lands, so the shell can refresh approvals/status. */
  onReply?: () => void;
  /**
   * Marks this thread's chat POST as in flight (parent suppresses the SSE
   * echo). Returns the generation the shell stamped this send's receipt with
   * (issue #1935 review, codex 3892702774) — captured below and threaded
   * through whichever terminal outcome this POST reaches, so the shell can
   * tell "my own armed receipt settling" apart from a newer send having
   * re-armed the same (possibly cross-company-reused) thread id in the
   * meantime. See `shouldClearReceipt` in `ChatLiveReceipt.tsx`.
   */
  onSendStart?: (threadId: string) => number | undefined;
  /** Clears the in-flight mark + live timeline once the POST resolves. */
  onSendEnd?: (threadId: string, gen?: number) => void;
  /** The host answered `202` and the turn continues on the stream (#983). */
  onSendDetached?: (threadId: string, turnId?: string, gen?: number) => void;
  /** The chat POST threw and the turn probably outlived it (#1000). */
  onSendFailed?: (threadId: string, gen?: number) => void;
  /** Whether a turn is open on this thread — a live POST or a detached one. */
  running: boolean;
  /** Reports a send starting/ending, so the view can hold its working row. */
  setSending: (sending: boolean) => void;
}

/** The runtime plus the board-card actions the transcript renders alongside it. */
export interface ConversationRuntime {
  runtime: AssistantRuntime;
  /** Turns one transcript message into a board card (issue #246). */
  addToBoard: (message: ChatMessage) => Promise<void>;
  /** The message whose card create is in flight, if any. */
  addingId: string | null;
  /** Deletes the card a line opened and drops its chip (issue #984). */
  dismissCard: (taskId: string) => Promise<void>;
  /**
   * The **card** whose delete is in flight — a task id, unlike its sibling
   * {@link ConversationRuntime.addingId}, which is a message id. Named for the
   * namespace it holds so the two cannot be read as the same kind of thing.
   */
  dismissingCardId: string | null;
}

export function useConversationRuntime(opts: ConversationRuntimeOptions): ConversationRuntime {
  const {
    client,
    company,
    thread,
    setMessages,
    onReply,
    onSendStart,
    onSendEnd,
    onSendDetached,
    onSendFailed,
    running,
    setSending,
  } = opts;

  const [addingId, setAddingId] = useState<string | null>(null);
  const [dismissingCardId, setDismissingCardId] = useState<string | null>(null);

  // Decorated once per snapshot, then read by index — see `decorate`.
  const lines = useMemo(() => decorate(thread.messages, thread.contact), [thread.messages, thread.contact]);
  // Indexed rather than re-derived, and checked rather than trusted: see
  // `soloLine` for the snapshot skew this guards against.
  const convertMessage = useCallback(
    (message: ChatMessage, index: number) => {
      const line = lines[index];
      return toThreadMessageLike(
        line && line.message === message ? line : soloLine(message, thread.contact),
      );
    },
    [lines, thread.contact],
  );

  const onNew = useCallback(
    async (appended: AppendMessage) => {
      // Belt to the composer's own `disabled` state below: never mutate state
      // or call `client.chat` for a channel the server's read-only guard will
      // refuse anyway (issue #1757). `threadsFromDesks` builds this thread
      // list straight from `/desks`, so a bypass of the disabled input still
      // cannot reach the network.
      if (thread.readOnly) return;
      const text = textOf(appended);
      if (!text) return;
      setMessages(thread.id, (m) => [...m, makeMessage("you", text)]);
      setSending(true);
      // The generation the shell stamped this send's receipt with, if any
      // (issue #1935 review, codex 3892702774). Threaded through to whichever
      // terminal callback this POST reaches below, so a clear this send
      // triggers can never delete a receipt a *later* send — on this same
      // Conversation surface, or on `ChatView` for the same reused thread id —
      // has since armed. See `shouldClearReceipt`'s doc for the cross-company
      // race this closes.
      const gen = onSendStart?.(thread.id);
      // Which of the POST's three outcomes happened, reported once in the
      // `finally`. Only `"resolved"` means the reply reached the screen — the
      // other two leave a turn running with the stream as its delivery path.
      let outcome: "resolved" | "detached" | "failed" = "resolved";
      try {
        // Address the active desk thread (issue #53). "main" and any id the
        // company doesn't define fall to the orchestrator on the backend.
        //
        // `detach` is asked for, never assumed: a host that predates it answers
        // the full synchronous body, so the branch below reads what came back.
        const answer = await client.chat(text, company, thread.id, undefined, undefined, true);
        if (isDetachedChat(answer)) {
          outcome = "detached";
          // The reply arrives on the stream, and durably in `chat/history` when
          // the turn settles. Nothing to render here — but the id IS known now,
          // at accept time rather than at settle, which is the improvement.
          onSendDetached?.(thread.id, answer.turnId, gen);
          return;
        }
        const replies = answer.responses.length
          ? answer.responses.map((r) =>
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
        outcome = "failed";
        // Still said even when the reply lands on the stream a moment later: the
        // request did fail, and the operator has no other way to know whether
        // their message was taken.
        const msg = err instanceof ApiError ? err.message : "something went wrong";
        setMessages(thread.id, (m) => [...m, makeMessage("system", `Couldn't send — ${msg}`)]);
      } finally {
        setSending(false);
        // A detached turn is not over when its POST is: ending the send here
        // would clear the live timeline and drop the working row mid-turn.
        //
        // Nor is a *failed* one, which is the easier miss. `onSendEnd` tells the
        // parent the reply is on screen and so licenses it to drop the live frame
        // it held; a throw rendered nothing and the turn carries on regardless, so
        // that frame is the only copy of the answer this console will be handed.
        if (outcome === "resolved") onSendEnd?.(thread.id, gen);
        else if (outcome === "failed") onSendFailed?.(thread.id, gen);
      }
    },
    [
      client,
      company,
      onReply,
      onSendDetached,
      onSendEnd,
      onSendFailed,
      onSendStart,
      setMessages,
      setSending,
      thread.id,
      thread.readOnly,
    ],
  );

  const runtime = useExternalStoreRuntime<ChatMessage>({
    messages: thread.messages,
    isRunning: running,
    convertMessage,
    onNew,
    // No `onEdit`, `onReload`, `onCancel` or `setMessages`: this host has no
    // edit, regenerate or branch semantics, and withholding the callback is how
    // assistant-ui is told not to offer the affordance. Cancelling an open turn
    // is a real action here, but it belongs to the run rather than to the
    // message — it is the in-flight strip below the transcript, which can steer
    // a dispatched task by name, not just abandon the last reply.
  });

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

  /**
   * Delete the board card a chat line opened, and stop drawing its chip
   * (issue #984).
   *
   * #442 allowed a turn to open a card from an ordinary message on the grounds
   * that *"a spurious card can be dismissed in one click"*. That click did not
   * exist on this surface: the chip was a bare link to the card's detail
   * screen, so dismissing a mis-fired card meant leaving chat, finding the
   * card, and deleting it there. This is that click.
   *
   * Deletes on the host FIRST and clears the chip only on success, which is
   * the opposite of the optimistic reaction toggle on the Chat tab and
   * deliberately so: a reaction that rolls back costs nothing, whereas a chip
   * that vanishes while the card survives tells the operator the board is clean
   * when it is not. A refusal leaves the chip exactly where it was and says why.
   *
   * Clears by CARD id rather than by the message clicked - see
   * {@link clearTaskCard}. Once the card is gone, every chip naming it is a
   * link to a 404, not just the one under the pointer.
   */
  const dismissCard = useCallback(
    async (taskId: string) => {
      if (dismissingCardId) return;
      setDismissingCardId(taskId);
      try {
        await deleteTask(client, company, taskId);
        setMessages(thread.id, (all) => clearTaskCard(all, taskId));
        toast.success("Card dismissed.");
      } catch (e) {
        // A 404 means the card is already gone — deleted from the board, most
        // likely, which tells this surface nothing. The chip would otherwise be
        // a permanent link to a 404 that clicking can never clear. Treat it as
        // the success it is for the operator. Copy matches `ChatView`: the same
        // action on two surfaces should not report itself two ways.
        if (e instanceof ApiError && e.status === 404) {
          setMessages(thread.id, (all) => clearTaskCard(all, taskId));
          toast.success("That card was already gone — chip cleared.");
        } else {
          toast.error(e instanceof Error && e.message ? e.message : "Couldn't dismiss that card.");
        }
      } finally {
        setDismissingCardId(null);
      }
    },
    [client, company, dismissingCardId, setMessages, thread.id],
  );

  return { runtime, addToBoard, addingId, dismissCard, dismissingCardId };
}
