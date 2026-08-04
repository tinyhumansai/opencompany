import { expect, test, type Page } from "@playwright/test";

/**
 * Proof for issue #343: an admin can set, change and clear a teammate's daily
 * cap **from the Console**, against the live host, with no redeploy.
 *
 * `team-budget.spec.ts` (issue #304) proves the cap is *displayed*. This proves
 * it is *editable* — which is the whole of #343, because before it the only way
 * to change a cap was for us to edit `company.toml` and ship a new image.
 *
 * Runs against the same live host as `wiring.spec.ts` (`companies/e2e_harness`),
 * whose `writer` carries a $5.00/day cap and whose `engineer` carries none. The
 * suite signs in through `global-setup.ts` as `harness-e2e@tinyhumans.ai`, which
 * the harness manifest lists under `[users] admins` — so the session driving
 * these specs really is an admin, and the controls below are visible.
 *
 * The spec restores every teammate it touches, so it can run repeatedly against
 * a host whose data directory persists between runs.
 */

/** The card for the teammate whose role matches `role`. */
function card(page: Page, role: string) {
  return page.getByTestId("team-card").filter({ hasText: role }).first();
}

/**
 * Opens a card's action menu.
 *
 * Located by label rather than by role: the trigger carries `aria-haspopup`,
 * and `getByRole("button", …)` does not resolve it.
 */
async function openMenu(page: Page, role: string) {
  await card(page, role).getByLabel("Member actions").click();
}

/**
 * A fresh host greets the first visit with a welcome tour rendered over the
 * console, which swallows clicks on the view beneath it. Dismiss it if present.
 */
async function dismissOnboarding(page: Page) {
  const skip = page.getByRole("button", { name: "Skip for now" });
  await skip.waitFor({ state: "visible", timeout: 15_000 }).catch(() => {});
  if (await skip.isVisible()) {
    await skip.click();
    await expect(skip).toBeHidden();
  }
}

/** Sets a cap through the dialog. */
async function setCap(page: Page, role: string, amount: string) {
  await openMenu(page, role);
  await page.getByTestId("team-budget-edit").click();
  const input = page.getByTestId("team-budget-input");
  await expect(input).toBeVisible();
  await input.fill(amount);
  await page.getByTestId("team-budget-save").click();
}

test.beforeEach(async ({ page }) => {
  await page.goto("/#/team");
  await dismissOnboarding(page);
  await expect(page.getByTestId("team-card").first()).toBeVisible({ timeout: 30_000 });
});

test("an admin can cap a teammate the company left uncapped, and reset it back", async ({
  page,
}) => {
  // The engineer starts uncapped: no budget line at all.
  await expect(card(page, "Engineer").getByTestId("team-budget")).toHaveCount(0);

  await setCap(page, "Engineer", "9");

  // The cap is on the card, from the host — and attributed to a person.
  const budget = card(page, "Engineer").getByTestId("team-budget");
  await expect(budget).toHaveText(/\$9\.00\/day/, { timeout: 30_000 });
  await expect(card(page, "Engineer").getByTestId("team-budget-attribution")).toContainText(
    /Set by/,
  );

  // Host-backed, not local state: it survives a storage-cleared reload. This is
  // the assertion that separates "the console remembers" from "the company was
  // actually changed".
  await page.evaluate(() => {
    localStorage.clear();
    sessionStorage.clear();
  });
  await page.goto("/#/team");
  await dismissOnboarding(page);
  await expect(card(page, "Engineer").getByTestId("team-budget")).toHaveText(/\$9\.00\/day/, {
    timeout: 30_000,
  });

  // Reset hands the teammate back to the company's own definition — which for
  // the engineer means uncapped, so both the cap and the attribution go away.
  await openMenu(page, "Engineer");
  await page.getByTestId("team-budget-reset").click();
  await expect(card(page, "Engineer").getByTestId("team-budget")).toHaveCount(0, {
    timeout: 30_000,
  });
  await expect(card(page, "Engineer").getByTestId("team-budget-attribution")).toHaveCount(0);
});

test("an admin can change a company-set cap, remove it, and reset it back", async ({ page }) => {
  // The writer's $5.00 comes from the company definition, so no attribution.
  await expect(card(page, "Writer").getByTestId("team-budget")).toHaveText(/\$5\.00\/day/, {
    timeout: 30_000,
  });
  await expect(card(page, "Writer").getByTestId("team-budget-attribution")).toHaveCount(0);

  // Raising it beats the company's number — the remedy this issue exists for.
  await setCap(page, "Writer", "42");
  await expect(card(page, "Writer").getByTestId("team-budget")).toHaveText(/\$42\.00\/day/, {
    timeout: 30_000,
  });

  // Removing the cap is not the same as zeroing it: the line disappears
  // entirely rather than reading "$0.00/day", and the attribution stays so an
  // operator can see a human did this.
  await openMenu(page, "Writer");
  await page.getByTestId("team-budget-remove").click();
  await expect(card(page, "Writer").getByTestId("team-budget")).toHaveCount(0, { timeout: 30_000 });
  await expect(card(page, "Writer").getByTestId("team-budget-attribution")).toContainText(
    /Uncapped by/,
  );

  // Reset restores the company's $5.00 — which no "set" could express, because
  // the value lives in the manifest rather than in the console.
  await openMenu(page, "Writer");
  await page.getByTestId("team-budget-reset").click();
  await expect(card(page, "Writer").getByTestId("team-budget")).toHaveText(/\$5\.00\/day/, {
    timeout: 30_000,
  });
  await expect(card(page, "Writer").getByTestId("team-budget-attribution")).toHaveCount(0);
});
