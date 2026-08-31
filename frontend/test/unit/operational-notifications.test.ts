import { describe, expect, it } from "vitest";

import type { NotificationDto } from "@/api/types";
import {
  flushPendingAcknowledgements,
  isOperationalNotification,
  operationalNotificationSeverity,
  operationalNotificationsToAnnounce,
  scheduleAcknowledgement,
  type PendingAcknowledgement,
} from "@/lib/operational-notifications";

/**
 * `mentionCountsByChannel` / `mentionsToClear` / `threadsToReReadForMentions`
 * (see `chat-mention-badge.test.ts`) all filter to `kind === "mention"` by
 * design, which left `dispatch_failed` / `approval_expired` /
 * `workflow_run_*` rows with no rendering and no acknowledgement path even
 * though `GET /notifications` returns them (Codex #1883 P1). These tests
 * pin the pure logic behind the toast-based fix.
 */

const note = (over: Partial<NotificationDto> & Pick<NotificationDto, "id" | "kind">): NotificationDto => ({
  subjectKind: "task",
  subjectId: "t-1",
  title: "A card's dispatch failed and returned to To-do: boom",
  createdAt: 1,
  ...over,
});

describe("isOperationalNotification", () => {
  it("is false for mentions", () => {
    expect(isOperationalNotification(note({ id: "a", kind: "mention" }))).toBe(false);
  });

  it("is true for every non-mention kind the runtime writes", () => {
    for (const kind of [
      "dispatch_failed",
      "approval_expired",
      "workflow_run_failed",
      "workflow_run_stranded",
      "workflow_run_blocked",
    ]) {
      expect(isOperationalNotification(note({ id: kind, kind }))).toBe(true);
    }
  });

  /**
   * PR #1878 review (comment 3893066248): `notifications()` retired its
   * server-side kind allowlist in favour of a mixed feed with client-side
   * filtering — the same design #1883's toast+ack consumer relies on. That
   * makes `workflow_nudge` (issue #1845's week-1 nudge) just another
   * non-mention row on the exact feed `app-shell` polls unfiltered, so
   * without an explicit carve-out it reads as operational: toasted as a
   * generic warning and marked read the moment the tab is visible, before
   * `WorkflowsView`'s `pickActiveNudge` ever gets a chance to show the
   * banner. The nudge must never be auto-acknowledged by this classifier.
   */
  it("is false for the week-1 nudge — it has its own banner, not a toast+ack", () => {
    expect(isOperationalNotification(note({ id: "a", kind: "workflow_nudge" }))).toBe(false);
  });
});

describe("operationalNotificationsToAnnounce", () => {
  it("returns unread operational rows not already announced", () => {
    const rows = [
      note({ id: "a", kind: "dispatch_failed" }),
      note({ id: "b", kind: "mention" }),
      note({ id: "c", kind: "approval_expired" }),
    ];
    expect(operationalNotificationsToAnnounce(rows, new Set()).map((n) => n.id)).toEqual([
      "a",
      "c",
    ]);
  });

  it("excludes rows already read", () => {
    const rows = [note({ id: "a", kind: "dispatch_failed", readAt: 5 })];
    expect(operationalNotificationsToAnnounce(rows, new Set())).toEqual([]);
  });

  it("excludes rows already announced this session", () => {
    const rows = [note({ id: "a", kind: "dispatch_failed" })];
    expect(operationalNotificationsToAnnounce(rows, new Set(["a"]))).toEqual([]);
  });

  it("does not re-announce a row on the next poll once it is in the guard set", () => {
    const announced = new Set<string>();
    const rows = [note({ id: "a", kind: "dispatch_failed" })];
    const first = operationalNotificationsToAnnounce(rows, announced);
    expect(first.map((n) => n.id)).toEqual(["a"]);
    first.forEach((n) => announced.add(n.id));
    // The row is durable and keeps coming back on every poll until the
    // server marks it read — the guard, not the row disappearing, is what
    // stops the repeat toast.
    expect(operationalNotificationsToAnnounce(rows, announced)).toEqual([]);
  });

  /**
   * PR #1878 review (comment 3893066248) — the actual bug shape: app-shell's
   * poll of the unfiltered feed must never surface a `workflow_nudge` row as
   * something to toast-and-ack. If it did, `app-shell`'s handler would mark
   * it read server-side on the very next visible-tab poll, and
   * `pickActiveNudge` (`WorkflowsView`) would never see it unread again — the
   * banner this PR exists to ship would be silently defeated by a sibling
   * consumer of the same feed.
   */
  it("never includes a workflow_nudge row alongside operational rows from the same poll", () => {
    const rows = [
      note({ id: "a", kind: "dispatch_failed" }),
      note({ id: "b", kind: "workflow_nudge" }),
      note({ id: "c", kind: "mention" }),
    ];
    expect(operationalNotificationsToAnnounce(rows, new Set()).map((n) => n.id)).toEqual(["a"]);
  });
});

describe("operationalNotificationSeverity", () => {
  it("treats dispatch_failed as an error", () => {
    expect(operationalNotificationSeverity(note({ id: "a", kind: "dispatch_failed" }))).toBe(
      "error",
    );
  });

  it("treats every workflow_run_* kind as an error", () => {
    for (const kind of ["workflow_run_failed", "workflow_run_stranded", "workflow_run_blocked"]) {
      expect(operationalNotificationSeverity(note({ id: kind, kind }))).toBe("error");
    }
  });

  it("treats approval_expired as a warning", () => {
    expect(operationalNotificationSeverity(note({ id: "a", kind: "approval_expired" }))).toBe(
      "warning",
    );
  });

  it("defaults an unrecognized kind to warning rather than error", () => {
    expect(operationalNotificationSeverity(note({ id: "a", kind: "something_new" }))).toBe(
      "warning",
    );
  });
});

/**
 * `app-shell` toasts an operational row (sonner renders it, hidden tab or
 * not) the instant it is polled — but the previous revision of the toast+ack
 * fix marked the row read server-side at that same instant, regardless of
 * whether anyone could actually see the tab (Codex #1883 P2). A tab closed
 * or reloaded before it was ever brought to the foreground lost the
 * in-memory toast while the durable row already read as handled. These tests
 * pin the deferred-ack replacement: nothing is acknowledged while the tab is
 * hidden, and it flushes once — scoped to the right company — on return.
 */
describe("scheduleAcknowledgement", () => {
  it("acks immediately when the tab is visible", () => {
    const result = scheduleAcknowledgement(["a", "b"], "acme", false, []);
    expect(result.ackNow).toEqual(["a", "b"]);
    expect(result.pending).toEqual([]);
  });

  it("parks every id instead of acking when the tab is hidden", () => {
    const result = scheduleAcknowledgement(["a", "b"], "acme", true, []);
    expect(result.ackNow).toEqual([]);
    expect(result.pending).toEqual([
      { company: "acme", id: "a" },
      { company: "acme", id: "b" },
    ]);
  });

  it("accumulates onto whatever was already parked", () => {
    const already: PendingAcknowledgement[] = [{ company: "acme", id: "z" }];
    const result = scheduleAcknowledgement(["a"], "acme", true, already);
    expect(result.pending).toEqual([
      { company: "acme", id: "z" },
      { company: "acme", id: "a" },
    ]);
  });
});

describe("flushPendingAcknowledgements", () => {
  it("acks every id parked for the current company and clears them", () => {
    const pending: PendingAcknowledgement[] = [
      { company: "acme", id: "a" },
      { company: "acme", id: "b" },
    ];
    const result = flushPendingAcknowledgements("acme", pending);
    expect(result.ackNow.sort()).toEqual(["a", "b"]);
    expect(result.pending).toEqual([]);
  });

  it("leaves a different company's parked ids untouched", () => {
    const pending: PendingAcknowledgement[] = [
      { company: "acme", id: "a" },
      { company: "globex", id: "b" },
    ];
    const result = flushPendingAcknowledgements("acme", pending);
    expect(result.ackNow).toEqual(["a"]);
    expect(result.pending).toEqual([{ company: "globex", id: "b" }]);
  });

  it("is a no-op when nothing is parked", () => {
    const result = flushPendingAcknowledgements("acme", []);
    expect(result.ackNow).toEqual([]);
    expect(result.pending).toEqual([]);
  });
});

describe("the hidden-tab ack round trip does not reopen the once-per-poll bug", () => {
  it("parks under hidden, then flushes exactly those ids once visible — never re-toasting via the announced guard", () => {
    // Mirrors app-shell's actual sequence: toast (and mark `operationalAnnouncedRef`)
    // fires unconditionally; only the server ack is deferred.
    const announced = new Set<string>();
    let pending: PendingAcknowledgement[] = [];

    const rows = [note({ id: "a", kind: "dispatch_failed" })];
    const toAnnounce = operationalNotificationsToAnnounce(rows, announced);
    expect(toAnnounce.map((n) => n.id)).toEqual(["a"]);
    toAnnounce.forEach((n) => announced.add(n.id)); // toasted — the guard updates regardless of visibility

    const scheduled = scheduleAcknowledgement(["a"], "acme", /* documentHidden */ true, pending);
    pending = scheduled.pending;
    expect(scheduled.ackNow).toEqual([]); // not acked yet — tab is hidden

    // A poll tick fires again before the tab is ever seen: the durable row is
    // still unread server-side, so it comes back — but the announced guard
    // must keep it from toasting a second time.
    expect(operationalNotificationsToAnnounce(rows, announced)).toEqual([]);

    // The tab becomes visible.
    const flushed = flushPendingAcknowledgements("acme", pending);
    pending = flushed.pending;
    expect(flushed.ackNow).toEqual(["a"]);
    expect(pending).toEqual([]);
  });
});
