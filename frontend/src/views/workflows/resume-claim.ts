// What deciding a blocked run's parked cards actually does — one answer, for
// every screen that makes the claim (issue B-013).
//
// # The bug this exists to stop
//
// A founder parked a run on an approval and read two screens about it:
//
//   Workflow detail: "This run parked 1 approval. Approve it in Approvals and
//   this run continues on its own — approving re-runs the step, so a changed
//   decision may ask again."
//
//   Observatory: "The card is a question the agent raised, not a call waiting
//   to be authorised: answering it is recorded against the card, but it does
//   not restart this run — re-run the workflow once the answer is in hand."
//
// Wait, or re-run? One of those was teaching them the wrong model of what their
// agents do.
//
// # Which one was right
//
// The Observatory. There are two kinds of card behind a blocked node and they
// behave oppositely:
//
//   * A **gated tool call**'s park carries the node's turn key, so a verdict
//     re-runs the turn and the run goes on by itself.
//   * A **blocker** — a question the agent raised — is parked with no
//     continuation, deliberately, because answering a question is not the act
//     of authorising a call. `resume_node_blocker` (`src/company/runtime.rs`)
//     banks the answer, delivers it into the DM the question was asked in, and
//     then re-dispatches the node itself from the run's own trigger input,
//     carrying the answer onto it (issues #1863, #2005). So answering it also
//     restarts the run — just by a different route than a gated call's.
//
// The host has always known the difference — `ParkedCalls::blockers`
// (`src/workflows/caps/mod.rs`) is what `blocked_diagnosis` branches on to word
// the Observatory's sentence in three ways. What it did not do was put that
// count on the wire, so the console's run drawer had no way to ask, and stated
// the gated-call behaviour for both kinds. `WorkflowBlockedNode.blockers` is
// that count, and this module is the one place either console screen turns it
// into a sentence.
//
// # Why the wording matches the host's, phrase for phrase
//
// Because the founder read both screens about one run. Two correct sentences
// that word the same fact differently still read as two claims, and checking
// whether they agree is work an operator should not be doing. The parity test
// in `test/unit/blocked-run-resume-claim.test.ts` reads `blocked_diagnosis`'s
// own source and fails if the two drift apart.

import type { WorkflowBlockedNode } from "@/api/workflows";

/**
 * How a blocked run's still-decidable cards split between the two kinds.
 *
 * `unknown` is not a third kind — it is the honest state for a host that
 * predates `WorkflowBlockedNode.blockers`, whose runs report a count of zero
 * for a node that may well be holding a question. Assuming "all gated" there
 * would reintroduce exactly the false promise this module exists to remove.
 */
export interface ParkedSplit {
  /** Cards that authorise a gated tool call. Approving one continues the run. */
  gated: number;
  /** Cards that are a question the agent raised. Answering one records, and stops there. */
  blockers: number;
  /** Whether any blocked node came from a host that cannot answer the question. */
  unknown: boolean;
}

/**
 * Split a blocked run's cards by what deciding them does.
 *
 * Counted over `approvalIds` rather than over the live queue, because this is
 * the same receipt the surrounding copy's own counts come from — mixing a
 * receipt count with a live one is how a sentence comes to describe a different
 * set of cards than the number beside it.
 *
 * A node whose `blockers` key is absent while it holds cards is `unknown`: the
 * field is `skip_serializing_if = "is_zero"` on the host, so absent means
 * either "no blockers" or "this host never sends it", and only the second is
 * dangerous. Distinguishing them is not possible from one node, so the
 * conservative reading is taken and the claim degrades to the one sentence that
 * is true either way.
 */
export function parkedSplit(nodes: readonly WorkflowBlockedNode[]): ParkedSplit {
  let gated = 0;
  let blockers = 0;
  let unknown = false;
  for (const node of nodes) {
    const cards = node.approvalIds?.length ?? 0;
    if (cards === 0) continue;
    if (node.blockers === undefined) {
      unknown = true;
      continue;
    }
    const questions = Math.min(node.blockers, cards);
    blockers += questions;
    gated += cards - questions;
  }
  return { gated, blockers, unknown };
}

/**
 * The claim about what deciding this run's cards does, or `null` when the run
 * has nothing decidable and the caller should say so its own way.
 *
 * Four outcomes, mirroring `blocked_diagnosis`'s three branches plus the
 * degraded one an older host forces:
 *
 * - only gated calls → approving continues the run automatically;
 * - only questions → answering re-enters the node, by approving it again or
 *   stopping the run;
 * - both, or an unclassifiable node → say both, naming the verdicts each kind
 *   actually takes.
 *
 * The mixed sentence is deliberately the fallback rather than an error state.
 * It is true whichever kind the cards turn out to be, so a host that cannot
 * answer costs the operator precision and never accuracy.
 */
export function resumeClaim(split: ParkedSplit): string | null {
  const { gated, blockers, unknown } = split;
  if (gated + blockers === 0 && !unknown) return null;
  if (unknown || (gated > 0 && blockers > 0)) {
    return (
      "Some of these are gated tool calls, which continue this run when approved; the rest are " +
      "questions the agent raised, which re-enter the step they stopped — approving runs it " +
      "again, and denying stops the run."
    );
  }
  if (blockers === 0) {
    return (
      "Approving the card continues this run automatically; because approving re-runs the " +
      "agent's turn, a changed decision may ask again."
    );
  }
  return (
    `${blockers === 1 ? "The card is" : "The cards are"} a question the agent raised, not a ` +
    "call waiting to be authorised: answering it re-enters this step — approving runs it " +
    "again, and denying stops the run."
  );
}

/** {@link resumeClaim} straight off a run's blocked nodes. */
export function resumeClaimFor(nodes: readonly WorkflowBlockedNode[]): string | null {
  return resumeClaim(parkedSplit(nodes));
}
