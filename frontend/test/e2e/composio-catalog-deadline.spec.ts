import { expect, test, type Page } from "@playwright/test";

import { COMPOSIO, COMPOSIO_FIXTURE_URL, COMPOSIO_REASON } from "./capabilities";

/**
 * Issue #2007 — the Apps page painted "couldn't check" on a first visit and the
 * whole catalog on the second.
 *
 * # The shape of it
 *
 * Two budgets, nested, and the same size. The console bounded `GET …/composio`
 * at five seconds; the host bounds its own upstream catalog fetch at five
 * seconds *inside* that call. So on a cold catalog the host spends its whole
 * budget upstream, degrades to a flagged fallback, and starts writing an answer
 * that explains itself — at exactly the moment the console stops listening. The
 * abandoned request still completed server-side and filled a fifteen-minute
 * cache, which is why the second visit worked. The reload in the report was a
 * red herring: any visit inside that window would have done.
 *
 * # Why this needs a browser
 *
 * `test/unit/oauth-catalog-probe.test.ts` drives the same states against a
 * scripted transport, and it is the faster gate. It cannot see the half that
 * actually broke: that a REAL host, degrading for its own real reasons, has
 * something to say — and that the console is still listening when it says it.
 * The fallback notice is produced here by a genuinely slow upstream, not by a
 * fixture asserting the shape somebody believed the host emits.
 *
 * # The two clocks, kept apart on purpose
 *
 * The host's degradation is caused by the **fixture's** catalog delay. The
 * console's wait is caused by a **route delay in the browser**. Separating them
 * is what makes these specs deterministic: the host's catalog cache is
 * process-level with a fifteen-minute TTL, so a spec that tried to produce the
 * console's wait by keeping the cache cold would pass or fail on what the spec
 * before it happened to leave behind. Every spec here evicts that cache
 * explicitly (setting the Composio token evicts it) and then states the
 * console's latency itself.
 */

test.skip(!COMPOSIO || !COMPOSIO_FIXTURE_URL, COMPOSIO_REASON);

/** The Composio bearer the company holds. The fixture checks no auth. */
const TOKEN = "e2e-catalog-deadline-token";

/**
 * Longer than the host's own upstream budget (`FETCH_TIMEOUT`, five seconds),
 * so the host gives up on the catalog and answers with its flagged fallback.
 */
const PAST_THE_HOSTS_BUDGET_MS = 20_000;

/**
 * How long the console's own read is made to take.
 *
 * A second past the budget the console used to hold, which is the window the
 * defect lived in: long enough that the old five-second race had already fired,
 * short enough that a healthy read is plainly what arrives.
 */
const CONSOLE_LATENCY_MS = 6_000;

const PROBE_FAILED = "providers-probe-failed";
const GRANT_UNKNOWN = "Couldn't check whether this company grants";

/**
 * A provider this company has NOT connected.
 *
 * Load-bearing, and the reason it is not Gmail or Slack: a connected provider
 * gets a tile from the connection list alone, so it appears whether or not the
 * catalog ever arrived. An assertion on one of those passes on a page whose
 * catalog read failed — which is precisely the state under test. Only an
 * unconnected provider proves the catalog reached the grid.
 */
const UNCONNECTED = {
  slug: "notion",
  name: "Notion",
  enabled: true,
  description: "Pages and databases.",
  categories: ["productivity"],
};

/** Which toolkits the fixture publishes. */
async function setCatalog(page: Page, entries: unknown[]): Promise<void> {
  const set = await page.request.post(`${COMPOSIO_FIXTURE_URL}/__catalog`, {
    data: { catalog: entries },
  });
  expect(set.ok(), `the composio fixture refused /__catalog: ${set.status()}`).toBeTruthy();
}

/** How long the fixture's catalog route takes to answer. */
async function setCatalogDelay(page: Page, toolkitsMs: number): Promise<void> {
  const set = await page.request.post(`${COMPOSIO_FIXTURE_URL}/__delay`, {
    data: { toolkitsMs },
  });
  expect(set.ok(), `the composio fixture refused /__delay: ${set.status()}`).toBeTruthy();
}

/**
 * Give this company a Composio credential — and, in doing so, drop the host's
 * cached catalog for it.
 *
 * The eviction is the point as much as the credential: a rotated token can
 * resolve to a different Composio account, so the host drops the cached catalog
 * on every token write. That makes this the one lever a spec has to guarantee
 * the next catalog read is genuinely cold, short of restarting the host.
 */
async function setTokenAndEvictCatalog(page: Page): Promise<void> {
  const set = await page.request.put("/api/v1/company/composio/token", {
    data: { token: TOKEN },
  });
  expect(set.ok(), `setting the composio token failed: ${set.status()}`).toBeTruthy();
}

/**
 * The status read, and only it.
 *
 * A predicate rather than a glob because the console addresses its host by
 * whichever scope its connection carries — `/api/v1/company/…` in
 * single-company mode, `/api/v1/companies/<id>/…` otherwise — and a pattern
 * pinned to one of them silently matches nothing against the other. The `$`
 * keeps `…/composio/connections` and `…/composio/token` out of it.
 */
const isComposioStatus = (url: URL) => /\/composio$/.test(url.pathname);

/** Make the console's own status read take `ms`, whatever the host is doing. */
async function delayTheConsolesRead(page: Page, ms: number): Promise<void> {
  await page.route(isComposioStatus, async (route) => {
    await new Promise((resolve) => setTimeout(resolve, ms));
    await route.continue();
  });
}

/**
 * The provider grid, and nothing else on the page.
 *
 * Scoped deliberately: "Gmail" also appears in the account-choice section
 * below, which is fed by a different read and paints while the catalog is still
 * in flight — an unscoped match there passes before the thing under test has
 * happened.
 */
function providerGrid(page: Page) {
  return page.locator("section", { has: page.getByRole("heading", { name: "Providers" }) });
}

async function openApps(page: Page): Promise<void> {
  await page.goto("/#/connections/apps");
  const skip = page.getByRole("button", { name: "Skip for now" });
  await skip
    .waitFor({ state: "visible", timeout: 10_000 })
    .then(() => skip.click())
    .catch(() => {
      /* already dismissed in this context */
    });
  await expect(skip).toBeHidden({ timeout: 10_000 });
  await expect(page.getByRole("heading", { name: "Providers" })).toBeVisible({ timeout: 60_000 });
}

test.beforeEach(async ({ page }) => {
  await page.request.post(`${COMPOSIO_FIXTURE_URL}/__reset`);
});

test("a first visit paints the catalog the host sent, however long it took to send it", async ({
  page,
}) => {
  test.setTimeout(120_000);
  await setCatalogDelay(page, 0);
  await setCatalog(page, [UNCONNECTED]);
  await setTokenAndEvictCatalog(page);
  await delayTheConsolesRead(page, CONSOLE_LATENCY_MS);

  await openApps(page);

  // The catalog the fixture actually serves, on the FIRST visit — no reload,
  // no warm cache. Asserted on the unconnected provider: Gmail and Slack have
  // tiles either way, so they cannot tell a catalog that arrived from one that
  // was abandoned.
  const grid = providerGrid(page);
  await expect(grid.getByText(UNCONNECTED.name, { exact: true })).toBeVisible({ timeout: 60_000 });

  // And neither warning. They were never two failures: abandoning the read
  // nulled the whole status, so the catalog warning and the grant warning came
  // from one abandoned request.
  await expect(page.getByTestId(PROBE_FAILED)).toHaveCount(0);
  await expect(page.getByText(GRANT_UNKNOWN)).toHaveCount(0);
});

test("a host that degraded gets to say so, instead of being cut off mid-sentence", async ({
  page,
}) => {
  test.setTimeout(120_000);
  // The upstream outlasts the host's own budget, so the host degrades for real:
  // it serves its built-in starter list and marks it as one.
  await setCatalogDelay(page, PAST_THE_HOSTS_BUDGET_MS);
  await setTokenAndEvictCatalog(page);
  await delayTheConsolesRead(page, CONSOLE_LATENCY_MS);

  await openApps(page);

  // The graceful degradation the inversion was hiding. "Couldn't check" and
  // "here is a starter list, and here is why" are different things to tell an
  // operator, and only one of them is true here.
  await expect(providerGrid(page).getByText("could not be fetched")).toBeVisible({
    timeout: 60_000,
  });
  await expect(page.getByTestId(PROBE_FAILED)).toHaveCount(0);
});

test("a host that never answers at all still gets the honest warning", async ({ page }) => {
  test.setTimeout(120_000);
  await setCatalogDelay(page, 0);
  await setTokenAndEvictCatalog(page);
  // Accepted and never answered — the state the console's own deadline exists
  // for. Widening that deadline must not have removed the warning, only moved
  // it off the path a healthy host takes. A handler returning a promise that
  // never settles is what holds the request pending; one that merely returns
  // lets Playwright continue it.
  await page.route(isComposioStatus, () => new Promise<never>(() => {}));

  await openApps(page);

  await expect(page.getByTestId(PROBE_FAILED)).toBeVisible({ timeout: 60_000 });
});

// Still skipped, but no longer for the reason it was.
//
// It was skipped because the control it drives had gone: it fills the
// managed-route token field, and with `COMPOSIO_MANAGED_HIDDEN` set the managed
// route was filtered out of the picker entirely, so the field never rendered
// and this failed as a `locator.fill` timeout rather than as a regression in
// the re-read.
//
// That flag is now off and the managed route is a row again, so the field is
// reachable — behind the row's own "Replace token" control rather than a card
// that rendered on its own. **The body below has been retargeted to that
// path**, but it has NOT been run: un-skipping needs a pass of the Console E2E
// lane against a live host, which the change that retargeted it did not have.
// Whoever runs that lane next should drop the `.skip` and find out.
//
// Every other case in this file sets the token through the API; only this one
// needs the UI, because a *rotation through the form* is what it is about.
test.skip("rotating the credential re-reads the catalog instead of warning about it", async ({
  page,
}) => {
  test.setTimeout(120_000);
  await setCatalogDelay(page, 0);
  await setTokenAndEvictCatalog(page);

  await openApps(page);
  const grid = providerGrid(page);
  await expect(grid.getByText("Gmail", { exact: true })).toBeVisible({ timeout: 60_000 });
  // Nothing is showing it yet, which is what makes its arrival below evidence
  // of a re-read rather than of a page that never changed.
  await expect(grid.getByText(UNCONNECTED.name, { exact: true })).toHaveCount(0);

  // A rotated token can resolve a different Composio account, so the catalog
  // moves with the credential — that is why the host evicts its cached one on
  // every token write, and it is the only way this spec can tell a re-read from
  // a page that simply kept what it had.
  await setCatalog(page, [UNCONNECTED]);

  // A rotation re-probes by tearing the in-flight read down and starting
  // another. That teardown now cancels the request rather than merely ignoring
  // its answer, and a cancellation this page caused says nothing about the
  // host — so it must leave no warning where a fresh catalog belongs.
  const reread = page.waitForResponse(
    (response) => isComposioStatus(new URL(response.url())) && response.status() === 200,
  );
  // The form is opened by the row that owns the credential, rather than
  // rendering unasked — "do not show a control that cannot act" applies to a
  // password field as much as to a button.
  await page.getByTestId("composio-row-managed-replace").click();
  const field = page.getByLabel(/Composio token/);
  await field.fill(`${TOKEN}-rotated`);
  // "Rotate", not "Save": the row knows a token is already stored, and the
  // button says which of the two things it is doing.
  await page.getByTestId("composio-form-save").click();
  // The dialog closes only on a write the host accepted — a refusal keeps it
  // open with the message inside it, next to the field — so this separates
  // "the rotation happened" from "the click did nothing".
  //
  // Asserting the FIELD is empty cannot do that job now. It used to, while the
  // form was a card on the page that stayed mounted; the credential opens in a
  // modal instead, so on a successful write the input unmounts with it and an
  // assertion on an element that is no longer there fails rather than passes.
  await expect(page.getByTestId("composio-form-dialog")).toHaveCount(0, {
    timeout: 30_000,
  });
  await reread;

  // The provider this company has not connected, so it can only have reached
  // the grid through a catalog read AFTER the rotation.
  await expect(grid.getByText(UNCONNECTED.name, { exact: true })).toBeVisible({ timeout: 60_000 });
  await expect(page.getByTestId(PROBE_FAILED)).toHaveCount(0);
  await expect(page.getByText(GRANT_UNKNOWN)).toHaveCount(0);
});

/**
 * Put the company back as this file found it: no Composio token, and a fixture
 * that answers immediately and publishes what it seeds with.
 *
 * The suite runs serially against one host, and this file sorts among the other
 * `composio-*` specs and before `connections-*`. A token left set hands those a
 * credentialled company; a delay left set makes their catalog read take twenty
 * seconds and fails them for this file's reasons.
 */
test.afterAll(async ({ playwright }, testInfo) => {
  const request = await playwright.request.newContext({
    baseURL: testInfo.project.use.baseURL,
    storageState: testInfo.project.use.storageState as string | undefined,
  });
  try {
    const undelayed = await request.post(`${COMPOSIO_FIXTURE_URL}/__delay`, {
      data: { toolkitsMs: 0 },
    });
    expect(
      undelayed.ok(),
      `clearing the composio fixture delay failed: ${undelayed.status()}`,
    ).toBeTruthy();
    // Back to the seed catalog: an empty body restores it, and the specs
    // running after this file expect gmail and slack to be what is published.
    const reseeded = await request.post(`${COMPOSIO_FIXTURE_URL}/__catalog`, { data: {} });
    expect(
      reseeded.ok(),
      `restoring the composio fixture catalog failed: ${reseeded.status()}`,
    ).toBeTruthy();
    const cleared = await request.put("/api/v1/company/composio/token", { data: { token: "" } });
    expect(
      cleared.ok(),
      `clearing the composio token failed: ${cleared.status()} ${await cleared.text()}`,
    ).toBeTruthy();
  } finally {
    await request.dispose();
  }
});
