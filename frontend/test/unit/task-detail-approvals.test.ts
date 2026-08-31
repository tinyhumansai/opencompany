// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { TaskApproval } from "@/api/tasks";
import type { ApprovalSummary, GrantScope, Verdict } from "@/api/types";
import { AwaitingApprovalRow } from "@/views/TaskDetailView";
import type { DecidedApproval } from "@/views/chat/model";

/**
 * The task detail's blocked section decides, and says what it cannot decide
 * (#1891).
 *
 * The screen used to print one sentence and a link, so the operator who came
 * here to find out why a card was stuck had to leave to do anything about it.
 * It now itemises the card's parked approvals — one row each, unlike the board
 * card's single consolidated batch, because this is where a request is studied
 * and a turn that parked a fetch *and* a payment should be decidable one at a
 * time.
 *
 * The claim these exist for is the third one. This section reads **two**
 * sources that land separately: the host's own `approvals`, which decides
 * whether the card is waiting, and the company queue, which is what makes a
 * row decidable. For a poll they can disagree — four pending, three rows — and
 * a surface that quietly rendered the three would tell an operator they had
 * cleared a card that is still stopped. That is invisible on a screen polling
 * every few seconds and permanent in the operator's head.
 */

const T0 = new Date("2026-03-02T10:00:00Z").getTime();
const NOW = T0 + 300_000;

/** What the host's own task read says: whether the card is waiting, and on how many. */
function hostPending(id: string): TaskApproval {
  return {
    id,
    kind: "web_fetch",
    status: "pending",
    atMillis: T0,
  } as TaskApproval;
}

/** What the company queue carries: the payload, the deadline, the decidable id. */
function queued(id: string, over: Partial<ApprovalSummary> = {}): ApprovalSummary {
  return {
    id,
    kind: "web_fetch",
    amount_usd: null,
    at_millis: T0,
    agent: "qa",
    task: { link: "task", id: "task-1" },
    payload: { url: `https://example.com/${id}` },
    ...over,
  };
}

interface Decision {
  id: string;
  verdict: Verdict;
}

let container: HTMLDivElement;
let root: Root;
let decisions: Decision[];

async function render(
  approvals: TaskApproval[],
  parked: ApprovalSummary[],
  {
    decided = {},
    deciding = new Map<string, Verdict>(),
    canDecide = true,
  }: {
    decided?: Record<string, DecidedApproval>;
    deciding?: ReadonlyMap<string, Verdict>;
    canDecide?: boolean;
  } = {},
) {
  decisions = [];
  await act(async () => {
    root.render(
      createElement(AwaitingApprovalRow, {
        approvals,
        parked,
        taskId: "task-1",
        now: NOW,
        askerNames: new Map([["qa", "QA Engineer"]]),
        deciding,
        decided,
        failed: {},
        onDecide: canDecide
          ? (approval: ApprovalSummary, verdict: Verdict, _scope: GrantScope) =>
              decisions.push({ id: approval.id, verdict })
          : undefined,
      }),
    );
  });
}

function rows(): HTMLElement[] {
  return [...container.querySelectorAll<HTMLElement>("[data-approval-id]")];
}

function buttons(label: string): HTMLButtonElement[] {
  return [...container.querySelectorAll("button")].filter((b) =>
    b.textContent?.includes(label),
  ) as HTMLButtonElement[];
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(async () => {
  await act(async () => root.unmount());
  container.remove();
});

describe("the task detail's blocked section", () => {
  it("renders nothing at all when the card is waiting on nothing", async () => {
    await render([], []);
    expect(container.textContent).toBe("");
  });

  it("itemises the card's parked approvals, one decidable row each", async () => {
    await render(
      [hostPending("a1"), hostPending("a2")],
      [queued("a1"), queued("a2", { at_millis: T0 + 1_000 })],
    );
    expect(rows()).toHaveLength(2);
    expect(container.textContent).toContain("https://example.com/a1");
    expect(container.textContent).toContain("https://example.com/a2");
  });

  /**
   * One at a time, which is the difference from the board card. There, one
   * Approve clears the whole batch because the card's question is "unblock
   * this"; here the question is "what is this asking for", and each answer is
   * its own.
   */
  it("decides one row without touching its siblings", async () => {
    await render(
      [hostPending("a1"), hostPending("a2")],
      [queued("a1"), queued("a2", { at_millis: T0 + 1_000 })],
    );
    await act(async () => {
      buttons("Approve")[0].click();
    });
    expect(decisions).toEqual([{ id: "a1", verdict: "approve" }]);
  });

  /** A decision in flight on one row must not freeze the others (#373). */
  it("leaves a sibling's buttons live while one row is resolving", async () => {
    await render(
      [hostPending("a1"), hostPending("a2")],
      [queued("a1"), queued("a2", { at_millis: T0 + 1_000 })],
      { deciding: new Map([["a1", "approve"]]) },
    );
    const approve = buttons("Approve");
    expect(approve[0].disabled).toBe(true);
    expect(approve[1].disabled).toBe(false);
  });

  /**
   * The honest residual. The host counts four; the queue has delivered three.
   * Deciding those three leaves the card stopped, and the section has to say so
   * rather than let the operator infer they are finished.
   */
  it("says how many the queue has not delivered yet", async () => {
    await render(
      [hostPending("a1"), hostPending("a2"), hostPending("a3")],
      [queued("a1")],
    );
    expect(rows()).toHaveLength(1);
    expect(container.textContent).toContain("2 more not loaded yet");
    expect(container.textContent).toContain("this card stays stopped");
  });

  /** And when it has delivered none of them, the link out is the whole answer. */
  it("sends the operator to the Approvals page when it can decide nothing here", async () => {
    await render([hostPending("a1")], []);
    expect(rows()).toHaveLength(0);
    expect(container.textContent).toContain("Still loading it");
    const link = container.querySelector<HTMLAnchorElement>('a[href*="approvals"]');
    expect(link?.getAttribute("href")).toBe("#/approvals/task-1");
  });

  /**
   * The count of what the card is waiting on comes from the host's read, never
   * from the queue. The queue's ownership rule cannot see the attempt-level key
   * the host's does, so counting rows would under-report a card mid-poll — the
   * mistake this section is built to avoid.
   */
  it("counts the wait from the host's read, not from the rows it can draw", async () => {
    await render([hostPending("a1"), hostPending("a2"), hostPending("a3")], [queued("a1")]);
    expect(container.textContent).toContain("Waiting on 3 approvals");
  });

  /**
   * A verdict witnessed anywhere drops the row: the resolution belongs to the
   * timeline, which is where a decided approval is read. Repeating it here
   * would rebuild the Approvals tab #468 removed, one row at a time.
   */
  it("drops a row this console has already witnessed a verdict for", async () => {
    const a1 = queued("a1");
    await render([hostPending("a1")], [a1], {
      decided: { a1: { verdict: "approve", approval: a1 } },
    });
    expect(rows()).toHaveLength(0);
  });

  /**
   * The residual must not count what the operator has just settled (#1895
   * review). `approvals` is this screen's own 4s poll and `rows` follows the
   * queue, so straight after a decision the host still calls the approval
   * pending while nothing is left to show — and the arithmetic version
   * announced "still loading it" about a request the reader had just decided.
   */
  it("says nothing is outstanding when the only approval was just decided", async () => {
    const a1 = queued("a1");
    await render([hostPending("a1")], [a1], {
      decided: { a1: { verdict: "approve", approval: a1 } },
    });
    expect(container.textContent).not.toContain("Still loading");
    expect(container.textContent).not.toContain("not loaded yet");
  });

  /** And still says nothing once the queue has dropped it, while the host's own
   *  read is a poll behind. */
  it("stays quiet after the queue drops an approval this console decided", async () => {
    const a1 = queued("a1");
    await render([hostPending("a1")], [], {
      decided: { a1: { verdict: "approve", approval: a1 } },
    });
    expect(container.textContent).not.toContain("Still loading");
    expect(container.textContent).not.toContain("not loaded yet");
  });

  /** A decision on *another* card's approval accounts for nothing here. */
  it("still reports an undelivered approval that nobody has decided", async () => {
    const other = queued("b1", { task: { link: "task", id: "task-2" } });
    await render([hostPending("a1")], [], {
      decided: { b1: { verdict: "approve", approval: other } },
    });
    expect(container.textContent).toContain("Still loading it");
  });

  /**
   * Ownership is the host's answer, not the queue's (#1895 review).
   *
   * `approvalsForTask` can only match on the park's task link. `approval_owner`
   * decides with an attempt-level `run_id` the console cannot see, so an
   * approval linked to this card but belonging to another attempt is left out
   * of the host's own `approvals` — and must not be rendered here as this
   * card's, still less offered a resolve.
   */
  it("does not render a queue row the host left out of this card's approvals", async () => {
    await render([hostPending("a1")], [queued("a1"), queued("other-attempt")]);
    expect(rows()).toHaveLength(1);
    expect(container.textContent).toContain("https://example.com/a1");
    expect(container.textContent).not.toContain("https://example.com/other-attempt");
  });

  /** And a row the host has stopped calling pending stops being decidable. */
  it("drops a row once the host no longer reports it pending", async () => {
    await render([{ ...hostPending("a1"), status: "approved" } as TaskApproval, hostPending("a2")], [
      queued("a1"),
      queued("a2", { at_millis: T0 + 1_000 }),
    ]);
    expect(rows()).toHaveLength(1);
    expect(container.textContent).toContain("https://example.com/a2");
  });

  /** No handler, no controls — never live buttons that do nothing. */
  it("renders the wait and no controls when the surface cannot decide", async () => {
    await render([hostPending("a1")], [queued("a1")], { canDecide: false });
    expect(rows()).toHaveLength(0);
    expect(buttons("Approve")).toHaveLength(0);
    expect(container.textContent).toContain("Waiting on your approval");
  });
});
