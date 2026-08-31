// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { ApprovalSummary, GrantScope, Verdict } from "@/api/types";
import { ApprovalRow } from "@/views/chat/ApprovalRow";

/**
 * A batch does not lose the consequence the single card shows (#1426).
 *
 * Consolidating three parked calls into one card with one Approve (#842) is
 * the common shape for exactly the calls that carry a consequence — a research
 * turn parks several fetches, an outreach turn several sends — so a batch that
 * dropped the warning would hide it precisely where it is most often earned,
 * while the Approvals page went on showing it for the same parks. Two surfaces
 * disagreeing about what a call does is the drift this work exists to remove.
 *
 * Rendered rather than tested as a pure function for the same reason
 * `approval-batch-card` earns its exception: the claim is about what reaches
 * the operator's eye, and the grouping helper cannot see whether the card drew
 * it.
 */

const T0 = new Date("2026-03-02T10:00:00Z").getTime();

function approval(id: string, group: ApprovalSummary["group"], kind = "web_fetch"): ApprovalSummary {
  return {
    id,
    kind,
    group,
    amount_usd: null,
    at_millis: T0,
    agent: "seo",
    thread: "desk-marketing",
    batch: "turn-1",
    broadly_grantable: true,
    payload: { url: `https://example.com/${id}` },
  };
}

let container: HTMLDivElement;
let root: Root;

async function render(
  approvals: ApprovalSummary[],
  decided: Record<string, Verdict> = {},
  failed: Record<string, string> = {},
  compact = false,
) {
  await act(async () => {
    root.render(
      createElement(ApprovalRow, {
        approvals,
        now: T0 + 60_000,
        askerNames: new Map([["seo", "SEO Specialist"]]),
        deciding: new Map(),
        decided,
        failed,
        variant: compact ? ("compact" as const) : ("full" as const),
        onDecide: (_a: ApprovalSummary, _v: Verdict, _s: GrantScope) => {},
      }),
    );
  });
}

/** Every consequence badge on the card, headline and item lines alike. */
function badges(): string[] {
  return [...container.querySelectorAll<HTMLElement>("[data-approval-consequence]")].map(
    (el) => el.dataset.approvalConsequence ?? "",
  );
}

/** The headline's icon tile, whose tint carries a uniform batch's consequence. */
function iconTile(): HTMLElement {
  const tile = container.querySelector<HTMLElement>(".size-10");
  if (!tile) throw new Error(`no icon tile on the card: ${container.innerHTML}`);
  return tile;
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("the consequence on a batched approval card", () => {
  it("marks a batch whose calls all carry the same consequence", async () => {
    await render([approval("a1", "send"), approval("a2", "send"), approval("a3", "send")]);

    expect(container.textContent).toContain("Leaves the company");
    // Once in the headline for the batch, once per line for attribution.
    expect(badges()).toEqual([
      "Leaves the company",
      "Leaves the company",
      "Leaves the company",
      "Leaves the company",
    ]);
  });

  it("tints a uniform batch's icon with its consequence", async () => {
    await render([approval("a1", "spend"), approval("a2", "spend")]);

    expect(iconTile().className).toContain("bg-tone-4/15");
  });

  it("names every distinct consequence in a mixed batch", async () => {
    await render([approval("a1", "spend"), approval("a2", "send"), approval("a3", "spend")]);

    const headline = badges().slice(0, 2);
    expect(headline).toEqual(["Spends money", "Leaves the company"]);
  });

  it("leaves a mixed batch's icon neutral rather than letting one item speak", async () => {
    await render([approval("a1", "spend"), approval("a2", "send")]);

    const tile = iconTile();
    expect(tile.className).toContain("bg-muted");
    expect(tile.className).not.toContain("bg-tone-");
  });

  it("attributes each consequence to the call that carries it", async () => {
    await render([approval("a1", "spend"), approval("a2", "send")]);

    const lines = [...container.querySelectorAll<HTMLElement>("[data-approval-item]")];
    expect(lines).toHaveLength(2);
    expect(lines[0].textContent).toContain("Spends money");
    expect(lines[1].textContent).toContain("Leaves the company");
  });

  it("stays unmarked for a batch of internal calls", async () => {
    await render([approval("a1", "other"), approval("a2", undefined)]);

    expect(badges()).toEqual([]);
    expect(iconTile().className).toContain("bg-muted");
  });

  it("drops a settled line's warning, which no longer informs a decision", async () => {
    await render([approval("a1", "spend"), approval("a2", "send")], { a1: "approve" });

    const lines = [...container.querySelectorAll<HTMLElement>("[data-approval-item]")];
    expect(lines[0].textContent).not.toContain("Spends money");
    expect(lines[1].textContent).toContain("Leaves the company");
  });

  // The headline describes what the next Approve authorises, and `decideAll`
  // acts only on the still-pending subset. An item settled on the Approvals
  // page while this card sat open is no longer part of that decision.
  it("stops claiming a consequence settled elsewhere while the card sat open", async () => {
    await render([approval("a1", "spend"), approval("a2", "other")], { a1: "approve" });

    expect(badges()).toEqual([]);
    expect(iconTile().className).not.toContain("bg-tone-");
  });

  it("narrows a mixed headline to what is still pending", async () => {
    await render([approval("a1", "spend"), approval("a2", "send"), approval("a3", "send")], {
      a1: "approve",
    });

    // Only the sends are still up for decision, so the batch now agrees with
    // itself and wears that one consequence.
    expect(container.textContent).not.toContain("Spends money");
    expect(iconTile().className).toContain("bg-tone-2/15");
  });

  // A failed item is still pending and still retryable, so its warning is
  // attached to a decision the operator has yet to make.
  it("renders consequences and tint in the compact chat row", async () => {
    await render([approval("a1", "send"), approval("a2", "send")], {}, {}, true);

    expect(badges()).toEqual(["Leaves the company"]);
    const tile = container.querySelector<HTMLElement>(".size-7");
    expect(tile?.className).toContain("bg-tone-2/15");
  });

  it("keeps a compact mixed batch neutral when one item is unclassified", async () => {
    await render([approval("a1", "spend"), approval("a2", "other")], {}, {}, true);

    expect(badges()).toEqual(["Spends money"]);
    const tile = container.querySelector<HTMLElement>(".size-7");
    expect(tile?.className).toContain("bg-muted");
    expect(tile?.className).not.toContain("bg-tone-");
  });

  it("keeps a compact batch neutral when a group is absent", async () => {
    await render([approval("a1", "spend"), approval("a2", undefined)], {}, {}, true);

    expect(badges()).toEqual(["Spends money"]);
    const tile = container.querySelector<HTMLElement>(".size-7");
    expect(tile?.className).toContain("bg-muted");
    expect(tile?.className).not.toContain("bg-tone-");
  });
});
