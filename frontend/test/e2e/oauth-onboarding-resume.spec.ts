import { expect, test } from "@playwright/test";

/**
 * Issue #300 — an OAuth connection started from inside the onboarding tour.
 *
 * The redirect back from the provider is a **full-page navigation**, so neither
 * half of this can be proven by a unit test: the tour's step state lives in
 * react-joyride's memory and dies with the document, and the failure arms of the
 * host callback used to render a JSON body *as the page*.
 *
 * These specs drive a running host (see `playwright.config.ts` — the harness
 * brings it up, there is no `webServer`). CI does not run Playwright.
 */

type Page = import("@playwright/test").Page;

/**
 * Dismiss the first-run welcome dialog if it is up.
 *
 * Not cosmetic: it is a Radix dialog, so while it is open every other element
 * is `aria-hidden` and therefore invisible to `getByRole`. Any role-based
 * assertion about the console underneath has to come after this.
 */
async function dismissWelcome(page: Page): Promise<void> {
  const skip = page.getByRole("button", { name: "Skip for now" });
  if (await skip.isVisible().catch(() => false)) await skip.click();
}

/** The tour's per-company localStorage key, discovered from the running app. */
async function tourKey(page: Page): Promise<string> {
  const key = await page.evaluate(() =>
    Object.keys(window.localStorage).find((k) => k.startsWith("oc-tour:")),
  );
  expect(key, "the console should have written a per-company tour key").toBeTruthy();
  return key!;
}

test("a cancelled handshake lands back in the console, not on a dead page", async ({ page }) => {
  // Exactly what the host now redirects to when the operator cancels at the
  // provider's consent screen. Before the fix this route answered with
  // `{"error":"provider returned: access_denied"}` as the document body.
  await page.goto("/connections?connect_error=denied&provider=slack");

  // Assert the message first — the toast auto-dismisses, so anything that
  // blocks for a timeout before this would race it away.
  await expect(page.getByText(/cancelled/i)).toBeVisible();

  // The console renders — the operator is not stranded on raw JSON.
  await dismissWelcome(page);
  await expect(page.getByRole("heading", { name: "Connections", level: 2 })).toBeVisible();

  // The param is stripped, so a refresh doesn't re-fire the toast.
  await expect
    .poll(() => new URL(page.url()).searchParams.get("connect_error"))
    .toBeNull();

  // Connecting is still offered: the failure is a retry, not a terminal state.
  await expect(page.getByRole("button", { name: "Connect" }).first()).toBeEnabled();
});

test("an unknown failure code still produces a usable message", async ({ page }) => {
  // An older console against a newer host must not fall silent.
  await page.goto("/connections?connect_error=something_new_2099");
  await expect(page.getByText(/couldn't connect/i)).toBeVisible();
  await dismissWelcome(page);
  await expect(page.getByRole("heading", { name: "Connections", level: 2 })).toBeVisible();
});

test("the tour resumes on the Connections stop after a redirect", async ({ page }) => {
  await page.goto("/#/overview");

  // First run offers the tour. Skipping writes the per-company key, which is
  // how we learn the key name without hard-coding the company id.
  const skip = page.getByRole("button", { name: "Skip for now" });
  await expect(skip).toBeVisible();
  await skip.click();
  const key = await tourKey(page);

  // Seed exactly what `armTourResume` writes just before ConnectionsView hands
  // the browser to the provider: mid-tour, on the Connections stop, no
  // completed/skipped flag (the tour never finished).
  await page.evaluate(
    ([k]) =>
      window.localStorage.setItem(
        k,
        JSON.stringify({ pendingResume: { view: "connections", at: Date.now() } }),
      ),
    [key],
  );

  // The return trip from the provider.
  await page.goto("/connections?connected=slack");

  // Resumed on the stop the operator left...
  await expect(page.getByText("Connect your tools")).toBeVisible();
  // ...and NOT restarted from step 1, which is the bug.
  await expect(page.getByText("Welcome to your company")).toHaveCount(0);

  // The marker is consumed, so it can't fire again on a later visit.
  const after = await page.evaluate(([k]) => window.localStorage.getItem(k), [key]);
  expect(JSON.parse(after ?? "{}").pendingResume).toBeUndefined();
});

test("a stale resume marker does not hijack a later visit", async ({ page }) => {
  await page.goto("/#/overview");
  const skip = page.getByRole("button", { name: "Skip for now" });
  await expect(skip).toBeVisible();
  await skip.click();
  const key = await tourKey(page);

  // Older than the 15-minute TTL.
  await page.evaluate(
    ([k]) =>
      window.localStorage.setItem(
        k,
        JSON.stringify({
          skipped: true,
          pendingResume: { view: "connections", at: Date.now() - 60 * 60 * 1000 },
        }),
      ),
    [key],
  );

  await page.goto("/#/overview");
  // No tour: the marker aged out and the tour is already marked skipped.
  await expect(page.getByText("Connect your tools")).toHaveCount(0);
  await expect(page.getByText("Welcome to your company")).toHaveCount(0);
});
