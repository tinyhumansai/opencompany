import { expect, test, type Locator, type Page } from "@playwright/test";

/**
 * End-to-end proof for issue #1002 — a run parked on a gated node is unblocked
 * from the workflow window, without leaving it.
 *
 * Before this, the run drawer *told* the operator: a "parked N approvals" badge
 * and a "Not finished — …" sentence. Acting on it meant leaving the run,
 * finding the row in a flat queue on the Approvals page, deciding it, and
 * navigating back. `resolveApproval` was reachable from exactly two places, and
 * neither of them was the screen the operator was already looking at.
 *
 * Written in the spirit of `chat-inline-approval.spec.ts`, and asserting the
 * same four things one surface over: **placement** (a card belongs to the run
 * that parked it, and must not appear under another), **addition** (the
 * Approvals page still shows everything, including this one), the **verb** (one
 * ordinary per-id resolve, never a batch), and **settling** (a decision made on
 * the page must not leave a stale card offering buttons in the run window).
 *
 * The approvals feed, the run body and the event stream are **mocked**, and
 * deliberately so. Parking a real workflow gate needs an inference backend and
 * a model that chooses a policy-gated call, and none of what is under test is
 * the parking. Only the *workflow* itself is real — authored over HTTP against
 * the live host, so the picker and the Run button are the console's own.
 *
 * Like the rest of `test/e2e` this needs a running host; `playwright.config.ts`
 * brings one up for the `Console E2E` lane (issue #428).
 */

const COMPANY_SCOPE = "/api/v1/company";

/** The run the console will be handed back, and the id every card joins on. */
const RUN_ID = "run-e2e-1002";

/** The graph the spec authors, runs, and tears down. */
const WORKFLOW = { id: "inline_approval_1002", name: "Inline approval 1002" };

/** The card this run parked, on its "spec" node. */
const PARKED = {
  id: "appr-run-1002",
  kind: "external.publish",
  amount_usd: null,
  at_millis: Date.now(),
  task: { link: "unlinked" },
  agent: "writer",
  payload: { path: "/notes/quarterly-update.md" },
  workflow_run_id: RUN_ID,
};

/**
 * A card another workflow run parked. It is served on the same queue — nothing
 * upstream filters it — so it is the leak this surface must not have.
 */
const OTHER_RUN = {
  id: "appr-other-run",
  kind: "external.publish",
  amount_usd: null,
  at_millis: Date.now(),
  task: { link: "unlinked" },
  agent: "writer",
  payload: { path: "/notes/somebody-elses.md" },
  workflow_run_id: "run-somebody-else",
};

/** A card no workflow run parked — a chat turn or a scheduler tick. */
const PAGE_ONLY = {
  id: "appr-page-only-1002",
  kind: "email.send",
  amount_usd: null,
  at_millis: Date.now(),
  task: { link: "unlinked" },
};

/** The settled run body: blocked on "spec", with its frozen park receipt. */
const BLOCKED_RUN = {
  output: { nodes: {} },
  pendingApprovals: ["spec"],
  runId: RUN_ID,
  deliveries: [],
  nodes: [{ nodeId: "spec", status: "blocked", elapsedMs: 1_200 }],
  blockedNodes: [{ nodeId: "spec", tools: ["publish_artifact"], approvalIds: [PARKED.id] }],
  approvals: [
    { nodeId: "spec", tool: "publish_artifact", outcome: "parked", approvalId: PARKED.id },
  ],
};

function graphBody() {
  return {
    id: WORKFLOW.id,
    name: WORKFLOW.name,
    description: "Created by the #1002 e2e spec.",
    nodes: [
      { id: "start", kind: "trigger", name: "Start" },
      { id: "spec", kind: "agent", name: "Draft the note", agent: "ceo" },
      { id: "done", kind: "output", name: "Report" },
    ],
    edges: [
      { from: "start", to: "spec" },
      { from: "spec", to: "done" },
    ],
  };
}

test.beforeEach(async ({ page, request }) => {
  // Skip the first-run tour, whose overlay would swallow the clicks below.
  await page.addInitScript(() => {
    const real = Storage.prototype.getItem;
    Storage.prototype.getItem = function getItem(key: string) {
      return key.startsWith("oc-tour:") ? '{"skipped":true}' : real.call(this, key);
    };
  });
  const res = await request.post(`${COMPANY_SCOPE}/workflows`, { data: graphBody() });
  expect(res.ok(), `create ${WORKFLOW.id}: ${res.status()} ${await res.text()}`).toBeTruthy();
});

// `expectedVersion` is required (issue #1013), so teardown reads the
// workflow's current token first rather than sending a bare DELETE.
test.afterEach(async ({ request }) => {
  const version = await request
    .get(`${COMPANY_SCOPE}/workflows/${WORKFLOW.id}`)
    .then(async (res) => (res.ok() ? ((await res.json()).version as string | null) : null))
    .catch(() => null);
  const query = version ? `?expectedVersion=${encodeURIComponent(version)}` : "";
  await request.delete(`${COMPANY_SCOPE}/workflows/${WORKFLOW.id}${query}`).catch(() => undefined);
});

/**
 * Answer the approvals feed. Matched by suffix rather than by full path, for
 * the reason the chat spec gives: the console addresses a *named* company while
 * the host also answers a single-company alias, and a pattern pinned to one of
 * them stops intercepting — silently — when the deployment shape changes.
 */
async function serveApprovals(page: Page, approvals: unknown[]) {
  await page.route("**/approvals", (route) =>
    route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify(approvals),
    }),
  );
}

/** Hand back a settled, blocked run instead of actually walking the graph. */
async function serveBlockedRun(page: Page) {
  await page.route("**/workflows/*/run", (route) =>
    route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify(BLOCKED_RUN),
    }),
  );
}

/** Capture every resolve the console sends, and answer each one. */
async function captureResolves(page: Page, bodies: unknown[]) {
  await page.route("**/approvals/*", (route) => {
    bodies.push({ url: route.request().url(), body: route.request().postDataJSON() });
    return route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ recorded: true, alreadyResolved: false }),
    });
  });
}

function picker(page: Page) {
  return page.getByRole("combobox").first();
}

/** Opens Workflows, selects the spec's graph, and runs it. */
async function runTheWorkflow(page: Page) {
  await page.goto("/#/workflows");
  await expect(picker(page)).toBeEnabled({ timeout: 30_000 });
  await picker(page).click();
  await page.getByRole("option", { name: WORKFLOW.name, exact: true }).click();
  await expect(picker(page)).toContainText(WORKFLOW.name);
  await page.getByRole("button", { name: "Run", exact: true }).click();
  await expect(page.getByTestId("workflow-run-result")).toBeVisible({ timeout: 60_000 });
}

/** The run-scoped approvals section inside the run drawer. */
function section(page: Page): Locator {
  return page.getByTestId("workflow-run-approvals");
}

/** The card for one approval, wherever it is rendered. */
function card(page: Page, approvalId: string): Locator {
  return page.locator(`[data-approval-id="${approvalId}"]`);
}

test("a run's parked card is decidable in the run window, and only that run's", async ({
  page,
}) => {
  await serveApprovals(page, [PARKED, OTHER_RUN, PAGE_ONLY]);
  await serveBlockedRun(page);

  await runTheWorkflow(page);

  // The card the run is held on, told in full — the same shared approval card
  // the Approvals page renders, so the payload being consented to is on screen
  // rather than a bare "approve?".
  await expect(section(page)).toBeVisible({ timeout: 30_000 });
  await expect(card(page, PARKED.id)).toBeVisible();
  await expect(card(page, PARKED.id)).toContainText("/notes/quarterly-update.md");
  // Named by the step that parked it, by its graph name.
  await expect(section(page)).toContainText("Draft the note");

  // The trap: the whole queue is handed to this view unfiltered, so the scoping
  // has to hold on the same render that can see all three. Another run's card
  // is not this run's business, and a card no run parked belongs to the page.
  await expect(card(page, OTHER_RUN.id)).toHaveCount(0);
  await expect(card(page, PAGE_ONLY.id)).toHaveCount(0);
});

test("the Approvals page still shows everything, including the run's card", async ({ page }) => {
  // Additive, not a replacement. The page stays the queue and the system of
  // record — an approval that reached no run window still has to be somewhere,
  // and the one that did must not have been moved out of the queue to get here.
  await serveApprovals(page, [PARKED, OTHER_RUN, PAGE_ONLY]);

  await page.goto("/#/approvals");
  await expect(page.getByText("3 things need your approval")).toBeVisible({ timeout: 30_000 });
  await expect(card(page, PARKED.id)).toBeVisible();
});

test("deciding in the run window sends one ordinary resolve, on that card's own id", async ({
  page,
}) => {
  await serveApprovals(page, [PARKED, OTHER_RUN, PAGE_ONLY]);
  await serveBlockedRun(page);
  const bodies: unknown[] = [];
  await captureResolves(page, bodies);

  await runTheWorkflow(page);
  await card(page, PARKED.id).getByRole("button", { name: "Approve" }).click();

  await expect.poll(() => bodies.length, { timeout: 30_000 }).toBe(1);
  // One per-id resolve, addressed to this approval — no batch route, no batch
  // body. Each park stays a separate single-use decision underneath, which is
  // what makes a racing operator on the Approvals page safe: they get a real
  // approval or a `NotParked` no-op receipt, never a double execution.
  expect(bodies[0]).toMatchObject({ body: { verdict: "approve" } });
  expect(String((bodies[0] as { url: string }).url)).toContain(`/approvals/${PARKED.id}`);

  // The card settles rather than vanishing, so the decision visibly lands.
  await expect(card(page, PARKED.id)).toContainText(/Approved/, { timeout: 30_000 });
  await expect(card(page, PARKED.id).getByRole("button", { name: "Approve" })).toHaveCount(0);
});

test("a decision made on the Approvals page settles the run window, with no reload", async ({
  page,
}) => {
  // The staleness guard, end to end. The run body's park receipt still names
  // this card — a receipt never flips — so a run window grounded in it would go
  // on offering two live buttons for a decision that is already made.
  //
  // The clearing is driven by what the HOST answers, flipped between polls,
  // rather than by a resolution frame raced against the drawer opening: this is
  // the state another operator's decision leaves behind, and the console has to
  // reach it on its own without a reload.
  let clearedElsewhere = false;
  await page.route("**/approvals", (route) =>
    route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify(clearedElsewhere ? [] : [PARKED]),
    }),
  );
  await serveBlockedRun(page);

  await runTheWorkflow(page);
  await expect(section(page)).toBeVisible({ timeout: 30_000 });
  await expect(card(page, PARKED.id).getByRole("button", { name: "Approve" })).toBeVisible();

  // Somebody clears it on the Approvals page, or in another console. The host
  // drops the row from the queue at once.
  clearedElsewhere = true;

  // No reload, no re-run: this window stops calling the step blocked. The run
  // body it was rendered from has not changed at all — its `approvals` receipt
  // and its `blockedNodes` still name this exact card.
  await expect(card(page, PARKED.id)).toHaveCount(0, { timeout: 30_000 });
  await expect(section(page)).toHaveCount(0);
  // …and the drawer is still the drawer: the receipt sentence is untouched,
  // because a receipt is a true statement about what the run opened.
  await expect(page.getByTestId("workflow-run-result")).toContainText("parked 1 approval");
});
