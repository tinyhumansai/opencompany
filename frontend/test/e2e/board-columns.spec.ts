import { expect, test, type Page } from "@playwright/test";

import { LIVE_BRAIN } from "./capabilities";

/**
 * Issue #301 — the board's shape, asserted against a live host.
 *
 * **The two-language mirror this used to guard is gone.** `TASK_COLUMNS` was a
 * hand-maintained copy of the host's list, and this spec existed because *"a
 * Rust test cannot see the TS list"* — so only a rendered board driven against
 * a real host could join them. The board now reads its columns and their labels
 * off the `tasks` ledger, built from one host table (`src/ledger/board.rs`),
 * and the labels below are pinned there by
 * `the_labels_are_the_ones_every_surface_renders`.
 *
 * What this still guards is the end-to-end path, which no unit test on either
 * side reaches: that the declared columns actually arrive over the wire, in
 * order, and render. A column the host declares but the console never shows —
 * a broken ledger read, a dropped label — fails here and nowhere else. It is
 * also the guard on intake: one prompt box, landing in To-do.
 *
 * **It drives `#/ledgers/tasks`, not `#/tasks`.** The standalone Tasks page was
 * retired in issue #1140 and the board it showed is the `tasks` ledger's
 * columns, rendered by the same component it always was. The two claims that
 * deletion could have taken with it — that work can still be *created*, and
 * that a card can still be *opened* — are asserted below rather than left to
 * the reader, because both fail silently: a console with no intake looks like a
 * company with nothing to do, and a dead card link looks like a link that
 * worked.
 */

const API = "/api/v1/company";

/** The board's vocabulary, in board order — three phases since issue #1512. */
const EXPECTED_COLUMNS = ["Pending", "Working", "Done"];

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
  // By testid, not by shape. An empty column collapses to a rail (issue #1101)
  // and renders its label inside a button rather than the open column's header
  // row, so a structural selector would silently stop counting the very columns
  // this asserts the order of.
  return page.getByTestId("ledger-board").getByTestId("column-label");
}

/**
 * Issue #1140 — the two things retiring the Tasks page could have taken.
 *
 * `#/tasks` is in every operator's history and fingers, and `#/tasks/<id>` is
 * linked from chat, from an approval card and from a workflow run's rows. The
 * first has to land on the board and the second has to keep opening the card,
 * and both failures are quiet: the router drops an address it does not know and
 * renders Overview, which looks like a link that worked.
 */
test("the retired #/tasks lands on the board, and #/tasks/<id> still opens the card", async ({
  page,
  request,
}) => {
  const title = `e2e retired route ${Date.now()}`;
  const seeded = await request.post(`${API}/tasks`, { data: { title } });
  expect(seeded.ok()).toBeTruthy();
  const id = (await seeded.json()).id as string;

  await page.goto("/#/tasks");
  await dismissTour(page);

  // The board, and the address rewritten to name where it actually is. A push
  // rather than a replace would leave `#/tasks` one Back away, bouncing the
  // operator forward again on arrival.
  await expect(columnLabels(page)).toHaveText(EXPECTED_COLUMNS, { timeout: 15_000 });
  await expect.poll(() => new URL(page.url()).hash).toBe("#/ledgers/tasks");

  // And the card detail, which Ledgers deliberately does not reproduce.
  await page.goto(`/#/tasks/${id}`);
  await expect(page.getByRole("heading", { name: title })).toBeVisible({ timeout: 15_000 });
  expect(new URL(page.url()).hash).toBe(`#/tasks/${id}`);
});

test("the board renders the three phases in order, and none of the retired columns", async ({
  page,
}) => {
  await page.goto("/#/ledgers/tasks");
  await dismissTour(page);

  // The columns are a read now, not a literal, so the board is not itself
  // until they land.
  await expect(columnLabels(page)).toHaveText(EXPECTED_COLUMNS, { timeout: 15_000 });
  // The collapse, stated as its own assertion. Backlog went in #301; the four
  // stages between To-do and Done went in #1512, and they are the ones an
  // operator would notice missing — so each is named rather than counted.
  for (const gone of ["Backlog", "To-do", "Planning", "In progress", "Paused", "In review"]) {
    await expect(columnLabels(page).filter({ hasText: gone })).toHaveCount(0);
  }
});

test("an empty board leaves its column affordances to explain the empty state", async ({
  page,
  request,
}) => {
  // A ledger declared just for this assertion makes the empty condition
  // independent of cards that earlier specs may have added to the shared host.
  const marker = Date.now();
  const slug = `e2e-empty-board-${marker}`;
  const declared = await request.post(`${API}/ledgers`, {
    data: {
      slug,
      title: `E2E empty board ${marker}`,
      purpose: "A list used to verify empty board copy.",
      fields: [
        { name: "id", role: "id" },
        { name: "title", role: "title", required: true },
        { name: "status", role: "status", required: true },
      ],
      statuses: [{ name: "open" }, { name: "closed", closed: true }],
      checks: ["required-field", "known-status"],
    },
  });
  expect(declared.ok()).toBeTruthy();

  try {
    await page.goto(`/#/ledgers/${slug}`);
    await dismissTour(page);

    // Switch to the board explicitly. `defaultLedgerMode` gives columns to the
    // native `tasks` ledger and rows to every other one — "columns for
    // dispatched tasks; rows for every agent-written ledger" — so a ledger
    // declared here opens as a list, and waiting for `ledger-board` without
    // asking for it waits forever.
    //
    // By testid: the toggle's label and title both flip with the mode, and
    // "Board" also matches the list switcher's trigger.
    await page.getByTestId("ledger-mode-toggle").click();
    await expect(page.getByTestId("ledger-board")).toBeVisible({ timeout: 15_000 });

    // Board columns already say what an empty board is for. A second status
    // line above them repeats the fact instead of helping the operator act.
    await expect(page.getByTestId("ledger-empty")).toHaveCount(0);
    await expect(page.getByTestId("ledger-filtered-empty")).toHaveCount(0);

    const search = page.getByPlaceholder("Search every field");
    await search.fill("no matching row");
    await expect(page.getByTestId("ledger-filtered-empty")).toHaveCount(0);

    // The list has no per-status-column affordance, so it retains both forms
    // of the above-list notice.
    await page.getByTestId("ledger-mode-toggle").click();
    await expect(page.getByTestId("ledger-filtered-empty")).toBeVisible({ timeout: 15_000 });
    await search.fill("");
    await expect(page.getByTestId("ledger-empty")).toBeVisible({ timeout: 15_000 });
  } finally {
    await request.delete(`${API}/ledgers/${slug}`);
  }
});

test("new work enters through one prompt box and lands in Pending", async ({ page, request }) => {
  await page.goto("/#/ledgers/tasks");
  await dismissTour(page);

  // Exactly one entry point on the whole board (issue #206's rule, kept).
  const addTask = page.getByRole("button", { name: "Add task" });
  await expect(addTask).toHaveCount(1);
  await addTask.click();
  await expect(page.getByRole("heading", { name: "New task" })).toBeVisible();

  // Title / Note / Priority stay gone from create — the host defaults priority
  // and the card's edit surface owns them (#278).
  await expect(page.locator("#new-prompt")).toBeVisible();
  for (const gone of ["#new-title", "#new-note", "#new-priority"]) {
    await expect(page.locator(gone)).toHaveCount(0);
  }

  // Assignee came *back* in #1106, and is the one exception to "one field".
  // #301 removed it on the reasoning that the host defaults it; what that missed
  // is that the host's default is a planning pass which picks an owner, and picks
  // one silently when two teammates fit. Offering it here is the pre-empt.
  //
  // The rule that keeps this from re-breaking what #301 fixed is the *default*,
  // asserted below rather than the control's absence: an operator who ignores it
  // types a prompt, hits Create, and gets exactly the unassigned card they got
  // before — the field is omitted from the body entirely when untouched.
  await expect(page.locator("#new-assignee")).toHaveCount(1);

  // A prompt longer than the title cap: the title is shortened and the full
  // text survives in the note, so nothing the operator typed is lost.
  const marker = `e2e board shape ${Date.now()}`;
  const long = `${marker} — and then a great deal more detail that runs well past the eighty character title cap so the note has to carry it`;
  await page.locator("#new-prompt").fill(long);
  await page.getByRole("button", { name: "Create", exact: true }).click();

  type Row = { title: string; note?: string; column: string; assignee: string };
  const find = async (): Promise<Row | undefined> => {
    const rows = (await (await request.get(`${API}/tasks`)).json()) as Row[];
    return rows.find((r) => r.title.startsWith(marker));
  };

  await expect.poll(async () => (await find()) !== undefined, { timeout: 15_000 }).toBe(true);
  const created = (await find())!;

  expect(created.column).toBe("pending");
  expect(created.title.length).toBeLessThanOrEqual(81); // 80 + the ellipsis
  expect(created.note).toBe(long);
  // The #1106 default, and the reason adding the control is a no-op for anyone
  // who does not use it: the prompt was the only thing filled in, so the card is
  // unassigned exactly as it was before the picker existed.
  expect(created.assignee).toBe("");
});

/**
 * Issue #501. This test states a **no-planner** contract, and only a host
 * without one keeps it.
 *
 * **The gesture moved in issue #1512.** Planning used to be a board column, and
 * this test used to drag a card into it. Collapsing the board to three phases
 * took the drop target away — `planning` is a stage now, one of the four that
 * read as Working — so the deliberate "plan this before anything runs" act is a
 * control on the card instead, which is where an *act* belonged rather than a
 * *state*. What it writes is unchanged: the `planning` stage, which edge-fires
 * exactly one pass.
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
test("Plan first moves the card into planning without dispatching it", async ({
  page,
  request,
}) => {
  test.skip(
    LIVE_BRAIN,
    "asserts Planning is inert, which is only true without a planner; the harness " +
      "contract is covered by the live-brain test below. Issue #501.",
  );
  const title = `e2e planning control ${Date.now()}`;
  const seeded = await request.post(`${API}/tasks`, { data: { title } });
  expect(seeded.ok()).toBeTruthy();
  const id = (await seeded.json()).id as string;

  await page.goto(`/#/tasks/${id}`);
  await dismissTour(page);
  await expect(page.getByRole("heading", { name: title })).toBeVisible({ timeout: 15_000 });

  // Offered on a pending card, and it is the only route to a planning pass now
  // that no column takes a drop into one.
  const planFirst = page.getByRole("button", { name: "Plan first" });
  await expect(planFirst).toBeVisible();
  await planFirst.click();

  await expect
    .poll(
      async () => (await (await request.get(`${API}/tasks/${id}`)).json()).task.stage,
      { timeout: 15_000 },
    )
    .toBe("planning");
  // The phase a reader of the board sees, beside it: still one column, still
  // "started and not finished" (issue #1512).
  const parked = await (await request.get(`${API}/tasks/${id}`)).json();
  expect(parked.task.column).toBe("working");

  // Planning is deliberately inert: only `in_progress` spends an agent turn, so
  // the dispatch toast must NOT appear.
  await expect(page.getByText("Dispatched — the assignee is working on it.")).toHaveCount(0);

  // The toast is a console-side signal and only fires for `in_progress`, so on
  // its own it cannot distinguish "never dispatched" from "dispatched and the
  // run already finished". Assert the host's own record instead: the task's
  // timeline is folded from the company journal, so a dispatch that happened at
  // any point leaves a `dispatched` entry behind that no later event removes.
  expect(
    (parked.timeline ?? []).filter((entry: { kind: string }) => entry.kind === "dispatched"),
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
 * edge-fires one pass per card entering the `planning` stage and lands the card
 * in `in_progress` (plan written, nothing blocking) or back in `todo` (a
 * missing prerequisite, or the pass itself failed) — always with a `[system]`
 * note saying which. The one outcome the product must never produce is a card
 * left parked in `planning` with nothing having happened, which is exactly what
 * a lost click or a stalled pass would look like.
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
test("a card sent to Plan first is planned and settled, never left parked", async ({
  page,
  request,
}) => {
  test.skip(
    !LIVE_BRAIN,
    "needs a --features openhuman host with a planner attached; without " +
      "one Planning is inert and the test above is the applicable contract. Issue #501.",
  );

  const title = `e2e planning settle ${Date.now()}`;
  const seeded = await request.post(`${API}/tasks`, { data: { title } });
  expect(seeded.ok()).toBeTruthy();
  const id = (await seeded.json()).id as string;

  await page.goto(`/#/tasks/${id}`);
  await dismissTour(page);
  await expect(page.getByRole("heading", { name: title })).toBeVisible({ timeout: 15_000 });
  await page.getByRole("button", { name: "Plan first" }).click();

  const read = async () => (await (await request.get(`${API}/tasks/${id}`)).json()).task;

  // The settle is the signal that matters, and the note is its durable
  // record: every planning outcome writes a `[system]` note and lands the
  // card in the same atomic `upsert`. Poll for the note rather than the
  // intermediate `planning` stage — a fast brain settles the card before a
  // poll interval elapses, so `planning` is a transient a pass can skip
  // entirely, and requiring it to be observed is a race masquerading as an
  // assertion. The note, by contrast, only exists once the pass has finished.
  await expect
    .poll(async () => (await read()).note ?? "", { timeout: 150_000 })
    .toContain("[system]");

  // Wherever it landed, it is a documented landing, not the parking lot the
  // contract forbids: `in_progress` (a plan, nothing blocking) or `todo` (a
  // missing prerequisite, or the pass itself failed) — read off the stage,
  // since both are the one `working` phase and `todo` is `pending`.
  const settled = await read();
  expect(["pending", "in_progress"]).toContain(settled.stage ?? settled.column);
  // And it says why it moved, rather than moving silently. `[system]` is the
  // attribution every planning outcome writes onto the note.
  expect(settled.note ?? "").toContain("[system]");
});
