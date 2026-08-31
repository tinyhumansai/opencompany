// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { MessageComposer } from "@/views/chat/MessageComposer";
import { MessageTimeline } from "@/views/chat/MessageTimeline";
import type { MessageIntent } from "@/api/tasks";
import type { Channel } from "@/views/chat/model";

/**
 * First-run chat has one job: get an operator to make a request.
 *
 * A staffed but empty company used to lead with roster administration. These
 * checks exercise the two ends of the invitation: the card asks the parent to
 * start a brief, and the composer honours that request by replacing and
 * focusing its draft. Keeping them as rendered controls catches a regression
 * where the copy survives but the affordance no longer does anything.
 */

const CHANNEL: Channel = {
  id: "general",
  name: "general",
  kind: "channel",
  purpose: "",
};

let container: HTMLDivElement;
let root: Root;

function renderTimeline(onStartBrief: () => void) {
  act(() => {
    root.render(
      createElement(MessageTimeline, {
        channel: CHANNEL,
        items: [],
        openThreadId: null,
        typing: false,
        onOpenThread: () => {},
        onReact: () => {},
        onDismissCard: () => {},
        dismissingCardId: null,
        onStartBrief,
      }),
    );
  });
}

function renderComposer(prefill?: { text: string; revision: number }) {
  act(() => {
    root.render(
      createElement(MessageComposer, {
        placeholder: "Message #general",
        onSend: () => {},
        deliverableChoice: true,
        prefill,
      }),
    );
  });
}

function renderComposerForSend(
  onSend: (text: string, intent?: MessageIntent) => void,
) {
  act(() => {
    root.render(
      createElement(MessageComposer, {
        placeholder: "Message #general",
        onSend,
        deliverableChoice: true,
      }),
    );
  });
}

beforeEach(() => {
  (
    globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }
  ).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("the empty-channel first brief", () => {
  it("offers a brief instead of teammate creation and starts the composer action", () => {
    const onStartBrief = vi.fn();
    renderTimeline(onStartBrief);

    const brief = [...container.querySelectorAll("button")].find((button) =>
      button.textContent?.includes("Give the team a brief"),
    );
    expect(brief).toBeDefined();
    expect(container.textContent).not.toContain("Create teammate");

    act(() => brief!.click());
    expect(onStartBrief).toHaveBeenCalledOnce();
  });

  it("uses every new prefill revision, then explains each message mode", () => {
    renderComposer({ text: "Plan our first week.", revision: 1 });
    const textarea = container.querySelector("textarea");
    expect(textarea?.value).toBe("Plan our first week.");
    expect(document.activeElement).toBe(textarea);

    renderComposer({ text: "Plan our first month.", revision: 2 });
    expect(textarea?.value).toBe("Plan our first month.");

    expect(
      container
        .querySelector('[data-testid="composer-deliverable-chat"]')
        ?.getAttribute("title"),
    ).toBe("Chat without automatically creating a task.");
    expect(
      container
        .querySelector('[data-testid="composer-deliverable-once"]')
        ?.getAttribute("title"),
    ).toBe("Ask the team to do this once.");
    expect(
      container
        .querySelector('[data-testid="composer-deliverable-workflow"]')
        ?.getAttribute("title"),
    ).toBe("Turn this into a repeating workflow.");
  });

  it("resets a stale mode when the brief replaces the draft", () => {
    const onSend = vi.fn();
    renderComposerForSend(onSend);

    // The operator had picked "Just chatting" for the previous draft...
    act(() => {
      (
        container.querySelector(
          '[data-testid="composer-deliverable-chat"]',
        ) as HTMLButtonElement
      ).click();
    });

    // ...then the first-brief action replaces the draft wholesale.
    act(() => {
      root.render(
        createElement(MessageComposer, {
          placeholder: "Message #general",
          onSend,
          deliverableChoice: true,
          prefill: { text: "Help us get started.", revision: 1 },
        }),
      );
    });
    expect(container.querySelector("textarea")?.value).toBe(
      "Help us get started.",
    );

    // The brief is sent as a one-off task, not under the stale "chat" intent —
    // otherwise its request would be withheld. No mention directory is loaded
    // here, so the mentions arg is absent (undefined) rather than an empty list.
    act(() => {
      [...container.querySelectorAll("button")]
        .find((button) => button.getAttribute("aria-label") === "Send")!
        .click();
    });
    // The composer always passes third (attachments, issue #1682) and fourth
    // (mentions) arguments now — undefined here since this test never gives it
    // `uploadAttachment` or a mention directory.
    expect(onSend).toHaveBeenCalledWith("Help us get started.", "once", undefined, undefined);
  });
});
