// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { ApiError, type FinancesDto } from "@/api/types";
import { FinancesView } from "@/views/FinancesView";

/**
 * The finance overview must tell an empty ledger apart from an absent route,
 * and an absent budget apart from a zero-dollar cap.
 *
 * A zero-filled response is a real accounting result; a 404 means this host
 * cannot make that claim. These checks also keep zero from inheriting either a
 * negative currency sign or the positive net treatment.
 */

const EMPTY_LEDGER: FinancesDto = {
  balanceUsd: -0,
  budgetUsd: null,
  spentUsd: 0,
  revenueUsd: 0,
  netUsd: -0,
  byCategory: [],
  transactions: [],
};

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

async function render(finances: Promise<FinancesDto>): Promise<void> {
  const client = { finances: vi.fn(() => finances) } as unknown as OpenCompanyClient;
  await act(async () => {
    root.render(createElement(FinancesView, { client, company: "acme" }));
    await Promise.resolve();
    await Promise.resolve();
  });
}

describe("FinancesView empty data", () => {
  it("renders a genuine empty ledger without a negative zero, budget bar, or positive net state", async () => {
    await render(Promise.resolve(EMPTY_LEDGER));

    expect(container.textContent).toContain("$0.00");
    expect(container.textContent).not.toContain("-$0.00");
    expect(container.textContent).toContain("No monthly budget is set.");
    expect(container.textContent).not.toContain("0% of budget used");
    expect(container.textContent).toContain("No spending has been recorded yet.");
    expect(container.textContent).toContain("No transactions have been recorded yet.");
    expect(container.textContent).not.toContain("Available USDC");

    const net = container.querySelectorAll<HTMLElement>(".text-2xl")[3];
    // Cents, like every other money figure on the page. This tile used to
    // render whole dollars, which is what made a real -$0.16 read as -$0
    // beside the transactions that added up to it (issue B-016).
    expect(net?.textContent).toBe("$0.00");
    expect(net?.className).not.toContain("text-status-done-text");
  });

  it("says when the host does not expose finances instead of rendering a zero ledger", async () => {
    await render(Promise.reject(new ApiError(404, "http_404", "no finances route", false)));

    expect(container.textContent).toContain("Finances unavailable");
    expect(container.textContent).toContain("This host doesn't expose finances");
    expect(container.textContent).not.toContain("Wallet balance");
  });

  it("distinguishes a deleted company from an unwired route", async () => {
    await render(Promise.reject(new ApiError(404, "company_not_found", "acme", true)));

    expect(container.textContent).toContain("Could not load finances");
    expect(container.textContent).not.toContain("This host doesn't expose finances");
  });

  it("labels an explicit zero-dollar cap rather than claiming no budget is set", async () => {
    await render(Promise.resolve({ ...EMPTY_LEDGER, budgetUsd: 0 }));

    expect(container.textContent).toContain("Spending is capped at $0.00 this month.");
    expect(container.textContent).not.toContain("No monthly budget is set.");
    expect(container.textContent).not.toContain("0% of budget used");
  });
});
