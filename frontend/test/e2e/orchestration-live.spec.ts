import { expect, test, type APIRequestContext } from "@playwright/test";

import { LIVE_LLM, LIVE_LLM_REASON } from "./capabilities";
import {
  DONE,
  SCOPE,
  column,
  dispatch,
  openBoard,
  openOrchestratorDm,
  say,
  silenceTour,
  waitForTurn,
} from "./orchestration";

/**
 * **The same loop as `orchestration-simulation.spec.ts`, with a real model
 * making the decisions.**
 *
 * The scripted lane proves the machinery runs: a goal reaches the orchestrator,
 * `spawn_task` opens cards, a dispatch runs a teammate's turn, `review_task`
 * closes them. What it cannot prove is that any of that is *reachable* — that a
 * model, handed this company's roster and the real descriptions of these tools,
 * decides to break a goal up, give the pieces to the right people, and accept
 * the results afterwards. A prompt that stopped describing the board, a tool
 * description that stopped saying what it is for, a roster the orchestrator can
 * no longer see: every one of those leaves the scripted lane green, because the
 * scripted lane never reads them.
 *
 * This lane reads them. Nothing here scripts a choice — `live-brain-proxy.mjs`
 * forwards the turn to a real router and returns whatever comes back.
 *
 * # Why it is not in CI, and what that costs
 *
 * It spends tokens and its verdict is a model's judgement, which is the one
 * thing a required check must not depend on: a model having a bad day would
 * turn every unrelated pull request red. So it is run by a person — before
 * changing an orchestrator prompt, a delegation tool's description, or the
 * drain — with `npm run e2e:live-llm`.
 *
 * The cost is that it can rot silently, which is why it is written to fail
 * *legibly*: every wait names what it was waiting for, and the two failures
 * this lane actually has — "the model was never asked" and "the model was asked
 * and chose nothing" — are separated by the first assertion below.
 *
 * # What is asserted, given that the answers are not ours
 *
 * Outcomes, never wording, and never *which* tool was used. An orchestrator
 * that opens cards with `spawn_task` and one that hands work over with
 * `delegate_to_teammate` (which opens its own card) are both doing the job; a
 * spec that demanded one would be asserting a preference. So what is checked is
 * the shape of the board afterwards: work appeared, somebody on the roster owns
 * it, dispatching it runs a turn, and asking for it to be closed out closes it.
 *
 * Cards are identified by **what was not there before**, not by title: the
 * model writes the titles, and a spec that grepped for its own words would be
 * testing the model's obedience rather than the company's behaviour.
 */

/** The board's own record for every card the company holds. */
async function allCards(
  request: APIRequestContext,
): Promise<{ id: string; title: string; column: string; stage?: string; assignee?: string }[]> {
  const response = await request.get(`${SCOPE}/tasks`);
  expect(response.ok(), `listing cards failed: ${response.status()}`).toBeTruthy();
  const body = await response.json();
  return (body.tasks ?? body) as never[];
}

/** The ids on the board right now — the baseline a goal is measured against. */
async function cardIds(request: APIRequestContext): Promise<Set<string>> {
  return new Set((await allCards(request)).map((task) => task.id));
}

/** The cards that appeared since `before`. */
async function newCards(request: APIRequestContext, before: Set<string>) {
  return (await allCards(request)).filter((task) => !before.has(task.id));
}

/**
 * Where a card is, in the one vocabulary that covers every case: the stage when
 * it has one, the phase otherwise (`stage` is omitted for a pending or done
 * card, because there is only one way to be either — issue #1512).
 */
function landing(card: { column: string; stage?: string }): string {
  return card.stage ?? card.column;
}

/**
 * The landings a run has actually stopped in.
 *
 * Written as an allow-list of *stopped* states rather than as "anything but
 * `in_progress`", which is the same shape `CLAUDE.md` warns about one level up:
 * a card the orchestrator has not dispatched yet reads `pending`, which is not
 * `in_progress`, so the loose form let a card that had not started count as one
 * that had finished. The first run of this spec passed that way — one card
 * accepted, the other still working, and a green tick over it.
 */
const SETTLED = ["in_review", "paused", "done"];

/** The roster ids an orchestrator may hand work to, read from the host. */
async function roster(request: APIRequestContext): Promise<string[]> {
  const response = await request.get(`${SCOPE}/team`);
  expect(response.ok(), `reading the team failed: ${response.status()}`).toBeTruthy();
  const body = await response.json();
  const members = (body.members ?? body.team ?? body) as { id: string }[];
  return members.map((member) => member.id);
}

test.beforeEach(async ({ page }) => {
  await silenceTour(page);
});

test("a real model takes a goal, gives it to its team, and closes it out", async ({
  page,
  request,
}) => {
  test.skip(!LIVE_LLM, LIVE_LLM_REASON);
  // Every turn here is a real model round trip, and there are at least five of
  // them: the goal, two dispatched teammate turns, and the close-out. Timed for
  // a slow rung rather than a fast one — a lane that fails on latency teaches
  // nobody anything.
  test.setTimeout(900_000);

  const before = await cardIds(request);
  const team = await roster(request);
  expect(team.length, "a company with no roster has nobody to delegate to").toBeGreaterThan(1);

  // ── 1. The goal, in the operator's own words ────────────────────────────
  // Specific about the *outcome* (cards on the board, owned by teammates) and
  // silent about the mechanism, because which tool to reach for is exactly the
  // decision under test.
  await openOrchestratorDm(page);
  await say(
    page,
    "Our workspace has a Standards note. I want a short \"How we write\" one-pager " +
      "for new agents based on it, saved into the workspace, and then checked by " +
      "somebody else for clarity. Break that into two pieces of work, give each one " +
      "to whichever agent should own it, and do not do the work yourself in this " +
      "message.",
  );

  // The first failure this lane can have: the model was never reached, or was
  // reached and chose nothing. A board that grew is the proof it decided.
  await expect
    .poll(() => newCards(request, before).then((cards) => cards.length), {
      message:
        "the orchestrator opened no cards for the goal — either the model was not " +
        "reached (check the proxy's stderr for a non-200) or it answered in prose " +
        "without calling a delegation tool",
      timeout: 300_000,
      intervals: [3_000],
    })
    .toBeGreaterThanOrEqual(1);

  // Let the turn *finish* before reading the board. This is the assertion two
  // runs of this spec were quietly wrong without: a turn's delegations are
  // drained **after** it, so a board read taken while the company is still
  // answering sees whichever cards happen to exist at that instant. Both runs
  // then dispatched, settled and closed out one card of a two-card goal — and
  // reported green over the half they never looked at.
  await waitForTurn(page);
  const opened = await newCards(request, before);
  // The cap is the host's own (`MAX_DELEGATIONS_PER_TURN`), so more than three
  // means something other than this turn wrote to the board.
  expect(opened.length, "more cards than one turn can open").toBeLessThanOrEqual(3);

  // Somebody real owns the work. An unassigned card is a to-do list entry; the
  // claim this lane exists for is that the orchestrator *delegated*.
  const owned = opened.filter((card) => card.assignee && team.includes(card.assignee));
  expect(
    owned.length,
    `no card was given to anyone on the roster (${opened
      .map((card) => `${card.title} → "${card.assignee ?? ""}"`)
      .join("; ")})`,
  ).toBeGreaterThanOrEqual(1);

  // The reply is a model's, not the fixture's. Cheap, and it is the one line
  // that tells a confused reader which lane they are actually running.
  await expect(page.getByText("__MOCK_LLM__")).toHaveCount(0);

  // ── 2. The operator starts the work ────────────────────────────────────
  // Whatever is still unstarted gets dispatched by hand, on the board. A card
  // the orchestrator already ran (a `delegate_to_teammate` hand-off opens one
  // mid-turn) is left alone rather than re-run.
  for (const card of opened) {
    const current = (await allCards(request)).find((held) => held.id === card.id);
    if ((current?.stage ?? current?.column) === "pending") {
      await dispatch(page, card.title);
    }
  }

  // ── 3. …and the team does it ───────────────────────────────────────────
  // Settled, not finished-in-a-particular-way: a real turn can land in review,
  // or park on an approval, and which of those happens is the model's business.
  // What must not happen is a card left running forever.
  for (const card of opened) {
    await expect
      .poll(
        async () => {
          const held = (await allCards(request)).find((row) => row.id === card.id);
          // A card that has gone missing keeps the poll waiting rather than
          // concluding: the alternative reads as "settled" for a card that no
          // longer exists.
          // Reduced to a boolean here rather than asserted with a set matcher,
          // because `expect.poll` has no `toBeOneOf`. The landing itself is
          // read back below for the message, so nothing is lost by it.
          return held ? SETTLED.includes(landing(held)) : false;
        },
        {
          message: `card "${card.title}" never settled`,
          timeout: 420_000,
          intervals: [5_000],
        },
      )
      .toBe(true);
  }

  // ── 4. The operator asks for it to be closed out ───────────────────────
  // The ids are named because an operator naming them is the realistic ask —
  // "the two you opened" would be a test of the model's memory of its own turn,
  // which is a different claim from the one this spec makes.
  const settled = (await allCards(request)).filter((held) =>
    opened.some((card) => card.id === held.id),
  );
  const reviewable = settled.filter((card) => landing(card) === "in_review");
  // A hard failure rather than a skip, deliberately. `paused` is a legitimate
  // landing for a single run — a turn that needed a tool it has no credential
  // for, or an approval nobody has decided — but a goal where *nothing* came
  // back for acceptance did not close, and that is the whole claim of this
  // lane. Skipping here would report green over exactly the outcome the spec
  // exists to notice, which is the pathology `CLAUDE.md` names for Rust targets
  // ("builds, runs and reports zero without failing anything").
  //
  // The landings are named in the message because the next question is always
  // which one it was: `paused` sends you to the approvals queue, `pending` to a
  // dispatch that never happened.
  expect(
    reviewable.length,
    `no card came back for acceptance — the run landed as ${settled
      .map((card) => `"${card.title}": ${landing(card)}`)
      .join("; ")}`,
  ).toBeGreaterThan(0);

  // **One card per message**, and that is not fastidiousness. Asked to accept
  // two cards in one sentence, the model under test approved the first, wrote a
  // paragraph about both, and never called `review_task` again — so the second
  // card sat in review while the reply said it was finished. Nothing is broken
  // there: the host's own cap allows three delegations per turn, and the tool
  // was reachable the whole time. It is simply not a claim about the product
  // that a model will always finish a batched instruction, and a lane that
  // asserted it would be measuring the model's diligence rather than the
  // company's plumbing.
  for (const card of reviewable) {
    await openOrchestratorDm(page);
    await say(
      page,
      `The work on card ${card.id} ("${card.title}") is back and it looks good to ` +
        "me. Please review and approve that card so it is marked done.",
    );

    await expect
      .poll(
        async () => {
          const held = (await allCards(request)).find((row) => row.id === card.id);
          return held?.column ?? "";
        },
        {
          message:
            `card "${card.title}" was never accepted — the orchestrator did not ` +
            "call review_task, or called it with a different id",
          timeout: 300_000,
          intervals: [5_000],
        },
      )
      .toBe("done");
  }

  // ── 5. The board an operator comes back to ─────────────────────────────
  await page.reload();
  await openBoard(page);
  for (const card of reviewable) {
    await expect(column(page, DONE)).toContainText(card.title, { timeout: 60_000 });
  }
});
