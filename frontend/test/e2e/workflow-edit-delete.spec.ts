import { expect, test, type Page, type APIRequestContext } from "@playwright/test";

import {
  backToWorkflowIndex,
  expectWorkflowIndex,
  openWorkflow,
  workflowCard,
  workflowDetailName,
} from "./workflows";

/**
 * Issue #259: a saved workflow used to be write-once. There was no `PUT` and no
 * `DELETE`, so a typo'd cron or a node pointed at the wrong teammate was
 * permanent — the only recovery was to author a second workflow and leave the
 * broken one in the picker, still firing on its schedule.
 *
 * These specs cover the half that is only observable in a browser:
 *
 * * the Delete affordance is **disabled with an explanation** for a workflow
 *   defined by a file in the company source tree (the host answers 409, and the
 *   console must say so before the click, not after);
 * * deleting is confirm-gated, and the confirm copy states both what goes (the
 *   workflow, its schedule) and what stays (past runs);
 * * a **version conflict surfaces distinctly and recoverably**. This is the one
 *   that matters most: the console must never silently overwrite an edit it
 *   never saw. The spec forces the race for real — it edits the graph
 *   out-of-band over HTTP while the console holds a stale copy — rather than
 *   trusting that the token is wired through.
 *
 * Edit mode landed after the rest of #259 (the backend, the client and Delete
 * shipped in #279), and its specs are here rather than in their own file
 * because they are the same feature and share this file's fixtures — a graph
 * created over HTTP, the tour dismissal, the helper that opens one. They
 * cover:
 *
 * * an edit round trip that the host actually stored;
 * * the id being **read-only**, not merely rejected on save (the host answers
 *   400 to a rename, so offering the field would be a trap);
 * * a stale save landing in the same conflict banner Delete uses, having
 *   written nothing;
 * * field errors **not** carrying from one graph to the next, which is what
 *   the fresh-row-key hydration exists to prevent and is invisible anywhere
 *   but a browser.
 *
 * Runs against the live host the harness brings up (see `playwright.config.ts`).
 * Not run by CI, which has no host.
 */

const COMPANY_SCOPE = "/api/v1/company";

/**
 * Dismisses the first-run tour if it is up. Its overlay swallows pointer
 * events, so without this the specs fail on an unrelated modal. Tolerates its
 * absence — a company that has seen it never shows it again.
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

/**
 * Node fields the dialog has no control for. `outputExtras` puts them on the
 * report node so a spec can check they survive an edit — the dialog cannot show
 * them, and a save that rebuilds nodes from the visible controls alone would
 * delete them.
 */
type NodeExtras = Record<string, unknown>;

/** A minimal valid graph body: one trigger, one output, one edge. */
function graphBody(
  id: string,
  name: string,
  schedule: string,
  description: string,
  outputExtras: NodeExtras = {},
) {
  return {
    id,
    name,
    description,
    nodes: [
      { id: "start", kind: "trigger", name: "Start", schedule },
      { id: "done", kind: "output", name: "Report", ...outputExtras },
    ],
    edges: [{ from: "start", to: "done" }],
  };
}

/** Creates a workflow over HTTP and returns its version token. */
async function createWorkflow(
  request: APIRequestContext,
  id: string,
  name: string,
  outputExtras: NodeExtras = {},
): Promise<string> {
  const res = await request.post(`${COMPANY_SCOPE}/workflows`, {
    data: graphBody(id, name, "0 9 * * *", "Created by the #259 e2e spec.", outputExtras),
  });
  expect(res.ok(), `create ${id}: ${res.status()} ${await res.text()}`).toBeTruthy();
  const body = await res.json();
  expect(body.editable, "a console-created workflow must be editable").toBe(true);
  expect(body.version, "a console-created workflow must carry a version token").toBeTruthy();
  return body.version as string;
}

/**
 * Best-effort teardown so a failed spec does not poison the next run.
 * `expectedVersion` is required (issue #1013), so this reads the workflow's
 * current token first — a spec's own copy may be stale by teardown time.
 */
async function removeWorkflow(request: APIRequestContext, id: string) {
  const version = await request
    .get(`${COMPANY_SCOPE}/workflows/${id}`)
    .then(async (res) => (res.ok() ? ((await res.json()).version as string | null) : null))
    .catch(() => null);
  const query = version ? `?expectedVersion=${encodeURIComponent(version)}` : "";
  await request.delete(`${COMPANY_SCOPE}/workflows/${id}${query}`).catch(() => undefined);
}

/**
 * Opens the workflow named `name`, from the index, and waits for its detail
 * view to settle.
 *
 * Issue #1110: `#/workflows` is the index, and Edit and Delete are controls of
 * one workflow — they exist only once one is open. This used to drive the
 * toolbar picker, which was reachable on arrival because the view auto-selected
 * the first row.
 *
 * Issue #1135 then took the picker out of the detail toolbar altogether: you
 * are inside this workflow, its name is the heading, and "All workflows" goes
 * back. #270's property — the console names the workflow, it does not show the
 * raw id — survives on that heading, which `openWorkflow` asserts by exact
 * text. The second line here pins the removal itself, so a picker cannot creep
 * back into the bar the issue cleared.
 */
async function selectWorkflow(page: Page, name: string) {
  await openWorkflow(page, name);
  await expect(
    page.getByTestId("workflow-detail-toolbar").getByRole("combobox"),
    "the detail toolbar no longer offers the workflow you are already in",
  ).toHaveCount(0);
}

const DELETE = "workflow-delete";
const EDIT = "workflow-edit";
const SUBMIT = "workflow-dialog-submit";

/**
 * Opens the edit dialog for the currently selected workflow, which must be the
 * one called `name`.
 *
 * The title used to read a bare "Edit workflow" from every call site. It now
 * names the graph it is editing (issue #1006), because the dialog is reachable
 * from a canvas, a card and a run, and an operator whose selection moved
 * underneath them had nothing on screen to notice it with. Asserting the name
 * here is therefore not a cosmetic check: it is what proves the dialog opened
 * on the graph the caller selected.
 */
async function openEditDialog(page: Page, name: string) {
  const button = page.getByTestId(EDIT);
  await expect(button, "an overlay-backed workflow must be editable").toBeEnabled();
  await button.click();
  const dialog = page.getByRole("dialog");
  await expect(dialog.getByText(`Edit “${name}”`, { exact: true })).toBeVisible();
  return dialog;
}

/**
 * Confirms the discard prompt a dirty dialog's Cancel raises (issue #1006).
 *
 * That prompt used to be `window.confirm`, answerable with a
 * `page.once("dialog", ...)` handler. Defect B-081 (`d5c710e06`) replaced it
 * with the console's own `AlertDialog` — a `window.confirm` an automated
 * environment declines on the operator's behalf made the editor inescapable,
 * only a page reload got out, and a reload destroys the graph being edited.
 * The in-app dialog fixes that for a real user, but it is not a native
 * dialog: nothing here fires Playwright's `dialog` event, so the old
 * `page.once("dialog", ...)` handler this replaced was inert. It left the
 * "Leave without saving?" `AlertDialog` standing over the page, and its
 * full-screen overlay swallowed every click a spec made afterwards —
 * `workflow-conflict-reload` or `workflow-back-to-index` timing out with
 * `alert-dialog-overlay` intercepting pointer events, never the button
 * itself missing.
 */
async function confirmDiscard(page: Page) {
  const confirm = page.getByTestId("workflow-discard-confirm");
  await expect(confirm).toBeVisible();
  await confirm.getByTestId("workflow-discard-leave").click();
  await expect(confirm).toBeHidden();
}

/** Reads a workflow's stored graph back from the host. */
async function readWorkflow(request: APIRequestContext, id: string) {
  const res = await request.get(`${COMPANY_SCOPE}/workflows/${id}`);
  expect(res.ok(), `read ${id}: ${res.status()}`).toBeTruthy();
  return res.json();
}

test("a source-defined workflow cannot be deleted, and the console says why", async ({
  page,
}) => {
  await page.goto("/#/workflows");
  await dismissTour(page);

  await selectWorkflow(page, "Committed flow");

  const button = page.getByTestId(DELETE);
  await expect(button).toBeVisible();
  await expect(button, "a seed-backed workflow must not be deletable").toBeDisabled();

  // The explanation lives on the wrapper (a disabled button swallows hover), and
  // it must name the actual remedy, not just refuse.
  const explanation = page.locator(`span:has([data-testid="${DELETE}"])`);
  await expect(explanation).toHaveAttribute("title", /company source tree/);
  await expect(explanation).toHaveAttribute("title", /workflows\/committed\.toml/);
});

test("deleting is confirm-gated, and the confirmation says what goes and what stays", async ({
  page,
  request,
}) => {
  const id = `e2e-confirm-${Date.now()}`;
  const name = `Confirm probe ${Date.now()}`;
  await createWorkflow(request, id, name);

  try {
    await page.goto("/#/workflows");
    await dismissTour(page);
    await selectWorkflow(page, name);

    const button = page.getByTestId(DELETE);
    await expect(button, "an overlay-backed workflow must be deletable").toBeEnabled();
    await button.click();

    const dialog = page.getByRole("alertdialog");
    await expect(dialog).toBeVisible();
    await expect(dialog).toContainText(name);
    // The two consequences an operator needs before committing.
    await expect(dialog, "must say the schedule stops").toContainText(/stops it running on its schedule/);
    await expect(dialog, "must say history is kept").toContainText(/Past runs stay/);

    // Backing out leaves it alone — a confirm gate that deletes anyway is worse
    // than none.
    await dialog.getByRole("button", { name: "Keep it" }).click();
    await expect(dialog).toBeHidden();
    const still = await request.get(`${COMPANY_SCOPE}/workflows/${id}`);
    expect(still.ok(), "cancelling must not delete").toBeTruthy();
  } finally {
    await removeWorkflow(request, id);
  }
});

test("confirming the delete returns to the index, and removes it from the host", async ({
  page,
  request,
}) => {
  const id = `e2e-delete-${Date.now()}`;
  const name = `Delete probe ${Date.now()}`;
  await createWorkflow(request, id, name);

  try {
    await page.goto("/#/workflows");
    await dismissTour(page);
    await selectWorkflow(page, name);

    await page.getByTestId(DELETE).click();
    await page.getByTestId("workflow-delete-confirm").click();

    // **Regression pin.** `AlertDialogAction` is a plain `Button`, not an
    // `AlertDialogPrimitive.Close` — only `AlertDialogCancel` is — so
    // confirming does NOT dismiss the dialog unless the view closes it
    // explicitly. Without that, the modal stays up over a view whose workflow
    // has just been deleted, rendering `Delete “”?`, with its backdrop
    // swallowing every click on the console behind it. Caught here first.
    await expect(
      page.getByRole("alertdialog"),
      "confirming must dismiss the dialog, not leave its backdrop blocking the app",
    ).toBeHidden({ timeout: 15_000 });

    // Issue #1110: back on the INDEX, and the deleted workflow is not in it.
    // The old assertion — that the picker no longer names it — could only be
    // made from inside whichever neighbouring workflow the view auto-selected
    // next, which is the behaviour this issue removed: deleting what you were
    // working on returns you to the list, it does not hand you somebody else's
    // graph. The URL going back to `#/workflows` is the other half of that.
    await expectWorkflowIndex(page);
    await expect(page).toHaveURL(/#\/workflows$/, { timeout: 15_000 });
    await expect(workflowCard(page, name)).toHaveCount(0, { timeout: 15_000 });

    // And gone from the host — the console did not merely hide it.
    await expect
      .poll(async () => (await request.get(`${COMPANY_SCOPE}/workflows/${id}`)).status(), {
        timeout: 15_000,
      })
      .toBe(404);
  } finally {
    await removeWorkflow(request, id);
  }
});

test("a version conflict surfaces distinctly with a way out, and deletes nothing", async ({
  page,
  request,
}) => {
  const id = `e2e-conflict-${Date.now()}`;
  const name = `Conflict probe ${Date.now()}`;
  const createdVersion = await createWorkflow(request, id, name);

  try {
    await page.goto("/#/workflows");
    await dismissTour(page);
    await selectWorkflow(page, name);

    // Force the race for real: someone else edits the graph while this console
    // holds the token it loaded a moment ago. `expectedVersion` is required
    // (issue #1013) even for this first out-of-band write, so it goes in as the
    // token `createWorkflow` handed back.
    const edited = await request.put(`${COMPANY_SCOPE}/workflows/${id}`, {
      data: {
        ...graphBody(id, name, "0 11 * * *", "Edited out-of-band, after the console loaded it."),
        expectedVersion: createdVersion,
      },
    });
    expect(edited.ok(), `out-of-band edit: ${edited.status()}`).toBeTruthy();

    await page.getByTestId(DELETE).click();
    await page.getByTestId("workflow-delete-confirm").click();

    // The conflict gets its own persistent banner — NOT a toast that fades and
    // NOT the generic load error — because the operator has to act on it.
    const banner = page.getByTestId("workflow-conflict");
    await expect(banner, "a 409 must surface distinctly").toBeVisible({ timeout: 15_000 });
    await expect(banner).toContainText(/changed since you loaded it/);
    await expect(banner).toContainText(/[Rr]eload/);

    // Nothing was deleted — this is the whole point of the guard.
    const survived = await request.get(`${COMPANY_SCOPE}/workflows/${id}`);
    expect(survived.ok(), "a refused delete must remove nothing").toBeTruthy();
    expect((await survived.json()).nodes[0].schedule).toBe("0 11 * * *");

    // The way out works: Reload re-reads the graph (and a fresh token), and the
    // banner clears.
    await banner.getByTestId("workflow-conflict-reload").click();
    await expect(banner).toBeHidden({ timeout: 15_000 });

    // …and now the delete goes through, so the loop actually terminates.
    await page.getByTestId(DELETE).click();
    await page.getByTestId("workflow-delete-confirm").click();
    await expect
      .poll(async () => (await request.get(`${COMPANY_SCOPE}/workflows/${id}`)).status(), {
        timeout: 15_000,
      })
      .toBe(404);
  } finally {
    await removeWorkflow(request, id);
  }
});

// ---------------------------------------------------------------------------
// Edit mode (issue #259, the remainder after #279)
// ---------------------------------------------------------------------------

test("a source-defined workflow offers no Edit, with the same explanation Delete gives", async ({
  page,
}) => {
  await page.goto("/#/workflows");
  await dismissTour(page);

  await selectWorkflow(page, "Committed flow");

  const button = page.getByTestId(EDIT);
  await expect(button).toBeVisible();
  await expect(button, "a seed-backed workflow must not be editable").toBeDisabled();

  // One refusal, one explanation: the host answers 409 to a PUT and a DELETE
  // for the same reason, so the console must not tell two stories about it.
  const explanation = page.locator(`span:has([data-testid="${EDIT}"])`);
  await expect(explanation).toHaveAttribute("title", /company source tree/);
  await expect(explanation).toHaveAttribute("title", /can't be changed or removed/);
  await expect(explanation).toHaveAttribute("title", /workflows\/committed\.toml/);
});

test("an author can change a saved workflow's schedule, and the id is read-only", async ({
  page,
  request,
}) => {
  const id = `e2e-edit-${Date.now()}`;
  const name = `Edit probe ${Date.now()}`;
  // Policies the dialog cannot show. A graph carrying them is not exotic: the
  // API accepts them and the orchestrator's own `create_workflow` tool writes
  // them, so an author can easily be editing one.
  const hidden = {
    config: { note: "kept across an edit" },
    onError: "continue",
    retry: { maxAttempts: 2, backoff: "fixed" },
    requiresApproval: true,
  };
  await createWorkflow(request, id, name, hidden);

  try {
    await page.goto("/#/workflows");
    await dismissTour(page);
    await selectWorkflow(page, name);

    const dialog = await openEditDialog(page, name);

    // Hydrated from the saved graph, not blank — the whole point of edit mode.
    await expect(dialog.getByLabel("Workflow ID", { exact: true })).toHaveValue(id);
    await expect(dialog.getByLabel("Name", { exact: true })).toHaveValue(name);
    await expect(dialog.getByRole("textbox", { name: "Node id" }).first()).toHaveValue("start");

    // The id may not change through a PUT — the host answers 400 — so the field
    // states that by being unwritable rather than by refusing after the click.
    await expect(
      dialog.getByLabel("Workflow ID", { exact: true }),
      "an id field that accepts a rename can only ever 400",
    ).toHaveJSProperty("readOnly", true);

    // Correct the trigger's schedule: the very mistake #259 is about.
    await dialog.getByRole("combobox", { name: "Schedule" }).click();
    await page.getByRole("option", { name: /Weekly/ }).click();

    await dialog.getByTestId(SUBMIT).click();
    await expect(dialog).toBeHidden({ timeout: 15_000 });

    // The host stored it — the console did not merely redraw its own copy.
    await expect
      .poll(async () => (await readWorkflow(request, id)).nodes[0].schedule, {
        timeout: 15_000,
      })
      .toBe("0 9 * * MON");

    // …and the save did not quietly delete the policies it had no control for.
    // A node rebuilt from the visible fields alone would have dropped every one
    // of these, silently, on the first edit of any field at all.
    const report = (await readWorkflow(request, id)).nodes[1];
    expect(report.config, "an edit must not drop node config").toEqual(hidden.config);
    expect(report.onError, "an edit must not drop the error policy").toBe("continue");
    expect(report.retry, "an edit must not drop the retry policy").toEqual(hidden.retry);
    expect(report.requiresApproval, "an edit must not drop an approval gate").toBe(true);
  } finally {
    await removeWorkflow(request, id);
  }
});

test("a stale save surfaces the conflict banner and writes nothing", async ({
  page,
  request,
}) => {
  const id = `e2e-edit-conflict-${Date.now()}`;
  const name = `Edit conflict probe ${Date.now()}`;
  const createdVersion = await createWorkflow(request, id, name);

  try {
    await page.goto("/#/workflows");
    await dismissTour(page);
    await selectWorkflow(page, name);

    const dialog = await openEditDialog(page, name);

    // Someone else saves while this dialog holds the token it loaded a moment
    // ago. Forced for real, as with the delete conflict above. `expectedVersion`
    // is required (issue #1013), so this out-of-band write needs the token
    // `createWorkflow` returned.
    const edited = await request.put(`${COMPANY_SCOPE}/workflows/${id}`, {
      data: {
        ...graphBody(id, name, "0 11 * * *", "Edited out-of-band, mid-edit."),
        expectedVersion: createdVersion,
      },
    });
    expect(edited.ok(), `out-of-band edit: ${edited.status()}`).toBeTruthy();

    await dialog.getByRole("combobox", { name: "Schedule" }).click();
    await page.getByRole("option", { name: /Weekly/ }).click();
    await dialog.getByTestId(SUBMIT).click();

    // Refused, and said so where the author is looking. The dialog stays open:
    // throwing away what they typed would be a second loss on top of the race.
    await expect(dialog).toBeVisible();
    await expect(dialog).toContainText(/changed since you loaded it/, { timeout: 15_000 });

    // Nothing was written — the out-of-band value still stands.
    expect((await readWorkflow(request, id)).nodes[0].schedule).toBe("0 11 * * *");

    // And the way out is the banner Delete already had, waiting behind the
    // dialog rather than vanishing with it.
    await dialog.getByRole("button", { name: "Cancel" }).click();
    await confirmDiscard(page);
    const banner = page.getByTestId("workflow-conflict");
    await expect(banner).toBeVisible();
    await banner.getByTestId("workflow-conflict-reload").click();
    await expect(banner).toBeHidden({ timeout: 15_000 });

    // Reloaded, the same edit goes through — so the loop terminates.
    const reopened = await openEditDialog(page, name);
    await reopened.getByRole("combobox", { name: "Schedule" }).click();
    await page.getByRole("option", { name: /Weekly/ }).click();
    await reopened.getByTestId(SUBMIT).click();
    await expect(reopened).toBeHidden({ timeout: 15_000 });
    await expect
      .poll(async () => (await readWorkflow(request, id)).nodes[0].schedule, {
        timeout: 15_000,
      })
      .toBe("0 9 * * MON");
  } finally {
    await removeWorkflow(request, id);
  }
});

test("a field error raised on one graph never appears on another", async ({
  page,
  request,
}) => {
  const stamp = Date.now();
  const first = { id: `e2e-errors-a-${stamp}`, name: `Errors probe A ${stamp}` };
  const second = { id: `e2e-errors-b-${stamp}`, name: `Errors probe B ${stamp}` };
  await createWorkflow(request, first.id, first.name);
  await createWorkflow(request, second.id, second.name);

  try {
    await page.goto("/#/workflows");
    await dismissTour(page);
    await selectWorkflow(page, first.name);

    // Raise a real blur-time error on the first graph's trigger row.
    const dialog = await openEditDialog(page, first.name);
    await dialog.getByRole("combobox", { name: "Schedule" }).click();
    await page.getByRole("option", { name: /Custom cron/ }).click();
    const cron = dialog.getByRole("textbox", { name: "Custom cron schedule" });
    await cron.fill("hourly");
    await cron.blur();
    // The rule, not the standing hint under the field (which also says
    // "5-field cron") — those are different strings for a reason.
    await expect(dialog).toContainText(/A schedule is a 5-field cron/);
    await dialog.getByRole("button", { name: "Cancel" }).click();
    await confirmDiscard(page);

    // Both graphs have a trigger node whose id is `start`, so an error map keyed
    // on the node id (rather than on the freshly minted row key) would carry
    // this complaint straight over to the second graph, attached to a field the
    // author never touched.
    // Issue #1135: switched through the INDEX, because that is now the only way
    // to move between two graphs — the toolbar picker this step used to drive
    // is gone, and "All workflows" is the way out.
    //
    // Still the case the defect needs: `WorkflowsView` itself never unmounts on
    // this trip (only the branch inside it re-renders), so the field-error map
    // the issue is about survives the switch exactly as it did through the
    // picker and has every chance to ride across.
    await backToWorkflowIndex(page);
    await openWorkflow(page, second.name);
    await expect(workflowDetailName(page)).toHaveText(second.name);

    const other = await openEditDialog(page, second.name);
    await expect(
      other,
      "a stale field error must not follow the author to another graph",
    ).not.toContainText("5-field cron, e.g.");
  } finally {
    await removeWorkflow(request, first.id);
    await removeWorkflow(request, second.id);
  }
});
