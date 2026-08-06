import { expect, test, type APIRequestContext, type Page } from "@playwright/test";

/**
 * Issue #384: workflow create / update / delete events reached the console's
 * SSE switch, matched no arm, and were dropped.
 *
 * The host has journalled and projected all three since #112/#259 — `GET
 * {scope}/events` carries `workflow_created`, `workflow_updated` and
 * `workflow_deleted` — so nothing was missing on the wire. The console simply
 * had no case for them, which is the failure shape this switch has produced
 * twice before (#464 for the board, #371 for the run canvas): the frames
 * arrive, fall through to `default:`, and nothing logs.
 *
 * What the operator saw: with the Workflows tab open, a workflow authored by
 * the orchestrator's `create_workflow` tool, by a second console session, or by
 * a machine credential did not appear; a rename did not land; and a delete left
 * its entry sitting in the picker, so the next click ran or edited a workflow
 * the host no longer had.
 *
 * **Every test here writes from outside the browser** and asserts the open tab
 * followed, with no reload and no company switch. That is the whole property —
 * a spec that clicked the console's own Create button would pass against the
 * broken build, because the local handler splices the row in by hand.
 *
 * Runs against the live host `playwright.config.ts` brings up, in the
 * `Console E2E` CI lane (issue #428).
 */

const COMPANY_SCOPE = "/api/v1/company";

/**
 * Dismisses the first-run tour if it is up. Its overlay swallows pointer
 * events. Tolerates its absence — a company that has seen it never shows it
 * again, so this is a no-op on every run after the first.
 */
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
    description: "Created by the #384 e2e spec.",
    nodes: [
      { id: "start", kind: "trigger", name: "Start", schedule: "0 9 * * *" },
      { id: "done", kind: "output", name: "Report" },
    ],
    edges: [{ from: "start", to: "done" }],
  };
}

/**
 * Authors a workflow over HTTP — the stand-in for the orchestrator's
 * `create_workflow` tool and for a second console session, neither of which a
 * browser test can drive. The host journals `WorkflowCreated` on this path, so
 * the page under test can only learn about it through the SSE stream.
 */
async function createWorkflow(request: APIRequestContext, id: string, name: string) {
  const res = await request.post(`${COMPANY_SCOPE}/workflows`, { data: graphBody(id, name) });
  expect(res.ok(), `create ${id}: ${res.status()} ${await res.text()}`).toBeTruthy();
}

/**
 * Renames a saved workflow out-of-band. Journals `WorkflowUpdated`.
 *
 * The graph goes in flat — `UpdateWorkflowBody` flattens it and carries only
 * the optional `expectedVersion` alongside — and no token is sent, which is the
 * unconditional write an out-of-band caller makes.
 */
async function renameWorkflow(request: APIRequestContext, id: string, name: string) {
  const res = await request.put(`${COMPANY_SCOPE}/workflows/${id}`, { data: graphBody(id, name) });
  expect(res.ok(), `rename ${id}: ${res.status()} ${await res.text()}`).toBeTruthy();
}

/** Best-effort teardown so a failed spec does not poison the next run. */
async function removeWorkflow(request: APIRequestContext, id: string) {
  await request.delete(`${COMPANY_SCOPE}/workflows/${id}`).catch(() => undefined);
}

/** The workflow picker's trigger. */
function picker(page: Page) {
  return page.getByRole("combobox").first();
}

/** Opens the Workflows tab and waits for the picker to settle. */
async function openWorkflows(page: Page) {
  await page.goto("/#/workflows");
  await dismissTour(page);
  await expect(picker(page)).toBeEnabled({ timeout: 30_000 });
}

/**
 * How many options the picker offers under `name`, from a fresh open.
 *
 * Opened and closed around the count on purpose, so the reading never depends
 * on whether a mounted popup re-renders in place when its list changes. That is
 * a detail of the Select primitive, and it is not what this file is about — the
 * property under test is that the console re-read the list at all, and opening
 * a dropdown fetches nothing (the view lists on mount, on a company switch, and
 * on an SSE frame, and that is all).
 */
async function pickerOptionCount(page: Page, name: string): Promise<number> {
  await picker(page).click();
  await expect(page.getByRole("option").first()).toBeVisible({ timeout: 15_000 });
  const count = await page.getByRole("option", { name, exact: true }).count();
  await page.keyboard.press("Escape");
  await expect(page.getByRole("option")).toHaveCount(0);
  return count;
}

/** Selects a workflow by name and waits for the selection to settle. */
async function selectWorkflow(page: Page, name: string) {
  await picker(page).click();
  await page.getByRole("option", { name, exact: true }).click();
  await expect(picker(page)).toContainText(name);
}

test("a workflow authored elsewhere reaches the picker, with no reload", async ({
  page,
  request,
}) => {
  const stamp = Date.now();
  const id = `e2e-live-create-${stamp}`;
  const name = `Live create probe ${stamp}`;

  try {
    await openWorkflows(page);

    // The baseline the fix has to move. Without it this stays at zero for the
    // life of the tab: the `workflow_created` frame reaches the console's
    // switch, matches no arm, and is discarded.
    expect(
      await pickerOptionCount(page, name),
      "the probe must not exist before it is created",
    ).toBe(0);

    await createWorkflow(request, id, name);

    await expect
      .poll(() => pickerOptionCount(page, name), { timeout: 20_000 })
      .toBe(1);
  } finally {
    await removeWorkflow(request, id);
  }
});

test("a workflow renamed elsewhere renames in the picker, with no reload", async ({
  page,
  request,
}) => {
  const stamp = Date.now();
  const id = `e2e-live-rename-${stamp}`;
  const before = `Live rename probe ${stamp}`;
  const after = `Live renamed probe ${stamp}`;
  await createWorkflow(request, id, before);

  try {
    await openWorkflows(page);
    await selectWorkflow(page, before);

    // The graph on screen is renamed under the operator — the second-session
    // edit #259 made possible, and which nothing told this tab about.
    await renameWorkflow(request, id, after);

    // Read off the trigger, which renders the *selected* workflow's name from
    // the same list the options come from: the rename has to land on the
    // selection, not merely somewhere in a dropdown nobody has open.
    await expect(picker(page), "a rename elsewhere must reach the picker live").toContainText(
      after,
      { timeout: 20_000 },
    );
    await expect(picker(page)).not.toContainText(before);
  } finally {
    await removeWorkflow(request, id);
  }
});

test("deleting the workflow on screen elsewhere takes it out of the picker, not just greys it", async ({
  page,
  request,
}) => {
  const stamp = Date.now();
  const id = `e2e-live-delete-${stamp}`;
  const name = `Live delete probe ${stamp}`;
  await createWorkflow(request, id, name);

  try {
    await openWorkflows(page);
    await selectWorkflow(page, name);

    // Deleted from another session while this tab has it selected and its graph
    // on the canvas. This is the worst symptom in the issue: the entry used to
    // stay put, and the next Run or Edit addressed a workflow the host had
    // already dropped.
    const deleted = await request.delete(`${COMPANY_SCOPE}/workflows/${id}`);
    expect(deleted.ok(), `delete ${id}: ${deleted.status()}`).toBeTruthy();

    // The selection moves off it, so the canvas stops showing a graph the host
    // no longer has.
    await expect(picker(page), "the canvas must not stay on a deleted workflow").not.toContainText(
      name,
      { timeout: 20_000 },
    );

    // …and it is GONE from the options, not merely deselected or disabled.
    expect(
      await pickerOptionCount(page, name),
      "a deleted workflow must leave the picker, not sit in it greyed out",
    ).toBe(0);
  } finally {
    await removeWorkflow(request, id);
  }
});
