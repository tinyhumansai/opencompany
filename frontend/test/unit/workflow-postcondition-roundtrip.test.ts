import { describe, expect, it } from "vitest";

import type { WorkflowGraph } from "@/api/workflows";
import { assembleGraph, draftNodes, type GraphDraft } from "@/views/WorkflowCreateDialog";

/**
 * Codex review on #1937 (issue #1866): the console's own load → edit → save
 * cycle, not merely `assembleGraph` fed a hand-built draft — this dialog has
 * no control for `postcondition` (same as `onError`/`retry`/
 * `requiresApproval`/`repeatable`), so the only way a Save doesn't clear one
 * is if BOTH halves carry it through untouched: `draftNodes` reading the
 * loaded graph into rows, and `assembleGraph` writing those rows back out.
 * A test that only exercises `assembleGraph` on a hand-built `GraphDraft`
 * (as `workflow-graph-assembly.test.ts`'s `ownerDesk` test does for that
 * field) cannot reproduce the read-side half of this bug — before this fix,
 * `draftNodes` never copied `postcondition` onto the row in the first place,
 * so it would already be gone by the time `assembleGraph` ran.
 */

/** A saved graph with one agent node carrying a declared postcondition —
 * the shape a `GET`/`PUT` response has after the backend fix on this PR. */
function savedGraphWithPostcondition(): WorkflowGraph {
  return {
    id: "greeter",
    name: "Greeter",
    version: "v1",
    nodes: [
      { id: "start", kind: "trigger", name: "Start" },
      {
        id: "ask",
        kind: "agent",
        name: "Ask",
        agent: "ceo",
        postcondition: { require: "field_present", field: "json.items" },
      },
      { id: "done", kind: "output", name: "Report" },
    ],
    edges: [
      { from: "start", to: "ask" },
      { from: "ask", to: "done" },
    ],
  };
}

describe("postcondition survives the console's load -> edit -> save cycle", () => {
  it("carries a declared postcondition through draftNodes into assembleGraph unedited", () => {
    const rows = draftNodes(savedGraphWithPostcondition());
    const ask = rows.find((r) => r.id === "ask");
    expect(ask?.postcondition).toEqual({ require: "field_present", field: "json.items" });

    const draft: GraphDraft = {
      id: "greeter",
      name: "Greeter",
      description: "",
      nodes: rows,
      edges: [
        { key: "e1", from: "start", to: "ask", label: "" },
        { key: "e2", from: "ask", to: "done", label: "" },
      ],
    };
    const out = assembleGraph(draft);
    expect(out.ok).toBe(true);
    if (!out.ok) return;
    const assembled = out.graph.nodes.find((n) => n.id === "ask");
    expect(assembled?.postcondition).toEqual({
      require: "field_present",
      field: "json.items",
    });
  });

  // The real-world scenario the reviewer named: open a workflow that has a
  // postcondition, change something UNRELATED (the workflow's own name, not
  // the gated node at all), hit save. The gate must still be there.
  it("keeps the postcondition after an unrelated edit elsewhere in the graph", () => {
    const rows = draftNodes(savedGraphWithPostcondition());

    const draft: GraphDraft = {
      id: "greeter",
      // The unrelated edit: only the workflow's own display name changes.
      name: "Greeter (renamed)",
      description: "",
      nodes: rows,
      edges: [
        { key: "e1", from: "start", to: "ask", label: "" },
        { key: "e2", from: "ask", to: "done", label: "" },
      ],
    };
    const out = assembleGraph(draft);
    expect(out.ok).toBe(true);
    if (!out.ok) return;
    expect(out.graph.name).toBe("Greeter (renamed)");
    const assembled = out.graph.nodes.find((n) => n.id === "ask");
    expect(assembled?.postcondition).toEqual({
      require: "field_present",
      field: "json.items",
    });
  });

  // Sibling verdict (coordinator's ask): onError/retry are the fields that
  // were erased-but-WARNED on the Rust fix-from-run path. On the console's
  // own round trip, they were never dropped at all -- this dialog already
  // carried them through, same mechanism as `repeatable`/`requiresApproval`.
  // Pinned here so a future edit to draftNodes/assembleGraph cannot regress
  // them silently.
  it("also carries onError and retry through the same round trip (already correct)", () => {
    const graph = savedGraphWithPostcondition();
    graph.nodes[1] = {
      ...graph.nodes[1],
      onError: "route",
      retry: { maxAttempts: 3, backoffMs: 500, backoff: "exponential" },
    };
    const rows = draftNodes(graph);
    const draft: GraphDraft = {
      id: "greeter",
      name: "Greeter",
      description: "",
      nodes: rows,
      edges: [
        { key: "e1", from: "start", to: "ask", label: "" },
        { key: "e2", from: "ask", to: "done", label: "" },
      ],
    };
    const out = assembleGraph(draft);
    expect(out.ok).toBe(true);
    if (!out.ok) return;
    const assembled = out.graph.nodes.find((n) => n.id === "ask");
    expect(assembled?.onError).toBe("route");
    expect(assembled?.retry).toEqual({
      maxAttempts: 3,
      backoffMs: 500,
      backoff: "exponential",
    });
  });
});
