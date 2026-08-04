import { expect, test, type Page, type Request } from "@playwright/test";

/**
 * Issue #263 — the task assignee is picked from the company roster, not typed.
 *
 * Issue #301 moved *where*: the create box asks for a prompt and nothing else,
 * so a new card is always unassigned (the host hands it to the orchestrator)
 * and the picker lives on the card's own edit surface. These tests therefore
 * exercise the picker through Edit / Reassign rather than through create — the
 * invariant under test is unchanged, only the screen it is reached from.
 *
 * These run against a live host serving the built console, with the
 * `e2e_harness` company: desks `engineering` / `content` and manifest agents
 * `ceo` / `engineer` / `writer`.
 *
 * The load-bearing assertion is the desk one. `AssigneeResolution` keeps a desk
 * assignment a *desk* (`links_working_agent()` is false for `Desk`), so a card
 * assigned to "Engineering desk" must persist `engineering` — never its lead
 * `engineer`. A picker that resolved the desk client-side would break the
 * invariant #214 exists to hold, and nothing else in the console would notice.
 */

const API = "/api/v1/company";

/**
 * The first-run product tour opens a modal over the board and would swallow
 * every click. `src/tour/state.ts` keys "seen" per company in localStorage, so
 * pre-seed every key this host could use before the app boots, and click the
 * dialog away if one still slips through.
 */
test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    const seen = JSON.stringify({ skipped: true, seenAt: Date.now() });
    for (const key of ["oc-tour:single", "oc-tour:e2e-harness-co", "oc-tour:null"]) {
      window.localStorage.setItem(key, seen);
    }
  });
});

async function dismissTour(page: Page) {
  const skip = page.getByRole("button", { name: "Skip for now" });
  for (let attempt = 0; attempt < 5; attempt += 1) {
    if (!(await skip.isVisible().catch(() => false))) return;
    await skip.click({ force: true }).catch(() => {});
    await page.waitForTimeout(300);
  }
  await expect(skip).toHaveCount(0);
}

/** Creates a card through the board's one prompt box (issue #301). */
async function createViaPromptBox(page: Page, prompt: string) {
  await page.goto("/#/tasks");
  await dismissTour(page);
  await page.getByRole("button", { name: "Add task" }).click();
  await expect(page.getByRole("heading", { name: "New task" })).toBeVisible();
  await page.locator("#new-prompt").fill(prompt);
  await page.getByRole("button", { name: "Create" }).click();
}

/** Opens a seeded card's edit dialog — where the assignee is now picked. */
async function openEditDialog(page: Page, id: string) {
  await page.goto(`/#/tasks/${id}`);
  await dismissTour(page);
  await page.getByRole("button", { name: "Edit" }).click();
  await expect(page.getByRole("heading", { name: "Edit task" })).toBeVisible();
}

/** Opens the assignee select and picks the option with this exact-ish name. */
async function pickAssignee(page: Page, trigger: string, option: RegExp | string) {
  await page.locator(`#${trigger}`).click();
  await page.getByRole("option", { name: option }).click();
}

/** The board card whose title matches, once the board has rendered it. */
function card(page: Page, title: string) {
  return page.locator("div[draggable=true]").filter({ hasText: title }).first();
}

test("the edit dialog offers Unassigned, desks and teammates instead of a text field", async ({
  page,
  request,
}) => {
  // The empty-desk row this asserts on needs a desk with nobody on it, and the
  // `e2e_harness` manifest ships only staffed desks — so seed it rather than
  // depend on one a previous run happened to leave behind. (This assertion
  // silently required that leftover state before #301 repaired the spec: on a
  // fresh host there was no `Legal` desk and it could not pass.) Idempotent, so
  // a re-run against the same host is fine.
  await request.post(`${API}/desks`, {
    data: { id: "legal", name: "Legal", description: "Contracts and review." },
  });

  const created = await request.post(`${API}/tasks`, {
    data: { title: `e2e picker ${Date.now()}` },
  });
  await openEditDialog(page, (await created.json()).id as string);

  // It is a select, not a free-text input: typing a name is no longer possible.
  await expect(page.locator("input#task-assignee")).toHaveCount(0);

  await page.locator("#task-assignee").click();

  // Blank is a labelled choice that says what it does, not an empty field.
  await expect(page.getByRole("option", { name: /Unassigned/ })).toBeVisible();
  await expect(page.getByText("hand it to the orchestrator")).toBeVisible();

  // Desks first, then teammates — the two kinds are separated, and in that
  // order. (Scoped to the popup's own group labels: the sidebar also has a
  // "Desks" nav item.)
  const groups = page.locator('[data-slot="select-label"]');
  await expect(groups).toHaveText(["Desks", "Teammates"]);

  await expect(page.getByRole("option", { name: /Engineering desk/ })).toBeVisible();
  await expect(page.getByRole("option", { name: /Content desk/ })).toBeVisible();

  // Manifest agents carry no display name, so the id is the handle shown, with
  // the role as the hint.
  await expect(page.getByRole("option", { name: /^engineer —/ })).toBeVisible();
  await expect(page.getByRole("option", { name: /^writer —/ })).toBeVisible();
  await expect(page.getByRole("option", { name: /^ceo —/ })).toBeVisible();

  // A desk with nobody on it stays assignable (EmptyDesk is real), and says so.
  await expect(page.getByRole("option", { name: /Legal — no members yet/ })).toBeVisible();
  // A staffed desk shows its headcount.
  await expect(page.getByRole("option", { name: /Engineering desk — 1 teammate/ })).toBeVisible();
});

test("a card assigned to a desk keeps the desk, not the desk's lead", async ({
  page,
  request,
}) => {
  const title = `e2e desk card ${Date.now()}`;
  const seeded = await request.post(`${API}/tasks`, { data: { title } });
  await openEditDialog(page, (await seeded.json()).id as string);
  await pickAssignee(page, "task-assignee", /Engineering desk/);
  await page.getByRole("button", { name: "Save" }).click();

  await page.goto("/#/tasks");
  await dismissTour(page);
  const created = card(page, title);
  await expect(created).toBeVisible({ timeout: 15_000 });
  // The whole point: `engineering` (the desk), never `engineer` (its lead).
  await expect(created).toContainText("engineering");
  await expect(created).not.toContainText(/\bengineer\b(?!ing)/);
});

test("a card can be assigned to a teammate, and created for nobody at all", async ({
  page,
  request,
}) => {
  const forWriter = `e2e writer card ${Date.now()}`;
  const seeded = await request.post(`${API}/tasks`, { data: { title: forWriter } });
  await openEditDialog(page, (await seeded.json()).id as string);
  await pickAssignee(page, "task-assignee", /^writer —/);
  await page.getByRole("button", { name: "Save" }).click();
  await page.goto("/#/tasks");
  await dismissTour(page);
  await expect(card(page, forWriter)).toContainText("writer", { timeout: 15_000 });

  // Issue #301: the prompt box collects no assignee, so a card created on the
  // board is unassigned by construction — the host hands it to the
  // orchestrator. This is the default the create surface now relies on.
  const forNobody = `e2e unassigned card ${Date.now()}`;
  await createViaPromptBox(page, forNobody);

  const nobody = card(page, forNobody);
  await expect(nobody).toBeVisible({ timeout: 15_000 });
  // No assignee row rendered at all.
  await expect(nobody.locator("span.truncate")).toHaveCount(0);
});

test("renaming a card does not resubmit its assignee", async ({ page, request }) => {
  // Seed a card assigned to a teammate through the API, so the edit is the only
  // thing under test.
  const title = `e2e rename ${Date.now()}`;
  const created = await request.post(`${API}/tasks`, {
    data: { title, assignee: "writer" },
  });
  expect(created.ok()).toBeTruthy();
  const id = (await created.json()).id as string;

  const patches: Array<Record<string, unknown>> = [];
  page.on("request", (req: Request) => {
    if (req.method() === "PATCH" && req.url().includes("/tasks/")) {
      patches.push(JSON.parse(req.postData() ?? "{}"));
    }
  });

  await openEditDialog(page, id);
  await page.locator("#task-title").fill(`${title} renamed`);
  await page.getByRole("button", { name: "Save" }).click();

  await expect.poll(() => patches.length, { timeout: 15_000 }).toBeGreaterThan(0);
  // Only the field that changed is sent. Sending `assignee` too is what made a
  // card with an off-roster assignee permanently uneditable: the host
  // re-resolves every assignee it receives.
  expect(patches[0]).toEqual({ title: `${title} renamed` });
  expect(patches[0]).not.toHaveProperty("assignee");
});

test("the detail screen can hand a card back to the orchestrator", async ({ page, request }) => {
  const title = `e2e unassign ${Date.now()}`;
  const created = await request.post(`${API}/tasks`, {
    data: { title, assignee: "engineer" },
  });
  const id = (await created.json()).id as string;

  await page.goto(`/#/tasks/${id}`);
  await dismissTour(page);
  await expect(page.getByRole("heading", { name: title })).toBeVisible();
  await page.getByRole("button", { name: "Reassign" }).click();

  // The reassign row is the same picker; Unassigned is reachable from it, which
  // it was not while the row early-returned on an empty string.
  await page.getByRole("combobox").last().click();
  await page.getByRole("option", { name: /Unassigned/ }).click();
  await page.getByRole("button", { name: "Save", exact: true }).click();

  await expect
    .poll(
      async () => (await (await request.get(`${API}/tasks/${id}`)).json()).task.assignee,
      { timeout: 15_000 },
    )
    .toBe("");
});

test("an assignee the roster no longer carries still renders, and the card stays editable", async ({
  page,
  request,
}) => {
  // Assign to an overlay teammate, then remove the teammate. The card now holds
  // an id that resolves to nothing.
  const ghost = await request.post(`${API}/team`, {
    data: { name: `Ghost ${Date.now()}`, role: "Tester" },
  });
  const ghostId = (await ghost.json()).id as string;
  const title = `e2e stale ${Date.now()}`;
  const created = await request.post(`${API}/tasks`, {
    data: { title, assignee: ghostId },
  });
  const id = (await created.json()).id as string;
  expect((await request.delete(`${API}/team/${ghostId}`)).status()).toBe(204);

  await page.goto(`/#/tasks/${id}`);
  await dismissTour(page);
  await page.getByRole("button", { name: "Edit" }).click();

  // Flagged, not silently dropped and not silently replaced by something else.
  await expect(page.locator("#task-assignee")).toContainText("not on roster", {
    timeout: 15_000,
  });
  await expect(page.locator("#task-assignee")).toContainText(ghostId);

  // And a save that does not touch the assignee still succeeds.
  await page.locator("#task-title").fill(`${title} renamed`);
  await page.getByRole("button", { name: "Save" }).click();
  await expect(page.getByText("could not save")).toHaveCount(0);
  await expect
    .poll(async () => (await (await request.get(`${API}/tasks/${id}`)).json()).task.title, {
      timeout: 15_000,
    })
    .toBe(`${title} renamed`);
});
