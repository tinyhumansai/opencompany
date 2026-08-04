import { expect, test } from "@playwright/test";

/**
 * The Overview command centre reads the live board and lets the operator dive
 * into it. The dive is the part worth guarding: it is the only place in the
 * console where clicking a diagram changes what a whole panel describes.
 */
test("operator dives from the company into a teammate's card and back out", async ({ page }) => {
  const title = `pw-overview-${Date.now()}`;
  const created = await page.request.post("/api/v1/company/tasks", {
    data: { title, note: "seeded by the e2e suite", column: "in_progress", assignee: "Ops Lead" },
  });
  expect(created.ok()).toBeTruthy();

  await page.goto("/#/overview");

  // The pulse row and the state line come from the same live data.
  await expect(page.getByText("command centre")).toBeVisible();
  await expect(page.getByText("In flight")).toBeVisible();

  const map = page.getByRole("img", { name: /Company map/ });
  await expect(map).toBeVisible();

  // Dive: company → teammate. The panel beside the map re-scopes with it.
  await map.locator("g[role=button]").filter({ hasText: "Ops Lead" }).first().click();
  await expect(page.getByRole("navigation", { name: "Map depth" })).toContainText("Ops Lead");
  await expect(page.getByRole("button", { name: title })).toBeVisible();

  // Dive: teammate → card.
  await page.getByRole("button", { name: title }).click();
  await expect(page.getByRole("heading", { name: title })).toBeVisible();
  await expect(page.getByText("seeded by the e2e suite")).toBeVisible();

  // Dive out with the keyboard, the same gesture at every depth.
  await page.keyboard.press("Escape");
  await expect(page.getByRole("navigation", { name: "Map depth" })).toContainText("Ops Lead");
  await page.keyboard.press("Escape");
  await expect(page.getByText("Everything at once")).toBeVisible();
});
