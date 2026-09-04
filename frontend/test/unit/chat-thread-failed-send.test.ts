// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { ChatMessage } from "@/lib/chat";
import type { TeamMember } from "@/lib/team";
import { ThreadPanel } from "@/views/chat/ThreadPanel";
import type { Channel } from "@/views/chat/model";

/**
 * B-099 follow-up (Codex review, PR #2052): a reply that failed to send from
 * the *thread* composer must not read as delivered.
 *
 * `MessageRow` grew `sendFailed` styling and a Retry control for the main
 * timeline, but `ThreadPanel` draws replies with its own `Line` renderer,
 * which never read `sendFailed` or received `onRetrySend` at all — so the
 * previous sibling `system` line's removal (this same PR) left a threaded
 * failure completely invisible: no dimming, no "Not sent" notice, no way to
 * send it again.
 */

const CHANNEL: Channel = {
  id: "engineering",
  name: "engineering",
  voice: "Engineering",
  kind: "channel",
  purpose: "",
};

const MEMBERS: TeamMember[] = [];

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.restoreAllMocks();
});

async function render(over: Partial<Parameters<typeof ThreadPanel>[0]> = {}) {
  await act(async () => {
    root.render(
      createElement(ThreadPanel, {
        channel: CHANNEL,
        members: MEMBERS,
        parent: { id: "p", from: "you", text: "root", at: 0 },
        replies: [],
        sending: false,
        onSend: vi.fn(),
        onClose: vi.fn(),
        ...over,
      }),
    );
  });
}

/** The Retry control, however it is currently laid out. */
function retryButton(): HTMLButtonElement | null {
  return (
    Array.from(container.querySelectorAll("button")).find(
      (b) => (b.textContent ?? "").trim() === "Retry",
    ) ?? null
  );
}

describe("a failed threaded reply", () => {
  const failedReply: ChatMessage = {
    id: "r",
    parentId: "p",
    from: "you",
    text: "will this land",
    at: 1,
    sendFailed: "cannot reach the company host at this origin",
  };

  it("says it was not sent, and offers Retry", async () => {
    const onRetrySend = vi.fn();
    await render({ replies: [failedReply], onRetrySend });

    const text = container.textContent ?? "";
    expect(text).toContain("Not sent");
    expect(text).toContain("cannot reach the company host at this origin");

    const retry = retryButton();
    expect(retry, "a failed threaded reply must offer a way to send it again").not.toBeNull();
    await act(async () => {
      retry!.click();
    });
    expect(onRetrySend).toHaveBeenCalledWith("r");
  });

  /** The negative half, pinning the bug: a delivered reply carries none of this. */
  it("is distinguishable from a delivered reply", async () => {
    await render({ replies: [{ ...failedReply, sendFailed: undefined }] });
    expect(container.textContent ?? "").not.toContain("Not sent");
    expect(retryButton()).toBeNull();
  });

  it("still reports the failure where no resend is wired", async () => {
    await render({ replies: [failedReply], onRetrySend: undefined });
    expect(container.textContent ?? "").toContain("Not sent");
    expect(retryButton()).toBeNull();
  });

  /** CodeRabbit review: an empty ApiError message must still count as failed. */
  it("renders the failed state for an empty failure reason", async () => {
    await render({ replies: [{ ...failedReply, sendFailed: "" }] });
    const text = container.textContent ?? "";
    expect(text).toContain("Not sent");
    expect(text).toContain("something went wrong");
  });
});
