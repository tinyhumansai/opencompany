// @vitest-environment jsdom
//
// Issue #1776: the copilot converses, and the operator decides.
//
// These pin the property the whole feature rests on and that nothing else can
// check — a draft reaches the form only through Use it, and reaches storage
// only through a Save pressed afterwards — plus the two things that made it a
// conversation rather than a Draft button: the transcript goes back with every
// turn, and a turn is allowed to ask instead of drafting.

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { refusalNotice, type CopilotTurn, type ProfileDraft } from "@/api/agent-copilot";
import { AgentFields } from "@/views/team/AgentFields";
import { FieldCopilot } from "@/views/team/FieldCopilot";
import { emptyDraft } from "@/lib/agent";

let container: HTMLDivElement;
let root: Root;

function render(element: ReturnType<typeof createElement>) {
  act(() => root.render(element));
}

function testid(id: string): HTMLElement | null {
  return container.querySelector(`[data-testid="${id}"]`);
}

function click(id: string) {
  const el = testid(id);
  if (!el) throw new Error(`no element with data-testid=${id}`);
  act(() => {
    el.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
}

/** Types into the composer the way React sees it. */
function say(field: string, text: string) {
  const el = testid(`agent-copilot-input-${field}`) as HTMLTextAreaElement;
  const setter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")!.set!;
  act(() => {
    setter.call(el, text);
    el.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

/** Lets a pending turn settle inside `act`, so React has applied its state. */
async function settle() {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  });
}

const FIRST = "Test features and block releases on open P1s.";
const SECOND = "Test features. Block releases on open P1s.";

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("the teammate copilot converses; the operator keeps or discards", () => {
  /// Opening asks for nothing. An earlier version drafted on this click, which
  /// spent a model call — and the operator's first seconds — on a guess made
  /// before they had said the one thing they opened the copilot to say.
  it("opens on an empty composer and asks for nothing", async () => {
    const asked: CopilotTurn[][] = [];
    render(
      createElement(FieldCopilot, {
        field: "instructions",
        onTurn: async (conversation: CopilotTurn[]): Promise<ProfileDraft> => {
          asked.push(conversation);
          return { field: "instructions", reply: "Here's a first pass.", text: FIRST, source: "model" };
        },
        onAccept: () => {},
      }),
    );

    click("agent-copilot-open-instructions");
    await settle();

    expect(asked).toEqual([]);
    expect(testid("agent-copilot-suggestion-instructions")).toBeNull();
    expect(testid("agent-copilot-empty-instructions")).not.toBeNull();
  });

  it("drafts from what the operator asked for, beside the field and not in it", async () => {
    const accepted: string[] = [];
    const asked: CopilotTurn[][] = [];
    render(
      createElement(FieldCopilot, {
        field: "instructions",
        onTurn: async (conversation: CopilotTurn[]): Promise<ProfileDraft> => {
          asked.push(conversation);
          return { field: "instructions", reply: "Here's a first pass.", text: FIRST, source: "model" };
        },
        onAccept: (text: string) => accepted.push(text),
      }),
    );

    click("agent-copilot-open-instructions");
    say("instructions", "they own release sign-off");
    click("agent-copilot-send-instructions");
    await settle();

    expect(asked).toEqual([[{ role: "operator", text: "they own release sign-off" }]]);
    expect(testid("agent-copilot-suggestion-instructions")?.textContent).toContain(FIRST);
    expect(container.textContent).toContain("Here's a first pass.");
    expect(accepted).toEqual([]);
  });

  /// The whole reason this stopped being a Draft button: "shorter" has to mean
  /// shorter than the last version, so the transcript — including the copilot's
  /// own draft — goes back every turn.
  it("carries the whole transcript, drafts included, into the next turn", async () => {
    const asked: CopilotTurn[][] = [];
    let call = 0;
    render(
      createElement(FieldCopilot, {
        field: "instructions",
        onTurn: async (conversation: CopilotTurn[]): Promise<ProfileDraft> => {
          asked.push(conversation);
          call += 1;
          return {
            field: "instructions",
            reply: call === 1 ? "Here's a first pass." : "Split it into two sentences.",
            text: call === 1 ? FIRST : SECOND,
            source: "model",
          };
        },
        onAccept: () => {},
      }),
    );

    click("agent-copilot-open-instructions");
    say("instructions", "they own release sign-off");
    click("agent-copilot-send-instructions");
    await settle();
    say("instructions", "shorter sentences");
    click("agent-copilot-send-instructions");
    await settle();

    expect(asked).toHaveLength(2);
    const second = asked[1];
    // What the operator asked for, what came back, and the correction — in
    // order. The copilot's turn carries its reply AND its draft: iterating on a
    // description of a draft is not iterating on the draft.
    expect(second).toHaveLength(3);
    expect(second[0]).toEqual({ role: "operator", text: "they own release sign-off" });
    expect(second[1].role).toBe("copilot");
    expect(second[1].text).toContain("Here's a first pass.");
    expect(second[1].text).toContain(FIRST);
    expect(second[2]).toEqual({ role: "operator", text: "shorter sentences" });

    // Every drafted turn keeps its card, so an operator can go back to a
    // version they preferred rather than asking for it again. The newest is the
    // last one.
    const cards = container.querySelectorAll(
      '[data-testid="agent-copilot-suggestion-instructions"]',
    );
    expect(cards).toHaveLength(2);
    expect(cards[0].textContent).toContain(FIRST);
    expect(cards[1].textContent).toContain(SECOND);
  });

  /// A refinement that quietly halves the draft is the same class of failure as
  /// one that drops it: work the operator had already accepted, gone, with
  /// nothing on screen saying so. The prompt tells the copilot to keep what it
  /// wrote; this is what catches the times it does not.
  it("says so when a new draft is much shorter than the one before it", async () => {
    const LONG = "A".repeat(2000);
    const SHORT = "B".repeat(400);
    let call = 0;
    render(
      createElement(FieldCopilot, {
        field: "instructions",
        onTurn: async () => {
          call += 1;
          return {
            field: "instructions" as const,
            reply: call === 1 ? "Here you go." : "Added the escalation section.",
            text: call === 1 ? LONG : SHORT,
            source: "model" as const,
          };
        },
        onAccept: () => {},
      }),
    );

    click("agent-copilot-open-instructions");
    say("instructions", "a thorough operating manual");
    click("agent-copilot-send-instructions");
    await settle();
    expect(testid("agent-copilot-shrank-instructions")).toBeNull();

    say("instructions", "add a section on escalation and keep everything else");
    click("agent-copilot-send-instructions");
    await settle();

    const notice = testid("agent-copilot-shrank-instructions");
    expect(notice?.textContent).toContain("80% shorter");
    // The version it replaced is still on screen with its own Use it, which is
    // what makes the notice actionable rather than merely alarming.
    expect(
      container.querySelectorAll('[data-testid="agent-copilot-accept-instructions"]'),
    ).toHaveLength(2);
  });

  /// A refinement that tightens wording is not a defect, and a notice that
  /// fires on every turn is one nobody reads.
  it("stays quiet when a redraft is a normal length", async () => {
    let call = 0;
    render(
      createElement(FieldCopilot, {
        field: "instructions",
        onTurn: async () => {
          call += 1;
          return {
            field: "instructions" as const,
            reply: "Done.",
            text: call === 1 ? "A".repeat(1000) : "B".repeat(900),
            source: "model" as const,
          };
        },
        onAccept: () => {},
      }),
    );

    click("agent-copilot-open-instructions");
    say("instructions", "an operating manual");
    click("agent-copilot-send-instructions");
    await settle();
    say("instructions", "tighten the wording");
    click("agent-copilot-send-instructions");
    await settle();

    expect(testid("agent-copilot-shrank-instructions")).toBeNull();
  });

  it("fills the field only on Use it, and saves nothing", async () => {
    const accepted: string[] = [];
    render(
      createElement(FieldCopilot, {
        field: "instructions",
        onTurn: async () => ({
          field: "instructions" as const,
          reply: "Here you go.",
          text: FIRST,
          source: "model" as const,
        }),
        onAccept: (text: string) => accepted.push(text),
      }),
    );

    click("agent-copilot-open-instructions");
    say("instructions", "they own release sign-off");
    click("agent-copilot-send-instructions");
    await settle();
    expect(accepted).toEqual([]);

    click("agent-copilot-accept-instructions");
    expect(accepted).toEqual([FIRST]);
    // Accepting closes the conversation — the draft is in the box now, and the
    // box is where editing belongs.
    expect(testid("agent-copilot-instructions")).toBeNull();
  });

  /// A turn may ASK. This is the thing a one-shot pass structurally could not
  /// do, and the reason it never found out what the operator meant.
  it("lets a turn ask a question and offer nothing to accept", async () => {
    render(
      createElement(FieldCopilot, {
        field: "description",
        onTurn: async () => ({
          field: "description" as const,
          reply: "Do they own returns as well, or just outbound?",
          source: "model" as const,
        }),
        onAccept: () => {},
      }),
    );

    click("agent-copilot-open-description");
    say("description", "they handle dispatch");
    click("agent-copilot-send-description");
    await settle();

    expect(container.textContent).toContain("Do they own returns as well");
    expect(testid("agent-copilot-suggestion-description")).toBeNull();
    expect(testid("agent-copilot-accept-description")).toBeNull();
    // …and it is not mistaken for a failure.
    expect(testid("agent-copilot-notice-description")).toBeNull();
  });

  /// A refusal names which of the three happened, so the sentence can name the
  /// operator's next move. Rendering it like a turn would be worse than useless.
  it("says why there is no answer at all", async () => {
    render(
      createElement(FieldCopilot, {
        field: "description",
        onTurn: async () => ({
          field: "description" as const,
          source: "unavailable" as const,
          reason: "no_model" as const,
        }),
        onAccept: () => {},
      }),
    );

    click("agent-copilot-open-description");
    say("description", "they handle dispatch");
    click("agent-copilot-send-description");
    await settle();

    expect(testid("agent-copilot-notice-description")?.textContent).toContain(
      "no model configured",
    );
    expect(testid("agent-copilot-suggestion-description")).toBeNull();
  });

  it("does not offer the copilot when there is nothing to draft with", () => {
    render(
      createElement(FieldCopilot, {
        field: "description",
        onTurn: async () => ({ field: "description" as const, source: "model" as const }),
        onAccept: () => {},
        disabled: true,
        disabledNotice: "No model is configured, so the copilot can't draft yet.",
      }),
    );

    const open = testid("agent-copilot-open-description") as HTMLButtonElement | null;
    expect(open?.disabled).toBe(true);
    expect(container.textContent).toContain("No model is configured");
  });

  /// Both forms refuse to draft from a blank role, and for the same reason:
  /// the briefs are written FROM it. The wire drops a blank role rather than
  /// sending it, so without this gate the host falls back to the STORED role
  /// and drafts for the job the operator is mid-way through moving off.
  it("will not draft while the role is blank", () => {
    render(
      createElement(FieldCopilot, {
        field: "description",
        onTurn: async () => ({ field: "description" as const, source: "model" as const }),
        onAccept: () => {},
        disabled: true,
        disabledNotice: "Give this teammate a role first — the copilot drafts from it.",
      }),
    );

    const open = testid("agent-copilot-open-description") as HTMLButtonElement | null;
    expect(open?.disabled).toBe(true);
    expect(container.textContent).toContain("Give this teammate a role first");
  });

  it("names a different next move for each reason", () => {
    expect(refusalNotice("no_model")).toContain("Settings → Inference");
    expect(refusalNotice("model_unreachable")).toContain("Try again");
    expect(refusalNotice("unreadable")).toContain("add a note");
    expect(refusalNotice(undefined)).not.toContain("Settings → Inference");
  });

  /// A spent budget is the one reason with nothing to retry — the ceiling is a
  /// plan setting, not a provider having a bad minute, so telling the operator
  /// to try again would be advice that cannot work.
  it("does not offer a retry for a budget that is spent", () => {
    const spent = refusalNotice("budget_exhausted");
    expect(spent).toContain("token budget");
    expect(spent).not.toContain("Try again");
  });
});

describe("where the copilot is offered at all", () => {
  const draft = emptyDraft();

  it("appears under the prose fields and nowhere else", () => {
    render(
      createElement(AgentFields, {
        idPrefix: "t",
        draft,
        onChange: () => {},
        copilot: (key: string) =>
          createElement("span", { "data-testid": `slot-${key}` }, key),
      }),
    );

    // A mandate and a persona are prose an operator wants help with.
    expect(testid("slot-description")).not.toBeNull();
    expect(testid("slot-instructions")).not.toBeNull();
    // A name is two words, and a ROLE is what delegation grounds on — drafting
    // one would change who the company routes work to.
    expect(testid("slot-name")).toBeNull();
    expect(testid("slot-role")).toBeNull();
  });

  it("is not offered on a field this host will not let you save", () => {
    render(
      createElement(AgentFields, {
        idPrefix: "t",
        draft,
        onChange: () => {},
        readOnly: (key: string) => key === "instructions",
        copilot: (key: string) =>
          createElement("span", { "data-testid": `slot-${key}` }, key),
      }),
    );

    expect(testid("slot-description")).not.toBeNull();
    expect(testid("slot-instructions")).toBeNull();
  });
});
