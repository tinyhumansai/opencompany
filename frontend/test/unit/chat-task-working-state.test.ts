// @vitest-environment jsdom

import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import {
  taskStatusesById,
  type InflightRun,
  type Task,
  type TaskStatus,
} from "@/api/tasks";
import {
  isTaskWorking,
  MessageRow,
  taskElapsedLabel,
} from "@/views/chat/MessageRow";
import type { TimelineEntry } from "@/views/chat/model";

const NOW = 1_700_000_300_000;
const here = dirname(fileURLToPath(import.meta.url));
const appShell = readFileSync(resolve(here, "../../src/components/app-shell.tsx"), "utf8");

const ENTRY: TimelineEntry = {
  message: {
    id: "h1",
    from: "company",
    channel: "engineering",
    text: "I’ll get back to you when the research is ready.",
    at: NOW - 6 * 60_000,
    taskId: "task-7",
  },
  sender: { key: "engineering", name: "Engineering", kind: "company" },
  continuation: false,
  replies: [],
  replySenders: [],
};

function task(over: Partial<Task> = {}): Task {
  return {
    id: "task-7",
    title: "Research the market",
    column: "working",
    stage: "in_progress",
    priority: "medium",
    assignee: "engineering",
    updatedAt: NOW,
    ...over,
  };
}

function run(over: Partial<InflightRun> = {}): InflightRun {
  return {
    taskId: "task-7",
    key: "task-7",
    kind: "task",
    title: "Research the market",
    agentId: "engineering",
    startedAt: NOW - 5 * 60_000,
    pendingAction: null,
    ...over,
  };
}

describe("the shell's task-status merge", () => {
  it("refreshes both sources on the existing poll clock and task-event tick", () => {
    expect(appShell).toContain("listTasks(client, company)");
    expect(appShell).toContain("listInflight(client, company)");
    expect(appShell).toContain("[feed.now, taskEventTick, refreshTaskStatuses]");
  });

  it("uses the board stage and adds the in-flight start clock", () => {
    expect(taskStatusesById([task()], [run()])).toEqual({
      "task-7": { column: "in_progress", startedAt: NOW - 5 * 60_000 },
    });
  });

  it("keeps an in-flight task visible during a board-read race", () => {
    expect(taskStatusesById([], [run()])).toEqual({
      "task-7": { column: "in_progress", startedAt: NOW - 5 * 60_000 },
    });
  });

  it("does not turn a delegation with no board task into a chat status", () => {
    expect(taskStatusesById([], [run({ taskId: null, kind: "delegation" })])).toEqual({});
  });
});

describe("background task working semantics", () => {
  it("stops at review, done or paused unless the in-flight read still owns it", () => {
    for (const column of ["in_review", "done", "paused"]) {
      expect(isTaskWorking({ column })).toBe(false);
      expect(isTaskWorking({ column, startedAt: NOW - 60_000 })).toBe(true);
    }
    expect(isTaskWorking({ column: "in_progress" })).toBe(true);
    expect(isTaskWorking(undefined)).toBe(false);
  });

  it("does not keep a card Working once it is back in To-do (#1768)", () => {
    // A planning failure, a cancel, or a revision all return a card to
    // `pending` with no stage (Task.stage is "absent on a pending or done
    // card" — frontend/src/api/tasks.ts). That is a stopped state, the same
    // as review/done/paused, and must not read as still working just
    // because it isn't in the terminal set.
    expect(isTaskWorking({ column: "pending" })).toBe(false);
    // ...unless the in-flight read still owns it (board-read race).
    expect(isTaskWorking({ column: "pending", startedAt: NOW - 60_000 })).toBe(true);
  });

  it("reports whole elapsed minutes without allowing a negative clock", () => {
    expect(taskElapsedLabel(NOW - 5 * 60_000, NOW)).toBe("5 min elapsed, still working");
    expect(taskElapsedLabel(NOW + 30_000, NOW)).toBe("0 min elapsed, still working");
    expect(taskElapsedLabel(undefined, NOW)).toBeNull();
  });
});

let container: HTMLDivElement;
let root: Root;

function render(status: TaskStatus) {
  act(() => {
    root.render(
      createElement(MessageRow, {
        entry: ENTRY,
        threadOpen: false,
        onOpenThread: () => {},
        onReact: () => {},
        onDismissCard: () => {},
        dismissingCardId: null,
        taskStatusByTaskId: { "task-7": status },
        now: NOW,
      }),
    );
  });
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  Object.defineProperty(window, "matchMedia", {
    configurable: true,
    value: () => ({
      matches: false,
      addEventListener: () => {},
      removeEventListener: () => {},
    }),
  });
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("a card-linked background reply", () => {
  it("keeps a compact Working pill and elapsed clock beside the card", () => {
    render({ column: "in_progress", startedAt: NOW - 5 * 60_000 });

    expect(container.querySelector('[data-testid="working-indicator"]')).not.toBeNull();
    expect(container.textContent).toContain("Working…");
    expect(container.textContent).toContain("5 min elapsed, still working");
    expect(container.textContent).toContain("Card opened");
  });

  it("removes only the working state when the card reaches In review", () => {
    render({ column: "in_progress", startedAt: NOW - 5 * 60_000 });
    render({ column: "in_review" });

    expect(container.querySelector('[data-testid="working-indicator"]')).toBeNull();
    expect(container.textContent).not.toContain("still working");
    expect(container.textContent).toContain("Card opened");
  });
});
