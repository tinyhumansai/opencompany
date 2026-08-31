import { expect, test, type Page } from "@playwright/test";

/**
 * End-to-end proof for issue #311 — the company's structure is editable from
 * the console again, through the org chart rather than the removed Desks page.
 *
 * # The failure this reproduces
 *
 * #302 closed the way in to desk management: "desk creation and membership
 * editing become unreachable ... editable by hand in the manifest and nowhere
 * else." The chat rebuild then went further than #302 described — `"desks"`
 * left the `View` union entirely, so `DesksView` and `DeskCreateDialog` were
 * imported by nothing and `#/desks` silently rewrote to Overview. Five host
 * routes (`POST /desks`, `DELETE /desks/{id}`, `POST /desks/{id}/members`,
 * `DELETE /desks/{id}/members/{agent}`, `PUT /desks/{id}/order`) were reachable
 * from no UI by any path, including by typing a URL.
 *
 * Every test below fails on the parent commit, and the first one fails in the
 * bluntest possible way: there is no Company nav item and `#/company` resolves
 * to Overview.
 *
 * # Why this mocks the operator API
 *
 * Like `chat-channel-membership.spec.ts` beside it. The interesting inputs are
 * a blueprint desk next to an operator-created one, an overlay member next to a
 * founding one, and a desk seat naming somebody the roster does not have — none
 * of which a default harness company produces on demand. The stub is
 * **stateful**: a write mutates the desks it will serve on the next read, which
 * is what makes the reload assertions mean "the host kept it" rather than "the
 * component remembered it". The page still comes from whatever `PW_BASE_URL`
 * serves, like the rest of the suite.
 *
 * No `LIVE_BRAIN` gate: nothing here needs inference, so this runs in the
 * default-feature lane.
 */

const COMPANY = "acme";

interface Desk {
  id: string;
  name: string;
  description?: string;
  members: string[];
  overlayMembers?: string[];
  overlayCreated?: boolean;
}

const ROSTER = [
  { id: "ada", name: "Ada", role: "Backend Engineer" },
  { id: "grace", name: "Grace", role: "Frontend Engineer" },
  { id: "linus", name: "Linus", role: "QA Engineer" },
  { id: "hedy", name: "Hedy", role: "Designer" },
  // On no desk, deliberately: a roster the chart cannot place is the case
  // that would silently vanish from a tree built only from desk membership.
  { id: "turing", name: "Turing", role: "Researcher" },
];

/**
 * The toast layer (issue #1099).
 *
 * Since #1099 an *action* on this page answers in a toast; the `role="alert"`
 * banner is left to a chart that could not be loaded. Located by sonner's own
 * attribute, like `toast-dismissal.spec.ts` — the toast carries `role="status"`,
 * which several live regions on this page share.
 */
const toasts = (page: Page) => page.locator("[data-sonner-toast]");

/** Every write the console made, so the request shape can be asserted on. */
let writes: { method: string; path: string; body?: unknown }[] = [];
let roster = [...ROSTER];
let teamWriteAvailable = true;

/**
 * Makes `POST .../desks/{id}/members` refuse, so the partial failure is
 * reachable: the teammate is created on the host and then cannot be placed.
 */
let deskAddAvailable = true;

/**
 * Makes `GET .../desks` fail from the moment a teammate is created, so the
 * create-landed-then-read-back-failed case is reachable. The write still
 * succeeds — that is the point: the host has the teammate and the console
 * cannot see them.
 */
let desksReadFailsAfterCreate = false;
let desksReadable = true;

/** The host's desks, as this stub holds them. Mutated by the write routes. */
let desks: Desk[] = [];

function reset() {
  writes = [];
  roster = [...ROSTER];
  teamWriteAvailable = true;
  deskAddAvailable = true;
  desksReadFailsAfterCreate = false;
  desksReadable = true;
  desks = [
    {
      id: "engineering",
      name: "Engineering",
      description: "Ships the product",
      // Lead first, and NOT roster order: a fix that sorted would crown Ada.
      members: ["grace", "ada"],
    },
    {
      id: "growth",
      name: "Growth",
      // `linus` founded it, `hedy` was added later — two provenances, one desk.
      members: ["linus", "hedy"],
      overlayMembers: ["hedy"],
      overlayCreated: true,
    },
  ];
}

/**
 * Stub the operator API, holding desk state across requests so a reload reads
 * back what a write left behind.
 */
async function mockApi(page: Page) {
  // The first-run product tour renders a modal over everything and swallows
  // clicks beneath it. Answer "already skipped" for any company id.
  await page.addInitScript(() => {
    const real = Storage.prototype.getItem;
    Storage.prototype.getItem = function getItem(key: string) {
      return key.startsWith("oc-tour:")
        ? '{"skipped":true}'
        : real.call(this, key);
    };
  });

  await page.route("**/api/v1/**", async (route) => {
    const request = route.request();
    const method = request.method();
    const path = new URL(request.url()).pathname;
    const json = (body: unknown, status = 200) =>
      route.fulfill({
        status,
        contentType: "application/json",
        body: JSON.stringify(body),
      });
    const noContent = () => route.fulfill({ status: 204, body: "" });
    const status = {
      id: COMPANY,
      name: "Acme",
      lifecycle: "running",
      pending_approvals: 0,
    };

    if (path === "/api/v1/companies") return json([status]);
    if (path === `/api/v1/companies/${COMPANY}`) return json(status);

    const desk = (id: string) => desks.find((d) => d.id === id);

    // PUT .../desks/{id}/order — the operator's member order; index 0 is lead.
    const order = path.match(/\/desks\/([^/]+)\/order$/);
    if (order && method === "PUT") {
      // snake_case on the wire: the host's `SetDeskOrder` carries no
      // `rename_all`, so the console posts `ordered_member_ids`.
      const body = request.postDataJSON() as { ordered_member_ids?: string[] };
      writes.push({ method, path, body });
      const target = desk(order[1]);
      if (target && body.ordered_member_ids)
        target.members = [...body.ordered_member_ids];
      return noContent();
    }

    // DELETE .../desks/{id}/members/{agent}
    const member = path.match(/\/desks\/([^/]+)\/members\/([^/]+)$/);
    if (member && method === "DELETE") {
      const target = desk(member[1]);
      // A manifest-declared (blueprint) member cannot be let go at runtime —
      // matches the real host, per `client.ts`'s own doc on `removeDeskMember`.
      // Issue #1227's cross-desk move relies on this refusal never being
      // silent, so the stub has to actually produce it.
      if (target && !(target.overlayMembers ?? []).includes(member[2])) {
        writes.push({ method, path });
        return json({ error: "blueprint member cannot be removed" }, 409);
      }
      writes.push({ method, path });
      if (target) {
        target.members = target.members.filter((m) => m !== member[2]);
        target.overlayMembers = (target.overlayMembers ?? []).filter(
          (m) => m !== member[2],
        );
      }
      return noContent();
    }

    // POST .../desks/{id}/members
    const members = path.match(/\/desks\/([^/]+)\/members$/);
    if (members && method === "POST") {
      // snake_case on the wire, as above (`AddDeskMember { agent_id }`).
      const body = request.postDataJSON() as { agent_id: string };
      if (!deskAddAvailable) {
        writes.push({ method, path, body });
        return json({ error: "the desk refused the member" }, 409);
      }
      writes.push({ method, path, body });
      const target = desk(members[1]);
      if (target) {
        target.members = [...target.members, body.agent_id];
        target.overlayMembers = [
          ...(target.overlayMembers ?? []),
          body.agent_id,
        ];
      }
      return noContent();
    }

    // DELETE .../desks/{id}
    const one = path.match(/\/desks\/([^/]+)$/);
    if (one && method === "DELETE") {
      writes.push({ method, path });
      desks = desks.filter((d) => d.id !== one[1]);
      return noContent();
    }

    if (path.endsWith("/desks")) {
      if (method === "POST") {
        const body = request.postDataJSON() as {
          name: string;
          description?: string;
          members?: string[];
        };
        writes.push({ method, path, body });
        const created: Desk = {
          id: body.name.toLowerCase().replace(/[^a-z0-9]+/g, "_"),
          name: body.name,
          description: body.description,
          members: body.members ?? [],
          overlayCreated: true,
        };
        desks = [...desks, created];
        return json(created, 201);
      }
      if (!desksReadable) return json({ error: "unavailable" }, 500);
      return json(desks);
    }

    if (path.endsWith("/team") && method === "POST") {
      if (!teamWriteAvailable) return json({ error: "not supported" }, 404);
      const body = request.postDataJSON() as {
        name: string;
        role: string;
        description?: string;
      };
      const created = {
        id: `new-${roster.length}`,
        name: body.name,
        role: body.role,
        description: body.description,
      };
      roster = [...roster, created];
      writes.push({ method, path, body });
      if (desksReadFailsAfterCreate) desksReadable = false;
      return json(created, 201);
    }
    if (path.endsWith("/team")) return json(roster);
    // GET .../team/{agentId} — the detail page the chart's teammate links open
    // (issue #1102). Stubbed so following one lands on a real screen rather
    // than on the catch-all's `[]`, which is an agent with none of its fields.
    const agent = path.match(/\/team\/([^/]+)$/);
    if (agent && method === "GET") {
      const found = roster.find((m) => m.id === agent[1]);
      if (!found) return json({ error: "no such teammate" }, 404);
      return json({
        ...found,
        source: "manifest",
        editable: [],
        isOrchestrator: false,
        tools: { requested: [], companyAllow: [], deskAllow: [], deskCeilingActive: false, effective: [] },
        desks: [],
        inboxEnabled: false,
      });
    }
    if (path.endsWith("/users")) {
      return json([
        {
          id: "u1",
          email: "op@example.com",
          displayName: "Operator",
          role: "admin",
          status: "active",
          hasPassword: true,
          mustChangePassword: false,
          createdAtMillis: 0,
        },
      ]);
    }
    if (path.endsWith("/events")) {
      return route.fulfill({
        status: 200,
        contentType: "text/event-stream",
        body: "",
      });
    }
    if (path.endsWith("/memory"))
      // `GET /memory` answers with `{ items, totalContext, contextTruncated }`
      // — the Overview's constellation reads the rows from `items`, and a bare
      // array would leave it `undefined` and crash the graph render.
      return json({ items: [], totalContext: 0, contextTruncated: false });
    if (path.endsWith("/me"))
      return json({ id: "op", email: "op@example.com", role: "admin" });
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
    return json([]);
  });
}

const chart = (page: Page) =>
  page.getByRole("tree", { name: "Company org chart" });

/** A desk node by its heading text; `aria-level` 2 keeps seats from matching. */
const deskNode = (page: Page, name: string) =>
  chart(page)
    .locator('[role="treeitem"][aria-level="2"]')
    .filter({ hasText: name });

/**
 * Land on the chart from a cold load.
 *
 * Always the test's **first** navigation, so the document loads at `#/company`
 * rather than changing only its fragment. A `goto` between two hashes of the
 * same document is a same-document navigation the SPA may or may not have
 * routed by the time the assertion runs — see the reachability test below,
 * which needs a second navigation and therefore clicks instead.
 */
async function openChart(page: Page) {
  // The chart has an address of its own since #1193 — it is a destination under
  // the Company page, not a mode of it, so it survives a reload and can be
  // linked. `#/company` is the roster.
  await page.goto("/#/company/desks");
  await expect(chart(page)).toBeVisible({ timeout: 30_000 });
}

/**
 * The chat member pane — the channel rail is the other `complementary` on
 * screen, so the pane is always the last one.
 */
const memberPane = (page: Page) => page.getByRole("complementary").last();

/**
 * Open the chat member pane if it is shut.
 *
 * Its open state lives in `ChatView`, which unmounts when the operator steps
 * into another view — so this has to be re-run after a round trip through the
 * chart, and a blind click would close what a previous call opened.
 */
async function openMemberPane(page: Page) {
  const toggle = page.getByRole("button", { name: /teammates$/i });
  if ((await toggle.getAttribute("aria-pressed")) !== "true")
    await toggle.click();
  await expect(page.getByRole("heading", { name: "Team" })).toBeVisible();
}

test.beforeEach(reset);

test("#311 the org chart is reachable, which it was not before", async ({
  page,
}) => {
  await mockApi(page);

  // Start somewhere else, so arriving at the chart is a real navigation and not
  // the page it happened to load on. Before this change there was no nav entry
  // and no hash that reached this surface at all.
  await page.goto("/#/overview");
  // `exact`, because the sidebar header's host switcher is a button too and its
  // accessible name ends "… Current company" (#1142). Without it the substring
  // match picks the switcher — which is above the nav — and clicking it opens a
  // menu instead of routing.
  const nav = page
    .getByRole("link", { name: "Company", exact: true })
    .or(page.getByRole("button", { name: "Company", exact: true }))
    .first();
  await expect(nav).toBeVisible({ timeout: 30_000 });

  // Click it rather than `goto` a second hash. Two reasons, and the first is
  // why this test flaked once before: `page.goto` from `#/overview` to
  // `#/company` changes only the fragment, so the document never reloads and
  // the assertion can run against the view still on screen — a captured
  // failure snapshot showed the knowledge graph, not the chart. Clicking is
  // also the stronger claim: it proves the nav entry *routes*, which typing a
  // URL does not.
  await nav.click();
  // Cards first (issue #1141): the nav entry lands on the teammates. The chart
  // is one named action away rather than gone — "Manage desks", because desk
  // management is what it is for (issue #1193).
  await expect(page.getByTestId("team-card").first()).toBeVisible({ timeout: 30_000 });
  await page.getByTestId("company-manage-desks").click();
  await expect(chart(page)).toBeVisible({ timeout: 30_000 });

  // And the hash names where we are rather than being rewritten to the
  // fallback view, which is exactly what `#/desks` does — it names no view.
  await expect.poll(() => page.url()).toContain("#/company/desks");
});

test("#311 the chart is three levels and never a fourth", async ({ page }) => {
  await mockApi(page);
  await openChart(page);

  // Level 1 is the company itself, named and counted.
  const root = chart(page).locator('[role="treeitem"][aria-level="1"]');
  await expect(root).toHaveCount(1);
  await expect(root).toContainText("Acme");
  await expect(root).toContainText("2 desks · 4 seats");

  // Level 2 is the desks, level 3 the seats on them.
  await expect(
    chart(page).locator('[role="treeitem"][aria-level="2"]'),
  ).toHaveCount(2);
  await expect(
    chart(page).locator('[role="treeitem"][aria-level="3"]'),
  ).toHaveCount(4);

  // And there is no fourth. The cap is structural: no desk can name a parent
  // desk, so nothing here can emit one.
  await expect(
    chart(page).locator('[role="treeitem"][aria-level="4"]'),
  ).toHaveCount(0);
});

test("#311 the lead is the desk's first member, badged", async ({ page }) => {
  await mockApi(page);
  await openChart(page);

  const seats = deskNode(page, "Engineering").locator(
    '[role="treeitem"][aria-level="3"]',
  );
  await expect(seats).toHaveCount(2);
  await expect(seats.first()).toContainText("Grace");
  // Through the role, not the bare attribute: a `getByLabel` match proves the
  // attribute is in the DOM, not that anything would announce it. The crown
  // carries `role="img"` so the accessible name actually resolves.
  await expect(
    seats.first().getByRole("img", { name: "Desk lead" }),
  ).toBeVisible();
  await expect(seats.nth(1)).toContainText("Ada");
  await expect(
    seats.nth(1).getByRole("img", { name: "Desk lead" }),
  ).toHaveCount(0);
});

test("#311 a desk can be created from the chart and survives a reload", async ({
  page,
}) => {
  await mockApi(page);
  await openChart(page);

  await page.getByRole("button", { name: "New desk" }).click();
  const dialog = page.getByRole("dialog");
  await dialog.getByLabel("Name").fill("Support");
  await dialog.getByLabel("Description").fill("Answers customers");
  // Scoped to the dialog: the chart behind it has "Move Linus up in ..."
  // buttons, so an unscoped name match would be ambiguous.
  // The first member picked leads the new desk.
  await dialog.getByRole("button", { name: /Linus/ }).click();
  await dialog.getByRole("button", { name: "Create desk" }).click();

  await expect(deskNode(page, "Support")).toBeVisible();

  const created = writes.find(
    (w) => w.method === "POST" && w.path.endsWith("/desks"),
  );
  expect(created?.body).toMatchObject({ name: "Support", members: ["linus"] });

  // The proof that this is the host's and not the component's: reload.
  await page.reload();
  await expect(chart(page)).toBeVisible({ timeout: 30_000 });
  await expect(deskNode(page, "Support")).toBeVisible();
  await expect(deskNode(page, "Support")).toContainText("Linus");
});

test("#311 membership can be edited from the chart and survives a reload", async ({
  page,
}) => {
  await mockApi(page);
  await openChart(page);

  // Turing sits on no desk, so the chart accounts for him beside the tree
  // rather than dropping him.
  await expect(page.getByRole("heading", { name: "Desks", level: 1 })).toBeVisible();
  await expect(page.getByRole("heading", { name: "People outside desks", level: 2 })).toBeAttached();
  const unplaced = page.getByRole("heading", { name: "Not on a desk", level: 3 });
  await expect(unplaced).toBeVisible();
  await expect(
    unplaced.locator("xpath=following-sibling::ul[1]"),
  ).toContainText("Turing");

  const engineering = deskNode(page, "Engineering");
  await engineering.getByRole("button", { name: "Add teammate" }).click();
  await page.getByRole("menuitem", { name: "Linus" }).click();

  await expect(
    engineering.locator('[role="treeitem"][aria-level="3"]'),
  ).toHaveCount(3);
  expect(
    writes.some(
      (w) =>
        w.method === "POST" && w.path.endsWith("/desks/engineering/members"),
    ),
  ).toBe(true);

  await page.reload();
  await expect(chart(page)).toBeVisible({ timeout: 30_000 });
  await expect(deskNode(page, "Engineering")).toContainText("Linus");

  // And removing an operator-added member takes it back off.
  await deskNode(page, "Engineering")
    .getByRole("button", { name: "Remove Linus from Engineering" })
    .click();
  await expect(
    deskNode(page, "Engineering").locator('[role="treeitem"][aria-level="3"]'),
  ).toHaveCount(2);
  await page.reload();
  await expect(chart(page)).toBeVisible({ timeout: 30_000 });
  await expect(deskNode(page, "Engineering")).not.toContainText("Linus");
});

test("#839 creates a teammate on a selected desk and persists it", async ({
  page,
}) => {
  await mockApi(page);
  await openChart(page);

  const growth = deskNode(page, "Growth");
  // One control per desk now, and "New teammate…" is an item inside its menu
  // rather than an unlabelled icon button beside it.
  await growth.getByRole("button", { name: "Add teammate" }).click();
  await page
    .getByRole("menuitem", { name: "Add teammate to Growth" })
    .click();
  const dialog = page.getByRole("dialog");
  await dialog.getByLabel("Name").fill("Babbage");
  await dialog.getByLabel("Role").fill("Platform Engineer");
  await dialog.getByLabel("What they do").fill("Builds the platform");
  await dialog.getByRole("button", { name: "Add teammate" }).click();

  await expect(growth).toContainText("Babbage");
  expect(
    writes.find(
      (write) => write.method === "POST" && write.path.endsWith("/team"),
    ),
  ).toBeTruthy();
  expect(
    writes.find(
      (write) =>
        write.method === "POST" && write.path.endsWith("/desks/growth/members"),
    ),
  ).toBeTruthy();

  await page.reload();
  await expect(chart(page)).toBeVisible({ timeout: 30_000 });
  await expect(deskNode(page, "Growth")).toContainText("Babbage");
});

test("#839 creates a teammate with no desk as unplaced", async ({ page }) => {
  await mockApi(page);
  await openChart(page);

  await page.getByRole("button", { name: "Add teammate" }).first().click();
  const dialog = page.getByRole("dialog");
  await dialog.getByLabel("Name").fill("No Desk");
  await dialog.getByLabel("Role").fill("Roaming Engineer");
  await dialog.getByRole("button", { name: "Add teammate" }).click();

  await expect(
    page.getByRole("heading", { name: "Not on a desk" }),
  ).toContainText("Not on a desk");
  await expect(
    page
      .getByRole("heading", { name: "Not on a desk" })
      .locator("xpath=following-sibling::ul[1]")
      .getByRole("link", { name: "No Desk", exact: true }),
  ).toBeVisible();
  expect(writes.some((write) => write.path.includes("/members"))).toBe(false);
});

test("#839 refuses a company-page teammate add when the host has no team write plane", async ({
  page,
}) => {
  teamWriteAvailable = false;
  await mockApi(page);
  await openChart(page);

  await page.getByRole("button", { name: "Add teammate" }).first().click();
  const dialog = page.getByRole("dialog");
  await dialog.getByLabel("Name").fill("Not Saved");
  await dialog.getByLabel("Role").fill("Unavailable");
  await dialog.getByRole("button", { name: "Add teammate" }).click();

  await expect(toasts(page)).toContainText("can't create teammates");
  await expect(chart(page).locator("text=Not Saved")).toHaveCount(0);
});

test("#1099 a teammate added from the company page is confirmed by name", async ({
  page,
}) => {
  await mockApi(page);
  await openChart(page);

  await page.getByRole("button", { name: "Add teammate" }).first().click();
  const dialog = page.getByRole("dialog");
  await dialog.getByLabel("Name").fill("Katherine");
  await dialog.getByLabel("Role").fill("Navigator");
  await dialog.getByRole("button", { name: "Add teammate" }).click();

  // The whole of #1099 on this surface: the operator is told, by name, rather
  // than left to infer the add from a chart that repaints a moment later.
  await expect(toasts(page)).toContainText("Added Katherine.");
  // And it is a *success*, not the warning a half-landed add gets — asserted
  // through sonner's own type attribute so the two cannot be confused by
  // wording alone.
  await expect(toasts(page).first()).toHaveAttribute("data-type", "success");
});

test("#1099 a teammate the chart cannot read back is not confirmed as added", async ({
  page,
}) => {
  // The host takes the teammate and then the chart's own read fails. `boot`
  // swallows that — it has to, the chart has an error state and a Retry — so
  // without the check the console toasted "Added Grace Murray." over a banner
  // saying the chart could not be loaded.
  desksReadFailsAfterCreate = true;
  await mockApi(page);
  await openChart(page);

  await page.getByRole("button", { name: "Add teammate" }).first().click();
  const dialog = page.getByRole("dialog");
  await dialog.getByLabel("Name").fill("Grace Murray");
  await dialog.getByLabel("Role").fill("Compiler");
  await dialog.getByRole("button", { name: "Add teammate" }).click();

  const notice = toasts(page).first();
  await expect(notice).toContainText("Added Grace Murray, but");
  await expect(notice).toContainText("chart couldn't be read back");
  await expect(notice).toHaveAttribute("data-type", "warning");
  // The teeth: nothing anywhere claims the clean add. `Added Grace Murray.`
  // with a full stop is the exact string the success arm produces.
  await expect(page.getByText("Added Grace Murray.", { exact: true })).toHaveCount(0);
  // And the write really did land, so this is the honest half-landing rather
  // than a failure being reported as one.
  expect(
    writes.find(
      (write) => write.method === "POST" && write.path.endsWith("/team"),
    ),
  ).toBeTruthy();
});

test("#311 the lead can be changed from the chart and survives a reload", async ({
  page,
}) => {
  await mockApi(page);
  await openChart(page);

  // Moving the second seat up is the change of lead — `members[0]` is the lead
  // and there is no separate call to make.
  await deskNode(page, "Engineering")
    .getByRole("button", { name: "Move Ada up in Engineering" })
    .click();

  const seats = deskNode(page, "Engineering").locator(
    '[role="treeitem"][aria-level="3"]',
  );
  await expect(seats.first()).toContainText("Ada");
  await expect(
    seats.first().getByRole("img", { name: "Desk lead" }),
  ).toBeVisible();

  const ordered = writes.find(
    (w) => w.method === "PUT" && w.path.endsWith("/order"),
  );
  expect(ordered?.body).toEqual({ ordered_member_ids: ["ada", "grace"] });

  await page.reload();
  await expect(chart(page)).toBeVisible({ timeout: 30_000 });
  await expect(
    deskNode(page, "Engineering")
      .locator('[role="treeitem"][aria-level="3"]')
      .first(),
  ).toContainText("Ada");
});

test("a desk offers one add control, and it stays usable when the roster is exhausted", async ({
  page,
}) => {
  await mockApi(page);
  await openChart(page);

  const engineering = deskNode(page, "Engineering");

  // One control per desk. It used to be two: this button, and — flush against
  // it, unlabelled — a `UserPlus` icon that created a teammate here. The icon
  // wore the same glyph as the page header's "New teammate", touching a button
  // that already said the words, so the create path was invisible.
  await expect(engineering.getByRole("button", { name: /teammate/i })).toHaveCount(1);

  // Seat everyone the roster has left, through the menu.
  for (const name of ["Linus", "Hedy", "Turing"]) {
    await engineering.getByRole("button", { name: "Add teammate" }).click();
    await page.getByRole("menuitem", { name }).click();
    await expect(engineering).toContainText(name);
  }

  // Nobody is left to seat — and the control is still live, because creating a
  // teammate here is still something an operator can do. It used to go
  // disabled and read "Everyone is on this desk", which left the unlabelled
  // icon as the only way in.
  const add = engineering.getByRole("button", { name: "Add teammate" });
  await expect(add).toBeEnabled();
  await add.click();

  const menu = page.getByRole("menu");
  await expect(menu).toContainText("Everyone on the roster is already here.");
  await expect(
    menu.getByRole("menuitem", { name: "Add teammate to Engineering" }),
  ).toBeVisible();
});

test("#839 a teammate created but not placed is still on the chart to place by hand", async ({
  page,
}) => {
  // The half-done case: the host takes the teammate and then refuses the desk.
  // The message tells the operator to place them themselves, so the chart has
  // to have re-read — otherwise it points at a dropdown the teammate is not in
  // yet, and the only way out is an unrelated reload.
  deskAddAvailable = false;
  await mockApi(page);
  await openChart(page);

  const growth = deskNode(page, "Growth");
  // One control per desk now, and "New teammate…" is an item inside its menu
  // rather than an unlabelled icon button beside it.
  await growth.getByRole("button", { name: "Add teammate" }).click();
  await page
    .getByRole("menuitem", { name: "Add teammate to Growth" })
    .click();
  const dialog = page.getByRole("dialog");
  await dialog.getByLabel("Name").fill("Hopper");
  await dialog.getByLabel("Role").fill("Compiler");
  await dialog.getByRole("button", { name: "Add teammate" }).click();

  const halfLanded = toasts(page).first();
  await expect(halfLanded).toContainText("couldn't be added to that desk");
  await expect(halfLanded).toContainText("Hopper");
  // Not dressed as a success (#1099): a teammate the host took but could not
  // place is exactly the outcome "Added Hopper." would have lied about.
  await expect(halfLanded).toHaveAttribute("data-type", "warning");
  // Created on the host, so it must be on the chart's unplaced list — the one
  // place a teammate with no desk belongs, and where the operator picks them
  // up to place by hand. Asserted against that list rather than the page: the
  // alert names Hopper too, so a page-wide match would pass whether or not the
  // chart had re-read at all.
  const unplaced = page
    .locator("section", {
      has: page.getByRole("heading", { name: "Not on a desk" }),
    })
    .last();
  await expect(unplaced).toContainText("Hopper");
  expect(
    writes.find(
      (write) => write.method === "POST" && write.path.endsWith("/team"),
    ),
  ).toBeTruthy();
});

test("#839 dragging a seat reorders the desk and persists the new lead", async ({
  page,
}) => {
  await mockApi(page);
  await openChart(page);

  const engineering = deskNode(page, "Engineering");
  const seats = engineering.locator('[role="treeitem"][aria-level="3"]');
  await seats.nth(1).dragTo(seats.first());

  await expect(seats.first()).toContainText("Ada");
  expect(
    writes.find(
      (write) => write.method === "PUT" && write.path.endsWith("/order"),
    )?.body,
  ).toEqual({
    ordered_member_ids: ["ada", "grace"],
  });

  await page.reload();
  await expect(chart(page)).toBeVisible({ timeout: 30_000 });
  const reloadedSeats = deskNode(page, "Engineering").locator(
    '[role="treeitem"][aria-level="3"]',
  );
  await expect(reloadedSeats.first()).toContainText("Ada");
  await expect(
    reloadedSeats.first().getByRole("img", { name: "Desk lead" }),
  ).toBeVisible();
  await expect(reloadedSeats.nth(1)).toContainText("Grace");
});

test("#1227 dragging a seat across desks moves it, and persists", async ({
  page,
}) => {
  // The org chart's own subtitle promises "move someone between desks" — this
  // is the drag the subtitle was lying about before the fix: same gesture as
  // same-desk reorder (`dragTo`, real `dragstart`/`dragover`/`drop`), just
  // crossing a desk boundary. Hedy is Growth's *overlay* member, so the host
  // will let her go.
  await mockApi(page);
  await openChart(page);

  const growth = deskNode(page, "Growth");
  const growthSeats = growth.locator('[role="treeitem"][aria-level="3"]');
  const engineering = deskNode(page, "Engineering");
  const engineeringSeats = engineering.locator(
    '[role="treeitem"][aria-level="3"]',
  );
  await expect(growthSeats).toHaveCount(2);
  await expect(engineeringSeats).toHaveCount(2);

  await growthSeats.filter({ hasText: "Hedy" }).dragTo(engineeringSeats.first());

  // Landed: gone from Growth, present on Engineering.
  await expect(growth.getByText("Hedy")).toHaveCount(0);
  await expect(engineering.getByText("Hedy")).toBeVisible();
  await expect(toasts(page).filter({ hasText: /error|fail|wrong/i })).toHaveCount(0);

  // Add-then-remove, in that order — nothing invented beyond the host's own
  // two verbs (issue #1227's "what a fix would be").
  expect(
    writes.find(
      (w) => w.method === "POST" && w.path === "/api/v1/companies/acme/desks/engineering/members",
    )?.body,
  ).toEqual({ agent_id: "hedy" });
  expect(
    writes.some(
      (w) =>
        w.method === "DELETE" &&
        w.path === "/api/v1/companies/acme/desks/growth/members/hedy",
    ),
  ).toBe(true);

  await page.reload();
  await expect(chart(page)).toBeVisible({ timeout: 30_000 });
  await expect(deskNode(page, "Growth").getByText("Hedy")).toHaveCount(0);
  await expect(deskNode(page, "Engineering").getByText("Hedy")).toBeVisible();
});

test("#1227 dragging a blueprint seat across desks is refused, visibly", async ({
  page,
}) => {
  // Linus is Growth's *blueprint* founder — the manifest still declares him
  // there, and the host refuses to remove a blueprint member from its desk
  // (simulated above as a 409). Before the fix this drag was a total silent
  // no-op; the fix is refusing it visibly, not making it work — the host
  // invariant is real, not a frontend bug.
  await mockApi(page);
  await openChart(page);

  const growth = deskNode(page, "Growth");
  const growthSeats = growth.locator('[role="treeitem"][aria-level="3"]');
  const engineering = deskNode(page, "Engineering");
  const engineeringSeats = engineering.locator(
    '[role="treeitem"][aria-level="3"]',
  );

  await growthSeats
    .filter({ hasText: "Linus" })
    .dragTo(engineeringSeats.first());

  // Nothing moved.
  await expect(growth.getByText("Linus")).toBeVisible();
  await expect(engineering.getByText("Linus")).toHaveCount(0);
  // And nothing was silent about it: a toast named the reason.
  await expect(toasts(page).filter({ hasText: /blueprint/i })).toBeVisible();
  // Never even asked the host to do what it would refuse.
  expect(
    writes.some(
      (w) =>
        w.path === "/api/v1/companies/acme/desks/growth/members/linus" ||
        (w.method === "POST" &&
          w.path === "/api/v1/companies/acme/desks/engineering/members"),
    ),
  ).toBe(false);

  await page.reload();
  await expect(chart(page)).toBeVisible({ timeout: 30_000 });
  await expect(deskNode(page, "Growth").getByText("Linus")).toBeVisible();
  await expect(deskNode(page, "Engineering").getByText("Linus")).toHaveCount(0);
});

test("#311 blueprint structure offers no control the host would refuse", async ({
  page,
}) => {
  await mockApi(page);
  await openChart(page);

  // A manifest desk cannot be deleted at runtime, so no delete is offered.
  // Its blueprint provenance is a muted lock, not a word badge — the badge only
  // survives on a mixed-provenance desk, where it distinguishes one member from
  // the runtime-added one beside it.
  await expect(
    deskNode(page, "Engineering")
      .getByRole("img", { name: "Part of the company blueprint" })
      .first(),
  ).toBeVisible();
  await expect(
    deskNode(page, "Engineering").getByRole("button", {
      name: "Delete Engineering",
    }),
  ).toHaveCount(0);
  // Nor can its manifest members be removed.
  await expect(
    deskNode(page, "Engineering").getByRole("button", { name: /^Remove / }),
  ).toHaveCount(0);

  // An operator-created desk can be deleted, and its added member removed —
  // but its founding member is still blueprint to the host.
  const growth = deskNode(page, "Growth");
  await expect(
    growth.getByRole("button", { name: "Delete Growth" }),
  ).toBeVisible();
  await expect(
    growth.getByRole("button", { name: "Remove Hedy from Growth" }),
  ).toBeVisible();
  await expect(
    growth.getByRole("button", { name: "Remove Linus from Growth" }),
  ).toHaveCount(0);

  await growth.getByRole("button", { name: "Delete Growth" }).click();
  await expect(deskNode(page, "Growth")).toHaveCount(0);
  await page.reload();
  await expect(chart(page)).toBeVisible({ timeout: 30_000 });
  await expect(deskNode(page, "Growth")).toHaveCount(0);
});

test("#485 #/company/<deskId> lands on that desk, not just on the chart", async ({
  page,
}) => {
  await mockApi(page);

  // The chat member pane's "Manage on the org chart" writes exactly this
  // address. Before #485 the shell dropped the hash's second segment on the
  // way into this view, so the chart had no per-desk address at all.
  await page.goto("/#/company/growth");
  await expect(chart(page)).toBeVisible({ timeout: 30_000 });

  const growth = deskNode(page, "Growth");
  await expect(growth).toHaveAttribute("data-desk-focused", "true");
  // Focus, not only a ring: a ring says nothing to a screen reader, and "where
  // did that link put me" is the question focus answers.
  await expect(growth).toBeFocused();
  // Nobody else is marked — arriving at one desk must not light up the chart.
  await expect(chart(page).locator("[data-desk-focused]")).toHaveCount(1);

  // The address survives, so the link the operator followed stays shareable.
  await expect.poll(() => page.url()).toContain("#/company/growth");

  // And the ring is not sticky: it clears on the first move rather than on a
  // timer nobody can pick correctly against a chart that loads over a network.
  await page.keyboard.press("Escape");
  await expect(growth).not.toHaveAttribute("data-desk-focused", "true");
});

test("#485 a desk id the chart doesn't have is a silent no-op", async ({
  page,
}) => {
  await mockApi(page);

  // `useHashView` hands the second segment back unvalidated, and a link can
  // outlive the desk it names. A stale bookmark gets the company, not a banner.
  await page.goto("/#/company/deleted-last-week");
  await expect(chart(page)).toBeVisible({ timeout: 30_000 });
  await expect(
    chart(page).locator('[role="treeitem"][aria-level="2"]'),
  ).toHaveCount(2);
  await expect(chart(page).locator("[data-desk-focused]")).toHaveCount(0);
  await expect(page.getByRole("alert")).toHaveCount(0);
  // Not rewritten either: only this view knows which desk ids exist, so the
  // shell has no business canonicalising a segment it cannot judge.
  await expect.poll(() => page.url()).toContain("#/company/deleted-last-week");

  // The teeth on the assertions above: focus is not simply broken. Point the
  // same chart at a desk it *does* have and the marker appears.
  await page.goto("/#/company/engineering");
  await expect
    .poll(
      () => deskNode(page, "Engineering").getAttribute("data-desk-focused"),
      {
        timeout: 30_000,
      },
    )
    .toBe("true");
});

test("#485 following the same desk link twice still lands on it", async ({
  page,
}) => {
  await mockApi(page);

  // Every hop here is an in-app hash change, never `page.goto`: a full
  // navigation remounts the chart and resets the very state this pins, so it
  // would pass whether or not the bug exists.
  await page.goto("/#/company/growth");
  await expect(chart(page)).toBeVisible({ timeout: 30_000 });
  await expect(deskNode(page, "Growth")).toHaveAttribute(
    "data-desk-focused",
    "true",
  );

  // Off to the bare chart — `#/company/desks` since #1193, because plain
  // `#/company` is the roster now and would unmount this view rather than leave
  // it holding what it remembers. The view stays mounted, so whatever it
  // remembers about the last honoured id survives.
  await page.evaluate(() => {
    window.location.hash = "#/company/desks";
  });
  await expect(chart(page)).toBeVisible();
  // The previous desk must not keep wearing the ring once it is no longer the
  // route's target.
  await expect(chart(page).locator("[data-desk-focused]")).toHaveCount(0);

  // Back to the same desk. This is the case that regressed: the id matched the
  // one already honoured, so the arrival was skipped and the link did nothing.
  await page.evaluate(() => {
    window.location.hash = "#/company/growth";
  });
  const growth = deskNode(page, "Growth");
  await expect(growth).toHaveAttribute("data-desk-focused", "true");
  await expect(growth).toBeFocused();
  await expect(chart(page).locator("[data-desk-focused]")).toHaveCount(1);
});

test("#485 a stale id does not leave the previous desk wearing the ring", async ({
  page,
}) => {
  await mockApi(page);

  await page.goto("/#/company/growth");
  await expect(chart(page)).toBeVisible({ timeout: 30_000 });
  await expect(deskNode(page, "Growth")).toHaveAttribute(
    "data-desk-focused",
    "true",
  );

  // An id the chart does not have is a no-op for *arrival*, but it must still
  // retire the previous target's mark — otherwise the ring claims a desk the
  // address no longer names.
  await page.evaluate(() => {
    window.location.hash = "#/company/deleted-last-week";
  });
  await expect(chart(page)).toBeVisible();
  await expect(chart(page).locator("[data-desk-focused]")).toHaveCount(0);
});

test("#485 a membership edit on the chart is there when you get back to chat", async ({
  page,
}) => {
  await mockApi(page);

  // The round trip #485 is actually for: read the desk in chat, notice it is
  // short a person, fix that where it can be fixed, come back.
  await page.goto("/#/chat/engineering");
  await expect(page.getByPlaceholder("Message #engineering")).toBeVisible({
    timeout: 30_000,
  });
  await openMemberPane(page);
  await expect(memberPane(page)).toContainText("Grace");
  await expect(memberPane(page).locator("ul").first()).not.toContainText(
    "Turing",
  );

  await memberPane(page)
    .getByRole("button", { name: "Manage on the org chart" })
    .click();
  await expect(chart(page)).toBeVisible({ timeout: 30_000 });

  const engineering = deskNode(page, "Engineering");
  await engineering.getByRole("button", { name: "Add teammate" }).click();
  await page.getByRole("menuitem", { name: "Turing" }).click();
  await expect(
    engineering.locator('[role="treeitem"][aria-level="3"]'),
  ).toHaveCount(3);

  // Back the way we came — a hash navigation, so Back is the operator's own
  // route home and not a reload.
  await page.goBack();
  await expect(page.getByPlaceholder("Message #engineering")).toBeVisible({
    timeout: 30_000,
  });
  await openMemberPane(page);
  // Chat re-reads the desks when it remounts, so no reload is needed for the
  // edit to show. The two surfaces stay separate reads of one host list —
  // there is no shared client cache to keep in step, and adding one would put
  // the pane's drop rule and the chart's badge rule on the same data path.
  await expect(memberPane(page).locator("ul").first()).toContainText("Turing");
});

test("#311 a seat naming nobody on the roster is shown, not hidden", async ({
  page,
}) => {
  reset();
  desks[0].members = ["grace", "ghost"];
  await mockApi(page);
  await openChart(page);

  // The chat member pane drops an unresolvable id, which is right for a chat.
  // Here it is a fact about the structure that only the operator can fix, so it
  // stays on screen and says what it is.
  const seats = deskNode(page, "Engineering").locator(
    '[role="treeitem"][aria-level="3"]',
  );
  await expect(seats).toHaveCount(2);
  await expect(seats.nth(1)).toContainText("ghost");
  await expect(seats.nth(1)).toContainText("Not on the roster");
});

test("#1102 a teammate on the chart opens their detail page", async ({
  page,
}) => {
  await mockApi(page);
  await openChart(page);

  // A **link**, not a handler: the address has to be on the element, because
  // that is what makes middle-click, cmd-click, the keyboard and the browser's
  // own hover preview work. `toHaveAttribute` is the assertion a `div` with an
  // `onClick` — the shape this issue warns against — could never pass.
  const grace = deskNode(page, "Engineering")
    .locator('[role="treeitem"][aria-level="3"]')
    .first()
    .getByRole("link", { name: "Grace" });
  await expect(grace).toHaveAttribute("href", "#/team/grace");
  await grace.click();
  await expect.poll(() => page.url()).toContain("#/team/grace");
  await expect(page.getByTestId("agent-breadcrumb-company")).toBeVisible({
    timeout: 30_000,
  });

  // The chips under "Not on a desk" name the same teammates and were the worse
  // half of #1102 — bordered pills that read as controls and did nothing.
  await openChart(page);
  const unplaced = page
    .locator("section", {
      has: page.getByRole("heading", { name: "Not on a desk" }),
    })
    .last();
  const turing = unplaced.getByRole("link", { name: "Turing" });
  await expect(turing).toHaveAttribute("href", "#/team/turing");
  await turing.click();
  await expect.poll(() => page.url()).toContain("#/team/turing");
});

test("#1102 a seat naming nobody on the roster is not offered as a link", async ({
  page,
}) => {
  reset();
  desks[0].members = ["grace", "ghost"];
  await mockApi(page);
  await openChart(page);

  // `#/team/ghost` is a dead end that only repeats the badge beside the name,
  // so the ghost seat stays text. The link on the seat above it is the teeth:
  // without it this would pass on a chart that linked nothing at all.
  const seats = deskNode(page, "Engineering").locator(
    '[role="treeitem"][aria-level="3"]',
  );
  await expect(seats.first().getByRole("link")).toHaveCount(1);
  await expect(seats.nth(1).getByRole("link")).toHaveCount(0);
});
