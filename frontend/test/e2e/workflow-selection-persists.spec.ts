import { expect, test, type APIRequestContext, type Page } from "@playwright/test";

import { expectWorkflowIndex, openWorkflow, workflowDetailName } from "./workflows";

const COMPANY_SCOPE = "/api/v1/company";

/** Dismisses the first-run tour if it is still visible. */
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

/** A minimal valid graph body: one trigger, one output, one edge. */
function graphBody(id: string, name: string) {
  return {
    id,
    name,
    description: "Created by the #864 e2e selection persistence spec.",
    nodes: [
      { id: "start", kind: "trigger", name: "Start" },
      { id: "done", kind: "output", name: "Done" },
    ],
    edges: [{ from: "start", to: "done" }],
  };
}

async function createWorkflow(request: APIRequestContext, id: string, name: string) {
  const res = await request.post(`${COMPANY_SCOPE}/workflows`, { data: graphBody(id, name) });
  expect(res.ok(), `create ${id}: ${res.status()} ${await res.text()}`).toBeTruthy();
}

/**
 * Best-effort teardown so a failed spec does not poison the next run.
 * `expectedVersion` is required (issue #1013), so this reads the workflow's
 * current token first rather than sending a bare DELETE.
 */
async function deleteWorkflow(request: APIRequestContext, id: string) {
  const version = await request
    .get(`${COMPANY_SCOPE}/workflows/${id}`)
    .then(async (res) => (res.ok() ? ((await res.json()).version as string | null) : null))
    .catch(() => null);
  const query = version ? `?expectedVersion=${encodeURIComponent(version)}` : "";
  await request.delete(`${COMPANY_SCOPE}/workflows/${id}${query}`).catch(() => undefined);
}

/**
 * Which workflow is open, read off the detail view's own heading.
 *
 * Issue #1110 moved this assertion off the toolbar picker, and issue #1135
 * removed that picker outright. Either way the heading is the surface that says
 * "this workflow is the one on screen": the tab opens on the index, where
 * nothing is open, and the heading is what the detail view names itself with.
 */
function openWorkflowName(page: Page) {
  return workflowDetailName(page);
}

async function openWorkflows(page: Page) {
  await page.goto("/#/workflows");
  await dismissTour(page);
  await expectWorkflowIndex(page);
}

/**
 * Retired with the trigger it drove.
 *
 * It covered the other half of #864: that switching companies clears the
 * previous company's workflow route rather than resolving the new company's
 * matching id, which would silently open the wrong graph. It drove that
 * through the host switcher's own "Companies" menu, mocking a two-company API
 * so the switch had somewhere real to land.
 *
 * That menu is gone. `showCompanies` in `host-switcher.tsx` reads
 * `COMPANY_SWITCHING_HIDDEN` from `src/product-scope.ts`, which is `true` on
 * this build, and `onSwitchCompany` is wired to nothing else — there is no
 * second way into a company switch to re-point this at. The route-clearing
 * logic itself is untouched and still unit-tested directly
 * (`test/unit/connection-console-switch-known-status.test.ts`); what is no
 * longer exercised is reaching it through the console's own UI. Turning the
 * flag off restores the menu and this case.
 */

test("workflows tab selection is preserved across tab switches (#864)", async ({ page, request }) => {
  const stamp = Date.now();
  const firstId = `e2e-864-first-${stamp}`;
  const secondId = `e2e-864-second-${stamp}`;
  // Stamped like every other probe here: a run that dies before its cleanup
  // leaves these workflows behind, and a static name would then match twice on
  // the index and fail the NEXT run on a strict-mode violation.
  const firstName = `Workflow selector probe A ${stamp}`;
  const secondName = `Workflow selector probe B ${stamp}`;

  try {
    await createWorkflow(request, firstId, firstName);
    await createWorkflow(request, secondId, secondName);

    await openWorkflows(page);
    await openWorkflow(page, secondName);
    await expect(page).toHaveURL(new RegExp(`#/workflows/${secondId}$`));

    // Room and Flows: two section rows, both reachable in one click from
    // anywhere. Workspace is a child under Company now, so stepping away
    // through it would take two clicks and test the sidebar rather than the
    // remembered workflow this spec is about.
    await page.getByRole("button", { name: "Room", exact: true }).click();
    await page.getByRole("button", { name: "Flows", exact: true }).click();
    await expect(openWorkflowName(page)).toHaveText(secondName);

    await page.goto(`/#/workflows/${firstId}`);
    // A full navigation, so the view has to fetch the workflow list again
    // before the heading can name anything — the default 5s assertion timeout
    // is the flake, not the console.
    await expect(openWorkflowName(page)).toHaveText(firstName, { timeout: 30_000 });

    // Room and Flows: two section rows, both reachable in one click from
    // anywhere. Workspace is a child under Company now, so stepping away
    // through it would take two clicks and test the sidebar rather than the
    // remembered workflow this spec is about.
    await page.getByRole("button", { name: "Room", exact: true }).click();
    await page.getByRole("button", { name: "Flows", exact: true }).click();
    await expect(openWorkflowName(page)).toHaveText(firstName);
  } finally {
    await deleteWorkflow(request, firstId);
    await deleteWorkflow(request, secondId);
  }
});
