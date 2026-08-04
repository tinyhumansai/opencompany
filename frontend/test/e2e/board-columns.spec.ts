import { expect, test, type Page } from "@playwright/test";

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

test("dragging into Planning moves the card without dispatching it", async ({ page, request }) => {
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
  const planning = page.locator("div.w-72").nth(EXPECTED_COLUMNS.indexOf("Planning"));
  await card.dispatchEvent("dragstart");
  await planning.dispatchEvent("dragover");
  await planning.dispatchEvent("drop");

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
