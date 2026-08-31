import { describe, expect, it } from "vitest";

import type { ApprovalSummary } from "@/api/types";
import {
  approvalsForTask,
  blockingTaskApprovals,
  decidingForTask,
  taskApprovalBlock,
  taskApprovalRows,
  taskApprovalVerdicts,
} from "@/lib/task-approvals";
import type { Verdict } from "@/api/types";

/**
 * What a paused card is blocked on (issue #883).
 *
 * The board reads `…/tasks`, whose card projection carries no approvals, so
 * before this a paused card could only show a Resume button and no reason —
 * and Resume is the wrong click from that state: the turn continues on its own
 * when the last decision it parked lands (#469), so re-dispatching re-runs the
 * work and parks the same calls again.
 *
 * These pin the join the card is derived from. Each case below is a way the
 * card could look completely normal while saying something false, which is why
 * they are here rather than left to the rendered board.
 */

const T0 = new Date("2026-03-02T10:00:00Z").getTime();

function approval(
  id: string,
  at: number,
  task: ApprovalSummary["task"],
): ApprovalSummary {
  return {
    id,
    kind: "web_fetch",
    amount_usd: null,
    at_millis: at,
    agent: "seo",
    task,
    payload: { url: `https://example.com/${id}` },
  };
}

const MINE = (id: string, at: number) => approval(id, at, { link: "task", id: "task-1" });
const THEIRS = (id: string, at: number) => approval(id, at, { link: "task", id: "task-2" });

describe("approvalsForTask", () => {
  it("takes only the approvals whose park named this card", () => {
    const feed = [MINE("a1", T0), THEIRS("b1", T0), MINE("a2", T0 + 1_000)];
    expect(approvalsForTask(feed, "task-1").map((a) => a.id)).toEqual(["a1", "a2"]);
  });

  /**
   * `{link: "unlinked"}` is a park the runtime performed for no card — a
   * workflow delivery, an operator-chat turn, a scheduler tick. Counting one
   * would put "blocked on 1 approval" on a card that is not blocked at all, and
   * then disable its Resume button forever, since deciding that approval would
   * never be something the operator connects to this card.
   */
  it("ignores an approval that belongs to no card", () => {
    const feed = [approval("u1", T0, { link: "unlinked" }), MINE("a1", T0)];
    expect(approvalsForTask(feed, "task-1").map((a) => a.id)).toEqual(["a1"]);
  });

  /**
   * An absent link is a park written before #333 stamped one. The host keeps a
   * run-window heuristic for exactly this case; the board has no window to
   * apply it against, so it must skip rather than guess — attributing an
   * unrecorded park to whichever card happened to be read would block a card
   * for a reason that is not its own.
   */
  it("ignores an approval whose park recorded no link at all", () => {
    const feed = [approval("old", T0, undefined), MINE("a1", T0)];
    expect(approvalsForTask(feed, "task-1").map((a) => a.id)).toEqual(["a1"]);
  });
});

describe("taskApprovalBlock", () => {
  it("is null when nothing in the queue names this card", () => {
    expect(taskApprovalBlock([THEIRS("b1", T0)], "task-1")).toBeNull();
    expect(taskApprovalBlock([], "task-1")).toBeNull();
  });

  it("counts every approval parked for this card", () => {
    const feed = [MINE("a1", T0), THEIRS("b1", T0), MINE("a2", T0), MINE("a3", T0)];
    expect(taskApprovalBlock(feed, "task-1")?.count).toBe(3);
  });

  /**
   * The oldest park, not the newest — the thing the card reports is how long it
   * has really been stopped. Taking the newest would reset a climbing clock
   * every time a second effect parked behind the first, so a card wedged for an
   * hour would read as freshly blocked and nothing about it would look wrong.
   */
  it("anchors the wait to the oldest park, whatever order the feed is in", () => {
    const feed = [MINE("late", T0 + 600_000), MINE("early", T0), MINE("mid", T0 + 60_000)];
    expect(taskApprovalBlock(feed, "task-1")?.since).toBe(T0);
  });

  /**
   * Oldest first, because the card names the *first* blocked call when there is
   * only one to name and the detail row reads the same list. An order that
   * varied with the host's response order would make the card's sentence change
   * between two polls that carry identical facts.
   */
  it("orders the approvals oldest park first", () => {
    const feed = [MINE("late", T0 + 600_000), MINE("early", T0), MINE("mid", T0 + 60_000)];
    expect(taskApprovalBlock(feed, "task-1")?.approvals.map((a) => a.id)).toEqual([
      "early",
      "mid",
      "late",
    ]);
  });
});

/**
 * The same join, plus what has become of each row (#1891).
 *
 * The card decides now, so it needs more than "is this blocked": it needs the
 * *set* it is deciding, and it needs a row the operator has just settled to
 * read as settled rather than offer its buttons a second time. That gap — the
 * resolve's answer and the feed's next poll are two moments — is what these
 * pin, and it is invisible on a rendered card that is polling every four
 * seconds.
 */
describe("taskApprovalRows", () => {
  it("carries this card's parked approvals, oldest park first, undecided", () => {
    const feed = [MINE("a2", T0 + 1_000), THEIRS("b1", T0), MINE("a1", T0)];
    const rows = taskApprovalRows(feed, {}, "task-1");
    expect(rows.map((r) => r.approval.id)).toEqual(["a1", "a2"]);
    expect(rows.every((r) => r.verdict === null)).toBe(true);
  });

  /** Two parks in the same millisecond still order the same way on every poll:
   *  a list that reordered under a pointer mid-click would be its own bug. */
  it("breaks a tie on id, so the order survives a refresh", () => {
    const rows = taskApprovalRows([MINE("b", T0), MINE("a", T0)], {}, "task-1");
    expect(rows.map((r) => r.approval.id)).toEqual(["a", "b"]);
  });

  /**
   * The live queue is the only source of rows (#1895 review). The shell holds
   * `decided` for the whole company session, so a card that is re-dispatched
   * and parks one new approval next week must not find last week's verdicts
   * folded back in beside it — `ApprovalRow` would report "3 of 4 decided"
   * over a batch of one, and the count would grow with every repeat run.
   *
   * This is where the run drawer's shape does not transfer: a workflow run is
   * one-shot, so every verdict witnessed for it belongs to it. A task is not.
   */
  it("does not resurrect a verdict the queue has already dropped", () => {
    const a1 = MINE("a1", T0);
    const rows = taskApprovalRows([], { a1: { verdict: "approve", approval: a1 } }, "task-1");
    expect(rows).toEqual([]);
  });

  /**
   * The bound that keeps a settling batch whole (#1895 review). The board's one
   * Approve resolves each id separately and refreshes the feed on each, so a
   * partial failure leaves the successes out of the queue — and a live-only
   * projection would collapse to a single-item card reading "Not recorded",
   * after the operator had authorised two effects.
   */
  it("keeps a decided sibling while its batch is still parked", () => {
    const a1 = { ...MINE("a1", T0), batch: "turn-1" };
    const a2 = { ...MINE("a2", T0 + 1_000), batch: "turn-1" };
    const rows = taskApprovalRows([a2], { a1: { verdict: "approve", approval: a1 } }, "task-1");
    expect(rows.map((r) => [r.approval.id, r.verdict])).toEqual([
      ["a1", "approve"],
      ["a2", null],
    ]);
  });

  /** …and releases it once the host has taken the last of that batch. */
  it("drops the batch entirely when nothing of it is left in the queue", () => {
    const a1 = { ...MINE("a1", T0), batch: "turn-1" };
    const rows = taskApprovalRows([], { a1: { verdict: "approve", approval: a1 } }, "task-1");
    expect(rows).toEqual([]);
  });

  /** A new turn is a new batch, so a re-dispatch cannot reach the old one. */
  it("does not let a new batch pick up the previous run's verdicts", () => {
    const old = { ...MINE("old-1", T0), batch: "turn-1" };
    const fresh = { ...MINE("fresh", T0 + 900_000), batch: "turn-2" };
    const rows = taskApprovalRows([fresh], { "old-1": { verdict: "approve", approval: old } }, "task-1");
    expect(rows.map((r) => r.approval.id)).toEqual(["fresh"]);
  });

  /** No batch key, no re-attachment — the safe direction on an older host. */
  it("never re-attaches an approval that carries no batch", () => {
    const a1 = MINE("a1", T0);
    const a2 = { ...MINE("a2", T0 + 1_000), batch: "turn-1" };
    const rows = taskApprovalRows([a2], { a1: { verdict: "approve", approval: a1 } }, "task-1");
    expect(rows.map((r) => r.approval.id)).toEqual(["a2"]);
  });

  it("does not let a re-dispatched card inherit its own earlier verdicts", () => {
    const old1 = MINE("old-1", T0);
    const old2 = MINE("old-2", T0 + 1_000);
    const fresh = MINE("fresh", T0 + 900_000);
    const rows = taskApprovalRows(
      [fresh],
      {
        "old-1": { verdict: "approve", approval: old1 },
        "old-2": { verdict: "deny", approval: old2 },
      },
      "task-1",
    );
    expect(rows.map((r) => r.approval.id)).toEqual(["fresh"]);
    expect(taskApprovalVerdicts(rows)).toEqual({});
  });

  /** `decided` is console-wide. Another card's settled row is not this card's
   *  business, and folding it in would inflate this card's total. */
  it("ignores a decision taken on another card's approval", () => {
    const b1 = THEIRS("b1", T0);
    const rows = taskApprovalRows([], { b1: { verdict: "deny", approval: b1 } }, "task-1");
    expect(rows).toEqual([]);
  });

  /**
   * What the annotation is actually for: between the resolve's answer and the
   * feed's next poll the queue still holds a row that has already been decided,
   * and offering its buttons again there would invite a second decision on a
   * settled request.
   */
  it("marks a still-queued row with the verdict this console witnessed", () => {
    const a1 = MINE("a1", T0);
    const rows = taskApprovalRows([a1], { a1: { verdict: "deny", approval: a1 } }, "task-1");
    expect(rows[0].verdict).toBe("deny");
    expect(blockingTaskApprovals(rows)).toEqual([]);
    // Still a row, though — which is what keeps Resume down while the host is
    // only just starting the continuation the verdict released.
    expect(rows).toHaveLength(1);
  });
});

describe("taskApprovalVerdicts", () => {
  it("maps only the rows that have one", () => {
    const a1 = MINE("a1", T0);
    const rows = taskApprovalRows(
      [a1, MINE("a2", T0 + 1_000)],
      { a1: { verdict: "approve", approval: a1 } },
      "task-1",
    );
    expect(taskApprovalVerdicts(rows)).toEqual({ a1: "approve" });
  });
});

describe("decidingForTask", () => {
  /**
   * The shell's map covers every surface that resolves. `ApprovalRow` reads
   * `size > 0` as "I am busy", so handing it through whole would grey out every
   * blocked card on the board the moment one of them was clicked (#373, one
   * surface over).
   */
  it("keeps only the in-flight decisions belonging to this card", () => {
    const rows = taskApprovalRows([MINE("a1", T0), MINE("a2", T0 + 1_000)], {}, "task-1");
    const deciding = new Map<string, Verdict>([
      ["a1", "approve"],
      ["b1", "deny"],
    ]);
    expect([...decidingForTask(rows, deciding)]).toEqual([["a1", "approve"]]);
  });

  it("is empty when the console is deciding nothing of this card's", () => {
    const rows = taskApprovalRows([MINE("a1", T0)], {}, "task-1");
    expect(decidingForTask(rows, new Map([["b1", "deny"]])).size).toBe(0);
  });
});
