// @vitest-environment jsdom

import { beforeEach, describe, expect, it, vi } from "vitest";

const toast = vi.hoisted(() => vi.fn());
vi.mock("sonner", () => ({ toast }));

import { handleEvent, type CompanyStreamEvent } from "@/hooks/use-events";

function completed(): Extract<CompanyStreamEvent, { type: "desk_task_completed" }> {
  return {
    type: "desk_task_completed",
    seq: 19,
    atMillis: 1_700_000_000_000,
    taskId: "task 7",
    desk: "engineering",
    column: "in_review",
    chatId: "engineering",
  };
}

beforeEach(() => {
  toast.mockReset();
  window.location.hash = "#/chat/strategy";
});

describe("background task completion attention", () => {
  it("toasts off the origin channel and links straight to the task", () => {
    const onTaskEvent = vi.fn();
    const onDispatchTerminal = vi.fn();

    handleEvent(completed(), {
      onTaskEvent,
      onDispatchTerminal,
      isViewingTaskOrigin: () => false,
    });

    expect(onTaskEvent).toHaveBeenCalledOnce();
    expect(onDispatchTerminal).toHaveBeenCalledOnce();
    expect(toast).toHaveBeenCalledOnce();
    expect(toast.mock.calls[0][0]).toBe("Task run finished");

    const options = toast.mock.calls[0][1];
    expect(options.action.label).toBe("Open task");
    options.action.onClick();
    expect(window.location.hash).toBe("#/tasks/task%207");
  });

  it("does not stack a toast over the origin channel's terminal pill", () => {
    const onDispatchTerminal = vi.fn();

    handleEvent(completed(), {
      onDispatchTerminal,
      isViewingTaskOrigin: () => true,
    });

    expect(onDispatchTerminal).toHaveBeenCalledOnce();
    expect(toast).not.toHaveBeenCalled();
  });

  it("toasts when no origin channel can be resolved", () => {
    handleEvent({ ...completed(), chatId: undefined }, {});

    expect(toast).toHaveBeenCalledOnce();
  });
});
