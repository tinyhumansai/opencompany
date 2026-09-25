import { expect, test } from "@playwright/test";

import { LIVE_BRAIN } from "./capabilities";

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
  await page.goto("/#/connections/mcp");
  const skip = page.getByRole("button", { name: "Skip for now" });
  await skip
    .waitFor({ state: "visible", timeout: 10_000 })
    .then(() => skip.click())
    .catch(() => {
      /* already seen in this context — nothing to dismiss */
    });
}

test("the MCP page lists the company's servers instead of crashing on open", async ({
  page,
}) => {
  const pageErrors: string[] = [];
  page.on("pageerror", (error) => pageErrors.push(error.message));

  await openMcpSettings(page);

  await expect(
    page.getByRole("heading", { name: "MCP Servers", level: 1 }),
  ).toBeVisible();
  await expect(
    page.getByRole("heading", { name: "Installed servers", level: 2 }),
  ).toBeVisible();

  // The manifest server the harness company declares. Rendering it at all is
  // the fix: the old view read the list off a wrapper key the host never sent.
  const manifest = page
    .getByTestId("mcp-server-row")
    .filter({ hasText: "deepwiki" });
  await expect(manifest).toBeVisible();
  await expect(manifest).toContainText("manifest");
  await expect(manifest).toContainText("https://mcp.deepwiki.com/mcp");

  // Issue #567: the screen states what this deployment can do with these
  // servers. Asserted from both sides, because the failure being fixed is a
  // console that reads identically on a host that honours a server and one that
  // never can — the default-feature host behind this lane has no `mcp` bridge
  // and must say so; the live-brain lane compiles it in and must stay quiet.
  const bridgeAbsent = page.getByTestId("mcp-bridge-absent");
  if (LIVE_BRAIN) {
    await expect(bridgeAbsent).toHaveCount(0);
  } else {
    await expect(bridgeAbsent).toBeVisible();
    await expect(bridgeAbsent).toContainText(
      "no agent ever receives their tools",
    );
  }

  // The page must not be one that renders and throws. `.length` of `undefined`
  // was the exact crash; any uncaught error here is a failure regardless.
  expect(pageErrors, `the page threw: ${pageErrors.join(" | ")}`).toEqual([]);
});

test("a server opens into its own page, not a row that grew", async ({
  page,
}) => {
  // What is asserted here is not "a page opened" but the claims the page
  // exists to make, because each has a plausible-looking wrong answer the list
  // surface would have given.
  const pageErrors: string[] = [];
  page.on("pageerror", (error) => pageErrors.push(error.message));

  await openMcpSettings(page);

  await expect(
    page.getByRole("heading", { name: "MCP Servers", level: 1 }),
  ).toBeVisible();

  const row = page
    .getByTestId("mcp-server-row")
    .filter({ hasText: "deepwiki" });
  await expect(row).toBeVisible();
  await row.getByTestId("mcp-server-open").click();

  const panel = page.getByTestId("mcp-server-page");
  await expect(panel).toBeVisible();
  // A page, so the list it came from is gone rather than pushed down.
  await expect(page.getByTestId("mcp-server-row")).toHaveCount(0);
  await expect(panel.getByTestId("mcp-page-back")).toBeVisible();

  await expect(panel).toContainText("https://mcp.deepwiki.com/mcp");

  // The manifest server the harness declares has never been probed on this
  // host: `Test` needs the `openhuman` feature and reports `not_wired` here. So
  // this is the case with no honest single-badge rendering — not reachable, not
  // broken — and the panel has to say which.
  await expect(panel.getByTestId("mcp-page-probe")).toContainText(
    "has not been probed",
  );

  // MCP records no connect, the same answer the native path gets and for the
  // same reason. A blank here reads as "never connected".
  await expect(panel.getByTestId("mcp-page-connected-on")).toContainText(
    "connection date not recorded",
  );

  // What a disconnect reaches — and, for a manifest server, that the console
  // cannot remove it at all.
  const scope = panel.getByTestId("mcp-page-disconnect-scope");
  await expect(scope).toContainText("cannot be removed from the console");
  await expect(scope).toContainText(
    "Nothing is revoked at the server's own end",
  );

  // Usage, read under `mcp:deepwiki`. The harness host serves the usage route
  // and no agent has called this server, so a real zero is the right answer —
  // the case that must NOT be confused with the unavailable one below it.
  await expect(panel.getByTestId("connection-detail-usage")).toContainText(
    "in the last 30 days",
  );

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

  // The Remove click below is what this test is *about*, but it is also the
  // only thing that takes the server back out of the host's secret store — and
  // it is the last step, so any assertion before it that fails leaves the
  // server registered for good. Unique names mean a re-run still passes, which
  // is exactly why the leak would go unnoticed: the store just grows. The
  // `finally` deletes through the API regardless of how the body ended, and is
  // a harmless 404 on the happy path where the UI already removed it.
  try {
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
    expect(
      Array.isArray(body),
      "GET .../mcp/servers answers a bare array",
    ).toBeTruthy();
    expect((body as { name: string }[]).map((server) => server.name)).toContain(
      name,
    );

    await row.getByRole("button", { name: `Remove ${name}` }).click();

    // Removing takes the stored credential with it, so the trash asks first.
    // The row has to survive the question, or the confirmation is decorative.
    const confirm = page.getByRole("alertdialog");
    await expect(confirm).toContainText(`Remove ${name}?`);
    await expect(row).toHaveCount(1);
    await confirm.getByRole("button", { name: "Remove", exact: true }).click();

    await expect(row).toHaveCount(0, { timeout: 15_000 });

    expect(pageErrors, `the page threw: ${pageErrors.join(" | ")}`).toEqual([]);
  } finally {
    // Best-effort: a teardown that throws would replace the real failure with
    // its own, and the thing it is cleaning up is test residue either way.
    await page.request
      .delete(`/api/v1/company/mcp/servers/${encodeURIComponent(name)}`)
      .catch(() => undefined);
  }
});

test("the permissions panel reads a tier as set or unset, and says what is never sent", async ({
  page,
}) => {
  // Issue #2373's console half, driven against the host rather than a mock.
  //
  // Two things here are only true end to end. The tier control renders "Not
  // set" from a `stored` flag the host computes — a console that rendered the
  // tier's nominal mode instead would show "Read-only: Runs" above read-only
  // tools reading "Asks", and choosing the value already on screen would grant
  // a bulk allow nobody asked for. And a tool the deny list keeps from ever
  // being sent still carries a mode, which will never be consulted; the row
  // has to say so or the mode invites an edit with no effect.
  const pageErrors: string[] = [];
  page.on("pageerror", (error) => pageErrors.push(error.message));

  const name = `pw-perm-${Date.now()}`;
  const blocked = "never_sent_tool";

  try {
    const added = await page.request.post("/api/v1/company/mcp/servers", {
      data: {
        name,
        endpoint: `https://mcp.example.test/${name}`,
        disallowedTools: [blocked],
      },
    });
    expect(
      added.ok(),
      "the host accepted the server the panel is about",
    ).toBeTruthy();

    const policyPath = `/api/v1/company/mcp/servers/${encodeURIComponent(name)}/tools/policy`;

    // Nothing has probed this server on a default-feature host, so the only
    // row the panel can list is one an operator decided themselves.
    const seeded = await page.request.put(policyPath, {
      data: { tools: [{ tool: blocked, tier: "read_only" }] },
    });
    expect(seeded.ok(), "the host stored the per-tool decision").toBeTruthy();

    await openMcpSettings(page);
    const row = page.getByTestId("mcp-server-row").filter({ hasText: name });
    await expect(row).toBeVisible({ timeout: 15_000 });
    await row
      .getByRole("button", { name: `Tool permissions for ${name}` })
      .click();

    // On the server's own page, not under the row. A panel that drifted back
    // onto the row would satisfy every other assertion here.
    const detail = page.getByTestId("mcp-server-page");
    await expect(detail).toBeVisible();
    const panel = detail.getByTestId("mcp-tool-permissions");
    await expect(panel).toBeVisible();

    // No tier default was ever written, so every tier reads as unset — not as
    // the mode it would fall back to.
    for (const tier of ["read_only", "interactive", "write_delete"]) {
      await expect(panel.locator(`#tier-${tier}`)).toContainText("Not set");
    }
    await expect(panel.locator("#tier-read_only")).not.toContainText("Allow");

    await expect(
      panel.getByTestId("mcp-permission-row").filter({ hasText: blocked }),
    ).toContainText("Not sent");

    // The round trip the wire shape exists for: a written tier comes back
    // stored, and reads as the mode rather than as unset.
    const wrote = await page.request.put(policyPath, {
      data: { tierDefaults: { read_only: "always_allow" } },
    });
    expect(wrote.ok()).toBeTruthy();
    await page.reload();
    await openMcpSettings(page);
    await page
      .getByTestId("mcp-server-row")
      .filter({ hasText: name })
      .getByRole("button", { name: `Tool permissions for ${name}` })
      .click();
    await expect(
      page.getByTestId("mcp-tool-permissions").locator("#tier-read_only"),
    ).toContainText("Allow");

    // And a tier named as nothing is cleared, which the wire could not say
    // before: an omitted tier means "leave it alone".
    const cleared = await page.request.put(policyPath, {
      data: { tierDefaults: { read_only: null } },
    });
    expect(cleared.ok()).toBeTruthy();
    await page.reload();
    await openMcpSettings(page);
    await page
      .getByTestId("mcp-server-row")
      .filter({ hasText: name })
      .getByRole("button", { name: `Tool permissions for ${name}` })
      .click();
    await expect(
      page.getByTestId("mcp-tool-permissions").locator("#tier-read_only"),
    ).toContainText("Not set");

    expect(pageErrors, `the page threw: ${pageErrors.join(" | ")}`).toEqual([]);
  } finally {
    await page.request
      .delete(`/api/v1/company/mcp/servers/${encodeURIComponent(name)}`)
      .catch(() => undefined);
  }
});

test("mcp.json shows the same servers the rows do, and saves an edit back", async ({
  page,
}) => {
  // The second half of the MCP page: the declared set as one document. What is
  // asserted is the property that makes two surfaces safe — they are one
  // configuration. A document that were an import format could show a server
  // the rows do not, or accept a save the rows never see, and an operator would
  // have no way to tell which one was true.
  const pageErrors: string[] = [];
  page.on("pageerror", (error) => pageErrors.push(error.message));

  await openMcpSettings(page);
  await page.getByTestId("mcp-tab-json").click();

  const editor = page.getByTestId("mcp-json-text");
  await expect(editor).toBeVisible({ timeout: 15_000 });

  // The manifest server the harness company declares is in the file, with its
  // provenance echoed — and no credential, which is the rule this surface
  // cannot be allowed to break.
  const before = (await editor.inputValue()).trim();
  const doc = JSON.parse(before) as {
    mcpServers: Record<
      string,
      { url: string; source?: string; headers?: unknown }
    >;
  };
  expect(Object.keys(doc.mcpServers)).toContain("deepwiki");
  expect(doc.mcpServers.deepwiki.url).toBe("https://mcp.deepwiki.com/mcp");
  expect(doc.mcpServers.deepwiki.source).toBe("manifest");
  expect(
    doc.mcpServers.deepwiki.headers,
    "a read must never echo a credential",
  ).toBeUndefined();

  // An unedited document is not an edit: Save stays disabled, so an operator
  // who opens the tab and leaves cannot write an override for every declared
  // server by pressing the only button on screen.
  await expect(page.getByTestId("mcp-json-save")).toBeDisabled();

  const name = `pw-json-${Date.now()}`;
  try {
    // Adding a server here must reach the same store the rows read.
    doc.mcpServers[name] = { url: `https://mcp.example.test/${name}` };
    await editor.fill(`${JSON.stringify(doc, null, 2)}\n`);
    await page.getByTestId("mcp-json-save").click();

    // The host is the authority on what was stored — not the editor's own text.
    await expect
      .poll(
        async () => {
          const listed = await page.request.get("/api/v1/company/mcp/servers");
          if (!listed.ok()) return [];
          const body = (await listed.json()) as { name: string }[];
          return Array.isArray(body) ? body.map((server) => server.name) : [];
        },
        { timeout: 15_000 },
      )
      .toContain(name);

    // And the rows show it, because both tabs read one configuration.
    await page.getByTestId("mcp-tab-connections").click();
    await expect(
      page.getByTestId("mcp-server-row").filter({ hasText: name }),
    ).toBeVisible({
      timeout: 15_000,
    });

    expect(pageErrors, `the page threw: ${pageErrors.join(" | ")}`).toEqual([]);
  } finally {
    await page.request
      .delete(`/api/v1/company/mcp/servers/${encodeURIComponent(name)}`)
      .catch(() => undefined);
  }
});
