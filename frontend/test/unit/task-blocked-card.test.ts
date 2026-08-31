// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { Task } from "@/api/tasks";
import type { ApprovalSummary, GrantScope, Verdict } from "@/api/types";
import { taskApprovalRows } from "@/lib/task-approvals";
import { TaskItem } from "@/views/TaskCard";
import type { DecidedApproval } from "@/views/chat/model";

/**
 * The paused card says what it is waiting on, and does not offer Resume as the
 * way out of it (issue #883).
 *
 * This suite is normally for pure functions — see `vitest.config.ts` — and the
 * exception is earned exactly as `approval-batch-card.test.ts` earns it: the
 * claim under test only exists at the rendered card. The issue's reproduction
 * is a loop, and the loop is a click:
 *
 *   1. a turn parks five approvals;
 *   2. the operator decides one, nothing visibly happens — the turn continues
 *      only when the *last* of them lands (#469);
 *   3. so Resume is the natural next click, and it re-dispatches: the agent
 *      re-runs from the top, parks the same calls again, the queue grows.
 *
 * `taskApprovalRows` is unit-tested next door and decides *whether* the card is
 * blocked. What it cannot reach is whether the button the operator's hand lands
 * on is actually stopped, which is the half that breaks the loop.
 *
 * Since #1891 the card also carries the decision itself, and the claims that
 * only exist at the rendered card grew accordingly: that the row names *which*
 * call is blocked rather than its kind, that it says when the deadline will
 * decide for you, that Approve resolves every id the card is held on, and that
 * a request the row can only paraphrase is not something it will one-click
 * authorise.
 */

const T0 = new Date("2026-03-02T10:00:00Z").getTime();
const NOW = T0 + 240_000;

function card(): Task {
  return {
    id: "task-1",
    title: "Triage the release blockers",
    // Phase, then stage (issue #1512): the board files it under Working and
    // the card itself says it is paused, which is what puts Resume on it.
    column: "working",
    stage: "paused",
    priority: "high",
    assignee: "qa",
    updatedAt: T0,
  } as Task;
}

/**
 * One parked approval. Batched into `turn-1` by default, because that is the
 * ordinary case a card is blocked by — the calls a single agent turn gated —
 * and since #1895 the card groups by that key rather than treating everything
 * it holds as one batch.
 */
function parked(id: string, kind: string, at = T0, batch: string | null = "turn-1"): ApprovalSummary {
  return {
    id,
    kind,
    amount_usd: null,
    at_millis: at,
    agent: "qa",
    task: { link: "task", id: "task-1" },
    payload: { url: "https://example.com/a" },
    ...(batch ? { batch } : {}),
  };
}

interface Decision {
  id: string;
  verdict: Verdict;
}

let container: HTMLDivElement;
let root: Root;
let resumes: number;
let opens: number;
let decisions: Decision[];

async function render(
  approvals: ApprovalSummary[],
  {
    dragging = false,
    decided = {},
    deciding = new Map<string, Verdict>(),
    failed = {},
  }: {
    dragging?: boolean;
    decided?: Record<string, DecidedApproval>;
    deciding?: ReadonlyMap<string, Verdict>;
    failed?: Record<string, string>;
  } = {},
) {
  resumes = 0;
  opens = 0;
  decisions = [];
  await act(async () => {
    root.render(
      createElement(TaskItem, {
        task: card(),
        dragging,
        rows: taskApprovalRows(approvals, decided, "task-1"),
        now: NOW,
        askerNames: new Map([["qa", "QA Engineer"]]),
        deciding,
        failed,
        onDecide: (approval: ApprovalSummary, verdict: Verdict, _scope: GrantScope) =>
          decisions.push({ id: approval.id, verdict }),
        onOpen: () => {
          opens += 1;
        },
        onResume: () => {
          resumes += 1;
        },
      }),
    );
  });
}

/** The decidable blocked row, when the card is showing one. */
function blockedRow(): HTMLElement | null {
  return container.querySelector<HTMLElement>('[data-approval-inline="card"]');
}

function button(label: string): HTMLButtonElement {
  const found = Array.from(container.querySelectorAll("button")).find((b) =>
    b.textContent?.includes(label),
  );
  if (!found) throw new Error(`no ${label} button in:\n${container.innerHTML}`);
  return found as HTMLButtonElement;
}

function resumeButton(): HTMLButtonElement {
  const button = Array.from(container.querySelectorAll("button")).find((b) =>
    b.textContent?.includes("Resume"),
  );
  if (!button) throw new Error(`no Resume button in:\n${container.innerHTML}`);
  return button as HTMLButtonElement;
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

describe("a paused card with approvals outstanding", () => {
  it("uses its title button as the task action, leaving its controls independent", async () => {
    await render([]);

    const card = container.firstElementChild as HTMLDivElement;
    const title = [...container.querySelectorAll("button")].find((button) =>
      button.textContent?.includes("Triage the release blockers"),
    );
    if (!title) throw new Error(`no task title button in:\n${container.innerHTML}`);

    expect(card.getAttribute("role")).toBeNull();
    expect(card.hasAttribute("tabindex")).toBe(false);
    expect(title.querySelectorAll("button, a")).toHaveLength(0);
    await act(async () => {
      title.click();
    });
    expect(opens).toBe(1);
  });

  it("looks lifted while the board is carrying it", async () => {
    await render([], { dragging: true });

    const card = container.firstElementChild;
    expect(card).not.toBeNull();
    expect(card?.className).toContain("-translate-y-1");
    expect(card?.className).toContain("rotate-1");
    expect(card?.className).toContain("shadow-xl");
    expect(card?.className).not.toContain("opacity-50");
  });

  it("names the one call it is blocked on, rather than the mechanism", async () => {
    await render([parked("a1", "web_fetch")]);
    // `approvalAction`'s words — the same function the Approvals page and the
    // chat card label their rows with, so all three say one thing about one
    // approval instead of three different things.
    expect(blockedRow()?.textContent).toContain("Fetch a web page");
  });

  /**
   * The gap #1891 is about. "Fetch a web page" is the *kind*; two cards blocked
   * on `web_fetch` read identically until the row names the argument being
   * consented to, and the card had that argument in hand the whole time.
   */
  it("names which call, not only what kind of call", async () => {
    await render([parked("a1", "web_fetch")]);
    expect(blockedRow()?.textContent).toContain("https://example.com/a");
  });

  /**
   * A batch names every call one Approve would authorise. Counting them — the
   * card's old "Blocked on 4 approvals" — lets a harmless first call stand in
   * for a consequential later one, which is the objection `compactLabel` was
   * written around and which a narrower surface does not get to relax.
   */
  it("names every call in a batch rather than counting them", async () => {
    await render([
      { ...parked("a1", "shell"), payload: { command: "ls" } },
      { ...parked("a2", "shell", T0 + 1_000), payload: { command: "rm -rf build" } },
    ]);
    const text = blockedRow()?.textContent ?? "";
    expect(text).toContain("ls");
    expect(text).toContain("rm -rf build");
    expect(text).not.toContain("Blocked on 2 approvals");
  });

  /**
   * The other half of what the card never said. It counted *up* from the park
   * and never once mentioned that the approval default-denies on the company's
   * deadline — so the operator's first news of an expiry was the work not
   * having happened.
   */
  it("says when the deadline will decide for the operator", async () => {
    await render([
      { ...parked("a1", "web_fetch"), expires_at_millis: NOW + 90 * 60_000 },
    ]);
    expect(blockedRow()?.textContent).toContain("Declines itself in 1h");
  });

  /** And says nothing at all when the host reports no deadline — never one it
   *  computed itself, which nothing would enforce. */
  it("invents no deadline when the host reports none", async () => {
    await render([parked("a1", "web_fetch")]);
    expect(blockedRow()?.textContent).not.toContain("Declines itself");
  });

  it("names who asked", async () => {
    await render([parked("a1", "web_fetch")]);
    expect(blockedRow()?.textContent).toContain("QA Engineer");
  });

  /**
   * The point of the whole change: the decision happens here. One click covers
   * every call the card is held on, resolved per id — the turn continues only
   * when the last of them lands (#469), so a click that left one undecided
   * would hold the card open while looking like it had cleared it.
   */
  it("approves every approval the card is held on, per id", async () => {
    await render([
      parked("a1", "web_fetch"),
      parked("a2", "web_fetch", T0 + 1_000),
      parked("a3", "shell", T0 + 2_000),
    ]);
    await act(async () => {
      button("Approve").click();
    });
    expect(decisions).toEqual([
      { id: "a1", verdict: "approve" },
      { id: "a2", verdict: "approve" },
      { id: "a3", verdict: "approve" },
    ]);
  });

  it("declines them the same way", async () => {
    await render([parked("a1", "web_fetch"), parked("a2", "shell", T0 + 1_000)]);
    await act(async () => {
      button("Decline").click();
    });
    expect(decisions).toEqual([
      { id: "a1", verdict: "deny" },
      { id: "a2", verdict: "deny" },
    ]);
  });

  /**
   * A decision witnessed anywhere — this card, the Approvals page, another
   * operator's console over the event stream — is not something this Approve
   * still covers, and the row has to say so rather than silently renumbering
   * the work under the operator.
   */
  it("subtracts an approval already decided elsewhere", async () => {
    const a1 = parked("a1", "web_fetch");
    const a2 = parked("a2", "shell", T0 + 1_000);
    // Both still in the queue — the decision landed a moment ago and the feed
    // has not dropped `a1` yet, which is the only window the annotation covers.
    await render([a1, a2], {
      decided: { a1: { verdict: "approve", approval: a1 } },
    });
    expect(blockedRow()?.textContent).toContain("1 of 2 decided");
    await act(async () => {
      button("Approve").click();
    });
    expect(decisions).toEqual([{ id: "a2", verdict: "approve" }]);
  });

  /**
   * A body cut to fit the row is a preview, not the payload — two POSTs sharing
   * their first 60 characters render identically even when the tail changes
   * what the request does. The board is the most compressed surface there is,
   * so the gate #1330 put on the chat row has to reach it too.
   */
  it("will not one-click approve a request it can only paraphrase", async () => {
    await render([
      {
        ...parked("a1", "http_request"),
        payload: { url: "https://example.com/a", body: "x".repeat(400) },
      },
    ]);
    expect(
      Array.from(container.querySelectorAll("button")).some((b) =>
        b.textContent?.includes("Approve"),
      ),
    ).toBe(false);
    // Decline is always safe and stays; the way to Approve is to read it first.
    expect(button("Decline")).toBeTruthy();
    expect(blockedRow()?.textContent).toContain("Read it first");
  });

  /** The details link addresses *this card's* rows, not the flat queue. */
  it("links to its own rows on the Approvals page", async () => {
    await render([parked("a1", "web_fetch")]);
    const link = blockedRow()?.querySelector<HTMLAnchorElement>('a[href*="approvals"]');
    expect(link?.getAttribute("href")).toContain("approvals/task-1");
  });

  /**
   * A card is not a batch (#1895 review). `ApprovalRow` consolidates because
   * #842's premise is that a batch is *one turn's work*, interrupting once — so
   * two turns' parks under one Approve would authorise across unrelated
   * requests. The card holds what it holds; the grouping is the transcript's
   * own `approvalBatchKey`, shared rather than restated.
   */
  it("keeps two turns' approvals in separate rows, each with its own Approve", async () => {
    await render([
      parked("a1", "web_fetch", T0, "turn-1"),
      parked("a2", "shell", T0 + 1_000, "turn-2"),
    ]);
    expect(container.querySelectorAll('[data-approval-inline="card"]')).toHaveLength(2);
    await act(async () => {
      button("Approve").click();
    });
    // The first row's Approve decided the first turn, and nothing else.
    expect(decisions).toEqual([{ id: "a1", verdict: "approve" }]);
  });

  /**
   * And batchless approvals are never grouped, not even with each other: an
   * absent key means the host did not say which turn a park came from, and
   * folding two of those together invents a batch out of two facts that are
   * only alike in being unknown.
   */
  it("never groups approvals the host gave no batch key", async () => {
    await render([
      parked("a1", "web_fetch", T0, null),
      parked("a2", "web_fetch", T0 + 1_000, null),
    ]);
    expect(container.querySelectorAll('[data-approval-inline="card"]')).toHaveLength(2);
  });

  /** Deciding one turn must not freeze the other's buttons. */
  it("leaves one turn's buttons live while another turn is resolving", async () => {
    await render(
      [parked("a1", "web_fetch", T0, "turn-1"), parked("a2", "shell", T0 + 1_000, "turn-2")],
      { deciding: new Map([["a1", "approve"]]) },
    );
    const approve = Array.from(container.querySelectorAll("button")).filter((b) =>
      b.textContent?.includes("Approve"),
    ) as HTMLButtonElement[];
    expect(approve).toHaveLength(2);
    expect(approve[0].disabled).toBe(true);
    expect(approve[1].disabled).toBe(false);
  });

  /**
   * A decision in flight elsewhere on the board is not this card's business.
   * The shell's `deciding` map is console-wide, and passed through whole it
   * would grey out every blocked card at once on one click.
   */
  it("stays live while another card's decision is in flight", async () => {
    await render([parked("a1", "web_fetch")], {
      deciding: new Map([["somebody-elses", "approve"]]),
    });
    expect(button("Approve").disabled).toBe(false);
  });

  /**
   * The behaviour the issue is actually about. Step 3 of its reproduction is an
   * operator pressing Resume on a card that is waiting, and the card getting
   * worse for it.
   */
  it("disables Resume, so the re-dispatch loop cannot be started from here", async () => {
    await render([parked("a1", "web_fetch")]);
    const button = resumeButton();
    expect(button.disabled).toBe(true);
    await act(async () => {
      button.click();
    });
    expect(resumes).toBe(0);
  });

  /**
   * Disabled, not hidden. A card with no visible next action is the ambiguity
   * being fixed — the operator has to see that Resume is the wrong click now,
   * not wonder where it went.
   */
  it("still shows the Resume button, with the reason on it", async () => {
    await render([parked("a1", "web_fetch")]);
    expect(resumeButton().getAttribute("title")).toContain("decide its approvals first");
  });
});

describe("a paused card with nothing outstanding", () => {
  it("renders no blocked row and an enabled Resume", async () => {
    await render([]);
    expect(blockedRow()).toBeNull();
    const button = resumeButton();
    expect(button.disabled).toBe(false);
    await act(async () => {
      button.click();
    });
    expect(resumes).toBe(1);
  });

  it("is not blocked by another card's approvals", async () => {
    await render([
      { ...parked("b1", "web_fetch"), task: { link: "task", id: "task-2" } },
    ]);
    expect(resumeButton().disabled).toBe(false);
  });
});

/**
 * The window between the click and the continuation (#1895 review).
 *
 * The resolve detaches (#391), so its answer comes back *before* the follow-up
 * cycle the verdict released has run. The decided row therefore has to stop
 * asking — its buttons are spent — while Resume stays down, because
 * re-dispatching there would duplicate work the decision had already set going,
 * with the operator's finger in exactly the right place to do it.
 */
describe("a paused card whose last approval was just decided", () => {
  it("stops asking, but keeps Resume down until the host clears the queue", async () => {
    const a1 = parked("a1", "web_fetch");
    await render([a1], { decided: { a1: { verdict: "approve", approval: a1 } } });
    // Nothing left to decide: no second Approve on a settled request.
    expect(
      Array.from(container.querySelectorAll("button")).some((b) =>
        b.textContent?.includes("Approve"),
      ),
    ).toBe(false);
    expect(resumeButton().disabled).toBe(true);
  });

  it("re-enables Resume once the host has dropped it from the queue", async () => {
    const a1 = parked("a1", "web_fetch");
    // The feed has refreshed and no longer carries the approval. The witnessed
    // verdict must not resurrect a row — see `task-approval-block.test.ts`.
    await render([], { decided: { a1: { verdict: "approve", approval: a1 } } });
    expect(blockedRow()).toBeNull();
    expect(resumeButton().disabled).toBe(false);
  });
});
