import { describe, expect, it } from "vitest";

import type { WorkflowRunOutcome, WorkflowRunVerdict } from "@/api/workflows";
import {
  compareRuns,
  runMatchesFilters,
  startedAt,
} from "@/views/workflows/RunTracesList";

/**
 * Issue #1697's traces list: the sort comparator and the filter predicate
 * that decide what the table shows and in what order. Neither had a test
 * before this — and the status column's first-click direction (see
 * `compareRuns`) had a one-line sign error a mount-and-screenshot check
 * would not have caught, which is exactly the gap these pin.
 */

const NOW = 1_700_000_000_000;

function run(over: Partial<WorkflowRunOutcome> = {}): WorkflowRunOutcome {
  return {
    seq: 1,
    atMillis: NOW,
    workflowId: "feature_pipeline",
    scheduled: false,
    runId: "run-1",
    deliveries: [],
    pendingApprovals: [],
    ...over,
  };
}

describe("startedAt", () => {
  it("prefers the recorded start over the finish", () => {
    expect(startedAt(run({ atMillis: NOW, startedAtMillis: NOW - 5_000 }))).toBe(
      NOW - 5_000,
    );
  });

  it("falls back to the finish for a run journaled before issue #371", () => {
    expect(startedAt(run({ atMillis: NOW, startedAtMillis: undefined }))).toBe(NOW);
  });
});

describe("compareRuns", () => {
  const nameById = new Map([
    ["feature_pipeline", "Feature pipeline"],
    ["weekly_review", "Weekly review"],
  ]);

  it("sorts by workflow name, respecting direction", () => {
    const a = run({ workflowId: "weekly_review" });
    const b = run({ workflowId: "feature_pipeline" });
    expect(compareRuns(a, b, "workflow", "asc", nameById)).toBeGreaterThan(0);
    expect(compareRuns(a, b, "workflow", "desc", nameById)).toBeLessThan(0);
  });

  it("sorts manual before scheduled ascending, and the reverse descending", () => {
    const manual = run({ scheduled: false });
    const scheduled = run({ scheduled: true });
    expect(compareRuns(manual, scheduled, "trigger", "asc", nameById)).toBeLessThan(0);
    expect(compareRuns(manual, scheduled, "trigger", "desc", nameById)).toBeGreaterThan(0);
  });

  it("sorts started-at chronologically", () => {
    const earlier = run({ startedAtMillis: NOW - 10_000 });
    const later = run({ startedAtMillis: NOW });
    expect(compareRuns(earlier, later, "startedAt", "asc", nameById)).toBeLessThan(0);
    expect(compareRuns(earlier, later, "startedAt", "desc", nameById)).toBeGreaterThan(0);
  });

  it("puts a failed run before an ok one on the first (descending) status click", () => {
    // The exact regression: a plain rank difference put `ok` first here,
    // which is the one reading a "descending" status sort must not give.
    const failed = run({ error: "boom", verdict: "failed" });
    const ok = run({ verdict: "ok" });
    expect(compareRuns(failed, ok, "status", "desc", nameById)).toBeLessThan(0);
  });

  it("reverses to ok-first on the second (ascending) status click", () => {
    const failed = run({ error: "boom", verdict: "failed" });
    const ok = run({ verdict: "ok" });
    expect(compareRuns(failed, ok, "status", "asc", nameById)).toBeGreaterThan(0);
  });

  it("orders every verdict by severity, most-attention-first, descending", () => {
    const verdicts: WorkflowRunVerdict[] = [
      "running",
      "failed",
      "stopped",
      "stranded",
      "blocked",
      "undelivered",
      "awaiting-approval",
      "ok",
    ];
    const runs = verdicts.map((verdict) => run({ verdict }));
    const sorted = [...runs].sort((a, b) => compareRuns(a, b, "status", "desc", nameById));
    expect(sorted.map((r) => r.verdict)).toEqual(verdicts);
  });
});

describe("runMatchesFilters", () => {
  it("keeps every run when nothing is filtered", () => {
    expect(
      runMatchesFilters(run(), {
        now: NOW,
        rangeMs: null,
        workflowFilter: new Set(),
        verdictFilter: new Set(),
      }),
    ).toBe(true);
  });

  it("drops a run older than the time-range cutoff", () => {
    const filters = {
      now: NOW,
      rangeMs: 60 * 60 * 1000,
      workflowFilter: new Set<string>(),
      verdictFilter: new Set<WorkflowRunVerdict>(),
    };
    expect(
      runMatchesFilters(run({ startedAtMillis: NOW - 30 * 60 * 1000 }), filters),
    ).toBe(true);
    expect(
      runMatchesFilters(run({ startedAtMillis: NOW - 90 * 60 * 1000 }), filters),
    ).toBe(false);
  });

  it("filters by started-at, not the finish time", () => {
    // A run that finished within the window but started well before it must
    // still be excluded — the window is about when it kicked off.
    const filters = {
      now: NOW,
      rangeMs: 60 * 60 * 1000,
      workflowFilter: new Set<string>(),
      verdictFilter: new Set<WorkflowRunVerdict>(),
    };
    expect(
      runMatchesFilters(
        run({ atMillis: NOW, startedAtMillis: NOW - 90 * 60 * 1000 }),
        filters,
      ),
    ).toBe(false);
  });

  it("keeps only the checked workflows", () => {
    const filters = {
      now: NOW,
      rangeMs: null,
      workflowFilter: new Set(["weekly_review"]),
      verdictFilter: new Set<WorkflowRunVerdict>(),
    };
    expect(runMatchesFilters(run({ workflowId: "weekly_review" }), filters)).toBe(true);
    expect(runMatchesFilters(run({ workflowId: "feature_pipeline" }), filters)).toBe(
      false,
    );
  });

  it("keeps only the checked verdicts", () => {
    const filters = {
      now: NOW,
      rangeMs: null,
      workflowFilter: new Set<string>(),
      verdictFilter: new Set<WorkflowRunVerdict>(["failed"]),
    };
    expect(runMatchesFilters(run({ error: "boom", verdict: "failed" }), filters)).toBe(
      true,
    );
    expect(runMatchesFilters(run({ verdict: "ok" }), filters)).toBe(false);
  });

  it("combines all three filters with AND, not OR", () => {
    const filters = {
      now: NOW,
      rangeMs: 60 * 60 * 1000,
      workflowFilter: new Set(["weekly_review"]),
      verdictFilter: new Set<WorkflowRunVerdict>(["failed"]),
    };
    // Matches workflow and verdict, but started outside the window.
    expect(
      runMatchesFilters(
        run({
          workflowId: "weekly_review",
          error: "boom",
          verdict: "failed",
          startedAtMillis: NOW - 90 * 60 * 1000,
        }),
        filters,
      ),
    ).toBe(false);
    // Matches all three.
    expect(
      runMatchesFilters(
        run({
          workflowId: "weekly_review",
          error: "boom",
          verdict: "failed",
          startedAtMillis: NOW - 5 * 60 * 1000,
        }),
        filters,
      ),
    ).toBe(true);
  });
});
