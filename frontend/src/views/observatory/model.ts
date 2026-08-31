/**
 * Turning attempts into the things the lenses draw.
 *
 * Pure, and separate from both the API module and the components, so the
 * decisions worth arguing about — what counts as a failure, what an unfinished
 * attempt contributes to a total — are testable and stated once.
 */

import type { ObservatoryRun, ObservatoryStep } from "@/api/observatory";
import type { Span, SpanState } from "./waterfall";

/** How an attempt's status paints. */
export function runState(run: ObservatoryRun): SpanState {
  switch (run.status) {
    case "succeeded":
      return "done";
    // A by-design decline (issue #1809) is a clean terminal outcome, not a
    // failure and not still in flight — but it is not a success either, so it
    // must not paint "done"'s green. `RunTimeline` and `AgentRuns` already
    // give `declined` the same neutral tone `cancelled` wears; here that is
    // "idle", the closed vocabulary's own word for "nothing is happening and
    // nothing went wrong" (the tone `stopped`/`stranded` wear in
    // `run-health.ts`) — never red, and never the `default` "running" that
    // would leave a settled attempt reading as live forever.
    case "declined":
      return "idle";
    case "failed":
    case "cancelled":
      return "failed";
    case "waiting_approval":
    case "paused":
    case "blocked":
      // Blocked, not failed: a person still has to decide, and nothing broke.
      // The distinction is the whole of issue #411 one surface over.
      return "blocked";
    default:
      return "running";
  }
}

/**
 * One span per attempt — the default depth.
 *
 * `startedAtMillis` rather than `createdAtMillis`: a row is minted before its
 * turn begins, and drawing the gap between the two as work would overstate
 * every agent's share of the run.
 */
export function spansFromRuns(runs: ObservatoryRun[]): Span[] {
  return runs.map((run) => ({
    id: run.id,
    lane: run.agentId,
    startMs: run.startedAtMillis ?? run.createdAtMillis,
    endMs: run.finishedAtMillis,
    state: runState(run),
    label: run.nodeId ?? run.agentId,
  }));
}

/**
 * One span per **step**, for the deeper zoom.
 *
 * A completed tool call's `atMillis` is stamped when its completion row was
 * written — the call's **end**, not its start — so the span runs *backward*
 * from there by `elapsedMs`. A step with no elapsed (a thinking marker, or a
 * call still running) carries its start stamp and becomes a point — which is
 * honest: we know when it happened and not how long it took.
 */
export function spansFromSteps(run: ObservatoryRun): Span[] {
  return run.steps.map((step) => {
    const elapsed = step.elapsedMs ?? 0;
    return {
      id: `${run.id}#${step.seq}`,
      lane: run.agentId,
      startMs: elapsed > 0 ? step.atMillis - elapsed : step.atMillis,
      endMs: step.atMillis,
      state: stepState(step),
      label: step.label,
    };
  });
}

/** How one step paints. */
export function stepState(step: ObservatoryStep): SpanState {
  switch (step.status) {
    case "error":
      return "failed";
    case "awaiting_approval":
      return "blocked";
    case "running":
      return "running";
    default:
      return "done";
  }
}

/** What a run cost and how much of it there was. */
export interface RunTotals {
  agents: number;
  attempts: number;
  steps: number;
  tokens: number;
  costUsd: number;
  /** Wall-clock from the first start to the last finish, or to `now`. */
  elapsedMs: number;
  /** How many attempts overlapped at the busiest moment. */
  peakConcurrency: number;
}

/** Folds a run's attempts into the header's numbers. */
export function totals(runs: ObservatoryRun[], nowMs: number): RunTotals {
  const agents = new Set(runs.map((r) => r.agentId));
  let tokens = 0;
  let costUsd = 0;
  let steps = 0;
  let startMs = Infinity;
  let endMs = -Infinity;
  for (const run of runs) {
    // `cachedInput` is a breakdown of `input` (providers report it as
    // `prompt_tokens_details.cached_tokens`, a subset of `prompt_tokens`), so
    // adding it again would inflate cache-heavy runs. The canonical metering
    // aggregate totals input + output only; cached rides along as its own
    // diagnostic column.
    tokens += run.usage.inputTokens + run.usage.outputTokens;
    costUsd += run.usage.costUsd;
    // `steps.length`, never `stepCount`: the settled total is null while an
    // attempt is live, and treating that as zero would under-report exactly the
    // run somebody is watching.
    steps += run.steps.length;
    startMs = Math.min(startMs, run.startedAtMillis ?? run.createdAtMillis);
    endMs = Math.max(endMs, run.finishedAtMillis ?? nowMs);
  }
  return {
    agents: agents.size,
    attempts: runs.length,
    steps,
    tokens,
    costUsd,
    elapsedMs: runs.length === 0 ? 0 : Math.max(0, endMs - startMs),
    peakConcurrency: 0,
  };
}

/** One agent's share of a run, for the analytics lens. */
export interface AgentShare {
  agentId: string;
  attempts: number;
  steps: number;
  inputTokens: number;
  outputTokens: number;
  cachedInputTokens: number;
  costUsd: number;
  failures: number;
}

/** Per-agent totals, heaviest first. */
export function byAgent(runs: ObservatoryRun[]): AgentShare[] {
  const map = new Map<string, AgentShare>();
  for (const run of runs) {
    const share = map.get(run.agentId) ?? {
      agentId: run.agentId,
      attempts: 0,
      steps: 0,
      inputTokens: 0,
      outputTokens: 0,
      cachedInputTokens: 0,
      costUsd: 0,
      failures: 0,
    };
    share.attempts += 1;
    share.steps += run.steps.length;
    share.inputTokens += run.usage.inputTokens;
    share.outputTokens += run.usage.outputTokens;
    share.cachedInputTokens += run.usage.cachedInputTokens;
    share.costUsd += run.usage.costUsd;
    if (run.status === "failed") share.failures += 1;
    map.set(run.agentId, share);
  }
  return [...map.values()].sort(
    (a, b) =>
      b.inputTokens + b.outputTokens - (a.inputTokens + a.outputTokens) ||
      a.agentId.localeCompare(b.agentId),
  );
}

/** Where attempts stop, for the analytics lens. */
export interface NodeOutcome {
  nodeId: string;
  succeeded: number;
  failed: number;
  blocked: number;
  /** By-design refusals (issue #1809) — never folded into `succeeded`. */
  declined: number;
}

/**
 * Per-node outcomes, worst first.
 *
 * `blocked` is counted apart from `failed` deliberately: a node waiting on a
 * person has not gone wrong, and folding the two would send an operator hunting
 * a bug in the node that most often needs a click. `declined` is counted apart
 * from `succeeded` for the same reason in reverse (issue #1809): a node the
 * compiler refused to automate has not succeeded, and folding it in would tell
 * an operator a gate is healthy when it is actually the one being declined.
 */
export function byNode(runs: ObservatoryRun[]): NodeOutcome[] {
  const map = new Map<string, NodeOutcome>();
  for (const run of runs) {
    const nodeId = run.nodeId;
    if (!nodeId) continue;
    const row = map.get(nodeId) ?? {
      nodeId,
      succeeded: 0,
      failed: 0,
      blocked: 0,
      declined: 0,
    };
    const state = runState(run);
    if (state === "failed") row.failed += 1;
    else if (state === "blocked") row.blocked += 1;
    else if (state === "idle") row.declined += 1;
    else if (state === "done") row.succeeded += 1;
    map.set(nodeId, row);
  }
  return [...map.values()].sort(
    (a, b) =>
      b.failed + b.blocked - (a.failed + a.blocked) || a.nodeId.localeCompare(b.nodeId),
  );
}

/** How often each failure class appeared across a set of attempts. */
export function failureHistogram(runs: ObservatoryRun[]): { failure: string; n: number }[] {
  const map = new Map<string, number>();
  for (const run of runs) {
    for (const step of run.steps) {
      if (!step.failure) continue;
      map.set(step.failure, (map.get(step.failure) ?? 0) + 1);
    }
  }
  return [...map.entries()]
    .map(([failure, n]) => ({ failure, n }))
    .sort((a, b) => b.n - a.n || a.failure.localeCompare(b.failure));
}

/** Groups attempts by the workflow run that spawned them, newest first. */
export function byWorkflowRun(runs: ObservatoryRun[]): Map<string, ObservatoryRun[]> {
  const map = new Map<string, ObservatoryRun[]>();
  for (const run of runs) {
    const key = run.workflowRunId;
    if (!key) continue;
    const bucket = map.get(key);
    if (bucket) bucket.push(run);
    else map.set(key, [run]);
  }
  return map;
}
