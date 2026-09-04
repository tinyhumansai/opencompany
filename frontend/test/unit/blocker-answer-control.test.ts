// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { OpenCompanyClient } from "@/api/client";
import type { ApprovalSummary, GrantScope, Verdict } from "@/api/types";
import { isAnswerOnlyBlocker } from "@/components/approval-card";
import { answerRecordedLine } from "@/lib/approval-wording";
import { ApprovalCard } from "@/views/ApprovalsView";
import { ApprovalRow, type ApprovalRowVariant } from "@/views/chat/ApprovalRow";

/**
 * The console control an operator answers a blocker's question in (defect
 * B-046, 2026-09-02).
 *
 * ## The defect
 *
 * A card parked with "parked on your answer to the two questions I asked". The
 * only text control anywhere near it was the Discussion tab, whose posts carry
 * no approval id and never touch a blocker — so an answer typed there was
 * stored and ignored, and every Approve re-entered the step with the same
 * input, re-asked, and re-billed the turn (~$0.0287 an attempt) with reasoning
 * that read "nothing's changed on the two decisions I parked with you".
 *
 * ## Why this suite renders instead of calling a helper
 *
 * On `approval-batch-card`'s precedent, and for the same reason: the claims
 * here are only true *at the click*. That a blocker offers a box and a
 * `payment.send` does not, that a board card does not grow one it has no room
 * for, and above all that what is typed in it reaches the request — none of
 * those can be seen from a pure function, which can only observe that a
 * component was built.
 *
 * The wire half is tested against the real client with a recording transport,
 * on `budget-pause-redeem-wire`'s precedent: the field's whole contract is that
 * an omitted `answer` leaves the request byte-identical to the one every
 * existing caller already sends, and that is a statement about the body.
 */

const T0 = new Date("2026-09-02T10:00:00Z").getTime();

/** An agent's parked question — the kind this control exists for. */
function blocker(id: string): ApprovalSummary {
  return {
    id,
    kind: "blocker.information",
    amount_usd: null,
    at_millis: T0,
    agent: "ceo",
    thread: "desk-founder",
    // An agent's question parks ungrouped, which is why it never batches.
    group_key: null,
    broadly_grantable: false,
    payload: {
      reason: "parked on your answer to the two questions I asked",
      needed: "a decision on pricing and on the launch date",
    },
  };
}

/** A gated call, which asks for consent rather than for an answer. */
function payment(id: string): ApprovalSummary {
  return {
    id,
    kind: "payment.send",
    amount_usd: 42.5,
    at_millis: T0,
    agent: "ceo",
    thread: "desk-founder",
    broadly_grantable: false,
    payload: { to: "vendor@example.test", amount_usd: 42.5 },
  };
}

interface Decision {
  id: string;
  verdict: Verdict;
  answer?: string;
}

let container: HTMLDivElement;
let root: Root;
let decisions: Decision[];

async function render(
  approvals: ApprovalSummary[],
  variant: ApprovalRowVariant = "full",
) {
  await act(async () => {
    root.render(
      createElement(ApprovalRow, {
        approvals,
        now: T0 + 60_000,
        askerNames: new Map([["ceo", "Founder"]]),
        variant,
        deciding: new Map<string, Verdict>(),
        decided: {},
        failed: {},
        onDecide: (
          approval: ApprovalSummary,
          verdict: Verdict,
          _scope: GrantScope,
          answer?: string,
        ) => decisions.push({ id: approval.id, verdict, answer }),
      }),
    );
  });
}

/** The answer box, if this surface offers one. */
function box(): HTMLTextAreaElement | null {
  return container.querySelector<HTMLTextAreaElement>(
    "[data-blocker-answer] textarea",
  );
}

function button(label: string): HTMLButtonElement {
  const match = [...container.querySelectorAll("button")].find((b) =>
    (b.textContent ?? "").includes(label),
  );
  if (!match) throw new Error(`no "${label}" button: ${container.textContent}`);
  return match as HTMLButtonElement;
}

/**
 * Type into the box the way a person does.
 *
 * Through the prototype's own setter, not `el.value = …`: React tracks an
 * element's last-rendered value to decide whether an input event changed
 * anything, so assigning directly makes the event look like a no-op and
 * `onChange` never fires.
 */
async function type(el: HTMLTextAreaElement, text: string) {
  const setter = Object.getOwnPropertyDescriptor(
    HTMLTextAreaElement.prototype,
    "value",
  )!.set!;
  await act(async () => {
    setter.call(el, text);
    el.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

async function click(el: HTMLElement) {
  await act(async () => {
    el.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  decisions = [];
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("the blocker answer control", () => {
  it("offers a box on a blocker card, and none on a gated call", async () => {
    await render([blocker("b1")]);
    expect(box()).not.toBeNull();

    // The other half of the same claim. A payment asks for consent, not for an
    // answer, and a box on it would invite words the host has nowhere to put.
    await render([payment("p1")]);
    expect(box()).toBeNull();
  });

  it("labels the box as the operator's answer rather than a note", async () => {
    // The copy is load-bearing: this is the operator answering a question an
    // agent asked, and "add a note" would describe the Discussion tab — the
    // control that looked like this one and did nothing (B-046).
    await render([blocker("b1")]);
    const label = container.querySelector<HTMLLabelElement>(
      "[data-blocker-answer] label",
    );
    expect(label?.textContent).toContain("Your answer");
    // Tied to the box, so the label is what a screen reader announces on focus.
    expect(label?.htmlFor).toBe(box()!.id);
    expect(box()!.getAttribute("placeholder")).toContain("Answer what was asked");
  });

  it("carries what was typed to Approve, and nothing to Decline", async () => {
    await render([blocker("b1")]);
    await type(box()!, "Price at $49. Launch on the 14th.");
    await click(button("Approve"));
    expect(decisions).toEqual([
      { id: "b1", verdict: "approve", answer: "Price at $49. Launch on the 14th." },
    ]);

    // A refusal ends the step rather than re-entering it, and the host ignores
    // `answer` on a deny — sending it would be the console claiming something
    // happened to the operator's words that does not.
    decisions = [];
    await render([blocker("b2")]);
    await type(box()!, "no");
    await click(button("Decline"));
    expect(decisions).toEqual([{ id: "b2", verdict: "deny", answer: undefined }]);
  });

  it("approves with no answer when the box was never touched", async () => {
    // The pre-B-046 decision, still available and still correct: "go ahead" is
    // sometimes the whole answer, and an empty box must not become a required
    // field standing between the operator and a one-click approve.
    await render([blocker("b1")]);
    await click(button("Approve"));
    expect(decisions).toEqual([{ id: "b1", verdict: "approve", answer: "" }]);
  });

  it("renders the box in a chat transcript and not on a board card", async () => {
    // `compact` is the transcript row, which is where the question was asked,
    // so it grows for the one kind that asked one.
    await render([blocker("b1")], "compact");
    expect(box()).not.toBeNull();

    // `card` is a `w-65` board column — roughly 220px of content, and a drag
    // target besides. It sends the operator to its own rows on the Approvals
    // page instead of drawing a box it cannot make legible.
    await render([blocker("b1")], "card");
    expect(box()).toBeNull();
  });

  it("offers no box over a batch, where one answer would cover work it was not written for", async () => {
    // Unreachable in practice — an agent's question parks with no `group_key`,
    // so a blocker never batches — and checked anyway, because the host appends
    // the answer to the card each approval is linked to and the cost of being
    // wrong is one operator's words attached to somebody else's card.
    await render([blocker("b1"), blocker("b2")]);
    expect(box()).toBeNull();
  });
});

/** A client whose transport records the body of every request. */
function recordingClient() {
  const bodies: string[] = [];
  const transport = {
    request: async (req: { method: string; url: string; body?: string }) => {
      bodies.push(req.body ?? "");
      return {
        status: 200,
        statusText: "OK",
        url: req.url,
        text: JSON.stringify({ outcome: "settled" }),
        header: () => null,
      };
    },
    subscribe: () => () => {},
  };
  const client = new OpenCompanyClient(
    { baseUrl: "", company: "acme", operatorToken: "t0ken", sessionHeader: null },
    transport as never,
  );
  return { client, bodies };
}

describe("client.resolveApproval — the `answer` wire contract", () => {
  it("puts a typed answer on the wire as `answer`", async () => {
    const { client, bodies } = recordingClient();
    await client.resolveApproval("a1", "approve", "Price at $49.", "acme");
    expect(JSON.parse(bodies[0])).toEqual({ verdict: "approve", answer: "Price at $49." });
  });

  it("trims the answer, so leading and trailing whitespace never reaches the card note", async () => {
    const { client, bodies } = recordingClient();
    await client.resolveApproval("a1", "approve", "  ship it  ", "acme");
    expect(JSON.parse(bodies[0]).answer).toBe("ship it");
  });

  it("omits the field entirely for an empty, whitespace-only or absent answer", async () => {
    // The compatibility rule the `scope` field already follows: an omitted
    // field is what an old host understands, so every existing caller — all of
    // which pass nothing — keeps sending byte-identical requests, and a new
    // console against an old host keeps working rather than 400ing on a key
    // that host has never heard of.
    const { client, bodies } = recordingClient();
    await client.resolveApproval("a1", "approve", undefined, "acme");
    await client.resolveApproval("a2", "approve", "", "acme");
    await client.resolveApproval("a3", "approve", "   \n\t ", "acme");
    for (const body of bodies) {
      expect(JSON.parse(body)).toEqual({ verdict: "approve" });
      expect(body).not.toContain("answer");
    }
  });

  it("leaves the grant scope and detach fields untouched beside it", async () => {
    const { client, bodies } = recordingClient();
    await client.resolveApproval("a1", "approve", "yes", "acme", {
      detach: true,
      scope: { kind: "tool", expiresInMillis: 86_400_000 },
    });
    expect(JSON.parse(bodies[0])).toEqual({
      verdict: "approve",
      detach: true,
      scope: "tool",
      expires_in_millis: 86_400_000,
      answer: "yes",
    });
  });
});

/**
 * The Approvals page's own card (`ApprovalsView.ApprovalCard`), which is a
 * separate component from `ApprovalRow` with its own resolve call.
 *
 * It has to carry the box because it is where the other two surfaces send an
 * operator who needs room: the board card's "View details" and a condensed
 * row's "Read it first" both land on `#/approvals/<taskId>`. A fix that reached
 * only the shared row would leave the full decision surface unable to answer.
 */
describe("the Approvals page card", () => {
  async function renderCard(approval: ApprovalSummary) {
    await act(async () => {
      root.render(
        createElement(ApprovalCard, {
          approval,
          now: T0 + 60_000,
          askerNames: new Map([["ceo", "Founder"]]),
          deciding: null,
          batchIndex: 1,
          batchTotal: 1,
          onDecide: (verdict: Verdict, _scope: GrantScope, answer?: string) =>
            decisions.push({ id: approval.id, verdict, answer }),
        }),
      );
    });
  }

  it("offers the box on a blocker and not on a gated call", async () => {
    await renderCard(blocker("b1"));
    expect(box()).not.toBeNull();

    await renderCard(payment("p1"));
    expect(box()).toBeNull();
  });

  it("sends what was typed with Approve and nothing with Decline", async () => {
    await renderCard(blocker("b1"));
    await type(box()!, "Price at $49.");
    await click(button("Approve"));
    expect(decisions).toEqual([
      { id: "b1", verdict: "approve", answer: "Price at $49." },
    ]);

    decisions = [];
    await renderCard(blocker("b2"));
    await type(box()!, "no");
    await click(button("Decline"));
    expect(decisions).toEqual([{ id: "b2", verdict: "deny", answer: undefined }]);
  });

  /**
   * A typed answer lives only in this card's own `useState` — the single-row
   * Approve button above reads it fine, but a page-level bulk action
   * ("Approve all" in `ApprovalsView.decideAll`) has no card to read it off.
   * Before `onAnswerChange`, a batch containing an answered blocker sent that
   * row as a wordless retry regardless of what was typed: the host re-ran the
   * stopped step unchanged and could re-ask and re-bill for a question the
   * operator had just answered, with nothing on screen saying the box's
   * contents were about to be dropped. This is the mirror `decideAll` reads
   * from — see its own doc in `ApprovalsView.tsx`.
   */
  it("mirrors every keystroke to onAnswerChange, for a bulk decision with no card to ask", async () => {
    const seen: string[] = [];
    await act(async () => {
      root.render(
        createElement(ApprovalCard, {
          approval: blocker("b3"),
          now: T0 + 60_000,
          askerNames: new Map([["ceo", "Founder"]]),
          deciding: null,
          batchIndex: 1,
          batchTotal: 1,
          onDecide: () => {},
          onAnswerChange: (value: string) => seen.push(value),
        }),
      );
    });
    await type(box()!, "Route it to ops.");
    expect(seen).toContain("Route it to ops.");
  });

  it("puts the box above the decide footer, not below it", async () => {
    // #1406's rule, applied to the newest thing Approve carries: a card is read
    // top to bottom and commits at the bottom, so a control that changes what
    // Approve sends must come before the button. The same mistake in reverse —
    // an answer box under the footer — would be reachable only after the
    // operator had already decided.
    await renderCard(blocker("b1"));
    const answerBox = container.querySelector<HTMLElement>("[data-blocker-answer]")!;
    const footer = container.querySelector<HTMLElement>('[data-testid="approval-decide"]')!;
    expect(
      answerBox.compareDocumentPosition(footer) & Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();
  });

  /**
   * Defect B-070: the console said the work was starting when nothing was.
   *
   * B-046 gave this card a field to answer in, which is half of what a founder
   * needs. The other half is being told the truth about what pressing Approve
   * did. A blocker parks two different things on one queue — a question raised
   * *from a card*, whose task link names it and whose approval really does move
   * that card back into In Progress, and a question asked mid-conversation, which
   * parks `unlinked` with no continuation at all. The console read the same
   * story off both.
   *
   * So a founder answered a genuine either/or ("route it to operations, or add
   * Marcus as a teammate?"), read "Approved — carrying it out now", watched the
   * card leave the queue, and twenty-five minutes later had no task, no teammate
   * and no reply — and no way to re-decide the question, because approving had
   * consumed it.
   *
   * This is B-013 and B-072's fault a third time: a park with no continuation
   * described as one that continues by itself. The host half is
   * `company::runtime::tests::blocker_resume_note`; these are the two places the
   * console says it.
   */
  describe("a question with nothing behind it is recorded, not carried out (B-070)", () => {
    /** B-070's card: an `escalate_to_human` question, parked unlinked. */
    function unlinkedQuestion(id: string): ApprovalSummary {
      return {
        ...blocker(id),
        // `escalate_to_human` stamps no teammate, which is why the banner fell to
        // the runtime wording rather than the teammate wording.
        agent: null,
        task: { link: "unlinked" },
        payload: {
          reason:
            "There's no teammate called Marcus. Should I route it to operations, or add Marcus as a new teammate?",
          needed: "an answer to the question on this card",
        },
      };
    }

    /** The same question, but raised from a card that really does resume. */
    function cardQuestion(id: string): ApprovalSummary {
      return { ...blocker(id), task: { link: "task", id: "t-1" } };
    }

    /**
     * The same question again, but raised by a workflow node — `unlinked`
     * like B-070's card, with no board card behind it either, but it really
     * does re-enter the node it stopped (issues #1863, #2005). Distinguished
     * from `unlinkedQuestion` only by `workflow_run_id`.
     */
    function workflowNodeQuestion(id: string): ApprovalSummary {
      return {
        ...blocker(id),
        agent: null,
        task: { link: "unlinked" },
        workflow_run_id: "run-1",
      };
    }

    it("tells the operator nothing restarts, on a question with no card behind it", async () => {
      await renderCard(unlinkedQuestion("b1"));
      const text = container.textContent ?? "";
      expect(text).toContain("nothing restarts on its own");
      // The claim B-070 was filed about must not be made here.
      expect(text).not.toContain("the teammate picks the card up with it");
    });

    // Defect B-124: once the host actually delivers a non-blank answer
    // (`deliver_blocker_answer`, `src/company/runtime.rs`), the unconditional
    // "nothing restarts on its own" claim above is wrong for the case this
    // box exists for. The footnote now tracks the box's own live value rather
    // than a static claim, so typing into it changes what the card says
    // before Approve is ever clicked.
    it("says the answer will be carried once something is typed (B-124)", async () => {
      await renderCard(unlinkedQuestion("b1"));
      await type(box()!, "Route it to operations.");
      const text = container.textContent ?? "";
      expect(text).toContain("carries your answer into that conversation");
      expect(text).not.toContain("nothing restarts on its own");
    });

    it("still promises the resume where a card really is picked up", async () => {
      // The other half of the property: this is not "delete the sentence". A
      // question raised from a card is re-dispatched, and saying so is correct.
      await renderCard(cardQuestion("b2"));
      const text = container.textContent ?? "";
      expect(text).toContain("the teammate picks the card up with it");
      expect(text).not.toContain("nothing restarts on its own");
    });

    it("promises the resume for a workflow node's question too, without claiming a card", async () => {
      // A third shape: `unlinked` like B-070's card, but a run behind it
      // (issues #1863, #2005) — neither of the other two sentences is true.
      await renderCard(workflowNodeQuestion("b4"));
      const text = container.textContent ?? "";
      expect(text).toContain("re-enters the workflow step");
      expect(text).not.toContain("nothing restarts on its own");
      expect(text).not.toContain("the teammate picks the card up with it");
    });

    it("keeps the answer box on all three — the distinction is the claim, not the field", async () => {
      await renderCard(unlinkedQuestion("b3"));
      expect(box()).toBeTruthy();
    });
  });
});


/**
 * The confirmation banner, which is where B-070's founder read the promise.
 */
describe("the approve confirmation for an answer-only question (B-070)", () => {
  it("says what happened, not that the work is starting", () => {
    expect(answerRecordedLine(false, "Do something that needs your sign-off")).toBe(
      "Answer recorded — this doesn't restart anything on its own: Do something that needs your sign-off",
    );
    // The literal sentence from the founder's screen.
    expect(answerRecordedLine(false)).not.toContain("carrying it out now");
  });

  // Defect B-124: a non-blank answer is no longer just banked — the host
  // carries it into the conversation, so the same unconditional "doesn't
  // restart anything" claim would now be false for the one case this box
  // exists for. `carried` is the caller's job to compute (a trimmed,
  // non-empty answer on an approve), not this function's.
  it("says the answer was carried once the host actually delivers it (B-124)", () => {
    const line = answerRecordedLine(true, "route it to operations");
    expect(line).not.toContain("doesn't restart anything on its own");
    expect(line).toContain("Answer sent");
    expect(line).toContain("route it to operations");
  });

  it("is chosen for exactly the cards that restart nothing", () => {
    const unlinked = { kind: "blocker.information", task: { link: "unlinked" } } as const;
    const fromCard = { kind: "blocker.information", task: { link: "task", id: "t-1" } } as const;
    const gatedCall = { kind: "payment.send", task: { link: "unlinked" } } as const;
    const workflowNode = {
      kind: "blocker.information",
      task: { link: "unlinked" },
      workflow_run_id: "run-1",
    } as const;

    expect(isAnswerOnlyBlocker(unlinked)).toBe(true);
    // A blocker the host says has a card behind it resumes, and must not be
    // demoted to "recorded".
    expect(isAnswerOnlyBlocker(fromCard)).toBe(false);
    // A gated tool call is not a question at all; approving it really does
    // continue the turn.
    expect(isAnswerOnlyBlocker(gatedCall)).toBe(false);
    // A workflow node's blocker is `unlinked` too — there is no card — but it
    // carries a `workflow_run_id`, and `resume_node_blocker` really does
    // re-dispatch that node from the run's trigger input (issues #1863,
    // #2005). The task link alone cannot tell this apart from the true
    // answer-only case above; the run id is what does.
    expect(isAnswerOnlyBlocker(workflowNode)).toBe(false);
    // A host that never sent the field is "we do not know", and guessing is
    // the failure this whole family of fixes is about.
    expect(isAnswerOnlyBlocker({ kind: "blocker.information" })).toBe(false);
  });
});
