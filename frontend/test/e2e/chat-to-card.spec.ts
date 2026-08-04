import { expect, test, type Page } from "@playwright/test";

/**
 * End-to-end proof for the chat↔card edge (issue #246).
 *
 * Runs against a live host the harness brings up separately, with an inference
 * backend whose *choices* are scripted: a prompt carrying `SPAWNONE` makes the
 * orchestrator call `spawn_task` once. Everything else — the harness, the tool
 * plumbing, the cycle, the journal, the HTTP surface — is real.
 *
 * Three things are asserted that a curl cannot reach:
 *
 * 1. the "Add to board" action exists on a **desk** thread, where no responder
 *    carries the delegation tools and a card was previously unreachable;
 * 2. the chip a spawned card produces survives a **reload**, not merely the
 *    live POST response;
 * 3. the card links back to the conversation it came from.
 */

/**
 * A console opened against a fresh company home shows the welcome tour, which
 * renders as a modal over the whole console and swallows the first click.
 * Dismiss it when it is up. Whether it appears depends on console-local state,
 * so its absence is not a failure.
 */
async function dismissWelcome(page: Page) {
  const skip = page.getByRole("button", { name: /Skip for now/ });
  try {
    await skip.waitFor({ state: "visible", timeout: 5_000 });
  } catch {
    return;
  }
  await skip.click();
  await skip.waitFor({ state: "hidden", timeout: 10_000 });
}

/**
 * Opens the conversation view and selects a thread by its contact name.
 *
 * Scoped to the chat list: the sidebar's company switcher is also a button and
 * also carries the company name, and it precedes the list in the DOM — so an
 * unscoped `.first()` resolves to the switcher and never opens a thread.
 */
async function openThread(page: Page, name: RegExp) {
  await page.goto("/#/conversation");
  await dismissWelcome(page);
  await page.getByRole("complementary").getByRole("button", { name }).first().click();
}

test("any message on a desk thread can be added to the board", async ({ page }) => {
  await openThread(page, /Engineering desk/);

  const prompt = `ship the launch checklist ${Date.now()}`;
  await page.getByPlaceholder(/^Message /).fill(prompt);
  await page.getByRole("button", { name: "Send" }).click();

  // The operator's own bubble is the one being turned into a card.
  const bubble = page.getByText(prompt, { exact: true }).first();
  await expect(bubble).toBeVisible({ timeout: 60_000 });

  // The action is hover-revealed but always focusable; hover for realism.
  await bubble.hover();
  const row = page.locator("div.group\\/msg", { hasText: prompt }).first();
  await row.getByRole("button", { name: "Add to board" }).click();

  // The confirmation chip appears on that message and links to the card.
  const chip = row.getByRole("link", { name: /Added to the board/ });
  await expect(chip).toBeVisible({ timeout: 30_000 });
  const href = await chip.getAttribute("href");
  expect(href).toMatch(/^#\/tasks\/.+/);

  // The card is real, titled from the message, and — the spend gate — did NOT
  // land in `in_progress`.
  await page.goto(href!);
  await dismissWelcome(page);
  await expect(page.getByText(prompt).first()).toBeVisible({ timeout: 30_000 });
  await expect(page.getByText("In progress", { exact: true })).toHaveCount(0);

  // …and it knows which conversation opened it.
  await expect(page.getByRole("button", { name: /Opened from chat/ })).toBeVisible();
});

test("a card the orchestrator opens is chipped in chat, and survives a reload", async ({
  page,
}) => {
  await openThread(page, /Your company/);

  // `SPAWNONE` is the scripted backend's cue to call `spawn_task` once.
  const prompt = `please track this SPAWNONE ${Date.now()}`;
  await page.getByPlaceholder(/^Message /).fill(prompt);
  await page.getByRole("button", { name: "Send" }).click();

  // Live: the reply bubble says a card was opened.
  const chip = page.getByRole("link", { name: /Card opened/ }).last();
  await expect(chip).toBeVisible({ timeout: 60_000 });
  const href = await chip.getAttribute("href");
  expect(href).toMatch(/^#\/tasks\/.+/);

  // After a reload the transcript is rehydrated from `chat/history`, so a chip
  // that only existed on the live POST response would vanish here.
  await page.reload();
  await openThread(page, /Your company/);
  const rehydrated = page.getByRole("link", { name: /Card opened/ }).last();
  await expect(rehydrated).toBeVisible({ timeout: 30_000 });
  expect(await rehydrated.getAttribute("href")).toBe(href);
});
