import { expect, test, type Page, type Route } from "@playwright/test";

/**
 * "Agents with access" on an MCP server page edits each teammate's own tool
 * list in place. The server list, the capability read's bridge flag and the
 * per-teammate `PATCH` are faked; the page, session and router are real.
 */

const path = (suffix: RegExp) => (url: URL) => suffix.test(url.pathname);

async function json(route: Route, body: unknown, status = 200) {
  await route.fulfill({ status, contentType: "application/json", body: JSON.stringify(body) });
}

function notion(saved: boolean) {
  return {
    name: "notion",
    endpoint: "https://mcp.notion.com/mcp",
    source: "runtime",
    enabled: true,
    allowedTools: [],
    disallowedTools: [],
    readOnlyTools: [],
    timeoutSecs: 30,
    authConfigured: true,
    accessGrant: "mcp:notion",
    reachableBy: saved ? [{ id: "writer", name: "Writer" }] : [{ id: "ceo", name: "Chief Executive" }],
    agentAccess: [
      saved
        ? { id: "ceo", name: "Chief Executive", state: "excluded", reaches: false, grantTools: ["search", "mcp:linear", "mcp:notion"] }
        : { id: "ceo", name: "Chief Executive", state: "inherited", reaches: true, revokeTools: ["search", "mcp:linear"] },
      saved
        ? { id: "writer", name: "Writer", state: "included", reaches: true, revokeTools: ["search"] }
        : { id: "writer", name: "Writer", state: "excluded", reaches: false, grantTools: ["search", "mcp:notion"] },
      { id: "hermit", name: "Hermit", state: "blocked", reaches: false },
    ],
    health: { status: "ok", message: "", toolCount: 3, checkedAtMillis: Date.now() },
  };
}

async function fakeHost(page: Page) {
  const patches: Array<{ agent: string; body: unknown }> = [];
  await page.route(path(/\/mcp\/servers$/), (route) => json(route, [notion(patches.length >= 2)]));
  await page.route(path(/\/capabilities$/), async (route) => {
    const real = await route.fetch();
    const body = await real.json();
    await json(route, { ...body, mcpInBuild: true });
  });
  await page.route(path(/\/team\/[^/]+$/), async (route) => {
    if (route.request().method() !== "PATCH") return route.fallback();
    const agent = new URL(route.request().url()).pathname.split("/").pop()!;
    patches.push({ agent, body: route.request().postDataJSON() });
    await json(route, { id: agent });
  });
  return patches;
}

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    const real = Storage.prototype.getItem;
    Storage.prototype.getItem = function getItem(key: string) {
      return key.startsWith("oc-tour:") ? '{"skipped":true}' : real.call(this, key);
    };
  });
});

test("an admin adds and removes teammates without leaving the server page", async ({ page }) => {
  const patches = await fakeHost(page);
  await page.goto("/#/connections/mcp?server=notion");

  const access = page.getByTestId("mcp-agent-access");
  await expect(access).toBeVisible({ timeout: 30_000 });
  await expect(page.getByTestId("mcp-access-agent-toggle-ceo")).toBeChecked();
  await expect(page.getByTestId("mcp-access-agent-toggle-writer")).not.toBeChecked();
  await expect(page.getByTestId("mcp-access-agent-toggle-hermit")).toBeDisabled();
  await expect(page.getByTestId("mcp-access-agent-reason-hermit")).toContainText("mcp:notion");
  await expect(page.getByTestId("mcp-access-tools-link-hermit")).toHaveAttribute(
    "href",
    /hermit\?tab=tools$/,
  );

  await page.getByTestId("mcp-access-agent-toggle-writer").check();
  await page.getByTestId("mcp-access-agent-toggle-ceo").uncheck();
  await expect(page.getByTestId("mcp-agent-access-pin-warning")).toBeVisible();
  await page.getByTestId("mcp-agent-access-save").click();

  await expect(page.getByTestId("mcp-agent-access-saved")).toBeVisible();
  expect(patches).toEqual([
    { agent: "ceo", body: { tools: ["search", "mcp:linear"] } },
    { agent: "writer", body: { tools: ["search", "mcp:notion"] } },
  ]);
  await expect(page.getByTestId("mcp-server-page")).toBeVisible();
  await expect(page).toHaveURL(/server=notion/);
  await expect(page.getByTestId("mcp-access-agent-toggle-writer")).toBeChecked();
  await expect(page.getByTestId("mcp-access-agent-toggle-ceo")).not.toBeChecked();
  await expect(page.getByTestId("mcp-page-edit-agents")).toHaveCount(0);
});
