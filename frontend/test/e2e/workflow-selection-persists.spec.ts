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

async function mockCompanySwitchApi(page: Page) {
  const companies = [
    { id: "acme", name: "Acme", lifecycle: "running", pending_approvals: 0 },
    { id: "other", name: "Other", lifecycle: "running", pending_approvals: 0 },
  ];
  const workflows = {
    acme: [
      { id: "shared-workflow", name: "Acme shared workflow" },
      { id: "acme-default", name: "Acme default workflow" },
    ],
    other: [
      { id: "other-default", name: "Other default workflow" },
      { id: "shared-workflow", name: "Other shared workflow" },
    ],
  };

  await page.addInitScript(() => {
    const real = Storage.prototype.getItem;
    Storage.prototype.getItem = function getItem(key: string) {
      return key.startsWith("oc-tour:")
        ? '{"skipped":true}'
        : real.call(this, key);
    };
  });

  await page.route("**/api/v1/**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    const json = (body: unknown) =>
      route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify(body),
      });

    if (path === "/api/v1/companies") return json(companies);
    const company = path.match(/^\/api\/v1\/companies\/(acme|other)(?:\/|$)/)?.[1] as
      | keyof typeof workflows
      | undefined;
    if (!company) return json([]);
    if (path === `/api/v1/companies/${company}`)
      return json(companies.find((candidate) => candidate.id === company));
    if (path === `/api/v1/companies/${company}/workflows`) return json(workflows[company]);
    if (path.endsWith("/workflows/tool-slugs")) return json({ slugs: [] });
    if (path.endsWith("/workflows/wired-channels")) return json({ channels: [] });
    if (path.endsWith("/workflows/runs")) return json({ runs: [], hasMore: false });

    const workflowId = path.match(/\/workflows\/([^/]+)$/)?.[1];
    if (workflowId) {
      const workflow = workflows[company].find((candidate) => candidate.id === workflowId);
      if (workflow) return json(graphBody(workflow.id, workflow.name));
    }
    if (path.endsWith("/team")) return json([]);
    if (path.endsWith("/users")) return json([]);
    if (path.endsWith("/me"))
      return json({ id: "op", email: "op@example.com", role: "member" });
    if (path.endsWith("/events"))
      return route.fulfill({ status: 200, contentType: "text/event-stream", body: "" });
    return json([]);
  });
}

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

    await page.getByRole("button", { name: "Workspace" }).click();
    await page.getByRole("button", { name: "Workflows" }).click();
    await expect(openWorkflowName(page)).toHaveText(secondName);

    await page.goto(`/#/workflows/${firstId}`);
    // A full navigation, so the view has to fetch the workflow list again
    // before the heading can name anything — the default 5s assertion timeout
    // is the flake, not the console.
    await expect(openWorkflowName(page)).toHaveText(firstName, { timeout: 30_000 });

    await page.getByRole("button", { name: "Workspace" }).click();
    await page.getByRole("button", { name: "Workflows" }).click();
    await expect(openWorkflowName(page)).toHaveText(firstName);
  } finally {
    await deleteWorkflow(request, firstId);
    await deleteWorkflow(request, secondId);
  }
});

test("a company switch does not reuse the previous company's workflow route (#864)", async ({
  page,
}) => {
  await mockCompanySwitchApi(page);
  await page.goto("/#/workflows/shared-workflow");

  await page.locator('[role="button"]').filter({ hasText: "Acme" }).click();
  await expect(openWorkflowName(page)).toHaveText("Acme shared workflow", {
    timeout: 30_000,
  });

  // Companies live in the host switcher's own menu now — the sidebar footer's
  // "Switch company" row moved in there with the lifecycle line, leaving the
  // footer to the profile alone. The trigger is named by its nameplate (the
  // company and its state), so it is addressed by its test id rather than by a
  // label it deliberately does not carry.
  await page.getByTestId("host-switcher").click();
  await page.getByRole("menuitem", { name: "Other", exact: true }).click();
  // Issue #1110 sharpened what this test proves. It used to assert the switch
  // landed on the OTHER company's first workflow — `#/workflows/other-default`
  // — which was the auto-select answering a question nobody asked, and which
  // only happened to differ from the id being left behind. The switch now lands
  // on the index, and the assertion is the one #864 was actually about: the
  // previous company's route is gone from the address bar, and nothing was
  // opened in its place.
  await expectWorkflowIndex(page);
  await expect(page).toHaveURL(/#\/workflows$/);
  await expect(openWorkflowName(page)).toHaveCount(0);
  // `other` has a `shared-workflow` of its own, so a view that merely kept the
  // id would have resolved to a real graph and looked correct.
  await expect(page.getByText("Other shared workflow", { exact: true })).toBeVisible();
});
