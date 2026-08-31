// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { WorkflowRunOutcome, WorkflowSummary } from "@/api/workflows";
import {
  WorkflowIndex,
  workflowTriggerLine,
} from "@/views/workflows/WorkflowIndex";

const WORKFLOWS: WorkflowSummary[] = [
  {
    id: "scheduled",
    name: "Scheduled digest",
    schedule: "0 9 * * MON",
    nodeCount: 3,
    enabled: false,
  },
  { id: "manual", name: "Manual review", schedule: null, nodeCount: 1 },
  { id: "older", name: "Older host row" },
];

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT =
    true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

function render(
  mode: "cards" | "list",
  runsByWorkflow: Map<string, WorkflowRunOutcome[]> = new Map(),
) {
  act(() => {
    root.render(
      createElement(WorkflowIndex, {
        workflows: WORKFLOWS,
        runsByWorkflow,
        onSelect: () => {},
        mode,
        loading: false,
        runsLoaded: true,
      }),
    );
  });
}

function item(selector: string, name: string): HTMLElement {
  const result = Array.from(container.querySelectorAll<HTMLElement>(selector)).find((row) =>
    row.textContent?.includes(name),
  );
  expect(result, `no ${selector} for ${name}`).toBeTruthy();
  return result as HTMLElement;
}

describe("workflow index summary facts", () => {
  for (const [mode, selector] of [
    ["cards", '[data-testid="workflow-card"]'],
    ["list", '[data-testid="workflow-list-row"]'],
  ] as const) {
    it(`shows the same trigger prose and step count in ${mode} mode`, () => {
      render(mode);

      expect(item(selector, "Scheduled digest").textContent).toContain(
        "Runs every Monday at 09:00 UTC · 3 steps",
      );
      expect(item(selector, "Scheduled digest").textContent).toContain("Paused");
      expect(item(selector, "Manual review").textContent).toContain("Runs on request · 1 step");

      const older = item(selector, "Older host row");
      expect(older.querySelector('[data-testid="workflow-index-facts"]')).toBeNull();
      expect(older.textContent).not.toContain("Runs on request");
      expect(older.textContent).not.toContain("0 steps");
    });
  }

  it("orders list rows by the latest run and puts workflows without runs last", () => {
    const run = (workflowId: string, atMillis: number): WorkflowRunOutcome => ({
      seq: atMillis,
      atMillis,
      workflowId,
      scheduled: false,
      deliveries: [],
      pendingApprovals: [],
    });

    render(
      "list",
      new Map([
        ["scheduled", [run("scheduled", 1_000)]],
        ["manual", [run("manual", 3_000)]],
      ]),
    );

    expect(
      Array.from(container.querySelectorAll('[data-testid="workflow-list-row"]')).map((row) =>
        row.querySelector("span[title]")?.getAttribute("title"),
      ),
    ).toEqual(["Manual review", "Scheduled digest", "Older host row"]);
  });
});

describe("workflowTriggerLine", () => {
  it("distinguishes unknown, manual, and common scheduled cadences", () => {
    expect(workflowTriggerLine(undefined)).toBeNull();
    expect(workflowTriggerLine(null)).toBe("Runs on request");
    expect(workflowTriggerLine("0 * * * *")).toBe("Runs hourly on the hour");
    expect(workflowTriggerLine("15 2 1 * *")).toBe("Runs monthly on day 1 at 02:15 UTC");
    expect(workflowTriggerLine("*/10 8-17 * * MON-FRI")).toBe(
      "Runs automatically on a custom schedule",
    );
    expect(workflowTriggerLine("*/10 8-17 * * MON-FRI")).not.toContain("*/10");
  });
});
