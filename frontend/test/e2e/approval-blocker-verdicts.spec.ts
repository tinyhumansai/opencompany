import { expect, test, type Page, type Route } from "@playwright/test";

/**
 * **Issue #2028 — reachability, which only a real click can prove.**
 *
 * The engine has taken four blocker verdicts for some time, and a green unit
 * suite said so while an operator on the Approvals page could still only send
 * two of them: approve became a retry, decline became a cancel, and skip and
 * amend had no control at all. That gap is invisible to any test that calls the
 * host directly — it exists between a pointer and a request body — so it is
 * asserted here, on the request the browser actually sends when the operator
 * clicks the thing they are looking at.
 *
 * ## What is faked, and what is not
 *
 * The approvals *list* is faked, for the reason `approval-timeout-honesty`
 * gives: parking a real blocker needs an agent turn that fails in a particular
 * way, which needs an inference credential and is not deterministic. The shape
 * served is the host's own `ApprovalSummary` with a `blocker.*` kind.
 *
 * The resolve is intercepted rather than fulfilled by the host, because what is
 * under test is the **body the console composes** — the host's own tests cover
 * what it does with that body, and pairing the two is what stops the halves
 * drifting on a token.
 *
 * Real: the host, the console bundle, the session, the routing, the polling
 * feed, and every DOM interaction.
 *
 * Runs in both Console E2E lanes. On fixed main b4cab3ea3, twenty serial
 * repetitions without retries failed 10/200 cases across two-step Skip,
 * single Skip, Cancel, and Retry, all at the company-read stub's response.json.
 * The verdict assertions finished while the post-resolve company refresh was
 * still in flight; context teardown disposed the response underneath it.
 *
 * Thirty isolated Retry runs with tracing passed: a trace showed the company
 * fetch finishing 1.6 ms AFTER After Hooks began; with tracing, the context
 * stayed alive another 65 ms. This is consistent with tracing masking the
 * teardown race. Drain this spec's routes before context teardown, without
 * ignoring callback errors or adding sleeps to the verdict assertions.
 */

const BLOCKER_ID = "e2e-2028-blocker";
const TASK_BLOCKER_ID = "e2e-2028-task-blocker";
const NODE_BLOCKER_ID = "e2e-2028-node-blocker";
const GATED_ID = "e2e-2028-gated";

const isApprovalList = (url: URL) => /\/approvals$/.test(url.pathname);
const isCompanyRead = (url: URL) =>
  /\/api\/v1\/(company|companies|companies\/[^/]+)$/.test(url.pathname);
const isApprovalResolve = (url: URL) => /\/approvals\/[^/]+$/.test(url.pathname);
const isSpec = (url: URL) => url.pathname === "/spec";

/**
 * A parked blocker with no step behind it — a bare agent question
 * (`escalate_to_human`), in the host's own `ApprovalSummary` shape. Carries no
 * `blocker_step_kind`, the same as a host that predates the field, so this
 * fixture also stands in for "unknown step" — see
 * `blockerStepKindShapesTheConsequence` below.
 */
function parkedBlocker() {
  return {
    id: BLOCKER_ID,
    kind: "blocker.infrastructure",
    amount_usd: null,
    at_millis: Date.now() - 30_000,
    task: { link: "unlinked" as const },
    agent: "writer",
    payload: {
      kind: "infrastructure",
      source: "provider",
      reason: "the model id `gpt-nope` was rejected",
      needed: "a model id this provider serves",
    },
  };
}

/** A parked blocker whose stopped step is a paused board card. */
function parkedTaskBlocker() {
  return {
    ...parkedBlocker(),
    id: TASK_BLOCKER_ID,
    blocker_step_kind: "task" as const,
  };
}

/** A parked blocker whose stopped step is a workflow-run node (#2028). */
function parkedNodeBlocker() {
  return {
    ...parkedBlocker(),
    id: NODE_BLOCKER_ID,
    blocker_step_kind: "node" as const,
  };
}

/** An ordinary gated tool call, for the control case. */
function parkedGatedCall() {
  return {
    id: GATED_ID,
    kind: "payment.send",
    amount_usd: 42.5,
    at_millis: Date.now() - 30_000,
    task: { link: "unlinked" as const },
    agent: "finance",
    payload: { to: "acme-supplies", memo: "invoice 8812" },
  };
}

test.beforeEach(async ({ page }) => {
  // The first-run tour would open a modal over every click below.
  await page.addInitScript(() => {
    const real = Storage.prototype.getItem;
    Storage.prototype.getItem = function getItem(key: string) {
      return key.startsWith("oc-tour:") ? '{"skipped":true}' : real.call(this, key);
    };
  });
});

const pendingRoutes = new WeakMap<Page, Set<Promise<void>>>();

async function stubRoute(
  page: Page,
  matches: (url: URL) => boolean,
  handle: (route: Route) => Promise<void>,
) {
  let pending = pendingRoutes.get(page);
  if (!pending) {
    pending = new Set();
    pendingRoutes.set(page, pending);
  }
  const calls = pending;
  await page.route(matches, async (route) => {
    const call = handle(route);
    calls.add(call);
    try {
      await call;
    } finally {
      calls.delete(call);
    }
  });
}

test.afterEach(async ({ page }) => {
  // Drain while handlers remain registered. unrouteAll(wait) alone failed
  // 5/200 cases: Playwright removed its handler list before waiting,
  // so one callback finishing could disable interception under another's
  // pending fulfill, which then failed with "Route is already handled!".
  const pending = pendingRoutes.get(page);
  while (pending?.size) await Promise.all(pending);
  await page.unrouteAll({ behavior: "wait" });
});

/**
 * Serve a fixed queue, with the company status stubbed in step with it — both
 * project the journal's parked set, and a fixture that fakes one and leaves the
 * other real is not a state the host can be in.
 */
async function stubQueue(page: Page, parked: unknown[]) {
  await advertiseFourWayBlockers(page);
  await stubRoute(page, isApprovalList, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify(parked),
    });
  });
  await stubRoute(page, isCompanyRead, async (route) => {
    const response = await route.fetch();
    if (!response.ok()) return route.fulfill({ response });
    const body = await response.json();
    await route.fulfill({
      status: response.status(),
      contentType: "application/json",
      body: JSON.stringify(
        Array.isArray(body)
          ? body.map((c) => ({ ...c, pending_approvals: parked.length }))
          : { ...body, pending_approvals: parked.length },
      ),
    });
  });
}

/**
 * Add the four-way blocker capability to the host's own `/spec` handshake.
 *
 * The console negotiates before it sends: `skip` and `amend` lower to an
 * `approve` that an unaware host would act on differently — it would re-run the
 * step rather than leave it out, and re-run it without the operator's words —
 * so the client refuses to send either unless the host advertises
 * `blocker-verdict` (`src/api/client.ts`).
 *
 * The binary this lane drives is a default-feature `cargo build --bin
 * opencompany`, and the capability is gated on `openhuman` because that is
 * where the blocker resume lives — a host without it refuses `blocker_verdict`
 * outright (`operator::blocker_verdict`). So the real handshake here says
 * "cannot", correctly, and the cases below are about the body a host that
 * *can* receives.
 *
 * This is the same class of fake as the queue itself, and stated in the same
 * terms: the response is the host's real one with one capability added, not a
 * fabricated handshake. `blockerVerdictRefusedByAnUnawareHost` covers the other
 * side by taking it back off.
 */
async function advertiseFourWayBlockers(page: Page) {
  await stubRoute(page, isSpec, async (route) => {
    const response = await route.fetch();
    if (!response.ok()) return route.fulfill({ response });
    const body = await response.json();
    const capabilities: string[] = Array.isArray(body.capabilities) ? body.capabilities : [];
    await route.fulfill({
      status: response.status(),
      contentType: "application/json",
      body: JSON.stringify({
        ...body,
        capabilities: capabilities.includes("blocker-verdict")
          ? capabilities
          : [...capabilities, "blocker-verdict"],
      }),
    });
  });
}

/** Capture the resolve body the console composes, and answer it plausibly. */
async function captureResolve(page: Page) {
  const bodies: Record<string, unknown>[] = [];
  await stubRoute(page, isApprovalResolve, async (route) => {
    const raw = route.request().postData();
    bodies.push(raw ? JSON.parse(raw) : {});
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ responses: [], stillAwaiting: 0, outcome: "settled" }),
    });
  });
  return bodies;
}

const decideFooter = (page: Page) => page.getByTestId("approval-decide");
const cardFooter = (page: Page, id: string) =>
  page.locator(`[data-approval-id="${id}"]`).getByTestId("approval-decide");

async function openApprovals(page: Page, parked: unknown[]) {
  await stubQueue(page, parked);
  await page.goto("/#/approvals");
  await expect(decideFooter(page).first()).toBeVisible({ timeout: 15_000 });
}

test("a blocker card offers four verdicts where an ordinary card offers two", async ({
  page,
}) => {
  await openApprovals(page, [parkedBlocker()]);
  const footer = decideFooter(page);

  // The whole issue in one assertion: the operator can see, and reach, all four.
  await expect(footer.getByRole("button", { name: /^Retry/ })).toBeVisible();
  await expect(footer.getByRole("button", { name: /^Answer this question/ })).toBeVisible();
  await expect(footer.getByRole("button", { name: /^Skip this step/ })).toBeVisible();
  await expect(footer.getByRole("button", { name: /^Cancel run/ })).toBeVisible();
  // …and the two that flattened them are gone.
  await expect(footer.getByRole("button", { name: /^Approve:/ })).toHaveCount(0);
  await expect(footer.getByRole("button", { name: /^Decline:/ })).toHaveCount(0);

  // The consequence of each is on the card, not hidden behind a hover — skip
  // and cancel are opposite outcomes one click apart. `parkedBlocker()` names
  // no `blocker_step_kind`, so this is the generic (step-unknown) wording —
  // see "worded by which step it stopped" below for the task/node pair.
  await expect(page.getByText("Moves on without doing it now.", { exact: false })).toBeVisible();
  await expect(page.getByText("Stops it here.", { exact: false })).toBeVisible();
});

test("a blocker's consequence text is worded by which step it stopped, not one shared line", async ({
  page,
}) => {
  await openApprovals(page, [parkedTaskBlocker(), parkedNodeBlocker(), parkedBlocker()]);

  const task = cardFooter(page, TASK_BLOCKER_ID);
  const node = cardFooter(page, NODE_BLOCKER_ID);
  const unknown = cardFooter(page, BLOCKER_ID);

  // The node card keeps the original, still-accurate wording.
  await expect(node.getByText("Moves past this step. It produces nothing", { exact: false })).toBeVisible();
  await expect(node.getByText("Stops the run here. Nothing after this step will run.", { exact: false })).toBeVisible();

  await expect(task.getByText("Moves past this step", { exact: false })).toHaveCount(0);
  await expect(task.getByText("Stops the run here.", { exact: false })).toHaveCount(0);
  await expect(task.getByText("Moves the card to In review without running it again", { exact: false })).toBeVisible();
  await expect(task.getByText("No output is produced", { exact: false })).toBeVisible();
  await expect(task.getByText("Moves the card back to To-do without running it.", { exact: false })).toBeVisible();

  // A blocker with no step behind it (or an old host) must not borrow either
  // path's specific claim — it gets the generic, always-true wording.
  await expect(unknown.getByText("Moves past this step. It produces nothing", { exact: false })).toHaveCount(0);
  await expect(unknown.getByText("Puts the card back in progress", { exact: false })).toHaveCount(0);
  await expect(unknown.getByText("Moves on without doing it now.", { exact: false })).toBeVisible();
  await expect(unknown.getByText("Stops it here.", { exact: false })).toBeVisible();

  // The three fixtures render three DIFFERENT skip sentences — the assertion
  // that would have failed against the one-shared-line bug.
  const skipTexts = new Set(
    await Promise.all(
      [task, node, unknown].map((footer) =>
        footer.locator("li", { hasText: "Skip" }).innerText(),
      ),
    ),
  );
  expect(skipTexts.size, `expected 3 distinct skip sentences, got ${[...skipTexts].join(" | ")}`).toBe(3);
});

/**
 * Whatever the card says will happen, it must still send the same wire
 * verdict — the copy differs by step kind, the request does not.
 */
test("Skip sends a skip on every step kind, however its consequence is worded", async ({
  page,
}) => {
  const bodies = await captureResolve(page);
  await openApprovals(page, [parkedTaskBlocker(), parkedNodeBlocker()]);

  await cardFooter(page, TASK_BLOCKER_ID).getByRole("button", { name: /^Skip this step/ }).click();
  await cardFooter(page, NODE_BLOCKER_ID).getByRole("button", { name: /^Skip this step/ }).click();
  await expect.poll(() => bodies.length).toBe(2);
  for (const body of bodies) {
    expect(body).toMatchObject({ verdict: "approve", blocker_verdict: "skip" });
  }
});

test("an ordinary approval still decides with Decline and Approve", async ({ page }) => {
  await openApprovals(page, [parkedGatedCall()]);
  const footer = decideFooter(page);
  await expect(footer.getByRole("button", { name: /^Approve:/ })).toBeVisible();
  await expect(footer.getByRole("button", { name: /^Decline:/ })).toBeVisible();
  await expect(footer.getByRole("button", { name: /^Skip this step/ })).toHaveCount(0);
});

test("Skip sends a skip, not the approve it rides on", async ({ page }) => {
  const bodies = await captureResolve(page);
  await openApprovals(page, [parkedBlocker()]);

  await decideFooter(page).getByRole("button", { name: /^Skip this step/ }).click();
  await expect.poll(() => bodies.length).toBe(1);
  expect(bodies[0]).toMatchObject({ verdict: "approve", blocker_verdict: "skip" });
  // Nothing rides along that the host would refuse.
  expect(bodies[0]).not.toHaveProperty("blocker_answer");
  expect(bodies[0]).not.toHaveProperty("scope");
});

/**
 * **The other side of the negotiation (P1 review finding on PR #2038).** Against
 * a host that does not advertise `blocker-verdict`, `skip` must not reach the
 * wire at all: it lowers to an `approve`, and an unaware host acts on that by
 * re-running the very step the operator asked it to leave out, while this
 * console reports a skip. `retry` lowers faithfully and still goes.
 *
 * The retry is the control, and it is what makes the absence provable: polling
 * for "no body" would pass before a slow request arrived, whereas a retry that
 * has landed means the click before it has had its chance and sent nothing.
 */
test("a skip is refused, not lowered, on a host that cannot perform it", async ({
  page,
}) => {
  const bodies = await captureResolve(page);
  await openApprovals(page, [parkedBlocker()]);
  // Registered after `stubQueue`'s, and Playwright prefers the newest match.
  await stubRoute(page, isSpec, async (route) => {
    const response = await route.fetch();
    const body = await response.json();
    await route.fulfill({
      status: response.status(),
      contentType: "application/json",
      body: JSON.stringify({
        ...body,
        capabilities: (Array.isArray(body.capabilities) ? body.capabilities : []).filter(
          (c: string) => c !== "blocker-verdict",
        ),
      }),
    });
  });

  const footer = decideFooter(page);
  await footer.getByRole("button", { name: /^Skip this step/ }).click();
  await footer.getByRole("button", { name: /^Retry/ }).click();

  await expect.poll(() => bodies.length).toBe(1);
  expect(
    bodies[0],
    "the retry must be the only thing that reached the host — a skip that got \
through would have been carried out as a re-run",
  ).toMatchObject({ verdict: "approve", blocker_verdict: "retry" });
});

test("Cancel run sends a cancel on the deny it rides on", async ({ page }) => {
  const bodies = await captureResolve(page);
  await openApprovals(page, [parkedBlocker()]);

  await decideFooter(page).getByRole("button", { name: /^Cancel run/ }).click();
  await expect.poll(() => bodies.length).toBe(1);
  expect(bodies[0]).toMatchObject({ verdict: "deny", blocker_verdict: "cancel" });
});

test("Retry sends a retry", async ({ page }) => {
  const bodies = await captureResolve(page);
  await openApprovals(page, [parkedBlocker()]);

  await decideFooter(page).getByRole("button", { name: /^Retry/ }).click();
  await expect.poll(() => bodies.length).toBe(1);
  expect(bodies[0]).toMatchObject({ verdict: "approve", blocker_verdict: "retry" });
});

test("an answer reaches the host verbatim, and a blank one cannot be sent", async ({
  page,
}) => {
  const bodies = await captureResolve(page);
  await openApprovals(page, [parkedBlocker()]);
  const footer = decideFooter(page);

  await footer.getByRole("button", { name: /^Answer this question/ }).click();
  const send = footer.getByRole("button", { name: /^Send this answer/ });
  // The host refuses a blank amend rather than downgrading it to a retry — a
  // retry would re-run the step that stopped for want of these words — so the
  // control must refuse it first.
  await expect(send).toBeDisabled();
  await page.getByRole("textbox", { name: /^Answer:/ }).fill("   ");
  await expect(send).toBeDisabled();

  await page.getByRole("textbox", { name: /^Answer:/ }).fill("use gpt-4o-mini instead");
  await expect(send).toBeEnabled();
  await send.click();
  await expect.poll(() => bodies.length).toBe(1);
  expect(bodies[0]).toMatchObject({
    verdict: "approve",
    blocker_verdict: "amend",
    blocker_answer: "use gpt-4o-mini instead",
  });
});

test("a bulk decision never sweeps a question in with the gated calls", async ({
  page,
}) => {
  const bodies = await captureResolve(page);
  await openApprovals(page, [parkedGatedCall(), parkedBlocker()]);

  // One gated call and one blocker: the bulk control speaks for the gated call
  // alone, so it is not offered over a queue of one.
  await expect(page.getByRole("button", { name: "Approve all" })).toHaveCount(0);
  expect(bodies).toHaveLength(0);
});
