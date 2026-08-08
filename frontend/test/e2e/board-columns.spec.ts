import { expect, test, type Page } from "@playwright/test";

import { LIVE_BRAIN } from "./capabilities";

/**
 * Issue #301 — the board's shape, asserted against a live host.
 *
 * This spec is the drift guard for the console half of a two-language mirror.
 * `BOARD_COLUMNS` (`src/ports/tasks.rs`) is the source of truth and the REST
 * write boundary rejects a card against it; `TASK_COLUMNS`
 * (`src/lib/tasks-sample.ts`) is what actually renders. A Rust test cannot see
 * the TS list, and there is no frontend unit runner here (the console's scripts
 * are typecheck / build / e2e only) — so the two lists are only ever joined by
 * a test that drives the rendered board against a real host. A column present
 * on one side alone shows up here as either one that never renders or one whose
 * writes always 400.
 *
 * Generating one list from the other is the durable fix and stays deferred: it
 * needs a build step across a separate npm build that this crate does not have.
 */

const API = "/api/v1/company";

/** Epic #183 §3's vocabulary, in board order. */
const EXPECTED_COLUMNS = [
  "To-do",
  "Planning",
  "In progress",
  "Paused",
  "In review",
  "Done",
];

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

/** The column headers the board actually renders, left to right. */
function columnLabels(page: Page) {
  return page.locator("div.w-72 > div > span.text-sm.font-medium");
}

test("the board renders the six #183 columns in order, with Backlog gone", async ({ page }) => {
  await page.goto("/#/tasks");
  await dismissTour(page);

  await expect(columnLabels(page)).toHaveText(EXPECTED_COLUMNS);
  // The collapse, stated as its own assertion: Backlog is not a column any more.
  await expect(page.getByText("Backlog", { exact: true })).toHaveCount(0);
});

test("new work enters through one prompt box and lands in To-do", async ({ page, request }) => {
  await page.goto("/#/tasks");
  await dismissTour(page);

  // Exactly one entry point on the whole board (issue #206's rule, kept).
  const addTask = page.getByRole("button", { name: "Add task" });
  await expect(addTask).toHaveCount(1);
  await addTask.click();
  await expect(page.getByRole("heading", { name: "New task" })).toBeVisible();

  // One field. Title / Note / Priority / Assignee are gone from create — the
  // host defaults the last two and the card's edit surface owns them (#278).
  await expect(page.locator("#new-prompt")).toBeVisible();
  for (const gone of ["#new-title", "#new-note", "#new-assignee"]) {
    await expect(page.locator(gone)).toHaveCount(0);
  }

  // A prompt longer than the title cap: the title is shortened and the full
  // text survives in the note, so nothing the operator typed is lost.
  const marker = `e2e board shape ${Date.now()}`;
  const long = `${marker} — and then a great deal more detail that runs well past the eighty character title cap so the note has to carry it`;
  await page.locator("#new-prompt").fill(long);
  await page.getByRole("button", { name: "Create", exact: true }).click();

  type Row = { title: string; note?: string; column: string };
  const find = async (): Promise<Row | undefined> => {
    const rows = (await (await request.get(`${API}/tasks`)).json()) as Row[];
    return rows.find((r) => r.title.startsWith(marker));
  };

  await expect.poll(async () => (await find()) !== undefined, { timeout: 15_000 }).toBe(true);
  const created = (await find())!;

  expect(created.column).toBe("todo");
  expect(created.title.length).toBeLessThanOrEqual(81); // 80 + the ellipsis
  expect(created.note).toBe(long);
});

/**
 * Issue #501. This test states a **no-planner** contract, and only a host
 * without one keeps it.
 *
 * Its own comment used to say the no-dispatch assertion "lets the column ship
 * ahead of epic #183 §4's auto-advance". §4 has since landed as the planning
 * station (`src/harness/planning.rs`, issue #337), and a card entering Planning
 * on a planner-attached host now edge-fires exactly one pass and is **settled**
 * by it — never left sitting in `planning`:
 *
 * | pass outcome | where the card lands |
 * |---|---|
 * | a plan, nothing blocking, a valid assignee | `in_progress` — and the dispatch edge fires |
 * | a plan, a hard prerequisite missing | `todo`, with the gap on the note |
 * | the pass itself failed | `todo`, with the reason on the note |
 *
 * So on the live-brain lane both of this test's claims are false by design: the
 * card does not stay in `planning`, and the first row dispatches. `plan_task`
 * is a `#[cfg(feature = "openhuman")]` no-op without the harness, so the
 * assertions below remain exactly right on the default lane, which is where
 * this runs.
 *
 * The skip is therefore INVERTED — `skip(LIVE_BRAIN)`, not `skip(!LIVE_BRAIN)`.
 * This test needs the absence of a brain, which is the opposite of every other
 * capability skip in the suite. The harness contract is asserted by the test
 * below instead, so the lane loses no coverage.
 */
test("dragging into Planning moves the card without dispatching it", async ({ page, request }) => {
  test.skip(
    LIVE_BRAIN,
    "asserts Planning is inert, which is only true without a planner; the harness " +
      "contract is covered by the live-brain test below. Issue #501.",
  );
  const title = `e2e planning drag ${Date.now()}`;
  const seeded = await request.post(`${API}/tasks`, { data: { title } });
  expect(seeded.ok()).toBeTruthy();
  const id = (await seeded.json()).id as string;

  await page.goto("/#/tasks");
  await dismissTour(page);

  const card = page.locator("div[draggable=true]").filter({ hasText: title }).first();
  await expect(card).toBeVisible({ timeout: 15_000 });

  // Playwright's dragTo does not drive React's HTML5 drag handlers reliably
  // here, so the drop is dispatched directly at the Planning column.
  //
  // One **shared `DataTransfer`** across the three events, and it is
  // load-bearing (issue #501). A bare `dispatchEvent("dragstart")` builds a
  // `DragEvent` whose `dataTransfer` is `null`, so the board cannot stash the
  // card id where a real drag puts it, and the drop handler falls back to
  // React state — `moveTo(col, dropped)` reads `dropped || dragId`. That
  // fallback exists for browsers that mangle the payload; it is not the path a
  // real gesture takes, and leaning on it makes the three dispatches straddle a
  // window in which a re-render matters. Handing the same `DataTransfer` to all
  // three makes this the gesture a browser actually performs: `setData` at
  // `dragstart`, `getData` at `drop`, and nothing in between that a re-render
  // can touch.
  const dataTransfer = await page.evaluateHandle(() => new DataTransfer());
  const planning = page.locator("div.w-72").nth(EXPECTED_COLUMNS.indexOf("Planning"));
  await card.dispatchEvent("dragstart", { dataTransfer });
  await planning.dispatchEvent("dragover", { dataTransfer });
  await planning.dispatchEvent("drop", { dataTransfer });

  await expect
    .poll(
      async () => (await (await request.get(`${API}/tasks/${id}`)).json()).task.column,
      { timeout: 15_000 },
    )
    .toBe("planning");

  // Planning is deliberately inert: only `in_progress` spends an agent turn, so
  // the dispatch toast must NOT appear. This is the assertion that lets the
  // column ship ahead of epic #183 §4's auto-advance.
  await expect(page.getByText("Dispatched — the assignee is working on it.")).toHaveCount(0);

  // The toast is a console-side signal and only fires for `in_progress`, so on
  // its own it cannot distinguish "never dispatched" from "dispatched and the
  // run already finished". Assert the host's own record instead: the task's
  // timeline is folded from the company journal, so a dispatch that happened at
  // any point leaves a `dispatched` entry behind that no later event removes.
  const detail = await (await request.get(`${API}/tasks/${id}`)).json();
  expect(
    (detail.timeline ?? []).filter((entry: { kind: string }) => entry.kind === "dispatched"),
  ).toHaveLength(0);
});

/**
 * The other half of issue #501: what Planning means **with** a planner.
 *
 * This is the assertion the live-brain lane was missing. It found the defect
 * that opened #501 and had nothing to replace the inert-column claim with, so
 * the lane reported a failure without ever stating the contract that actually
 * holds there.
 *
 * The contract is settlement, not a destination. `src/harness/planning.rs`
 * edge-fires one pass per card entering Planning and lands the card in
 * `in_progress` (plan written, nothing blocking) or back in `todo` (a missing
 * prerequisite, or the pass itself failed) — always with a `[system]` note
 * saying which. The one outcome the product must never produce is a card left
 * parked in `planning` with nothing having happened, which is exactly what a
 * lost drop or a stalled pass would look like.
 *
 * So this asserts the negative that matters — the card does not stay put — and
 * then that wherever it landed is one of the two documented landings and says
 * why. It is deliberately agnostic about WHICH: the harness lane's brain echoes
 * rather than plans, so today it always takes the failure row, and pinning that
 * would make this test a description of the mock rather than of the product.
 *
 * The window is generous because a pass makes a real model call, bounded by
 * `PLANNING_TIMEOUT` (120s) on the host side.
 */
test("a card dropped into Planning is planned and settled, never left parked", async ({
  page,
  request,
}) => {
  test.skip(
    !LIVE_BRAIN,
    "needs a --features openhuman,tinycortex host with a planner attached; without " +
      "one Planning is inert and the test above is the applicable contract. Issue #501.",
  );

  const title = `e2e planning settle ${Date.now()}`;
  const seeded = await request.post(`${API}/tasks`, { data: { title } });
  expect(seeded.ok()).toBeTruthy();
  const id = (await seeded.json()).id as string;

  await page.goto("/#/tasks");
  await dismissTour(page);

  const card = page.locator("div[draggable=true]").filter({ hasText: title }).first();
  await expect(card).toBeVisible({ timeout: 15_000 });

  // The same faithful gesture as the test above: one DataTransfer across all
  // three events, so the id travels where a real drag puts it.
  const dataTransfer = await page.evaluateHandle(() => new DataTransfer());
  const planning = page.locator("div.w-72").nth(EXPECTED_COLUMNS.indexOf("Planning"));
  await card.dispatchEvent("dragstart", { dataTransfer });
  await planning.dispatchEvent("dragover", { dataTransfer });
  await planning.dispatchEvent("drop", { dataTransfer });

  const read = async () => (await (await request.get(`${API}/tasks/${id}`)).json()).task;

  // The drop lands through the host, which is an async round-trip. Without this
  // first wait the "leaves planning" poll below can short-circuit on the card's
  // pre-drop `todo` state (that is what it was created in), pass instantly, and
  // then observe it still parked in `planning` before the pass settles it. So
  // prove the drop landed first — the card must be seen IN `planning` — before
  // waiting for it to leave.
  await expect
    .poll(async () => (await read()).column, { timeout: 15_000 })
    .toBe("planning");

  // It reached the host and the pass settled it. A card still in `planning`
  // when this expires is the real failure this test exists to catch: either the
  // drop never landed, or a pass started and never finished.
  await expect
    .poll(async () => (await read()).column, { timeout: 150_000 })
    .not.toBe("planning");

  const settled = await read();
  expect(["todo", "in_progress"]).toContain(settled.column);
  // And it says why it moved, rather than moving silently. `[system]` is the
  // attribution every planning outcome writes onto the note.
  expect(settled.note ?? "").toContain("[system]");
});
