// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { Task } from "@/api/tasks";
import { taskApprovalRows } from "@/lib/task-approvals";
import { TaskItem } from "@/views/TaskCard";

/**
 * A real double-click on the board card's Resume button.
 *
 * `task-blocked-card.test.ts` proves Resume is *disabled* whenever the card's
 * own approvals are outstanding — the derived-state half of the rule. It never
 * drives two rapid clicks through the button while the card is clear to
 * resume, which is the other half: nothing in `resumeButton`'s `onClick`
 * depends on anything that changes between the two clicks of a real
 * double-click, so a debounce would have to live in the handler itself.
 */

const T0 = new Date("2026-03-02T10:00:00Z").getTime();

function card(): Task {
  return {
    id: "task-1",
    title: "Triage the release blockers",
    column: "working",
    stage: "paused",
    priority: "high",
    assignee: "qa",
    updatedAt: T0,
  } as Task;
}

let container: HTMLDivElement;
let root: Root;

async function render(onResume: () => void) {
  await act(async () => {
    root.render(
      createElement(TaskItem, {
        task: card(),
        dragging: false,
        // No approvals outstanding: the card is clear to resume, which is the
        // only state a real double-click on a live Resume is possible in.
        rows: taskApprovalRows([], {}, "task-1"),
        now: T0 + 60_000,
        askerNames: new Map(),
        deciding: new Map(),
        failed: {},
        onDecide: () => {},
        onOpen: () => {},
        onResume,
      }),
    );
  });
}

function resumeButton(): HTMLButtonElement {
  const found = Array.from(container.querySelectorAll("button")).find((b) =>
    b.textContent?.includes("Resume"),
  );
  if (!found) throw new Error(`no Resume button in:\n${container.innerHTML}`);
  return found as HTMLButtonElement;
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(async () => {
  await act(async () => root.unmount());
  container.remove();
});

describe("A real double-click on Resume", () => {
  // Fixed in `TaskItem`'s Resume `onClick`: a click-time gate ignores a
  // second click within two seconds of an accepted one, because nothing in
  // props changes between the two clicks of a real double-click — the resume
  // PATCH has not had a chance to flip the card off `paused` yet.
  it("reaches the resume handler once, not twice, for two clicks before either settles", async () => {
    const onResume = vi.fn();
    await render(onResume);

    const button = resumeButton();
    expect(button.disabled).toBe(false);

    // Two synchronous handler invocations, exactly as two real clicks would
    // land before a re-render (or any async work `onResume` kicks off) has a
    // chance to run.
    await act(async () => {
      button.click();
      button.click();
    });

    expect(onResume).toHaveBeenCalledTimes(1);
  });
});

describe("Resume is offered from props alone", () => {
  it("renders live with no client and no role prop at all — nothing to gate it by role", async () => {
    await render(() => {});

    // `TaskItem` takes no `client` and no `canManage`: there is no read it
    // could make to withhold Resume, which matches `patch_task`'s member-open
    // authority. A live button here is the whole of the AUTH property.
    expect(resumeButton().disabled).toBe(false);
  });
});

describe("A resume that did not land", () => {
  it("leaves Resume live rather than stuck disabled — the card holds no busy state of its own", async () => {
    // A rejected `patchTask` inside the caller's `onResume` (e.g. `LedgersView`'s
    // `resume()`, which toasts the failure) — represented here as a promise
    // the card is never given back, since `onResume: () => void` is
    // fire-and-forget from `TaskItem`'s own point of view.
    const onResume = vi.fn(() => {
      void Promise.reject(new Error("network_error")).catch(() => {});
    });
    await render(onResume);

    const button = resumeButton();
    expect(button.disabled).toBe(false);

    await act(async () => {
      button.click();
    });
    // Never held down waiting on a caller's own async work — the card holds
    // no "resuming" state of its own that a rejection could leave stuck.
    expect(button.disabled).toBe(false);

    // The one-second click gate against double-clicks is a rate limit, not
    // busy state: once the gesture window has passed, a retry click reaches
    // the handler exactly as before.
    await act(async () => {
      vi.useFakeTimers();
      vi.advanceTimersByTime(1500);
    });
    try {
      await act(async () => {
        button.click();
      });
      expect(onResume).toHaveBeenCalledTimes(2);
    } finally {
      vi.useRealTimers();
    }
  });
});
