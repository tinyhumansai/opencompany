import { useEffect, useState } from "react";

import type { TurnStep } from "@/api/types";
import { cn } from "@/lib/utils";
import { TeammateAvatar } from "@/components/teammate-avatar";
import { StepTimeline } from "./StepTimeline";
import { runningStepLabel } from "./WorkingIndicator";
import type { Channel } from "./model";

/**
 * The live receipt for a chat instruction the operator just sent (issue #1934).
 *
 * Between hitting send and the reply landing there used to be a dead gap — the
 * composer cleared, and nothing said the turn had been taken until the whole
 * answer arrived, which for a long turn is many silent seconds. This is the row
 * that fills that gap: **Sent** the instant the POST is armed, **Picked up by
 * <teammate>** once the first live frame names who answered, **On step <label>**
 * while a step is in flight — each with a ticking elapsed readout so the wait is
 * legible rather than a frozen spinner.
 *
 * It is deliberately reversible. If no frame arrives (or advances) for
 * {@link RECEIPT_STALL_AFTER_MS}, it adds a soft "still waiting" note — not an
 * error, not a terminal state. Any new frame, or the reply itself, clears it.
 * The receipt clears the moment the real reply bubble lands (`AppShell`'s
 * `onSendEnd`), so there is never a frame where both the reply and the receipt
 * are absent.
 *
 * The state line reuses {@link runningStepLabel} — the same source
 * `WorkingIndicator` derives its line from — so the two surfaces never phrase
 * the same step differently (the #264 drift rule). The teammate is always shown
 * by name; a raw agent id is never rendered.
 */
export const RECEIPT_STALL_AFTER_MS = 30_000;

/**
 * What `AppShell` tracks per thread for the duration of a synchronous chat
 * turn. `startedAt` fixes the elapsed clock; `lastFrameAt` seeds to `startedAt`
 * and bumps on every live frame, so the stall check is "no frame for 30s"
 * rather than "no reply for 30s". `agentId` is captured off the first frame
 * that names one and never rendered raw — it is resolved to a display name.
 *
 * `gen` is the generation the send that armed this receipt was stamped with
 * (issue #1935 review). Host thread ids like `main` recur across companies,
 * so without it a slow POST from a company the operator has since left can
 * land after a *new* send has re-armed the same thread id and delete that
 * newer receipt out from under the company actually on screen. See
 * {@link shouldClearReceipt}.
 */
export interface ChatReceipt {
  startedAt: number;
  lastFrameAt: number;
  agentId?: string;
  gen?: number;
}

/**
 * Whether a clear request for a thread's receipt should actually delete it.
 *
 * The bug this guards (issue #1935 review, codex 3892523790 / coderabbit
 * 3892517512, and its sibling codex 3892702774): thread ids are reused across
 * companies (`main` above all), and `AppShell`'s
 * `onSendStale`/`onSendEnd`/`onSendDetached`/`onSendFailed` all clear a
 * receipt by thread id alone. Send it from company A, switch to company B,
 * send again on the same thread id — B's send arms a *new* receipt — and when
 * A's slow POST finally settles, its own clear call must not delete B's
 * receipt just because they share a thread id.
 *
 * Each armed receipt is stamped with the generation counter value current at
 * arm time; each terminal callback is handed the generation its own
 * `onSendStart` call returned. A clear only proceeds when the receipt
 * currently on file carries that same generation — if a newer send has
 * re-armed the slot in between, the generations differ and the clear is a
 * no-op, leaving the newer receipt alone.
 *
 * `gen === undefined` now REFUSES to clear (issue #1935 review, codex
 * 3892702774 — reversing the original "clears unconditionally" reading of
 * this branch). That original reading treated an omitted generation as
 * `Conversation`'s calling convention, on the assumption the parked
 * conversation view "has no scope to switch out from under" — which was
 * wrong: `#/conversation` is a live, still-routable surface (`ROUTABLE.
 * conversation` in `console-routes.ts`), it stays mounted across a company
 * switch exactly like `ChatView`, and its own `useConversationRuntime` send
 * never generation-tagged its calls — so a slow Conversation POST from
 * company A could delete a newer `ChatView` (or Conversation) receipt on
 * company B through the identical reused-thread-id race, just missing the
 * `shouldClearReceipt` guard that was supposed to close it everywhere.
 *
 * `Conversation`'s `useConversationRuntime` is now generation-tagged too (see
 * `runtime.ts`), so every current caller always supplies a defined `gen` and
 * this branch is not load-bearing for them. It stays fail-closed rather than
 * being deleted so a FUTURE send surface that forgets to capture and forward
 * `onSendStart`'s return value cannot reintroduce this exact leak by omission
 * — the failure mode of forgetting becomes a receipt that lingers until the
 * next send or company switch clears it, never a live receipt deleted out
 * from under a different company.
 */
export function shouldClearReceipt(
  current: Pick<ChatReceipt, "gen"> | undefined,
  gen: number | undefined,
): boolean {
  if (!current) return false;
  if (gen === undefined) return false;
  return current.gen === gen;
}

/**
 * Elapsed since a turn was sent — `Ns` under a minute, `m:ss` beyond it. Kept a
 * pure function so the format is unit-testable without a clock.
 */
export function formatElapsed(ms: number): string {
  const totalSeconds = Math.max(0, Math.floor(ms / 1000));
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return minutes > 0 ? `${minutes}:${String(seconds).padStart(2, "0")}` : `${seconds}s`;
}

/**
 * The teammate on the other end of this receipt, by name — never a raw id.
 *
 * Resolves the captured `agentId` against the roster's name map, falling back
 * to the channel's own voice and then a neutral "a teammate" so an unresolved
 * id is still shown as a person rather than an opaque token. `undefined` only
 * when no frame has named an agent yet, which is the "Sent" state.
 */
export function resolveReceiptAgentName(
  receipt: ChatReceipt,
  agentNames: Record<string, string> | undefined,
  channel: Channel,
): string | undefined {
  if (!receipt.agentId) return undefined;
  return agentNames?.[receipt.agentId] ?? channel.voice ?? "a teammate";
}

/**
 * The one visible state line, progressing Sent → Picked up by <name> → On step
 * <label>. A running step is the most specific thing to say, so it outranks the
 * name; the name outranks the bare "Sent". Pure, for the same reason
 * {@link formatElapsed} is.
 */
export function receiptStateLine(
  receipt: ChatReceipt,
  steps: readonly TurnStep[] | undefined,
  agentNames: Record<string, string> | undefined,
  channel: Channel,
): string {
  const step = runningStepLabel(steps);
  if (step) return `On step ${step}`;
  const name = resolveReceiptAgentName(receipt, agentNames, channel);
  if (name) return `Picked up by ${name}`;
  return "Sent";
}

export function ChatLiveReceipt({
  channel,
  receipt,
  agentNames,
  steps,
}: {
  channel: Channel;
  receipt: ChatReceipt;
  /** Roster agent id → display name, so the receipt never shows a raw id. */
  agentNames?: Record<string, string>;
  /** The turn's live steps, when any have arrived — folded below the line. */
  steps: TurnStep[];
}) {
  const reduced = usePrefersReducedMotion();
  // Self-contained 1s clock, mounted only while this row is (the receipt is
  // present). `feed.now` is too coarse for a seconds readout, so this owns its
  // own interval and tears it down on unmount.
  const clock = useReceiptClock();
  const elapsed = Math.max(0, clock - receipt.startedAt);
  // Soft and reversible: a lull with no frame, seeded from `startedAt`, cleared
  // by the next frame that bumps `lastFrameAt`. Not an error state.
  const stalled = clock - receipt.lastFrameAt >= RECEIPT_STALL_AFTER_MS;
  const line = receiptStateLine(receipt, steps, agentNames, channel);

  return (
    <div className="flex items-start gap-2.5 px-4 py-1">
      <TeammateAvatar
        name={channel.voice ?? channel.name}
        tone={channel.tone}
        avatar={channel.member?.avatar}
        company={channel.kind === "channel" && channel.id === "main"}
        className="size-9 shrink-0"
      />
      <div className="min-w-0 flex-1 space-y-1.5">
        <span
          data-testid="chat-live-receipt"
          data-stalled={stalled ? "true" : "false"}
          className="flex w-fit items-center gap-2 rounded-full bg-muted px-3 py-2 text-sm text-muted-foreground"
        >
          <span
            aria-hidden
            className={cn(
              "size-1.5 shrink-0 rounded-full bg-status-running",
              // The pulse is the "something is happening" signal; a reader who
              // asked for stillness keeps the mark without the motion.
              !reduced && "animate-pulse",
            )}
          />
          {/* `aria-hidden`, because the stable assistive line below is what a
              screen reader should read — the visible line changes as the turn
              advances, and re-announcing every transition is noise. */}
          <span aria-hidden className="truncate">
            {line}
          </span>
          <span aria-hidden className="shrink-0 tabular-nums text-2xs text-muted-foreground/80">
            {formatElapsed(elapsed)}
          </span>
          <span className="sr-only">Waiting for a reply…</span>
        </span>
        {stalled && (
          <p role="status" className="px-1 text-2xs text-muted-foreground">
            No update for 30s… still waiting.
          </p>
        )}
        {/* Kept below the line when steps exist; renders nothing otherwise. */}
        <StepTimeline steps={steps} defaultOpen />
      </div>
    </div>
  );
}

/** A 1-second clock that lives exactly as long as the row it drives. */
function useReceiptClock(): number {
  const [clock, setClock] = useState(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => setClock(Date.now()), 1000);
    return () => clearInterval(id);
  }, []);
  return clock;
}

/**
 * Whether the viewer asked for reduced motion, kept live. Mirrors
 * `WorkingIndicator`'s hook — reads `false` where `matchMedia` is unavailable,
 * and prefers the modern `addEventListener` spelling with the deprecated
 * `addListener` as the fallback older WebKitGTK builds still need.
 */
function usePrefersReducedMotion(): boolean {
  const [reduced, setReduced] = useState(false);
  useEffect(() => {
    const mql = window.matchMedia?.("(prefers-reduced-motion: reduce)");
    if (!mql) return;
    setReduced(mql.matches);
    const onChange = () => setReduced(mql.matches);
    if (typeof mql.addEventListener === "function") {
      mql.addEventListener("change", onChange);
      return () => mql.removeEventListener("change", onChange);
    }
    mql.addListener(onChange);
    return () => mql.removeListener(onChange);
  }, []);
  return reduced;
}
