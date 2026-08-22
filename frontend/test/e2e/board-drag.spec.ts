import { expect, test, type APIRequestContext, type Locator, type Page } from "@playwright/test";

/**
 * Issue #334 — moving a card out of Working by dragging it onto Done.
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
 * board scrolls itself while a drag sits on its edge, because HTML5
 * drag-and-drop does not scroll a nested scroll container, so a column parked
 * off-screen could not be reached by the gesture at all.
 *
 * # Why this one drives `#/ledgers/tasks`
 *
 * Because that is where the board is. `LedgerBoard` renders every ledger's
 * columns under `#/ledgers/<slug>`, and the task board is one of them — the
 * `tasks` ledger — since issue #1140 retired the standalone Tasks page that had
 * been showing the same records through the same component.
 *
 * The drag mechanics, all three fixes above, live in that shared component. So
 * this spec and `board-columns.spec.ts` now drive the same address for
 * different claims: this one owns the *gesture* (geometry, the miss, the edge
 * scroll) and that one owns the board's *shape* (which columns, in what order,
 * and where new work enters).
 *
 * Two things about the port are worth stating, because both are places where a
 * lazy rewrite would have quietly stopped testing anything:
 *
 * * The column labels come from the **host** now (the `tasks` ledger's statuses,
 *   built from `src/ledger/board.rs`). `COLUMNS` below is still spelled out
 *   rather than read from the API, deliberately: a test that asked the host what
 *   to expect and then asserted the host said it would pass against any answer.
 *   Rust pins the same literals in `the_labels_are_the_ones_every_surface_renders`.
 * * The board is addressed by `data-testid`, not by a utility class. The old
 *   spec located it as "the first `div.overflow-x-auto`", which was true of a
 *   screen that held nothing else; the Ledgers screen has a nav and a detail
 *   pane, and inferring the board from a Tailwind class would have made this
 *   spec fail for reasons that have nothing to do with dragging.
 *
 * They drive a real browser against a live host, which the issue calls out as
 * the only way this is observable. CI does not run this suite.
 */

const API = "/api/v1/company";

/** The board's address now that the standalone screen is gone. */
const BOARD = "/#/ledgers/tasks";

/**
 * Board order (issue #301), so a column can be addressed by position.
 *
 * These are the host's labels — see the note above on why they are written out
 * here rather than read back from the ledger.
 */
const COLUMNS = ["Pending", "Working", "Done"];
const WORKING = COLUMNS.indexOf("Working");
const DONE = COLUMNS.indexOf("Done");

test.beforeEach(async ({ page }) => {
  // The first-run tour opens a modal over the console and swallows clicks.
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

const board = (page: Page) => page.getByTestId("ledger-board");
const column = (page: Page, index: number) => board(page).getByTestId("board-column").nth(index);

/**
 * Opens every column that has collapsed itself to a rail (issue #1101).
 *
 * The board shows the three phases of a ledger (Pending / Working / Done) and
 * collapses an empty phase to a ~40px rail. The edge-scroll geometry claims in
 * this file need the board wider than its pane — a board of two rails and one
 * column does not overflow at all — so this puts the board back to its full
 * three-phase width before measuring, which is also the state a person
 * reaches by clicking a rail open. Without it the edge-scroll assertion would
 * pass by never being tested.
 *
 * Clicking a rail pins it open, which is the operator's own control — so this
 * puts the board in a state a person can reach, rather than defeating the
 * feature with a style override.
 */
async function expandAll(page: Page) {
  const rails = board(page).locator("button[aria-label^='Expand ']");
  const collapsed = board(page).locator('[data-collapsed="true"]');
  // Bounded: each click removes one rail, and the board has three phases.
  //
  // The loop exits on COLLAPSED COLUMNS, not on rails. Those are different
  // conditions, and the difference is why this helper used to return with the
  // board still collapsed: a column carries `data-collapsed` while its Expand
  // rail is a separate element that need not be rendered in the same frame.
  // Waiting for the rails to disappear therefore said "expanded" one render too
  // early, and the caller's own `[data-collapsed="true"]` assertion failed five
  // seconds later and forty lines away from the cause — intermittently, on pull
  // requests that touched none of this.
  // Bounded twice, because two different things can go wrong and one bound
  // cannot cover both. The click budget counts CLICKS: each one removes a rail
  // and the board has three phases, so eight is already generous. Counting
  // loop passes instead would let a run that never clicked anything exhaust
  // the budget — eight transient 100 ms waits are eight passes and zero
  // progress, and the assertion below would then fail on columns this helper
  // had no chance to expand, which is the same intermittent shape it exists to
  // remove. The deadline covers what the click budget then cannot: a rail that
  // never arrives at all, which would otherwise spin here forever.
  const deadline = Date.now() + 10_000;
  let clicks = 0;
  while (clicks < 8 && Date.now() < deadline) {
    if ((await collapsed.count()) === 0) return;
    if ((await rails.count()) === 0) {
      // Collapsed with nothing to click: the render is mid-flight. Give it the
      // turn it needs rather than spending click budget on nothing.
      await page.waitForTimeout(100);
      continue;
    }
    await rails.first().click();
    clicks += 1;
  }
  // The postcondition callers actually depend on, so a genuine failure names
  // the board's state rather than the rails' absence.
  await expect(collapsed).toHaveCount(0);
}

/** Opens the board and waits for the host's columns to arrive. */
async function openBoard(page: Page) {
  await page.goto(BOARD);
  await dismissTour(page);
  // The columns are a read, not a literal, so the board is not itself until it
  // has one. Every assertion below measures a column's box; none of them mean
  // anything against a board still showing none.
  await expect(column(page, DONE)).toHaveCount(1, { timeout: 15_000 });
  await expandAll(page);
}

/**
 * Seeds a card straight into the `in_review` stage and opens the board on it.
 *
 * The stage still exists and is still what a finished run lands in; since issue
 * #1512 it is not a *column*, so the card renders under Working. The seed
 * writes the stage word directly because that is the state this spec is about —
 * a card one drop away from Done, which is where #334's report came from.
 */
async function seedInReview(page: Page, request: APIRequestContext, title: string) {
  const created = await request.post(`${API}/tasks`, { data: { title } });
  expect(created.ok()).toBeTruthy();
  const id = (await created.json()).id as string;
  const moved = await request.patch(`${API}/tasks/${id}`, { data: { column: "in_review" } });
  expect(moved.ok()).toBeTruthy();

  await openBoard(page);
  const card = page.locator("[draggable=true]").filter({ hasText: title }).first();
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
  await openBoard(page);

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

test("a card drags from Working to Done, and the board scrolls to get there", async ({
  page,
  request,
}) => {
  const title = `e2e in-review to done ${Date.now()}`;
  const { id, card } = await seedInReview(page, request, title);

  // Three phases of ~260px no longer overflow the default 1280px window, so
  // the board cannot scroll and the park below lands on zero. Narrow the
  // window so the board genuinely overflows: the sidebar stays expanded above
  // `md` (768px), and the park needs ~260px of overflow to land mid-range.
  const pane = () => board(page).evaluate((el) => el.clientWidth);
  const wide = await pane();
  await page.setViewportSize({ width: 800, height: 720 });
  // The board learns its own width through a `ResizeObserver`, so the frame
  // after `setViewportSize` is not the frame it has re-measured on. Everything
  // below turns on that measurement — which columns collapse, and by how much
  // the board overflows — so wait for it rather than for a timeout.
  await expect.poll(pane).toBeLessThan(wide);

  // And expand *again*, because narrowing the window is what created the rails.
  // `openBoard` ran `expandAll` at 1280px, where three phases fit and the board
  // therefore collapses nothing — so it found no rails and pinned nothing.
  // Narrowing is what makes the columns stop fitting, and an empty Done folds
  // itself into a ~40px rail. Dragging onto that rail is a different gesture
  // from the one this test is about — the rail opens under the drag, reflowing
  // the board mid-drop — and it also leaves the board barely wider than its
  // pane, so the park below lands on zero and the edge-scroll claim stops being
  // tested. Pinning every phase open puts the board back to the full
  // three-phase width the assertions below assume.
  await expandAll(page);
  await expect(board(page).locator('[data-collapsed="true"]')).toHaveCount(0);

  // Park the board so Working is on screen and Done is not. This is the
  // operator's actual starting position, and the case the gesture could not
  // finish before: nothing scrolls a nested container during an HTML5 drag.
  const scrolled = await board(page).evaluate((el) => {
    el.scrollLeft = Math.max(0, el.scrollWidth - el.clientWidth - 260);
    return el.scrollLeft;
  });
  await page.waitForTimeout(300);

  const box = await card.boundingBox();
  if (!box) throw new Error("the seeded card has no box");
  // The **board's** right edge, not the window's. On its own screen the board
  // ran to the window edge and the two were the same pixel; inside Ledgers it
  // sits past a nav and inside the section's padding. Riding the window edge
  // here would hold the pointer over the page *outside* the board, where its
  // `dragover` never fires — the test would fail for a reason that has nothing
  // to do with the behaviour, and the band is defined relative to the board
  // anyway.
  const edge = await board(page).boundingBox();
  if (!edge) throw new Error("the board has no box");
  const rightEdge = edge.x + edge.width - 8;

  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  // Ride the right edge and let the board bring Done to the pointer.
  // Vertically at the board's middle, not a fixed offset below the card. The
  // board is only as tall as the pane leaves it, and a pointer held below its
  // bottom edge is a pointer off the board — where its `dragover` never fires.
  const rideY = edge.y + edge.height / 2;
  for (let tick = 0; tick < 25; tick += 1) {
    await page.mouse.move(rightEdge - (tick % 2), rideY);
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
  // move happened rather than merely appeared to. It is also the assertion the
  // port could most easily have lost: the board writes through `patchTask` here
  // exactly as it did on its own screen, because entering a column fires work
  // and `record_entry` is refused for this ledger.
  await expect
    .poll(async () => (await (await request.get(`${API}/tasks/${id}`)).json()).task.column, {
      timeout: 15_000,
    })
    .toBe("done");

  // And the counts the operator was watching.
  await expect(column(page, DONE)).toContainText(title);
  await expect(column(page, WORKING)).not.toContainText(title);
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
  const gutter = await board(page).locator("div[aria-hidden].w-4").first().boundingBox();
  if (!gutter) throw new Error("no trailing gutter");
  // The gutter's own middle, rather than a fixed offset down it. It is a flex
  // item stretched to the tallest column, so on a board holding a card or two
  // it is short — and a fixed 200px would drop *below* the board entirely,
  // which is a miss of a different kind than the one under test.
  await handDrag(page, card, gutter.x + gutter.width / 2, gutter.y + gutter.height / 2);

  await expect(page.getByText("Drop the card on a column to move it.")).toBeVisible({
    timeout: 5_000,
  });
  // Saying so is not the same as moving it: the card must stay where it was.
  // Unmoved: still the `in_review` stage, which reads as the Working column.
  const unmoved = (await (await request.get(`${API}/tasks/${id}`)).json()).task;
  expect(unmoved.stage).toBe("in_review");
  expect(unmoved.column).toBe("working");
  await expect(column(page, WORKING)).toContainText(title);
});

test("a move the host refuses names the card, the column, and the reason", async ({
  page,
  request,
}) => {
  const title = `e2e refused move ${Date.now()}`;
  const { card } = await seedInReview(page, request, title);

  // The host accepts working → done, so the refusal has to be induced. This
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

  // "Done" is the host's label for the column, reaching the toast through the
  // ledger's statuses rather than through a list the console keeps.
  await expect(page.getByText(`Could not move "${title}" to Done.`)).toBeVisible({
    timeout: 5_000,
  });
  await expect(page.getByText("the board is read-only right now")).toBeVisible();
  // Refused means refused: the optimistic move is rolled back.
  await expect(column(page, WORKING)).toContainText(title);
});
