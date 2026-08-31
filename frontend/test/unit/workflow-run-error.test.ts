import { describe, expect, it } from "vitest";

import type { WorkflowGraph } from "@/api/workflows";
import { ApiError } from "@/api/types";
import type { CompanyStreamEvent } from "@/hooks/use-events";
import { foldLiveRun } from "@/views/workflows/graph";
import { classifyRunError } from "@/views/workflows/run-error";

/**
 * How the Workflows console reads a failed run POST (issues #528, #514).
 *
 * The `run()` catch has three genuinely different jobs, and picking the wrong
 * one is a user-visible failure every time:
 *
 * - a host refusal it can FIX (no inference configured) must raise a persistent
 *   banner, never a vanishing toast — and it must key on the structured `code`,
 *   because the prose is localisable and drifts;
 * - `not_wired` is deliberately NOT such a refusal: on a genuinely offline build
 *   pointing the operator at an inference setting would lie (issue #514 exists to
 *   split those), so it must fall through to a plain error;
 * - a dropped connection on a run we SAW start is survivable — the host keeps
 *   walking the graph (#383) — so it must reassure rather than cry failure, but
 *   only when we actually saw the run begin.
 */
describe("classifyRunError", () => {
  it("treats an inference_required host refusal as a fixable refusal", () => {
    const e = new ApiError(409, "inference_required", "needs an inference source", true);
    expect(classifyRunError(e, false)).toBe("refusal-inference");
  });

  it("treats a restart_required host refusal as a fixable refusal", () => {
    const e = new ApiError(409, "restart_required", "a restart is owed", true);
    expect(classifyRunError(e, false)).toBe("refusal-inference");
  });

  it("does NOT treat not_wired as an inference refusal — that stays a plain error", () => {
    // A genuinely inference-less build answers 404 not_wired; #514 keeps it out
    // of the refusal set so the console never tells the operator to configure a
    // provider the deployment cannot use.
    const e = new ApiError(404, "not_wired", "this build has no execution", true);
    expect(classifyRunError(e, false)).toBe("other");
    expect(classifyRunError(e, true)).toBe("other");
  });

  it("reads a status-0 transport failure as connection-lost when the run was seen to start", () => {
    const e = new ApiError(0, "network_error", "cannot reach the company host", false);
    expect(classifyRunError(e, true)).toBe("connection-lost");
  });

  it("reads a status-0 transport failure as a plain error when the run never started", () => {
    const e = new ApiError(0, "network_error", "cannot reach the company host", false);
    expect(classifyRunError(e, false)).toBe("other");
  });

  it("reads a proxy 504 (no host envelope) as connection-lost when the run was seen to start", () => {
    const e = new ApiError(504, "http_504", "HTTP 504", false);
    expect(classifyRunError(e, true)).toBe("connection-lost");
  });

  it("reads a proxy 504 as a plain error when the run never started", () => {
    const e = new ApiError(504, "http_504", "HTTP 504", false);
    expect(classifyRunError(e, false)).toBe("other");
  });

  it("treats a non-ApiError rejection as a plain error", () => {
    expect(classifyRunError(new Error("boom"), true)).toBe("other");
    expect(classifyRunError("boom", true)).toBe("other");
    expect(classifyRunError(undefined, true)).toBe("other");
  });
});

/**
 * The fold carries the run's `scheduled` flag straight off its start frame
 * (issue #528), so the console can tell its own manual run apart from a
 * concurrent cron fire while the run POST is still open.
 */
describe("foldLiveRun scheduled", () => {
  const GRAPH: WorkflowGraph = {
    id: "wf1",
    name: "Nightly digest",
    version: null,
    nodes: [
      { id: "t", kind: "trigger", name: "Trigger" },
      { id: "a", kind: "agent", name: "Agent" },
    ],
    edges: [{ from: "t", to: "a" }],
  };

  function startFrame(scheduled: boolean): CompanyStreamEvent {
    return {
      type: "workflow_run_started",
      seq: 1,
      atMillis: 1_000,
      workflowId: "wf1",
      runId: "run-1",
      scheduled,
      startedBy: scheduled ? "schedule" : "operator",
    };
  }

  it("is false for an operator-started run", () => {
    const live = foldLiveRun([startFrame(false)], "wf1", GRAPH);
    expect(live).not.toBeNull();
    expect(live?.scheduled).toBe(false);
    expect(live?.runId).toBe("run-1");
    expect(live?.active).toBe(true);
  });

  it("is true for a cron-started run", () => {
    const live = foldLiveRun([startFrame(true)], "wf1", GRAPH);
    expect(live?.scheduled).toBe(true);
  });
});
