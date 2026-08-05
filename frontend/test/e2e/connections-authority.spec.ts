import { expect, test } from "@playwright/test";

/**
 * Issue #403 — the Connections page must not offer a member controls the host
 * refuses.
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

async function openConnections(page: Page) {
  await page.goto("/#/settings/connections");
  const skip = page.getByRole("button", { name: "Skip for now" });
  await skip
    .waitFor({ state: "visible", timeout: 10_000 })
    .then(() => skip.click())
    .catch(() => {
      /* already seen in this context — nothing to dismiss */
    });
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
    await openConnections(memberPage);

    // The page says why, in the operator's language.
    await expect(memberPage.getByTestId("connections-read-only")).toBeVisible({ timeout: 30_000 });

    // No credential field anywhere on the page. This is the assertion that
    // matters most: a member must never be handed somewhere to paste a token.
    await expect(memberPage.locator("#composio-token")).toHaveCount(0);
    await expect(memberPage.locator("#tg-token")).toHaveCount(0);
    await expect(memberPage.locator("#mcp-token")).toHaveCount(0);

    // Nor the controls behind the other refused writes. `exact` matters on the
    // last two: role-name matching is substring by default, and the Settings
    // rail carries a "Who can sign in, and as what" item that a loose "Sign in"
    // matches — which would make this assertion fail for an admin too, and so
    // prove nothing about either role.
    await expect(memberPage.getByTestId("inference-save")).toHaveCount(0);
    const button = (name: string) => memberPage.getByRole("button", { name, exact: true });
    await expect(button("Save token")).toHaveCount(0);
    await expect(button("Sign in")).toHaveCount(0);
    await expect(button("Add")).toHaveCount(0);

    // But the read is intact — a member can still see what the company is
    // wired to, which is what explains why an agent can reach a provider.
    await expect(memberPage.getByRole("heading", { name: "Connections" })).toBeVisible();
    const status = await memberPage.request.get("/api/v1/company/composio");
    expect(status.ok()).toBeTruthy();
    expect(await status.text()).not.toContain("token");
  } finally {
    await memberContext.close();
  }
});

test("an admin is still offered every control on the same page", async ({ page }) => {
  await openConnections(page);

  // The member's banner is absent, and the credential surfaces are present.
  await expect(page.getByTestId("connections-read-only")).toHaveCount(0);
  await expect(page.getByTestId("inference-save")).toBeVisible({ timeout: 30_000 });
  await expect(page.locator("#tg-token")).toBeVisible();
  await expect(page.locator("#mcp-name")).toBeVisible();
});
