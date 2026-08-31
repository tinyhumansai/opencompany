import { expect, test, type Page, type Route } from "@playwright/test";

/**
 * Issue #1845: the week-1 "save your first workflow" nudge banner.
 *
 * `LifecycleScheduler` (host-side, `src/runtime/lifecycle_scheduler.rs`) is a
 * daily background tick with no manual-fire route, so a spec cannot make it
 * actually dispatch a nudge inside a test run. What CAN be driven from a
 * browser — and is everything downstream of the host's own decision — is
 * "given the host answers `GET …/notifications?kind=workflow_nudge` with an
 * unread row, does the console show the banner, does its CTA open the create
 * dialog, does Dismiss (and a `workflow_created` frame) mark it read, and does
 * a row the host has already marked read stay hidden (the reload case)". So
 * `/notifications` and `/events` are intercepted; everything else — desks,
 * team, the workflow list — hits the real harness host, the same middle
 * ground `workflow-create-affordance.spec.ts` uses.
 *
 * Runs against the live host `playwright.config.ts` brings up.
 */

/** The notifications route, and only it — `GET` lists, `PUT` marks read. */
function isNotifications(url: URL): boolean {
  return /\/api\/v1\/(company|companies\/[^/]+)\/notifications$/.test(url.pathname);
}

/** Dismisses the first-run tour if it is up; its overlay swallows clicks. */
async function dismissTour(page: Page) {
  const skip = page.getByRole("button", { name: "Skip for now" });
  try {
    await skip.waitFor({ state: "visible", timeout: 10_000 });
  } catch {
    return;
  }
  await skip.click();
  await expect(skip).toBeHidden();
}

const NUDGE_ROW = {
  id: "week1-nudge-1",
  kind: "workflow_nudge",
  subjectKind: "workflow",
  subjectId: "week1-first-workflow",
  title: "Save your first workflow",
  createdAt: Date.now(),
};

/** Every `PUT …/notifications` body the console sent, so a mark-read can be asserted. */
let marked: Array<{ ids?: string[] }> = [];

/**
 * Mocks `/notifications` over one in-memory row, mutated by `PUT` exactly the
 * way the real host's mark-read latch behaves — read once, stays read.
 */
async function mockNotifications(page: Page, row: typeof NUDGE_ROW | null) {
  const state = { row };
  await page.route(
    (url) => isNotifications(url),
    async (route: Route) => {
      const request = route.request();
      if (request.method() === "PUT") {
        const body = (request.postDataJSON() ?? {}) as { ids?: string[] };
        marked.push(body);
        if (state.row && (body.ids === undefined || body.ids.includes(state.row.id))) {
          state.row = { ...state.row, readAt: Date.now() } as typeof NUDGE_ROW;
        }
        const unread = state.row && !("readAt" in state.row) ? 1 : 0;
        return route.fulfill({
          status: 200,
          contentType: "application/json",
          body: JSON.stringify({ unread }),
        });
      }
      const url = new URL(request.url());
      const kind = url.searchParams.get("kind") ?? "mention";
      const rows = state.row && state.row.kind === kind ? [state.row] : [];
      return route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          notifications: rows,
          unread: rows.filter((r) => !("readAt" in r)).length,
        }),
      });
    },
  );
}

/** An `/events` stream that never delivers a frame — the "renders" tests' baseline. */
async function mockEmptyEvents(page: Page) {
  await page.route("**/events", (route) =>
    route.fulfill({
      status: 200,
      headers: { "content-type": "text/event-stream", "cache-control": "no-cache" },
      body: "",
    }),
  );
}

/**
 * An `/events` stream carrying one `workflow_created` frame from the moment
 * the console connects — the stand-in for a teammate's create, or the
 * orchestrator's `create_workflow` tool, or this session's own (all three are
 * indistinguishable on the wire by design; see `use-events.ts`). Used to
 * prove the frame alone must NOT clear or mark-read THIS user's nudge (PR
 * #1878 review) — only a confirmed local create (`handleCreated`, exercised
 * by `workflow-create-affordance.spec.ts`) or the host's own per-user feed
 * saying so may do that.
 */
async function mockEventsWithCreate(page: Page) {
  await page.route("**/events", (route) =>
    route.fulfill({
      status: 200,
      headers: { "content-type": "text/event-stream", "cache-control": "no-cache" },
      body:
        `data: ${JSON.stringify({
          type: "workflow_created",
          seq: 1,
          atMillis: Date.now(),
          workflowId: "wf-e2e",
          name: "A workflow created elsewhere",
        })}\n\n`,
    }),
  );
}

const banner = (page: Page) => page.getByTestId("workflow-week1-nudge");

test.beforeEach(() => {
  marked = [];
});

test("the banner renders when the host has an unread week-1 nudge", async ({ page }) => {
  await mockNotifications(page, NUDGE_ROW);
  await mockEmptyEvents(page);

  await page.goto("/#/workflows");
  await dismissTour(page);

  await expect(banner(page)).toBeVisible();
  await expect(banner(page)).toContainText("Save your first workflow");
  await expect(page.getByTestId("workflow-week1-nudge-create")).toBeVisible();
});

test("a host with no unread nudge shows no banner", async ({ page }) => {
  await mockNotifications(page, null);
  await mockEmptyEvents(page);

  await page.goto("/#/workflows");
  await dismissTour(page);

  // Give the poll a moment to land before asserting absence, so this proves
  // "the host said no" rather than "the fetch had not resolved yet".
  await page.waitForTimeout(300);
  await expect(banner(page)).toHaveCount(0);
});

test("the CTA opens the same create dialog the toolbar's New workflow button does", async ({
  page,
}) => {
  await mockNotifications(page, NUDGE_ROW);
  await mockEmptyEvents(page);

  await page.goto("/#/workflows");
  await dismissTour(page);
  await expect(banner(page)).toBeVisible();

  await page.getByTestId("workflow-week1-nudge-create").click();
  const dialog = page.getByRole("dialog");
  await expect(dialog.getByText("New workflow", { exact: true })).toBeVisible();
});

test("Dismiss marks the nudge read and hides the banner, without creating anything", async ({
  page,
}) => {
  await mockNotifications(page, NUDGE_ROW);
  await mockEmptyEvents(page);

  await page.goto("/#/workflows");
  await dismissTour(page);
  await expect(banner(page)).toBeVisible();

  await page.getByTestId("workflow-week1-nudge-dismiss").click();
  await expect(banner(page)).toHaveCount(0);

  await expect
    .poll(() => marked.some((m) => m.ids?.includes(NUDGE_ROW.id)))
    .toBe(true);
});

/**
 * PR #1878 review fix: a `workflow_created` frame carries no actor, so it
 * must NOT clear or mark-read THIS user's nudge — a teammate's or the
 * orchestrator's create used to silence a nudge for a user who has never
 * saved a workflow themselves, exactly the false-negative the review flagged.
 * The tick still re-asks the host's own per-user feed (`refreshNudge`), which
 * this mock keeps answering "still unread" — proving the banner survives an
 * anonymous frame rather than merely proving nothing crashed.
 */
test("a workflow_created frame from an unattributed source does not clear the banner", async ({
  page,
}) => {
  await mockNotifications(page, NUDGE_ROW);
  await mockEventsWithCreate(page);

  await page.goto("/#/workflows");
  await dismissTour(page);

  // The frame rides the SAME connection the console opens on boot, so by the
  // time the page has settled the (non-)clear has already happened —
  // asserting the SETTLED state is the property under test, not a race
  // against when the frame arrives.
  await page.waitForTimeout(2_000);
  await expect(banner(page)).toBeVisible();
  expect(marked.some((m) => m.ids?.includes(NUDGE_ROW.id))).toBe(false);
});

/**
 * The reload case: once the host has the row marked read, a fresh page load
 * — which is all "mark-read persists across reload" can mean for a
 * server-backed banner — must not resurrect it. `pickActiveNudge`
 * (`src/lib/week1-nudge.ts`) is the unit-tested decision this exercises end
 * to end; this proves the WIRING reads the host's answer rather than some
 * client-only memory of "I dismissed this already".
 */
test("a nudge the host has already marked read never reappears on load", async ({ page }) => {
  await mockNotifications(page, { ...NUDGE_ROW, readAt: Date.now() } as typeof NUDGE_ROW);
  await mockEmptyEvents(page);

  await page.goto("/#/workflows");
  await dismissTour(page);

  await page.waitForTimeout(300);
  await expect(banner(page)).toHaveCount(0);
});

test("a host with no notification route simply shows no banner", async ({ page }) => {
  await page.route(
    (url) => isNotifications(url),
    (route: Route) =>
      route.fulfill({
        status: 404,
        contentType: "application/json",
        body: JSON.stringify({ error: "not_found" }),
      }),
  );
  await mockEmptyEvents(page);

  await page.goto("/#/workflows");
  await dismissTour(page);

  await expect(page.getByTestId("workflow-index")).toBeVisible({ timeout: 30_000 });
  await expect(banner(page)).toHaveCount(0);
});
