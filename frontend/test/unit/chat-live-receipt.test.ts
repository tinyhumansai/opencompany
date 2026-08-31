// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { TurnStep } from "@/api/types";
import {
  ChatLiveReceipt,
  formatElapsed,
  shouldClearReceipt,
  RECEIPT_STALL_AFTER_MS,
  type ChatReceipt,
} from "@/views/chat/ChatLiveReceipt";
import type { Channel } from "@/views/chat/model";

/**
 * Coverage for the chat live receipt (issue #1934) — the row that fills the gap
 * between a sent instruction and its reply. The unit runner has no
 * testing-library, so this renders through `react-dom/client` directly (the
 * same shape `use-typing.test.ts` and `inference-model-picker.test.ts` use) and
 * drives its self-contained 1s clock with fake timers.
 */

const BASE = 1_700_000_000_000;

function channel(overrides: Partial<Channel> = {}): Channel {
  return {
    id: "engineering",
    name: "engineering",
    voice: "Engineering",
    kind: "channel",
    purpose: "",
    tone: "sky",
    ...overrides,
  };
}

function runningStep(label: string): TurnStep {
  return { kind: "tool_call", status: "running", label };
}

let container: HTMLDivElement;
let root: Root;

async function render(props: {
  channel?: Channel;
  receipt: ChatReceipt;
  agentNames?: Record<string, string>;
  steps?: TurnStep[];
}) {
  await act(async () => {
    root.render(
      createElement(ChatLiveReceipt, {
        channel: props.channel ?? channel(),
        receipt: props.receipt,
        agentNames: props.agentNames,
        steps: props.steps ?? [],
      }),
    );
  });
}

function text(): string {
  return container.textContent ?? "";
}

function receiptEl(): HTMLElement {
  const el = container.querySelector<HTMLElement>('[data-testid="chat-live-receipt"]');
  if (!el) throw new Error("receipt row not rendered");
  return el;
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  vi.useFakeTimers();
  vi.setSystemTime(BASE);
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(async () => {
  await act(async () => {
    root.unmount();
  });
  container.remove();
  vi.useRealTimers();
});

describe("ChatLiveReceipt", () => {
  it("shows Sent with an elapsed readout when no agent and no steps", async () => {
    await render({ receipt: { startedAt: BASE - 5_000, lastFrameAt: BASE - 5_000 } });
    expect(text()).toContain("Sent");
    expect(text()).toContain("5s");
    expect(text()).not.toContain("Picked up by");
  });

  it("names the teammate once an agent id resolves", async () => {
    await render({
      receipt: { startedAt: BASE, lastFrameAt: BASE, agentId: "a-ada" },
      agentNames: { "a-ada": "Ada" },
    });
    expect(text()).toContain("Picked up by Ada");
    // A raw agent id is never rendered.
    expect(text()).not.toContain("a-ada");
  });

  it("falls back to the channel voice for an unresolvable agent id", async () => {
    await render({
      channel: channel({ voice: "Front desk" }),
      receipt: { startedAt: BASE, lastFrameAt: BASE, agentId: "a-ghost" },
      agentNames: {},
    });
    expect(text()).toContain("Picked up by Front desk");
    expect(text()).not.toContain("a-ghost");
  });

  it("shows the running step label when a step is in flight", async () => {
    await render({
      receipt: { startedAt: BASE, lastFrameAt: BASE, agentId: "a-ada" },
      agentNames: { "a-ada": "Ada" },
      steps: [runningStep("Searching the web")],
    });
    expect(text()).toContain("On step Searching the web");
  });

  it("goes stalled after 30s with no frame, and a fresh frame clears it", async () => {
    const receipt: ChatReceipt = { startedAt: BASE, lastFrameAt: BASE };
    await render({ receipt });
    expect(receiptEl().dataset.stalled).toBe("false");

    // Push the self-contained clock past the 30s stall window with no new frame.
    await act(async () => {
      vi.advanceTimersByTime(31_000);
    });
    expect(receiptEl().dataset.stalled).toBe("true");
    expect(text()).toContain("No update for 30s");

    // A fresh frame bumps lastFrameAt (a new receipt object from the shell) —
    // the stall is soft and reversible, so it clears without a remount.
    await render({ receipt: { startedAt: BASE, lastFrameAt: BASE + 31_000 } });
    expect(receiptEl().dataset.stalled).toBe("false");
    expect(text()).not.toContain("No update for 30s");
  });

  it("is already stalled at exactly the 30s threshold, not a tick after (issue #1935 review)", async () => {
    // coderabbit 3892517524: `clock - lastFrameAt > RECEIPT_STALL_AFTER_MS`
    // reads exactly 30,000ms as "not yet stalled", so the notice appeared
    // about a second late. `>=` is the fix under test.
    const receipt: ChatReceipt = { startedAt: BASE, lastFrameAt: BASE };
    await render({ receipt });

    await act(async () => {
      vi.advanceTimersByTime(RECEIPT_STALL_AFTER_MS);
    });
    expect(receiptEl().dataset.stalled).toBe("true");
    expect(text()).toContain("No update for 30s");
  });

  it("advances the elapsed readout as its own clock ticks", async () => {
    await render({ receipt: { startedAt: BASE, lastFrameAt: BASE } });
    expect(text()).toContain("0s");
    await act(async () => {
      vi.advanceTimersByTime(3_000);
    });
    expect(text()).toContain("3s");
  });
});

describe("formatElapsed", () => {
  it("renders sub-minute durations as seconds", () => {
    expect(formatElapsed(0)).toBe("0s");
    expect(formatElapsed(5_400)).toBe("5s");
    expect(formatElapsed(59_999)).toBe("59s");
  });

  it("rolls into m:ss at a minute and pads the seconds", () => {
    expect(formatElapsed(60_000)).toBe("1:00");
    expect(formatElapsed(65_000)).toBe("1:05");
    expect(formatElapsed(605_000)).toBe("10:05");
  });

  it("floors a negative elapsed to zero", () => {
    expect(formatElapsed(-1_000)).toBe("0s");
  });
});

describe("shouldClearReceipt", () => {
  it("refuses to clear when the request carries no generation (fail-safe)", () => {
    // issue #1935 review, codex 3892702774 — reversed from the original
    // "clears unconditionally" reading of an omitted generation. That
    // reading assumed `Conversation` never races a company switch; it does
    // (see the cross-surface test below), and treating "no generation" as
    // "clear anyway" is exactly the loophole that let it. A caller with no
    // generation cannot prove which send armed the receipt it is trying to
    // clear, so the safe default is to leave it alone — the worst a future
    // caller that forgets to thread `onSendStart`'s return value can do is
    // leak a receipt, never delete a different company's live one.
    expect(shouldClearReceipt({ gen: 7 }, undefined)).toBe(false);
    expect(shouldClearReceipt(undefined, undefined)).toBe(false);
  });

  it("clears a matching generation — the ordinary same-scope resolve", () => {
    expect(shouldClearReceipt({ gen: 3 }, 3)).toBe(true);
  });

  it("does not delete a company B receipt when company A's stale send settles late", () => {
    // issue #1935 review — codex 3892523790 / coderabbit 3892517512. Host
    // thread ids like `main` recur across companies, so this is not a
    // hypothetical mismatch: the same thread id genuinely holds two
    // different companies' receipts in sequence within one browser tab.
    //
    // Sequence: operator sends on company A's "main" thread (gen 1 armed).
    // Before A's POST resolves, the operator switches to company B and
    // sends on B's own "main" thread too — same thread id, new generation
    // (gen 2) re-arms the slot. A's slow POST finally settles and its
    // `onSendStale` fires with the generation IT was armed with (1), not
    // whatever is currently on file.
    const armedForCompanyA = 1;
    const armedForCompanyB = 2;
    const currentReceipt: ChatReceipt = {
      startedAt: 2_000,
      lastFrameAt: 2_000,
      gen: armedForCompanyB,
    };

    // Company A's late completion must not win against company B's receipt.
    expect(shouldClearReceipt(currentReceipt, armedForCompanyA)).toBe(false);

    // Company B's own completion, naming its own generation, still may.
    expect(shouldClearReceipt(currentReceipt, armedForCompanyB)).toBe(true);
  });

  it("does not delete a ChatView receipt when a Conversation send from the old company settles late", () => {
    // issue #1935 review, codex 3892702774 — the sibling of the test above,
    // through the OTHER door. `#/conversation` is a second, still-routable
    // send surface (`ROUTABLE.conversation` in console-routes.ts) that stays
    // mounted across a company switch exactly like `ChatView` does, and
    // shares the same `receiptByThread` map in `AppShell`.
    //
    // Sequence: operator is on `#/conversation` for company A and sends on
    // the "main" thread (gen 1 armed, captured from `onSendStart`'s return).
    // Before that POST resolves, the operator switches to company B and
    // switches to `#/chat`, sending on B's own "main" thread (gen 2 re-arms
    // the slot). Conversation's slow POST for company A finally settles and
    // its `onSendEnd` fires with the generation IT captured (1) — not
    // whatever is currently on file — exactly like `ChatView`'s own case,
    // just originating from `useConversationRuntime` instead of `ChatView.send`.
    const armedByConversationForCompanyA = 1;
    const armedByChatViewForCompanyB = 2;
    const currentReceipt: ChatReceipt = {
      startedAt: 5_000,
      lastFrameAt: 5_000,
      gen: armedByChatViewForCompanyB,
    };

    // Conversation's late company-A completion must not win against
    // ChatView's newer company-B receipt on the same reused thread id.
    expect(shouldClearReceipt(currentReceipt, armedByConversationForCompanyA)).toBe(false);

    // ChatView's own completion, naming its own generation, still may.
    expect(shouldClearReceipt(currentReceipt, armedByChatViewForCompanyB)).toBe(true);
  });

  it("is a no-op (not a throw) when nothing is armed for the thread at all", () => {
    // A stray terminal callback for a thread whose receipt was already
    // cleared (or never armed, e.g. a background frame) must not attempt to
    // delete a key that is not there.
    expect(shouldClearReceipt(undefined, 5)).toBe(false);
  });
});
