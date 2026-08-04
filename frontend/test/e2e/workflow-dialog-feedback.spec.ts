import { expect, test, type Page } from "@playwright/test";

/**
 * Issues #260 / #261 / #262: how the workflow create dialog validates input and
 * gives feedback.
 *
 * These three are one behaviour seen from three sides — a rule stated once
 * (#260), stated *early* (#261), and stated back in plain English (#262) — so
 * they are pinned together, in the browser, against the live host.
 *
 * The browser is the only place two of these are real. Blur validation is a
 * DOM event and a render, not a function call: the unit-testable part
 * (`destinationTargetProblem`) was already correct before #261 and would stay
 * green while the error rendered nowhere, or rendered under a control the
 * author can no longer see. And the cron preview's whole job is to show a
 * *round trip* — debounce, request, response, two clock renderings — which
 * cannot be observed anywhere else.
 *
 * Runs against the live host the harness brings up (see `playwright.config.ts`).
 */

/**
 * Every cron-preview assertion waits on a round trip the other assertions in
 * this file do not: a 350 ms debounce, then a request to the host, then a
 * render. Playwright's default `expect` timeout is 5 s and the config's
 * `timeout` governs the whole test, not each assertion — so on a slow host
 * these are the assertions that flake first. Stated explicitly rather than
 * inherited.
 */
const PREVIEW_TIMEOUT = 15_000;

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

/** Opens Workflows → New workflow and returns the dialog. */
async function openCreateDialog(page: Page) {
  await page.goto("/#/workflows");
  await dismissTour(page);
  await page.getByRole("button", { name: "New workflow" }).click();
  const dialog = page.getByRole("dialog");
  await expect(dialog.getByText("New workflow", { exact: true })).toBeVisible();
  return dialog;
}

/**
 * Turns the dialog's second node row into an `output` node routing to `email`,
 * and returns its target Input.
 *
 * The starter draft holds one trigger row, so index 1 is the row this adds.
 */
async function emailOutputRow(page: Page) {
  const dialog = await openCreateDialog(page);
  await dialog.getByRole("button", { name: "Add node" }).click();

  const kind = dialog.getByLabel("Node kind").nth(1);
  await kind.click();
  await page.getByRole("option", { name: /^Output/ }).click();

  const destination = dialog.getByLabel("Send report to");
  await destination.click();
  await page.getByRole("option", { name: /^Email/ }).click();

  return dialog.getByLabel("Recipient address");
}

test("a bad email target is reported on blur, and typing again clears it", async ({
  page,
}) => {
  const target = await emailOutputRow(page);
  const dialog = page.getByRole("dialog");

  // Typing alone must NOT complain: `n` is not a valid address, and neither is
  // `no`, so an author gets an error on the way to typing a correct one. #261
  // rejects keystroke validation explicitly.
  await target.fill("nope");
  await expect(dialog.getByText(/is not an email address/)).toHaveCount(0);

  // Blur is the moment the author has finished with the field.
  await target.blur();
  const error = dialog.getByText(/is not an email address/);
  await expect(error).toBeVisible();
  // #260: the message ends with the SAME fix instruction the host's 400 ends
  // with, and echoes the offending target.
  await expect(error).toContainText("`nope`");
  await expect(error).toContainText("give the recipient's full address.");
  await expect(target).toHaveAttribute("aria-invalid", "true");

  // Typing again clears it — the author is already fixing it.
  await target.fill("ada@example.com");
  await expect(error).toHaveCount(0);
  await expect(target).not.toHaveAttribute("aria-invalid", "true");

  // A now-valid address stays quiet on blur.
  await target.blur();
  await expect(dialog.getByText(/is not an email address/)).toHaveCount(0);
});

test("an empty field is never nagged on blur — that stays submit's business", async ({
  page,
}) => {
  const target = await emailOutputRow(page);
  const dialog = page.getByRole("dialog");

  // Tabbing straight through a fresh field must say nothing. "You haven't
  // filled this in yet" is true of every field an author passes on the way
  // somewhere else.
  await target.click();
  await target.blur();
  await expect(dialog.getByText(/is not an email address/)).toHaveCount(0);
});

test("changing a node's kind clears the field AND its error, leaving no orphan", async ({
  page,
}) => {
  const target = await emailOutputRow(page);
  const dialog = page.getByRole("dialog");

  await target.fill("nope");
  await target.blur();
  await expect(dialog.getByText(/is not an email address/)).toBeVisible();

  // Switching the kind un-renders the destination controls (PR #226's
  // `changeKind` reset). The error must go with them: an error about a field
  // with no control left on screen is one the author cannot clear.
  const kind = dialog.getByLabel("Node kind").nth(1);
  await kind.click();
  await page.getByRole("option", { name: /^Agent/ }).click();

  await expect(dialog.getByLabel("Recipient address")).toHaveCount(0);
  await expect(dialog.getByText(/is not an email address/)).toHaveCount(0);

  // Switching back gives a clean field, not a resurrected value or error.
  await kind.click();
  await page.getByRole("option", { name: /^Output/ }).click();
  await expect(dialog.getByLabel("Send report to")).toBeVisible();
  await expect(dialog.getByText(/is not an email address/)).toHaveCount(0);
});

/** Opens the trigger row's custom-cron input. */
async function customCronInput(page: Page) {
  const dialog = page.getByRole("dialog");
  await dialog.getByLabel("Schedule").click();
  await page.getByRole("option", { name: "Custom cron…" }).click();
  return dialog.getByLabel("Custom cron schedule");
}

test("a cron expression is read back in plain English, in UTC and local time", async ({
  page,
}) => {
  await openCreateDialog(page);
  const cron = await customCronInput(page);
  const dialog = page.getByRole("dialog");

  // **The issue #262 case.** `0 9 * * *` and `9 0 * * *` are two characters
  // apart, both valid, and nine hours apart in meaning. Only the preview tells
  // them apart before the report lands at the wrong time.
  await cron.fill("0 9 * * *");
  await expect(dialog.getByText("Every day at 09:00 UTC")).toBeVisible({
    timeout: PREVIEW_TIMEOUT,
  });
  // The local gloss is the part that earns its keep: it is the only place the
  // UTC contract becomes concrete rather than a hint the author skimmed.
  await expect(dialog.getByText(/your time\)/)).toBeVisible({ timeout: PREVIEW_TIMEOUT });

  await cron.fill("9 0 * * *");
  await expect(dialog.getByText("Every day at 00:09 UTC")).toBeVisible({
    timeout: PREVIEW_TIMEOUT,
  });
  await expect(dialog.getByText("Every day at 09:00 UTC")).toHaveCount(0, {
    timeout: PREVIEW_TIMEOUT,
  });

  // The UTC hint stays — the preview says what THIS expression means, it does
  // not replace the statement of the contract.
  await expect(dialog.getByText("5-field cron. Times are UTC.")).toBeVisible();
});

test("a schedule the humaniser won't paraphrase still previews its next runs", async ({
  page,
}) => {
  await openCreateDialog(page);
  const cron = await customCronInput(page);
  const dialog = page.getByRole("dialog");

  // A restricted day-of-month is left undescribed on purpose — a wrong
  // paraphrase would be worse than none — but the fire times still state it.
  await cron.fill("0 0 1 * *");
  await expect(dialog.getByText(/^Next runs:/)).toBeVisible({ timeout: PREVIEW_TIMEOUT });
});

test("garbage in the cron field previews the parser's message without blocking", async ({
  page,
}) => {
  await openCreateDialog(page);
  const cron = await customCronInput(page);
  const dialog = page.getByRole("dialog");

  // Five fields, so the shape check passes and the request goes out; the hour
  // is out of range, so the host's parser rejects it. That arrives as a 200
  // with a message, not a thrown error, and it does not disable anything.
  await cron.fill("0 99 * * *");
  await expect(dialog.getByText(/value out of range/)).toBeVisible({
    timeout: PREVIEW_TIMEOUT,
  });
  await expect(dialog.getByRole("button", { name: "Create workflow" })).toBeEnabled();

  // Fewer than five fields never reaches the wire — the pre-flight shape check
  // gates it — so the author sees the blur message, not a parser message.
  await cron.fill("hourly");
  await cron.blur();
  await expect(dialog.getByText(/5-field cron, e\.g\./)).toBeVisible();
  // And one mistake produces ONE complaint, not a blur error stacked on a
  // preview error.
  await expect(dialog.getByText(/value out of range/)).toHaveCount(0, {
    timeout: PREVIEW_TIMEOUT,
  });
});

test("a valid workflow still saves", async ({ page }) => {
  // The id AND the name must both be fresh: the host rejects a duplicate of
  // either, so a fixed name would pass on a clean company and fail on the
  // second run against the same one.
  const stamp = Date.now();
  await openCreateDialog(page);
  const dialog = page.getByRole("dialog");

  // `exact` matters: "Id" is also a substring of the row-level "Node id".
  await dialog.getByLabel("Id", { exact: true }).fill(`e2e_feedback_${stamp}`);
  await dialog.getByLabel("Name", { exact: true }).fill(`Feedback probe ${stamp}`);

  // Trigger row: a preset schedule, which previews too.
  await dialog.getByLabel("Node id").first().fill("start");
  await dialog.getByLabel("Node name").first().fill("Start");
  await dialog.getByLabel("Schedule").click();
  await page.getByRole("option", { name: /^Daily/ }).click();
  await expect(dialog.getByText("Every day at 09:00 UTC")).toBeVisible({
    timeout: PREVIEW_TIMEOUT,
  });

  // Output row routing to the owner — no target to get wrong.
  await dialog.getByRole("button", { name: "Add node" }).click();
  await dialog.getByLabel("Node id").nth(1).fill("done");
  await dialog.getByLabel("Node name").nth(1).fill("Ship it");
  const kind = dialog.getByLabel("Node kind").nth(1);
  await kind.click();
  await page.getByRole("option", { name: /^Output/ }).click();
  await dialog.getByLabel("Send report to").click();
  await page.getByRole("option", { name: /^Owner/ }).click();

  await dialog.getByRole("button", { name: "Create workflow" }).click();
  await expect(dialog).toBeHidden({ timeout: 30_000 });
});
