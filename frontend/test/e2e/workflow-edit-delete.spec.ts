import { expect, test, type Page, type APIRequestContext } from "@playwright/test";

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
 * created over HTTP, the tour dismissal, the picker helper. They cover:
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

/** A minimal valid graph body: one trigger, one output, one edge. */
function graphBody(id: string, name: string, schedule: string, description: string) {
  return {
    id,
    name,
    description,
    nodes: [
      { id: "start", kind: "trigger", name: "Start", schedule },
      { id: "done", kind: "output", name: "Report" },
    ],
    edges: [{ from: "start", to: "done" }],
  };
}

/** Creates a workflow over HTTP and returns its version token. */
async function createWorkflow(
  request: APIRequestContext,
  id: string,
  name: string,
): Promise<string> {
  const res = await request.post(`${COMPANY_SCOPE}/workflows`, {
    data: graphBody(id, name, "0 9 * * *", "Created by the #259 e2e spec."),
  });
  expect(res.ok(), `create ${id}: ${res.status()} ${await res.text()}`).toBeTruthy();
  const body = await res.json();
  expect(body.editable, "a console-created workflow must be editable").toBe(true);
  expect(body.version, "a console-created workflow must carry a version token").toBeTruthy();
  return body.version as string;
}

/** Best-effort teardown so a failed spec does not poison the next run. */
async function removeWorkflow(request: APIRequestContext, id: string) {
  await request.delete(`${COMPANY_SCOPE}/workflows/${id}`).catch(() => undefined);
}

/**
 * Selects the workflow named `name` in the picker and waits for the selection
 * to settle.
 *
 * Asserts the trigger shows the **name**. It used to show the raw id — the
 * picker binds `<SelectItem value={w.id}>` and Base UI's `SelectValue` renders
 * the value, not the item's children — which is #270, fixed on this branch
 * because it is the same picker this issue's Delete affordance sits next to.
 */
async function selectWorkflow(page: Page, name: string) {
  await page.getByRole("combobox").first().click();
  await page.getByRole("option", { name, exact: true }).click();
  await expect(page.getByRole("combobox").first()).toContainText(name);
}

const DELETE = "workflow-delete";
const EDIT = "workflow-edit";
const SUBMIT = "workflow-dialog-submit";

/** Opens the edit dialog for the currently selected workflow. */
async function openEditDialog(page: Page) {
  const button = page.getByTestId(EDIT);
  await expect(button, "an overlay-backed workflow must be editable").toBeEnabled();
  await button.click();
  const dialog = page.getByRole("dialog");
  await expect(dialog.getByText("Edit workflow", { exact: true })).toBeVisible();
  return dialog;
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

test("confirming the delete removes it from the picker and from the host", async ({
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

    // Gone from the picker: the selection moves off it and it is no longer an
    // option.
    await expect(page.getByRole("combobox").first()).not.toContainText(name, {
      timeout: 15_000,
    });

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
  await createWorkflow(request, id, name);

  try {
    await page.goto("/#/workflows");
    await dismissTour(page);
    await selectWorkflow(page, name);

    // Force the race for real: someone else edits the graph while this console
    // holds the token it loaded a moment ago.
    const edited = await request.put(`${COMPANY_SCOPE}/workflows/${id}`, {
      data: graphBody(id, name, "0 11 * * *", "Edited out-of-band, after the console loaded it."),
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
  await createWorkflow(request, id, name);

  try {
    await page.goto("/#/workflows");
    await dismissTour(page);
    await selectWorkflow(page, name);

    const dialog = await openEditDialog(page);

    // Hydrated from the saved graph, not blank — the whole point of edit mode.
    await expect(dialog.getByLabel("Id", { exact: true })).toHaveValue(id);
    await expect(dialog.getByLabel("Name", { exact: true })).toHaveValue(name);
    await expect(dialog.getByRole("textbox", { name: "Node id" }).first()).toHaveValue("start");

    // The id may not change through a PUT — the host answers 400 — so the field
    // states that by being unwritable rather than by refusing after the click.
    await expect(
      dialog.getByLabel("Id", { exact: true }),
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
  await createWorkflow(request, id, name);

  try {
    await page.goto("/#/workflows");
    await dismissTour(page);
    await selectWorkflow(page, name);

    const dialog = await openEditDialog(page);

    // Someone else saves while this dialog holds the token it loaded a moment
    // ago. Forced for real, as with the delete conflict above.
    const edited = await request.put(`${COMPANY_SCOPE}/workflows/${id}`, {
      data: graphBody(id, name, "0 11 * * *", "Edited out-of-band, mid-edit."),
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
    const banner = page.getByTestId("workflow-conflict");
    await expect(banner).toBeVisible();
    await banner.getByTestId("workflow-conflict-reload").click();
    await expect(banner).toBeHidden({ timeout: 15_000 });

    // Reloaded, the same edit goes through — so the loop terminates.
    const reopened = await openEditDialog(page);
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
    const dialog = await openEditDialog(page);
    await dialog.getByRole("combobox", { name: "Schedule" }).click();
    await page.getByRole("option", { name: /Custom cron/ }).click();
    const cron = dialog.getByRole("textbox", { name: "Custom cron schedule" });
    await cron.fill("hourly");
    await cron.blur();
    // The rule, not the standing hint under the field (which also says
    // "5-field cron") — those are different strings for a reason.
    await expect(dialog).toContainText(/A schedule is a 5-field cron/);
    await dialog.getByRole("button", { name: "Cancel" }).click();

    // Both graphs have a trigger node whose id is `start`, so an error map keyed
    // on the node id (rather than on the freshly minted row key) would carry
    // this complaint straight over to the second graph, attached to a field the
    // author never touched.
    await selectWorkflow(page, second.name);
    const other = await openEditDialog(page);
    await expect(
      other,
      "a stale field error must not follow the author to another graph",
    ).not.toContainText("5-field cron, e.g.");
  } finally {
    await removeWorkflow(request, first.id);
    await removeWorkflow(request, second.id);
  }
});
