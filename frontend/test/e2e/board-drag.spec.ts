import { expect, test, type APIRequestContext, type Locator, type Page } from "@playwright/test";

/**
 * Issue #334 — moving a card out of In review by dragging it onto Done.
 *
 * QA reported this as "a card cannot be moved out of In review": the drop
 * appeared to work, the counts did not change, and nothing said why. Two
 * separate things produced that:
 *
 * 1. **The last column was mostly off-window.** `SidebarInset` is a flex item
 *    beside the sidebar, and a flex item's default `min-width: auto` floors it
 *    at its content's min-content width — so the inset measured a full window
 *    wide while starting a sidebar's width in from the left, and its trailing
 *    strip hung outside the window inside a wrapper that clips and does not
 *    scroll. Scrolled hard right, "Done" showed a ~64px sliver hugging the
 *    window edge. At five columns the strip held only slack; the sixth (#301)
 *    pushed a real column into it.
 * 2. **Every miss was silent.** The drop read the dragged id out of React state
 *    alone and returned wordlessly when it was absent, and the pixels between
 *    and around the columns had no drop handler at all — so a near miss, which
 *    a 64px target makes the common case, did nothing and said nothing.
 *
 * These tests are the guard for both, and for the third leg of the fix: the
 * board now scrolls itself while a drag sits on its edge, because HTML5
 * drag-and-drop does not scroll a nested scroll container, so a column parked
 * off-screen could not be reached by the gesture at all.
 *
 * They drive a real browser against a live host, which the issue calls out as
 * the only way this is observable. CI does not run this suite.
 */

const API = "/api/v1/company";

/** Board order (issue #301), so a column can be addressed by position. */
const COLUMNS = ["To-do", "Planning", "In progress", "Paused", "In review", "Done"];
const IN_REVIEW = COLUMNS.indexOf("In review");
const DONE = COLUMNS.indexOf("Done");

test.beforeEach(async ({ page }) => {
  // The first-run tour opens a modal over the board and swallows clicks.
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

const column = (page: Page, index: number) => page.locator("div.w-72").nth(index);
const board = (page: Page) => page.locator("div.overflow-x-auto").first();

/** Seeds a card straight into In review and opens the board on it. */
async function seedInReview(page: Page, request: APIRequestContext, title: string) {
  const created = await request.post(`${API}/tasks`, { data: { title } });
  expect(created.ok()).toBeTruthy();
  const id = (await created.json()).id as string;
  const moved = await request.patch(`${API}/tasks/${id}`, { data: { column: "in_review" } });
  expect(moved.ok()).toBeTruthy();

  await page.goto("/#/tasks");
  await dismissTour(page);
  const card = page.locator("div[draggable=true]").filter({ hasText: title }).first();
  await expect(card).toBeVisible({ timeout: 15_000 });
  return { id, card };
}

/**
 * A real pointer drag: press the card, walk to `(toX, toY)` in small steps so
 * Chromium promotes it to a drag session, and release.
 *
 * `dragTo` is not usable for these cases — it scrolls the target into view
 * before pressing, which is precisely the step the operator cannot take
 * mid-gesture and which this fix exists to supply.
 */
async function handDrag(page: Page, card: Locator, toX: number, toY: number) {
  const box = await card.boundingBox();
  if (!box) throw new Error("the card has no box to grab");
  const fromX = box.x + box.width / 2;
  const fromY = box.y + box.height / 2;
  await page.mouse.move(fromX, fromY);
  await page.mouse.down();
  for (let step = 1; step <= 12; step += 1) {
    await page.mouse.move(fromX + ((toX - fromX) * step) / 12, fromY + ((toY - fromY) * step) / 12);
    await page.waitForTimeout(40);
  }
  await page.waitForTimeout(150);
  await page.mouse.up();
}

test("the whole board fits the window, so the last column is a full drop target", async ({
  page,
}) => {
  await page.goto("/#/tasks");
  await dismissTour(page);
  await expect(column(page, DONE)).toHaveCount(1);

  // The regression this guards: the shell used to be a sidebar's width wider
  // than the window, and the overshoot was clipped by an ancestor that cannot
  // be scrolled — so those pixels were unreachable by any means.
  const overshoot = await page.evaluate(() => {
    const inset = document.querySelector('[data-slot="sidebar-inset"]');
    if (!inset) throw new Error("no sidebar inset");
    return Math.round(inset.getBoundingClientRect().right - window.innerWidth);
  });
  expect(overshoot).toBeLessThanOrEqual(0);

  // Scrolled to the end, Done is a whole column inside the window rather than
  // a sliver of one pinned against the edge.
  await board(page).evaluate((el) => {
    el.scrollLeft = el.scrollWidth;
  });
  await page.waitForTimeout(300);
  const done = await column(page, DONE).boundingBox();
  const width = page.viewportSize()!.width;
  expect(done).not.toBeNull();
  expect(Math.min(done!.x + done!.width, width) - done!.x).toBeGreaterThan(200);
});

test("a card drags from In review to Done, and the board scrolls to get there", async ({
  page,
  request,
}) => {
  const title = `e2e in-review to done ${Date.now()}`;
  const { id, card } = await seedInReview(page, request, title);

  // Park the board so In review is on screen and Done is not. This is the
  // operator's actual starting position, and the case the gesture could not
  // finish before: nothing scrolls a nested container during an HTML5 drag.
  const scrolled = await board(page).evaluate((el) => {
    el.scrollLeft = Math.max(0, el.scrollWidth - el.clientWidth - 260);
    return el.scrollLeft;
  });
  await page.waitForTimeout(300);

  const box = await card.boundingBox();
  if (!box) throw new Error("the seeded card has no box");
  const viewport = page.viewportSize()!;

  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  // Ride the right edge and let the board bring Done to the pointer.
  for (let tick = 0; tick < 25; tick += 1) {
    await page.mouse.move(viewport.width - 8 - (tick % 2), box.y + 160);
    await page.waitForTimeout(60);
  }
  const rode = await board(page).evaluate((el) => el.scrollLeft);
  expect(rode, "the board follows a drag held against its edge").toBeGreaterThan(scrolled);

  const done = await column(page, DONE).boundingBox();
  if (!done) throw new Error("Done has no box");
  await page.mouse.move(done.x + done.width / 2, done.y + done.height / 2);
  await page.waitForTimeout(150);
  await page.mouse.up();

  // The host's own record, which is the only thing that settles whether the
  // move happened rather than merely appeared to.
  await expect
    .poll(async () => (await (await request.get(`${API}/tasks/${id}`)).json()).task.column, {
      timeout: 15_000,
    })
    .toBe("done");

  // And the counts the operator was watching.
  await expect(column(page, DONE)).toContainText(title);
  await expect(column(page, IN_REVIEW)).not.toContainText(title);
});

test("a drop that misses every column says so instead of doing nothing", async ({
  page,
  request,
}) => {
  const title = `e2e drag near miss ${Date.now()}`;
  const { id, card } = await seedInReview(page, request, title);

  await board(page).evaluate((el) => {
    el.scrollLeft = el.scrollWidth;
  });
  await page.waitForTimeout(300);

  // The trailing gutter past the last column: board pixels that belong to no
  // column. This is where a near miss lands, and it used to swallow the whole
  // gesture without a word.
  const gutter = await page.locator("div[aria-hidden].w-4").first().boundingBox();
  if (!gutter) throw new Error("no trailing gutter");
  await handDrag(page, card, gutter.x + gutter.width / 2, gutter.y + 200);

  await expect(page.getByText("Drop the card on a column to move it.")).toBeVisible({
    timeout: 5_000,
  });
  // Saying so is not the same as moving it: the card must stay where it was.
  expect((await (await request.get(`${API}/tasks/${id}`)).json()).task.column).toBe("in_review");
  await expect(column(page, IN_REVIEW)).toContainText(title);
});

test("a move the host refuses names the card, the column, and the reason", async ({
  page,
  request,
}) => {
  const title = `e2e refused move ${Date.now()}`;
  const { card } = await seedInReview(page, request, title);

  // The host accepts in_review → done, so the refusal has to be induced. This
  // asserts the console's half of the contract: whatever the host says, the
  // operator reads it rather than watching the card snap back in silence.
  await page.route("**/tasks/*", async (route) => {
    if (route.request().method() !== "PATCH") return route.fallback();
    await route.fulfill({
      status: 400,
      contentType: "application/json",
      body: JSON.stringify({ code: "invalid_request", error: "the board is read-only right now" }),
    });
  });

  await board(page).evaluate((el) => {
    el.scrollLeft = el.scrollWidth;
  });
  await page.waitForTimeout(300);
  const done = await column(page, DONE).boundingBox();
  if (!done) throw new Error("Done has no box");
  await handDrag(page, card, done.x + done.width / 2, done.y + done.height / 2);

  await expect(page.getByText(`Could not move "${title}" to Done.`)).toBeVisible({
    timeout: 5_000,
  });
  await expect(page.getByText("the board is read-only right now")).toBeVisible();
  // Refused means refused: the optimistic move is rolled back.
  await expect(column(page, IN_REVIEW)).toContainText(title);
});
