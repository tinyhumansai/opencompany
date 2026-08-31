// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { ApprovalSummary, GrantScope, Verdict } from "@/api/types";
import { ApprovalCard } from "@/views/ApprovalsView";

/**
 * Issue #1805: the deadline is not only shown, it can be extended. The Extend
 * button is the operator's lever against a run default-denying over a weekend.
 *
 * A render test earns the same exception the sibling `approval-card-decide-order`
 * suite does: the claims are about the DOM — that the button appears only when
 * the host reports a deadline, and that pressing it asks to extend — which no
 * test of a pure helper can see.
 */

const T0 = new Date("2026-03-02T10:00:00Z").getTime();

const BASE: ApprovalSummary = {
  id: "a1",
  kind: "payment.send",
  amount_usd: 1200,
  at_millis: T0,
  agent: "ops",
  payload: { to: "vendor@example.test" },
};

let container: HTMLDivElement;
let root: Root;

async function render(approval: ApprovalSummary, onExtend?: () => void) {
  await act(async () => {
    root.render(
      createElement(ApprovalCard, {
        approval,
        now: T0 + 60_000,
        askerNames: new Map([["ops", "Ops"]]),
        deciding: null,
        batchIndex: 1,
        batchTotal: 1,
        onDecide: (_verdict: Verdict, _scope: GrantScope) => {},
        onExtend,
      }),
    );
  });
}

function extendButton(): HTMLButtonElement | undefined {
  return Array.from(container.querySelectorAll("button")).find((b) =>
    b.textContent?.includes("Extend"),
  ) as HTMLButtonElement | undefined;
}

beforeEach(() => {
  (
    globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }
  ).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("the Extend button (#1805)", () => {
  it("appears when the host reports a deadline and asks to extend on click", async () => {
    let extended = 0;
    await render(
      { ...BASE, expires_at_millis: T0 + 3_600_000 },
      () => {
        extended += 1;
      },
    );
    const button = extendButton();
    expect(button, "the Extend button must render for a card with a deadline")
      .toBeTruthy();
    await act(async () => {
      button!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
    expect(extended, "clicking Extend asks the parent to extend").toBe(1);
  });

  it("is hidden when the host reports no deadline", async () => {
    // No `expires_at_millis`: this host has no deadlines, so there is nothing to
    // extend and the button must not appear.
    await render(BASE);
    expect(
      extendButton(),
      "a host without deadlines offers no Extend button",
    ).toBeUndefined();
  });
});
