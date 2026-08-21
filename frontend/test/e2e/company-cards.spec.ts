import { expect, test, type Page } from "@playwright/test";

/**
 * End-to-end proof for issue #1141 — the Company page leads with the teammates.
 *
 * # The failure this reproduces
 *
 * Everything here already existed and none of it was reachable. `#/team`
 * rendered the teammate card grid and `#/team/<agentId>` opened a full detail
 * sub-page, but `team` was routable *without a nav entry* — so the only way to
 * either was to type a URL nobody knew. The one nav entry that leads here,
 * Company, opened the org chart: the desks, not the people.
 *
 * On the parent commit the first test fails because the Company nav lands on a
 * tree with no teammate card on it, and the rest because there is no toggle, no
 * status or workload on a card, and no breadcrumb on the detail page.
 *
 * # Why this mocks the operator API
 *
 * Like `org-tree.spec.ts` beside it. The interesting input is a *board*: cards
 * spread across the host's columns, one of them assigned to a desk rather than
 * to a person, which is what proves the count is the teammate's own work and
 * not their desk's. No default harness company produces that on demand.
 *
 * No `LIVE_BRAIN` gate: nothing here needs inference.
 */

const COMPANY = "acme";

const ROSTER = [
  {
    id: "maya",
    name: "Maya",
    role: "Research Lead",
    description: "Tracks competitor moves and drafts the weekly brief.",
  },
  { id: "ravi", name: "Ravi", role: "Analyst", description: "Digs through the numbers." },
  { id: "priya", name: "Priya", role: "Writer", description: "Turns findings into words." },
];

/**
 * The board, as the host's fixed column table declares it (`src/ledger/board.rs`).
 * `closed` is what makes a card open or finished, and it comes from here rather
 * than from any console-side list.
 */
const STATUSES = [
  { name: "todo", label: "To-do", closed: false },
  { name: "planning", label: "Planning", closed: false },
  { name: "in_progress", label: "In progress", closed: false },
  { name: "paused", label: "Paused", closed: false },
  { name: "in_review", label: "In review", closed: false },
  { name: "done", label: "Done", closed: true },
];

/**
 * One card per interesting case:
 *
 * - Maya has an attempt open **and** something queued → working, 2 open.
 * - Ravi has only finished work → idle, 0 open. The `done` card must not count.
 * - Priya has nothing of her own; the in-flight card next to her name is her
 *   *desk's*, and the host deliberately never resolves a desk assignment to a
 *   person. So she reads idle with nothing, not working with one.
 */
const TASKS = [
  { id: "t1", title: "Scan competitor pricing", column: "in_progress", priority: "high", assignee: "maya", updatedAt: 0 },
  { id: "t2", title: "Draft the weekly brief", column: "todo", priority: "medium", assignee: "maya", updatedAt: 0 },
  { id: "t3", title: "Q3 cohort numbers", column: "done", priority: "medium", assignee: "ravi", updatedAt: 0 },
  { id: "t4", title: "Rewrite the landing copy", column: "in_progress", priority: "medium", assignee: "research", updatedAt: 0 },
];

async function mockApi(page: Page) {
  // The first-run product tour renders a modal over everything and swallows
  // clicks beneath it. Answer "already skipped" for any company id.
  await page.addInitScript(() => {
    const real = Storage.prototype.getItem;
    Storage.prototype.getItem = function getItem(key: string) {
      return key.startsWith("oc-tour:") ? '{"skipped":true}' : real.call(this, key);
    };
  });

  await page.route("**/api/v1/**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    const json = (body: unknown, status = 200) =>
      route.fulfill({ status, contentType: "application/json", body: JSON.stringify(body) });
    const status = { id: COMPANY, name: "Acme", lifecycle: "running", pending_approvals: 0 };

    if (path === "/api/v1/companies") return json([status]);
    if (path === `/api/v1/companies/${COMPANY}`) return json(status);
    if (path.endsWith("/desks"))
      return json([{ id: "research", name: "Research", members: ["maya", "priya"] }]);
    if (path.endsWith("/tasks")) return json(TASKS);
    if (path.endsWith("/ledgers"))
      return json({
        ledgers: [
          {
            slug: "tasks",
            title: "Tasks",
            purpose: "The board",
            source: "builtin",
            derived: "derived/TASKS.md",
            writtenBy: "the board",
            builtin: true,
            fields: [],
            statuses: STATUSES,
            sections: [],
            open: 3,
            closed: 1,
          },
        ],
      });
    if (path.endsWith("/team")) return json(ROSTER);
    const agent = path.match(/\/team\/([^/]+)$/);
    if (agent) {
      const found = ROSTER.find((m) => m.id === agent[1]);
      if (!found) return json({ error: "no such teammate" }, 404);
      return json({
        ...found,
        source: "overlay",
        // An overlay teammate is editable, which is what makes the header's
        // Edit a live control rather than a disabled explanation.
        editable: ["name", "role", "description"],
        isOrchestrator: false,
        tools: { requested: [], companyAllow: ["web_search"], effective: ["web_search"] },
        desks: [{ id: "research", name: "Research", lead: true }],
        inboxEnabled: false,
      });
    }
    if (path.endsWith("/me")) return json({ id: "op", email: "op@example.com", role: "admin" });
    if (path.endsWith("/events"))
      return route.fulfill({ status: 200, contentType: "text/event-stream", body: "" });
    return json([]);
  });
}

const card = (page: Page, name: string) =>
  page.getByTestId("team-card").filter({ hasText: name }).first();

test("#1141 the Company nav lands on the teammates, not on the desks", async ({ page }) => {
  await mockApi(page);

  // Start elsewhere, so arriving is a real navigation rather than the page the
  // document happened to load on.
  await page.goto("/#/overview");
  const nav = page
    .getByRole("link", { name: "Company", exact: true })
    .or(page.getByRole("button", { name: "Company", exact: true }))
    .first();
  await expect(nav).toBeVisible({ timeout: 30_000 });
  await nav.click();

  // The cards, which no operator could reach before this: `#/team` rendered
  // them from a route with no nav entry.
  await expect(card(page, "Maya")).toBeVisible({ timeout: 30_000 });
  await expect.poll(() => page.url()).toContain("#/company");
});

test("#1141 a card carries the description, the status and the open count", async ({ page }) => {
  await mockApi(page);
  await page.goto("/#/company");

  const maya = card(page, "Maya");
  await expect(maya).toBeVisible({ timeout: 30_000 });

  // What the teammate is for. It was on the roster read all along.
  await expect(maya.getByTestId("team-card-description")).toContainText(
    "Tracks competitor moves",
  );

  // What they are on, and how much. Both derived from the board — an attempt
  // open makes Maya working, and the queued card counts beside it.
  await expect(maya.getByTestId("team-card-status")).toHaveText("Working");
  await expect(maya.getByTestId("team-card-tasks")).toHaveText("2 open tasks");

  // Ravi's only card is finished, and a closed column is not open work.
  const ravi = card(page, "Ravi");
  await expect(ravi.getByTestId("team-card-status")).toHaveText("Idle");
  await expect(ravi.getByTestId("team-card-tasks")).toHaveText("0 open tasks");

  // Priya sits on the desk the in-flight card is assigned to, and that card is
  // the *desk's*. The host refuses to resolve a desk assignment to a person
  // (`AssigneeResolution::links_working_agent`), and neither does this — so she
  // is idle with nothing rather than credited with somebody else's work.
  const priya = card(page, "Priya");
  await expect(priya.getByTestId("team-card-status")).toHaveText("Idle");
  await expect(priya.getByTestId("team-card-tasks")).toHaveText("0 open tasks");
});

test("the status line sits on one baseline across a row, whatever the description", async ({
  page,
}) => {
  await mockApi(page);
  await page.goto("/#/company");
  await expect(card(page, "Maya")).toBeVisible({ timeout: 30_000 });

  // The precondition. This proves nothing unless the descriptions really do
  // wrap to different heights: Maya's sentence takes two lines at a card's
  // width, Ravi's takes one. If a future fixture flattens them, this fails
  // here rather than passing vacuously below.
  const tall = await card(page, "Maya").getByTestId("team-card-description").boundingBox();
  const short = await card(page, "Ravi").getByTestId("team-card-description").boundingBox();
  expect(tall!.height).toBeGreaterThan(short!.height);

  // Same grid row, so the cards are the same height — and the one line an
  // operator scans a roster *for* has to land in the same place on each. It
  // used to follow the description, which put "Working · 2 open tasks" a line
  // higher or lower than its neighbour in every row, with dead space below
  // each card.
  const ys = await Promise.all(
    ["Maya", "Ravi", "Priya"].map(async (name) => {
      const box = await card(page, name).getByTestId("team-card-status").boundingBox();
      return Math.round(box!.y);
    }),
  );
  expect(ys[1]).toBe(ys[0]);
  expect(ys[2]).toBe(ys[0]);

  // And it is the *bottom* they share, not an accident of equal content: the
  // gap under the status line is the card's own padding on every card.
  const gaps = await Promise.all(
    ["Maya", "Ravi", "Priya"].map(async (name) => {
      const c = await card(page, name).boundingBox();
      const status = await card(page, name).getByTestId("team-card-status").boundingBox();
      return Math.round(c!.y + c!.height - (status!.y + status!.height));
    }),
  );
  expect(gaps[1]).toBe(gaps[0]);
  expect(gaps[2]).toBe(gaps[0]);
  // Padding, not a void. Measured at 32px against a live host — `CardContent`'s
  // own `py-4` plus the card's. Bounded generously because the exact figure is
  // the design system's to change; what this test is about is that the three
  // agree, which they did not before.
  expect(gaps[0]).toBeLessThan(48);
});

test("#1193 the chart is a destination with its own address, not a mode", async ({ page }) => {
  await mockApi(page);
  await page.goto("/#/company");
  await expect(card(page, "Maya")).toBeVisible({ timeout: 30_000 });

  // There is no toggle. Cards is the Company page.
  await expect(page.getByTestId("company-mode-cards")).toHaveCount(0);
  await expect(page.getByTestId("company-mode-chart")).toHaveCount(0);

  // One named way in, and it lands on an address of its own.
  await page.getByTestId("company-manage-desks").click();
  const chart = page.getByRole("tree", { name: "Company org chart" });
  await expect(chart).toBeVisible({ timeout: 30_000 });
  await expect.poll(() => page.url()).toContain("#/company/desks");

  // Which means it survives a reload. The remembered *mode* it replaced could
  // not be linked, and could open Company on the chart for an operator who
  // only wanted to see their team.
  await page.reload();
  await expect(chart).toBeVisible({ timeout: 30_000 });

  // And it owes a way back, like any sub-page.
  await page.getByTestId("desks-breadcrumb-company").click();
  await expect(card(page, "Maya")).toBeVisible({ timeout: 30_000 });
});

test("#1193 the Company nav row means the roster, even after visiting Desks", async ({
  page,
}) => {
  await mockApi(page);
  await page.goto("/#/company");
  await page.getByTestId("company-manage-desks").click();
  await expect(page.getByRole("tree", { name: "Company org chart" })).toBeVisible({
    timeout: 30_000,
  });

  // Step away, then come back through the sidebar. The shell remembers the last
  // sub-segment per view so a tab switch keeps deep state — right for
  // `#/workflows/<id>`, wrong here, because Company's segments are two different
  // surfaces rather than two places inside one. Remembering it would open the
  // org chart for an operator who clicked "Company" wanting their team: the
  // remembered-mode failure #1193 removed, wearing a different mechanism.
  await page.getByRole("link", { name: "Overview", exact: true })
    .or(page.getByRole("button", { name: "Overview", exact: true }))
    .first()
    .click();
  await page.getByRole("link", { name: "Company", exact: true })
    .or(page.getByRole("button", { name: "Company", exact: true }))
    .first()
    .click();

  await expect(card(page, "Maya")).toBeVisible({ timeout: 30_000 });
  await expect.poll(() => page.url()).not.toContain("desks");
});

test("#1193 desk management survives — a desk can still be created and reached", async ({
  page,
}) => {
  await mockApi(page);
  await page.goto("/#/company");

  // The failure mode this guards: a Company page with no way to create a desk.
  // The chart is the only surface that can (issues #302, #311), so the route to
  // it has to work from the roster with no desk already in hand.
  await page.getByTestId("company-manage-desks").click();
  await expect(page.getByRole("tree", { name: "Company org chart" })).toBeVisible({
    timeout: 30_000,
  });
  await expect(page.getByRole("button", { name: "New desk" })).toBeEnabled({ timeout: 30_000 });
  await expect(page.getByRole("button", { name: "Add teammate" }).first()).toBeEnabled();
});

test("#485 a desk address still opens the chart at that desk", async ({ page }) => {
  await mockApi(page);
  // The deep link chat's member pane relies on. It predates the toggle and
  // outlives it.
  await page.goto("/#/company/research");
  await expect(page.getByRole("tree", { name: "Company org chart" })).toBeVisible({
    timeout: 30_000,
  });
  await expect.poll(() => page.url()).toContain("#/company/research");
});

test("#1141 a host with no board says nothing, rather than idle", async ({ page }) => {
  await mockApi(page);
  // Routed *after* `mockApi`, so this handler wins: a ledger list with no board
  // in it. `fetchBoardColumns` resolves empty for that rather than rejecting,
  // which is the failure most likely to be read as "everybody is free".
  await page.route("**/api/v1/**/ledgers", (route) =>
    route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ ledgers: [] }),
    }),
  );

  await page.goto("/#/company");
  const maya = card(page, "Maya");
  await expect(maya).toBeVisible({ timeout: 30_000 });
  // The teammate still renders in full; only the claim this console cannot
  // support is missing.
  await expect(maya.getByTestId("team-card-description")).toBeVisible();
  await expect(maya.getByTestId("team-card-status")).toHaveCount(0);
  await expect(maya.getByTestId("team-card-tasks")).toHaveCount(0);
});

test("#1181 the card and the detail header wear the same mascot", async ({ page }) => {
  await mockApi(page);
  await page.goto("/#/company");

  // A drawing, not two letters. Before #1181 both surfaces rendered `initials()`
  // over a tone tile while the same teammate had a face in chat.
  const onCard = card(page, "Maya").locator("img");
  await expect(onCard).toBeVisible({ timeout: 30_000 });
  const cardSrc = await onCard.getAttribute("src");
  expect(cardSrc).toContain("/avatars/blob-");

  await card(page, "Maya").getByTestId("team-card-open").click();
  await expect(page.getByTestId("agent-name")).toHaveText("Maya", { timeout: 30_000 });

  // The same file, not merely some mascot. A screenshot showing *a* face proves
  // nothing — two surfaces seeding differently each look internally consistent,
  // which is exactly why the mismatch survives review. See
  // `test/unit/teammate-avatar-seed.test.ts` for the two seeds in play.
  const onDetail = page.getByTestId("agent-avatar").locator("img");
  await expect(onDetail).toHaveAttribute("src", cardSrc ?? "");
});

test("#1190 the card carries no switch; the inbox lives on the teammate", async ({ page }) => {
  await mockApi(page);
  await page.goto("/#/company");
  await expect(card(page, "Maya")).toBeVisible({ timeout: 30_000 });

  // Nothing on the grid writes to the host. The card is for recognising a
  // teammate, and the switch was one mis-click from a silent config change
  // while scanning thirteen of them.
  await expect(page.getByTestId("team-inbox-toggle")).toHaveCount(0);
  await expect(page.getByRole("switch")).toHaveCount(0);

  // The capability is not gone — it moved to the page that already reported it.
  await card(page, "Maya").getByTestId("team-card-open").click();
  await expect(page.getByTestId("agent-inbox-toggle")).toBeVisible({ timeout: 30_000 });
});

test("#1141 bare #/team is the Company page now", async ({ page }) => {
  await mockApi(page);

  // The address that used to render this grid from nowhere. One grid, one
  // address: it redirects rather than answering in parallel.
  await page.goto("/#/team");
  await expect(card(page, "Maya")).toBeVisible({ timeout: 30_000 });
  await expect.poll(() => page.url()).toContain("#/company");
  await expect.poll(() => page.url()).not.toContain("#/team");
});

test("#1141 a card opens a teammate, breadcrumbed and editable", async ({ page }) => {
  await mockApi(page);
  await page.goto("/#/company");
  await card(page, "Maya").getByTestId("team-card-open").click();

  // Still a linkable page rather than a modal (issue #264).
  await expect.poll(() => page.url()).toContain("#/team/maya");
  await expect(page.getByTestId("agent-name")).toHaveText("Maya", { timeout: 30_000 });

  // The breadcrumb says where the operator *is* — this page is linked into
  // from the org chart and the chat pane, so "Back to team" named a page half
  // its arrivals had never seen.
  const crumb = page.getByTestId("agent-breadcrumb");
  await expect(crumb).toContainText("Company");
  await expect(crumb).toContainText("Maya");

  // The same two facts the card showed, on the page they belong to.
  await expect(page.getByTestId("agent-status")).toHaveText("Working");
  await expect(page.getByTestId("agent-tasks")).toHaveText("2 open tasks");

  // Edit is on the header row, not buried in a card halfway down, and this
  // teammate is an overlay so it is live.
  const edit = page.getByTestId("agent-edit");
  await expect(edit).toBeEnabled();
  await edit.click();
  await expect(page.getByTestId("agent-save")).toBeVisible();

  // And the crumb goes back to the page it names.
  await page.getByTestId("agent-breadcrumb-company").click();
  await expect(card(page, "Maya")).toBeVisible({ timeout: 30_000 });
  await expect.poll(() => page.url()).toContain("#/company");
});
