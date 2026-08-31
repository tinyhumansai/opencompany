import { expect, test, type Page, type APIRequestContext } from "@playwright/test";

import { workflowDetailName } from "./workflows";

/**
 * Issue #541: the five withheld node kinds (`tool_call`, `http_request`,
 * `switch`, `output_parser`, `sub_workflow`) grew config forms in the workflow
 * creator. The engine has always run them; only the console lacked controls, so
 * an operator could not author (say) a tool call from the UI.
 *
 * This spec covers the half only a browser proves: authoring a `tool_call`
 * node through the dialog's config form, saving it, and confirming the config
 * the form emitted actually round-tripped through the host — reopened from the
 * saved graph and shown in the node inspector. A unit test pins the string
 * transforms (`workflow-node-config.test.ts`); this pins that the form is wired
 * to them and that the host stored what it produced.
 *
 * Runs against the live host the harness brings up (see `playwright.config.ts`).
 * Not run by CI, which has no host.
 */

const COMPANY_SCOPE = "/api/v1/company";

/** Dismisses the first-run tour if it is up; its overlay swallows pointer
 * events. Tolerates its absence. */
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

/** Best-effort teardown so a failed spec does not poison the next run. */
/**
 * Best-effort teardown so a failed spec does not poison the next run.
 * `expectedVersion` is required (issue #1013), so this reads the workflow's
 * current token first.
 */
async function removeWorkflow(request: APIRequestContext, id: string) {
  const version = await request
    .get(`${COMPANY_SCOPE}/workflows/${id}`)
    .then(async (res) => (res.ok() ? ((await res.json()).version as string | null) : null))
    .catch(() => null);
  const query = version ? `?expectedVersion=${encodeURIComponent(version)}` : "";
  await request.delete(`${COMPANY_SCOPE}/workflows/${id}${query}`).catch(() => undefined);
}

const SUBMIT = "workflow-dialog-submit";

test("authoring a tool_call node's config through the form round-trips to the host", async ({
  page,
  request,
}) => {
  const stamp = Date.now();
  const id = `e2e-cfg-${stamp}`;
  const name = `Config probe ${stamp}`;

  try {
    await page.goto("/#/workflows");
    await dismissTour(page);

    // Open the creator.
    await page.getByRole("button", { name: "New workflow" }).click();
    const dialog = page.getByRole("dialog");
    await expect(dialog.getByText("New workflow", { exact: true })).toBeVisible();

    // Name the workflow.
    await dialog.getByLabel("Workflow ID", { exact: true }).fill(id);
    await dialog.getByLabel("Name", { exact: true }).fill(name);

    // The starter trigger row is already `start`. Add a second node and make it
    // a tool call — its config form appears only once the kind is chosen.
    await dialog.getByRole("button", { name: "Add node" }).click();
    const nodeId = dialog.getByRole("textbox", { name: "Node id" }).nth(1);
    await nodeId.fill("act");

    // Change the second row's kind to Tool call.
    await dialog.getByRole("combobox", { name: "Node kind" }).nth(1).click();
    await page.getByRole("option", { name: /Tool call/ }).click();

    await dialog.getByRole("textbox", { name: "Node name" }).nth(1).fill("Do it");

    // The config form is now mounted. Fill the engine-exact keys.
    await dialog.getByRole("textbox", { name: "Tool slug", exact: true }).fill("web_fetch");
    await dialog
      .getByRole("textbox", { name: "Arguments", exact: true })
      .fill('{ "url": "https://example.com" }');

    // Connect trigger → tool call so the graph is valid.
    await dialog.getByRole("button", { name: "Add edge" }).click();
    await dialog.getByRole("combobox", { name: "Edge from" }).click();
    await page.getByRole("option", { name: "start", exact: true }).click();
    await dialog.getByRole("combobox", { name: "Edge to" }).click();
    await page.getByRole("option", { name: "act", exact: true }).click();

    // Issue #1808: create mode gates the write behind an id-confirm. The first
    // click only opens it (portalled onto `document.body`, so it is reached by
    // test id on the page, not scoped to `dialog`); the confirm's own action —
    // also rendered "Create workflow" — is what fires the write.
    await dialog.getByTestId(SUBMIT).click();
    await page.getByTestId("workflow-id-confirm-create").click();
    await expect(dialog).toBeHidden({ timeout: 15_000 });

    // The host stored exactly the keys the form emitted — not the node-id
    // fallback, and with the args object intact.
    await expect
      .poll(
        async () => {
          const res = await request.get(`${COMPANY_SCOPE}/workflows/${id}`);
          if (!res.ok()) return null;
          const graph = await res.json();
          return graph.nodes.find((n: { id: string }) => n.id === "act")?.config ?? null;
        },
        { timeout: 15_000 },
      )
      .toEqual({ slug: "web_fetch", args: { url: "https://example.com" } });

    // Reopen from the saved graph (a fresh load, not local state) and click the
    // node: the inspector's Config block shows the slug the host round-tripped.
    //
    // Issue #1110: creating landed on the new workflow's own URL, so the reload
    // comes back on its detail view with no picking to do — which is also this
    // spec's incidental proof that a `#/workflows/<id>` survives a reload.
    await expect(page).toHaveURL(new RegExp(`#/workflows/${id}$`));
    await page.reload();
    await dismissTour(page);
    await expect(workflowDetailName(page)).toHaveText(name, { timeout: 30_000 });

    const node = page.locator('.react-flow__node[data-id="act"]');
    await expect(node).toBeVisible({ timeout: 15_000 });
    await node.click();

    await expect(page.getByText("Config", { exact: true })).toBeVisible();
    await expect(page.getByText(/"slug": "web_fetch"/)).toBeVisible();
  } finally {
    await removeWorkflow(request, id);
  }
});
