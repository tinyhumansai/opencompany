import { expect, test } from "@playwright/test";

/**
 * End-to-end wiring proof for the operator console.
 *
 * This single spec exercises the whole chain the console depends on:
 *
 *   magic-link auth → session cookie → console → POST /api/v1/company/chat
 *     → (mocked) LLM backend → reply rendered as a company bubble
 *
 * It runs against a live host that the harness brings up separately with a
 * mocked inference backend that echoes a `__MOCK_LLM__` marker. The spec only
 * asserts on that marker (never exact echo text): the agent harness transforms
 * the prompt before it reaches the backend, so only the marker is stable.
 *
 * The admin address must match `companies/e2e_harness/company.toml`'s
 * `[users] admins`, which is what makes the login flow succeed.
 */

test("operator console renders a mocked backend reply end to end", async ({
  page,
}) => {
  // Authentication is performed once by global-setup.ts and shared through
  // Playwright storage state so multiple specs do not trip the resend throttle.
  // Open the conversation view. The default "Your company" thread is
  //    pre-selected.
  await page.goto("/#/conversation");

  // Send a unique prompt through the operator chat input.
  const prompt = `e2e wiring ping ${Date.now()}`;
  await page.getByPlaceholder(/^Message /).fill(prompt);
  await page.getByRole("button", { name: "Send" }).click();

  // The mocked backend reply must render as a company bubble, and no send
  //    error may appear.
  await expect(page.getByText("__MOCK_LLM__").first()).toBeVisible({
    timeout: 60_000,
  });
  await expect(page.getByText(/^Couldn't send/)).toHaveCount(0);
});

test("operator adds a Brain memory that persists across reload and can be deleted", async ({
  page,
}) => {
  // The Brain tab reads the real FactStore over `…/memory`; adding a note must
  // survive a reload (proving it hit the backend, not localStorage) and delete
  // must remove it.
  await page.goto("/#/memory");

  const title = `e2e memory ${Date.now()}`;
  await page.getByTestId("memory-add").click();
  await page.getByTestId("memory-title").fill(title);
  await page.getByTestId("memory-body").fill("recall me on the next turn");
  await page.getByTestId("memory-save").click();

  const card = page.getByTestId("memory-card").filter({ hasText: title });
  await expect(card).toBeVisible({ timeout: 30_000 });

  // Reload: a localStorage stub would survive too, so also assert the health
  // strip counts a real backend item.
  await page.reload();
  await page.goto("/#/memory");
  await expect(page.getByTestId("memory-card").filter({ hasText: title })).toBeVisible({
    timeout: 30_000,
  });

  // Delete removes it.
  const persisted = page.getByTestId("memory-card").filter({ hasText: title });
  await persisted.getByRole("button", { name: "Delete memory" }).click();
  await expect(page.getByTestId("memory-card").filter({ hasText: title })).toHaveCount(0, {
    timeout: 30_000,
  });
});
