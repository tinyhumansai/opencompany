// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { ApprovalSummary, GrantScope, Verdict } from "@/api/types";
import type { WorkflowRunResult } from "@/api/workflows";
import type { DecidedApproval } from "@/views/chat/model";
import { approvalsForRun, runApprovals } from "@/views/workflows/run-approvals";
import { RunResultPanel } from "@/views/workflows/RunResultPanel";

/**
 * Deciding a run's parked gates inside the run window (issue #1002).
 *
 * The drawer already told the operator their run stopped for a person — the
 * parked badge and the "Not finished — …" sentence — and gave them nowhere to
 * act: they had to leave the run, find the row in a flat queue, resolve it, and
 * come back. This suite pins the three claims that make the second surface
 * safe rather than merely convenient:
 *
 *  1. it is **scoped to the run on screen** — a card another run parked, or one
 *     no workflow run parked at all, must not appear under these steps;
 *  2. it **subscribes rather than snapshots** — a card cleared on the Approvals
 *     page, or by another operator, stops being shown as blocking here with no
 *     reload. This is the likeliest defect in the whole change, so it is tested
 *     against a fixture whose frozen receipt still lists the card: an
 *     implementation reading `result.approvals` passes every other test here
 *     and fails that one;
 *  3. it issues **one ordinary per-id resolve per approval** — no batch on the
 *     wire, no optimistic local state that could offer a settled card again.
 *
 * A jsdom render rather than a pure test, and it earns the exception the same
 * way `approval-batch-card` does: the claims above are only true at the click
 * and at the re-render. A pure test of the filter can see which rows were
 * selected, not whether a button was offered for them.
 */

const T0 = new Date("2026-03-02T10:00:00Z").getTime();
const RUN = "run-a1b2";
const OTHER_RUN = "run-zzzz";

function approval(id: string, over: Partial<ApprovalSummary> = {}): ApprovalSummary {
  return {
    id,
    kind: "external.publish",
    amount_usd: null,
    at_millis: T0,
    task: { link: "unlinked" },
    agent: "writer",
    payload: { path: `/notes/${id}.md` },
    workflow_run_id: RUN,
    ...over,
  };
}

/** The card this run's "spec" node parked. */
const MINE = approval("appr-mine");
/** The same shape, parked by a different run — the leak this must not have. */
const THEIRS = approval("appr-theirs", {
  workflow_run_id: OTHER_RUN,
  payload: { path: "/notes/theirs.md" },
});
/** A card no workflow run parked — a chat turn or a scheduler tick. */
const PAGE_ONLY = approval("appr-page-only", { workflow_run_id: undefined });

/**
 * The run body, as the host answers a synchronous run that blocked.
 *
 * `approvals` is the run's own **receipt** (#880): a frozen record of what this
 * run opened, which nothing ever comes back to flip. Every fixture below keeps
 * it fully populated — including the staleness case — precisely so a panel that
 * grounded its blocking list there would still find `appr-mine` listed and be
 * caught.
 */
function result(over: Partial<WorkflowRunResult> = {}): WorkflowRunResult {
  return {
    output: {},
    pendingApprovals: ["spec"],
    runId: RUN,
    blockedNodes: [
      { nodeId: "spec", tools: ["publish_artifact"], approvalIds: [MINE.id] },
    ],
    approvals: [
      {
        nodeId: "spec",
        tool: "publish_artifact",
        outcome: "parked",
        approvalId: MINE.id,
      },
    ],
    ...over,
  };
}

interface Decision {
  id: string;
  verdict: Verdict;
  scope: GrantScope;
}

let container: HTMLDivElement;
let root: Root;
let decisions: Decision[];

async function render(
  approvals: ApprovalSummary[],
  decided: Record<string, DecidedApproval> = {},
  run: WorkflowRunResult = result(),
) {
  await act(async () => {
    root.render(
      createElement(RunResultPanel, {
        result: run,
        graph: {
          id: "feature_pipeline",
          name: "Feature pipeline",
          version: null,
          nodes: [{ id: "spec", kind: "agent", name: "Draft the note", agent: "writer" }],
          edges: [],
        },
        request: "",
        onClose: () => {},
        approvals,
        now: T0 + 60_000,
        askerNames: new Map([["writer", "Staff Writer"]]),
        deciding: new Map(),
        decided,
        failed: {},
        onDecide: (approval: ApprovalSummary, verdict: Verdict, scope: GrantScope) =>
          decisions.push({ id: approval.id, verdict, scope }),
      }),
    );
  });
}

/** The run-scoped approvals section, or `null` when the drawer offers none. */
function section(): HTMLElement | null {
  return container.querySelector<HTMLElement>('[data-testid="workflow-run-approvals"]');
}

/** The card rendered for one approval id, wherever it sits in the drawer. */
function card(id: string): HTMLElement | null {
  return container.querySelector<HTMLElement>(`[data-approval-id="${id}"]`);
}

/** The approve/decline control on one approval's card, if it is offered one. */
function control(id: string, label: string): HTMLButtonElement | null {
  const buttons = [...(card(id)?.querySelectorAll("button") ?? [])];
  return (
    (buttons.find((b) => (b.textContent ?? "").includes(label)) as HTMLButtonElement) ?? null
  );
}

async function click(el: HTMLElement) {
  await act(async () => {
    el.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  decisions = [];
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("the join", () => {
  it("selects only the cards this run parked", () => {
    expect(approvalsForRun([MINE, THEIRS, PAGE_ONLY], RUN)).toEqual([MINE]);
  });

  it("joins to nothing when the host handed back no run id", () => {
    // A host predating #371 answers with no `runId`. An absent id must match
    // NOTHING rather than everything — matching every unlinked card there would
    // put another run's gates, and a scheduler's, under this run's steps.
    expect(approvalsForRun([MINE, THEIRS, PAGE_ONLY], undefined)).toEqual([]);
    expect(runApprovals([MINE], {}, null)).toEqual([]);
  });

  it("keeps a decided card the queue has already dropped", () => {
    // The host removes a resolved approval from `GET …/approvals` at once, so
    // the witnessed map is the only thing left to draw the row from — without
    // it the operator's own decision blinks out of the panel instead of
    // settling in place.
    const rows = runApprovals([], { [MINE.id]: { verdict: "approve", approval: MINE } }, RUN);

    expect(rows).toHaveLength(1);
    expect(rows[0].verdict).toBe("approve");
  });
});

describe("the run drawer's approvals section", () => {
  it("offers a decision for the card this run parked, naming the step", async () => {
    await render([MINE]);

    expect(section()).not.toBeNull();
    // The node the gate is on, by its graph name rather than its id.
    expect(section()?.textContent).toContain("Draft the note");
    expect(control(MINE.id, "Approve")).not.toBeNull();
    expect(control(MINE.id, "Decline")).not.toBeNull();
    // The same content the Approvals page shows, from the same shared card —
    // the payload is the thing being consented to.
    expect(section()?.textContent).toContain("/notes/appr-mine.md");
  });

  it("offers nothing for a card another run parked, or one no run parked", async () => {
    // The queue is handed over WHOLE — nothing upstream filters it, which is
    // what keeps the Approvals page and the badge intact. So the scoping has to
    // hold here, on the same render that can see all three.
    await render([MINE, THEIRS, PAGE_ONLY]);

    expect(card(MINE.id)).not.toBeNull();
    expect(card(THEIRS.id)).toBeNull();
    expect(card(PAGE_ONLY.id)).toBeNull();
    expect(section()?.getAttribute("data-still-waiting")).toBe("1");
  });

  it("resolves on the approval's own id, once, with no batch", async () => {
    await render([MINE]);
    await click(control(MINE.id, "Approve")!);

    // One ordinary per-id resolve. There is no batch decision on the wire and
    // none is invented here: each park stays a separate single-use decision, so
    // an operator racing on the Approvals page gets a real approval or a
    // `NotParked` no-op receipt — never a double execution.
    expect(decisions).toEqual([
      { id: MINE.id, verdict: "approve", scope: { kind: "once" } },
    ]);
  });

  it("declines on its own id too", async () => {
    await render([MINE]);
    await click(control(MINE.id, "Decline")!);

    expect(decisions).toEqual([{ id: MINE.id, verdict: "deny", scope: { kind: "once" } }]);
  });

  it("says the cards are here as well as on the Approvals page", async () => {
    await render([MINE]);

    // Additive wording: the queue is still named, because it is still the queue
    // and still holds every one of these rows.
    expect(container.textContent).toContain("below or in Approvals");
  });

  it("keeps the pre-#1002 drawer when no decision handler is wired", async () => {
    await act(async () => {
      root.render(
        createElement(RunResultPanel, {
          result: result(),
          graph: null,
          request: "",
          onClose: () => {},
          approvals: [MINE],
        }),
      );
    });

    // No handler, no surface — and the sentence goes back to pointing at the
    // page alone rather than at a section that is not there.
    expect(section()).toBeNull();
    expect(container.textContent).toContain("in Approvals");
    expect(container.textContent).not.toContain("below or in Approvals");
  });
});

describe("the staleness guard", () => {
  it("stops showing a card cleared elsewhere as blocking, with no reload", async () => {
    await render([MINE]);
    expect(section()?.getAttribute("data-still-waiting")).toBe("1");
    expect(control(MINE.id, "Approve")).not.toBeNull();

    // What the operator clearing it on the Approvals page (or another operator,
    // in another console) produces here: the `approval_resolved` frame refreshes
    // the feed, the host has already dropped the row, and the shell records the
    // witnessed verdict. No remount, no reload — the same panel, new props.
    //
    // `result` is UNCHANGED, and that is the whole point of the test: its
    // receipt still lists `appr-mine` as a park this run opened, because a
    // receipt never flips. A panel that derived its blocking list from
    // `result.approvals` would still be offering two live buttons here.
    await render([], { [MINE.id]: { verdict: "approve", approval: MINE } });

    expect(section()?.getAttribute("data-still-waiting")).toBe("0");
    expect(control(MINE.id, "Approve")).toBeNull();
    expect(control(MINE.id, "Decline")).toBeNull();
    // The row does not vanish — the operator has to be able to see the decision
    // land — but it states the verdict instead of asking again.
    expect(card(MINE.id)?.textContent).toContain("Approved");
    expect(section()?.textContent).toContain("run the workflow again");
  });

  it("is not fooled by the run's own frozen receipt", async () => {
    // The negative form of the same guard, stated on its own so a regression
    // names itself: a run whose receipt says it parked a card, whose live queue
    // no longer carries it and which this console never witnessed being decided
    // — another tab decided it before this drawer ever opened — offers nothing.
    await render([]);

    expect(section()).toBeNull();
    expect(card(MINE.id)).toBeNull();
    // And the receipt itself still says what it always said, so the sentence
    // above the section is untouched by any of this.
    expect(container.textContent).toContain("parked 1 approval");
  });
});
