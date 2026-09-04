import { expect, test, type Page } from "@playwright/test";

/**
 * Regression proof for #380 — a slow approval that dies at the proxy.
 *
 * Reported from a hosted tenant: an operator approved something, the request
 * sat open for a full agent turn, nginx gave up at its default read timeout and
 * answered its own `504` page. The console then rendered
 *
 *     Couldn't record your decision — <html><head><title>504 Gateway Time-out…
 *
 * which is two bugs wearing one message.
 *
 * **The copy was false.** `CompanyRuntime::resolve_approval` does four things:
 * drops the approval from the parked queue, journals the verdict durably, mints
 * the single-use grant, then runs a whole follow-up agent turn. Only the last is
 * slow, so a proxy that gives up has given up *after* the verdict is durable.
 * "Couldn't record your decision" invited the one response that cannot help —
 * approving again.
 *
 * **The body was rendered verbatim.** `safeJson` failed open, turning any
 * unparseable body into `{error: text, code: "unparseable"}`, which then read as
 * the host's own error envelope. Not approvals-specific: it applied to every
 * route on both readers.
 *
 * ## What is faked here, and what is not
 *
 * The proxy is faked. These tests fulfil the resolve request from Playwright
 * with the byte-for-byte body nginx emits, rather than standing a real reverse
 * proxy in front of the host and waiting out a real timeout — the console's
 * behaviour is a pure function of the response it gets, and a fabricated one is
 * both faster and deterministic. What is real: the host, the console bundle, the
 * session, and the polling feed the queue assertion depends on.
 *
 * The approvals *list* is faked too, because parking a real approval needs an
 * agent turn that decides to call a gated tool — which needs an inference
 * credential and is not deterministic. The shape served here is the host's own
 * `ApprovalSummary`.
 *
 * Like the rest of `test/e2e`, this drives a running host and is not wired into
 * CI (the Playwright config declares no `webServer`); `npm run typecheck:e2e`
 * compiles it, nothing runs it automatically. It is a reproduction and a
 * written-down contract, not a merge gate.
 */

/** The page nginx serves on a read timeout, padding comments and all. */
const NGINX_504 = [
  "<html>",
  "<head><title>504 Gateway Time-out</title></head>",
  "<body>",
  "<center><h1>504 Gateway Time-out</h1></center>",
  "<hr><center>nginx/1.24.0</center>",
  "</body>",
  "</html>",
  // nginx pads short error pages so IE and Chrome show the real one rather than
  // their own. This is the part that made the old console message enormous.
  ...Array.from(
    { length: 6 },
    () => "<!-- a padding to disable MSIE and Chrome friendly error page -->",
  ),
].join("\n");

/** An upstream that closed the connection, as the proxy reports it. */
const NGINX_502 = [
  "<html>",
  "<head><title>502 Bad Gateway</title></head>",
  "<body>",
  "<center><h1>502 Bad Gateway</h1></center>",
  "<hr><center>nginx/1.24.0</center>",
  "</body>",
  "</html>",
].join("\n");

const APPROVAL_ID = "e2e-380-approval";

/**
 * Route matchers by pathname rather than by glob.
 *
 * The client serves two deployment shapes from one code path — single-company
 * (`/api/v1/company/…`) and platform (`/api/v1/companies/{id}/…`) — and which
 * one the console picks depends on the host it is pointed at. Matching on the
 * tail of the pathname keeps this spec working against either, which a literal
 * prefix glob silently does not: it matches nothing and the test then fails on
 * an empty queue rather than on the behaviour under test.
 */
const isApprovalList = (url: URL) => /\/approvals$/.test(url.pathname);
/**
 * Every read that carries `pending_approvals`: the company list the console
 * boots from, and the single-company status it then polls, in both addressing
 * shapes. All three have to move together — the list is what seeds
 * `initialStatus`, so patching only the poll leaves the console's *first* count
 * at zero and the first poll then reads as a rise.
 */
const isCompanyRead = (url: URL) =>
  /\/api\/v1\/(company|companies|companies\/[^/]+)$/.test(url.pathname);
const isApprovalResolve = (url: URL) => /\/approvals\/[^/]+$/.test(url.pathname);
const isTaskExport = (url: URL) => /\/tasks\/[^/]+\/export$/.test(url.pathname);
const isTaskDetail = (url: URL) => /\/tasks\/[^/]+$/.test(url.pathname);
const isTaskList = (url: URL) => /\/tasks$/.test(url.pathname);

/** One parked approval, in the host's own `ApprovalSummary` shape. */
function parkedApproval() {
  return {
    id: APPROVAL_ID,
    kind: "payment.send",
    amount_usd: 42.5,
    at_millis: Date.now() - 30_000,
    task: { link: "unlinked" as const },
    agent: "finance",
    payload: { to: "acme-supplies", memo: "invoice 8812" },
  };
}

/**
 * The first-run product tour opens a modal over a fresh console and would
 * intercept every click below. Answer "already skipped" for whatever company id
 * the host resolves to, rather than hard-coding the harness's.
 */
test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    const real = Storage.prototype.getItem;
    Storage.prototype.getItem = function getItem(key: string) {
      return key.startsWith("oc-tour:") ? '{"skipped":true}' : real.call(this, key);
    };
  });
});

const toasts = (page: Page) => page.locator("[data-sonner-toast]");
/**
 * The queue card for this approval, by id rather than by its "acme-supplies"
 * text.
 *
 * Defect B-068 (`4c293f0f2`) put the request's counterparty in the card
 * headline (`approvalHeadline`, via `ApprovalHeadline`) *and* it was already
 * printed verbatim in the raw payload block below (`ApprovalPayload`,
 * `frontend/src/components/approval-card.tsx`) — both real, both intentional,
 * so `getByText("acme-supplies")` now matches two elements on one card and
 * trips Playwright's strict-mode locator check. Every assertion here only
 * ever cares whether *the card* is present or gone, never which of its two
 * lines carries the name, so scoping to `ApprovalCard`'s own
 * `data-approval-id` (`frontend/src/views/ApprovalsView.tsx`) is the correct
 * fix rather than picking one of the two text matches with `.first()`.
 */
const approvalCard = (page: Page) =>
  page.locator(`[data-approval-id="${APPROVAL_ID}"]`);

/**
 * Serve one parked approval until `resolved` flips, then serve an empty queue —
 * which is what the real host does, since `resolve_outcome` removes the
 * approval from the parked map in step one, before anything slow happens.
 *
 * The company-status read is stubbed **in step with it** (issue #932). Both
 * reads project one thing — the journal's parked set — so a fixture that fakes
 * the queue and leaves the count real is not a state the host can be in: it
 * says "nothing is pending" and "here is a pending approval" in the same
 * breath. The console now resolves that contradiction in favour of the queue,
 * which made this fixture read as a park landing on the first poll and raised
 * the "needs a sign-off" push over the decision toast these tests assert on.
 *
 * The real response is fetched and one field overwritten, rather than
 * fabricated: everything else on it — the company's name, lifecycle, whether
 * the kill switch is engaged — is the host's to answer, and a hand-written
 * status is a second place for those to drift.
 */
async function stubQueue(page: Page): Promise<{ resolve: () => void }> {
  let resolved = false;
  await page.route(isApprovalList, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify(resolved ? [] : [parkedApproval()]),
    });
  });
  await page.route(isCompanyRead, async (route) => {
    const response = await route.fetch();
    // Anything but a 200 goes through untouched. Parsing it as the status
    // envelope would throw inside the route handler, which surfaces as a dead
    // page rather than as the sign-in or lookup failure it actually is.
    if (!response.ok()) return route.fulfill({ response });
    const body = await response.json();
    const count = () => (resolved ? 0 : 1);
    await route.fulfill({
      status: response.status(),
      contentType: "application/json",
      // One route serves both shapes: the list is an array of statuses, the
      // single read is one status.
      body: JSON.stringify(
        Array.isArray(body)
          ? body.map((company) => ({ ...company, pending_approvals: count() }))
          : { ...body, pending_approvals: count() },
      ),
    });
  });
  return { resolve: () => (resolved = true) };
}

/** Land on the approvals queue with the stubbed card visible. */
async function openApprovals(page: Page) {
  await page.goto("/#/approvals");
  await expect(approvalCard(page)).toBeVisible({ timeout: 15_000 });
}

/**
 * Move the operator's attention off the queue.
 *
 * `useStableList` (#1414, `frontend/src/hooks/use-stable-list.ts`) freezes the
 * rendered order — and holds removals, deliberately, including the row the
 * operator just decided — for as long as the pointer is over the queue OR
 * keyboard focus is inside it, thawing only once BOTH clear. Clicking a decide
 * button leaves both true: the click puts the pointer over the container, and
 * Chromium focuses a `<button>` on click, so focus is inside it too. Neither
 * this test nor a real operator has to do anything special to *cause* the
 * freeze — the click already did. This mirrors what "moving away" means to the
 * hook: the mouse leaves the container, and focus is dropped rather than
 * merely moved to a still-inside sibling.
 */
async function leaveQueue(page: Page) {
  await page.mouse.move(0, 0);
  await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
}

test("a proxy timeout does not claim the decision failed, and clears the card", async ({
  page,
}) => {
  const queue = await stubQueue(page);
  await page.route(isApprovalResolve, async (route) => {
    // The verdict is already durable host-side by the time the proxy gives up,
    // so the queue drops it even though this request "fails".
    queue.resolve();
    await route.fulfill({
      status: 504,
      contentType: "text/html",
      body: NGINX_504,
    });
  });

  await openApprovals(page);
  await page.getByRole("button", { name: "Approve" }).click();

  const message = toasts(page).first();
  await expect(message).toBeVisible({ timeout: 10_000 });
  const text = (await message.innerText()).trim();

  // The headline assertion: the old copy is gone.
  expect(text).not.toContain("Couldn't record your decision");
  // ...and it is replaced by something that says the verdict landed and the
  // continuation did not, rather than asserting plain success.
  expect(text).toContain("your decision was recorded");
  expect(text).toContain("may still be working");
  // Defect 2, on the exact body that produced the field report.
  expect(text).not.toContain("<");

  // The queue reconciles once the operator is no longer interacting with it
  // (#1414 — see `leaveQueue`). The bound is deliberately under the feed's 5s
  // poll (`POLL_MS` in `use-company.ts`) so that passing means the fix's own
  // `feed.refresh()` cleared the card, not the next scheduled tick.
  await leaveQueue(page);
  await expect(approvalCard(page)).toHaveCount(0, { timeout: 2_500 });
});

test("a declined approval that times out reads as recorded, not failed", async ({
  page,
}) => {
  const queue = await stubQueue(page);
  await page.route(isApprovalResolve, async (route) => {
    queue.resolve();
    await route.fulfill({
      status: 502,
      contentType: "text/html",
      body: NGINX_502,
    });
  });

  await openApprovals(page);
  await page.getByRole("button", { name: "Decline" }).click();

  const message = toasts(page).first();
  await expect(message).toBeVisible({ timeout: 10_000 });
  const text = (await message.innerText()).trim();

  expect(text).not.toContain("Couldn't record your decision");
  expect(text).toContain("your decision was recorded");
  // A decline is terminal — there is no agent left working on it, and the copy
  // must not imply otherwise.
  expect(text).not.toContain("may still be working");
  expect(text).not.toContain("<");
  // Same reconciliation gate as the timeout test above — see `leaveQueue`.
  await leaveQueue(page);
  await expect(approvalCard(page)).toHaveCount(0, { timeout: 2_500 });
});

test("a host refusal still reports honestly, with no markup in the message", async ({
  page,
}) => {
  // 500 is below the gateway band and carries no host envelope, so this takes
  // the "the host refused" arm — the one that still renders `ApiError.message`
  // as prose. That makes it the isolated acceptance test for defect 2: only the
  // fail-closed parse can keep this HTML out of the rendered string.
  const queue = await stubQueue(page);
  await page.route(isApprovalResolve, async (route) => {
    await route.fulfill({
      status: 500,
      contentType: "text/html",
      body: NGINX_504.replace(/504 Gateway Time-out/g, "500 Internal Server Error"),
    });
  });
  void queue;

  await openApprovals(page);
  await page.getByRole("button", { name: "Approve" }).click();

  const message = toasts(page).first();
  await expect(message).toBeVisible({ timeout: 10_000 });
  const text = (await message.innerText()).trim();

  // The failure is still reported as a failure — this arm is correct copy.
  expect(text).toContain("Couldn't record your decision");
  // But the reason is the status line, never the body.
  expect(text).not.toContain("<");
  expect(text).not.toContain("nginx");
  expect(text).not.toContain("padding");

  // The approval was genuinely not resolved, so it stays on the queue.
  await expect(approvalCard(page)).toBeVisible();
});

test("a connection that fails instantly is still reported as a failure", async ({
  page,
}) => {
  // The other side of the honesty coin. An offline browser or a refused
  // connection rejects immediately, which means the request never reached the
  // host and the verdict cannot have been recorded — so the timeout copy would
  // be a fresh lie in place of the one #380 removed. Only a *slow* failure
  // supports the "recorded before the delay" inference.
  const queue = await stubQueue(page);
  await page.route(isApprovalResolve, (route) => route.abort("failed"));
  void queue;

  await openApprovals(page);
  await page.getByRole("button", { name: "Approve" }).click();

  const message = toasts(page).first();
  await expect(message).toBeVisible({ timeout: 10_000 });
  const text = (await message.innerText()).trim();

  expect(text).toContain("Couldn't record your decision");
  expect(text).not.toContain("your decision was recorded");
  expect(text).not.toContain("<");

  // Nothing was decided, so nothing leaves the queue.
  await expect(approvalCard(page)).toBeVisible();
});

test("an unparseable body on the document reader falls back to the status line", async ({
  page,
}) => {
  // The second throw site. `getDocument` is the reader for host-rendered
  // documents (#352) and had its own byte-identical copy of the error path, so
  // #380 existed twice; both now share `httpError`. Driving it through the task
  // record export is the only way to reach that copy from the UI.
  const task = {
    id: "e2e-380-task",
    title: "Reconcile the August ledger",
    column: "pending",
    priority: "medium",
    assignee: "finance",
    updatedAt: Date.now() - 60_000,
  };
  const detail = {
    task,
    timeline: [],
    durations: {
      workedMillis: 0,
      workedLive: false,
      waitingMillis: 0,
      waitingLive: false,
      asOfMillis: Date.now(),
    },
    approvals: [],
    irreversibleEffects: [],
    historyIncomplete: false,
    discussion: [],
    discussionHasMore: false,
    lineage: { children: [] },
    runs: [],
  };
  await page.route(isTaskList, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify([task]),
    });
  });
  await page.route(isTaskDetail, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify(detail),
    });
  });
  await page.route(isTaskExport, async (route) => {
    // A plain-text upstream failure: not JSON, not the host's envelope, and the
    // exact shape the old `safeJson` would have promoted to `error`.
    await route.fulfill({
      status: 502,
      contentType: "text/plain",
      body: "upstream connect error or disconnect/reset before headers",
    });
  });

  await page.goto(`/#/tasks/${task.id}`);
  await page.getByRole("button", { name: "Export" }).click({ timeout: 15_000 });

  const message = toasts(page).first();
  await expect(message).toBeVisible({ timeout: 10_000 });
  const text = (await message.innerText()).trim();

  // The upstream's prose is not the console's prose.
  expect(text).not.toContain("upstream connect error");
  expect(text).not.toContain("<");
});
