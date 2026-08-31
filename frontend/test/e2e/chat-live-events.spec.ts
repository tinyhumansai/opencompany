import { expect, test, type APIRequestContext, type Locator, type Page } from "@playwright/test";

import { LIVE_BRAIN } from "./capabilities";

import { bubbles, openChannel, reply, workingRow } from "./chat-helpers";

/**
 * End-to-end proof for issue #367 — the Chat tab receives what the company
 * says, not only what this browser tab typed.
 *
 * `#361` made Chat the nav-listed surface while every live writer stayed
 * pointed at the parked Conversation, so a channel showed the console's own
 * turns and nothing else: an inbound reply appeared only after a reload, a
 * running turn showed a generic dot instead of its tool rows, and the rail's
 * unread badges were fed a hard-coded empty map.
 *
 * Three of the four tests drive a **live host** (`companies/e2e_harness`) and a
 * real SSE stream, because the defect was in the addressing between a host
 * *thread* id and a console *channel* id — a mocked stream would have agreed
 * with whatever the console believed. The last one mocks `…/events` on purpose:
 * the offline echo brain calls no tools, so the only way to put real
 * `tool_call` / `tool_result` frames on the wire without a model is to write
 * them, and what is under test there is the rendering, not the plumbing.
 *
 * Like the rest of `test/e2e` this needs a running host and is not a CI gate —
 * the Playwright config declares no `webServer`.
 */

/**
 * The single-company alias the host answers on. Used only for the out-of-band
 * POSTs below — the console itself addresses the company by name
 * (`/api/v1/companies/<id>/…`), which is why the route patterns further down
 * match by suffix instead.
 */
const SCOPE = "/api/v1/company";

/** The harness manifest's two desks. Their ids are their channel ids. */
const ENGINEERING = { id: "engineering", channel: "engineering-desk" };
const CONTENT = { id: "content", channel: "content-desk" };

/**
 * The first-run product tour renders a modal over the console and swallows
 * every click beneath it. Answer "already skipped" for whatever company id the
 * host resolves to rather than hard-coding the harness's.
 */
test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    const real = Storage.prototype.getItem;
    Storage.prototype.getItem = function getItem(key: string) {
      return key.startsWith("oc-tour:") ? '{"skipped":true}' : real.call(this, key);
    };
  });
});

/**
 * A channel's row in the rail, located by the name the rail renders.
 *
 * Not an exact name match: an unread badge is inside the same button, so the
 * row's accessible name grows a count the moment the thing under test happens —
 * and an exact matcher would stop resolving exactly when it matters.
 */
function railRow(page: Page, channelName: string): Locator {
  return page.getByRole("complementary").first().getByRole("button", { name: channelName });
}

/**
 * The three tests below find the reply to their own turn by the offline echo
 * brain's `You said: <text>` — which is exactly the right way to prove an SSE
 * frame carried the answer to *this* message rather than some other one, and
 * exactly why they cannot run against a brain that answers differently.
 *
 * So they skip in the live-brain lane (#467) rather than being loosened into
 * counting bubbles, which would no longer distinguish the reply from anything
 * else that arrived. The default-feature lane runs them on every push and is a
 * required check, so nothing here goes uncovered.
 *
 * Making them brain-agnostic needs a reply the console can attribute to a turn
 * without reading its text — a message id on the bubble the send bracket
 * already knows. Worth doing; not this change.
 */
const ECHO_BRAIN_ONLY =
  "asserts the offline echo brain's `You said: <text>` reply, which a live " +
  "brain replaces. Covered by the default-feature `Console E2E` lane (#467).";

/**
 * The bubble count, once the channel's rehydration has stopped adding to it.
 *
 * A channel opens empty and fills from `chat/history` a moment later, so a
 * count taken on arrival is a count of nothing — and every "one more bubble
 * than before" assertion below would be measuring the hydration instead of the
 * reply. Waits for two equal readings rather than a fixed sleep.
 */
async function settledBubbleCount(page: Page): Promise<number> {
  let last = -1;
  await expect
    .poll(
      async () => {
        const current = await bubbles(page).count();
        const settled = current === last;
        last = current;
        return settled;
      },
      { intervals: [400, 400, 400, 400, 400, 400, 400, 400], timeout: 20_000 },
    )
    .toBe(true);
  return last;
}

/**
 * Says something to a desk from **outside the browser**, over the same session.
 *
 * This is the whole point of the first two tests: nothing the page did produced
 * this turn, so the only way its reply can reach the channel is the SSE stream.
 * It stands in for the inbound Telegram turn / background desk turn from the
 * issue, which cannot be triggered from a test.
 */
async function sayFromElsewhere(request: APIRequestContext, deskId: string, text: string) {
  const response = await request.post(`${SCOPE}/chat`, { data: { text, chat: deskId } });
  expect(
    response.ok(),
    `posting to ${deskId} failed: ${response.status()} ${await response.text()}`,
  ).toBeTruthy();
}

test("a reply the console never asked for lands in the open channel, with no reload", async ({
  page,
  request,
}) => {
  test.skip(LIVE_BRAIN, ECHO_BRAIN_ONLY);

  await openChannel(page, ENGINEERING.id);
  const before = await settledBubbleCount(page);

  const marker = `inbound-${Date.now()}`;
  await sayFromElsewhere(request, ENGINEERING.id, marker);

  // The offline brain answers "You said: <text>", so the marker rides back in
  // the reply. Only the company's half arrives here — the operator line of a
  // turn this console did not send is not ours to draw.
  await expect(reply(page, marker)).toBeVisible({ timeout: 60_000 });
  await expect(bubbles(page)).toHaveCount(before + 1);
});

test("a reply to a channel you are not on leaves an unread badge, and opening it clears the badge", async ({
  page,
  request,
}) => {
  test.skip(LIVE_BRAIN, ECHO_BRAIN_ONLY);

  // Sit on the Content desk; the reply below is addressed to Engineering.
  await openChannel(page, CONTENT.id);

  const marker = `unread-${Date.now()}`;
  await sayFromElsewhere(request, ENGINEERING.id, marker);

  const badge = railRow(page, ENGINEERING.channel).getByTestId("channel-unread");
  await expect(badge).toBeVisible({ timeout: 60_000 });

  // Opening the channel both shows the line and settles the badge — an unread
  // count that never clears is the same failure as one that never appears.
  await railRow(page, ENGINEERING.channel).click();
  await expect(reply(page, marker)).toBeVisible({ timeout: 30_000 });
  await expect(badge).toHaveCount(0);
});

test("a turn sent from the composer renders exactly one company bubble", async ({ page }) => {
  // The regression guard for the duplicate-bubble race, and the reason the
  // send bracket had to ship in the same commit as the SSE injection: the host
  // journals an `AgentReply` for the console's own turn too and pushes it over
  // SSE *while the POST is still in flight*. Without the bracket the injected
  // echo and the awaited reply both render and one turn answers twice.
  test.skip(LIVE_BRAIN, ECHO_BRAIN_ONLY);

  await openChannel(page, ENGINEERING.id);
  const before = await settledBubbleCount(page);

  const marker = `composer-${Date.now()}`;
  await page.getByPlaceholder(/^Message /).fill(marker);
  await page.keyboard.press("Enter");

  // Your line plus one reply — never three rows.
  await expect(bubbles(page)).toHaveCount(before + 2, { timeout: 60_000 });
  await expect(reply(page, marker)).toHaveCount(1);

  // A late echo would land after the POST resolved, so settle before believing
  // the count above.
  await page.waitForTimeout(3_000);
  await expect(bubbles(page)).toHaveCount(before + 2);
  await expect(reply(page, marker)).toHaveCount(1);
});

test("a running turn shows its tool rows in the channel", async ({ page }) => {
  // The one test that writes its own stream. The frames below are the exact
  // shape `src/turn_stream.rs` puts on the wire and `use-events.ts` types; the
  // offline brain this suite runs against calls no tools, so there is no live
  // turn to watch without inventing one. What is being proved is that a frame
  // carrying a desk's thread id reaches *that channel's* timeline — which is
  // what Chat never did.
  const frames = [
    { type: "tool_call", seq: 1, chatId: ENGINEERING.id, toolCallId: "t1", label: "workspace_list" },
    {
      type: "tool_result",
      seq: 2,
      chatId: ENGINEERING.id,
      toolCallId: "t1",
      label: "workspace_list",
      // What came back. Carried onto the row since ACP turns started
      // streaming: an ACP tool call has no arguments to derive a `detail`
      // from and reports only this, so a dropped `result` left its finished
      // rows saying nothing at all.
      result: "3 files",
      status: "ok",
      elapsedMs: 120,
    },
    { type: "tool_call", seq: 3, chatId: ENGINEERING.id, toolCallId: "t2", label: "workspace_read" },
  ];
  await page.route("**/events", (route) =>
    route.fulfill({
      status: 200,
      headers: { "content-type": "text/event-stream", "cache-control": "no-cache" },
      body: frames.map((f) => `data: ${JSON.stringify(f)}\n\n`).join(""),
    }),
  );

  await openChannel(page, ENGINEERING.id);

  // The rows themselves, not a typing dot — and the finished one keeps the
  // elapsed time the frame carried.
  await expect(page.getByText("workspace_list").first()).toBeVisible({ timeout: 30_000 });
  await expect(page.getByText("3 files").first()).toBeVisible();
  await expect(page.getByText("workspace_read").first()).toBeVisible();
  await expect(page.getByText("Replying…")).toHaveCount(0);

  // Addressed, not broadcast: the other desk's channel shows none of it.
  await openChannel(page, CONTENT.id);
  await expect(page.getByText("workspace_list")).toHaveCount(0);
});

/* -------------------------------------------------------------------------- *
 * Detached turns (issue #983)
 *
 * The console now asks every chat POST to detach, so the host answers `202`
 * with the turn's id instead of holding the request open for a turn whose
 * duration is unbounded — the shape that produced five 504s out of five real
 * persona tasks, with the work running on invisibly behind them.
 *
 * Two legs are worth proving here and nowhere else. The **live reply** leg
 * exercises the highest-risk line in the console change: the echo suppression
 * had to become conditional, and getting it wrong means the reply never appears
 * at all rather than appearing twice — a failure no type and no unit test of the
 * POST would catch, because the bubble is drawn by the *stream*. The **reload**
 * leg is the one that was impossible before the turn became durable: there was
 * nothing to ask about a turn in flight, so a console reloaded mid-turn showed a
 * settled-looking transcript with an answer still on its way.
 * -------------------------------------------------------------------------- */

test("a detached turn's reply arrives over the stream, not on the POST", async ({ page }) => {
  // The regression guard for the conditional suppression. Before #983 the
  // shell dropped every live `agent_reply` for a thread with a POST in flight,
  // because the awaited POST carried the authoritative copy. A detached POST
  // carries nothing — so if that suppression had stayed unconditional, this
  // reply would never be drawn and the operator would watch a spinner forever.
  //
  // Deliberately asserted with no reload and no mock: the bubble under test can
  // only have come from the SSE frame.
  test.skip(LIVE_BRAIN, ECHO_BRAIN_ONLY);

  await openChannel(page, ENGINEERING.id);
  const before = await settledBubbleCount(page);

  const marker = `detached-${Date.now()}`;
  await page.getByPlaceholder(/^Message /).fill(marker);
  await page.keyboard.press("Enter");

  await expect(reply(page, marker)).toBeVisible({ timeout: 60_000 });
  // Still exactly one company bubble: lifting the suppression must not
  // reintroduce the duplicate the bracket exists to prevent, since the durable
  // re-read on the turn's terminal transition folds by message id.
  await expect(reply(page, marker)).toHaveCount(1);
  await page.waitForTimeout(3_000);
  await expect(bubbles(page)).toHaveCount(before + 2);
  await expect(reply(page, marker)).toHaveCount(1);
});

test("a detached turn survives a reload and rebuilds from the durable record", async ({ page }) => {
  // The backstop, proved by throwing the live path away. Everything the stream
  // delivered dies with the page; what comes back has to come from the journal.
  // Before #983 the operator's own message was only appended *inside* the cycle
  // lock, so this reload could show neither the question nor the answer.
  test.skip(LIVE_BRAIN, ECHO_BRAIN_ONLY);

  await openChannel(page, ENGINEERING.id);

  const marker = `durable-${Date.now()}`;
  await page.getByPlaceholder(/^Message /).fill(marker);
  await page.keyboard.press("Enter");
  await expect(reply(page, marker)).toBeVisible({ timeout: 60_000 });

  await page.reload();
  await openChannel(page, ENGINEERING.id);

  // Both halves, rebuilt from `chat/history` alone.
  await expect(reply(page, marker)).toBeVisible({ timeout: 30_000 });
  await expect(bubbles(page).filter({ hasText: marker }).first()).toBeVisible();
});

test("a turn still open on reload re-arms the working row, and clears when it settles", async ({
  page,
}) => {
  // The reload leg, and the only test here that stubs the run reads.
  //
  // Why it must: the offline echo brain answers in milliseconds, so there is no
  // window to reload *into* — a real mid-turn reload against this host is a race
  // that would pass by luck. The rows below are the exact shape
  // `src/server/ops/runs.rs` serves, and what is under test is the console's
  // re-arm → poll → settle machinery, not whether the host writes the row (the
  // Rust suite pins that). Same reasoning the tool-rows test above states for
  // writing its own stream.
  let settled = false;
  const openRun = {
    id: "turn-e2e-1",
    chatId: ENGINEERING.id,
    agentId: "engineering",
    attempt: 1,
    status: "pending",
    phase: "active",
    createdAtMillis: Date.now(),
  };

  // The hydration read: which turns are open right now.
  await page.route("**/runs?*", (route) =>
    route.fulfill({
      status: 200,
      headers: { "content-type": "application/json" },
      body: JSON.stringify(settled ? [] : [openRun]),
    }),
  );
  // The per-turn poll, which is what carries the queued → running → settled
  // transitions. It keeps reporting the `pending` row until the test has
  // observed the queued state, so the `running` response can never land before
  // the assertion that pins the wording to "Queued…" has passed — otherwise the
  // first poll could flip the row to working mid-assertion and turn a
  // deterministic test into a race.
  let runningAllowed = false;
  await page.route("**/runs/turn-e2e-1", (route) =>
    route.fulfill({
      status: 200,
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        run: settled
          ? { ...openRun, status: "succeeded", phase: "terminal", finishedAtMillis: Date.now() }
          : runningAllowed
            ? { ...openRun, status: "running", phase: "active", startedAtMillis: Date.now() }
            : openRun,
        steps: [],
      }),
    }),
  );

  await openChannel(page, ENGINEERING.id);

  // Re-armed from the open-turn read, on a page that never POSTed anything —
  // which is precisely the mid-turn reload this design exists to make work.
  await expect(workingRow(page)).toBeVisible({ timeout: 30_000 });
  // And it says the truth: the row is `pending`, so the turn is queued behind
  // the per-company serial lock rather than working. A spinner implying
  // progress here would be the console inventing something.
  await expect(workingRow(page)).toHaveAttribute("data-queued", "true");

  // Let the poll move to `running`; the wording follows the row.
  runningAllowed = true;
  await expect(workingRow(page)).toHaveAttribute("data-queued", "false", { timeout: 30_000 });

  settled = true;

  // On the terminal transition the row comes down — a turn that has finished
  // must never leave a spinner behind, which is the failure mode the whole
  // durable-record design is meant to remove.
  await expect(workingRow(page)).toHaveCount(0, { timeout: 30_000 });
});

test("a failed turn leaves a durable line, not a spinner", async ({ page }) => {
  // The other half of "a lost response is not lost work". A turn killed with
  // the pod used to be permanent silence; since #983 it settles `Failed` and
  // writes a transcript line, and the console's job is to stop claiming the
  // turn is live and show what the journal says.
  //
  // Stubbed for the same reason as the test above — the echo brain cannot be
  // made to fail on demand — and the failure line itself comes from
  // `chat/history`, which is the point: the console renders the durable record
  // rather than special-casing a status.
  await page.route("**/runs?*", (route) =>
    route.fulfill({
      status: 200,
      headers: { "content-type": "application/json" },
      body: JSON.stringify([
        {
          id: "turn-e2e-2",
          chatId: ENGINEERING.id,
          agentId: "engineering",
          attempt: 1,
          status: "running",
          phase: "active",
          createdAtMillis: Date.now(),
        },
      ]),
    }),
  );

  // The harness host cannot actually fail a turn on demand, so the journal
  // read stands in for the `TurnFailed` line the runtime would otherwise have
  // written — in the exact shape `chat/history` serves. What is asserted below
  // is that the console renders that line, which is the durable-record
  // behaviour the title promises rather than a status it invented.
  //
  // The `?*` matters, not decoration: the host's path is `…/chat/history?desk=…`,
  // and a bare `**/chat/history` glob stops at the query string, so the mock
  // never fires and the journal read misses the line the test is proving.
  await page.route("**/chat/history?*", (route) =>
    route.fulfill({
      status: 200,
      headers: { "content-type": "application/json" },
      body: JSON.stringify([
        {
          id: "turn-e2e-2-failed",
          channel: ENGINEERING.id,
          author: "engineering",
          text: "the turn did not finish",
          atMillis: Date.now(),
          mine: false,
        },
      ]),
    }),
  );
  await page.route("**/runs/turn-e2e-2", (route) =>
    route.fulfill({
      status: 200,
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        run: {
          id: "turn-e2e-2",
          chatId: ENGINEERING.id,
          agentId: "engineering",
          attempt: 1,
          status: "failed",
          phase: "terminal",
          createdAtMillis: Date.now(),
          finishedAtMillis: Date.now(),
          failureReason: "the turn did not finish",
        },
        steps: [],
      }),
    }),
  );

  await openChannel(page, ENGINEERING.id);

  // It is allowed to show the row first — what it is not allowed to do is keep
  // showing it once the turn is known to be over.
  await expect(workingRow(page)).toHaveCount(0, { timeout: 30_000 });
  // …and the journaled failure line is what took its place: the console shows
  // what the durable record says instead of leaving a spinner behind.
  await expect(
    bubbles(page).filter({ hasText: "the turn did not finish" }),
  ).toBeVisible({ timeout: 30_000 });
});
