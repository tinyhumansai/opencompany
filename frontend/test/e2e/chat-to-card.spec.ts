import { expect, test, type APIRequestContext, type Page } from "@playwright/test";

import { LIVE_BRAIN, LIVE_BRAIN_REASON } from "./capabilities";

/**
 * End-to-end proof for the chat↔card edge (issue #246).
 *
 * Runs against a live host the harness brings up separately, with an inference
 * backend whose *choices* are scripted: a prompt carrying `SPAWNONE` makes the
 * orchestrator call `spawn_task` once. Everything else — the harness, the tool
 * plumbing, the cycle, the journal, the HTTP surface — is real.
 *
 * Three things are asserted that a curl cannot reach:
 *
 * 1. the chip a spawned card produces survives a **reload**, not merely the
 *    live POST response;
 * 2. a dismissed card's chip does not come back on one;
 * 3. the card links back to the conversation it came from.
 *
 * Cards reach chat one way: a turn raises one and the host journals its id onto
 * the reply (`chat_history.rs`, `task_id`). There is no operator-initiated
 * per-message action, so every chip here is one the company opened.
 */

/**
 * A console opened against a fresh company home shows the welcome tour, which
 * renders as a modal over the whole console and swallows the first click.
 * Dismiss it when it is up. Whether it appears depends on console-local state,
 * so its absence is not a failure.
 */
async function dismissWelcome(page: Page) {
  const skip = page.getByRole("button", { name: /Skip for now/ });
  // Up to twice: a console opened against a genuinely fresh home stacks two of
  // these — the first-run gate, then the tour behind it — and both carry this
  // label. They arrive in sequence, so the second is not in the DOM to be
  // counted when the first is clicked; the only way to see it is to look again.
  // A host that shows one leaves the second wait to time out and returns, which
  // is what makes this cost nothing where only the tour appears.
  for (let attempt = 0; attempt < 2; attempt += 1) {
    try {
      await skip.first().waitFor({ state: "visible", timeout: attempt === 0 ? 5_000 : 2_000 });
    } catch {
      return;
    }
    const openDialogs = await skip.count();
    await skip.first().click({ timeout: 10_000 });
    // A dismissed dialog animates out, so it stays visible for a beat after
    // its click. Waiting for one to actually go is what leaves the next pass
    // looking at a genuinely second dialog rather than this one on its way out.
    await expect(skip).toHaveCount(openDialogs - 1, { timeout: 10_000 });
  }
  await expect(skip).toHaveCount(0, { timeout: 10_000 });
}

/**
 * Opens one Room channel by id.
 *
 * The channel comes from the address rather than a rail click, so the spec
 * cannot proceed against an unselected transcript that still accepts a `fill`.
 */
async function openThread(page: Page, channelId: string) {
  await page.goto(`/#/chat/${channelId}`);
  await dismissWelcome(page);
  await expect(page.getByPlaceholder(/^Message /)).toBeVisible({ timeout: 30_000 });
}

/** The orchestrator's roster id, read off the host's own roster rule. */
async function orchestratorId(request: APIRequestContext): Promise<string> {
  const response = await request.get("/api/v1/company/team");
  expect(response.ok(), await response.text()).toBeTruthy();
  const body = await response.json();
  const members = (body.members ?? body.team ?? body) as { id: string; isOrchestrator?: boolean }[];
  const orchestrator = members.find((member) => member.isOrchestrator);
  expect(orchestrator, "the company has no orchestrator").toBeTruthy();
  return orchestrator!.id;
}

type Task = {
  id: string;
  title: string;
  note?: string;
  originChatId?: string;
  originParent?: number;
};

/** Wait for the asynchronous orchestrator to persist the card it opened. */
async function taskMatching(
  request: APIRequestContext,
  matches: (task: Task) => boolean,
): Promise<Task> {
  let found: Task | undefined;
  await expect
    .poll(
      async () => {
        const response = await request.get("/api/v1/company/tasks");
        expect(response.ok()).toBeTruthy();
        found = ((await response.json()) as Task[]).find(matches);
        return Boolean(found);
      },
      { timeout: 15_000 },
    )
    .toBe(true);
  if (!found) throw new Error("Timed out waiting for the orchestrator to persist its card");
  return found!;
}

test("a card raised from a channel line links back to the channel", async ({
  page,
  request,
}) => {
  test.skip(!LIVE_BRAIN, LIVE_BRAIN_REASON);
  // `SPAWNONE` has the live fixture open exactly one card for this request.
  const API = "/api/v1/company";
  const marker = Date.now();
  const prompt = `build the launch checklist SPAWNONE ${marker}`;
  const beforePost = await request.get(`${API}/tasks`);
  expect(beforePost.ok(), await beforePost.text()).toBeTruthy();
  const taskIdsBeforePost = new Set((await beforePost.json() as Task[]).map((task) => task.id));
  const posted = await request.post(`${API}/chat`, {
    data: { text: prompt, chat: "engineering" },
  });
  expect(posted.ok(), await posted.text()).toBeTruthy();

  const card = await taskMatching(
    request,
    (task) =>
      task.originChatId === "engineering" &&
      !taskIdsBeforePost.has(task.id),
  );

  // The card is real and titled from the message. Its *stage* is deliberately
  // not asserted: a triage-raised card is one the company decided is work, and
  // buying it a planning pass is the mechanism doing its job. The spend gate
  // this file used to carry belonged to the operator-pressed create, which no
  // longer exists — what a card costs is pinned in `company/runtime.rs`
  // (`a_prompt_box_card_buys_exactly_one_planning_pass`), a level at which
  // "exactly one pass, never two" is decidable. It was never decidable here:
  // `planning`, `in_progress`, `paused` and `in_review` all render the one
  // word "Working" (`board-columns.ts`), so a page-wide match on it could not
  // tell a planned card from a dispatched one.
  await page.goto(`/#/company/tasks/${card!.id}`);
  await dismissWelcome(page);
  // On the request text, which the card keeps in its **note**, not on the
  // heading. Since #2055 a titling pass names the card, so the heading is a
  // model's words — against the fixture that is the same fixed string for
  // every card, which would make a heading match prove only that some detail
  // page rendered. The note is where the operator's own words are kept, so it
  // is what says *this* is the card that message opened.
  await expect(page.getByText(card.title, { exact: true })).toBeVisible({ timeout: 15_000 });
  await expect(page.getByRole("heading", { level: 1 })).toBeVisible();

  // …and it knows which conversation opened it.
  const origin = page.getByRole("button", { name: /Opened from chat/ });
  await expect(origin).toBeVisible({ timeout: 15_000 });

  // The other half of the round trip: the jump lands in Room, on the channel
  // carrying that conversation.
  await origin.click();
  // The Engineering desk's own channel, not merely some channel: a regression
  // that landed the jump on the wrong one would still match a bare `/.+/`.
  await expect(page).toHaveURL(/#\/chat\/engineering(?:[/?]|$)/);
  // `data-active` is a boolean attribute the sidebar row renders empty when
  // set and omits when not, so the assertion is on its presence.
  await expect(
    page
      .locator("[data-slot=sidebar-content]")
      .getByRole("button", { name: "Room", exact: true }),
  ).toHaveAttribute("data-active", "");

  // And Back returns to the card, because the jump went through the address
  // rather than through shell state the history knows nothing about.
  await page.goBack();
  await expect(page).toHaveURL(/#\/company\/tasks\/.+/);
  await expect(origin).toBeVisible();
});

/**
 * Issue #2020: a card raised from **inside a thread** opens that thread on the
 * jump back, not merely the channel it lives in.
 *
 * The test above proves the channel-level half; it cannot prove this half,
 * because a first message in an empty conversation and a reply nested under it
 * both resolve to the same channel — the console would land in the right place
 * either way even with the thread root dropped on the floor. So this seeds a
 * root message and a threaded reply to it directly (the same REST surface
 * `sayFromElsewhere`-style specs already use for setup elsewhere in this
 * suite), asks the live fixture to open a card from the reply,
 * and asserts the jump renders the thread panel holding the *root's* text —
 * not the reply's, and not merely the channel.
 */
test("a card raised inside a thread opens that thread on the jump back, not just the channel", async ({
  page,
  request,
}) => {
  test.skip(!LIVE_BRAIN, LIVE_BRAIN_REASON);
  const marker = Date.now();
  const channel = "content";
  const rootText = `quick sync on Q3 priorities ${marker}`;
  const API = "/api/v1/company";
  const rootResponse = await request.post(`${API}/chat`, {
    data: { text: rootText, chat: channel },
  });
  expect(rootResponse.ok(), await rootResponse.text()).toBeTruthy();
  const rootId = (await rootResponse.json()).messageId as string;
  expect(rootId).toBeTruthy();
  const beforeReply = await request.get(`${API}/tasks`);
  expect(beforeReply.ok(), await beforeReply.text()).toBeTruthy();
  const taskIdsBeforeReply = new Set((await beforeReply.json() as Task[]).map((task) => task.id));

  // `SPAWNONE` asks the live fixture to call `spawn_task`, and `parent` makes
  // this a threaded reply rather than a second channel-level line.
  const replyText = `build the onboarding checklist SPAWNONE ${marker}`;
  const replyResponse = await request.post(`${API}/chat`, {
    data: { text: replyText, chat: channel, parent: rootId },
  });
  expect(replyResponse.ok(), await replyResponse.text()).toBeTruthy();

  const card = await taskMatching(
    request,
    (task) =>
      task.originChatId === channel &&
      !taskIdsBeforeReply.has(task.id),
  );

  await page.goto(`/#/company/tasks/${card!.id}`);
  await dismissWelcome(page);
  const origin = page.getByRole("button", { name: /Opened from chat/ });
  await expect(origin).toBeVisible({ timeout: 15_000 });
  await origin.click();

  await expect(page).toHaveURL(new RegExp(`#\\/chat\\/${channel}(?:[/?]|$)`));

  const thread = page
    .locator("aside")
    .filter({ has: page.getByRole("heading", { name: "Thread" }) });
  await expect(thread).toBeVisible({ timeout: 15_000 });
  // The root's own text is in the panel — proof the jump opened *that*
  // thread, since a channel-level landing (or the reply's own self-thread)
  // would show none of this or the wrong message.
  await expect(thread.getByText(rootText, { exact: true })).toBeVisible({ timeout: 15_000 });
});

test("a card the orchestrator opens is chipped in chat, and survives a reload", async ({
  page,
  request,
}) => {
  // Only THIS test needs the scripted backend — the one above it drives the
  // console's own "Add to board" action and passes against a default host, so
  // the skip is per-test rather than per-file.
  test.skip(!LIVE_BRAIN, LIVE_BRAIN_REASON);

  const orchestrator = await orchestratorId(request);
  await openThread(page, `dm:${orchestrator}`);

  // `SPAWNONE` is the scripted backend's cue to call `spawn_task` once.
  const before = await request.get("/api/v1/company/tasks");
  expect(before.ok(), await before.text()).toBeTruthy();
  const previousIds = new Set(((await before.json()) as Task[]).map((task) => task.id));
  const marker = `${Date.now()}`;
  const prompt = `please track this SPAWNONE ${marker}`;
  await page.getByPlaceholder(/^Message /).fill(prompt);
  await page.getByRole("button", { name: "Send", exact: true }).click();

  const card = await taskMatching(
    request,
    (task) =>
      task.originChatId === orchestrator &&
      task.title.includes(marker) &&
      !previousIds.has(task.id),
  );
  const href = `#/company/tasks/${card.id}`;

  // Live: the reply bubble says a card was opened.
  const chip = page.locator(`a[href="${href}"]`, { hasText: "Card opened" });
  await expect(chip).toBeVisible({ timeout: 60_000 });
  await expect(chip).toHaveAttribute("href", href);

  // After a reload the transcript is rehydrated from `chat/history`, so a chip
  // that only existed on the live POST response would vanish here.
  await page.reload();
  await openThread(page, `dm:${orchestrator}`);
  const rehydrated = page.locator(`a[href="${href}"]`, { hasText: "Card opened" });
  await expect(rehydrated).toBeVisible({ timeout: 30_000 });
  await expect(rehydrated).toHaveAttribute("href", href);
});

test("a persisted chat card is rendered and rehydrated on the default lane", async ({
  page,
  request,
}) => {
  test.skip(LIVE_BRAIN, "the live-brain lane covers the real spawn_task flow above");

  const taskId = `default-lane-card-${Date.now()}`;
  const href = `#/company/tasks/${taskId}`;
  const orchestrator = await orchestratorId(request);
  await page.route("**/chat/history?*", async (route) => {
    const desk = new URL(route.request().url()).searchParams.get("desk");
    if (desk !== orchestrator) return route.continue();
    return route.fulfill({
      status: 200,
      headers: { "content-type": "application/json" },
      body: JSON.stringify([
        {
          id: "default-lane-card-message",
          channel: orchestrator,
          author: "orchestrator",
          text: "I opened a card for this request.",
          atMillis: Date.now(),
          mine: false,
          taskId,
        },
      ]),
    });
  });

  await openThread(page, `dm:${orchestrator}`);
  const chip = page.locator(`a[href="${href}"]`, { hasText: "Card opened" });
  await expect(chip).toBeVisible({ timeout: 30_000 });
  await expect(chip).toHaveAttribute("href", href);

  await page.reload();
  await expect(chip).toBeVisible({ timeout: 30_000 });
  await expect(chip).toHaveAttribute("href", href);
});

/**
 * **The dismissal, end to end, including the reload (issue #984).**
 *
 * The affordance had no coverage at all, and the half that had none was the
 * half that was broken: clearing the chip in React state alone meant a
 * dismissal that looked right in the session came back on the next reload —
 * the console rehydrates from `chat/history`, and the host still had `task_id`
 * on the journaled row. The chip returned pointing at a card that no longer
 * existed, which reads as the delete having failed. `drop_dead_cards` blanking
 * that field is the half only a reload can see.
 *
 * So the reload is the assertion that matters here, and it is deliberately the
 * mirror image of the reload assertion in the test above: that one proves a
 * live card's chip *survives*, this one proves a dismissed card's chip *does
 * not come back*. Neither is safe without the other — a host that dropped every
 * `task_id` would pass this and fail that.
 *
 * `LIVE_BRAIN`, like its mirror above, because a chip is something the company
 * puts there: `task_id` is journaled onto the *reply* a turn writes
 * (`chat_history.rs`), never onto the operator's own line, so a card the
 * transcript can draw a chip for needs a turn that actually ran.
 */
test("a dismissed card's chip goes away and does not come back on reload", async ({
  page,
  request,
}) => {
  test.skip(!LIVE_BRAIN, LIVE_BRAIN_REASON);

  await openThread(page, "");

  const prompt = `dismiss this one SPAWNONE ${Date.now()}`;
  await page.getByPlaceholder(/^Message /).fill(prompt);
  await page.getByRole("button", { name: "Send", exact: true }).click();

  const chip = page.getByRole("link", { name: /Card opened/ }).last();
  await expect(chip).toBeVisible({ timeout: 60_000 });
  const href = await chip.getAttribute("href");
  expect(href).toMatch(/^#\/company\/tasks\/.+/);
  const taskId = decodeURIComponent(href!.replace("#/company/tasks/", ""));

  // The turn that opened this card also dispatched it, and the host refuses to
  // delete a card with a run registered against it — `tasks.rs` answers 409
  // with "cancel it first", because a delete would not stick: the turn writes
  // the card back when it settles. So cancel, exactly as that message says to.
  // Tolerated rather than asserted: the run may already have settled, and a
  // card at rest is the state this test wants either way.
  await request
    .post(`/api/v1/company/tasks/${encodeURIComponent(taskId)}/steer`, {
      data: { action: "cancel", confirm: true },
    })
    .catch(() => undefined);

  // …and *wait for it to land*. A cancel is a request, not an event: the run
  // leaves the in-flight list when the turn notices, which is the same check
  // the delete route makes under its write lock. Dismissing before then races
  // that 409 back, and the chip stays up for a reason the assertion below
  // cannot name.
  await expect
    .poll(
      async () => {
        const live = await request.get(`/api/v1/company/tasks/inflight`);
        if (!live.ok()) return true;
        const runs = (await live.json()) as Array<{ taskId: string | null }>;
        return runs.some((run) => run.taskId === taskId);
      },
      {
        timeout: 30_000,
        message: "the cancelled run must leave the in-flight list before the card can be deleted",
      },
    )
    .toBe(false);

  // The control is a confirm, not a bare delete — a card is not something to
  // lose to a stray click. Scoped to the row the chip sits on, so the dialog
  // opened is that card's.
  const row = page
    .locator("article[data-message-id]")
    .filter({ has: page.locator(`a[href="${href}"]`) });
  await row.getByRole("button", { name: "Dismiss this card" }).click();
  await expect(page.getByText("Dismiss this card?")).toBeVisible();
  await page.getByRole("button", { name: "Dismiss card", exact: true }).click();

  // Gone from the transcript in-session… asserted on *this card's* chip, by
  // href. `chip` is a `.last()` over the channel, and the sibling spec above
  // leaves its own card's chip in this same conversation — so a bare re-read
  // can settle on a chip that was never dismissed and is correctly still there.
  const thisChip = page.locator(`a[href="${href}"]`);
  await expect(thisChip).toHaveCount(0, { timeout: 30_000 });

  // …and gone from the board, which is what makes it a dismissal rather than a
  // hidden chip over a card that is still filling the board.
  await page.goto(href!);
  await dismissWelcome(page);
  await expect(page.getByText(prompt).first()).toHaveCount(0, { timeout: 30_000 });

  // …and still gone after a reload. This is the regression: the transcript is
  // rehydrated from the host here, not from the React state the click cleared.
  await openThread(page, "");
  await page.reload();
  await openThread(page, "");
  await expect(page.getByText(prompt, { exact: true }).first()).toBeVisible({ timeout: 30_000 });
  await expect(page.locator(`a[href="${href}"]`)).toHaveCount(0);
});
