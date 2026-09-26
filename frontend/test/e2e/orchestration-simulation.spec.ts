import { expect, test, type APIRequestContext } from "@playwright/test";

import { LIVE_BRAIN, LIVE_BRAIN_REASON } from "./capabilities";
import {
  DONE,
  PENDING,
  SCOPE,
  WORKING,
  column,
  dispatch,
  markers,
  openBoard,
  openCard,
  openOrchestratorDm,
  say,
  settledMarkerCount,
  silenceTour,
  waitForTurn,
} from "./orchestration";

/**
 * **The whole orchestration loop, driven from the console** — a goal stated in
 * chat, broken up by the orchestrator, worked by two teammates, and closed out
 * on the board.
 *
 * Every other spec in this suite owns one link of that chain: `chat-to-card`
 * proves a message can become a card, `chat-dispatch-marker` proves a settled
 * card marks the channel it came from, `board-drag` proves the gesture that
 * dispatches one. None of them runs the loop, and the loop is the product —
 * a company that opens cards and never closes them is not doing the job the
 * chat promises, and nothing here would have said so.
 *
 * So this is one long test rather than several short ones. That is deliberate:
 * the claims are ordered in time and each is only meaningful after the one
 * before it landed. Splitting them would mean re-establishing the state at the
 * top of each, and the re-establishment (writing a card straight into
 * `in_review`, say) is precisely the part the loop is supposed to have done.
 *
 * # What is real and what is scripted
 *
 * Everything except the cognition. The console, the HTTP surface, the harness,
 * the toolbelt, the delegation queue and its drain, the board's write
 * boundary, the journal and the SSE stream are all the shipped ones. What is
 * scripted is which tools the orchestrator decides to call, through
 * `mock-brain.mjs`'s `__MOCK_PLAN__` directive — because a decision is the one
 * thing a test cannot assert on and a model is the one thing a required check
 * must not depend on.
 *
 * `orchestration-live.spec.ts` is the other half of this: the same loop with a
 * real model making those choices, run by a person rather than by CI.
 *
 * # The spend gate is the operator's, and this spec keeps it that way
 *
 * `spawn_task` opens a card in **Pending**, not Working (issue #1512). Nothing
 * an agent says starts paid work; a person drags the card into Working. So the
 * dispatch below is a real drag on the real board, and the assertion before it
 * — that nothing has run yet — is as load-bearing as the ones after it.
 */

/** One call in a `__MOCK_PLAN__` step. */
function call(name: string, args: Record<string, unknown>) {
  return { name, arguments: args };
}

/**
 * The plan directive for `steps`, with this run's marker after the payload.
 *
 * After, because that is where `mock-brain.mjs` keys its cursor off: the
 * directive plus the rest of its line. A marker in front of it would leave two
 * runs of this spec sharing one cursor, and the second would start half way
 * through the first one's plan.
 */
function plan(steps: unknown[][], marker: string): string {
  return `__MOCK_PLAN__ ${JSON.stringify(steps)} ${marker}`;
}

/** The host's own record for one card: the truth a rendered column reports on. */
async function record(
  request: APIRequestContext,
  id: string,
): Promise<{ column: string; stage?: string; assignee?: string }> {
  const response = await request.get(`${SCOPE}/tasks/${id}`);
  expect(response.ok(), `reading card ${id} failed: ${response.status()}`).toBeTruthy();
  return (await response.json()).task;
}

/** The stage word, which is finer than the phase: `in_review` inside Working. */
async function stageOf(request: APIRequestContext, id: string): Promise<string> {
  const task = await record(request, id);
  return task.stage ?? task.column;
}

test.beforeEach(async ({ page }) => {
  await silenceTour(page);
});

test("a goal becomes delegated cards, the team works them, and review closes them", async ({
  page,
  request,
}) => {
  // Without the harness compiled in there is no orchestrator, no delegation
  // drain and no teammate turn — the message would be answered by nothing and
  // every claim below would be about a card that was never opened.
  test.skip(!LIVE_BRAIN, LIVE_BRAIN_REASON);
  // Two agent turns, two dispatched runs and two settles, each a real round
  // trip through the harness. The suite's 60s default expires inside the first
  // wait and reports a *timeout*, which is the least useful failure this spec
  // could produce.
  test.setTimeout(420_000);

  const run = Date.now();
  const gather = `sim gather the sources ${run}`;
  const write = `sim write the digest ${run}`;

  // ── 1. The goal ─────────────────────────────────────────────────────────
  // One sentence, in the one place an operator says anything. The plan riding
  // on it is the orchestrator's decision, scripted: two cards, one per
  // teammate, each with the brief it should carry.
  await openOrchestratorDm(page);
  const goal =
    `Ship a short market digest this week: find what is being said and write it up. ` +
    plan(
      [
        [
          call("spawn_task", {
            title: gather,
            note: "Search and collect what is current, newest first.",
            assignee: "engineer",
          }),
          call("spawn_task", {
            title: write,
            note: "Turn the collected sources into a short digest with links.",
            assignee: "writer",
          }),
        ],
        // An empty second step: the turn answers in prose and ends, rather than
        // looping until the harness caps it.
        [],
      ],
      `goal-${run}`,
    );
  await say(page, goal);

  // The conversation says a card was opened, and links to it. This is the chat
  // half of the chain — the card knows which thread raised it, which is what
  // later lets its completion answer back here (issue #151 §3.2).
  await expect(page.getByRole("link", { name: /Card opened/ }).last()).toBeVisible({
    timeout: 180_000,
  });

  // ── 2. The board holds the work, unstarted ──────────────────────────────
  // After the turn has *finished*, not merely after its first card appeared:
  // delegations are drained at the end of a turn, so a board read taken mid-turn
  // sees whichever cards exist at that instant. It costs nothing here — the chip
  // above is already proof the drain ran — and it is what keeps this spec from
  // teaching the pattern that made `orchestration-live.spec.ts` report on half
  // a goal.
  await waitForTurn(page);

  // The markers this thread already holds, counted **here** — on the transcript
  // this test has been watching all along, rather than after a fresh navigation
  // that would have to be waited on again. The suite shares one data root, so a
  // marker an earlier test left in the orchestrator DM is legitimately in it, and a
  // baseline taken before a hydration has landed reads zero for a thread that
  // holds three.
  const markersBefore = await settledMarkerCount(page);

  await openBoard(page);
  await expect(column(page, PENDING)).toContainText(gather, { timeout: 60_000 });
  await expect(column(page, PENDING)).toContainText(write);

  const gatherId = await openCard(page, gather);
  // The brief travelled with the card, not just the title: a teammate reading
  // "gather the sources" and one reading the note are doing different work.
  await expect(page.getByText("Search and collect what is current")).toBeVisible();
  // …and it is owned by the teammate the orchestrator named, which is what
  // decides whose turn the dispatch below runs.
  await expect(page.getByText("engineer").first()).toBeVisible();
  const writeId = await openCard(page, write);
  await expect(page.getByText("writer").first()).toBeVisible();

  // The spend gate, before anything is spent. An orchestrator cannot start paid
  // work by asking for it (issue #1512) — these cards are open and idle, and a
  // build that dispatched them here would pass every assertion after this one
  // while having taken the operator's decision away.
  expect(await stageOf(request, gatherId), "spawn_task must not dispatch").toBe("pending");
  expect(await stageOf(request, writeId), "spawn_task must not dispatch").toBe("pending");

  // ── 3. The operator starts the work ─────────────────────────────────────
  // The real gesture on the real board: entering Working is dispatch.
  await dispatch(page, gather);
  await dispatch(page, write);

  // ── 4. The team works them, and stops for a decision ────────────────────
  // A finished run lands in `in_review`, never `done` — accepting work is the
  // operator's call and a run does not make it for them.
  for (const [id, title] of [
    [gatherId, gather],
    [writeId, write],
  ] as const) {
    await expect
      .poll(() => stageOf(request, id), {
        message: `card "${title}" never settled`,
        timeout: 180_000,
        intervals: [2_000],
      })
      .toBe("in_review");
  }

  // ── 5. …and the conversation the goal was stated in is told ─────────────
  // The structural line a reader needs and the relay prose cannot give them:
  // the run *stopped*, and here is where it landed (issue #377). Counted rather
  // than addressed by card, because this surface renders a marker as a plain
  // system pill — see `markers` in `./orchestration`.
  await openOrchestratorDm(page);
  await expect
    .poll(() => markers(page).count(), {
      message: "the two settled cards did not mark the thread they were raised in",
      timeout: 120_000,
      intervals: [1_000],
    })
    .toBeGreaterThanOrEqual(markersBefore + 2);
  // `at least`, not `exactly`. Both halves of that are deliberate. Two is the
  // floor because two cards settled and each must say so. It is not a ceiling
  // because this surface renders a marker as a plain system pill with no card
  // id on it (see `markers` in `./orchestration`), so a third marker cannot be
  // told apart from ours — and in a full-suite run there is one: a card an
  // earlier spec raised in this same thread, settling on its own schedule while
  // this test runs. An exact count made this spec pass alone and fail in the
  // suite, which is the worst of both.
  // …and they say where the work landed, which is the half the relay prose
  // cannot carry: "finished" alone would leave a reader with the same wrong
  // impression issue #377 is about. Counted rather than read off the last pill,
  // because a straggler from another spec can arrive after ours.
  await expect
    .poll(
      () => page.getByRole("main").getByText("finished → In review", { exact: true }).count(),
      { message: "neither settled card said where it landed", timeout: 30_000 },
    )
    .toBeGreaterThanOrEqual(2);

  // ── 6. The orchestrator closes the goal out ─────────────────────────────
  // The second half of the loop, and the half nothing else covers: work that
  // was delegated coming back and being *accepted*. `review_task` with
  // `approve` is the `in_review → done` transition (issue #171, PR #179).
  await say(
    page,
    `Both pieces are back — take a look and close them out. ` +
      plan(
        [
          [
            call("review_task", {
              task_id: gatherId,
              decision: "approve",
              note: "Sources look current.",
            }),
            call("review_task", {
              task_id: writeId,
              decision: "approve",
              note: "Reads well; shipping it.",
            }),
          ],
          [],
        ],
        `close-${run}`,
      ),
  );

  for (const [id, title] of [
    [gatherId, gather],
    [writeId, write],
  ] as const) {
    await expect
      .poll(() => record(request, id).then((task) => task.column), {
        message: `card "${title}" was never accepted`,
        timeout: 180_000,
        intervals: [2_000],
      })
      .toBe("done");
  }

  // ── 7. The board an operator would come back to ─────────────────────────
  // Read from a reload rather than from the session that made the changes: the
  // columns above were written through the API, and a console that only moved
  // them in its own React state would still show them here.
  await page.reload();
  await openBoard(page);
  await expect(column(page, DONE)).toContainText(gather, { timeout: 60_000 });
  await expect(column(page, DONE)).toContainText(write);
  await expect(column(page, PENDING)).not.toContainText(gather);
  await expect(column(page, WORKING)).not.toContainText(gather);
});
