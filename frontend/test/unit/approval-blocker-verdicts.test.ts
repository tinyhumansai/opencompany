// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { OpenCompanyClient } from "@/api/client";
import type {
  ApprovalSummary,
  BlockerVerdict,
  GrantScope,
  Verdict,
} from "@/api/types";
import { blockerEventVerdict } from "@/api/types";
import type { Transport, TransportRequest, TransportResponse } from "@/api/transport";
import { blockerDecidedLine } from "@/lib/approval-wording";
import {
  BLOCKER_VERDICTS,
  blockerVerdictConsequence,
  blockerVerdictLabel,
} from "@/lib/language";
import { ApprovalCard } from "@/views/ApprovalsView";

/**
 * **Issue #2028.** A parked blocker is a question with four answers, and the
 * Approvals page could only send two of them: approve became a retry, decline
 * became a cancel, and skip and amend were unreachable however the operator
 * meant to answer.
 *
 * The per-control body assertions here mirror the host's refusal tests one for
 * one. That pairing is the point: the two halves agree on four wire tokens and
 * on which `approve`/`deny` each rides, and a token that drifted on one side
 * would otherwise only show up as a 400 in front of an operator.
 */

const T0 = new Date("2026-03-02T10:00:00Z").getTime();

const BLOCKER: ApprovalSummary = {
  id: "b1",
  kind: "blocker.infrastructure",
  amount_usd: null,
  at_millis: T0,
  agent: "eng",
  broadly_grantable: false,
  payload: {
    reason: "the model id `gpt-nope` was rejected",
    needed: "a model id this provider serves",
  },
};

const GATED_CALL: ApprovalSummary = {
  id: "a1",
  kind: "shell",
  amount_usd: null,
  at_millis: T0,
  agent: "ops",
  broadly_grantable: true,
  payload: { command: "make release" },
};

interface Decision {
  verdict: Verdict;
  scope: GrantScope;
  blocker?: { verdict: BlockerVerdict; answer?: string };
}

let container: HTMLDivElement;
let root: Root;
let decisions: Decision[];

async function render(approval: ApprovalSummary) {
  decisions = [];
  await act(async () => {
    root.render(
      createElement(ApprovalCard, {
        approval,
        now: T0 + 60_000,
        askerNames: new Map([
          ["eng", "Engineer"],
          ["ops", "Ops"],
        ]),
        deciding: null,
        batchIndex: 1,
        batchTotal: 1,
        onDecide: (
          verdict: Verdict,
          scope: GrantScope,
          blocker?: { verdict: BlockerVerdict; answer?: string },
        ) => {
          decisions.push({ verdict, scope, blocker });
        },
      }),
    );
  });
}

/** The decide footer's buttons, by their visible text. */
function buttons(): HTMLButtonElement[] {
  const footer = container.querySelector('[data-testid="approval-decide"]');
  return [...(footer?.querySelectorAll("button") ?? [])] as HTMLButtonElement[];
}

function button(label: string): HTMLButtonElement {
  const found = buttons().find((b) => b.textContent?.trim().startsWith(label));
  if (!found) {
    throw new Error(
      `no control labelled ${label}; found ${buttons()
        .map((b) => JSON.stringify(b.textContent?.trim()))
        .join(", ")}`,
    );
  }
  return found;
}

async function click(el: HTMLElement) {
  await act(async () => {
    el.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT =
    true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("a blocker card offers four verdicts, not two", () => {
  it("renders retry, answer, skip and cancel — and no Approve or Decline", async () => {
    await render(BLOCKER);
    const labels = buttons().map((b) => b.textContent?.trim() ?? "");
    for (const verdict of BLOCKER_VERDICTS) {
      const label = blockerVerdictLabel(verdict).replace("…", "");
      expect(
        labels.some((l) => l.startsWith(label)),
        `${verdict} must be reachable; footer had ${JSON.stringify(labels)}`,
      ).toBe(true);
    }
    expect(labels.some((l) => l === "Approve" || l === "Decline")).toBe(false);
  });

  it("leaves an ordinary approval on Decline and Approve", async () => {
    await render(GATED_CALL);
    const labels = buttons().map((b) => b.textContent?.trim() ?? "");
    expect(labels).toContain("Decline");
    expect(labels).toContain("Approve");
    expect(container.querySelector('[data-testid="blocker-decide"]')).toBeNull();
  });

  it("sends each wordless verdict with the approve or deny the host pairs it with", async () => {
    for (const verdict of ["retry", "skip", "cancel"] as BlockerVerdict[]) {
      await render(BLOCKER);
      await click(button(blockerVerdictLabel(verdict)));
      expect(decisions).toHaveLength(1);
      expect(decisions[0].blocker?.verdict).toBe(verdict);
      // The pairing rule the host validates, asserted here so the console
      // cannot start sending a pair the host refuses.
      expect(decisions[0].verdict).toBe(blockerEventVerdict(verdict));
      // A blocker buys no standing permission — the host refuses that pairing.
      expect(decisions[0].scope).toEqual({ kind: "once" });
      // …and carries no words, which the host also refuses on these three.
      expect(decisions[0].blocker?.answer).toBeUndefined();
    }
  });

  it("keeps Send inert until the answer has words, then carries them verbatim", async () => {
    await render(BLOCKER);
    await click(button(blockerVerdictLabel("amend")));
    const send = button("Send answer");
    expect(send.disabled, "an empty amend must not be sendable").toBe(true);

    const field = container.querySelector("textarea") as HTMLTextAreaElement;
    // Whitespace is not an answer: the host refuses it rather than downgrading
    // it to a retry, so the control must refuse it first.
    await act(async () => {
      setValue(field, "   \n\t ");
    });
    expect(button("Send answer").disabled).toBe(true);

    await act(async () => {
      setValue(field, "use gpt-4o-mini instead");
    });
    await click(button("Send answer"));
    expect(decisions).toEqual([
      {
        verdict: "approve",
        scope: { kind: "once" },
        blocker: { verdict: "amend", answer: "use gpt-4o-mini instead" },
      },
    ]);
  });

  it("separates skip from cancel by words and by consequence, not by position", async () => {
    // BLOCKER carries no `blocker_step_kind` — an old host, or a blocker with
    // no step behind it — so this exercises the generic fallback wording, not
    // either step-specific pair (see the "worded by which step" describe
    // block below for those).
    await render(BLOCKER);
    const skip = blockerVerdictLabel("skip");
    const cancel = blockerVerdictLabel("cancel");
    expect(skip).not.toBe(cancel);
    // The two opposite outcomes are one click apart, so neither may rest on its
    // label: each says what happens, and they say different things.
    const skipLine = blockerVerdictConsequence("skip");
    const cancelLine = blockerVerdictConsequence("cancel");
    expect(skipLine).not.toBe(cancelLine);
    // Different weight as well as different words.
    expect(button(cancel).className).not.toBe(button(skip).className);
    // …and both sentences are on the card, not only in a tooltip.
    expect(container.textContent).toContain(skipLine);
    expect(container.textContent).toContain(cancelLine);
  });
});

describe("a blocker's consequence is worded by which step it stopped", () => {
  it("gives retry, skip and cancel a different sentence for a card than for a node", () => {
    for (const verdict of ["retry", "skip", "cancel"] as BlockerVerdict[]) {
      const task = blockerVerdictConsequence(verdict, "task");
      const node = blockerVerdictConsequence(verdict, "node");
      expect(
        task,
        `${verdict}'s task wording must not equal its node wording: both read "${task}"`,
      ).not.toBe(node);
    }
  });

  it("describes the task skip landing without another run", () => {
    const skip = blockerVerdictConsequence("skip", "task");
    const cancel = blockerVerdictConsequence("cancel", "task");
    expect(skip).toMatch(/in review/i);
    expect(skip).toMatch(/without running it again/i);
    expect(skip).toMatch(/no output is produced/i);
    expect(cancel).not.toMatch(/stops the run/i);
    expect(cancel).toMatch(/to-do/i);
  });

  it("falls back to wording no step kind contradicts when the step is unknown", () => {
    const generic = {
      retry: blockerVerdictConsequence("retry"),
      skip: blockerVerdictConsequence("skip"),
      cancel: blockerVerdictConsequence("cancel"),
    };
    // Neither the node's specific claims ("produces nothing", "stops the
    // run") nor the card's ("back in progress", "To-do") belong here — an
    // unknown step must not borrow either path's promise.
    expect(generic.skip).not.toMatch(/produces nothing|back in progress/i);
    expect(generic.cancel).not.toMatch(/stops the run|to-do/i);
  });

  it("renders the task-specific sentence on a task-backed card, not the node's", async () => {
    await render({ ...BLOCKER, blocker_step_kind: "task" });
    const skipLine = blockerVerdictConsequence("skip", "task");
    const nodeSkipLine = blockerVerdictConsequence("skip", "node");
    expect(container.textContent).toContain(skipLine);
    expect(container.textContent).not.toContain(nodeSkipLine);
  });

  it("renders the node-specific sentence on a node-backed card, not the task's", async () => {
    await render({ ...BLOCKER, blocker_step_kind: "node" });
    const skipLine = blockerVerdictConsequence("skip", "node");
    const taskSkipLine = blockerVerdictConsequence("skip", "task");
    expect(container.textContent).toContain(skipLine);
    expect(container.textContent).not.toContain(taskSkipLine);
  });

  it("sends the same skip wire verdict regardless of which sentence the operator read", async () => {
    for (const stepKind of ["task", "node", undefined] as const) {
      await render({ ...BLOCKER, blocker_step_kind: stepKind });
      await click(button(blockerVerdictLabel("skip")));
      expect(decisions).toHaveLength(1);
      expect(decisions[0].blocker?.verdict).toBe("skip");
      expect(decisions[0].verdict).toBe("approve");
    }
  });
});

/** React's controlled inputs ignore a plain `.value =` assignment. */
function setValue(el: HTMLTextAreaElement, value: string) {
  const setter = Object.getOwnPropertyDescriptor(
    HTMLTextAreaElement.prototype,
    "value",
  )?.set;
  setter?.call(el, value);
  el.dispatchEvent(new Event("input", { bubbles: true }));
}

/**
 * A transport that answers one canned body and records what it was asked.
 *
 * `/spec` is answered apart from the canned body, and is not recorded: these
 * cases are about the resolve body a *capable* host receives, and the client
 * declines to send `skip` or `amend` to one that does not advertise
 * `blocker-verdict` (see `blocker-verdict-host-capability.test.ts`). Folding
 * the probe into `sent` would also shift the resolve off `sent[0]`.
 */
function client(body: unknown) {
  const sent: { url: string; body: Record<string, unknown> }[] = [];
  const transport: Transport = {
    request: async ({ url, body: raw }: TransportRequest): Promise<TransportResponse> => {
      const spec = url.endsWith("/spec");
      if (!spec) sent.push({ url, body: raw ? JSON.parse(raw) : {} });
      return {
        status: 200,
        statusText: "",
        url,
        text: JSON.stringify(
          spec ? { capabilities: ["rest", "approvals", "blocker-verdict"] } : body,
        ),
        header: () => null,
      };
    },
    subscribe: () => () => {},
    // Test double: an abort stops the caller; there is no real work to cancel.
    cancelsInFlight: true,
  };
  return {
    client: new OpenCompanyClient(
      { baseUrl: "", company: null, operatorToken: null, sessionHeader: null },
      transport,
    ),
    sent,
  };
}

describe("the resolve body a blocker verdict produces", () => {
  it("carries the verdict token, and the answer only on an amend", async () => {
    for (const verdict of BLOCKER_VERDICTS) {
      const { client: c, sent } = client({ recorded: true, alreadyResolved: false });
      await c.resolveApproval("b1", blockerEventVerdict(verdict), undefined, null, {
        blocker: { verdict, answer: verdict === "amend" ? "use gpt-4o-mini" : undefined },
      });
      expect(sent[0].body.blocker_verdict).toBe(verdict);
      expect(sent[0].body.verdict).toBe(blockerEventVerdict(verdict));
      if (verdict === "amend") {
        expect(sent[0].body.blocker_answer).toBe("use gpt-4o-mini");
      } else {
        // The host refuses words on a wordless verdict, so none are sent.
        expect("blocker_answer" in sent[0].body).toBe(false);
      }
    }
  });

  it("sends nothing at all when there is no blocker verdict", async () => {
    const { client: c, sent } = client({ recorded: true, alreadyResolved: false });
    await c.resolveApproval("a1", "approve", undefined, null, {});
    expect(sent[0].body).toEqual({ verdict: "approve" });
  });
});

describe("the confirmation an answered blocker leaves", () => {
  it("says which of the four it was, not just that it was approved", () => {
    const lines = BLOCKER_VERDICTS.map((v) => blockerDecidedLine(v));
    // Three of the four are approves. If the wording came from the verdict the
    // wire carries, three of these would be the same sentence — the flattening
    // this issue is about, restated in the receipt.
    expect(new Set(lines).size).toBe(4);
    for (const verdict of BLOCKER_VERDICTS) {
      expect(blockerDecidedLine(verdict)).toContain(
        blockerVerdictConsequence(verdict).toLowerCase(),
      );
    }
  });

  it("says how many siblings a root-cause group settled with it", () => {
    expect(blockerDecidedLine("skip", "the model id", ["b1"])).not.toContain(
      "other question",
    );
    expect(blockerDecidedLine("skip", "the model id", ["b1", "b2"])).toContain(
      "1 other question",
    );
    expect(
      blockerDecidedLine("skip", "the model id", ["b1", "b2", "b3"]),
    ).toContain("2 other questions");
  });
});
