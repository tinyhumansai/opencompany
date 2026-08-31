import { describe, expect, it } from "vitest";

import type { WorkflowGraph } from "@/api/workflows";
import { backEdges, contentBounds, layout } from "@/views/workflows/graph";

/**
 * Layering is a longest-path relaxation, and a longest path is only defined on a
 * DAG. A workflow with a revision loop is not one — and revision loops are not
 * exotic, they are how any review lifecycle is drawn:
 *
 *   plan rejected      -> re-plan
 *   PR needs changes   -> re-review
 *
 * Before `backEdges`, each pass around such a loop raised every node in it by
 * one. The `!changed` break therefore never fired, the relaxation ran its full
 * `nodes.length` iterations, and what it left behind was the iteration count
 * rather than the graph's shape.
 *
 * Measured in the console against a real 11-node issue-to-PR lifecycle with two
 * revision loops, before this fix:
 *
 *   trigger      depth  0  →  x     0
 *   review_plan  depth 32  →  x  9600
 *   notify_done  depth 39  →  x 11700
 *   content width          →   11890 units (a 10-node DAG is 2890)
 *
 * On screen: the trigger alone on the left, the other ten past the right edge,
 * and nothing anywhere to say the layout had given up. That silence is why the
 * assertions below are on the *numbers* and not merely on "it terminated".
 */

/** The real graph the bug was found on, node-for-node. */
function issueToPr(): WorkflowGraph {
  const ids = [
    "issue_arrives",
    "read_and_plan",
    "review_plan",
    "plan_approved",
    "implement",
    "open_pr",
    "qa_review",
    "pr_approved",
    "revise_pr",
    "merge_pr",
    "notify_done",
  ];
  return {
    id: "issue-to-pr",
    name: "Issue to PR lifecycle",
    nodes: ids.map((id) => ({ id, kind: "agent", name: id })),
    edges: [
      { from: "issue_arrives", to: "read_and_plan" },
      { from: "read_and_plan", to: "review_plan" },
      { from: "review_plan", to: "plan_approved" },
      { from: "plan_approved", to: "implement" },
      // The first revision loop: a rejected plan goes back for another pass.
      { from: "plan_approved", to: "read_and_plan" },
      { from: "implement", to: "open_pr" },
      { from: "open_pr", to: "qa_review" },
      { from: "qa_review", to: "pr_approved" },
      { from: "pr_approved", to: "merge_pr" },
      { from: "pr_approved", to: "revise_pr" },
      // The second: a PR that needs changes goes back to review.
      { from: "revise_pr", to: "qa_review" },
      { from: "merge_pr", to: "notify_done" },
    ],
  } as WorkflowGraph;
}

const xOf = (nodes: ReturnType<typeof layout>["nodes"], id: string) =>
  nodes.find((n) => n.id === id)!.position.x;

describe("cyclic workflow layout", () => {
  it("breaks the loop's return edge, not an edge on the way in", () => {
    const broken = [...backEdges(issueToPr())].map((e) => `${e.from}->${e.to}`).sort();
    // Exactly the two returns. Breaking `read_and_plan->review_plan` instead
    // would also terminate, and would cut the pipeline at an arbitrary point.
    expect(broken).toEqual(["plan_approved->read_and_plan", "revise_pr->qa_review"]);
  });

  it("lays the lifecycle out as a pipeline rather than a 12k-unit smear", () => {
    const { nodes } = layout(issueToPr());

    expect(xOf(nodes, "issue_arrives")).toBe(0);
    expect(xOf(nodes, "read_and_plan")).toBe(300);
    expect(xOf(nodes, "notify_done")).toBe(2700);

    // The regression in one number: 2890, not 11890.
    expect(contentBounds(nodes).width).toBe(2890);
  });

  it("keeps the first node adjacent to the second", () => {
    const { nodes } = layout(issueToPr());
    // The reported symptom was "the first node is too far away from the rest".
    // One column, never thirty-two.
    expect(xOf(nodes, "read_and_plan") - xOf(nodes, "issue_arrives")).toBe(300);
  });

  it("still draws the loops it excluded from layering", () => {
    const { edges } = layout(issueToPr());
    // Excluded from the arithmetic, not from the canvas.
    expect(edges).toHaveLength(12);
    expect(edges.some((e) => e.source === "plan_approved" && e.target === "read_and_plan")).toBe(
      true,
    );
  });

  it("gives siblings the same layer", () => {
    const { nodes } = layout(issueToPr());
    // `pr_approved` fans out to both, so they sit in one column with the row
    // offset doing the separating.
    expect(xOf(nodes, "revise_pr")).toBe(xOf(nodes, "merge_pr"));
  });

  it("breaks the authored return when a branch enters the loop's middle", () => {
    // `agentic_math_lab/euler_solve`, shipped. `cost` fans out to BOTH `solve`
    // and `approach`, so a DFS reaches `solve` first, descends to `agree`, and
    // finds `approach -> solve` closing onto the stack — an ordinary forward
    // edge. Breaking that one lays `approach` out AFTER `agree`, so the normal
    // approach-to-solve path runs backwards and the retry branch points forward.
    // The authored return is `agree -> approach`, and nothing about traversal
    // order can tell you that.
    const euler: WorkflowGraph = {
      id: "euler_solve",
      name: "euler_solve",
      nodes: [
        "problem",
        "restate",
        "cost",
        "approach",
        "solve",
        "check",
        "agree",
        "record",
        "report",
      ].map((id) => ({ id, kind: "agent", name: id })),
      edges: [
        { from: "problem", to: "restate" },
        { from: "restate", to: "cost" },
        { from: "cost", to: "solve" },
        { from: "cost", to: "approach" },
        { from: "approach", to: "solve" },
        { from: "solve", to: "check" },
        { from: "check", to: "agree" },
        { from: "agree", to: "approach" },
        { from: "agree", to: "record" },
        { from: "record", to: "report" },
      ],
    } as WorkflowGraph;

    expect([...backEdges(euler)].map((e) => `${e.from}->${e.to}`)).toEqual(["agree->approach"]);

    const { nodes } = layout(euler);
    // The authored path reads left to right: cost, then approach, then solve.
    expect(xOf(nodes, "cost")).toBeLessThan(xOf(nodes, "approach"));
    expect(xOf(nodes, "approach")).toBeLessThan(xOf(nodes, "solve"));
    // And the retry target sits before the node that retries it.
    expect(xOf(nodes, "approach")).toBeLessThan(xOf(nodes, "agree"));
  });

  it("does not mistake a converging path for a loop", () => {
    // `a` reaches `d` two ways. Nothing here is cyclic, so nothing may be
    // broken — the condition is reachability back to the source, not "I have
    // seen this node before".
    const diamond: WorkflowGraph = {
      id: "diamond",
      name: "diamond",
      nodes: ["a", "b", "c", "d"].map((id) => ({ id, kind: "agent", name: id })),
      edges: [
        { from: "a", to: "b" },
        { from: "a", to: "c" },
        { from: "b", to: "d" },
        { from: "c", to: "d" },
      ],
    } as WorkflowGraph;
    expect(backEdges(diamond).size).toBe(0);
  });

  it("breaks a cycle even when no edge runs against the authored order", () => {
    // Nodes declared out of flow order, so there is no authored return to find.
    // Termination must not depend on the author having been tidy.
    const scrambled: WorkflowGraph = {
      id: "scrambled",
      name: "scrambled",
      nodes: ["c", "a", "b"].map((id) => ({ id, kind: "agent", name: id })),
      edges: [
        { from: "a", to: "b" },
        { from: "b", to: "c" },
        { from: "c", to: "a" },
      ],
    } as WorkflowGraph;
    expect(backEdges(scrambled).size).toBe(1);
    expect(contentBounds(layout(scrambled).nodes).width).toBe(790);
  });

  it("treats a self-loop as its own return", () => {
    const selfish: WorkflowGraph = {
      id: "selfish",
      name: "selfish",
      nodes: ["a", "b"].map((id) => ({ id, kind: "agent", name: id })),
      edges: [
        { from: "a", to: "b" },
        { from: "b", to: "b" },
      ],
    } as WorkflowGraph;
    expect([...backEdges(selfish)].map((e) => `${e.from}->${e.to}`)).toEqual(["b->b"]);
    expect(layout(selfish).nodes.map((n) => n.position.x)).toEqual([0, 300]);
  });

  it("layers a graph that is entirely a cycle, with no root to start from", () => {
    const ring: WorkflowGraph = {
      id: "ring",
      name: "ring",
      nodes: ["a", "b", "c"].map((id) => ({ id, kind: "agent", name: id })),
      edges: [
        { from: "a", to: "b" },
        { from: "b", to: "c" },
        { from: "c", to: "a" },
      ],
    } as WorkflowGraph;
    const { nodes } = layout(ring);
    // No node has in-degree zero, so the root pass finds nothing and the
    // fall-through pass is the only thing that layers this at all.
    expect(contentBounds(nodes).width).toBe(790);
    expect(nodes.map((n) => n.position.x)).toEqual([0, 300, 600]);
  });

  it("leaves an acyclic graph exactly as it laid out before", () => {
    const chain: WorkflowGraph = {
      id: "chain",
      name: "chain",
      nodes: ["a", "b", "c"].map((id) => ({ id, kind: "agent", name: id })),
      edges: [
        { from: "a", to: "b" },
        { from: "b", to: "c" },
      ],
    } as WorkflowGraph;
    expect(backEdges(chain).size).toBe(0);
    expect(layout(chain).nodes.map((n) => n.position.x)).toEqual([0, 300, 600]);
  });
});
