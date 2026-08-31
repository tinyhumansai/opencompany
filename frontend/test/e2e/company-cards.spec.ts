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
  {
    id: "ravi",
    name: "Ravi",
    role: "Analyst",
    description: "Digs through the numbers.",
    global: true,
  },
  { id: "priya", name: "Priya", role: "Writer", description: "Turns findings into words." },
];

/**
 * The board, as the host's fixed phase table declares it (`src/ledger/board.rs`).
 * `closed` is what makes a card open or finished, and it comes from here rather
 * than from any console-side list.
 *
 * Three since issue #1512. The *stage* a card is in rides on the card itself,
 * which is why the fixtures below carry both.
 */
const STATUSES = [
  { name: "pending", label: "Pending", closed: false },
  { name: "working", label: "Working", closed: false },
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
  { id: "t1", title: "Scan competitor pricing", column: "working", stage: "in_progress", priority: "high", assignee: "maya", updatedAt: 0 },
  { id: "t2", title: "Draft the weekly brief", column: "pending", priority: "medium", assignee: "maya", updatedAt: 0 },
  { id: "t3", title: "Q3 cohort numbers", column: "done", priority: "medium", assignee: "ravi", updatedAt: 0 },
  { id: "t4", title: "Rewrite the landing copy", column: "working", stage: "in_progress", priority: "medium", assignee: "research", updatedAt: 0 },
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
    if (path.endsWith("/memory"))
      // `GET /memory` answers with `{ items, totalContext, contextTruncated }`
      // — the Overview's constellation reads the rows from `items`, and a bare
      // array would leave it `undefined` and crash the graph render.
      return json({ items: [], totalContext: 0, contextTruncated: false });
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
        tools: { requested: [], companyAllow: ["web_search"], deskAllow: [], deskCeilingActive: false, effective: ["web_search"] },
        desks: [{ id: "research", name: "Research", lead: true }],
        inboxEnabled: false,
      });
    }
    if (path.endsWith("/me")) return json({ id: "op", email: "op@example.com", role: "admin" });
    // Issue #1844: without this the fallback below answers `GET …/activation`
    // with `[]` — truthy, so `shouldShowOnboardingGate` reads `isActivated` as
    // `undefined` and, since `/me` above already resolves this operator as
    // admin, opens the blocking gate over every one of this file's tests
    // instead of the shell they actually exercise.
    if (path.endsWith("/activation"))
      return json({
        nameConfirmed: true,
        integrationConnected: true,
        workflowRunSucceeded: true,
        isActivated: true,
      });
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

test("#1436 the roster can search names, show working teammates, and identify the baseline", async ({
  page,
}) => {
  await mockApi(page);
  await page.goto("/#/company");
  await expect(card(page, "Maya")).toBeVisible({ timeout: 30_000 });

  // Ravi follows the company roster only because he belongs to the shared
  // baseline. The badge explains the deliberate boundary in host ordering.
  await expect(card(page, "Ravi").getByTestId("team-card-global")).toHaveText("Global baseline");

  const search = page.getByTestId("team-roster-search");
  await search.fill("priya");
  await expect(card(page, "Priya")).toBeVisible();
  await expect(card(page, "Maya")).toHaveCount(0);

  await search.fill("");
  await page.getByTestId("team-roster-working").click();
  await expect(card(page, "Maya")).toBeVisible();
  await expect(card(page, "Ravi")).toHaveCount(0);
  await expect(card(page, "Priya")).toHaveCount(0);
});

test("#1436 a workload outage disables the Working filter instead of hiding the roster", async ({
  page,
}) => {
  await mockApi(page);
  // The two reads behind the workload status fail, so `workload` stays null
  // and no card gets a fabricated status. The Working filter has no data to
  // judge against — it must be disabled, so it cannot be switched on into a
  // state that would hide the whole roster with nothing to uncheck.
  await page.route("**/api/v1/companies/acme/tasks", (route) => route.abort());
  await page.route("**/api/v1/companies/acme/ledgers", (route) => route.abort());
  await page.goto("/#/company");

  await expect(card(page, "Maya")).toBeVisible({ timeout: 30_000 });
  // The roster is complete: no filter is silently swallowing rows, and no card
  // claims a workload the host never reported.
  await expect(card(page, "Ravi")).toBeVisible();
  await expect(card(page, "Priya")).toBeVisible();
  await expect(page.getByTestId("team-roster-working")).toBeDisabled();
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
  // Scoped to the cards, not the whole page: the roster's "Working" filter
  // above the grid is also a switch, but it is a local view filter that writes
  // nothing to the host — the thing #1190 removed from the cards.
  await expect(page.getByTestId("team-card").getByRole("switch")).toHaveCount(0);

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

  // The desk is identity, not a final detail section. Its chip goes to the
  // desk's shareable chart address, and the workload names each open card so
  // the count is not a dead end (issue #1433).
  const desk = page.getByTestId("agent-desk-research");
  await expect(desk).toContainText("Research");
  await expect(desk).toContainText("(lead)");
  await expect(desk).toHaveAttribute("href", "#/company/research");
  await expect(page.getByTestId("agent-open-task-t1")).toHaveText("Scan competitor pricing");
  await expect(page.getByTestId("agent-open-task-t1")).toHaveAttribute("href", "#/tasks/t1");
  await expect(page.getByTestId("agent-open-task-t2")).toHaveText("Draft the weekly brief");
  await expect(page.getByTestId("agent-open-task-t2")).toHaveAttribute("href", "#/tasks/t2");
  await expect(page.getByTestId("agent-open-task-t3")).toHaveCount(0);

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

test("#1433 switching teammates drops the previous one's open tasks", async ({ page }) => {
  await mockApi(page);

  // The board read, under the test's control. `AgentDetailView` stays mounted
  // across a hash change — `TeamView` renders it without a `key` — so the
  // agent-detail request for the teammate being opened races the board request
  // independently. Stalling the board on the second read is what makes that
  // race deterministic: it is the "and indefinitely if that request hangs" half
  // of the failure, and it is the half a real slow host produces.
  //
  // Registered after `mockApi`, so it wins: Playwright gives the most recently
  // added handler priority.
  let stall = false;
  await page.route("**/tasks", async (route) => {
    // Deliberately neither fulfilled nor aborted — the board read never lands.
    if (stall) return;
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify(TASKS),
    });
  });

  await page.goto("/#/team/maya");
  await expect(page.getByTestId("agent-open-task-t1")).toBeVisible({ timeout: 30_000 });

  // Same-document navigation, which is what the operator's click does and what
  // keeps the view mounted. A full `goto` would remount it and prove nothing.
  stall = true;
  await page.evaluate(() => {
    window.location.hash = "#/team/ravi";
  });

  // Ravi's page is ready…
  await expect(page.getByTestId("agent-name")).toHaveText("Ravi", { timeout: 30_000 });

  // …and carries no reading of the board at all, rather than Maya's. Before the
  // fix the previous teammate's task links and workload survived here, because
  // the effect cleared them only when `company` went falsy — so Ravi rendered
  // with links to cards that are not his, for as long as the board read took.
  await expect(page.getByTestId("agent-open-tasks")).toHaveCount(0);
  await expect(page.getByTestId("agent-open-task-t1")).toHaveCount(0);
  await expect(page.getByTestId("agent-open-task-t2")).toHaveCount(0);
  await expect(page.getByTestId("agent-tasks")).toHaveCount(0);
});
