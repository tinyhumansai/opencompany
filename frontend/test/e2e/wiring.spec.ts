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
