// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { ObservatoryRun, ObservatoryStep } from "@/api/observatory";
import { AttemptCard } from "@/views/observatory/AttemptCard";

/**
 * The attempt cards behind a deep link (issue #1679).
 *
 * A unit render, earned for the same reason the agent-runs render is: the claims
 * under test are what reaches the operator's eye, and no pure helper can hold
 * them. The grammar promises `#/observatory/<runId>?turn=<agentRunId>&step=7`,
 * and every way a card can betray that promise looks perfectly normal on
 * screen.
 *
 * 1. **A `step` alone must not open every card.** The selector used to read as
 *    "any focused step exists" and opened the whole page — the deep link to
 *    step 7 of one attempt was indistinguishable from "show me everything."
 * 2. **A `turn` opens the attempt it names, and only that one.** The operator
 *    lands on one attempt's trace, not a stack of every attempt that ever ran.
 * 3. **The named step is the one that scrolls and opens.** Within the matched
 *    attempt, the step the link names gets the focus treatment.
 */

function run(over: Partial<ObservatoryRun>): ObservatoryRun {
  return {
    id: "att-a",
    agentId: "alice",
    attempt: 1,
    status: "succeeded",
    phase: "terminal",
    taskId: null,
    chatId: null,
    workflowRunId: "wf-1",
    nodeId: "engineer",
    createdAtMillis: 1_700_000_000_000,
    startedAtMillis: 1_700_000_000_000,
    finishedAtMillis: 1_700_000_010_000,
    error: null,
    usage: { inputTokens: 100, outputTokens: 40, cachedInputTokens: 0, costUsd: 0 },
    stepCount: 1,
    steps: [],
    ...over,
  };
}

function step(over: Partial<ObservatoryStep> = {}): ObservatoryStep {
  return {
    seq: 1,
    atMillis: 1_700_000_000_000,
    kind: "tool_call",
    status: "ok",
    label: "read_file",
    detail: null,
    result: null,
    failure: null,
    truncated: false,
    elapsedMs: 12,
    deep: null,
    ...over,
  };
}

let container: HTMLDivElement;
let root: Root;

async function mount(
  runs: ObservatoryRun[],
  props: {
    turn?: string | null;
    focusStep?: number | null;
    onOpen?: (runId: string) => void;
  } = {},
) {
  await act(async () => {
    root.render(
      createElement(
        "div",
        null,
        runs.map((r) =>
          createElement(AttemptCard, {
            key: r.id,
            run: r,
            nowMs: 1_700_000_020_000,
            turn: props.turn ?? null,
            focusStep: props.focusStep ?? null,
            onOpen: props.onOpen,
          }),
        ),
      ),
    );
  });
  await act(async () => {});
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT =
    true;
  // jsdom has no layout; the focus effect scrolls the row into view.
  Element.prototype.scrollIntoView = vi.fn();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.restoreAllMocks();
});

describe("attempt cards behind a deep link", () => {
  it("does not open an attempt when only a step is named", async () => {
    // Two succeeded cards, no `turn`. A `focusStep` alone must open neither —
    // the link names a step inside a specific attempt, not every attempt.
    const a = run({ id: "att-a", steps: [step({ label: "read_alpha" })] });
    const b = run({ id: "att-b", steps: [step({ label: "read_beta" })] });
    await mount([a, b], { focusStep: 7 });

    expect(container.textContent).not.toContain("read_alpha");
    expect(container.textContent).not.toContain("read_beta");
  });

  it("opens the attempt a turn names, and only that one", async () => {
    // The bug this regression exists to catch: before `turn` was read, the
    // `step` selector alone opened *every* card — the deep link to step 7 of
    // one attempt rendered as "show me everything."
    const a = run({ id: "att-a", steps: [step({ label: "read_alpha" })] });
    const b = run({ id: "att-b", steps: [step({ label: "read_beta" })] });
    await mount([a, b], { turn: "att-b", focusStep: 7 });

    // att-a stays collapsed — the step link must not leak across attempts.
    expect(container.textContent).not.toContain("read_alpha");
    // att-b opens, because the deep link named it.
    expect(container.textContent).toContain("read_beta");
  });

  it("scrolls to and opens the named step within the matched attempt", async () => {
    // The focused step carries a body, so opening it renders a pane heading —
    // the operator lands on the reasoning behind exactly that step.
    const focused = run({
      id: "att-b",
      steps: [
        step({ seq: 6, label: "quiet" }),
        step({ seq: 7, label: "read_file", detail: "src/main.rs" }),
      ],
    });
    await mount([focused], { turn: "att-b", focusStep: 7 });

    const rows = container.querySelectorAll("li");
    expect(rows.length).toBe(2);
    const seventh = rows[1];
    expect(seventh.className).toContain("bg-muted/60");
    expect(container.textContent).toContain("Arguments (redacted)");
  });

  it("still opens itself on trouble without any deep link", async () => {
    // Unrelated to the link work: a failed attempt remains readable at a glance
    // when the operator arrives by scrolling rather than by deep link.
    const failed = run({
      id: "att-c",
      status: "failed",
      steps: [step({ label: "read_gamma", status: "error" })],
    });
    await mount([failed]);

    expect(container.textContent).toContain("read_gamma");
  });

  it("re-reads the deep half when an open attempt's step trace grows", async () => {
    // An open card on a live attempt keeps receiving steps between polls. The
    // parent's deep read is keyed on `onOpen`, so a card that stays open must
    // announce the growth or the new steps would keep empty deep panes until
    // the reader closes and reopens it (the one-time snapshot bug).
    const onOpen = vi.fn();
    const live = run({
      id: "att-a",
      phase: "active",
      status: "running",
      steps: [step({ seq: 1, label: "first" })],
    });
    await mount([live], { turn: "att-a", onOpen });

    expect(onOpen).toHaveBeenCalledTimes(1);
    expect(onOpen).toHaveBeenCalledWith("att-a");

    // A poll appends a step to the same run; the card stays open.
    const grown = run({
      id: "att-a",
      phase: "active",
      status: "running",
      steps: [step({ seq: 1, label: "first" }), step({ seq: 2, label: "second" })],
    });
    await mount([grown], { turn: "att-a", onOpen });

    expect(onOpen).toHaveBeenCalledTimes(2);
    expect(onOpen).toHaveBeenLastCalledWith("att-a");
  });

  it("re-reads the deep half when an existing step completes, not just when one is added", async () => {
    // The length-only signal missed the in-place transition: completing a tool
    // call flips an existing step's status and lands its result without adding
    // an ordinal, and an open card must re-read the deep half then too — the
    // reasoning that just flushed is exactly the pane a reader is watching.
    const onOpen = vi.fn();
    const running = run({
      id: "att-a",
      phase: "active",
      status: "running",
      steps: [step({ seq: 1, label: "read_file", status: "running", elapsedMs: 0 })],
    });
    await mount([running], { turn: "att-a", onOpen });

    expect(onOpen).toHaveBeenCalledTimes(1);

    // A poll rewrites the same ordinal: the call finished, its result landed.
    const completed = run({
      id: "att-a",
      phase: "active",
      status: "running",
      steps: [
        step({
          seq: 1,
          label: "read_file",
          status: "ok",
          elapsedMs: 412,
          result: "src/main.rs",
        }),
      ],
    });
    await mount([completed], { turn: "att-a", onOpen });

    expect(onOpen).toHaveBeenCalledTimes(2);
    expect(onOpen).toHaveBeenLastCalledWith("att-a");
  });
});
