import { expect, test } from "@playwright/test";

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

    // No credential field anywhere on the page. This is the assertion that
    // matters most: a member must never be handed somewhere to paste a token.
    await expect(memberPage.locator("#composio-token")).toHaveCount(0);

    // Nor the controls behind the other refused writes. `exact` matters here:
    // role-name matching is substring by default, and the Settings rail carries
    // a "Who can sign in, and as what" item that a loose "Sign in" matches —
    // which would make this assertion fail for an admin too, and so prove
    // nothing about either role.
    const button = (name: string) => memberPage.getByRole("button", { name, exact: true });
    await expect(button("Save token")).toHaveCount(0);
    await expect(button("Sign in")).toHaveCount(0);
    await expect(button("Add")).toHaveCount(0);

    // But the read is intact — a member can still see what the company is
    // wired to, which is what explains why an agent can reach a provider.
    await expect(memberPage.getByRole("heading", { name: "Apps" })).toBeVisible();
    const status = await memberPage.request.get("/api/v1/company/composio");
    expect(status.ok()).toBeTruthy();
    expect(await status.text()).not.toContain("token");

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

test("an admin is still offered every control across the three pages", async ({ page }) => {
  await openSettingsPage(page, "oauth");
  // The member's banner is absent, and an admin-only write surface is present.
  // Company-credential is gone from this page product-wide with the managed
  // Composio route (`COMPOSIO_MANAGED_HIDDEN`, `src/product-scope.ts`) — it is
  // no longer the invariant to assert. The Composio token card is not one
  // either: it only renders when the host reports a credential the admin may
  // override, which a default-feature host never does. What is left on every
  // build is the by-slug connect hatch — gated on `canManage` the same as the
  // "Sign in" button the member half of this spec already proves is absent
  // for a member.
  await expect(page.getByTestId("connections-read-only")).toHaveCount(0);
  await expect(page.locator("#providers-other-toolkit")).toBeVisible({ timeout: 30_000 });

  await openSettingsPage(page, "mcp");
  await expect(page.getByTestId("mcp-read-only")).toHaveCount(0);
  await expect(page.locator("#mcp-name")).toBeVisible({ timeout: 30_000 });
  await page.getByTestId("mcp-tab-json").click();
  await expect(page.getByTestId("mcp-json-revert")).toBeVisible();

  await openSettingsPage(page, "inference");
  await expect(page.getByTestId("inference-save")).toBeVisible({ timeout: 30_000 });
});
