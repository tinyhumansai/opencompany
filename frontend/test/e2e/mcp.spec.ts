import { expect, test } from "@playwright/test";

/**
 * Issue #414 — Settings, MCP Servers must render the servers the host actually
 * serves.
 *
 * The page used to be written against an API no host has ever answered: a
 * `{ servers: [...] }` wrapper where `GET .../mcp/servers` returns a bare
 * array, servers keyed by `server_id` where the host keys by name, plus
 * `/connect` and `/disconnect` routes that exist nowhere. Opening it stored
 * `undefined` as the server list and threw `Cannot read properties of undefined
 * (reading 'length')` on render.
 *
 * That is why this spec asserts on an uncaught page error as well as on the
 * rendering: the failure was a thrown TypeError, and a spec that only checked
 * for visible rows could pass against a page that had already blown up before
 * painting them.
 *
 * Runs against the default-feature host the rest of this directory drives — no
 * extra capability needed. The harness company declares one `[[mcp_server]]`
 * (`deepwiki`) in its manifest, so the list is never empty here, and the add /
 * remove half exercises the runtime source against the routes the host serves.
 * Live probing (`Test`) and tool discovery (`Tools`) need the `openhuman`
 * feature and report `not_wired` on this build, so they are deliberately not
 * driven here.
 *
 * The suite's shared storage state is the harness admin, which is what the add
 * and remove controls require (issue #403).
 */

type Page = import("@playwright/test").Page;

/** The MCP settings page, with the product tour dismissed if it appears. */
async function openMcpSettings(page: Page) {
  await page.goto("/#/settings/mcp");
  const skip = page.getByRole("button", { name: "Skip for now" });
  await skip
    .waitFor({ state: "visible", timeout: 10_000 })
    .then(() => skip.click())
    .catch(() => {
      /* already seen in this context — nothing to dismiss */
    });
}

test("Settings MCP lists the company's servers instead of crashing on open", async ({ page }) => {
  const pageErrors: string[] = [];
  page.on("pageerror", (error) => pageErrors.push(error.message));

  await openMcpSettings(page);

  await expect(page.getByRole("heading", { name: "MCP Servers" })).toBeVisible();

  // The manifest server the harness company declares. Rendering it at all is
  // the fix: the old view read the list off a wrapper key the host never sent.
  const manifest = page.getByTestId("mcp-server-row").filter({ hasText: "deepwiki" });
  await expect(manifest).toBeVisible();
  await expect(manifest).toContainText("manifest");
  await expect(manifest).toContainText("https://mcp.deepwiki.com/mcp");

  // The page must not be one that renders and throws. `.length` of `undefined`
  // was the exact crash; any uncaught error here is a failure regardless.
  expect(pageErrors, `the page threw: ${pageErrors.join(" | ")}`).toEqual([]);
});

test("an admin adds and removes a runtime MCP server", async ({ page }) => {
  const pageErrors: string[] = [];
  page.on("pageerror", (error) => pageErrors.push(error.message));

  await openMcpSettings(page);

  // Unique per run: the host persists runtime servers in its secret store, and
  // a re-run against the same data directory would otherwise collide on name.
  const name = `pw-mcp-${Date.now()}`;
  const endpoint = `https://mcp.example.test/${name}`;

  await page.getByTestId("mcp-add-name").fill(name);
  await page.getByTestId("mcp-add-endpoint").fill(endpoint);
  await page.getByTestId("mcp-add-submit").click();

  const row = page.getByTestId("mcp-server-row").filter({ hasText: name });
  await expect(row).toBeVisible({ timeout: 15_000 });
  await expect(row).toContainText("runtime");
  await expect(row).toContainText(endpoint);

  // The host is the authority on what was stored, so confirm the row is not
  // just optimistic console state.
  const listed = await page.request.get("/api/v1/company/mcp/servers");
  expect(listed.ok()).toBeTruthy();
  const body: unknown = await listed.json();
  expect(Array.isArray(body), "GET .../mcp/servers answers a bare array").toBeTruthy();
  expect((body as { name: string }[]).map((server) => server.name)).toContain(name);

  await row.getByRole("button", { name: `Remove ${name}` }).click();
  await expect(row).toHaveCount(0, { timeout: 15_000 });

  expect(pageErrors, `the page threw: ${pageErrors.join(" | ")}`).toEqual([]);
});
