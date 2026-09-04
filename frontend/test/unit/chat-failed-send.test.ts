// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { makeMessage, markSendFailed, type ChatMessage } from "@/lib/chat";
import { MessageRow } from "@/views/chat/MessageRow";
import type { TimelineEntry } from "@/views/chat/model";

/**
 * B-099: a message that failed to send must not render as a delivered one.
 *
 * What was wrong was not the wording, it was where the fact lived. The failure
 * was appended as its own `system` bubble underneath, so the message itself
 * kept the styling of the delivered lines above it — same avatar, same
 * timestamp, nothing red — and the note scrolled away from it. A long enough
 * message pushed the note off screen entirely, leaving something that reads as
 * sent and was not, with the composer already cleared so the only copy of the
 * text was the pixels on screen.
 *
 * `sendFailed` is a field of the message now, so no renderer can draw the
 * bubble without seeing it, and the row carries its own Retry.
 */

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

function entryFor(message: ChatMessage): TimelineEntry {
  return {
    key: message.id,
    message,
    sender: { kind: "you", name: "You" },
    continuation: false,
    replies: 0,
  } as unknown as TimelineEntry;
}

async function render(message: ChatMessage, onRetrySend?: (id: string) => void) {
  await act(async () => {
    root.render(
      createElement(MessageRow, {
        entry: entryFor(message),
        threadOpen: false,
        onOpenThread: () => {},
        onReact: () => {},
        onDismissCard: () => {},
        dismissingCardId: null,
        onRetrySend,
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

describe("markSendFailed", () => {
  it("marks only the line it names", () => {
    const kept = makeMessage("you", "first");
    const failed = makeMessage("you", "second");
    const marked = markSendFailed([kept, failed], failed.id, "cannot reach the company host");

    expect(marked.find((m) => m.id === failed.id)?.sendFailed).toBe(
      "cannot reach the company host",
    );
    expect(marked.find((m) => m.id === kept.id)?.sendFailed).toBeUndefined();
  });

  /**
   * A send's target can be re-homed by a company switch while its POST is in
   * flight, and the transcript it aimed at is then somebody else's. Returning
   * the same reference makes the caller's `setState` a no-op rather than a
   * re-render that changes nothing.
   */
  it("returns the same array when the line is no longer in this transcript", () => {
    const messages = [makeMessage("you", "still here")];
    expect(markSendFailed(messages, "m-gone", "whatever")).toBe(messages);
  });
});

describe("a failed message's row", () => {
  it("says it was not sent, and offers Retry", async () => {
    const message: ChatMessage = {
      ...makeMessage("you", "Please pull last month's sales by scent."),
      sendFailed: "cannot reach the company host at this origin",
    };
    const onRetrySend = vi.fn();
    await render(message, onRetrySend);

    const text = container.textContent ?? "";
    expect(text).toContain("Not sent");
    expect(text).toContain("cannot reach the company host at this origin");

    const retry = retryButton();
    expect(retry, "a failed message must offer a way to send it again").not.toBeNull();
    await act(async () => {
      retry!.click();
    });
    expect(onRetrySend).toHaveBeenCalledWith(message.id);
  });

  /**
   * The negative half, and the one that actually pins the bug: a delivered
   * message must carry none of this. Without it the assertions above pass for a
   * renderer that decorates every row.
   */
  it("is distinguishable from a delivered one", async () => {
    await render(makeMessage("you", "Please pull last month's sales by scent."));
    expect(container.textContent ?? "").not.toContain("Not sent");
    expect(retryButton()).toBeNull();
  });

  /**
   * A surface with no resend wired still says the message failed. Silence is
   * the bug; a notice with no button is merely less good.
   */
  it("still reports the failure where no resend is wired", async () => {
    const message: ChatMessage = {
      ...makeMessage("you", "Please pull last month's sales by scent."),
      sendFailed: "cannot reach the company host at this origin",
    };
    await render(message, undefined);
    expect(container.textContent ?? "").toContain("Not sent");
    expect(retryButton()).toBeNull();
  });
});
