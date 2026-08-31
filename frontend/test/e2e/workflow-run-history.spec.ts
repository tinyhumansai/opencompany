import { expect, test } from "@playwright/test";

import { LIVE_BRAIN, LIVE_BRAIN_REASON } from "./capabilities";
import { openFirstWorkflow } from "./workflows";

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
 * The History toggle, once a workflow is open.
 *
 * Issue #1110: the caller opens a workflow first. The toggle is a per-workflow
 * control and the tab no longer opens inside one, so waiting for it on the
 * index would time out and — through the `test.skip` below — report a host that
 * does not serve `…/workflows/runs`, which would be a lie about the host to
 * cover a spec that never navigated. Opening the workflow explicitly keeps the
 * skip meaning only what it says.
 *
 * Still waits rather than probing `count()` immediately: the button renders
 * only after the graph resolves, so an instant check races the first paint and
 * would silently skip on a host that DOES serve the route.
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
  await dismissTour(page);

  // Issue #1110: the tab opens on the INDEX, and the index reads the run
  // journal UNSCOPED on purpose — one request has to feed every card's health
  // line, and `?workflow=` covers exactly one graph. Stating that here is what
  // makes the rule below a narrowing rather than a contradiction.
  await expect
    .poll(
      () => requested.some((url) => !new URL(url).searchParams.get("workflow")),
      { timeout: 30_000 },
    )
    .toBe(true);

  // Open one, and the read that backs ITS history panel must name it. A console
  // that fetched the company-wide page and filtered client-side would never
  // issue this request — and would tell a rarely-run workflow it "hasn't
  // finished a run yet" the moment busier workflows filled the page.
  const id = await openFirstWorkflow(page);
  await expect
    .poll(
      () => requested.some((url) => new URL(url).searchParams.get("workflow") === id),
      { timeout: 30_000 },
    )
    .toBe(true);
});

test("the run-history panel opens and shows only the selected workflow's runs", async ({
  page,
}) => {
  await page.goto("/#/workflows");
  await dismissTour(page);
  // Issue #1110: History belongs to one workflow, so one has to be open.
  await openFirstWorkflow(page);

  // Issue #1683: index select already opens the History rail, so the toggle
  // needs waiting on (host-support check) but not clicking.
  await historyToggle(page);

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
  // Per-test: the two above read history the host already holds and pass on a
  // default build. This one has to actually RUN the workflow, and the runner
  // lives behind `openhuman` — with the feature off the Run button journals
  // nothing and the poll below can only time out.
  test.skip(!LIVE_BRAIN, LIVE_BRAIN_REASON);

  await page.goto("/#/workflows");
  await dismissTour(page);
  // Issue #1110: open the workflow this test runs. The reload below keeps the
  // `#/workflows/<id>` this pushes, so the console comes back on the same
  // workflow's detail view — which is what makes the second read a re-read of
  // the same journal rather than of whichever workflow sorted first.
  await openFirstWorkflow(page);

  // Issue #1683: index select already opens the History rail, so the toggle
  // needs waiting on (host-support check) but not clicking.
  await historyToggle(page);
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
