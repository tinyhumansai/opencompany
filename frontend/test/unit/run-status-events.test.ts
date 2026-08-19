import { beforeEach, describe, expect, it, vi } from "vitest";

const toasts = vi.hoisted(() => ({
  base: vi.fn(),
  success: vi.fn(),
  error: vi.fn(),
  warning: vi.fn(),
  info: vi.fn(),
}));

vi.mock("sonner", () => {
  const toast = Object.assign(toasts.base, {
    success: toasts.success,
    error: toasts.error,
    warning: toasts.warning,
    info: toasts.info,
  });
  return { toast };
});

const { handleEvent } = await import("@/hooks/use-events");
type Ev = import("@/hooks/use-events").CompanyStreamEvent;
type Subs = import("@/hooks/use-events").Subscribers;

/**
 * How a `run_status_changed` frame is routed (issue #1015).
 *
 * The frame exists so the task detail screen can be pushed rather than polled —
 * it sat at `pending`/`running` for up to four seconds after an attempt had
 * really moved, and did not move at all while the tab was hidden. These pin the
 * two decisions that are easy to get wrong in the other direction: it must reach
 * the attempt subscriber, and it must **not** raise a toast or refetch the whole
 * board, because it fires several times per attempt on every card and every chat
 * turn.
 */
function frame(over: Partial<Extract<Ev, { type: "run_status_changed" }>> = {}): Ev {
  return {
    type: "run_status_changed",
    seq: 1,
    atMillis: 0,
    runId: "run-1",
    taskId: "card-1",
    attempt: 1,
    status: "running",
    from: "pending",
    ...over,
  } as Ev;
}

describe("run_status_changed", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("reaches the attempt subscriber", () => {
    const onRunEvent = vi.fn();
    handleEvent(frame(), { onRunEvent } as Subs);
    expect(onRunEvent).toHaveBeenCalledTimes(1);
  });

  it("raises no toast", () => {
    // Several frames per attempt — mint, start, settle — on every card and
    // every chat turn. Toasting those would train the operator to dismiss the
    // toasts that matter; the row moving IS the notification.
    handleEvent(frame(), { onRunEvent: vi.fn() } as Subs);
    expect(toasts.base).not.toHaveBeenCalled();
    expect(toasts.success).not.toHaveBeenCalled();
    expect(toasts.error).not.toHaveBeenCalled();
    expect(toasts.info).not.toHaveBeenCalled();
    expect(toasts.warning).not.toHaveBeenCalled();
  });

  it("does not refetch the board", () => {
    // `onTaskEvent` re-reads `GET …/tasks`. An attempt moving does not move a
    // card, so folding this in would refetch the whole board on every
    // transition of every run — which is the timer load #581 removed.
    const onTaskEvent = vi.fn();
    handleEvent(frame(), { onTaskEvent, onRunEvent: vi.fn() } as Subs);
    expect(onTaskEvent).not.toHaveBeenCalled();
  });

  it("is delivered for a chat turn, which names no card", () => {
    // A chat turn is a recorded attempt at work that opens no card, so `taskId`
    // is absent. The subscriber still hears it: the screen re-reads by run.
    const onRunEvent = vi.fn();
    handleEvent(frame({ taskId: undefined, from: undefined, status: "pending" }), {
      onRunEvent,
    } as Subs);
    expect(onRunEvent).toHaveBeenCalledTimes(1);
  });
});
