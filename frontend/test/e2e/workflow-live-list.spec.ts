import {
  expect,
  test,
  type APIRequestContext,
  type Locator,
  type Page,
} from "@playwright/test";

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
 * Returned by {@link pickerOptionCount} when the popup could not be read at
 * all — it never opened, or the click could not land.
 *
 * A sentinel rather than `0`, because `0` is a legitimate answer this file
 * asserts twice ("the probe does not exist yet", "the deleted one is gone").
 * Reporting an unread popup as zero would pass both of those against a console
 * that rendered nothing whatsoever. A negative count matches no expectation:
 * a poll keeps polling, and a direct assertion fails saying `-1`.
 */
const UNREADABLE = -1;

/** `waitFor` reduced to a boolean — a timeout is an answer here, not a throw. */
function settled(
  locator: Locator,
  state: "visible" | "hidden",
  timeout: number,
): Promise<boolean> {
  return locator.waitFor({ state, timeout }).then(
    () => true,
    () => false,
  );
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
 *
 * **Nothing in here throws**, which is a requirement and not a style: one call
 * site runs inside `expect.poll`, and the two automated reviews on this PR
 * disagreed about whether Playwright retries a rejected poll callback or lets
 * the rejection end the poll. The lockfile resolves `@playwright/test` to
 * 1.61.1 rather than the `^1.49.0` in `package.json`, so the semantics anyone
 * reasons about here may not be the semantics that run. A callback that cannot
 * reject is correct under either reading, and that is cheaper than settling the
 * argument.
 *
 * The transient this protects against is real: the popup is opened while an
 * SSE frame is re-rendering the list underneath it, so "no options for a
 * moment" is an ordinary state on the way to the answer, not a failure.
 */
async function pickerOptionCount(page: Page, name: string): Promise<number> {
  try {
    await picker(page).click();
    // Not `expect(...).toBeVisible()`: that throws, and a popup that is empty
    // for one render is exactly what this must survive.
    if (!(await settled(page.getByRole("option").first(), "visible", 15_000))) {
      return UNREADABLE;
    }
    const count = await page.getByRole("option", { name, exact: true }).count();
    await page.keyboard.press("Escape");
    // Best effort. The count is already read, so a popup slow to close is not
    // worth discarding it over — and if it really is stuck, the next attempt's
    // click fails and reports UNREADABLE rather than a wrong number.
    await settled(page.getByRole("option").first(), "hidden", 5_000);
    return count;
  } catch {
    // A click that could not land, a detached locator mid-re-render: all of it
    // is "could not read the popup this time", never "the popup held nothing".
    return UNREADABLE;
  }
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

/**
 * The selection can now move without the operator moving it, which is what
 * makes this worth pinning: the hash mirror pushes a history entry for a
 * selection change, and that was unconditionally right while only a click could
 * cause one.
 *
 * Measured as `history.length` rather than by pressing Back, because Back's
 * destination depends on which workflow the view happened to select on mount —
 * it could legitimately be any entry in the harness company. The stack not
 * growing is the property itself, and it is true regardless.
 */
test("a delete elsewhere corrects the URL in place, without pushing history", async ({
  page,
  request,
}) => {
  const stamp = Date.now();
  const id = `e2e-live-history-${stamp}`;
  const name = `Live history probe ${stamp}`;
  await createWorkflow(request, id, name);

  try {
    await openWorkflows(page);

    // Picking it IS a navigation the operator made, so this one is a genuine
    // history entry — and it is the entry the correction has to replace.
    await selectWorkflow(page, name);
    await expect.poll(() => page.url(), { timeout: 20_000 }).toContain(id);
    const entriesBefore = await page.evaluate(() => window.history.length);

    const deleted = await request.delete(`${COMPANY_SCOPE}/workflows/${id}`);
    expect(deleted.ok(), `delete ${id}: ${deleted.status()}`).toBeTruthy();

    // The selection moves off the deleted workflow and the hash follows it.
    await expect.poll(() => page.url(), { timeout: 20_000 }).not.toContain(id);

    // …by REPLACING that entry rather than stacking on it. A push would leave
    // a URL naming a workflow the host no longer has one Back press away — an
    // address the operator could copy out and share for a graph that does not
    // exist, reached by undoing something they never did.
    expect(
      await page.evaluate(() => window.history.length),
      "correcting a deleted selection must replace the history entry, not push one",
    ).toBe(entriesBefore);
  } finally {
    await removeWorkflow(request, id);
  }
});
