import { expect, test } from "@playwright/test";

/**
 * Proof for issue #304: a teammate's `budget_usd_daily` reaches the console.
 *
 * The issue described the cap as "validated, persisted and displayed" — the
 * last of those was never true. Nothing on the wire carried it: `GET …/team`
 * had no budget field, the GraphQL `TeamMember` had none, and the Team card
 * rendered nothing. So this spec is not a regression net over a working
 * surface; it is the first proof the surface exists.
 *
 * Two observable consequences, both against the live host (`companies/
 * e2e_harness`, whose `writer` carries a $5.00/day cap and whose `ceo` /
 * `engineer` carry none):
 *
 *   1. A capped teammate renders its cap and today's spend, from the host —
 *      not a client-side guess.
 *   2. An uncapped teammate renders **no** budget line at all. This is the
 *      assertion that matters: the host omits the fields rather than sending
 *      zeros, and a console that defaulted them to `0` would paint every
 *      uncapped teammate as permanently out of budget.
 *
 * The harness runs on the offline echo brain, which meters no cost, so spend is
 * a stable `$0.00` and the cap never trips mid-suite.
 *
 * Runs against the same live host as `wiring.spec.ts` (see that file's header).
 */

test("the Team page shows a capped teammate's daily budget and omits it for uncapped ones", async ({
  page,
}) => {
  await page.goto("/#/team");

  const cards = page.getByTestId("team-card");
  await expect(cards.first()).toBeVisible({ timeout: 30_000 });

  // The capped teammate: cap and spend both come from the host.
  const writer = cards.filter({ hasText: "Writer" }).first();
  const budget = writer.getByTestId("team-budget");
  await expect(budget).toBeVisible({ timeout: 30_000 });
  await expect(budget).toHaveText(/\$5\.00\/day/);
  await expect(budget).toHaveText(/\$0\.00 spent today/);
  // Under budget, so no paused state.
  await expect(budget).not.toHaveText(/paused/);

  // The uncapped teammates render no budget line whatsoever — absence is the
  // uncapped signal, and "$0.00/day" would be a different (and wrong) claim.
  for (const role of ["Chief Executive", "Engineer"]) {
    const uncapped = cards.filter({ hasText: role }).first();
    await expect(uncapped).toBeVisible({ timeout: 30_000 });
    await expect(uncapped.getByTestId("team-budget")).toHaveCount(0);
  }

  // Host-backed, not localStorage: the line survives a storage-cleared reload.
  await page.evaluate(() => {
    localStorage.clear();
    sessionStorage.clear();
  });
  await page.goto("/#/team");
  await expect(
    page.getByTestId("team-card").filter({ hasText: "Writer" }).first().getByTestId("team-budget"),
  ).toHaveText(/\$5\.00\/day/, { timeout: 30_000 });
});
