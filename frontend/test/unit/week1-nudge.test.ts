import { describe, expect, it } from "vitest";

import type { NotificationDto } from "@/api/types";
import { WEEK1_NUDGE_KIND, pickActiveNudge } from "@/lib/week1-nudge";

/**
 * Issue #1845: the week-1 "save your first workflow" nudge banner is a dumb
 * renderer over whatever `pickActiveNudge` says — see that module's own docs
 * for why a read row must never be picked, on this call or any later one
 * (that "any later one" clause is the proxy this file uses for "mark-read
 * persists across reload": a reload is just this same function called again
 * over the host's updated feed).
 */

const note = (over: Partial<NotificationDto> & Pick<NotificationDto, "id">): NotificationDto => ({
  kind: WEEK1_NUDGE_KIND,
  subjectKind: "workflow",
  subjectId: "week1-first-workflow",
  title: "Save your first workflow",
  createdAt: 1,
  ...over,
});

describe("pickActiveNudge", () => {
  it("picks the unread row when there is exactly one", () => {
    expect(pickActiveNudge([note({ id: "a" })])).toEqual(note({ id: "a" }));
  });

  it("returns null when the feed is empty", () => {
    expect(pickActiveNudge([])).toBeNull();
  });

  /**
   * The reload proxy: a row the host has already marked read (`readAt` set)
   * must never come back as the active nudge — not on the call that first
   * sees it read, and not on any later one either, which is exactly what "a
   * dismissed/completed nudge does not resurrect after a reload" means at
   * this layer. The wiring that turns a page reload into a fresh call is
   * `WorkflowsView`'s own `refreshNudge` effect, proven in
   * `test/e2e/workflow-week1-nudge.spec.ts`; this is the decision that call
   * lands on.
   */
  it("never picks a row that has already been read", () => {
    expect(pickActiveNudge([note({ id: "a", readAt: 5 })])).toBeNull();
  });

  it("ignores read rows even when an unread one also exists", () => {
    expect(
      pickActiveNudge([note({ id: "read", readAt: 5 }), note({ id: "unread" })]),
    ).toEqual(note({ id: "unread" }));
  });

  /**
   * `LifecycleScheduler` files at most one nudge per user (its own
   * idempotency ledger), but this function does not lean on that holding
   * forever on the wire — defensive against a future multi-touch nudge or a
   * stale duplicate, the newest unread row is the one answer that stays sane
   * either way.
   */
  it("picks the newest of several unread rows", () => {
    expect(
      pickActiveNudge([
        note({ id: "old", createdAt: 1 }),
        note({ id: "new", createdAt: 99 }),
        note({ id: "middle", createdAt: 50 }),
      ]),
    ).toEqual(note({ id: "new", createdAt: 99 }));
  });

  it("ignores a notification of a different kind entirely", () => {
    // Defensive: the console always requests `?kind=workflow_nudge`, but a
    // helper this small should not trust its caller never to hand it a mixed
    // feed (a future consumer, a test fixture reused sloppily).
    expect(pickActiveNudge([note({ id: "a", kind: "mention" })])).toBeNull();
  });

  it("picks the right row out of a mixed-kind feed", () => {
    expect(
      pickActiveNudge([note({ id: "a", kind: "mention" }), note({ id: "b" })]),
    ).toEqual(note({ id: "b" }));
  });
});
