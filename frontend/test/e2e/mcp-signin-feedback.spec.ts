import { expect, test, type Page, type Route } from "@playwright/test";

/**
 * Browser sign-in for an MCP server tells the operator when it worked.
 *
 * The host's server list, sign-in start and probe are faked so the flow runs on
 * any host build; the console bundle, session and timers are real. The OAuth
 * callback is never reached: the probe flipping to `ok` stands in for it, which
 * is exactly the only signal the console has.
 */

const path = (suffix: RegExp) => (url: URL) => suffix.test(url.pathname);

function server(status: "needs_config" | "ok", toolCount = 0) {
  return {
    name: "notion",
    endpoint: "https://mcp.notion.com/mcp",
    source: "runtime",
    enabled: true,
    allowedTools: [],
    disallowedTools: [],
    readOnlyTools: [],
    timeoutSecs: 30,
    authConfigured: status === "ok",
    reachableBy: [],
    health: {
      status,
      authHint: status === "ok" ? undefined : "oauth_required",
      message: status === "ok" ? "" : "needs a browser sign-in",
      toolCount,
      checkedAtMillis: Date.now(),
    },
  };
}

async function json(route: Route, body: unknown, status = 200) {
  await route.fulfill({ status, contentType: "application/json", body: JSON.stringify(body) });
}

async function fakeHost(page: Page, probes: Array<"needs_config" | "ok">) {
  let connected = false;
  await page.route(path(/\/mcp\/servers$/), (route) =>
    json(route, [server(connected ? "ok" : "needs_config", connected ? 3 : 0)]),
  );
  await page.route(path(/\/mcp\/servers\/notion\/oauth\/start$/), (route) =>
    json(route, { authorizeUrl: "https://auth.example.test/authorize?state=e2e" }),
  );
  await page.route(path(/\/mcp\/servers\/notion\/test$/), (route) => {
    const next = probes.length > 1 ? probes.shift()! : probes[0];
    if (next === "ok") connected = true;
    return json(route, server(next, next === "ok" ? 3 : 0).health);
  });
  await page.context().route(/auth\.example\.test/, (route) =>
    route.fulfill({ contentType: "text/html", body: "<p>authorize</p>" }),
  );
}

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    const real = Storage.prototype.getItem;
    Storage.prototype.getItem = function getItem(key: string) {
      return key.startsWith("oc-tour:") ? '{"skipped":true}' : real.call(this, key);
    };
  });
});

test("a finished sign-in raises a toast and a connected dialog", async ({ page }) => {
  await fakeHost(page, ["needs_config", "needs_config", "ok"]);
  await page.goto("/#/connections/mcp?view=yours");

  await page.getByTestId("mcp-sign-in").first().click();
  await expect(page.getByTestId("mcp-signin-flight")).toBeVisible();
  await expect(page.getByTestId("mcp-signin-checked")).toContainText(/checked \d+s ago/, {
    timeout: 10_000,
  });

  await expect(page.getByText("Connected to notion · 3 tools")).toBeVisible({ timeout: 15_000 });
  await expect(page.getByTestId("mcp-signin-flight")).toHaveCount(0);
  await expect(page.getByTestId("mcp-connect-done")).toContainText("Connected · 3 tools");
});

test("returning to the tab checks at once", async ({ page }) => {
  await fakeHost(page, ["ok"]);
  await page.clock.install();
  await page.goto("/#/connections/mcp?view=yours");

  await page.getByTestId("mcp-sign-in").first().click();
  await expect(page.getByTestId("mcp-signin-flight")).toBeVisible();

  await page.evaluate(() => window.dispatchEvent(new Event("focus")));
  await expect(page.getByText("Connected to notion · 3 tools")).toBeVisible();
});

test("a sign-in that never finishes times out with Check now and Try again", async ({
  page,
}) => {
  const probes: Array<"needs_config" | "ok"> = ["needs_config"];
  await fakeHost(page, probes);
  await page.clock.install();
  await page.goto("/#/connections/mcp?view=yours");

  await page.getByTestId("mcp-sign-in").first().click();
  await expect(page.getByTestId("mcp-signin-flight")).toBeVisible();

  await page.clock.runFor(5 * 60_000 + 5_000);
  await expect(page.getByTestId("mcp-signin-timed-out")).toBeVisible();
  await expect(page.getByTestId("mcp-signin-retry")).toBeVisible();

  probes.splice(0, probes.length, "ok");
  await page.getByTestId("mcp-signin-check").click();
  await expect(page.getByText("Connected to notion · 3 tools")).toBeVisible();
});
