import { describe, expect, it } from "vitest";

import type { CompanyStreamEvent } from "@/hooks/use-events";

/**
 * CodeRabbit review on PR #1882 (comment 3877648295): `startedBy` was typed
 * as a required member of the `workflow_run_started` frame, but the server
 * only inserts the `startedBy` key `if let Some(started_by)`
 * (`project_event_for_viewer` in `src/server/operator.rs`) — a run journaled
 * before issue #1862's prerequisite landed, or any producer that genuinely
 * has no sender, projects with the key absent, not `null`. The SSE decoder
 * casts parsed JSON straight to `CompanyStreamEvent`
 * (`frontend/src/hooks/use-events.ts`), so a required `startedBy` lets a
 * consumer dereference an `undefined` value on exactly that legacy frame.
 *
 * This is a type-level regression, proven the same way
 * `workflow-run-status-legend.test.ts` proves its own CodeRabbit finding:
 * `noUnusedLocals` turns a spent `@ts-expect-error` into a hard compile
 * error, so `npm run typecheck:unit` goes red the moment `startedBy` widens
 * back to a required member.
 */
type WorkflowRunStarted = Extract<
  CompanyStreamEvent,
  { type: "workflow_run_started" }
>;

describe("a workflow_run_started frame the server legitimately omits startedBy from", () => {
  it("still typechecks as a WorkflowRunStarted frame", () => {
    const legacyFrame: WorkflowRunStarted = {
      type: "workflow_run_started",
      seq: 1,
      atMillis: 0,
      workflowId: "wf",
      runId: "run-1",
      scheduled: false,
      // No `startedBy` — the server omits the key entirely for a run
      // journaled before this field existed, or one with no sender at all
      // (see `project_event_for_viewer`'s `if let Some(started_by)` guard).
    };

    expect(legacyFrame.startedBy).toBeUndefined();
  });
});
