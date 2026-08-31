import { expect, request as playwrightRequest, test } from "@playwright/test";

import { LIVE_BRAIN, LIVE_BRAIN_REASON } from "./capabilities";

/**
 * Issue #1796 — a connect page must be able to grant the `[tools].allow`
 * namespace it needs, instead of telling the operator it cannot.
 *
 * # What was broken
 *
 * Connecting an integration and granting its tool namespace are two separate
 * steps, and only the first had a write path anywhere in the product. Five
 * surfaces — Chargebee, PayPal, hosting, search and Composio — each ended in a
 * variant of *"Add `x` to `[tools].allow` in the company's manifest — it cannot
 * be fixed from this page."* The sentence was accurate, which is what made it a
 * product failure rather than a copy failure: the integration read **Connected**
 * and reached nobody, and on a hosted tenant the manifest is a read-only boot
 * snapshot baked into the image, so there was no page and no file the operator
 * could go to at all.
 *
 * # Why this runs against the real host
 *
 * The unit tests prove the button calls the route and the route stores an
 * override. Neither can prove the part that actually matters: that the grant
 * reaches the list the harness reads, through a real Rust host, a real store and
 * a real reload. `[tools].allow` is read at some three dozen sites, and a fix
 * that satisfied the console while leaving those readers on the old list would
 * reproduce the exact bug it claims to close.
 *
 * So this drives the console, clicks the control, and then **reloads the page**
 * — a fresh read from the host, not a re-render of optimistic state — and
 * requires the warning to stay gone.
 *
 * # Why the live-brain lane
 *
 * Both surfaces gate their grant warning on `inBuild`, and rightly: granting
 * `search` fixes nothing on a host compiled without the tools to hand out, so a
 * default-feature host shows the not-in-build alert instead and there is no
 * grant to offer. `inBuild` for search and hosting is `cfg!(feature =
 * "openhuman")`, so the `Console E2E (live brain)` lane — which builds
 * `--features openhuman,mcp,composio` — is the one where these pages reach the
 * state the issue describes at all. The route itself is always compiled and is
 * covered without a feature by `src/server/ops/tool_grants.rs`.
 *
 * `companies/e2e_harness` grants `composio`, `mcp:*`, `workspace*` and `web`,
 * and deliberately not `search` or `hosting`: the priced search family is
 * withheld on purpose so a wildcard can never buy a metered request. That makes
 * it the right fixture for this — a company in exactly the state the issue
 * describes, with no manifest edit needed to produce it.
 */

test.skip(!LIVE_BRAIN, LIVE_BRAIN_REASON);

type Page = import("@playwright/test").Page;

/**
 * The namespaces these specs grant, withdrawn again when they are done.
 *
 * Load-bearing, not tidiness. `companies/e2e_harness` is **shared**: the lane
 * runs `fullyParallel: false, workers: 1` against one host, and locally
 * `reuseExistingServer` can hand the next run the same one. A grant left behind
 * leaks two ways — the first spec here never sees `search-not-granted` on a
 * second run and hangs out its 30s wait, and every later spec in the lane runs
 * against a fixture that now grants `search` and `hosting`, which this company
 * withholds on purpose so a wildcard can never buy a metered request.
 *
 * `DELETE …/tools/grants?namespace=` removes only what the console added, so
 * this cannot damage the fixture's own `[tools].allow`.
 */
const GRANTED = ["search", "hosting"];

/**
 * Withdraws what these specs grant.
 *
 * A context of its own: the per-test fixtures are gone in `beforeAll`/`afterAll`,
 * and the signed-in storage state is what makes the `DELETE` an admin's.
 */
async function revokeAll(baseURL: string | undefined, storageState: unknown) {
  const context = await playwrightRequest.newContext({
    baseURL,
    storageState: storageState as string | undefined,
  });
  try {
    for (const namespace of GRANTED) {
      await context.delete(`/api/v1/company/tools/grants?namespace=${namespace}`);
    }
  } finally {
    await context.dispose();
  }
}

// Before as well as after. `afterAll` keeps the fixture clean for the rest of
// the lane, but it cannot help a run that inherits a dirty host — a previous
// run killed mid-suite, or `reuseExistingServer` handing this one the same
// process. Starting from a known state is what makes the first spec's
// "not granted" precondition true rather than hopeful.
test.beforeAll(async ({ baseURL }, testInfo) => {
  await revokeAll(baseURL, testInfo.project.use.storageState);
});

test.afterAll(async ({ baseURL }, testInfo) => {
  await revokeAll(baseURL, testInfo.project.use.storageState);
});

/**
 * A fresh browser context has no tour state, so the first-run welcome dialog
 * opens over the console and swallows clicks. Skip it when it shows up.
 */
async function open(page: Page, path: string) {
  await page.goto(path);
  const skip = page.getByRole("button", { name: "Skip for now" });
  await skip
    .waitFor({ state: "visible", timeout: 10_000 })
    .then(() => skip.click())
    .catch(() => {
      /* already seen in this context — nothing to dismiss */
    });
}

test("Settings → Search can grant `search`, and the grant survives a reload", async ({ page }) => {
  await open(page, "/#/settings/search");

  // The state the issue is reported from: a page that knows the company does
  // not grant the namespace its own form is for.
  const warning = page.getByTestId("search-not-granted");
  await expect(warning).toBeVisible({ timeout: 30_000 });

  // The dead-end sentence is gone from the product.
  await expect(warning).not.toContainText("cannot be fixed from this page");

  const grant = page.getByTestId("search-not-granted-action");
  await expect(grant).toBeVisible();
  await expect(grant).toHaveText(/Grant search/);
  await grant.click();

  // The host confirms, in its own words about when the grant bites — an
  // operator told a bare "done" who then watches the current turn behave
  // exactly as before would reasonably conclude the button did nothing.
  await expect(page.getByText("This company now grants search")).toBeVisible({
    timeout: 15_000,
  });

  // The page's own status re-read has moved off "not granted".
  await expect(warning).toHaveCount(0);

  // The half a unit test cannot reach: a full reload, so the verdict is
  // rebuilt from a fresh `GET …/search` served by the real host out of the real
  // store. A grant that lived only in the tab would pass everything above.
  await page.reload();
  await expect(page.getByTestId("search-view")).toBeVisible({ timeout: 30_000 });
  await expect(page.getByTestId("search-not-granted")).toHaveCount(0);
});

test("the grant is a company-wide fact, not a per-page one", async ({ page, request }) => {
  await open(page, "/#/settings/hosting");

  const warning = page.getByTestId("hosting-not-granted");
  await expect(warning).toBeVisible({ timeout: 30_000 });
  await page.getByTestId("hosting-not-granted-action").click();
  await expect(warning).toHaveCount(0);

  // Read the grant back through the host's own API, using the browser's session.
  // This is the assertion that ties the console's claim to what every other
  // reader of `[tools].allow` will see: the roster build, the harness tool
  // wiring, and the four other connect surfaces. `allow` is the effective list;
  // `manifestAllow` is version control's own, and `hosting` must be in exactly
  // one of them or the page is claiming credit for a grant it did not make.
  const cookies = await page.context().cookies();
  const response = await request.get("/api/v1/company/tools/grants", {
    headers: {
      cookie: cookies.map((c) => `${c.name}=${c.value}`).join("; "),
    },
  });
  expect(response.ok()).toBeTruthy();
  const grants = (await response.json()) as {
    allow: string[];
    manifestAllow: string[];
    added: string[];
    setBy?: string;
  };
  expect(grants.allow).toContain("hosting");
  expect(grants.manifestAllow).not.toContain("hosting");
  expect(grants.added).toContain("hosting");
  // Widening what a company's agents can reach is attributed. A grant that
  // could be made anonymously is not much of a boundary.
  expect(grants.setBy).toBeTruthy();
});
