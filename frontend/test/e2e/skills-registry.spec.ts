import { expect, test, type Page } from "@playwright/test";

/**
 * A fresh host greets the first visit with a welcome tour rendered over the
 * console, which swallows clicks on the view beneath it. Dismiss it if present.
 */
async function dismissOnboarding(page: Page) {
  const skip = page.getByRole("button", { name: "Skip for now" });
  // `isVisible()` is an immediate check, so it must be preceded by an explicit
  // wait — otherwise it races the tour's first paint and reports "absent" for a
  // tour that is about to cover the page.
  await skip.waitFor({ state: "visible", timeout: 15_000 }).catch(() => {});
  if (await skip.isVisible()) {
    await skip.click();
    await expect(skip).toBeHidden();
  }
}

/**
 * The Skills view's Registry tab must render the **host's** shared library, not
 * a catalog compiled into the console.
 *
 * The console used to ship a hardcoded six-entry array, three of whose slugs
 * (`competitor-analysis`, `social-scheduler`, `meeting-notes`) never existed
 * server-side — installing one persisted a content-less stub. This spec pins
 * the replacement: what the tab lists comes from `GET …/skills/registry`, so a
 * browsable entry is by construction an installable one.
 *
 * Runs against a live host brought up by the harness (see `wiring.spec.ts` for
 * the auth/storage-state arrangement).
 */

/** Slugs the deleted client-side array advertised but the host cannot serve. */
const PHANTOM_NAMES = ["Competitor Analysis", "Social Scheduler", "Meeting Notes"];

test("registry tab lists the live server registry, not a hardcoded array", async ({
  page,
  request,
}) => {
  // The expected count comes from the host, not a literal, so adding a skill to
  // the shared library does not break this spec — it still proves the tab
  // renders exactly what the server serves.
  const served = (await (await request.get("/api/v1/company/skills/registry")).json()) as unknown[];
  expect(served.length).toBeGreaterThanOrEqual(14); // the deleted array had 6

  await page.goto("/#/skills");
  await dismissOnboarding(page);
  await page.getByRole("tab", { name: "Registry" }).click();

  const cards = page.getByTestId("registry-card");
  await expect(cards).toHaveCount(served.length, { timeout: 30_000 });

  // A real library entry is present, with the metadata the host resolved.
  const scan = cards.filter({ hasText: "Competitor Scan" });
  await expect(scan).toHaveCount(1);
  await expect(scan).toContainText("Research");
  // `version` reaches the UI, so an install can be pinned to a revision.
  await expect(scan).toContainText("v1.0.0");

  // None of the phantom entries survive.
  for (const name of PHANTOM_NAMES) {
    await expect(cards.filter({ hasText: name })).toHaveCount(0);
  }
});

test("installing from the registry lands a skill the host can serve", async ({
  page,
  request,
}) => {
  await page.goto("/#/skills");
  await dismissOnboarding(page);
  await page.getByRole("tab", { name: "Registry" }).click();

  const scan = page.getByTestId("registry-card").filter({ hasText: "Competitor Scan" });
  await expect(scan).toBeVisible({ timeout: 30_000 });

  // Skip if a previous run already installed it (the suite shares one host).
  if ((await scan.getByRole("button", { name: "Install" }).count()) > 0) {
    await scan.getByRole("button", { name: "Install" }).click();
  }
  await expect(scan).toContainText("Installed", { timeout: 30_000 });

  // It shows up in the company's effective set…
  await page.getByRole("tab", { name: /^Installed/ }).click();
  await expect(
    page.getByTestId("installed-card").filter({ hasText: "Competitor Scan" }),
  ).toBeVisible({ timeout: 30_000 });

  // …and the registry payload the tab rendered carries no skill bodies: browse
  // stays metadata-only however large the library grows.
  const registry = await request.get("/api/v1/company/skills/registry");
  expect(registry.ok()).toBeTruthy();
  const rows = (await registry.json()) as Array<Record<string, unknown>>;
  expect(rows.length).toBeGreaterThanOrEqual(14);
  for (const row of rows) {
    expect(row).not.toHaveProperty("body");
  }
});
