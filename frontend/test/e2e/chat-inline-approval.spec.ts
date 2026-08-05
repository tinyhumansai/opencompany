import { expect, test, type Locator, type Page } from "@playwright/test";

/**
 * End-to-end proof for issue #379 — a request for sign-off is raised in the
 * conversation that produced it, decided there, and the work visibly resumes
 * there.
 *
 * Before this, an approval appeared only on a separate page, was decided in
 * isolation, and whatever happened next happened somewhere the operator was not
 * looking. Approving was an act of faith: a button greyed out, a toast
 * appeared, and nothing in the thread said the work had continued.
 *
 * The approvals feed and the event stream are **mocked**, and deliberately so.
 * Parking a real one needs a model that decides to make a policy-gated call,
 * and none of what is under test here is the parking — it is the *placement*
 * (which channel a card belongs to, and which it must not appear in), the
 * *verb* (an inline decision must detach, or the continuation is delivered
 * twice), and the *settling* (a decision made on the page must not leave a
 * stale card offering buttons in the channel).
 *
 * Like the rest of `test/e2e` this needs a running host and is not a CI gate —
 * the Playwright config declares no `webServer`.
 */

/** The harness manifest's two desks. A desk's id is both its channel id and its
 * host thread id, which is exactly the coincidence a DM does not share. */
const ENGINEERING = { id: "engineering", channel: "engineering-desk" };
const CONTENT = { id: "content", channel: "content-desk" };

/**
 * A request raised in the Engineering channel. `thread` is the host thread id;
 * everything else is the shape `GET …/approvals` answers with.
 */
const IN_ENGINEERING = {
  id: "appr-inline-1",
  kind: "payment.send",
  amount_usd: 42.5,
  at_millis: Date.now(),
  task: { link: "unlinked" },
  agent: "engineer",
  payload: { to: "vendor@example.test", amount_usd: 42.5 },
  thread: ENGINEERING.id,
};

/**
 * A request with no conversation behind it — a workflow delivery or a scheduler
 * tick. It carries no `thread`, so it must match no channel and live on the
 * Approvals page alone. This is the additive half of the contract.
 */
const PAGE_ONLY = {
  id: "appr-page-only",
  kind: "email.send",
  amount_usd: null,
  at_millis: Date.now(),
  task: { link: "unlinked" },
};

test.beforeEach(async ({ page }) => {
  // Skip the first-run tour, whose modal would swallow the clicks below.
  await page.addInitScript(() => {
    const real = Storage.prototype.getItem;
    Storage.prototype.getItem = function getItem(key: string) {
      return key.startsWith("oc-tour:") ? '{"skipped":true}' : real.call(this, key);
    };
  });
});

/**
 * Answer the approvals feed. Matched by suffix rather than by full path: the
 * console addresses a *named* company while the host also answers a
 * single-company alias, and a pattern pinned to one of them stops intercepting —
 * silently — the moment the deployment shape changes.
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

async function openChannel(page: Page, channelId: string) {
  await page.goto(`/#/chat/${channelId}`);
  await expect(page.getByPlaceholder(/^Message /)).toBeVisible({ timeout: 30_000 });
}

/** The inline card for one approval, wherever it is rendered. */
function card(page: Page, approvalId: string): Locator {
  return page.locator(`[data-approval-id="${approvalId}"]`);
}

test("a request raised in a channel appears in that channel, and only there", async ({ page }) => {
  await serveApprovals(page, [IN_ENGINEERING, PAGE_ONLY]);

  await openChannel(page, ENGINEERING.id);
  await expect(card(page, IN_ENGINEERING.id)).toBeVisible({ timeout: 30_000 });
  // Told in full, the same as on the page — the payload is the thing being
  // consented to, so a card without it asks for a blind signature.
  await expect(card(page, IN_ENGINEERING.id)).toContainText("vendor@example.test");

  // The trap, in both directions. A card belongs to the conversation that
  // raised it; a desk channel and a DM to its lead resolve to the same agent,
  // so a console placing cards by asker would leak one into the other.
  await openChannel(page, CONTENT.id);
  await expect(card(page, IN_ENGINEERING.id)).toHaveCount(0);

  // And the approval no conversation produced is in neither channel.
  await expect(card(page, PAGE_ONLY.id)).toHaveCount(0);
  await openChannel(page, ENGINEERING.id);
  await expect(card(page, PAGE_ONLY.id)).toHaveCount(0);
});

test("the Approvals page still shows everything, including the inline one", async ({ page }) => {
  // Inline is an addition, not a replacement. The page is the one surface that
  // must never filter — an approval that reached no channel has to be somewhere.
  await serveApprovals(page, [IN_ENGINEERING, PAGE_ONLY]);

  await page.goto("/#/approvals");
  await expect(page.getByText("2 things need your approval")).toBeVisible({ timeout: 30_000 });
});

test("deciding inline detaches, and the continuation lands in the same channel", async ({
  page,
}) => {
  await serveApprovals(page, [IN_ENGINEERING]);

  // Capture the resolve body: an inline decision MUST send `detach: true`.
  // Without it the host answers with the follow-up turn's replies *and* pushes
  // the same reply over SSE, so the continuation reaches the channel twice.
  const bodies: unknown[] = [];
  await page.route("**/approvals/*", (route) => {
    bodies.push(route.request().postDataJSON());
    return route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ recorded: true, alreadyResolved: false }),
    });
  });

  // The continuation, as the host would push it: an `agent_reply` on the desk
  // thread the approval was raised in. This is the assertion the whole issue
  // turns on — approving visibly causes the next thing to happen, in the place
  // the operator was already reading.
  const CONTINUATION = "Paid the vendor invoice.";
  await page.route("**/events", (route) =>
    route.fulfill({
      status: 200,
      headers: { "content-type": "text/event-stream", "cache-control": "no-cache" },
      body:
        `data: ${JSON.stringify({
          type: "agent_reply",
          seq: 1,
          atMillis: Date.now(),
          chatId: ENGINEERING.id,
          agentId: "engineer",
          text: CONTINUATION,
        })}\n\n`,
    }),
  );

  await openChannel(page, ENGINEERING.id);
  await card(page, IN_ENGINEERING.id).getByRole("button", { name: "Approve" }).click();

  await expect
    .poll(() => bodies.length, { timeout: 30_000 })
    .toBeGreaterThan(0);
  expect(bodies[0]).toMatchObject({ verdict: "approve", detach: true });

  // The card settles rather than vanishing, so the decision visibly lands.
  await expect(card(page, IN_ENGINEERING.id)).toContainText(
    /Approved — the agent is completing the action/,
    { timeout: 30_000 },
  );
  // And the work resumes here, exactly once.
  await expect(page.getByText(CONTINUATION)).toHaveCount(1, { timeout: 30_000 });
});

test("declining inline says so in the thread rather than leaving it stalled", async ({ page }) => {
  await serveApprovals(page, [IN_ENGINEERING]);
  await page.route("**/approvals/*", (route) =>
    route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ recorded: true, alreadyResolved: false }),
    }),
  );

  await openChannel(page, ENGINEERING.id);
  await card(page, IN_ENGINEERING.id).getByRole("button", { name: "Decline" }).click();

  // A decline is terminal and produces no continuation, so silence would read
  // as a stall. The line is addressed to this channel, not to "wherever the
  // operator last looked".
  await expect(
    page.getByText(/Declined — the agent will not take that action/),
  ).toBeVisible({ timeout: 30_000 });
});

test("a decision made on the Approvals page settles the inline card, with no reload", async ({
  page,
}) => {
  await serveApprovals(page, [IN_ENGINEERING]);

  // The host's resolution frame, as another surface (the page, another tab)
  // would produce it. The inline card has to stop offering buttons for a
  // decision that is already made.
  await page.route("**/events", (route) =>
    route.fulfill({
      status: 200,
      headers: { "content-type": "text/event-stream", "cache-control": "no-cache" },
      body:
        `data: ${JSON.stringify({
          type: "approval_resolved",
          seq: 1,
          atMillis: Date.now(),
          approvalId: IN_ENGINEERING.id,
          verdict: "approve",
        })}\n\n`,
    }),
  );

  await openChannel(page, ENGINEERING.id);

  await expect(card(page, IN_ENGINEERING.id)).toContainText(
    /Approved — the agent is completing the action/,
    { timeout: 30_000 },
  );
  await expect(card(page, IN_ENGINEERING.id).getByRole("button", { name: "Approve" })).toHaveCount(
    0,
  );
});

test("a parked approval raises its card live, without a reload", async ({ page }) => {
  // The feed is empty when the channel opens; the park frame is what makes the
  // console re-read it. That round trip is the design: the frame is thin on
  // purpose (no payload, no asker), so the card's content can only come from
  // the one place the host redacts.
  let served = 0;
  await page.route("**/approvals", (route) => {
    served += 1;
    return route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify(served <= 1 ? [] : [IN_ENGINEERING]),
    });
  });
  await page.route("**/events", (route) =>
    route.fulfill({
      status: 200,
      headers: { "content-type": "text/event-stream", "cache-control": "no-cache" },
      body:
        `data: ${JSON.stringify({
          type: "approval_parked",
          seq: 1,
          atMillis: Date.now(),
          approvalId: IN_ENGINEERING.id,
          kind: IN_ENGINEERING.kind,
          chatId: ENGINEERING.id,
        })}\n\n`,
    }),
  );

  await openChannel(page, ENGINEERING.id);
  await expect(card(page, IN_ENGINEERING.id)).toBeVisible({ timeout: 30_000 });
});
