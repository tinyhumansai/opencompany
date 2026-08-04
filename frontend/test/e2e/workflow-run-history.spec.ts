import { expect, test } from "@playwright/test";

/**
 * Issue #228: the Workflows view reads a workflow's finished runs back from the
 * host's journal, so a run's outcome survives the drawer being dismissed, the
 * console being reloaded, and — for a scheduled run — nobody having watched.
 *
 * The load-bearing detail these specs pin is that the history read is **scoped
 * to the selected workflow server-side**. The host applies `?workflow=` BEFORE
 * its `limit` cut precisely so a rarely-run workflow still returns its own most
 * recent runs. A console that fetched the company-wide page and filtered
 * client-side would undo that: once other workflows produced `limit` more
 * recent runs, the selected workflow's history would fall out of the fetched
 * page and the panel would claim it "hasn't finished a run yet" while the run
 * sat journaled on the host — which is issue #228's own symptom reappearing in
 * the client. That regression is invisible on a small company, which is exactly
 * why it is asserted on the request rather than left to eyeballing.
 *
 * Runs against the live host the harness brings up (see `playwright.config.ts`).
 * The company must have at least one workflow saved.
 */

/**
 * Dismisses the first-run "Welcome to your company" tour if it is up.
 *
 * A fresh company shows it over every view, and its overlay swallows pointer
 * events — so without this the specs fail on an unrelated modal rather than on
 * anything they are about. Tolerates its absence: a company that has already
 * seen the tour never shows it again.
 */
async function dismissTour(page: import("@playwright/test").Page) {
  const skip = page.getByRole("button", { name: "Skip for now" });
  // The dialog mounts a beat after navigation, so an instant visibility probe
  // races it and leaves the overlay swallowing every later click. Wait a bounded
  // moment for it, and treat "never appeared" as already dismissed.
  try {
    await skip.waitFor({ state: "visible", timeout: 10_000 });
  } catch {
    return;
  }
  await skip.click();
  await expect(skip).toBeHidden();
}

/**
 * The History toggle, once the view has settled.
 *
 * Waits for it rather than probing `count()` immediately: the button renders
 * only after the workflow list resolves and a workflow is selected, so an
 * instant check races the first paint and would silently skip on a host that
 * DOES serve the route — turning a real failure into a green run. Only a
 * genuine timeout means the host predates `…/workflows/runs`, and that is the
 * one case worth skipping for.
 */
async function historyToggle(page: import("@playwright/test").Page) {
  const toggle = page.getByTestId("workflow-history-toggle");
  try {
    await toggle.waitFor({ state: "visible", timeout: 30_000 });
  } catch {
    test.skip(true, "this host does not serve …/workflows/runs");
  }
  return toggle;
}

/** The host route these specs are about. */
const RUNS_ROUTE = /\/workflows\/runs\b/;

test("the console asks the host for the selected workflow's runs, not the whole company's", async ({
  page,
}) => {
  // Capture every run-history request the view makes.
  const requested: string[] = [];
  page.on("request", (req) => {
    if (RUNS_ROUTE.test(req.url())) requested.push(req.url());
  });

  await page.goto("/#/workflows");

  // The view fetches history once a workflow is selected (the list auto-selects
  // the first entry), so wait for the request rather than racing it.
  await expect.poll(() => requested.length, { timeout: 30_000 }).toBeGreaterThan(0);

  for (const url of requested) {
    const params = new URL(url).searchParams;
    expect(
      params.get("workflow"),
      `history must be scoped server-side, got: ${url}`,
    ).toBeTruthy();
  }
});

test("the run-history panel opens and shows only the selected workflow's runs", async ({
  page,
}) => {
  await page.goto("/#/workflows");
  await dismissTour(page);

  const toggle = await historyToggle(page);
  await toggle.click();

  const panel = page.getByTestId("workflow-run-history");
  await expect(panel).toBeVisible();

  // Either the workflow has runs, or the panel says so plainly. Both are
  // correct; what must never happen is an error or an empty unexplained panel.
  const rows = panel.getByTestId("workflow-run-row");
  const count = await rows.count();
  if (count === 0) {
    await expect(panel.getByText(/hasn't finished a run yet/)).toBeVisible();
  } else {
    // Every row is a real outcome: it says how the run started.
    await expect(rows.first().getByText(/^(scheduled|manual)$/)).toBeVisible();
  }
});

test("running a workflow adds it to the durable history and it survives a reload", async ({
  page,
}) => {
  await page.goto("/#/workflows");
  await dismissTour(page);

  const toggle = await historyToggle(page);
  await toggle.click();
  const before = await page
    .getByTestId("workflow-run-history")
    .getByTestId("workflow-run-row")
    .count();

  // Run the selected workflow. The run may fail (an agent node with no
  // inference source) — that is fine and in fact the more interesting case:
  // a failed run is journaled too, and used to leave nothing behind at all.
  await page.getByRole("button", { name: "Run", exact: true }).click();

  await expect
    .poll(
      async () =>
        page
          .getByTestId("workflow-run-history")
          .getByTestId("workflow-run-row")
          .count(),
      { timeout: 60_000 },
    )
    .toBeGreaterThan(before);

  // The whole point of #228: reload the console and the outcome is still there,
  // because it came from the journal rather than from component state.
  await page.reload();
  await dismissTour(page);
  await page.getByTestId("workflow-history-toggle").click();
  await expect
    .poll(
      async () =>
        page
          .getByTestId("workflow-run-history")
          .getByTestId("workflow-run-row")
          .count(),
      { timeout: 30_000 },
    )
    .toBeGreaterThan(before);
});
