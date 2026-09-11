import { expect, test } from "@playwright/test";

/**
 * Connections → LLM, against a real browser and a real host.
 *
 * The `Console E2E` job runs this (issue #428) and it is a merge gate, so treat
 * a red run here as a real regression rather than a stale reproduction.
 *
 * ## What these cover, and why they are the ones that need a browser
 *
 * The rules this surface is built on are pure functions with unit tests —
 * which category offers what, what a probe class means, what a removal
 * orphans, whether an override is sendable. None of those needs a browser.
 *
 * What does need one is the part that only breaks in integration: a credential
 * travelling from a dialog through a write route into a store and back as a
 * boolean, a probe classification reaching the row it belongs to, and a delete
 * that has to move a route in a different subsystem. Those are below.
 *
 * ## The invariant that predates the list (issue #265)
 *
 * The page must never report a successful save for a save that threw the
 * operator's key away. It used to be possible because there was **one**
 * credential slot: switching provider left the previous vendor's key in it, and
 * a managed save was a revert that carried none. The list removes the shape of
 * the bug — each provider holds its own credential — and the test for it is now
 * "two providers, two independent keys", below.
 */

type Page = import("@playwright/test").Page;

/**
 * A fresh browser context has no tour state, so the first-run welcome dialog
 * opens over the console and swallows clicks. Skip it when it shows up.
 */
async function openInference(page: Page) {
  await page.goto("/#/settings/inference");
  const skip = page.getByRole("button", { name: "Skip for now" });
  await skip
    .waitFor({ state: "visible", timeout: 10_000 })
    .then(() => skip.click())
    .catch(() => {
      /* already seen in this context — nothing to dismiss */
    });
  // Either the list or its empty state — a company with nothing connected shows
  // the second, and waiting only for the first would hang on exactly the
  // company a first run starts from.
  await expect(
    page.getByTestId("inference-providers").or(page.getByTestId("inference-providers-empty")),
  ).toBeVisible({ timeout: 30_000 });
}

/** Open the add dialog and choose one option out of a category. */
async function choose(page: Page, category: "cloud" | "local" | "cli", label: string) {
  await page.getByTestId("inference-add-open").click();
  await expect(page.getByTestId("inference-add-provider")).toBeVisible();
  await page.locator(`#inference-add-${category}`).click();
  await page.getByRole("option", { name: new RegExp(label) }).click();
}

/** The discard port: refused immediately, no DNS, no wait. */
const UNREACHABLE = "http://127.0.0.1:9/v1";

test("Managed is always present and is a badge rather than a switch", async ({ page }) => {
  await openInference(page);

  const managed = page.getByTestId("inference-provider-managed");
  await expect(managed).toBeVisible();
  await expect(managed).toContainText("Always on");
  // A locked switch reads as switchable-but-broken and invites a fight the
  // operator cannot win, so there must not be one on this row.
  await expect(managed.locator("[role='switch']")).toHaveCount(0);
});

test("a provider behind an unreachable endpoint is saved, amber, and keeps its key", async ({
  page,
}) => {
  // The non-destructive path, and the one the naive implementation gets wrong:
  // a proxy, a WAF, a rate limit and a mistyped model id all fail a probe while
  // the key is perfectly good.
  await openInference(page);

  await page.getByTestId("inference-add-custom").click();
  await page.locator("#inference-connect-name").fill("E2E Gateway");
  await expect(page.getByTestId("inference-slug-preview")).toHaveText("Slug: e2e-gateway");
  await page.locator("#inference-connect-url").fill(UNREACHABLE);
  await page.locator("#inference-connect-key").fill(`pw-e2e-${Date.now()}`);
  await page.getByTestId("inference-connect-submit").click();

  const row = page.getByTestId("inference-provider-e2e-gateway");
  await expect(row).toBeVisible({ timeout: 30_000 });
  // The row was created and the credential was kept: the save succeeded, and
  // only reachability is in question.
  await expect(row).toContainText("•••• configured");
  await expect(page.getByTestId("inference-provider-e2e-gateway-health")).toContainText(
    "unreachable",
  );

  // And it survives a reload, which is the half a component test cannot see.
  await page.reload();
  await openInference(page);
  await expect(page.getByTestId("inference-provider-e2e-gateway")).toContainText(
    "•••• configured",
  );
});

test("a second provider holds a credential of its own", async ({ page }) => {
  // The first moment two keys exist at once. One slot per company is why
  // switching provider used to strand a credential for the wrong vendor in the
  // only slot there was.
  await openInference(page);

  for (const name of ["E2E One", "E2E Two"]) {
    await page.getByTestId("inference-add-custom").click();
    await page.locator("#inference-connect-name").fill(name);
    await page.locator("#inference-connect-url").fill(UNREACHABLE);
    await page.locator("#inference-connect-key").fill(`pw-e2e-${name}-${Date.now()}`);
    await page.getByTestId("inference-connect-submit").click();
    await expect(page.getByTestId("inference-connect-provider")).toHaveCount(0, {
      timeout: 30_000,
    });
  }

  await expect(page.getByTestId("inference-provider-e2e-one")).toContainText("•••• configured");
  await expect(page.getByTestId("inference-provider-e2e-two")).toContainText("•••• configured");
});

test("the add dialog stops offering a provider once it is connected", async ({ page }) => {
  // Offering to add something twice is how you get two rows for one provider.
  await openInference(page);

  await choose(page, "cloud", "Groq");
  await page.locator("#inference-connect-key").fill(`pw-e2e-${Date.now()}`);
  await page.getByTestId("inference-connect-submit").click();
  await expect(page.getByTestId("inference-provider-groq")).toBeVisible({ timeout: 30_000 });

  await page.getByTestId("inference-add-open").click();
  await page.locator("#inference-add-cloud").click();
  await expect(page.getByRole("option", { name: /^Groq/ })).toHaveCount(0);
});

test("a custom provider may not take a name the catalogue ships", async ({ page }) => {
  // A routing entry saying `groq` would otherwise mean two things — and the
  // refusal happens before anything is written.
  await openInference(page);

  await page.getByTestId("inference-add-custom").click();
  await page.locator("#inference-connect-name").fill("Groq");
  await page.locator("#inference-connect-url").fill(UNREACHABLE);
  await expect(page.getByTestId("inference-slug-error")).toContainText("built-in");
  await expect(page.getByTestId("inference-connect-submit")).toBeDisabled();
});

test("disabling a provider keeps its credential and its routes", async ({ page }) => {
  // Distinct from deleting it: "stop billing this account this week" has to be
  // expressible, and a disable that scrubbed would make re-enabling a
  // re-configuration.
  await openInference(page);

  await page.getByTestId("inference-add-custom").click();
  await page.locator("#inference-connect-name").fill("E2E Parked");
  await page.locator("#inference-connect-url").fill(UNREACHABLE);
  await page.locator("#inference-connect-key").fill(`pw-e2e-${Date.now()}`);
  await page.getByTestId("inference-connect-submit").click();
  await expect(page.getByTestId("inference-provider-e2e-parked")).toBeVisible({ timeout: 30_000 });

  await page.getByTestId("inference-provider-e2e-parked-toggle").click();
  await page.reload();
  await openInference(page);

  const row = page.getByTestId("inference-provider-e2e-parked");
  await expect(row.locator("[role='switch']")).toHaveAttribute("aria-checked", "false");
  await expect(row).toContainText("•••• configured");
});

test("deleting a provider clears its key and resets the routes that named it", async ({ page }) => {
  await openInference(page);

  await page.getByTestId("inference-add-custom").click();
  await page.locator("#inference-connect-name").fill("E2E Doomed");
  await page.locator("#inference-connect-url").fill(UNREACHABLE);
  await page.locator("#inference-connect-key").fill(`pw-e2e-${Date.now()}`);
  await page.getByTestId("inference-connect-submit").click();
  await expect(page.getByTestId("inference-provider-e2e-doomed")).toBeVisible({ timeout: 30_000 });

  // Point one workload at it, through the routing tab.
  await page.getByRole("tab", { name: "Routing" }).click();
  await page.getByTestId("inference-mode-advanced").click();
  await page
    .getByTestId("inference-workload-reasoning")
    .getByRole("button", { name: /Model$/ })
    .click();
  await page.locator("#inference-workload-provider").click();
  await page.getByRole("option", { name: "E2E Doomed", exact: true }).click();
  await page.getByTestId("inference-workload-apply").click();
  await expect(page.getByTestId("inference-workload-reasoning")).toContainText("E2E Doomed");

  // Remove it, and the row that named it moves back to the primary.
  await page.getByRole("tab", { name: "LLM Providers" }).click();
  await page.getByTestId("inference-provider-e2e-doomed-menu").click();
  await page.getByRole("menuitem", { name: "Remove" }).click();
  await expect(page.getByTestId("inference-provider-e2e-doomed")).toHaveCount(0, {
    timeout: 30_000,
  });

  await page.getByRole("tab", { name: "Routing" }).click();
  await page.getByTestId("inference-mode-advanced").click();
  await expect(page.getByTestId("inference-workload-reasoning")).toContainText("Primary (");
});

test("the routing mode is inferred from the routes and round-trips", async ({ page }) => {
  // There is no stored mode to drift out of sync with the rows.
  await openInference(page);
  await page.getByRole("tab", { name: "Routing" }).click();

  await page.getByTestId("inference-mode-managed").click();
  await page.reload();
  await openInference(page);
  await page.getByRole("tab", { name: "Routing" }).click();
  await expect(page.getByTestId("inference-mode-managed")).toHaveAttribute("aria-pressed", "true");
});

test("coding is shown as an alias and cannot be routed separately", async ({ page }) => {
  // Four editable rows, not five: coding and agentic are one tier, so an
  // editable coding row would write one tier's route under two names.
  await openInference(page);
  await page.getByRole("tab", { name: "Routing" }).click();
  await page.getByTestId("inference-mode-advanced").click();

  const coding = page.getByTestId("inference-workload-coding");
  await expect(coding).toContainText("Follows Agentic");
  await expect(coding.getByRole("button")).toHaveCount(0);
});
