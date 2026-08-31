// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { Task } from "@/api/tasks";
import { TaskItem } from "@/views/TaskCard";

/**
 * The board's bounce chip (issue #1865): a card in `todo` because a run
 * FAILED reads differently from one nobody has touched yet.
 *
 * Before this, `task.bounced` did not exist on the wire type and `todo` was
 * both the failure state and the fresh state — an operator had to open every
 * card in the column to tell a bounced retry candidate apart from work
 * nobody had picked up. This pins the rendered distinction: the chip shows
 * only in the pending phase, carries the host's reason verbatim, and a card
 * the host never marked bounced renders nothing extra at all.
 *
 * The fixture sends `column: "pending"` because that is what the board API
 * actually serves: `TaskCard::from` maps the stored `todo` stage through
 * `board::phase_of`, so the stage word never reaches the client. A fixture
 * built on `column: "todo"` passes against a component reading either word,
 * which is exactly how the chip shipped unrenderable.
 */

const T0 = new Date("2026-03-02T10:00:00Z").getTime();

function card(over: Partial<Task> = {}): Task {
  return {
    id: "task-1",
    title: "Send the weekly digest",
    // Issue #1865 (Codex review): the task API never serializes `column:
    // "todo"` — a card in the store's `todo` column carries the wire phase
    // `column: "pending"` (issue #1512) and no `stage` at all. `"todo"` here
    // was a wire value no real card can ever have, which is exactly how the
    // production `task.column === "todo"` check went unnoticed: this fixture
    // made it look reachable.
    column: "pending",
    priority: "medium",
    assignee: "ops",
    updatedAt: T0,
    ...over,
  } as Task;
}

let container: HTMLDivElement;
let root: Root;

async function render(task: Task) {
  await act(async () => {
    root.render(
      createElement(TaskItem, {
        task,
        dragging: false,
        // Since #1891 the card takes its approval state as `rows` rather than
        // a single `block` — this suite is not exercising any of that, so an
        // empty/no-op set of them leaves the card unblocked and the bounce
        // chip the only thing under test.
        rows: [],
        now: T0,
        askerNames: new Map(),
        deciding: new Map(),
        failed: {},
        onOpen: () => {},
        onResume: () => {},
      }),
    );
  });
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

describe("a card the host marked bounced", () => {
  it("shows the reason verbatim while sitting in the pending phase", async () => {
    await render(
      card({ bounced: "the provider refused the request: rate limited" }),
    );
    expect(container.textContent).toContain(
      "bounced: the provider refused the request: rate limited",
    );
  });

  it("says nothing extra once the host clears it (re-dispatched)", async () => {
    await render(card({ bounced: undefined }));
    expect(container.textContent).not.toContain("bounced:");
  });

  it("does not render the chip outside the pending phase, even if the field is somehow present", async () => {
    // Defensive: the host only ever writes `bounced` alongside a `todo`
    // landing, but the card's own render must not trust that invariant blindly
    // — a card an operator dragged into In Progress must never keep showing a
    // stale failure reason from before the retry started.
    await render(
      card({ column: "working", stage: "in_progress", bounced: "boom" } as Partial<Task>),
    );
    expect(container.textContent).not.toContain("bounced:");
  });
});
