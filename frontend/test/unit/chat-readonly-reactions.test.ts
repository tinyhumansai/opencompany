// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { ChatMessage } from "@/lib/chat";
import { MessageTimeline } from "@/views/room/MessageTimeline";
import {
  buildTimeline,
  buildTimelineItems,
  legacyGeneralChannel,
  QUICK_REACTIONS,
  type Channel,
} from "@/views/room/model";

/**
 * Issue #1986 — a read-only channel must not offer a new reaction.
 *
 * Reactions are **not** allowed on a read-only channel (the `#general`
 * archive). The channel states "nothing can be posted here", so the UI must
 * not then offer an interaction — and the hover toolbar's five quick reactions
 * were the last interactive affordance left on that surface.
 *
 * **Hover is not what these tests have to simulate, and that is not a gap in
 * them.** The action bar is in the DOM unconditionally and revealed by CSS
 * alone (`hidden … group-hover/message:flex` in `MessageRow`), so the bug is
 * exactly "these buttons are in the tree on a read-only channel" — a jsdom
 * render sees precisely the state the pointer reaches, and no `:hover` is
 * involved in the assertion either way. Leaving them mounted-but-hidden would
 * also leave them reachable by keyboard focus (`group-focus-within/message`)
 * and by a screen reader, which is a second reason absence is the requirement
 * rather than invisibility. The browser check on the PR is what pins that the
 * *rest* of the toolbar still looks right when the emoji row is gone.
 *
 * What must survive, and is asserted here rather than left to inference:
 *
 * - **Reactions already left still render.** A reaction is content, and this
 *   channel is the only record of it; hiding one would lose information rather
 *   than withdraw an offer. It renders disabled, with a tooltip saying why.
 * - **The way into a thread stays.** An archived message is still worth reading
 *   the replies under. What may be *written* there is `ThreadPanel`'s question
 *   (#1757, #1984) and is out of this issue's scope — so a regression that
 *   swept the whole action bar away would be caught here.
 */

/** The read-only `#general` archive: `system` is the flag `RoomView` gates on. */
const ARCHIVE: Channel = legacyGeneralChannel([]);

/** An ordinary, writable channel — the control for every assertion below. */
const ENGINEERING: Channel = {
  id: "engineering",
  name: "engineering",
  kind: "channel",
  purpose: "",
};

const T0 = Date.UTC(2026, 8, 1, 9, 0, 0);

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
});

/**
 * One channel's transcript, rendered through `MessageTimeline`.
 *
 * Through the timeline rather than `MessageRow` directly, deliberately: the
 * flag is read off `channel.system` at that call site, so a test that handed
 * `MessageRow` a `readOnly` prop itself would pass with the plumbing removed.
 *
 * `createElement` rather than JSX because the unit suite's vitest `include` is
 * `*.test.ts` — a `.tsx` file is silently not collected, which reads as a
 * passing suite.
 */
function render(channel: Channel, messages: ChatMessage[]) {
  const items = buildTimelineItems(buildTimeline(messages, channel, []), []);
  act(() => {
    root.render(
      createElement(MessageTimeline, {
        channel,
        items,
        historyPending: false,
        openThreadId: null,
        typing: false,
        onOpenThread: () => {},
        onReact: () => {},
        onDismissCard: () => {},
        dismissingCardId: null,
      }),
    );
  });
}

/**
 * A durable line — an `h`-prefixed id, the shape `isHostMessageId` accepts.
 *
 * It has to be durable or `actionsUnavailableFor` disables the whole bar for
 * an unrelated reason ("not saved yet"), and every assertion below would pass
 * against a channel with no read-only rule at all.
 */
function report(over: Partial<ChatMessage> = {}): ChatMessage {
  return {
    id: "h1",
    from: "company",
    text: "Automation **weekly digest** finished.",
    at: T0,
    ...over,
  };
}

/** The hover toolbar's quick-reaction buttons — the affordance #1986 removes. */
function quickReactions(): HTMLButtonElement[] {
  return Array.from(container.querySelectorAll('button[aria-label^="React with "]'));
}

/** The chips under a line: one per emoji somebody has already used. */
function reactionChipButtons(): HTMLButtonElement[] {
  return Array.from(container.querySelectorAll("button[aria-pressed]")).filter(
    (button): button is HTMLButtonElement =>
      !(button.getAttribute("aria-label") ?? "").startsWith("React with "),
  );
}

describe("the hover reaction toolbar on a read-only channel (#1986)", () => {
  it("is not rendered at all on the archive", () => {
    render(ARCHIVE, [report()]);
    expect(quickReactions()).toHaveLength(0);
    // Absent, not merely hidden: nothing in the row may carry a quick
    // reaction's accessible name, however it is styled.
    expect(container.textContent).not.toContain(QUICK_REACTIONS[0]);
  });

  it("still offers every quick reaction on a writable channel", () => {
    render(ENGINEERING, [report()]);
    expect(quickReactions().map((button) => button.getAttribute("aria-label"))).toEqual(
      QUICK_REACTIONS.map((emoji) => `React with ${emoji}`),
    );
  });

  it("keeps the way into a thread on the archive", () => {
    // Scope guard. Reading the replies under a report is not reacting to it,
    // and #1986 is reactions only — a change that swept the whole action bar
    // away would satisfy the first test and be wrong.
    render(ARCHIVE, [report()]);
    expect(container.querySelector('button[aria-label="Reply in thread"]')).not.toBeNull();
  });
});

describe("reactions that are already there (#1986)", () => {
  it("still render on a read-only line, and say why they no longer toggle", () => {
    render(ARCHIVE, [report({ reactions: [{ emoji: "👍", by: "Mithil", mine: false }] })]);
    const chips = reactionChipButtons();
    expect(chips).toHaveLength(1);
    expect(chips[0].textContent).toContain("👍");
    expect(chips[0].disabled).toBe(true);
    expect(chips[0].title).toBe(
      "This channel is read-only — reactions cannot be added here.",
    );
  });

  it("stay toggleable on a writable channel", () => {
    render(ENGINEERING, [report({ reactions: [{ emoji: "👍", by: "Mithil", mine: false }] })]);
    const chips = reactionChipButtons();
    expect(chips).toHaveLength(1);
    expect(chips[0].disabled).toBe(false);
    expect(chips[0].title).toBe("Mithil reacted with 👍");
  });

  it("prefers the row's own reason over the channel's on an unsaved line", () => {
    // Both rules apply to a line the host has not journaled. "Not saved yet"
    // is the more specific fact and the one that would still be true in a
    // writable channel, so it must win — a read-only tooltip here would tell
    // an operator the wrong thing about why the chip is dead.
    render(ARCHIVE, [
      report({ id: "local-7", reactions: [{ emoji: "👍", by: "Mithil", mine: false }] }),
    ]);
    const chips = reactionChipButtons();
    expect(chips).toHaveLength(1);
    expect(chips[0].title).toBe(
      "Not saved yet — a reply or a reaction needs a message this company has stored.",
    );
  });
});
