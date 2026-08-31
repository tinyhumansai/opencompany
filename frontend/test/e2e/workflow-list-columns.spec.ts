import { expect, test, type Page, type Route } from "@playwright/test";

/**
 * Issue #1136: in List mode the workflow index had three columns and not one
 * shared vertical edge between them.
 *
 * Every row laid itself out with `justify-between`, so each one sized its own
 * cells: the descriptions were pushed right and their left edge zigzagged by a
 * couple of hundred pixels from row to row, the status cell floated so the state
 * dots never formed a line, and "No recent runs" landed somewhere different
 * again. The eye had nothing to track down.
 *
 * **Geometry is the assertion**, because geometry is the defect. Nothing about
 * the text changed — a spec that read the rendered strings passes just as
 * happily against the broken build. What is pinned here is that every cell in a
 * column starts on the same pixel, over enough rows and enough description
 * lengths for a ragged edge to have somewhere to hide.
 *
 * The list and the run page are both stubbed. The harness company ships three
 * workflows and a host with no inference source can never journal a failed,
 * blocked or running one — and three near-identical rows would report a
 * straight edge whether or not the columns are fixed.
 *
 * Runs against the live host `playwright.config.ts` brings up.
 */

/** The workflows list route, and only it — not `…/workflows/{id}` or `/runs`. */
function isWorkflowList(url: URL): boolean {
  return /\/api\/v1\/(company|companies\/[^/]+)\/workflows$/.test(url.pathname);
}

/** The company-wide run page. */
function isRunPage(url: URL): boolean {
  return /\/api\/v1\/(company|companies\/[^/]+)\/workflows\/runs$/.test(url.pathname);
}

/**
 * Twelve workflows whose names and descriptions vary as widely as real ones do
 * — one word to a full sentence, and one with no description at all. The width
 * spread is the point: it is what a per-row layout turns into a ragged edge.
 */
const WORKFLOWS = [
  { id: "feature_pipeline", name: "Feature pipeline", description: "A feature request goes from spec to a tested, documented ship.", editable: false },
  { id: "aj", name: "AJ", description: "Should triage across all closed github issues and reopen anything that regressed since the last release." },
  { id: "ai_trends", name: "AI trends", description: "Every day at 9 am look for trending ai papers." },
  { id: "incident_postmortem", name: "Incident postmortem drafting service", description: "Reads the incident channel, the run journal and the deploy log, then drafts a blameless postmortem with a timeline and follow-up actions." },
  { id: "release_notes", name: "Release notes", description: "Turn merged pull requests into notes." },
  { id: "customer_digest", name: "Customer digest", description: "Collect support threads from the last seven days and send the product team one digest." },
  { id: "dep_bumps", name: "Dependency bumps", description: "Bump." },
  { id: "security_sweep", name: "Security sweep of the vendored tree", description: "Walk every vendored dependency and open one issue per unpatched advisory." },
  { id: "onboarding_buddy", name: "Onboarding buddy", description: "Answer a new hire's first-week questions from the handbook." },
  { id: "cost_watch", name: "Cost watch", description: "Compare this month's model spend against the last three." },
  { id: "docs_linter", name: "Docs linter", description: "Check every markdown file for broken links and stale command names." },
  { id: "standup_summary", name: "Standup summary" },
].map((w) => ({ enabled: true, ...w }));

/** One run each for most of them, in a spread of states — and none at all for
 * three, so the "No recent runs" reading is in the sample too. */
const RUNS = (() => {
  const now = Date.now();
  const hour = 3_600_000;
  const base = (workflowId: string, seq: number, ageHours: number) => ({
    seq,
    atMillis: now - ageHours * hour,
    workflowId,
    scheduled: false,
    runId: `${workflowId}-run`,
    deliveries: [] as { status: string; destination: string }[],
    pendingApprovals: [] as string[],
  });
  return [
    { ...base("feature_pipeline", 40, 21), blockedNodes: [{ nodeId: "spec", tools: ["publish_artifact"], approvalIds: ["a1"] }], pendingApprovals: ["spec"] },
    { ...base("aj", 39, 168) },
    { ...base("incident_postmortem", 38, 2), error: "boom" },
    { ...base("release_notes", 37, 0.2), scheduled: true },
    { ...base("customer_digest", 36, 30), running: true, startedAtMillis: now - 30 * hour, startedNodes: ["collect"] },
    { ...base("dep_bumps", 35, 400), scheduled: true, deliveries: [{ status: "failed", destination: "email" }, { status: "failed", destination: "slack" }] },
    { ...base("security_sweep", 34, 9), cancelled: true },
    { ...base("cost_watch", 33, 0.01), scheduled: true },
    { ...base("docs_linter", 32, 72), deliveries: [{ status: "pending", destination: "email" }] },
  ];
})();

/**
 * Serves the stubbed index, in List mode.
 *
 * The mode is set through the key the view persists it under rather than by
 * clicking the toolbar's Cards/List toggle: which control switches it, and what
 * it is called, is the toolbar's business (issue #1110), and this spec is about
 * what the rows do once the mode is on.
 */
async function openList(page: Page) {
  await page.route(
    (url) => isWorkflowList(url),
    async (route: Route) => {
      if (route.request().method() !== "GET") return route.fallback();
      await route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(WORKFLOWS) });
    },
  );
  await page.route(
    (url) => isRunPage(url),
    async (route: Route) => {
      if (route.request().method() !== "GET") return route.fallback();
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({ runs: RUNS, hasMore: false }),
      });
    },
  );
  await page.addInitScript(() => {
    window.localStorage.setItem("oc.workflows.indexMode", "list");
  });

  await page.goto("/#/workflows");

  // The first-run tour's overlay swallows pointer events and blurs the page.
  const skip = page.getByRole("button", { name: "Skip for now" });
  try {
    await skip.waitFor({ state: "visible", timeout: 10_000 });
    await skip.click();
    await expect(skip).toBeHidden();
  } catch {
    /* a company that has seen it never shows it again */
  }

  const rows = page.getByTestId("workflow-list-row");
  await expect(rows).toHaveCount(WORKFLOWS.length);
  return rows;
}

/** The left and right edge of every visible cell of every row. */
async function cellEdges(page: Page): Promise<{ left: number; right: number; text: string }[][]> {
  return page.evaluate(() =>
    [...document.querySelectorAll('[data-testid="workflow-list-row"]')].map((row) =>
      [...row.children]
        .filter((cell) => (cell as HTMLElement).offsetParent !== null)
        .map((cell) => {
          const box = cell.getBoundingClientRect();
          return { left: Math.round(box.left), right: Math.round(box.right), text: cell.textContent ?? "" };
        }),
    ),
  );
}

/** Every value in `xs`, deduplicated — one entry means one shared edge. */
function distinct(xs: number[]): number[] {
  return [...new Set(xs)];
}

test("every column of the list shares one vertical edge", async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 });
  await openList(page);

  const rows = await cellEdges(page);
  // Name, description, status, time. A row whose last run is missing has no
  // time cell — it is the last column, so nothing follows it to shift.
  expect(distinct(rows.map((cells) => cells.length)).sort()).toEqual([3, 4]);

  for (const column of [0, 1, 2]) {
    const lefts = distinct(rows.map((cells) => cells[column].left));
    expect(lefts, `column ${column} left edges: ${lefts.join(", ")}`).toHaveLength(1);
    const rights = distinct(rows.map((cells) => cells[column].right));
    expect(rights, `column ${column} right edges: ${rights.join(", ")}`).toHaveLength(1);
  }

  // The time reads right to left — "21h ago" and "7d ago" have to end together.
  const timeRights = distinct(rows.filter((cells) => cells.length === 4).map((cells) => cells[3].right));
  expect(timeRights, `time right edges: ${timeRights.join(", ")}`).toHaveLength(1);

  // The three columns are in the order the operator reads them, and each one
  // holds what it says it does. Located by name rather than index 0: issue
  // #1683 sorts the index by most-recently-run, so which row that fixture
  // lands in is no longer "whichever is first in the fixture array".
  const featurePipelineRow = rows.find((cells) => cells[0].text.includes("Feature pipeline"));
  expect(featurePipelineRow, "the Feature pipeline row is rendered").toBeDefined();
  const [name, description, status] = featurePipelineRow!;
  expect(name.right).toBeLessThan(description.left);
  expect(description.right).toBeLessThan(status.left);
  expect(name.text).toContain("Feature pipeline");
  expect(description.text).toContain("A feature request goes from spec");
  expect(status.text).toContain("Manual run blocked");

  // And the dots the eye actually tracks down the page start on one pixel.
  const dotLefts = await page.evaluate(() =>
    [...document.querySelectorAll('[data-testid="workflow-list-row"] .rounded-full')].map((dot) =>
      Math.round(dot.getBoundingClientRect().left),
    ),
  );
  expect(dotLefts.length).toBeGreaterThan(5);
  expect(distinct(dotLefts), `dot left edges: ${distinct(dotLefts).join(", ")}`).toHaveLength(1);
});

test("the description is the column that yields when the list narrows", async ({ page }) => {
  await page.setViewportSize({ width: 700, height: 900 });
  await openList(page);

  const rows = await cellEdges(page);
  // Three cells now: the description is `display: none`, so it is not a grid
  // item at all and the name takes the space it left.
  expect(distinct(rows.map((cells) => cells.length)).sort()).toEqual([2, 3]);

  // Located by name rather than index 0 — see the comment on the same lookup
  // above.
  const featurePipelineRow = rows.find((cells) => cells[0].text.includes("Feature pipeline"));
  expect(featurePipelineRow, "the Feature pipeline row is rendered").toBeDefined();
  const [name, status] = featurePipelineRow!;
  expect(name.text).toContain("Feature pipeline");
  expect(status.text).toContain("Manual run blocked");
  expect(rows.some((cells) => cells.some((cell) => cell.text.includes("A feature request goes")))).toBe(false);

  // Still aligned — a narrow list is not an excuse for a ragged one.
  for (const column of [0, 1]) {
    const lefts = distinct(rows.map((cells) => cells[column].left));
    expect(lefts, `column ${column} left edges: ${lefts.join(", ")}`).toHaveLength(1);
  }
});
