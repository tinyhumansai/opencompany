import { expect, test } from "@playwright/test";

/**
 * The Overview is the agent graph. The dive is the part worth guarding: it is
 * the only place in the console where clicking a diagram changes what a whole
 * panel describes.
 */
test("operator dives from the company into a teammate's card and back out", async ({ page }) => {
  const title = `pw-overview-${Date.now()}`;
  const created = await page.request.post("/api/v1/company/tasks", {
    data: { title, note: "seeded by the e2e suite", column: "in_progress", assignee: "Ops Lead" },
  });
  expect(created.ok()).toBeTruthy();

  await page.goto("/#/overview");

  const graph = page.getByRole("img", { name: /Agent graph/ });
  await expect(graph).toBeVisible();
  // The inspector describes the whole company until something is dived into.
  await expect(page.getByText("Hover a node to light its chain")).toBeVisible();

  // Dive: company → teammate.
  await graph.getByRole("button", { name: /^Ops Lead —/ }).click();
  await expect(page.getByRole("heading", { name: "Ops Lead" })).toBeVisible();
  await expect(page.getByRole("button", { name: new RegExp(title) })).toBeVisible();

  // Dive: teammate → card. The inspector carries the card's own detail.
  await page.getByRole("button", { name: new RegExp(title) }).first().click();
  await expect(page.getByRole("heading", { name: title })).toBeVisible();
  await expect(page.getByText("seeded by the e2e suite")).toBeVisible();

  // Escape goes up exactly one level, at every depth.
  await page.keyboard.press("Escape");
  await expect(page.getByRole("heading", { name: "Ops Lead" })).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(page.getByText("Hover a node to light its chain")).toBeVisible();
});

test("the legend doubles as a lens over the graph", async ({ page }) => {
  await page.goto("/#/overview");

  const cards = page.getByRole("button", { name: "Cards" });
  await expect(cards).toHaveAttribute("aria-pressed", "true");

  await cards.click();
  await expect(cards).toHaveAttribute("aria-pressed", "false");
  // Teammates survive their cards being hidden; only the leaves go.
  await expect(page.getByRole("button", { name: "Teammates" })).toHaveAttribute(
    "aria-pressed",
    "true",
  );
});
