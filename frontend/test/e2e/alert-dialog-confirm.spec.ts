import { expect, test, type Page, type Route } from "@playwright/test";

/**
 * Regression proof for #268 — confirming an `AlertDialog` must dismiss it.
 *
 * `AlertDialogAction` used to render a plain `Button` while `AlertDialogCancel`
 * rendered the primitive's `Close`, so only *cancelling* closed the dialog.
 * Confirming ran the handler, the underlying action succeeded, and the popup
 * stayed mounted — its backdrop marking the rest of the console inert and
 * swallowing every subsequent click until a reload.
 *
 * Both tests hold the confirm handler's request open, because the interesting
 * moment is the one *between* the click and the response. That window is where
 * the bug lives, and holding it makes the assertion deterministic rather than a
 * race: while the request is in flight the dialog must already be gone. It also
 * removes the thing that used to mask the bug — on the delete path the console
 * navigates away once the request lands, which unmounts the stuck dialog and
 * makes a naive "is it hidden eventually?" assertion pass even on `main`.
 *
 * The dialog being *hidden* is only half of it. A popup that has animated out of
 * sight but left an inert backdrop behind is the same bug, so each test also
 * clicks its way back through the surface underneath with Playwright's
 * actionability checks on: an intercepting backdrop fails that click rather than
 * passing silently.
 *
 * Two of the three call sites on `main` are exercised, because the fix lives in
 * the shared primitive (`components/ui/alert-dialog.tsx`) and not at any one
 * call site: the task delete inside `TaskEditDialog`, and the lifecycle
 * `ConfirmAction` in `SettingsView` (the same shape as `TaskDetailView`'s
 * `ConfirmButton`, whose own confirm needs a live in-flight run to reach).
 *
 * Like the rest of `test/e2e`, this drives a *running* host and is not wired
 * into CI — the Playwright config declares no `webServer`. It is a reproduction
 * and a written-down contract, not a merge gate.
 */

/**
 * The first-run product tour opens its own modal over a fresh console and would
 * intercept every click below. Answer "already skipped" for whatever company id
 * the host resolves to, rather than hard-coding the harness's.
 */
test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    const real = Storage.prototype.getItem;
    Storage.prototype.getItem = function getItem(key: string) {
      return key.startsWith("oc-tour:") ? '{"skipped":true}' : real.call(this, key);
    };
  });
});

const dialog = (page: Page) => page.locator('[data-slot="alert-dialog-content"]');
const backdrop = (page: Page) => page.locator('[data-slot="alert-dialog-overlay"]');

/** Sidebar navigation only succeeds if nothing is intercepting pointer events. */
async function expectConsoleInteractive(page: Page) {
  await page
    .locator('[data-tour="nav-overview"]')
    .getByRole("button")
    .click({ timeout: 10_000 });
  await expect(page).toHaveURL(/#\/overview$/);
}

/** The dialog, and the backdrop it made the console inert with, are both gone. */
async function expectDismissed(page: Page) {
  await expect(dialog(page)).toHaveCount(0, { timeout: 10_000 });
  await expect(backdrop(page)).toHaveCount(0, { timeout: 10_000 });
}

/**
 * Park every `method` request to `url` and hand back a `release` that decides
 * their fate. Until it is called the confirm handler is stuck mid-await, which
 * is exactly the window this spec is about.
 */
async function holdNextRequest(
  page: Page,
  url: RegExp,
  method: string,
): Promise<(outcome: "continue" | "abort") => void> {
  let decide!: (outcome: "continue" | "abort") => void;
  const decided = new Promise<"continue" | "abort">((resolve) => (decide = resolve));
  await page.route(url, async (route: Route) => {
    if (route.request().method() !== method) return route.fallback();
    const outcome = await decided;
    if (outcome === "abort") return route.abort();
    return route.continue();
  });
  return decide;
}

test("confirming a task delete dismisses the dialog before the request lands", async ({
  page,
}) => {
  await page.goto("/#/tasks");

  // Create a card of our own so the test never deletes somebody else's row.
  const title = `e2e confirm-closes ${Date.now()}`;
  await page.getByRole("button", { name: "Add task" }).click();
  await page.locator("#new-prompt").fill(title);
  await page.getByRole("button", { name: "Create", exact: true }).click();

  const card = page.locator('[role="button"]').filter({ hasText: title });
  await expect(card).toBeVisible({ timeout: 30_000 });

  // Open the card's detail screen, then its edit dialog, then ask to delete.
  await card.click();
  await page.getByRole("button", { name: "Edit", exact: true }).click();
  await page.getByRole("button", { name: "Delete", exact: true }).click();
  await expect(dialog(page)).toContainText("Delete");

  const release = await holdNextRequest(page, /\/tasks\/[^/]+$/, "DELETE");

  await dialog(page).getByRole("button", { name: "Delete task" }).click();

  // The bug: the popup stayed mounted here, `data-open` still set, backdrop
  // still covering the console — for as long as the delete took, and on this
  // path for good if it failed.
  await expectDismissed(page);

  // The edit dialog this confirm was raised from is still open, and legitimately
  // so — it closes when the delete lands. What must be true now is that it takes
  // clicks again, which it cannot while the confirm's backdrop is over it. Close
  // it by hand, and the console behind it is reachable too.
  const editDialog = page.locator('[data-slot="dialog-content"]');
  await editDialog.getByRole("button", { name: "Close" }).click();
  await expect(editDialog).toHaveCount(0);
  await expectConsoleInteractive(page);

  // Let the delete through and confirm the handler really did run: closing the
  // dialog must not have cost us the action.
  release("continue");
  await page.goto("/#/tasks");
  await expect(page.locator('[role="button"]').filter({ hasText: title })).toHaveCount(
    0,
    { timeout: 30_000 },
  );
});

test("confirming a lifecycle change dismisses the dialog from a second call site", async ({
  page,
}) => {
  await page.goto("/#/settings");

  const suspend = page.getByRole("button", { name: "Suspend", exact: true });
  await expect(suspend).toBeVisible({ timeout: 30_000 });
  await suspend.click();

  // `SettingsView`'s shared `ConfirmAction` — same primitive, different caller.
  await expect(dialog(page)).toContainText("Suspend this company?");

  // Suspending for real ends the session, so the request is held and then
  // failed: the console reports "couldn't suspend", the harness company stays
  // running for the specs after this one, and the dialog's behaviour on confirm
  // — the only thing under test — is unaffected either way.
  const release = await holdNextRequest(page, /\/companies\/[^/]+\/suspend$/, "POST");

  await dialog(page).getByRole("button", { name: "Suspend", exact: true }).click();

  await expectDismissed(page);
  await expectConsoleInteractive(page);

  // Fail the held request, then confirm the company is still running — the
  // specs after this one drive a live company.
  release("abort");
  await page.goto("/#/settings");
  await expect(page.getByRole("button", { name: "Suspend", exact: true })).toBeVisible({
    timeout: 30_000,
  });
});
