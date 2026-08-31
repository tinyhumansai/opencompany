// The transcript and composer, rendered on assistant-ui primitives.
//
// assistant-ui owns the mechanics — the scroll viewport and its auto-scroll,
// the composer's state, Enter/Shift+Enter, autosize and focus, and the run
// state that disables sending mid-turn. This file owns what a line *looks*
// like, which stays exactly what it was: WhatsApp-style grouped bubbles with
// the company's own step timelines and board-card affordances.
//
// Structurally that means one render function rather than a components map:
// `ThreadPrimitive.Messages` hands it each message in turn, and the decorated
// {@link ConversationLine} riding in `metadata.custom` says what to draw.

import { useCallback, type ReactNode } from "react";
import { ArrowDown, ArrowUp, TriangleAlert } from "lucide-react";
import { AuiIf, ComposerPrimitive, ThreadPrimitive } from "@assistant-ui/react";

import type { TurnStep } from "@/api/types";
import { Button } from "@/components/ui/button";
import type { ChatMessage } from "@/lib/chat";
import type { OpenTurn } from "@/lib/live-reply";
import type { ThreadContact } from "@/lib/threads";
import { cn } from "@/lib/utils";
import { lineOf, type ConversationLine } from "./model";
import {
  AddToBoardAction,
  Bubble,
  DaySeparator,
  EmptyConversation,
  SenderAvatar,
  StepTimeline,
  TypingIndicator,
} from "./parts";

interface Props {
  contact: ThreadContact;
  /** Turns one transcript message into a board card (issue #246). */
  onAddToBoard: (message: ChatMessage) => void;
  addingId: string | null;
  /** Deletes the card a line opened and drops its chip (issue #984). */
  onDismissCard: (taskId: string) => void;
  dismissingCardId: string | null;
  /** A chat POST is in flight from this view. */
  sending: boolean;
  /** This thread's turn, when one is accepted but not settled (#983). */
  openTurn?: OpenTurn;
  /** The live in-flight tool timeline, built from transient SSE frames. */
  liveSteps?: TurnStep[];
  /** The in-flight steer strip, rendered between transcript and composer. */
  footer?: ReactNode;
  /**
   * The active thread is the durable Operator system channel (issue #1757):
   * a read-only feed the server refuses to post to. Disables the composer
   * the same way `ChatView`'s does, rather than letting the operator type
   * and submit before the server's read-only guard finally refuses it.
   */
  readOnly?: boolean;
}

export function Transcript({
  contact,
  onAddToBoard,
  addingId,
  onDismissCard,
  dismissingCardId,
  sending,
  openTurn,
  liveSteps,
  footer,
  readOnly,
}: Props) {
  const working = sending || !!openTurn;

  const renderMessage = useCallback(
    ({ message }: { message: { metadata?: { custom?: Record<string, unknown> } } }) => {
      const line = lineOf(message.metadata);
      // The empty assistant placeholder the external-store runtime inserts
      // while a run is open. This surface draws its own working row below —
      // one that survives a trailing assistant message, which the placeholder
      // does not, and which is what a detached turn (#983) needs — so drawing
      // the placeholder too would be a second indicator.
      if (!line) return null;
      return (
        <MessageLine
          line={line}
          onAddToBoard={onAddToBoard}
          addingId={addingId}
          onDismissCard={onDismissCard}
          dismissingCardId={dismissingCardId}
        />
      );
    },
    [addingId, dismissingCardId, onAddToBoard, onDismissCard],
  );

  return (
    <ThreadPrimitive.Root className="flex min-h-0 flex-1 flex-col overflow-hidden">
      <ThreadPrimitive.Viewport
        className="relative flex-1 overflow-y-auto"
        style={{
          backgroundImage:
            "radial-gradient(color-mix(in oklab, var(--muted-foreground) 9%, transparent) 1px, transparent 1px)",
          backgroundSize: "22px 22px",
        }}
      >
        <div className="mx-auto flex w-full max-w-3xl flex-col gap-1.5 px-4 py-6">
          {/* `AuiIf`, not the deprecated `ThreadPrimitive.Empty`. */}
          <AuiIf condition={(s) => s.thread.isEmpty}>
            <EmptyConversation contact={contact} />
          </AuiIf>
          <ThreadPrimitive.Messages>{renderMessage}</ThreadPrimitive.Messages>
          {working && (
            <>
              {/* Live tool timeline — the running/done rows stream in over SSE as
                  the turn works, before the final reply lands (issue: tool calls
                  weren't visible until the turn finished). */}
              {liveSteps && liveSteps.length > 0 && <StepTimeline steps={liveSteps} />}
              <TypingIndicator contact={contact} queued={openTurn?.queued} />
            </>
          )}
        </div>
        {/* Only drawn once the operator has scrolled away from the bottom, so
            a transcript being read from the top is not covered by a control
            for a place it is already at. */}
        <ThreadPrimitive.ScrollToBottom
          render={
            <Button
              variant="secondary"
              size="icon"
              className="sticky bottom-3 left-1/2 z-10 size-8 -translate-x-1/2 rounded-full shadow-md disabled:invisible"
              aria-label="Scroll to the latest message"
            >
              <ArrowDown className="size-4" />
            </Button>
          }
        />
      </ThreadPrimitive.Viewport>

      {footer}

      {readOnly && (
        <p
          role="status"
          className="flex shrink-0 items-center gap-1.5 border-t bg-muted/50 px-3 py-1.5 text-xs text-muted-foreground"
        >
          <TriangleAlert className="size-3.5 shrink-0" aria-hidden />
          <span className="min-w-0">
            The <span className="font-medium text-foreground">Operator</span> channel is a
            read-only feed of workflow reports and notifications — a scannable “what happened”
            view. There is nothing to reply to here.
          </span>
        </p>
      )}

      <div className="border-t bg-background/80 backdrop-blur">
        <div className="mx-auto w-full max-w-3xl px-4 py-3">
          <ComposerPrimitive.Root
            data-tour="chat-composer"
            className="relative flex items-end gap-2 rounded-xl border bg-card p-2 shadow-sm focus-within:ring-2 focus-within:ring-ring/50"
          >
            <ComposerPrimitive.Input
              placeholder={readOnly ? "This channel is read-only" : `Message ${contact.name}…`}
              rows={1}
              disabled={readOnly}
              className={cn(
                "max-h-40 min-h-9 flex-1 resize-none border-0 bg-transparent px-2 py-1.5 text-sm shadow-none outline-none",
                "placeholder:text-muted-foreground disabled:cursor-not-allowed disabled:opacity-50",
              )}
            />
            <ComposerPrimitive.Send
              render={
                // `disabled` is spread in only when `readOnly` — the Radix Slot
                // merge this `render` prop goes through lets an explicitly-set
                // child prop win over the primitive's own computed `disabled`
                // (empty composer / thread running), so an unconditional
                // `disabled={readOnly}` would pin the button permanently
                // enabled on every ordinary thread, where `readOnly` is
                // `undefined`/`false`. Omitting the key when not read-only
                // leaves the primitive's own disable logic in charge.
                <Button
                  size="icon"
                  className="size-9 shrink-0 rounded-lg"
                  aria-label="Send"
                  {...(readOnly ? { disabled: true } : {})}
                >
                  <ArrowUp className="size-4" />
                </Button>
              }
            />
          </ComposerPrimitive.Root>
          <p className="mt-1.5 px-1 text-center text-xs text-muted-foreground">
            Enter to send · Shift+Enter for a new line
          </p>
        </div>
      </div>
    </ThreadPrimitive.Root>
  );
}

function MessageLine({
  line,
  onAddToBoard,
  addingId,
  onDismissCard,
  dismissingCardId,
}: {
  line: ConversationLine;
  onAddToBoard: (message: ChatMessage) => void;
  addingId: string | null;
  onDismissCard: (taskId: string) => void;
  dismissingCardId: string | null;
}) {
  const { message, sender, showDay, groupHead, groupTail } = line;

  if (sender.kind === "system") {
    return (
      <>
        {showDay && <DaySeparator at={message.at} />}
        <div className="my-1 flex flex-col items-center gap-1">
          <div className="rounded-full bg-muted px-3 py-1 text-center text-xs text-muted-foreground">
            {message.text}
          </div>
        </div>
      </>
    );
  }

  const mine = sender.kind === "you";
  return (
    <>
      {showDay && <DaySeparator at={message.at} />}
      <div
        className={cn(
          "flex gap-2.5",
          groupHead && "mt-2",
          mine ? "flex-row-reverse" : "flex-row",
        )}
      >
        {/* The avatar column is held open for every line of a group, so the
            bubbles under the head stay aligned with it rather than sliding
            back to the gutter. */}
        {!mine && (groupHead ? <SenderAvatar sender={sender} /> : <div className="w-8 shrink-0" />)}
        <div className={cn("flex min-w-0 flex-1 flex-col gap-1", mine ? "items-end" : "items-start")}>
          {!mine && groupHead && (
            <div className="px-1">
              <span className="text-xs font-semibold">{sender.name}</span>
            </div>
          )}
          {!mine && message.steps && message.steps.length > 0 && <StepTimeline steps={message.steps} />}
          {/* The bubble and its hover action share a row so the action can
              sit outside the bubble without overlapping the text. `group`
              scopes the reveal to this one message. */}
          <div
            className={cn(
              "group/msg flex max-w-full items-center gap-1",
              mine ? "flex-row-reverse" : "flex-row",
            )}
          >
            <Bubble
              message={message}
              mine={mine}
              last={groupTail}
              onDismissCard={onDismissCard}
              dismissingCardId={dismissingCardId}
            />
            <AddToBoardAction
              message={message}
              busy={addingId === message.id}
              disabled={addingId !== null && addingId !== message.id}
              onAdd={onAddToBoard}
            />
          </div>
        </div>
      </div>
    </>
  );
}
