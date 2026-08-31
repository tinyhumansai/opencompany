import { describe, expect, it } from "vitest";

import type { WorkflowRunOutcome } from "@/api/workflows";
import { VERDICT_TONE, runTone, verdictOf } from "@/views/workflows/run-health";

/**
 * Issue #1865: the console's newest verdict — a run whose node errored under
 * `on_error: continue|route`, or whose agent turn truncated at the iteration
 * cap, kept going and settled with nothing else wrong. Before this the host
 * never sent this word, so the console's own reading of such a run reached
 * "ok" the same way `WorkflowRunVerdict::of` did before its own `degraded`
 * arm — the false-success half of the issue, mirrored client-side.
 *
 * The host is the source of truth for `degraded` when it sends one, so most
 * of these tests pin the READING side: once `run.verdict === "degraded"`
 * arrives, the console must render it as its own amber tone, not fold it
 * into `ok`.
 *
 * A prior version of this comment claimed there is no client-side fallback
 * ladder for `degraded` at all, unlike the older verdicts. That was wrong
 * (CodeRabbit review, PR #1883): a host old enough to predate #981's
 * `verdict` field entirely sends no `verdict`, so `verdictOf` falls all the
 * way through its pre-#981 ladder — and that ladder, before this fix, had no
 * arm reading `nodes[].status`, so a settled run with an errored
 * `on_error: continue|route` node and no higher-precedence condition still
 * landed on `ok`. `verdictOf`'s fallback ladder now carries its own
 * `degraded` arm, mirroring the host's `errored_nodes` fact off
 * `run.nodes[].status === "error"` — the last test below pins that.
 */

function baseRun(over: Partial<WorkflowRunOutcome> = {}): WorkflowRunOutcome {
  return {
    seq: 1,
    atMillis: 1_700_000_000_000,
    workflowId: "daily_digest",
    scheduled: false,
    deliveries: [],
    pendingApprovals: [],
    ...over,
  };
}

describe("the degraded verdict", () => {
  it("is a token VERDICT_TONE recognises, with its own amber dot", () => {
    expect(VERDICT_TONE.degraded).toBeDefined();
    expect(VERDICT_TONE.degraded.label).toBe("degraded");
    // Amber — the same shape `blocked`/`awaiting-approval` wear, not the red
    // `failed`/`undelivered` share. The run's own config asked for the
    // branch to survive the error, and it did.
    expect(VERDICT_TONE.degraded.dot).toBe(VERDICT_TONE.blocked.dot);
    expect(VERDICT_TONE.degraded.dot).not.toBe(VERDICT_TONE.failed.dot);
  });

  it("verdictOf trusts the host's word rather than folding it into ok", () => {
    const run = baseRun({ verdict: "degraded" });
    expect(verdictOf(run)).toBe("degraded");
    expect(runTone(run)).toEqual(VERDICT_TONE.degraded);
  });

  it("does not shadow a host word it does not recognise, so a genuinely ok run stays ok", () => {
    const run = baseRun({ verdict: "ok" });
    expect(verdictOf(run)).toBe("ok");
  });

  it("a pre-#981 host with no verdict field still reads degraded off an errored node, not ok (PR #1883)", () => {
    // No `verdict` at all — the shape a host predating #981 sends, forcing
    // `verdictOf` down its own fallback ladder instead of trusting the host's
    // word.
    const run = baseRun({
      nodes: [
        { nodeId: "fetch", status: "ok", elapsedMs: 40 },
        { nodeId: "notify", status: "error", elapsedMs: 12 },
      ],
    });
    expect(verdictOf(run)).toBe("degraded");
    expect(runTone(run)).toEqual(VERDICT_TONE.degraded);
  });

  it("a pre-#981 host's fallback still ranks a hard failure above a soft node error", () => {
    // Same errored node as above, but the run itself also carries `error` —
    // the more specific fact must win, exactly as it does on the host.
    const run = baseRun({
      error: "the graph broke",
      nodes: [{ nodeId: "notify", status: "error", elapsedMs: 12 }],
    });
    expect(verdictOf(run)).toBe("failed");
  });

  it("a pre-#981 host's fallback does not call a node blocked on a person degraded", () => {
    // A genuinely blocked node reports `status: "blocked"`, never `"error"` —
    // this pins that the new arm only fires on `"error"` rows, so it cannot
    // steal a blocked run's own verdict.
    const run = baseRun({
      nodes: [{ nodeId: "approve-me", status: "blocked", elapsedMs: 5 }],
    });
    expect(verdictOf(run)).not.toBe("degraded");
  });
});
