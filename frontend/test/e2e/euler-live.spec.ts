import { expect, test, type APIRequestContext, type Page } from "@playwright/test";

import { EULER, EULER_REASON, LIVE_LLM, LIVE_LLM_REASON } from "./capabilities";
import {
  ROUNDS,
  SETTLED,
  allCards,
  approveAll,
  chosenProblem,
  computed,
  landing,
  lastCompanyMessage,
  ledgerEntries,
  parked,
  pending,
  states,
  transcript,
  type Card,
} from "./euler";
import { dispatch, openOrchestratorDm, say, silenceTour } from "./orchestration";

/**
 * **The whole company, against a problem that has a right answer.**
 *
 * Every other end-to-end spec here asserts that the machinery ran: a card was
 * opened, a turn fired, a marker appeared, a column changed. Those are the
 * right claims and they share one limit — the *content* of what the company
 * produced is never checked, because prose has no pass condition. So a lab that
 * delegated correctly, ran every turn, closed every card and reached a
 * confidently wrong conclusion is green everywhere in this directory.
 *
 * This lane closes that. The company under test is `companies/math_lab`
 * and the goal is a Project Euler problem, which is the rare piece of work with
 * an operator-checkable outcome: one exact integer, published, and reachable
 * only by computing it. The verdict is that integer, so what passes here is not
 * "the orchestration ran" but "the orchestration produced the right answer".
 *
 * # What is asserted, and what is only observed
 *
 * **Asserted:** the lab, asked at the end for the final answer, states the
 * published integer.
 *
 * **Observed and reported, never asserted:** which tools were used, how the
 * work was split, how many rounds it took, and whether the answer was filed on
 * the `answers` ledger. Each of those is a preference about method, and this
 * company's own brief says a different route to the same number is the same
 * job done. A lane that failed a correct answer over its bookkeeping would be
 * measuring diligence rather than capability.
 *
 * The one method claim that *is* enforced is `computed()`: the answer has to
 * have come out of a program the lab ran. That, and not the manifest, is what
 * makes the number evidence. `math_lab` holding no `web` or `search`
 * grant (see `SEARCH_DENIED_COMPANIES` in `src/company/content_test.rs`)
 * removes the shortcut an agent would reach for first, but it closes no
 * network — `shell` is granted and there is no sandbox
 * (`docs/spec/security/agent-isolation.md`) — so "it could not have looked the
 * answer up" is not something this lane may claim.
 *
 * # Why the operator has to keep asking
 *
 * A turn ends when the model stops calling tools, and a hard problem does not
 * fit in one. So this spec is written as the operator it is simulating: state
 * the problem, start whatever was opened, wait for it to settle, ask where
 * things stand, repeat — up to `PW_EULER_ROUNDS` times. The loop exits early
 * the moment the answer appears, so an easy problem costs one round and a hard
 * one costs what it costs.
 *
 * That shape is also the finding this lane most often produces. When a run
 * fails it is usually not that the mathematics was too hard; it is that the
 * company stopped — a card in `pending` nobody dispatched, a reply that
 * described the plan instead of running it. Those are product bugs and they are
 * invisible to a spec that only checks a card moved.
 *
 * # Not run by CI
 *
 * It spends real tokens, takes tens of minutes, and its verdict depends on a
 * model's reasoning, which is the one thing a required check must not. It is
 * run by a person: `npm run e2e:euler`, optionally `PW_EULER_PROBLEM=61`.
 */

/**
 * One turn of the crank: answer whatever is waiting on the operator, then say
 * whether every card has stopped moving.
 *
 * The two are one function because they have to happen on the same clock. An
 * agent-picked `shell` call parks and is refused *inline* — the tool answers
 * "blocked, requires approval" and the turn carries on without it — so an
 * approval nobody answers is not a pause, it is work that silently did not
 * happen. Polling for settled cards while ignoring the approvals queue is how
 * this lane's first run watched a lab get refused its sandbox, fall back to
 * doing arithmetic in its head, and report a right answer for the wrong reason.
 *
 * Everything approved is accumulated in `approved`, which is what
 * `computed()` reads at the end.
 */
async function pump(
  page: Page,
  request: APIRequestContext,
  approved: string[],
): Promise<boolean> {
  approved.push(...(await approveAll(request)));
  // The turn itself, from the same indicator `waitForTurn` reads. It is part of
  // "quiet" rather than a separate wait because the two cannot be sequenced
  // here: approving `run_workflow` starts a whole workflow run, whose agent
  // nodes hold the indicator up for many minutes, and a turn wait that was not
  // also answering approvals would time out on work it was itself blocking.
  // That is how the first run of this loop failed — ten minutes of `working`
  // while a queue nobody was draining held the next step.
  if ((await page.getByTestId("working-indicator").count()) > 0) return false;
  const cards = await allCards(request);
  const moving = cards.some((card) => !SETTLED.includes(landing(card)));
  // An empty board counts as quiet, deliberately. The lab's own workflow does
  // its work in `agent` nodes rather than in board cards, so a run that went
  // that way legitimately never opens one — and a poll that waited for a card
  // would wait out its timeout on a company that was busy solving. Quiet here
  // means "nothing is waiting on me and nothing is moving"; whether anything
  // was *achieved* is the answer's business, and the loop asks about that
  // separately.
  return !moving && (await parked(request)).length === 0;
}

/**
 * Waits for the company to go quiet, answering approvals the whole time.
 *
 * The lane's one wait. It replaces `waitForTurn` here — not because that helper
 * is wrong, but because it measures one turn, and a nudge to this company can
 * set off a workflow run whose agent nodes are turns of their own. What an
 * operator is actually waiting for is "nothing is running and nothing is
 * waiting on me", which is what this returns.
 *
 * Reads the indicator from the conversation view, so it opens it first: on the
 * board — where a dispatch leaves the page — there is no indicator to read and
 * every wait would return instantly.
 */
async function waitQuiet(
  page: Page,
  request: APIRequestContext,
  approved: string[],
  budgetMs = 30 * 60_000,
): Promise<void> {
  await openOrchestratorDm(page);
  const deadline = Date.now() + budgetMs;
  // One settled reading is not enough: a turn that has just ended has not yet
  // drained its delegations, so the board is momentarily quiet in the middle of
  // starting work. Two consecutive quiet readings, five seconds apart, is the
  // same shape `settledMarkerCount` uses in `orchestration.ts`.
  let quiet = 0;
  while (Date.now() < deadline) {
    quiet = (await pump(page, request, approved)) ? quiet + 1 : 0;
    if (quiet >= 2) return;
    await page.waitForTimeout(5_000);
  }
  throw new Error(
    `the company never went quiet — cards: ${describe(await allCards(request))}; ` +
      `still parked: ${(await parked(request)).map((one) => one.kind).join(", ") || "nothing"}`,
  );
}

/** A one-line summary of the board, for a failure message worth reading. */
function describe(cards: Card[]): string {
  if (cards.length === 0) return "(no cards)";
  return cards.map((card) => `"${card.title}" [${landing(card)}] ${card.assignee ?? "-"}`).join("; ");
}

test("the lab takes a Project Euler problem and returns the published answer", async ({
  page,
  request,
}) => {
  test.skip(!LIVE_LLM, LIVE_LLM_REASON);
  test.skip(!EULER, EULER_REASON);

  const problem = chosenProblem();
  // Rounds of real model turns, each of which may be several teammate turns of
  // its own. Timed for a slow rung rather than a fast one: a lane that fails on
  // latency teaches nobody anything.
  test.setTimeout(90 * 60_000);

  /** Every effect the operator approved, in the order they were asked for. */
  const approved: string[] = [];

  await silenceTour(page);
  await openOrchestratorDm(page);

  // ── 1. The problem, as an operator would state it ──────────────────────
  // The statement is handed over in full and the *method* is left entirely to
  // the company: which desks, which tools, what order. The only instructions
  // are the two facts the lab could not otherwise know — that the answer is an
  // exact integer, and that it is to be computed rather than recalled.
  //
  // That second one is phrased as the instruction it is, not as a fact about
  // the environment. An earlier draft told the company it had "no internet
  // access", which is false: `shell` is granted and nothing sandboxes the
  // network (`docs/spec/security/agent-isolation.md`). Handing a model a false
  // premise about its own tools is a poor way to ask for honest work.
  await say(
    page,
    `Project Euler Problem ${problem.id} — ${problem.title}.\n\n` +
      `${problem.statement}\n\n` +
      "Please have the lab solve this. The answer is one exact integer, and I want it " +
      "computed here rather than recalled or looked up — a program that runs, with a " +
      "second route agreeing. Use your team. When the answer has been checked, tell me " +
      "what it is.",
  );
  await waitQuiet(page, request, approved);

  // The first failure this lane can have, separated from every other one: the
  // model was never reached, or was reached and did nothing at all.
  //
  // "Did something" is deliberately broader than "opened a card". A first turn
  // that hands work to a desk opens one; a first turn that reaches for the
  // lab's saved workflow opens none and parks a `run_workflow` approval
  // instead, and both are the company acting on the goal. An earlier version of
  // this check demanded a card and failed a run whose orchestrator had done
  // exactly what its brief asks — which was a bug in the spec, not in the lab.
  //
  // The poll pumps approvals while it waits, because the follow-up that opens
  // the cards cannot happen until the operator has answered.
  await expect
    .poll(
      async () => {
        approved.push(...(await approveAll(request)));
        return (await allCards(request)).length + approved.length;
      },
      {
        message:
          "the lab neither opened work nor asked for anything — either the model was " +
          "not reached (check the live-brain proxy's stderr for a non-200) or it " +
          "answered in prose without acting",
        timeout: 300_000,
        intervals: [3_000],
      },
    )
    .toBeGreaterThan(0);

  // ── 2. Rounds: start what is waiting, let it run, ask where it stands ───
  let rounds = 0;
  let stated: string | undefined;

  for (rounds = 1; rounds <= ROUNDS; rounds += 1) {
    // Anything the orchestrator parked with `spawn_task` needs the operator's
    // gesture to start — that is the spend gate (#1512), and a lane that
    // reached into the API to start work would be skipping the product.
    for (const card of pending(await allCards(request))) {
      await dispatch(page, card.title);
    }

    await waitQuiet(page, request, approved);

    // Has the lab said the number yet? Read from the journal rather than the
    // DOM, and across every message the operator did not write rather than only
    // the last one: a teammate's reply and a workflow's report both land in the
    // thread, and the answer arriving in one of those is the answer arriving.
    // The operator's own messages are excluded because the problem statement
    // carries numbers of its own.
    const said = (await transcript(request)).find(
      (message) => !message.mine && states(message.text, problem.answer),
    );
    if (said) {
      stated = said.text;
      break;
    }

    if (rounds === ROUNDS) break;

    // Not finished. Ask the way an operator would — for the state of things,
    // not for a particular tool — and let the company decide what to do next.
    await openOrchestratorDm(page);
    await say(
      page,
      "Where does this stand? If the answer is not yet checked, please carry on: " +
        "hand out whatever is still outstanding and come back to me when a second, " +
        "independent route agrees with the first.",
    );
  }

  // ── 3. The operator asks for the answer, plainly ───────────────────────
  // A final, explicit ask so the verdict reads on a deliberate statement rather
  // than on a number that happened to appear mid-working. Cheap, and it is what
  // makes a failure message quotable.
  await openOrchestratorDm(page);
  await say(
    page,
    `What is the final answer to Project Euler Problem ${problem.id}? ` +
      "Reply with the integer on its own line and nothing else.",
  );
  await waitQuiet(page, request, approved);

  approved.push(...(await approveAll(request)));
  const final = await lastCompanyMessage(request);
  const cards = await allCards(request);
  const answers = await ledgerEntries(request, "answers");
  const attempts = await ledgerEntries(request, "attempts");

  // Everything observed rather than asserted, attached to the run so a pass is
  // as readable as a failure: how long it took, how the work was split, and
  // whether the lab did its own bookkeeping.
  test.info().annotations.push(
    { type: "problem", description: `${problem.id} — ${problem.title} (${problem.why})` },
    { type: "rounds", description: `${rounds} of ${ROUNDS}` },
    { type: "board", description: describe(cards) },
    {
      type: "approved",
      description: approved.length === 0 ? "nothing was asked for" : approved.join(", "),
    },
    {
      type: "answers ledger",
      description:
        answers.length === 0
          ? "no row filed"
          : answers.map((row) => `${row.title}: ${row.fields.answer ?? "(no answer field)"}`).join("; "),
    },
    { type: "attempts ledger", description: `${attempts.length} row(s)` },
    { type: "final reply", description: final?.text?.slice(0, 500) ?? "(none)" },
  );

  expect(
    final,
    "the lab never answered the final question — the last message in the orchestrator DM is " +
      "the operator's own",
  ).toBeTruthy();

  // The lab has to have *run* something. Every published answer is in a model's
  // training data, so a lane that accepted a recalled number would report green
  // over a company whose sandbox had stopped working entirely — which is what
  // the first run of this lane did before this assertion existed.
  expect(
    computed(approved),
    "the lab never ran a program — it was never refused anything either, so the " +
      `number it reported was recalled rather than computed (approved: ${
        approved.join(", ") || "nothing"
      })`,
  ).toBe(true);

  // The verdict. `stated` is preferred when the loop already saw the answer,
  // because a lab that answered correctly in round two and then rambled in the
  // final message has still solved the problem.
  const said = stated ?? final?.text ?? "";
  expect(
    states(said, problem.answer),
    `the lab did not reach the published answer for Problem ${problem.id}.\n` +
      `  expected: ${problem.answer}\n` +
      `  it said:  ${(final?.text ?? "").slice(0, 800)}\n` +
      `  board:    ${describe(cards)}`,
  ).toBe(true);
});
