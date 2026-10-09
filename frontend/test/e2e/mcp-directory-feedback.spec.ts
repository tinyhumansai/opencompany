import { expect, test, type Page, type Route } from "@playwright/test";

/**
 * Discover against a mock registry: paging, slow and failing searches, and the
 * entry pop-up's install gating. Every `/mcp/registry/*` read is answered here,
 * so the spec runs on a host built with or without the `mcp` feature.
 */

const path = (suffix: RegExp) => (url: URL) => suffix.test(url.pathname);

function entry(name: string) {
  return {
    qualifiedName: `io.example/${name}`,
    displayName: name,
    description: `${name} tools`,
    source: "mcp_official",
    official: true,
    useCount: 0,
  };
}

function results(n: number, total: number, names: string[]) {
  return { page: n, totalPages: total, servers: names.map(entry) };
}

async function json(route: Route, body: unknown, status = 200) {
  await route.fulfill({ status, contentType: "application/json", body: JSON.stringify(body) });
}

const TIMEOUT = {
  error: "The MCP directory is taking too long to search right now. Try again in a moment.",
  code: "registry_timeout",
};

type Answer = { status?: number; body: unknown; delayMs?: number };

/** Answers each search by its `q` and `page`, recording what was asked. */
async function mockRegistry(page: Page, answer: (q: string, n: number) => Answer | Answer[]) {
  const asked: string[] = [];
  const queued = new Map<string, Answer[]>();
  await page.route(path(/\/mcp\/registry\/search$/), async (route) => {
    const url = new URL(route.request().url());
    const q = url.searchParams.get("q") ?? "";
    const n = Number(url.searchParams.get("page") ?? "1");
    const key = `${q}|${n}`;
    asked.push(key);
    if (!queued.has(key)) {
      const a = answer(q, n);
      queued.set(key, Array.isArray(a) ? [...a] : [a]);
    }
    const list = queued.get(key)!;
    const next = list.length > 1 ? list.shift()! : list[0];
    if (next.delayMs) await new Promise((r) => setTimeout(r, next.delayMs));
    await json(route, next.body, next.status ?? 200).catch(() => {});
  });
  return asked;
}

async function openDiscover(page: Page) {
  await page.goto("/#/connections/mcp?view=discover");
  await expect(page.getByTestId("mcp-discover")).toBeVisible({ timeout: 30_000 });
}

function cardNames(page: Page) {
  return page.getByTestId("mcp-discover-card").locator("span.truncate.text-sm");
}

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    const real = Storage.prototype.getItem;
    Storage.prototype.getItem = function getItem(key: string) {
      return key.startsWith("oc-tour:") ? '{"skipped":true}' : real.call(this, key);
    };
  });
});

test("Show more appends without duplicates, retries a failed page, and ends", async ({
  page,
}) => {
  await mockRegistry(page, (_q, n) => {
    if (n === 1) return { body: results(1, 3, ["alpha", "beta"]) };
    if (n === 2) return [{ status: 504, body: TIMEOUT }, { body: results(2, 3, ["beta", "gamma"]) }];
    return { body: results(3, 3, ["delta"]) };
  });
  await openDiscover(page);
  await expect(cardNames(page)).toHaveText(["alpha", "beta"]);

  await page.getByTestId("mcp-discover-more").click();
  await expect(page.getByTestId("mcp-discover-more-error")).toContainText("taking too long");
  await expect(page.getByTestId("mcp-discover-more")).toHaveText(/Retry/);
  await expect(cardNames(page)).toHaveText(["alpha", "beta"]);

  await page.getByTestId("mcp-discover-more").click();
  await expect(cardNames(page)).toHaveText(["alpha", "beta", "gamma"]);
  await expect(page.getByTestId("mcp-discover-more-error")).toHaveCount(0);

  await page.getByTestId("mcp-discover-more").click();
  await expect(cardNames(page)).toHaveText(["alpha", "beta", "gamma", "delta"]);
  await expect(page.getByTestId("mcp-discover-more")).toHaveCount(0);
});

test("a search keeps earlier results on screen and says what it is searching for", async ({
  page,
}) => {
  await mockRegistry(page, (q) =>
    q === "" ? { body: results(1, 1, ["alpha", "beta"]) } : { body: results(1, 1, ["notion"]), delayMs: 2_500 },
  );
  await openDiscover(page);
  await expect(cardNames(page)).toHaveText(["alpha", "beta"]);

  await page.getByTestId("mcp-discover-search").fill("notion");
  await expect(page.getByTestId("mcp-discover-searching")).toContainText("Searching for “notion”");
  await expect(cardNames(page)).toHaveText(["alpha", "beta"]);
  await expect(page.getByTestId("mcp-discover-results")).toHaveAttribute("aria-busy", "true");
  await expect(page.getByTestId("mcp-discover-results")).toHaveCSS("opacity", "0.5");

  await expect(cardNames(page)).toHaveText(["notion"], { timeout: 10_000 });
  await expect(page.getByTestId("mcp-discover-searching")).toHaveCount(0);
});

test("a superseded slow search is aborted and never overwrites newer results", async ({
  page,
}) => {
  await mockRegistry(page, (q) => {
    if (q === "git") return { body: results(1, 1, ["gitlab"]), delayMs: 4_000 };
    if (q === "github") return { body: results(1, 1, ["github"]) };
    return { body: results(1, 1, ["alpha"]) };
  });
  const aborted: string[] = [];
  page.on("requestfailed", (request) => {
    if (request.url().includes("/mcp/registry/search")) aborted.push(request.url());
  });
  await openDiscover(page);

  const search = page.getByTestId("mcp-discover-search");
  await search.fill("git");
  await page.waitForRequest((r) => r.url().includes("q=git&"));
  await search.fill("github");
  await expect(cardNames(page)).toHaveText(["github"]);

  await page.waitForTimeout(4_500);
  await expect(cardNames(page)).toHaveText(["github"]);
  expect(aborted.some((u) => u.includes("q=git&"))).toBe(true);
});

test("a timed-out search shows the typed copy and Retry searches again", async ({ page }) => {
  await mockRegistry(page, (q) =>
    q === "notion"
      ? [{ status: 504, body: TIMEOUT }, { body: results(1, 1, ["notion"]) }]
      : { body: results(1, 1, ["alpha"]) },
  );
  await openDiscover(page);

  await page.getByTestId("mcp-discover-search").fill("notion");
  const error = page.getByTestId("mcp-registry-error");
  await expect(error).toContainText("Couldn't search for “notion”");
  await expect(error).toContainText("taking too long");
  await expect(error).not.toContainText("harness");

  await page.getByTestId("mcp-registry-retry").click();
  await expect(cardNames(page)).toHaveText(["notion"]);
});

test("a search shows popular matches at once and keeps them when the directory times out", async ({
  page,
}) => {
  await mockRegistry(page, (q) => {
    if (q === "") return { body: results(1, 1, ["Notion", "GitHub"]) };
    if (q === "notion")
      return [
        { status: 504, body: TIMEOUT, delayMs: 2_500 },
        { body: results(1, 1, ["Notion", "Notion Calendar"]) },
      ];
    return { status: 504, body: TIMEOUT };
  });
  await openDiscover(page);
  await expect(cardNames(page)).toHaveText(["Notion", "GitHub"]);

  await page.getByTestId("mcp-discover-search").fill("notion");
  await expect(page.getByTestId("mcp-discover-searching")).toContainText("Searching for “notion”");
  await expect(cardNames(page)).toHaveText(["Notion"]);
  await expect(page.getByTestId("mcp-discover-results")).toHaveCSS("opacity", "1");

  const note = page.getByTestId("mcp-discover-fallback");
  await expect(note).toContainText("Showing popular matches — the MCP directory is slow right now.", {
    timeout: 10_000,
  });
  await expect(cardNames(page)).toHaveText(["Notion"]);
  await expect(page.getByTestId("mcp-registry-error")).toHaveCount(0);

  await page.getByTestId("mcp-discover-fallback-retry").click();
  await expect(cardNames(page)).toHaveText(["Notion", "Notion Calendar"]);
  await expect(note).toHaveCount(0);

  await page.getByTestId("mcp-discover-search").fill("zomato");
  await expect(page.getByTestId("mcp-registry-error")).toContainText("Couldn't search for “zomato”");
  await expect(page.getByTestId("mcp-registry-retry")).toBeVisible();
  await expect(page.getByTestId("mcp-discover-fallback")).toHaveCount(0);
});

test("the entry pop-up will not install while its lookup has failed", async ({ page }) => {
  await mockRegistry(page, () => ({ body: results(1, 1, ["zomato"]) }));
  let lookups = 0;
  await page.route(path(/\/mcp\/registry\/entry$/), (route) => {
    lookups += 1;
    return lookups === 1
      ? json(
          route,
          { error: "The MCP directory can't look up this server right now. Try again in a moment.", code: "registry_unavailable" },
          503,
        )
      : json(route, {
          ...entry("zomato"),
          endpoint: "https://mcp.example.test/mcp",
          installable: true,
          requiredEnvKeys: [],
        });
  });
  await openDiscover(page);

  await page.getByTestId("mcp-discover-card").first().click();
  await expect(page.getByTestId("mcp-discover-detail-failed")).toContainText("can't look up");
  await expect(page.getByTestId("mcp-discover-detail-install")).toBeDisabled();

  await page.getByTestId("mcp-discover-detail-retry").click();
  await expect(page.getByTestId("mcp-discover-detail-failed")).toHaveCount(0);
  await expect(page.getByTestId("mcp-discover-detail-install")).toBeEnabled();

  await page.route(path(/\/mcp\/registry\/install$/), (route) =>
    json(route, {
      server: {
        name: "zomato",
        endpoint: "https://mcp.example.test/mcp",
        source: "runtime",
        enabled: true,
        allowedTools: [],
        disallowedTools: [],
        readOnlyTools: [],
        timeoutSecs: 30,
        authConfigured: false,
      },
      note: "Installed.",
    }),
  );
  await page.getByTestId("mcp-discover-detail-install").click();
  await expect(page.getByTestId("mcp-discover-detail")).toHaveCount(0);
});
