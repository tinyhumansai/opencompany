// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { CognitionState } from "@/api/types";
import type { ChatMessage } from "@/lib/chat";
import { MessageTimeline } from "@/views/chat/MessageTimeline";
import { ThreadPanel } from "@/views/chat/ThreadPanel";
import { buildTimeline, buildTimelineItems, type Channel } from "@/views/chat/model";

/**
 * Issue #1734 — a reply the offline echo brain produced must not read as
 * written by the teammate it appears under.
 *
 * The defect is a reader being misled by output that is otherwise correct: on
 * an instance with no inference configured, `EchoBrain` answers "You said:
 * <your message>" and the transcript renders it with the same avatar, the same
 * name, the same timestamp and the same bubble as a considered reply. Nothing
 * throws; the operator simply concludes the product is stupid rather than
 * unconfigured.
 *
 * The console cannot tell the two apart from the message — `ChatMessage`
 * carries no provenance — so the marker is driven by the company-level
 * cognition state the host now reports (issue #1735). These tests pin both
 * directions of that state, the one row it must never touch, the cause its
 * tooltip has to name, and the second surface that renders an author line.
 */

const CHANNEL: Channel = {
  id: "engineering",
  name: "engineering",
  kind: "channel",
  purpose: "",
};

const T0 = Date.UTC(2026, 7, 25, 9, 0, 0);

function message(over: Partial<ChatMessage> & { id: string }): ChatMessage {
  return { from: "company", text: "…", at: T0, ...over };
}

let container: HTMLDivElement;
let root: Root;

/**
 * The transcript, with one operator line and the echo brain's answer to it.
 *
 * `createElement` rather than JSX because the unit suite's vitest `include` is
 * `*.test.ts` — a `.tsx` file is silently not collected, which reads as a
 * passing suite.
 */
function render(cognition: CognitionState) {
  const messages = [
    message({ id: "h1", from: "you", text: "yo" }),
    message({ id: "h2", from: "company", text: "You said: yo", at: T0 + 1000 }),
  ];
  const items = buildTimelineItems(buildTimeline(messages, CHANNEL, []), []);
  act(() => {
    root.render(
      createElement(MessageTimeline, {
        channel: CHANNEL,
        items,
        historyPending: false,
        openThreadId: null,
        typing: false,
        onOpenThread: () => {},
        onReact: () => {},
        onDismissCard: () => {},
        dismissingCardId: null,
        cognition,
      }),
    );
  });
}

/**
 * The thread panel, with one echoed company reply under the parent.
 *
 * A separate render path with its own author line — which is exactly why it
 * needs its own test: the first cut of this fix threaded the state into
 * `MessageTimeline` only, and a reader who opened a thread was handed back the
 * unmarked attribution the channel had just stopped showing them.
 */
function renderThread(cognition: CognitionState | undefined) {
  act(() => {
    root.render(
      createElement(ThreadPanel, {
        channel: CHANNEL,
        members: [],
        parent: message({ id: "p1", from: "you", text: "yo" }),
        replies: [message({ id: "r1", from: "company", text: "You said: yo", at: T0 + 1000 })],
        sending: false,
        onSend: () => {},
        onClose: () => {},
        cognition,
      }),
    );
  });
}

/** Every placeholder marker currently in the transcript. */
function markers(): HTMLElement[] {
  return Array.from(container.querySelectorAll('[data-testid="chat-echo-placeholder"]'));
}

/**
 * The author name on the same line as a marker.
 *
 * The chip is the middle child of the author line — name, chip, timestamp — so
 * the name is the line's first child. Reached structurally because the point is
 * *which* row was marked, and a query naming the company row would pass by
 * construction.
 */
function authorOf(chip: HTMLElement): string {
  return chip.parentElement!.firstElementChild!.textContent ?? "";
}

/** Every row in the transcript, marked or not. */
function rows(): HTMLElement[] {
  return Array.from(container.querySelectorAll("article[data-message-id]"));
}

function rowFor(id: string): HTMLElement {
  return rows().find((row) => row.dataset.messageId === id)!;
}

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

describe("the echo-brain placeholder marker", () => {
  it("marks the company's line when the company is on the echo brain", () => {
    render("unconfigured");

    expect(markers()).toHaveLength(1);
    expect(markers()[0].textContent).toBe("Placeholder");
    // The sentence behind the chip is what an operator actually needs, and it
    // has to name the voice it is contradicting — a bare "Placeholder" next to
    // a teammate's name explains nothing.
    expect(markers()[0].getAttribute("title")).toMatch(/did not write this/);
    expect(markers()[0].getAttribute("title")).toMatch(/no model configured/);
  });

  /**
   * The chip renders for both echo states, so it must not narrate the wrong
   * one. On a host with no harness the banner above says no setting will help
   * and the tooltip said the company had no model configured — the two
   * contradicting each other on the same screen (CodeRabbit and codex, PR
   * #1740). The cause travels with the state precisely so this cannot recur.
   */
  it("names the harness, not a missing model, when no harness is available", () => {
    render("unavailable");

    expect(markers()).toHaveLength(1);
    const title = markers()[0].getAttribute("title")!;
    expect(title).toMatch(/did not write this/);
    expect(title).toMatch(/No agent harness is available on this host/);
    // The remedy the banner rules out must not be implied here either.
    expect(title).not.toMatch(/no model configured/);
  });

  /** The chip names no cause either when the host could not determine one. */
  it("says the host could not tell, when cognition is undetermined", () => {
    render("undetermined");

    expect(markers()).toHaveLength(1);
    const title = markers()[0].getAttribute("title")!;
    expect(title).toMatch(/did not write this/);
    expect(title).toMatch(/could not read its inference configuration/);
    // Neither of the two remedies is implied — the host named neither.
    expect(title).not.toMatch(/no model configured/);
    expect(title).not.toMatch(/No agent harness is available/);
  });

  it("marks the company's line and not the operator's", () => {
    render("unconfigured");

    // The operator wrote `h1` and knows it — marking that would be the same
    // misattribution pointed the other way. `h2` is the echo.
    expect(rowFor("h1").querySelector('[data-testid="chat-echo-placeholder"]')).toBeNull();
    expect(rowFor("h2").querySelector('[data-testid="chat-echo-placeholder"]')).not.toBeNull();
    // And the marker is beside the voice it contradicts, not floating loose.
    expect(authorOf(markers()[0])).not.toBe("");
    expect(authorOf(markers()[0])).not.toMatch(/^You$/);
  });

  it("marks nothing when the company has a model", () => {
    render("configured");

    expect(markers()).toHaveLength(0);
  });

  it("marks nothing when the host never said either way", () => {
    // `cognition` omitted entirely — an older host, or one that could not answer.
    // Silence is not evidence of an echo, and a console that treats it as one
    // is the same bug pointed the other way.
    const messages = [message({ id: "h1", from: "company", text: "You said: yo" })];
    const items = buildTimelineItems(buildTimeline(messages, CHANNEL, []), []);
    act(() => {
      root.render(
        createElement(MessageTimeline, {
          channel: CHANNEL,
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

    expect(markers()).toHaveLength(0);
  });

  /**
   * The thread panel is the second place an author line is drawn, and it draws
   * its own — so it needs the state threaded to it or it silently keeps the
   * behaviour the channel just lost. A reader who clicks into a thread is doing
   * the *more* attentive kind of reading; handing them the unmarked version
   * there is the worse half of the bug, not a lesser one.
   */
  it("marks an echoed reply inside a thread, and not the operator's parent", () => {
    renderThread("unconfigured");

    expect(markers()).toHaveLength(1);
    expect(authorOf(markers()[0])).not.toMatch(/^You$/);
    expect(markers()[0].getAttribute("title")).toMatch(/did not write this/);
  });

  it("marks nothing in a thread when the host never said either way", () => {
    renderThread(undefined);

    expect(markers()).toHaveLength(0);
  });

  /**
   * The row this marker must never touch, and the one easiest to miss.
   *
   * In a multi-user company `fromHistory` maps every `mine: false` message to
   * `from: "company"`, so another signed-in person's own words arrive on the
   * company side of the transcript — distinguishable only by the `operator`
   * channel the host stamps on them (`chat_history.rs`, `OperatorMessage`).
   * Marking one tells a colleague that the echo brain wrote another colleague's
   * message, which is a *fabricated* attribution — strictly worse than the
   * missing one this whole change exists to fix (codex, PR #1740).
   */
  it("never marks another signed-in person's message", () => {
    const messages = [
      message({ id: "h1", from: "you", text: "yo" }),
      // A collaborator: not mine, so `from: "company"`, but the host says a
      // person typed it. Deliberately carrying the echo brain's own channel
      // label as well, because that label matches both and is exactly the
      // signal this must not be reading.
      message({
        id: "h2",
        from: "company",
        channel: "operator",
        byPerson: true,
        text: "on it",
        at: T0 + 1000,
      }),
      // And a genuine echo reply beside it, so the test proves the predicate
      // discriminates rather than simply marking nothing. Past the 5-minute
      // grouping window, or it would join `h2`'s run and render no author line
      // of its own — passing for a reason that has nothing to do with the
      // predicate under test.
      // The echo brain's reply carries `channel: "operator"` too — that is the
      // collision — and no `byPerson`.
      message({
        id: "h3",
        from: "company",
        channel: "operator",
        text: "You said: yo",
        at: T0 + 20 * 60 * 1000,
      }),
    ];
    const items = buildTimelineItems(buildTimeline(messages, CHANNEL, []), []);
    act(() => {
      root.render(
        createElement(MessageTimeline, {
          channel: CHANNEL,
          items,
          historyPending: false,
          openThreadId: null,
          typing: false,
          onOpenThread: () => {},
          onReact: () => {},
          onDismissCard: () => {},
          dismissingCardId: null,
          cognition: "unconfigured",
        }),
      );
    });

    expect(rowFor("h2").querySelector('[data-testid="chat-echo-placeholder"]')).toBeNull();
    expect(rowFor("h3").querySelector('[data-testid="chat-echo-placeholder"]')).not.toBeNull();
    expect(markers()).toHaveLength(1);
  });

  /**
   * A run is a claim that consecutive lines are one utterance, and two
   * different authors never are (codex, PR #1740).
   *
   * A collaborator's message and an echo reply share a sender key — both are
   * `from: "company"`, and the echo brain names its outbound channel `operator`
   * exactly as an operator message does — so within the 5-minute window they
   * grouped, and the second row rendered as a continuation with no author line
   * and therefore no marker. Both orders are wrong: an echo hides inside a
   * colleague's run unmarked, and a colleague's words sit under an author line
   * the marker has already labelled as the echo brain's.
   *
   * Deliberately inside the grouping window, because outside it the rows never
   * grouped and the test would pass on a build with the bug.
   */
  it("breaks a run between a person's line and an echo reply", () => {
    const person = (id: string, at: number) =>
      message({ id, from: "company", channel: "operator", byPerson: true, text: "on it", at });
    const echo = (id: string, at: number) =>
      message({ id, from: "company", channel: "operator", text: "You said: yo", at });

    const cases: Array<[string, ChatMessage[]]> = [
      ["person then echo", [person("a1", T0), echo("a2", T0 + 1000)]],
      ["echo then person", [echo("b1", T0), person("b2", T0 + 1000)]],
    ];
    for (const [order, messages] of cases) {
      const items = buildTimelineItems(buildTimeline(messages, CHANNEL, []), []);
      act(() => {
        root.render(
          createElement(MessageTimeline, {
            channel: CHANNEL,
            items,
            historyPending: false,
            openThreadId: null,
            typing: false,
            onOpenThread: () => {},
            onReact: () => {},
            onDismissCard: () => {},
            dismissingCardId: null,
            cognition: "unconfigured",
          }),
        );
      });

      // Exactly one marker, and it is on the echo row — never on the person's.
      expect(markers(), order).toHaveLength(1);
      const marked = markers()[0].closest("article[data-message-id]") as HTMLElement;
      expect(marked.dataset.messageId, order).toBe(order === "person then echo" ? "a2" : "b1");
    }
  });

  /** The same rule, on the panel that draws its own author line. */
  it("never marks another signed-in person's reply inside a thread", () => {
    act(() => {
      root.render(
        createElement(ThreadPanel, {
          channel: CHANNEL,
          members: [],
          parent: message({ id: "p1", from: "you", text: "yo" }),
          replies: [
            message({
              id: "r1",
              from: "company",
              channel: "operator",
              byPerson: true,
              text: "on it",
              at: T0 + 1000,
            }),
          ],
          sending: false,
          onSend: () => {},
          onClose: () => {},
          cognition: "unconfigured",
        }),
      );
    });

    expect(markers()).toHaveLength(0);
  });
});
