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

/** Every write the console made, so the request shape can be asserted on. */
let writes: { method: string; path: string; body?: unknown }[] = [];

/** The host's desks, as this stub holds them. Mutated by the write routes. */
let desks: Desk[] = [];

function reset() {
  writes = [];
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
      return key.startsWith("oc-tour:") ? '{"skipped":true}' : real.call(this, key);
    };
  });

  await page.route("**/api/v1/**", async (route) => {
    const request = route.request();
    const method = request.method();
    const path = new URL(request.url()).pathname;
    const json = (body: unknown, status = 200) =>
      route.fulfill({ status, contentType: "application/json", body: JSON.stringify(body) });
    const noContent = () => route.fulfill({ status: 204, body: "" });
    const status = { id: COMPANY, name: "Acme", lifecycle: "running", pending_approvals: 0 };

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
      if (target && body.ordered_member_ids) target.members = [...body.ordered_member_ids];
      return noContent();
    }

    // DELETE .../desks/{id}/members/{agent}
    const member = path.match(/\/desks\/([^/]+)\/members\/([^/]+)$/);
    if (member && method === "DELETE") {
      writes.push({ method, path });
      const target = desk(member[1]);
      if (target) {
        target.members = target.members.filter((m) => m !== member[2]);
        target.overlayMembers = (target.overlayMembers ?? []).filter((m) => m !== member[2]);
      }
      return noContent();
    }

    // POST .../desks/{id}/members
    const members = path.match(/\/desks\/([^/]+)\/members$/);
    if (members && method === "POST") {
      // snake_case on the wire, as above (`AddDeskMember { agent_id }`).
      const body = request.postDataJSON() as { agent_id: string };
      writes.push({ method, path, body });
      const target = desk(members[1]);
      if (target) {
        target.members = [...target.members, body.agent_id];
        target.overlayMembers = [...(target.overlayMembers ?? []), body.agent_id];
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
      return json(desks);
    }

    if (path.endsWith("/team")) return json(ROSTER);
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
      return route.fulfill({ status: 200, contentType: "text/event-stream", body: "" });
    }
    if (path.endsWith("/me")) return json({ id: "op", email: "op@example.com", role: "admin" });
    return json([]);
  });
}

const chart = (page: Page) => page.getByRole("tree", { name: "Company org chart" });

/** A desk node by its heading text; `aria-level` 2 keeps seats from matching. */
const deskNode = (page: Page, name: string) =>
  chart(page).locator('[role="treeitem"][aria-level="2"]').filter({ hasText: name });

async function openChart(page: Page) {
  await page.goto("/#/company");
  await expect(chart(page)).toBeVisible({ timeout: 30_000 });
}

test.beforeEach(reset);

test("#311 the org chart is reachable, which it was not before", async ({ page }) => {
  await mockApi(page);

  // The nav entry. Before this change there was none, and no hash reached it.
  await page.goto("/#/overview");
  const nav = page.getByRole("link", { name: "Company" }).or(
    page.getByRole("button", { name: "Company" }),
  );
  await expect(nav.first()).toBeVisible({ timeout: 30_000 });

  await openChart(page);
  // The hash survives rather than being rewritten to the fallback view, which
  // is exactly what `#/desks` did (and still does — it names no view).
  expect(page.url()).toContain("#/company");
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
  await expect(chart(page).locator('[role="treeitem"][aria-level="2"]')).toHaveCount(2);
  await expect(chart(page).locator('[role="treeitem"][aria-level="3"]')).toHaveCount(4);

  // And there is no fourth. The cap is structural: no desk can name a parent
  // desk, so nothing here can emit one.
  await expect(chart(page).locator('[role="treeitem"][aria-level="4"]')).toHaveCount(0);
});

test("#311 the lead is the desk's first member, badged", async ({ page }) => {
  await mockApi(page);
  await openChart(page);

  const seats = deskNode(page, "Engineering").locator('[role="treeitem"][aria-level="3"]');
  await expect(seats).toHaveCount(2);
  await expect(seats.first()).toContainText("Grace");
  // Through the role, not the bare attribute: a `getByLabel` match proves the
  // attribute is in the DOM, not that anything would announce it. The crown
  // carries `role="img"` so the accessible name actually resolves.
  await expect(seats.first().getByRole("img", { name: "Desk lead" })).toBeVisible();
  await expect(seats.nth(1)).toContainText("Ada");
  await expect(seats.nth(1).getByRole("img", { name: "Desk lead" })).toHaveCount(0);
});

test("#311 a desk can be created from the chart and survives a reload", async ({ page }) => {
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

  const created = writes.find((w) => w.method === "POST" && w.path.endsWith("/desks"));
  expect(created?.body).toMatchObject({ name: "Support", members: ["linus"] });

  // The proof that this is the host's and not the component's: reload.
  await page.reload();
  await expect(chart(page)).toBeVisible({ timeout: 30_000 });
  await expect(deskNode(page, "Support")).toBeVisible();
  await expect(deskNode(page, "Support")).toContainText("Linus");
});

test("#311 membership can be edited from the chart and survives a reload", async ({ page }) => {
  await mockApi(page);
  await openChart(page);

  // Turing sits on no desk, so the chart accounts for him beside the tree
  // rather than dropping him.
  const unplaced = page.getByRole("heading", { name: "Not on a desk" });
  await expect(unplaced).toBeVisible();
  await expect(unplaced.locator("xpath=following-sibling::ul[1]")).toContainText("Turing");

  const engineering = deskNode(page, "Engineering");
  await engineering.getByRole("button", { name: "Add teammate" }).click();
  await page.getByRole("menuitem", { name: "Linus" }).click();

  await expect(engineering.locator('[role="treeitem"][aria-level="3"]')).toHaveCount(3);
  expect(
    writes.some((w) => w.method === "POST" && w.path.endsWith("/desks/engineering/members")),
  ).toBe(true);

  await page.reload();
  await expect(chart(page)).toBeVisible({ timeout: 30_000 });
  await expect(deskNode(page, "Engineering")).toContainText("Linus");

  // And removing an operator-added member takes it back off.
  await deskNode(page, "Engineering")
    .getByRole("button", { name: "Remove Linus from Engineering" })
    .click();
  await expect(deskNode(page, "Engineering").locator('[role="treeitem"][aria-level="3"]')).toHaveCount(2);
  await page.reload();
  await expect(chart(page)).toBeVisible({ timeout: 30_000 });
  await expect(deskNode(page, "Engineering")).not.toContainText("Linus");
});

test("#311 the lead can be changed from the chart and survives a reload", async ({ page }) => {
  await mockApi(page);
  await openChart(page);

  // Moving the second seat up is the change of lead — `members[0]` is the lead
  // and there is no separate call to make.
  await deskNode(page, "Engineering")
    .getByRole("button", { name: "Move Ada up in Engineering" })
    .click();

  const seats = deskNode(page, "Engineering").locator('[role="treeitem"][aria-level="3"]');
  await expect(seats.first()).toContainText("Ada");
  await expect(seats.first().getByRole("img", { name: "Desk lead" })).toBeVisible();

  const ordered = writes.find((w) => w.method === "PUT" && w.path.endsWith("/order"));
  expect(ordered?.body).toEqual({ ordered_member_ids: ["ada", "grace"] });

  await page.reload();
  await expect(chart(page)).toBeVisible({ timeout: 30_000 });
  await expect(
    deskNode(page, "Engineering").locator('[role="treeitem"][aria-level="3"]').first(),
  ).toContainText("Ada");
});

test("#311 blueprint structure offers no control the host would refuse", async ({ page }) => {
  await mockApi(page);
  await openChart(page);

  // A manifest desk cannot be deleted at runtime, so no delete is offered.
  await expect(deskNode(page, "Engineering")).toContainText("Blueprint");
  await expect(
    deskNode(page, "Engineering").getByRole("button", { name: "Delete Engineering" }),
  ).toHaveCount(0);
  // Nor can its manifest members be removed.
  await expect(
    deskNode(page, "Engineering").getByRole("button", { name: /^Remove / }),
  ).toHaveCount(0);

  // An operator-created desk can be deleted, and its added member removed —
  // but its founding member is still blueprint to the host.
  const growth = deskNode(page, "Growth");
  await expect(growth.getByRole("button", { name: "Delete Growth" })).toBeVisible();
  await expect(growth.getByRole("button", { name: "Remove Hedy from Growth" })).toBeVisible();
  await expect(growth.getByRole("button", { name: "Remove Linus from Growth" })).toHaveCount(0);

  await growth.getByRole("button", { name: "Delete Growth" }).click();
  await expect(deskNode(page, "Growth")).toHaveCount(0);
  await page.reload();
  await expect(chart(page)).toBeVisible({ timeout: 30_000 });
  await expect(deskNode(page, "Growth")).toHaveCount(0);
});

test("#311 a seat naming nobody on the roster is shown, not hidden", async ({ page }) => {
  reset();
  desks[0].members = ["grace", "ghost"];
  await mockApi(page);
  await openChart(page);

  // The chat member pane drops an unresolvable id, which is right for a chat.
  // Here it is a fact about the structure that only the operator can fix, so it
  // stays on screen and says what it is.
  const seats = deskNode(page, "Engineering").locator('[role="treeitem"][aria-level="3"]');
  await expect(seats).toHaveCount(2);
  await expect(seats.nth(1)).toContainText("ghost");
  await expect(seats.nth(1)).toContainText("Not on the roster");
});
