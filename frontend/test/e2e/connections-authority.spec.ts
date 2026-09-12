import { expect, test } from "@playwright/test";

import { COMPOSIO, COMPOSIO_REASON } from "./capabilities";

/**
 * Issue #403 — the connection pages must not offer a member controls the host
 * refuses.
 *
 * Since the Connections split there are three of them, now across two sections:
 * Apps (`#/connections/apps`), MCP (`#/connections/mcp`) and Inference
 * (`#/settings/inference`). Each carries
 * its own read-only banner and its own credential fields, so each is driven
 * here — a split that left one page still inviting a member to paste a token
 * would be exactly the regression this spec is about.
 *
 * The host is the boundary: every write on this page answers `403` for a
 * member whatever the console renders. What this spec covers is the other
 * half — that the console does not *invite* the refusal. On this page that
 * matters more than usual, because the invitation is a password field: an
 * operator would learn they were not allowed only after pasting a live
 * credential into a form that could never submit it.
 *
 * Drives a real browser against a live host, like the rest of this directory,
 * and is not a merge gate (the Playwright config declares no `webServer`).
 *
 * The suite's shared storage state is the harness **admin**. The member half
 * signs in separately through the same magic-link flow the product uses, in
 * its own browser context, so both roles are exercised against one host.
 */

type Page = import("@playwright/test").Page;
type APIRequestContext = import("@playwright/test").APIRequestContext;

const MEMBER_EMAIL = "member-403@example.test";

// The first-run tour offers itself once per browser context and then records
// `skipped` in that context's localStorage. These tests visit three settings
// pages within one context, so waiting for the dismiss button on every visit
// burns a full `waitFor` timeout (10s) on the pages after the first, where the
// tour can no longer appear — three such waits already blow the 60s test
// budget. Key the dismissal by page object (one page per context here) so only
// the first visit in each context waits on it.
const tourDismissed = new WeakMap<Page, boolean>();

async function openSettingsPage(page: Page, sub: string) {
  await page.goto(`/#/settings/${sub}`);
  if (tourDismissed.has(page)) return;
  const skip = page.getByRole("button", { name: "Skip for now" });
  await skip
    .waitFor({ state: "visible", timeout: 10_000 })
    .then(() => skip.click())
    .catch(() => {
      /* already seen in this context — nothing to dismiss */
    });
  tourDismissed.set(page, true);
}

/**
 * The same, for a page under `#/connections` that no settings address rewrites
 * onto. Composio is one since issue #2259 — it was a tab of Apps, and the
 * `#/settings/*` rewrites predate it, so there is no legacy spelling to reuse.
 */
async function openConnectionsPage(page: Page, sub: string) {
  await page.goto(`/#/connections/${sub}`);
  if (tourDismissed.has(page)) return;
  const skip = page.getByRole("button", { name: "Skip for now" });
  await skip
    .waitFor({ state: "visible", timeout: 10_000 })
    .then(() => skip.click())
    .catch(() => {
      /* already seen in this context — nothing to dismiss */
    });
  tourDismissed.set(page, true);
}

/**
 * Invites `MEMBER_EMAIL` as a member and redeems a login code for `context`.
 *
 * Idempotent on the invite: a re-run hits `409 already a member`, which is a
 * success for our purposes — the address can sign in either way.
 */
async function signInAsMember(admin: APIRequestContext, context: APIRequestContext) {
  const invited = await admin.post("/api/v1/company/users/invites", {
    data: { email: MEMBER_EMAIL, role: "member" },
  });
  expect(
    invited.ok() || invited.status() === 409,
    `inviting ${MEMBER_EMAIL} failed: ${invited.status()} ${await invited.text()}`,
  ).toBeTruthy();

  const requested = await context.post("/api/v1/company/auth/request", {
    data: { email: MEMBER_EMAIL },
  });
  const devCode = (await requested.json())?.dev_code as string | undefined;
  expect(
    devCode,
    "no dev_code came back — the host must bind loopback with no mail transport " +
      "configured for the member half of this spec to sign in",
  ).toBeTruthy();

  const verified = await context.post("/api/v1/company/auth/verify", {
    data: { code: devCode },
  });
  expect(verified.ok(), `member sign-in failed: ${await verified.text()}`).toBeTruthy();
  expect((await verified.json()).role).toBe("member");
}

test("a member sees what is connected but is offered nothing that changes it", async ({
  page,
  browser,
}) => {
  // Establish the member session in its own context, using the suite's admin
  // session (this `page`) only to issue the invite.
  const memberContext = await browser.newContext({ storageState: undefined });
  try {
    await signInAsMember(page.request, memberContext.request);
    const memberPage = await memberContext.newPage();

    // ---- Apps: the third-party accounts the company acts through -----------
    await openSettingsPage(memberPage, "oauth");

    // The page says why, in the operator's language.
    await expect(memberPage.getByTestId("connections-read-only")).toBeVisible({ timeout: 30_000 });

    // Nor the controls behind the other refused writes. `exact` matters here:
    // role-name matching is substring by default, and the Settings rail carries
    // a "Who can sign in, and as what" item that a loose "Sign in" matches —
    // which would make this assertion fail for an admin too, and so prove
    // nothing about either role.
    const button = (name: string) => memberPage.getByRole("button", { name, exact: true });
    await expect(button("Sign in")).toHaveCount(0);
    await expect(button("Add")).toHaveCount(0);

    // But the read is intact — a member can still see what the company is
    // wired to, which is what explains why an agent can reach a provider.
    await expect(memberPage.getByRole("heading", { name: "Apps" })).toBeVisible();
    const status = await memberPage.request.get("/api/v1/company/composio");
    expect(status.ok()).toBeTruthy();
    expect(await status.text()).not.toContain("token");

    // ---- Composio: the key the whole catalog runs on -----------------------
    //
    // GATED on `COMPOSIO`, and that gate is a fix rather than a convenience.
    // `ComposioSection` renders NOTHING when the host was built without the
    // feature: `get_status` answers `inBuild: false` and the section returns
    // `null`. The binary this lane downloads is `--features openhuman,mcp`, so
    // there is no credential surface here for a member OR an admin — no rows,
    // no field, no controls.
    //
    // Both halves of this spec were therefore wrong on this lane, in opposite
    // directions: the member assertion passed for a reason that has nothing to
    // do with authority, and the admin assertion FAILED outright. That second
    // one is `connections-authority.spec.ts` failing on `main` today, on this
    // exact line. A lane that cannot render the surface cannot make a statement
    // about who may use it.
    //
    // `Console E2E (live brain)` runs a `--features composio` host with
    // `PW_COMPOSIO=1`, and that is where this half is exercised.
    //
    // What it asserts there: the WRITE CONTROLS, not the field. The credential
    // opens in a modal now, so "the field is not on the page" is true of an
    // admin too. `ComposioRowList` renders Add / Replace / Remove / Test only
    // for a viewer who may manage, and they are the only things that open the
    // form — so they are what separates the two roles.
    //
    // All of them, on BOTH rows, rather than the own-account pair alone. Which
    // controls a row offers depends on the route the company is on — a company
    // on the managed route offers the own-account row no Add at all — so
    // asserting only that pair is absent would pass for an ADMIN and read as
    // coverage. The set below is empty for a member on every route.
    //
    // The radio (`composio-row-*-select`) is deliberately NOT asserted absent:
    // a member sees which account the company is on, disabled, and that is what
    // tells them why their agents can reach Gmail.
    if (COMPOSIO) {
      await openConnectionsPage(memberPage, "composio");
      await expect(memberPage.getByTestId("connections-read-only")).toBeVisible({
        timeout: 30_000,
      });
      await expect(memberPage.getByTestId("composio-rows")).toBeVisible({ timeout: 30_000 });
      for (const control of [
        "composio-row-managed-add",
        "composio-row-managed-replace",
        "composio-row-managed-remove",
        "composio-row-byok-add",
        "composio-row-byok-replace",
        "composio-row-byok-test",
      ]) {
        await expect(memberPage.getByTestId(control), `a member is offered ${control}`).toHaveCount(
          0,
        );
      }
      await expect(memberPage.locator("#composio-api-key")).toHaveCount(0);
      await expect(button("Save key")).toHaveCount(0);
    } else {
      test.info().annotations.push({ type: "skipped-half", description: COMPOSIO_REASON });
    }

    // ---- MCP: the tool servers, rows and the file both ---------------------
    await openSettingsPage(memberPage, "mcp");
    await expect(memberPage.getByTestId("mcp-read-only")).toBeVisible({ timeout: 30_000 });
    await expect(memberPage.locator("#mcp-name")).toHaveCount(0);
    await expect(memberPage.locator("#mcp-token")).toHaveCount(0);
    // The document is the other way to write the same store, so it must refuse
    // a member too — read-only, and with no Save to press.
    await memberPage.getByTestId("mcp-tab-json").click();
    await expect(memberPage.getByTestId("mcp-json-text")).toHaveAttribute("readonly", "");
    await expect(memberPage.getByTestId("mcp-json-save")).toHaveCount(0);

    // ---- Inference: what every teammate's turn costs -----------------------
    await openSettingsPage(memberPage, "inference");
    await expect(memberPage.getByTestId("inference-save")).toHaveCount(0);
  } finally {
    await memberContext.close();
  }
});

test("an admin is still offered every control across the four pages", async ({ page }) => {
  await openSettingsPage(page, "oauth");
  // The member's banner is absent, and the control it was refused is present.
  //
  // This used to assert `#company-credential`, on the reasoning that the
  // company-credential key is the Apps page's write surface on every build.
  // `src/product-scope.ts` hides the OpenHuman-managed Composio route and
  // `OAuthView` hides that card with it — deliberately, since a company
  // reaching Composio through its own account has nothing to spend that key on.
  // It is no longer a surface on any build, so it cannot be the invariant.
  //
  // What survives is the provider grid's own "Sign in", which is exactly the
  // control the member case above asserts a member does NOT get. Presence, not
  // enabledness: with no credential there is nothing to authorize against, so it
  // renders disabled on a host with no Composio compiled in — which is this
  // lane. Asserting it is enabled would pass only on the gated build and turn
  // this into a second Composio test.
  await expect(page.getByTestId("connections-read-only")).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Sign in", exact: true })).toHaveCount(1, {
    timeout: 30_000,
  });

  // The credential the member above is refused, on the page it lives on. This
  // is what stops that assertion going vacuous: if the controls ever stop
  // rendering here, this fails rather than the member case quietly passing.
  //
  // Gated on `COMPOSIO` for the same reason the member half is — on a host
  // built without the feature `ComposioSection` renders nothing at all, and
  // this assertion is the one that fails on `main` today because of it.
  //
  // It takes a click to reach the field: the credential form is a modal opened
  // from the row that owns the credential. Which control opens it depends on
  // the route the company is on, so this asks for either rather than pinning
  // itself to a route it is not about — but NOT as one `.first()` over all
  // three testids. The own-account row renders its radio whenever it is active,
  // the radio comes first in the DOM, and `ComposioRowList` makes a click on an
  // already-checked radio a deliberate no-op, so `.first()` would silently pick
  // an inert control on a company already on BYOK.
  if (COMPOSIO) {
    await openConnectionsPage(page, "composio");
    await expect(page.getByTestId("connections-read-only")).toHaveCount(0);
    await expect(page.getByTestId("composio-rows")).toBeVisible({ timeout: 30_000 });
    const writeControl = page.locator(
      '[data-testid="composio-row-byok-add"], [data-testid="composio-row-byok-replace"]',
    );
    // "Use this" on a row that is not already the active one. That is the
    // hand-off the own-account route is chosen through: it cannot be selected
    // without the key that makes it resolve, so it opens the field instead of
    // writing (`ComposioSection`'s `onSelect`).
    const handOff = page.locator('[data-testid="composio-row-byok-select"][aria-checked="false"]');
    const opener = (await writeControl.count()) > 0 ? writeControl.first() : handOff.first();
    await expect(opener).toBeVisible({ timeout: 30_000 });
    await opener.click();
    await expect(page.getByTestId("composio-form-dialog")).toBeVisible({ timeout: 30_000 });
    await expect(page.locator("#composio-api-key")).toBeVisible({ timeout: 30_000 });
  } else {
    test.info().annotations.push({ type: "skipped-half", description: COMPOSIO_REASON });
  }

  await openSettingsPage(page, "mcp");
  await expect(page.getByTestId("mcp-read-only")).toHaveCount(0);
  await expect(page.locator("#mcp-name")).toBeVisible({ timeout: 30_000 });
  await page.getByTestId("mcp-tab-json").click();
  await expect(page.getByTestId("mcp-json-revert")).toBeVisible();

  await openSettingsPage(page, "inference");
  await expect(page.getByTestId("inference-save")).toBeVisible({ timeout: 30_000 });
});
