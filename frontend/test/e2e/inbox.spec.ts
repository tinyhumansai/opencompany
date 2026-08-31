import { expect, test, type Page } from "@playwright/test";

/**
 * Regression proof for issue #173: the Inbox surface must read the host's real
 * per-agent `InboxStore`, never a client-side fixture.
 *
 * The retired fixture (`src/lib/inbox.ts`) fabricated the same four emails —
 * "Priya Sharma", "Stripe", "Weekly Digest", "Figma" — in localStorage for every
 * teammate, so every inbox looked identical and genuinely ingested mail never
 * appeared. This spec asserts the two observable consequences of the fix:
 *
 *   1. The Team inbox toggle round-trips through `PUT …/team/{id}/inbox` — it
 *      survives a reload, which a localStorage-only toggle would too, so we also
 *      assert it survives a *storage-cleared* reload.
 *   2. No fixture sender ever renders. A freshly enabled inbox with no mail shows
 *      the real empty state instead of invented correspondence.
 *
 * Runs against the same live host as `wiring.spec.ts` (see that file's header).
 *
 * Inbox is parked rather than retired: it has no navigation row, but its direct
 * URL remains an operator-facing route. The notice below keeps that distinction
 * honest instead of presenting a complete mail client as a live console section
 * (issue #1337).
 */

/** All four senders the deleted fixture invented for every teammate. */
const FIXTURE_SENDERS = ["Priya Sharma", "Stripe", "Weekly Digest", "Figma"];

/**
 * A subject line only the fixture ever produced. Asserted alongside the senders
 * so a partial reintroduction — fixture bodies restored under other names —
 * cannot slip through either.
 */
const FIXTURE_SUBJECTS = ["Re: Spring campaign timeline"];

/** The first-run tour's overlay intercepts clicks on the roster beneath it. */
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

test("Inbox is reachable, explains that it is parked, and reads the host's per-agent store", async ({ page }) => {
  // Switch on the first teammate's inbox from that teammate's own page — the
  // control moved off the roster card in issue #1190. It writes to the host
  // keyed by agent id, the same key the ingest webhook files mail under.
  await page.goto("/#/company");
  await dismissTour(page);
  await page.getByTestId("team-card-open").first().click();
  const toggle = page.getByTestId("agent-inbox-toggle");
  await expect(toggle).toBeVisible({ timeout: 30_000 });
  if (!(await toggle.isChecked())) {
    await toggle.click();
  }
  await expect(toggle).toBeChecked({ timeout: 30_000 });
  // The switch's checked state is optimistic — `toggleInbox` flips it before
  // the PUT lands and holds the switch disabled (`busy`) until the host
  // acknowledges. Waiting for it to be enabled again is waiting for the write
  // to actually reach the `InboxStore`; only then is a reload a meaningful
  // persistence check.
  await expect(toggle).toBeEnabled({ timeout: 30_000 });

  // Drop every client-side store, then reload: only host state can survive this.
  await page.evaluate(() => {
    localStorage.clear();
    sessionStorage.clear();
  });
  // A real reload, not `goto(agentUrl)`: navigating to the URL the page is
  // already on is a same-document no-op in Chromium, so no reload fires, the
  // optimistic toggle state survives, and the assertion below would prove
  // nothing about the host.
  await page.reload();
  // Clearing storage also clears the tour's "seen" marker, so the first-run
  // overlay is back. Dismiss it again rather than let it sit over the page.
  await dismissTour(page);
  await expect(page.getByTestId("agent-inbox-toggle")).toBeChecked({ timeout: 30_000 });

  // The Inbox page lists that inbox and shows only real mail. With no ingested
  // mail this is the empty state — what matters is that it is never the fixture.
  await page.goto("/#/inbox");
  await expect(page.getByTestId("inbox-parked-notice")).toContainText(
    "Inbox is not in the console navigation right now",
  );
  await expect(page.getByTestId("inbox-select")).toBeVisible({ timeout: 30_000 });
  // The message pane is terminal only once the host's messages have rendered —
  // either the real empty state or an actual row. Synchronize on that before
  // asserting fixture absence, or the assertions below would pass against the
  // loading skeletons and finish before a reintroduced fixture got a chance to
  // appear.
  await expect(
    page.getByTestId("inbox-empty").or(page.getByTestId("inbox-message")),
  ).toBeVisible({ timeout: 30_000 });
  for (const invented of [...FIXTURE_SENDERS, ...FIXTURE_SUBJECTS]) {
    await expect(page.getByText(invented, { exact: false })).toHaveCount(0);
  }
});
