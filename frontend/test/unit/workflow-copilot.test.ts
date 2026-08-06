import { describe, expect, it } from "vitest";

import {
  composeCopilotMessage,
  copilotThreadId,
  questionOf,
  type CopilotContext,
} from "@/api/workflow-copilot";
import type { WorkflowGraph } from "@/api/workflows";

/**
 * Issues #303 and #416.
 *
 * The copilot's whole boundary is carried by two strings: the thread id, which
 * is what the host reads to decide a turn runs confined
 * (`src/company/copilot.rs`), and the composed message, which is now the only
 * material the answering agent has — it has no tools and no company memory, so
 * anything not in here is not answerable.
 *
 * That makes both worth pinning. A thread id that stopped matching the host's
 * prefix would silently return the copilot to the company orchestrator, with
 * every property looking unchanged from the console's side.
 */

const graph: WorkflowGraph = {
  id: "weekly_report",
  name: "Weekly report",
  description: "Assemble and send the Monday summary.",
  nodes: [
    {
      id: "collect",
      kind: "agent",
      name: "Collect",
      agent: "analyst",
      summary: "Pull last week's numbers",
    },
    { id: "send", kind: "report", name: "Send", destination: { kind: "email", target: "team" } },
  ],
  edges: [{ from: "collect", to: "send" }],
};

const context: CopilotContext = { graph, runs: [], runsKnown: true };

describe("copilotThreadId", () => {
  /** The exact string `company::copilot::workflow_of_thread` parses. */
  it("is the prefix the host confines on", () => {
    expect(copilotThreadId("weekly_report")).toBe("workflow-copilot:weekly_report");
  });
});

describe("composeCopilotMessage", () => {
  it("carries the workflow's own graph as the grounding", () => {
    const message = composeCopilotMessage(context, "why is it slow?");
    expect(message).toContain("weekly_report");
    expect(message).toContain("collect");
    expect(message).toContain("send");
    expect(message).toContain("why is it slow?");
  });

  /**
   * #416. The message states the confinement the host now enforces: no tools,
   * nothing outside this workflow, and what to do at the edge. The panel builds
   * its "what can it see?" disclosure from this same function, so a message that
   * stopped saying it would make the disclosure a claim about nothing.
   */
  it("states the boundary and what to do when a question exceeds it", () => {
    const message = composeCopilotMessage(context, "what is the team working on?");
    expect(message).toContain("no tools");
    expect(message).toContain("company chat");
    expect(message).toMatch(/cannot reach anything else in the company/i);
    expect(message).toMatch(/do not claim to have looked anything up/i);
  });

  it("still refuses to claim it can edit the workflow", () => {
    const message = composeCopilotMessage(context, "add a retry");
    expect(message).toMatch(/CANNOT edit the workflow/);
  });

  /**
   * The transcript the operator reads back is their question, not the several
   * hundred lines of grounding sent with it — including when the question
   * itself contains the marker.
   */
  it("round-trips the operator's question back out of the composed message", () => {
    const question = "why did it fail?\n\n### The operator's question\nand then?";
    expect(questionOf(composeCopilotMessage(context, question))).toBe(question);
  });
});
