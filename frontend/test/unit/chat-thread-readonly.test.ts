// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { TeamMember } from "@/lib/team";
import { ThreadPanel } from "@/views/room/ThreadPanel";
import { legacyGeneralChannel } from "@/views/room/model";

/**
 * `ThreadPanel` renders its own composer, so a read-only channel — the
 * `#general` archive — has to take it away there too, or an archived message
 * could be opened as a thread and replied to.
 *
 * The read-only answer is no composer, not a disabled one: a disabled textarea,
 * `@` button, paperclip and Send are still a claim that replying is a thing
 * you do here. So these assert absence, and the writable cases below keep this
 * from being satisfiable by a panel that renders nothing at all.
 */

const MEMBERS: TeamMember[] = [];
const CHANNEL = legacyGeneralChannel(MEMBERS);

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
        parent: { id: "p", from: "company", text: "archived update", at: 0 },
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

describe("thread composer on a read-only channel", () => {
  it("renders no composer at all for a read-only channel", async () => {
    await render(true);

    expect(textarea()).toBeNull();
    expect(sendButton()).toBeNull();
  });

  it("renders none of the composer's affordances either", async () => {
    await render(true);

    // Each of these is a separate claim that some action exists here. A
    // disabled one is still that claim, so absence is what is asserted.
    for (const label of ["Mention someone", "Attach a file", "Formatting"]) {
      expect(container.querySelector(`[aria-label="${label}"]`)).toBeNull();
    }
    // The keyboard hint describes a send there is no longer any way to make.
    expect(container.textContent).not.toContain("to send");
  });

  it("puts the explanation in the space the composer used to occupy", async () => {
    await render(true);

    const notice = container.querySelector('[data-testid="thread-read-only-notice"]');
    expect(notice).not.toBeNull();
    expect(notice?.getAttribute("role")).toBe("status");
    expect(notice?.textContent).toContain("nothing can be posted here");
    // It is the last thing in the panel: the composer is not below it, hidden
    // or otherwise. `lastElementChild` fails the moment one is rendered again.
    expect(container.querySelector("aside")?.lastElementChild).toBe(notice);
  });

  it("keeps the thread composer working on an ordinary channel", async () => {
    await render(false);
    await type("on it");

    expect(textarea().placeholder).toBe("Reply…");
    expect(sendButton().disabled).toBe(false);
    expect(container.querySelector('[data-testid="thread-read-only-notice"]')).toBeNull();

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
