import { expect, test } from "@playwright/test";

/**
 * The Settings pages must not offer a member controls the host refuses.
 *
 * `connections-authority.spec.ts` is this spec's older half, and covers the
 * three pages that existed when issue #403 was fixed: Apps, MCP and Inference.
 * Hosting, Search, and the Approvals and Domain cards on General arrived later
 * and never joined it — which is why each of them shipped rendering a member an
 * enabled Save. A page that gains a credential form after the spec was written
 * does not fail it; it is simply not in it. So this file walks the Settings
 * section by page, and is the thing to extend when a page is added.
 *
 * The host is the boundary either way: `PUT …/hosting`, `PUT …/search`,
 * `PUT …/domain` and the SMTP writes are `AdminScopedCompany`, and
 * `PUT …/policy` calls `require_admin`, so each answers a member
 * `403 only an admin can do that` whatever the console renders. What this spec
 * covers is that the console does not *invite* the refusal — which matters most
 * where the invitation is a password box, because a member learns they are not
 * allowed only after a live credential has left their password manager.
 *
 * Lifecycle is deliberately not asserted here. `POST …/{id}/pause` takes
 * `CompanyAuth` and never resolves a role, so a member's Pause genuinely stops
 * the company; asserting the button away would encode a guard that does not
 * exist. When the host grows one, add the case here alongside it.
 *
 * Drives a real browser against a live host, like the rest of this directory,
 * and is not a merge gate (the Playwright config declares no `webServer`).
 *
 * The suite's shared storage state is the harness **admin**. The member half
 * signs in separately through the same magic-link flow the product uses, in its
 * own browser context, so both roles are exercised against one host.
 *
 * A third principal — the platform bearer (`?token=` / `VITE_OC_TOKEN`) with no
 * human session behind it — is covered at the API layer by the test below
 * rather than through the browser. `AdminScopedCompany` (`scope.rs`) admits
 * that principal directly for exactly the mutations this page and Search gate,
 * so `useCanManage` has to grant it the same controls a member never sees; but
 * driving it through an actual page load hits an unrelated, pre-existing gap —
 * `client.onUnauthorized` (`api/client.ts`) flips the *whole* connection to
 * "unauthenticated" on the first 401 from any non-`/auth/` route, and several
 * app-shell routes a bearer has no session to answer for (`/presence`,
 * `/notifications`, `/chat/mentionables`) 401 within the first second of any
 * page load — bouncing the console back to the login screen out from under
 * whatever page was open, Settings included. That is a separate defect in the
 * connection registry, not in `useCanManage`, and fixing it is a larger change
 * than this file's scope; the case below asserts the same ground truth
 * `useCanManage` reads without depending on a page surviving that race.
 */

type Page = import("@playwright/test").Page;
type APIRequestContext = import("@playwright/test").APIRequestContext;

const MEMBER_EMAIL = "member-settings@example.test";

// The first-run tour offers itself once per browser context and then records
// `skipped` in that context's localStorage. These tests visit four settings
// pages within one context, so waiting for the dismiss button on every visit
// burns a full `waitFor` timeout on the pages after the first, where the tour
// can no longer appear. Key the dismissal by page object so only the first
// visit in each context waits on it.
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

test("a member sees what Settings holds but is offered nothing that changes it", async ({
  page,
  browser,
}) => {
  const memberContext = await browser.newContext({ storageState: undefined });
  try {
    await signInAsMember(page.request, memberContext.request);
    const memberPage = await memberContext.newPage();

    // ---- Hosting: where this company's sites go live ------------------------
    await openSettingsPage(memberPage, "hosting");
    await expect(memberPage.getByTestId("hosting-read-only")).toBeVisible({ timeout: 30_000 });

    // The assertion that matters most: no credential field anywhere. A member
    // must never be handed somewhere to paste a deployment token.
    await expect(memberPage.getByTestId("hosting-api-key")).toHaveCount(0);
    await expect(memberPage.getByTestId("hosting-save")).toHaveCount(0);
    await expect(memberPage.getByTestId("hosting-clear")).toHaveCount(0);

    // The read survives — what the company deploys through explains why a
    // teammate can deploy at all.
    await expect(memberPage.getByRole("heading", { name: "Hosting" })).toBeVisible();

    // ---- Search: whose index answers a teammate, under whose retention ------
    await openSettingsPage(memberPage, "search");
    await expect(memberPage.getByTestId("search-read-only")).toBeVisible({ timeout: 30_000 });
    await expect(memberPage.getByTestId("search-api-key")).toHaveCount(0);
    await expect(memberPage.getByTestId("search-save")).toHaveCount(0);
    await expect(memberPage.getByTestId("search-clear")).toHaveCount(0);

    // The provider stays visible and inert. This page's own footnote says the
    // choice is an administrator's; it used to print that under a live picker.
    const provider = memberPage.getByTestId("search-provider");
    await expect(provider).toBeVisible();
    await expect(provider).toBeDisabled();

    // ---- General: approvals, domain, outbound mail --------------------------
    await openSettingsPage(memberPage, "general");
    await expect(memberPage.getByTestId("policy-read-only")).toBeVisible({ timeout: 30_000 });
    // The tiers stay readable — which one is in force decides what this
    // member's teammates may do without asking — but none of them is a choice.
    await expect(memberPage.getByTestId("policy-tier-full")).toBeDisabled();

    await expect(memberPage.getByTestId("domain-read-only")).toBeVisible();
    await expect(memberPage.getByTestId("domain-remove")).toHaveCount(0);
    await expect(memberPage.getByTestId("domain-input")).toHaveCount(0);

    await expect(memberPage.getByTestId("smtp-read-only")).toBeVisible();
    await expect(memberPage.getByTestId("smtp-save")).toHaveCount(0);
    await expect(memberPage.getByTestId("smtp-password")).toHaveCount(0);

    // ---- Usage: read-only for everyone, and unchanged by any of this --------
    // `GET …/usage` is `ScopedCompany`, and the page carries no admin control
    // at all, so a member gets the whole thing. Asserted rather than assumed:
    // the risk when adding role gates is over-correcting into a member losing
    // a page that was always theirs.
    await openSettingsPage(memberPage, "usage");
    await expect(memberPage.getByRole("heading", { name: "Usage" })).toBeVisible({
      timeout: 30_000,
    });
    await expect(memberPage.getByRole("combobox", { name: "Usage date range" })).toBeEnabled();
  } finally {
    await memberContext.close();
  }
});

test("an admin is still offered every Settings control", async ({ page }) => {
  // The control for the spec above: each assertion there is only worth having
  // if the admin path still renders what the member's does not.
  await openSettingsPage(page, "hosting");
  await expect(page.getByTestId("hosting-read-only")).toHaveCount(0);
  await expect(page.getByTestId("hosting-api-key")).toBeVisible({ timeout: 30_000 });
  await expect(page.getByTestId("hosting-save")).toBeVisible();

  await openSettingsPage(page, "search");
  await expect(page.getByTestId("search-read-only")).toHaveCount(0);
  await expect(page.getByTestId("search-provider")).toBeEnabled({ timeout: 30_000 });
  await expect(page.getByTestId("search-save")).toBeVisible();

  await openSettingsPage(page, "general");
  await expect(page.getByTestId("policy-read-only")).toHaveCount(0);
  await expect(page.getByTestId("policy-tier-full")).toBeEnabled({ timeout: 30_000 });
  await expect(page.getByTestId("domain-read-only")).toHaveCount(0);
  await expect(page.getByTestId("smtp-read-only")).toHaveCount(0);
  await expect(page.getByTestId("smtp-save")).toBeVisible();
});

/**
 * `OPENCOMPANY_PLATFORM_TOKEN` this host was started with, if any — the
 * shared-secret credential `AdminScopedCompany` admits directly, with no
 * session behind it. Not configured by default: `host.sh` starts from an empty
 * environment and only forwards names listed in `PW_HOST_PASSTHROUGH` (see its
 * own header comment), so exercising this case needs both set on the run:
 *
 *   OPENCOMPANY_PLATFORM_TOKEN=<secret> PW_HOST_PASSTHROUGH=OPENCOMPANY_PLATFORM_TOKEN npm run e2e
 *
 * Skipped rather than failed when absent — the same way the suite treats every
 * fixture it does not manage (`COMPOSIO`, `LIVE_BRAIN`). A run against a host
 * nobody configured a bearer for has nothing to prove wrong.
 */
const PLATFORM_TOKEN = process.env.OPENCOMPANY_PLATFORM_TOKEN;

test("a platform bearer with no human session gets what AdminScopedCompany already grants it", async ({
  browser,
}) => {
  test.skip(
    !PLATFORM_TOKEN,
    "needs OPENCOMPANY_PLATFORM_TOKEN forwarded via PW_HOST_PASSTHROUGH — see the comment above.",
  );
  const bearerContext = await browser.newContext({ storageState: undefined });
  try {
    const bearer = bearerContext.request;
    const headers = { authorization: `Bearer ${PLATFORM_TOKEN}` };

    // The premise `useCanManage` now has to recognise: no session behind this
    // bearer at all.
    const me = await bearer.get("/api/v1/company/auth/me", { headers });
    expect(me.status()).toBe(401);

    // Proves the same authority `useCanManage` grants without writing a real
    // credential: `AdminScopedCompany` (`scope.rs`) runs before the handler
    // body, so an unsupported provider — refused by `put_hosting` itself,
    // before it ever calls `write_all` — only reaches that refusal if the
    // bearer already cleared the authority check. A principal `useCanManage`
    // should have refused would 401/403 here instead, never 400. `PW_BASE_URL`
    // can point this run at a real company, so nothing here may touch whatever
    // hosting credential it actually has stored.
    const probe = await bearer.put("/api/v1/company/hosting", {
      headers,
      data: { provider: "not-a-real-provider" },
    });
    expect(probe.status()).toBe(400);
    expect((await probe.json()).code).toBe("invalid_request");
  } finally {
    await bearerContext.close();
  }
});
