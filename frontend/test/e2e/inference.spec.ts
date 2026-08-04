import { expect, test } from "@playwright/test";

/**
 * Issue #265 — Connections → Inference must never report a successful save for
 * a save that threw the operator's key away.
 *
 * The key input is rendered only for the bring-your-own-key providers, but the
 * typed value lives in form state that survives a switch back to **managed**.
 * Managed is a revert (`DELETE …/inference`) and carries no credential, so the
 * old code dropped that pending key and still toasted "Inference updated".
 *
 * This spec drives the real browser against a live host. It is not part of CI
 * (the Playwright config declares no `webServer`), so treat it as an executable
 * reproduction plus documentation of the invariant, not a merge gate.
 */

const CONFLICT = "inference-key-conflict";

type Page = import("@playwright/test").Page;

/** Pick a provider from the base-ui select. */
async function pickProvider(page: Page, label: string) {
  await page.locator("#inference-provider").click();
  await page.getByRole("option", { name: label, exact: true }).click();
}

/**
 * A fresh browser context has no tour state, so the first-run welcome dialog
 * opens over the console and swallows clicks. Skip it when it shows up.
 */
async function openConnections(page: Page) {
  await page.goto("/#/connections");
  const skip = page.getByRole("button", { name: "Skip for now" });
  await skip
    .waitFor({ state: "visible", timeout: 10_000 })
    .then(() => skip.click())
    .catch(() => {
      /* already seen in this context — nothing to dismiss */
    });
}

test("a key typed for a BYOK provider is not discarded by switching to managed", async ({
  page,
}) => {
  await openConnections(page);

  // Managed is the default selection: no key input, and a line that says why.
  await expect(page.getByTestId("inference-managed-note")).toBeVisible({ timeout: 30_000 });
  await expect(page.locator("#inference-key")).toHaveCount(0);
  await expect(page.getByTestId(CONFLICT)).toHaveCount(0);

  const before = await page.request.get("/api/v1/company/inference");
  expect(before.ok()).toBeTruthy();
  const keyConfiguredBefore = (await before.json()).keyConfigured;

  // Type a key under a provider that can actually hold one.
  await pickProvider(page, "OpenRouter");
  const typed = `pw-e2e-${Date.now()}`;
  await page.locator("#inference-key").fill(typed);

  // Then switch back to managed — the input goes away but the value does not.
  await pickProvider(page, "Managed (TinyHumans)");
  await expect(page.locator("#inference-key")).toHaveCount(0);

  // The regression: Save must be refused, loudly, instead of reverting and
  // reporting success while the credential is dropped on the floor.
  await expect(page.getByTestId(CONFLICT)).toBeVisible();
  await expect(page.getByTestId("inference-save")).toBeDisabled();
  await expect(page.getByText("Inference updated.")).toHaveCount(0);

  // Nothing reached the host: the stored-credential flag is exactly as it was.
  const after = await page.request.get("/api/v1/company/inference");
  expect(after.ok()).toBeTruthy();
  expect((await after.json()).keyConfigured).toBe(keyConfiguredBefore);

  // Discarding is an explicit act, and it unblocks the managed save.
  await page.getByTestId("inference-discard-key").click();
  await expect(page.getByTestId(CONFLICT)).toHaveCount(0);
  await expect(page.getByTestId("inference-save")).toBeEnabled();

  // Going back to a BYOK provider offers an empty field, not the ghost value.
  await pickProvider(page, "OpenRouter");
  await expect(page.locator("#inference-key")).toHaveValue("");
});

test("a key typed for a BYOK provider does reach the host on save", async ({ page }) => {
  // The guard above must not be a blanket block: the same input, saved under a
  // provider that can store it, has to land server-side. The credential is
  // write-only, so `keyConfigured` is the only observable — run this against a
  // fresh `--home` for it to mean "this save stored it".
  await openConnections(page);
  await expect(page.getByTestId("inference-managed-note")).toBeVisible({ timeout: 30_000 });

  await pickProvider(page, "Custom (OpenAI-compatible)");
  await page.locator("#inference-base-url").fill("http://127.0.0.1:9/v1");
  await page.locator("#inference-model-chat-v1").fill("pw-e2e-model");
  await page.locator("#inference-key").fill(`pw-e2e-${Date.now()}`);
  await page.getByTestId("inference-save").click();

  // Either success wording is correct here, and which one shows is not this
  // spec's business: a company that booted with no inference source is on the
  // echo brain, so issue #266 makes the host report `restartRequired` for
  // exactly this not-configured → configured save and the toast says "restart"
  // instead of "next turn". What #265 asserts is that the save was *accepted*
  // rather than refused — the stored-credential check below is the real proof.
  await expect(
    page.getByText(/Inference updated\.|Inference saved — restart the company/),
  ).toBeVisible({ timeout: 30_000 });
  const status = await page.request.get("/api/v1/company/inference");
  expect(status.ok()).toBeTruthy();
  const body = await status.json();
  expect(body.keyConfigured).toBe(true);
  expect(body.provider).toBe("openai_compatible");

  // Put the company back on the managed default for whatever runs next.
  await page.getByRole("button", { name: "Reset to managed" }).click();
  await expect(page.getByText("Reverted to the managed configuration.")).toBeVisible({
    timeout: 30_000,
  });
});
