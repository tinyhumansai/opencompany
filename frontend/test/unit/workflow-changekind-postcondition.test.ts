import { describe, expect, it } from "vitest";

import { changeKind, type DraftNode } from "@/views/WorkflowCreateDialog";

// Codex review on #1937 (issue #1866): the same reset `changeKind` already
// applies to `repeatable` (see workflow-changekind-repeatable.test.ts) must
// also apply to `postcondition` — it is valid on `agent` nodes only, this
// dialog has no control to author or clear it, and switching a node's kind
// away from `agent` while it carries one would leave a value `submit()`
// still sends, which the host refuses on any non-agent kind, with no way
// for the author to see or clear it first.
function agentNode(): DraftNode {
  return {
    key: "k1",
    id: "ask",
    kind: "agent",
    name: "Ask",
    summary: "",
    agent: "ceo",
    schedule: "",
    destinationKind: "",
    destinationTarget: "",
    configDraft: {},
    postcondition: { require: "non_empty" },
  };
}

describe("changeKind — postcondition reset", () => {
  it("clears a declared postcondition when switching away from agent", () => {
    const row = agentNode();
    const next: DraftNode = { ...row, ...changeKind("transform") };
    expect(next.postcondition).toBeUndefined();
  });

  it("clears postcondition even when switching between two agent-shaped drafts", () => {
    // Matches the file's stated convention: reset unconditionally on any
    // kind change, rather than special-casing "still an agent node".
    const row = agentNode();
    const next: DraftNode = { ...row, ...changeKind("agent") };
    expect(next.postcondition).toBeUndefined();
  });
});
