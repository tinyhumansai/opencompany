import {
  expect,
  test,
  type APIRequestContext,
  type Locator,
  type Page,
} from "@playwright/test";

/**
 * Issue #824: the workflows toolbar laid out wider than its container and
 * overflowed into an `overflow-hidden` ancestor, so its last control —
 * **New workflow** — was clipped off the right edge. Nothing in that chain
 * scrolls, so the button was not merely off-screen: it could not be clicked.
 *
 * This is a **layout** defect, so it is only observable in a browser. jsdom
 * computes no geometry, and a unit test asserting the class string would pass
 * against any width — including a row that still overflows. The measurement has
 * to be a real one.
 *
 * The row grows every time a control is added. `Pause` (#814) took the overhang
 * from 22px to 113px, which is what made it visible, but the row was already
 * overflowing before it. So the spec asserts the **property** rather than
 * today's button count: every control in the toolbar is inside the viewport and
 * clickable. A tenth control that reintroduces the overflow fails here.
 *
 * Runs at two widths. 1280 is the common laptop width the defect was reported
 * at; 1024 is the narrow end, where a fix that merely bought a few pixels would
 * still fail.
 */

const COMPANY_SCOPE = "/api/v1/company";
const WORKFLOW_ID = "e2e-824-toolbar";
const WORKFLOW_NAME = "Toolbar reachability";

/** The tour's overlay swallows pointer events; tolerate its absence. */
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

async function createWorkflow(request: APIRequestContext) {
  const res = await request.post(`${COMPANY_SCOPE}/workflows`, {
    data: {
      id: WORKFLOW_ID,
      name: WORKFLOW_NAME,
      description: "Created by the #824 e2e spec.",
      nodes: [
        // The `schedule` is load-bearing, not decoration: it is what makes
        // `isScheduled` true and mounts the Pause control asserted below.
        { id: "start", kind: "trigger", name: "Start", schedule: "0 9 * * *" },
        { id: "done", kind: "output", name: "Report" },
      ],
      edges: [{ from: "start", to: "done" }],
    },
  });
  expect(res.ok(), `create: ${res.status()} ${await res.text()}`).toBeTruthy();
}

/**
 * Best-effort teardown so a failed spec does not poison the next run.
 * `expectedVersion` is required (issue #1013), so this reads the workflow's
 * current token first.
 */
async function removeWorkflow(request: APIRequestContext) {
  const version = await request
    .get(`${COMPANY_SCOPE}/workflows/${WORKFLOW_ID}`)
    .then(async (res) => (res.ok() ? ((await res.json()).version as string | null) : null))
    .catch(() => null);
  const query = version ? `?expectedVersion=${encodeURIComponent(version)}` : "";
  await request
    .delete(`${COMPANY_SCOPE}/workflows/${WORKFLOW_ID}${query}`)
    .catch(() => undefined);
}

/**
 * Selects the workflow in the picker, so the per-workflow half of the toolbar
 * mounts and the row is at its **widest** — the state the defect appears in.
 *
 * The view does auto-select when the list lands, so this is belt-and-braces
 * today. It is here anyway because the auto-select is not what this spec is
 * about: were it ever to change, the assertions below would start failing on a
 * toolbar that was simply never populated, which reads as a layout regression
 * and is not one. Selecting explicitly keeps the failure honest. Matches the
 * helper in `workflow-node-config.spec.ts`.
 */
async function selectWorkflow(page: Page, name: string) {
  const picker = page.getByRole("combobox").first();
  await expect(picker).toBeEnabled();
  await picker.click();
  await page.getByRole("option", { name, exact: true }).click();
  await expect(picker).toContainText(name);
}

/**
 * The toolbar's controls, each with the locator that finds it.
 *
 * `Pause` is not addressed by name like the rest: its accessible name comes
 * from an `aria-label` that flips to `Resume schedule` once the schedule is
 * off, so a name match would silently stop matching the day the fixture starts
 * out paused. The test id is stable across both.
 *
 * It also only mounts for a **scheduled** workflow (`isScheduled` — a trigger
 * node carrying a `schedule`), which is why the fixture's trigger has one.
 * That matters more than it looks: Pause is the control that made this defect
 * visible, taking the overhang from 22px to 113px, so a version of this spec
 * that skipped it would be measuring the row that fits.
 */
const CONTROLS: Array<{ label: string; find: (page: Page) => Locator }> = [
  {
    label: "Run",
    find: (p) => p.getByRole("button", { name: "Run", exact: false }).first(),
  },
  {
    label: "Test run",
    find: (p) => p.getByRole("button", { name: "Test run" }).first(),
  },
  {
    label: "Browse",
    find: (p) => p.getByRole("button", { name: "Browse" }).first(),
  },
  {
    label: "Copilot",
    find: (p) => p.getByRole("button", { name: "Copilot" }).first(),
  },
  {
    label: "History",
    find: (p) => p.getByRole("button", { name: "History" }).first(),
  },
  { label: "Pause", find: (p) => p.getByTestId("workflow-toggle-enabled") },
  {
    label: "Edit",
    find: (p) => p.getByRole("button", { name: "Edit" }).first(),
  },
  {
    label: "Delete",
    find: (p) => p.getByRole("button", { name: "Delete" }).first(),
  },
  {
    label: "New workflow",
    find: (p) => p.getByRole("button", { name: "New workflow" }),
  },
];

test.describe("workflows toolbar reachability (#824)", () => {
  test.beforeEach(async ({ request }) => {
    await removeWorkflow(request);
    await createWorkflow(request);
  });

  test.afterEach(async ({ request }) => {
    await removeWorkflow(request);
  });

  for (const width of [1280, 1024]) {
    test(`every toolbar control is inside the viewport at ${width}px`, async ({
      page,
    }) => {
      await page.setViewportSize({ width, height: 800 });
      await page.goto("/#/workflows");
      await dismissTour(page);

      // Wait for the toolbar to mount before measuring anything.
      await expect(
        page.getByRole("button", { name: "New workflow" }),
      ).toBeVisible();
      await selectWorkflow(page, WORKFLOW_NAME);

      for (const { label, find } of CONTROLS) {
        const control = find(page);
        await expect(control, `${label} should be mounted`).toBeVisible();
        // The assertion that matters. `toBeVisible` is true for a button that
        // has been pushed past the right edge — it is painted, just not
        // anywhere reachable. `toBeInViewport` is what distinguishes the two.
        await expect(
          control,
          `${label} should be reachable at ${width}px`,
        ).toBeInViewport();
      }
    });
  }

  test("New workflow can actually be clicked, not merely rendered", async ({
    page,
  }) => {
    // The defect's real cost. Every control above could be in the viewport and
    // this could still fail if something overlapped it, so the last word is an
    // actual click with its actual consequence.
    await page.setViewportSize({ width: 1280, height: 800 });
    await page.goto("/#/workflows");
    await dismissTour(page);
    // Selected, because that is the crowded row. Clicking against the short
    // one would prove nothing: the short row never clipped anything.
    await selectWorkflow(page, WORKFLOW_NAME);

    const newWorkflow = page.getByRole("button", { name: "New workflow" });
    // Measured before the click, and the reason is not belt-and-braces: a
    // Playwright click scrolls its target into view first, and it manages that
    // even inside the `overflow-hidden` ancestor a person cannot scroll. So
    // `click()` alone SUCCEEDS against the clipped layout — verified by
    // reverting the fix, where this test passed and the two above failed. The
    // click is what proves the button works; this line is what proves a person
    // could reach it.
    await expect(newWorkflow).toBeInViewport();

    await newWorkflow.click();
    await expect(page.getByRole("dialog")).toBeVisible();
    await expect(page.getByText("Describe the workflow")).toBeVisible();
  });
});
