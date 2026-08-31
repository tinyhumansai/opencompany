import { describe, expect, it } from "vitest";

import type { ObservatoryRun, ObservatoryStep } from "@/api/observatory";
import {
  byAgent,
  byNode,
  failureHistogram,
  runState,
  spansFromRuns,
  spansFromSteps,
  stepState,
  totals,
} from "@/views/observatory/model";

const NOW = 10_000;

function step(over: Partial<ObservatoryStep> = {}): ObservatoryStep {
  return {
    seq: 0,
    atMillis: 1000,
    kind: "tool_call",
    status: "ok",
    label: "Shell",
    detail: null,
    result: null,
    failure: null,
    truncated: false,
    elapsedMs: 100,
    deep: null,
    ...over,
  };
}

function run(over: Partial<ObservatoryRun> = {}): ObservatoryRun {
  return {
    id: "r1",
    agentId: "programmer",
    attempt: 1,
    status: "succeeded",
    phase: "terminal",
    taskId: null,
    chatId: null,
    workflowRunId: "wr-1",
    nodeId: "solve",
    createdAtMillis: 900,
    startedAtMillis: 1000,
    finishedAtMillis: 2000,
    error: null,
    usage: { inputTokens: 10, outputTokens: 5, cachedInputTokens: 2, costUsd: 0.5 },
    stepCount: 1,
    steps: [step()],
    ...over,
  };
}

describe("runState", () => {
  it("separates blocked from failed", () => {
    // A node waiting on a person has not gone wrong. Folding the two sends an
    // operator hunting a bug in the node that most often just needs a click.
    expect(runState(run({ status: "waiting_approval" }))).toBe("blocked");
    expect(runState(run({ status: "paused" }))).toBe("blocked");
    expect(runState(run({ status: "failed" }))).toBe("failed");
    expect(runState(run({ status: "cancelled" }))).toBe("failed");
    expect(runState(run({ status: "succeeded" }))).toBe("done");
    expect(runState(run({ status: "running" }))).toBe("running");
  });

  it("keeps a declined refusal out of the success tone (issue #1809)", () => {
    // A by-design decline is neither a failure nor a success — `RunTimeline`
    // and `AgentRuns` already paint it the same neutral tone `cancelled`
    // gets, never green. Folding it into "done" here would paint an
    // AttemptCard, the waterfall span and a workflow-run's summary dot green
    // for a run that was refused, not completed — and would count it in
    // `byNode`'s `succeeded` column. "idle" is the closed vocabulary's own
    // neutral word (docs/design-system/color.md), the one `stopped` and
    // `stranded` already wear for "nothing is happening, nothing went
    // wrong" — not a sixth colour invented for this one status.
    expect(runState(run({ status: "declined" }))).toBe("idle");
    expect(runState(run({ status: "declined" }))).not.toBe("done");
  });
});

describe("spansFromRuns", () => {
  it("starts at the turn, not at the row being minted", () => {
    // A row exists before its turn begins; drawing the gap as work would
    // overstate every agent's share of the run.
    const [s] = spansFromRuns([run({ createdAtMillis: 900, startedAtMillis: 1500 })]);
    expect(s.startMs).toBe(1500);
  });

  it("falls back to creation when the turn never started", () => {
    const [s] = spansFromRuns([run({ startedAtMillis: null, createdAtMillis: 900 })]);
    expect(s.startMs).toBe(900);
  });

  it("leaves a live attempt open", () => {
    const [s] = spansFromRuns([run({ finishedAtMillis: null })]);
    expect(s.endMs).toBeNull();
  });

  it("lanes by agent and labels by node", () => {
    const [s] = spansFromRuns([run({ agentId: "verifier", nodeId: "check" })]);
    expect(s.lane).toBe("verifier");
    expect(s.label).toBe("check");
  });
});

describe("spansFromSteps", () => {
  it("anchors a completed tool span at its actual start", () => {
    // A completed call's `atMillis` is its end stamp — the completion row
    // rewrote the start row with a fresh timestamp — so the span runs
    // *backward* from there by the elapsed duration.
    const spans = spansFromSteps(run({ steps: [step({ atMillis: 350, elapsedMs: 250 })] }));
    expect(spans[0].startMs).toBe(100);
    expect(spans[0].endMs).toBe(350);
  });

  it("renders a step with no elapsed as a point", () => {
    // A thinking marker: we know when it happened, not how long it took.
    const spans = spansFromSteps(run({ steps: [step({ atMillis: 100, elapsedMs: null })] }));
    expect(spans[0].endMs).toBe(100);
  });
});

describe("stepState", () => {
  it("keeps an awaiting-approval step out of the failure tone", () => {
    expect(stepState(step({ status: "awaiting_approval" }))).toBe("blocked");
    expect(stepState(step({ status: "error" }))).toBe("failed");
    expect(stepState(step({ status: "running" }))).toBe("running");
    expect(stepState(step({ status: "ok" }))).toBe("done");
  });
});

describe("totals", () => {
  it("counts live steps rather than the settled total", () => {
    // `stepCount` is null while an attempt is live; treating that as zero would
    // under-report exactly the run somebody is watching.
    const live = run({ stepCount: null, steps: [step(), step({ seq: 1 })] });
    expect(totals([live], NOW).steps).toBe(2);
  });

  it("does not add cached input to the token total", () => {
    // `cachedInputTokens` is a subset of `inputTokens` (providers report it
    // as prompt_tokens_details.cached_tokens), so the total is input + output
    // and cached rides as its own diagnostic column.
    const t = totals([run()], NOW);
    expect(t.tokens).toBe(15);
  });

  it("runs the clock to now for an unfinished attempt", () => {
    const t = totals([run({ startedAtMillis: 1000, finishedAtMillis: null })], NOW);
    expect(t.elapsedMs).toBe(NOW - 1000);
  });

  it("counts distinct agents", () => {
    const t = totals([run({ agentId: "a" }), run({ id: "r2", agentId: "b" })], NOW);
    expect(t.agents).toBe(2);
    expect(t.attempts).toBe(2);
  });

  it("is all zeros for no attempts rather than NaN", () => {
    const t = totals([], NOW);
    expect(t).toMatchObject({ agents: 0, attempts: 0, steps: 0, elapsedMs: 0 });
    expect(Number.isFinite(t.costUsd)).toBe(true);
  });
});

describe("byAgent", () => {
  it("sums an agent's attempts and orders heaviest first", () => {
    const rows = byAgent([
      run({ id: "a", agentId: "light", usage: { inputTokens: 1, outputTokens: 1, cachedInputTokens: 0, costUsd: 0 } }),
      run({ id: "b", agentId: "heavy", usage: { inputTokens: 100, outputTokens: 50, cachedInputTokens: 0, costUsd: 1 } }),
      run({ id: "c", agentId: "heavy", usage: { inputTokens: 10, outputTokens: 5, cachedInputTokens: 0, costUsd: 0.1 } }),
    ]);
    expect(rows[0].agentId).toBe("heavy");
    expect(rows[0].attempts).toBe(2);
    expect(rows[0].inputTokens).toBe(110);
    expect(rows[1].agentId).toBe("light");
  });

  it("counts failures per agent", () => {
    const rows = byAgent([run({ status: "failed" }), run({ id: "r2" })]);
    expect(rows[0].failures).toBe(1);
  });

  it("is empty for no attempts", () => {
    expect(byAgent([])).toEqual([]);
  });
});

describe("byNode", () => {
  it("keeps blocked and failed apart", () => {
    const rows = byNode([
      run({ id: "a", nodeId: "check", status: "failed" }),
      run({ id: "b", nodeId: "check", status: "waiting_approval" }),
      run({ id: "c", nodeId: "check", status: "succeeded" }),
    ]);
    expect(rows[0]).toEqual({
      nodeId: "check",
      succeeded: 1,
      failed: 1,
      blocked: 1,
      declined: 0,
    });
  });

  it("keeps a declined refusal out of the succeeded column (issue #1809)", () => {
    const rows = byNode([
      run({ id: "a", nodeId: "gate", status: "declined" }),
      run({ id: "b", nodeId: "gate", status: "succeeded" }),
    ]);
    expect(rows[0]).toEqual({
      nodeId: "gate",
      succeeded: 1,
      failed: 0,
      blocked: 0,
      declined: 1,
    });
  });

  it("ranks the node that stops runs most, first", () => {
    const rows = byNode([
      run({ id: "a", nodeId: "fine", status: "succeeded" }),
      run({ id: "b", nodeId: "bad", status: "failed" }),
      run({ id: "c", nodeId: "bad", status: "failed" }),
    ]);
    expect(rows.map((r) => r.nodeId)).toEqual(["bad", "fine"]);
  });

  it("ignores attempts that belong to no node", () => {
    expect(byNode([run({ nodeId: null })])).toEqual([]);
  });
});

describe("failureHistogram", () => {
  it("counts each class, commonest first", () => {
    const rows = failureHistogram([
      run({
        steps: [
          step({ seq: 0, failure: "timeout" }),
          step({ seq: 1, failure: "declined" }),
          step({ seq: 2, failure: "timeout" }),
          step({ seq: 3, failure: null }),
        ],
      }),
    ]);
    expect(rows).toEqual([
      { failure: "timeout", n: 2 },
      { failure: "declined", n: 1 },
    ]);
  });

  it("is empty when nothing failed", () => {
    expect(failureHistogram([run()])).toEqual([]);
  });
});
