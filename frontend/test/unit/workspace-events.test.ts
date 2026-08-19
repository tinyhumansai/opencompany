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
 * Issue #327: the console half of "the workspace announces its own writes".
 *
 * The host now projects a `workspace_changed` frame, but this file's own docs
 * record that a frame the host sends and this switch does not name is silently
 * dropped — it has happened three times (#464, #371, #384), and each time the
 * surface stayed stale with nothing to debug. So the routing is pinned here.
 */
describe("workspace_changed routing", () => {
  function subscribers() {
    return {
      onAgentReply: vi.fn(),
      onTaskEvent: vi.fn(),
      onWorkspaceEvent: vi.fn(),
      onTurnEvent: vi.fn(),
      onWorkflowRunEvent: vi.fn(),
      onWorkflowChanged: vi.fn(),
      onApprovalEvent: vi.fn(),
      onResync: vi.fn(),
    } satisfies Subs;
  }

  const frame = (change: string): Ev => ({
    type: "workspace_changed",
    seq: 7,
    atMillis: 1,
    nodeId: "n-1",
    change,
  });

  beforeEach(() => {
    for (const fn of Object.values(toasts)) fn.mockClear();
  });

  it("reaches the workspace subscriber and no other", () => {
    const subs = subscribers();
    handleEvent(frame("updated"), subs);

    expect(subs.onWorkspaceEvent).toHaveBeenCalledTimes(1);
    expect(subs.onWorkspaceEvent).toHaveBeenCalledWith(frame("updated"));
    // The board's subscriber is the tempting mis-wire: both frames are "a store
    // changed", but they refetch different surfaces, and crossing them would
    // make every note save re-read the board.
    expect(subs.onTaskEvent).not.toHaveBeenCalled();
    expect(subs.onWorkflowChanged).not.toHaveBeenCalled();
    expect(subs.onApprovalEvent).not.toHaveBeenCalled();
    expect(subs.onTurnEvent).not.toHaveBeenCalled();
    expect(subs.onAgentReply).not.toHaveBeenCalled();
  });

  it("carries the payload, not just the fact that something happened", () => {
    // Unlike the board's counter-based subscriber, the view's reaction depends
    // on WHICH node moved — the open note is refetched only when the frame
    // names it, and a removal closes the pane. A counter cannot say that.
    const subs = subscribers();
    handleEvent(frame("removed"), subs);
    const [received] = subs.onWorkspaceEvent.mock.calls[0] as [Ev];
    expect(received).toMatchObject({ nodeId: "n-1", change: "removed" });
  });

  it("raises no toast for any change word", () => {
    // A workspace write is not an attention signal: an autosaving editor fires
    // one per settled keystroke burst and every published deliverable adds
    // more. Toasting those would train the operator to dismiss the toasts that
    // do matter.
    const subs = subscribers();
    for (const change of ["opened", "updated", "removed"]) handleEvent(frame(change), subs);

    expect(subs.onWorkspaceEvent).toHaveBeenCalledTimes(3);
    for (const fn of Object.values(toasts)) expect(fn).not.toHaveBeenCalled();
  });

  it("tolerates a change word a newer host invented", () => {
    // `change` is widened past the host's three words on purpose, so a newer
    // host cannot make this a type error — and an unknown word must still
    // refetch rather than be dropped.
    const subs = subscribers();
    handleEvent(frame("restored"), subs);
    expect(subs.onWorkspaceEvent).toHaveBeenCalledTimes(1);
  });

  it("treats a stream gap as a silent canonical recovery instruction", () => {
    const subs = subscribers();
    handleEvent({ type: "stream_gap", missed: 44 }, subs);

    expect(subs.onResync).toHaveBeenCalledTimes(1);
    expect(subs.onWorkspaceEvent).not.toHaveBeenCalled();
    expect(subs.onTaskEvent).not.toHaveBeenCalled();
    expect(subs.onWorkflowChanged).not.toHaveBeenCalled();
    for (const fn of Object.values(toasts)) expect(fn).not.toHaveBeenCalled();
  });
});
