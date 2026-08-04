import { expect, test } from "@playwright/test";

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
 * Parked by issue #302: the console no longer lists Inbox, so `/#/inbox` now
 * canonicalizes to Overview and the assertions below cannot run. The host's
 * inbox routes and per-agent store are unchanged, so the #173 guarantee still
 * holds — this stays here verbatim to be un-skipped the day the surface is
 * relisted, rather than deleted and rewritten from memory.
 */

/** All four senders the deleted fixture invented for every teammate. */
const FIXTURE_SENDERS = ["Priya Sharma", "Stripe", "Weekly Digest", "Figma"];

/**
 * A subject line only the fixture ever produced. Asserted alongside the senders
 * so a partial reintroduction — fixture bodies restored under other names —
 * cannot slip through either.
 */
const FIXTURE_SUBJECTS = ["Re: Spring campaign timeline"];

test.skip("Inbox reads the host's per-agent store, not a seeded fixture", async ({ page }) => {
  // Switch on the first teammate's inbox from the Team page. The toggle writes
  // to the host keyed by agent id — the same key the ingest webhook files under.
  await page.goto("/#/team");
  const toggle = page.getByTestId("team-inbox-toggle").first();
  await expect(toggle).toBeVisible({ timeout: 30_000 });
  if (!(await toggle.isChecked())) {
    await toggle.click();
  }
  await expect(toggle).toBeChecked({ timeout: 30_000 });

  // Drop every client-side store, then reload: only host state can survive this.
  await page.evaluate(() => {
    localStorage.clear();
    sessionStorage.clear();
  });
  await page.goto("/#/team");
  await expect(page.getByTestId("team-inbox-toggle").first()).toBeChecked({ timeout: 30_000 });

  // The Inbox page lists that inbox and shows only real mail. With no ingested
  // mail this is the empty state — what matters is that it is never the fixture.
  await page.goto("/#/inbox");
  await expect(page.getByTestId("inbox-select")).toBeVisible({ timeout: 30_000 });
  for (const invented of [...FIXTURE_SENDERS, ...FIXTURE_SUBJECTS]) {
    await expect(page.getByText(invented, { exact: false })).toHaveCount(0);
  }
});
