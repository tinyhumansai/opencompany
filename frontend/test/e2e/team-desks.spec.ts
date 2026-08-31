import { expect, test, type Page } from "@playwright/test";

/**
 * End-to-end proof for issue #1440 — the roster cards show desk chips.
 *
 * Each card's `desks` field arrives on the roster read and was never drawn. The
 * card now shows one chip per desk with the desk name and a "(lead)" marker for
 * the desk it leads, and "Not on a desk" when the teammate sits on none.
 */

const COMPANY = "acme";

const ROSTER = [
  {
    id: "maya",
    name: "Maya",
    role: "Research Lead",
    description: "Tracks competitor moves and drafts the weekly brief.",
    desks: [{ id: "research", name: "Research", lead: true }],
  },
  {
    id: "ravi",
    name: "Ravi",
    role: "Analyst",
    description: "Digs through the numbers.",
    desks: [],
  },
  {
    id: "priya",
    name: "Priya",
    role: "Writer",
    description: "Turns findings into words.",
    desks: [{ id: "research", name: "Research", lead: false }],
  },
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
    const status = {
      id: COMPANY,
      name: "Acme",
      lifecycle: "running",
      pending_approvals: 0,
    };

    if (path === "/api/v1/companies") return json([status]);
    if (path === `/api/v1/companies/${COMPANY}`) return json(status);
    if (path.endsWith("/desks"))
      return json([{ id: "research", name: "Research", members: ["maya", "priya"] }]);
    if (path.endsWith("/tasks")) return json([]);
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
            statuses: [{ name: "todo", label: "To-do", closed: false }],
            sections: [],
            open: 0,
            closed: 0,
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
        editable: ["name", "role", "description"],
        isOrchestrator: false,
        tools: { requested: [], companyAllow: [], deskAllow: [], deskCeilingActive: false, effective: [] },
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

test("#1440 teammates on a desk show the desk's name on the card", async ({ page }) => {
  await mockApi(page);
  await page.goto("/#/company");

  await expect(card(page, "Maya")).toBeVisible({ timeout: 30_000 });

  // Maya sits on Research and leads it — the chip shows both.
  const mayaDesks = card(page, "Maya").getByTestId("team-card-desks");
  await expect(mayaDesks.getByTestId("team-card-desk-research")).toBeVisible();
  await expect(mayaDesks.getByTestId("team-card-desk-research")).toContainText("Research");
  await expect(mayaDesks.getByTestId("team-card-desk-research")).toContainText("(lead)");
});

test("#1440 a teammate on no desk says so on the card", async ({ page }) => {
  await mockApi(page);
  await page.goto("/#/company");

  await expect(card(page, "Ravi")).toBeVisible({ timeout: 30_000 });

  // Ravi's roster entry carries no desks — the card says so rather than gap.
  const raviDesks = card(page, "Ravi").getByTestId("team-card-desks");
  await expect(raviDesks.getByTestId("team-card-no-desks")).toBeVisible();
  await expect(raviDesks.getByTestId("team-card-no-desks")).toHaveText("Not on a desk");
});

test("#1440 a desk member who is not the lead carries no marker", async ({ page }) => {
  await mockApi(page);
  await page.goto("/#/company");

  await expect(card(page, "Priya")).toBeVisible({ timeout: 30_000 });

  // Priya sits on Research but does not lead it — the chip has the name only.
  const priyaDesks = card(page, "Priya").getByTestId("team-card-desks");
  await expect(priyaDesks.getByTestId("team-card-desk-research")).toBeVisible();
  await expect(priyaDesks.getByTestId("team-card-desk-research")).toContainText("Research");
  await expect(priyaDesks.getByTestId("team-card-desk-research")).not.toContainText("(lead)");
});

test("#1440 clicking a desk chip navigates to its own address", async ({ page }) => {
  await mockApi(page);

  await page.goto("/#/company");
  await expect(card(page, "Maya")).toBeVisible({ timeout: 30_000 });

  // Click the Research chip on Maya's card.
  await card(page, "Maya")
    .getByTestId("team-card-desks")
    .getByTestId("team-card-desk-research")
    .click();

  // The org chart landed at that desk's address (issue #485).
  await expect.poll(() => page.url()).toContain("#/company/research");
});

test("#1391 the teammate action is a focused title button, not an interactive card", async ({ page }) => {
  await mockApi(page);
  await page.goto("/#/company");

  const maya = card(page, "Maya");
  await expect(maya).toBeVisible({ timeout: 30_000 });

  // The card remains a grouping surface while its real controls — the title,
  // desk chip, and overflow menu — are siblings. This avoids a button owning
  // other interactive descendants and leaves the title's native Enter/Space
  // activation and focus indicator intact.
  await expect(maya).not.toHaveAttribute("role", "button");
  const open = maya.getByTestId("team-card-open");
  await expect(open).toHaveJSProperty("tagName", "BUTTON");
  await open.focus();
  await expect(open).toBeFocused();
  await open.press("Enter");
  await expect(page).toHaveURL(/#\/team\/maya$/);
});

test("#1810 the teammate card opens without swallowing its actions menu", async ({ page }) => {
  await mockApi(page);
  await page.goto("/#/company");

  const maya = card(page, "Maya");
  await expect(maya).toBeVisible({ timeout: 30_000 });

  // The description is deliberately plain card content, not the title button.
  // Clicking it proves the stretched title action covers the card surface.
  const description = await maya.getByTestId("team-card-description").boundingBox();
  if (!description) throw new Error("Maya's description has no clickable bounds");
  await page.mouse.click(description.x + 4, description.y + 4);
  await expect(page).toHaveURL(/#\/team\/maya$/);

  await page.goto("/#/company");
  await expect(maya).toBeVisible({ timeout: 30_000 });

  // The overflow stays above the stretched target: it opens Remove without
  // navigating to the teammate underneath it.
  await maya.getByRole("button", { name: "Teammate actions" }).click();
  await expect(page.getByRole("menuitem", { name: "Remove" })).toBeVisible();
  await expect(page).toHaveURL(/#\/company$/);
});
