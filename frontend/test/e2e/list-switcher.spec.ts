import { expect, test, type Page } from "@playwright/test";

/**
 * Issue #1284 (final shape, Rule 2) — the title dropdown switcher.
 *
 * Three earlier shapes were tried and rejected: a sidebar row per list
 * (unusable at the 12-declared-list cap), a collapsible sidebar section
 * (wrong premise — declared lists are read occasionally, not worked daily),
 * and a tab strip (solved scaling but taxed the most-visited screen with a
 * permanent band). What shipped: the page's own `<h1>` becomes the switcher —
 * `Tasks ▾` — opening a menu of every list (current one marked, open counts
 * shown), with no new element added to the page. See
 * `docs/spec/runtime/ledgers-console-ia.md`'s Rule 2 for the full reasoning.
 *
 * This spec covers: opening the menu, selecting a list, the address
 * updating, a deep link straight to a non-default list, the back button, and
 * that `#/tasks/<id>` (the card detail page) still resolves untouched. Plus a
 * round of fixes from real usage: "New list" opens the wizard in place
 * (rather than silently navigating to Manage Lists, which is what its
 * `onClick` handler collided with `onManageLists` into doing), the browser
 * Back button closes that wizard instead of skipping past it and Manage
 * Lists, Manage Lists lives in Work rather than Company, every count shows
 * (zero included, so it never reads as "no information"), and an unknown or
 * retired list's address still recovers through the same switcher.
 */

const API = "/api/v1/company";

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    const seen = JSON.stringify({ skipped: true, seenAt: Date.now() });
    for (const key of ["oc-tour:single", "oc-tour:e2e-harness-co", "oc-tour:null"]) {
      window.localStorage.setItem(key, seen);
    }
  });
});

async function dismissTour(page: Page) {
  const skip = page.getByRole("button", { name: "Skip for now" });
  for (let attempt = 0; attempt < 5; attempt += 1) {
    if (!(await skip.isVisible().catch(() => false))) return;
    await skip.click({ force: true }).catch(() => {});
    await page.waitForTimeout(300);
  }
  await expect(skip).toHaveCount(0);
}

function switcherTrigger(page: Page) {
  return page.getByTestId("list-switcher-trigger");
}

function switcherItem(page: Page, slug: string) {
  return page.getByTestId(`list-switcher-${slug}`);
}

test("Work is one nav row, landing on Tasks by default with the title as the switcher", async ({
  page,
}) => {
  await page.goto("/#/overview");
  await dismissTour(page);

  await expect(page.locator('[data-tour="nav-ledgers"]')).toHaveCount(1);
  await expect(page.locator('[data-tour="nav-ledgers"]').getByRole("button")).toHaveText("Work");

  await page.locator('[data-tour="nav-ledgers"]').getByRole("button").click();
  // The bare `#/ledgers` address resolves to Tasks and the URL follows —
  // consistent with `#/tasks` itself already rewriting to `#/ledgers/tasks`
  // (issue #1140) — rather than staying bare while the page shows Tasks.
  await expect.poll(() => new URL(page.url()).hash).toBe("#/ledgers/tasks");
  await expect(switcherTrigger(page)).toHaveText(/Tasks/);
  await expect(page.getByTestId("ledger-board")).toBeVisible({ timeout: 15_000 });
});

test("opening the switcher lists every list, current one marked, with open counts", async ({
  page,
}) => {
  await page.goto("/#/ledgers");
  await dismissTour(page);

  await switcherTrigger(page).click();
  await expect(switcherItem(page, "goals")).toBeVisible();
  await expect(switcherItem(page, "decisions")).toBeVisible();
  // The current list (Tasks) is in the menu too, not just "somewhere else to
  // go" — that's the property a switcher has and a plain link does not.
  await expect(switcherItem(page, "tasks")).toBeVisible();
});

test("selecting a list from the switcher swaps the page and updates the address", async ({
  page,
}) => {
  await page.goto("/#/ledgers");
  await dismissTour(page);

  await switcherTrigger(page).click();
  await switcherItem(page, "goals").click();

  await expect.poll(() => new URL(page.url()).hash).toBe("#/ledgers/goals");
  await expect(switcherTrigger(page)).toHaveText(/Goals/);
  await expect(page.getByRole("heading", { name: "Goals" })).toBeVisible({ timeout: 15_000 });
});

test("a deep link to a non-default list opens that list directly, and the back button returns", async ({
  page,
}) => {
  await page.goto("/#/ledgers/decisions");
  await dismissTour(page);

  await expect(switcherTrigger(page)).toHaveText(/Decisions/);

  await switcherTrigger(page).click();
  await switcherItem(page, "goals").click();
  await expect.poll(() => new URL(page.url()).hash).toBe("#/ledgers/goals");

  await page.goBack();
  await expect.poll(() => new URL(page.url()).hash).toBe("#/ledgers/decisions");
  await expect(switcherTrigger(page)).toHaveText(/Decisions/);
});

test("#/tasks/<id> still opens the card detail page, untouched by the switcher", async ({
  page,
  request,
}) => {
  const title = `e2e switcher card ${Date.now()}`;
  const seeded = await request.post(`${API}/tasks`, { data: { title } });
  expect(seeded.ok()).toBeTruthy();
  const id = (await seeded.json()).id as string;

  await page.goto(`/#/tasks/${id}`);
  await dismissTour(page);

  await expect(page.getByRole("heading", { name: title })).toBeVisible({ timeout: 15_000 });
  expect(new URL(page.url()).hash).toBe(`#/tasks/${id}`);
});

test("List is a navigable view that survives a task-detail round trip", async ({
  page,
  request,
}) => {
  const title = `e2e list route card ${Date.now()}`;
  const seeded = await request.post(`${API}/tasks`, { data: { title } });
  expect(seeded.ok()).toBeTruthy();
  const id = (await seeded.json()).id as string;

  await page.goto("/#/ledgers/tasks");
  await dismissTour(page);

  await page.getByRole("button", { name: "List", exact: true }).click();
  await expect.poll(() => new URL(page.url()).hash).toBe("#/ledgers/tasks?view=list");

  const detailLink = page.locator(`a[href="#/tasks/${id}?view=list"]`);
  await expect(detailLink).toHaveText(title);
  await detailLink.click();
  await expect.poll(() => new URL(page.url()).hash).toBe(`#/tasks/${id}?view=list`);

  await page.goBack();
  await expect.poll(() => new URL(page.url()).hash).toBe("#/ledgers/tasks?view=list");
  await expect(page.getByRole("button", { name: "Board" })).toBeVisible();
});

test("Back after choosing List returns to Board before leaving Work", async ({ page }) => {
  await page.goto("/#/overview");
  await dismissTour(page);

  await page.locator('[data-tour="nav-ledgers"]').getByRole("button").click();
  await expect.poll(() => new URL(page.url()).hash).toBe("#/ledgers/tasks");

  await page.getByRole("button", { name: "List", exact: true }).click();
  await expect.poll(() => new URL(page.url()).hash).toBe("#/ledgers/tasks?view=list");

  await page.goBack();
  await expect.poll(() => new URL(page.url()).hash).toBe("#/ledgers/tasks");
  await expect(page.getByRole("button", { name: "List", exact: true })).toBeVisible();
});

test("Manage lists opens from the switcher, in Work — not Company", async ({ page }) => {
  await page.goto("/#/ledgers");
  await dismissTour(page);

  await switcherTrigger(page).click();
  await page.getByTestId("list-switcher-manage").click();
  await expect.poll(() => new URL(page.url()).hash).toBe("#/ledgers/manage");
  await expect(page.getByRole("heading", { name: "Manage lists" })).toBeVisible({
    timeout: 15_000,
  });

  // The Company page never gets a Manage Lists button of its own (issue
  // #1284) — the switcher is the only way in.
  await page.getByTestId("lists-back").click();
  await page.locator('[data-tour="nav-company"]').getByRole("button").click();
  await expect(page.getByTestId("company-manage-lists")).toHaveCount(0);
});

test("New list opens the declare wizard in place, over the current list", async ({ page }) => {
  await page.goto("/#/ledgers/goals");
  await dismissTour(page);

  await switcherTrigger(page).click();
  await page.getByTestId("list-switcher-new").click();
  await expect(page.getByRole("heading", { name: "New list" })).toBeVisible();

  // "In place" means literally that: the address still names Goals, the
  // wizard is layered over it, not a navigation to Manage Lists.
  expect(new URL(page.url()).hash).toContain("#/ledgers/goals");
  await expect(page.getByRole("heading", { name: "Manage lists" })).toHaveCount(0);
});

test("the browser Back button closes the in-place wizard rather than skipping past it", async ({
  page,
}) => {
  await page.goto("/#/ledgers/goals");
  await dismissTour(page);

  await switcherTrigger(page).click();
  await page.getByTestId("list-switcher-new").click();
  await expect(page.getByRole("heading", { name: "New list" })).toBeVisible();

  await page.goBack();

  // Closed, not skipped past — still on Goals, not bounced out to Manage
  // Lists or Tasks the way local-only dialog state would let Back do.
  await expect(page.getByRole("heading", { name: "New list" })).toHaveCount(0);
  await expect(switcherTrigger(page)).toHaveText(/Goals/);
  await expect.poll(() => new URL(page.url()).hash).toBe("#/ledgers/goals");
});

test("counts show zero, not nothing, so a zero never reads as missing information", async ({
  page,
}) => {
  await page.goto("/#/ledgers");
  await dismissTour(page);

  await switcherTrigger(page).click();
  // Every entry — including a list with zero open rows — carries a number.
  const goalsCount = switcherItem(page, "goals").locator("span").last();
  await expect(goalsCount).toHaveText(/^\d+$/);
});

test("an unknown or retired list's address recovers through the same switcher", async ({
  page,
}) => {
  await page.goto("/#/ledgers/this-slug-does-not-exist");
  await dismissTour(page);

  await expect(switcherTrigger(page)).toHaveText(/Not found/);
  await expect(page.getByText(/Pick another from the title menu/)).toBeVisible();

  // The switcher itself is still the way back — no dead end.
  await switcherTrigger(page).click();
  await switcherItem(page, "tasks").click();
  await expect.poll(() => new URL(page.url()).hash).toBe("#/ledgers/tasks");
  await expect(switcherTrigger(page)).toHaveText(/Tasks/);
});
