import { expect, test, type Page, type Route } from "@playwright/test";

/**
 * Issue #341: the workflows view had **two** controls both accessible as
 * "New workflow" — the toolbar action and the empty state's call to action.
 *
 * This is pinned in a browser because it is a fact about rendered output and
 * nothing else. Both buttons call the same `setCreateOpen(true)`, so no unit
 * test of behaviour can tell the fixed view from the broken one; the only
 * difference is what the accessibility tree holds, which exists only once the
 * component has rendered into a document.
 *
 * The assertions are deliberately written the way the failure arrived —
 * `getByRole("button", { name: … })` with no `.first()` and no test id — so a
 * regression fails here exactly as `strict mode violation: … resolved to 2
 * elements` rather than silently picking one. A test id would pass over the
 * defect, which is the whole reason #341 says the fix is in the component and
 * not in the locator.
 *
 * Runs against the live host the harness brings up (see `playwright.config.ts`).
 */

/** The workflows list route, and only it — not `…/workflows/{id}` or `/run`. */
function isWorkflowList(url: URL): boolean {
  return /\/api\/v1\/(company|companies\/[^/]+)\/workflows$/.test(url.pathname);
}

/**
 * Dismisses the first-run tour if it is up — its overlay swallows pointer
 * events, so without this every spec fails on an unrelated modal. Tolerates its
 * absence; a company that has seen it never shows it again.
 */
async function dismissTour(page: Page) {
  const skip = page.getByRole("button", { name: "Skip for now" });
  try {
    await skip.waitFor({ state: "visible", timeout: 10_000 });
  } catch {
    return;
  }
  await skip.click();
  await expect(skip).toBeHidden();
}

test("one control answers to 'New workflow', and clicking it opens the dialog", async ({
  page,
}) => {
  await page.goto("/#/workflows");
  await dismissTour(page);

  const create = page.getByRole("button", { name: "New workflow" });
  await expect(create).toHaveCount(1);

  // The click a strict-mode violation used to abort. Unqualified on purpose.
  await create.click();
  const dialog = page.getByRole("dialog");
  await expect(dialog.getByText("New workflow", { exact: true })).toBeVisible();
});

test("the empty state's call to action is reachable and distinctly named", async ({
  page,
}) => {
  // The harness company ships `workflows/committed.toml`, so the empty state
  // is unreachable by driving the console — and it is the state that carried
  // the second button. An empty list is stubbed rather than left untested;
  // the alternative is a spec that never renders the offending branch.
  await page.route(
    (url) => isWorkflowList(url),
    async (route: Route) => {
      if (route.request().method() !== "GET") return route.fallback();
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: "[]",
      });
    },
  );

  await page.goto("/#/workflows");
  await dismissTour(page);

  await expect(page.getByText("This company has no saved workflows yet.")).toBeVisible();

  // Both controls are on screen together here. That is fine — what is not fine
  // is them sharing a name.
  const create = page.getByRole("button", { name: "New workflow" });
  await expect(create).toHaveCount(1);

  const emptyCta = page.getByRole("button", { name: "Create a workflow" });
  await expect(emptyCta).toHaveCount(1);

  // The call to action still does its job: same dialog, different name.
  await emptyCta.click();
  const dialog = page.getByRole("dialog");
  await expect(dialog.getByText("New workflow", { exact: true })).toBeVisible();
});
