import { describe, expect, it } from "vitest";

import type { AgentDetailDto } from "@/api/types";
import { agentEdits, draftFrom, isEditable } from "@/lib/agent";

/**
 * The persona-instructions half of an agent edit (issue #1530).
 *
 * These are the wire-shape claims that must hold whatever the detail view does
 * on screen: an unchanged persona is never sent, a changed one is, an emptied
 * one becomes `null` (the reset-to-blueprint signal, not an empty persona), and
 * a field the host does not mark editable is never emitted however the draft is
 * poked. Getting any of these wrong resets or blanks a persona on a save that
 * only meant to touch something else.
 */

function detail(over: Partial<AgentDetailDto> = {}): AgentDetailDto {
  return {
    id: "agent-1",
    role: "Growth Marketer",
    source: "overlay",
    editable: ["name", "role", "description", "tools", "instructions"],
    isOrchestrator: false,
    tools: { requested: [], companyAllow: [], deskAllow: [], deskCeilingActive: false, effective: [] },
    desks: [],
    inboxEnabled: false,
    name: "Nova",
    description: "Runs paid acquisition.",
    instructions: "Always confirm the budget before launching.",
    ...over,
  };
}

describe("agentEdits — instructions", () => {
  it("emits nothing when the instructions are untouched", () => {
    const d = detail();
    expect(agentEdits(d, draftFrom(d))).toBeNull();
  });

  it("emits only instructions when only the instructions changed", () => {
    const d = detail();
    const draft = { ...draftFrom(d), instructions: "Report ROAS every Friday." };
    expect(agentEdits(d, draft)).toEqual({ instructions: "Report ROAS every Friday." });
  });

  it("emits instructions: null when the field is cleared (reset to blueprint)", () => {
    const d = detail();
    const draft = { ...draftFrom(d), instructions: "   " };
    expect(agentEdits(d, draft)).toEqual({ instructions: null });
  });

  it("never emits instructions the host did not mark editable", () => {
    // A manifest teammate whose host somehow omitted instructions from editable:
    // the draft can hold a different value, but it must not reach the wire.
    const d = detail({ source: "manifest", editable: ["description"] });
    const draft = { ...draftFrom(d), instructions: "sneaky override" };
    expect(agentEdits(d, draft)).toBeNull();
  });

  it("emits instructions for a manifest agent that lists only instructions", () => {
    // The #1530 case: a blueprint agent is now editable in exactly one field,
    // and a persona change on it writes to the override, not the manifest.
    const d = detail({ source: "manifest", name: undefined, editable: ["instructions"] });
    const draft = { ...draftFrom(d), instructions: "Tone: terse. Escalate blockers same day." };
    expect(agentEdits(d, draft)).toEqual({
      instructions: "Tone: terse. Escalate blockers same day.",
    });
  });
});

describe("isEditable — instructions", () => {
  it("is true when the host lists instructions", () => {
    expect(isEditable(detail({ editable: ["instructions"] }), "instructions")).toBe(true);
  });

  it("is false when the host omits instructions", () => {
    expect(isEditable(detail({ editable: ["description"] }), "instructions")).toBe(false);
  });
});
