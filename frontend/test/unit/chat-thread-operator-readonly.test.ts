// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { TeamMember } from "@/lib/team";
import { ThreadPanel } from "@/views/chat/ThreadPanel";
import { operatorChannelFrom } from "@/views/chat/model";

/**
 * Issue #1757 follow-up (codex + CodeRabbit review on the Operator channel
 * PR). The main composer already disables on `readOnly` — `Boolean(channel?.
 * system)` — but `ThreadPanel` renders its own composer and used to disable
 * it only while `sending`. Opening a durable Operator report as a thread and
 * replying there reached `onSend` (and would have reached `client.chat`)
 * before the server's read-only guard finally refused it, after the operator
 * had already written and submitted the reply.
 *
 * These pin that a read-only thread disables the composer the same way the
 * main one does, and that neither a click nor Enter reaches `onSend` at all —
 * not just that the request would eventually be rejected server-side.
 *
 * Issue #1757 rework: the Operator channel is its own surface now (`GET
 * {scope}/operator-channel`), not an entry `list_desks` returns, so the
 * fixture channel is built through `operatorChannelFrom` — the same
 * projection `ChatView` uses — rather than a hand-rolled literal.
 */

const CHANNEL = operatorChannelFrom({
  id: "operator",
  name: "Operator",
  description: "Workflow reports and notifications",
});

const MEMBERS: TeamMember[] = [];

let container: HTMLDivElement;
let root: Root;
let sent: ReturnType<typeof vi.fn>;

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  sent = vi.fn();
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

async function render(readOnly: boolean | undefined) {
  await act(async () => {
    root.render(
      createElement(ThreadPanel, {
        channel: CHANNEL,
        members: MEMBERS,
        parent: { id: "p", from: "company", text: "nightly report", at: 0 },
        replies: [],
        sending: false,
        readOnly,
        onSend: sent,
        onClose: vi.fn(),
      }),
    );
  });
}

function textarea() {
  return container.querySelector("textarea") as HTMLTextAreaElement;
}

function sendButton() {
  return container.querySelector('[aria-label="Send"]') as HTMLButtonElement;
}

async function type(text: string) {
  const el = textarea();
  const setValue = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")?.set;
  await act(async () => {
    setValue?.call(el, text);
    el.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

describe("thread composer on a read-only channel (issue #1757)", () => {
  it("disables the thread composer for a read-only channel", async () => {
    await render(true);
    await type("can I help?");

    expect(textarea().placeholder).toBe("This channel is read-only");
    expect(sendButton().disabled).toBe(true);
  });

  it("never calls onSend from a click on a read-only thread", async () => {
    await render(true);
    await type("can I help?");
    await act(async () => sendButton().click());

    expect(sent).not.toHaveBeenCalled();
  });

  it("never calls onSend from Enter on a read-only thread", async () => {
    await render(true);
    await type("can I help?");
    await act(async () => {
      textarea().dispatchEvent(
        new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }),
      );
    });

    expect(sent).not.toHaveBeenCalled();
  });

  it("keeps the thread composer working on an ordinary channel", async () => {
    await render(false);
    await type("on it");

    expect(textarea().placeholder).toBe("Reply…");
    expect(sendButton().disabled).toBe(false);

    await act(async () => sendButton().click());
    expect(sent).toHaveBeenCalledTimes(1);
    expect(sent).toHaveBeenLastCalledWith("on it", undefined, undefined, undefined);
  });

  it("defaults to the writable behaviour when readOnly is omitted", async () => {
    await render(undefined);
    await type("on it");
    expect(textarea().placeholder).toBe("Reply…");
    expect(sendButton().disabled).toBe(false);
  });
});
