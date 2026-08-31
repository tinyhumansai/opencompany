import { describe, expect, it } from "vitest";

import type { CompanyStreamEvent } from "@/hooks/use-events";
import {
  foldLiveFrame,
  liveNodes,
  type LiveActivityState,
} from "@/views/workflows/run-live-activity";

/**
 * The live-streaming delta for the run-trace sheet (issue #1702).
 *
 * The fold is where the whole feature's correctness lives: a start and its
 * result are ONE row, a frame for another run is ignored, and a replayed frame
 * never doubles a row. These are pure-reducer facts, tested here without a DOM
 * so the sheet component stays a thin renderer over them.
 */
const RUN = "wfr-1";

function toolCall(
  over: Partial<Extract<CompanyStreamEvent, { type: "tool_call" }>>,
): CompanyStreamEvent {
  return {
    type: "tool_call",
    seq: 0,
    workflowRunId: RUN,
    nodeId: "summarise",
    toolCallId: "c1",
    label: "Search the web",
    status: "running",
    ...over,
  };
}

function toolResult(
  over: Partial<Extract<CompanyStreamEvent, { type: "tool_result" }>>,
): CompanyStreamEvent {
  return {
    type: "tool_result",
    seq: 1,
    workflowRunId: RUN,
    nodeId: "summarise",
    toolCallId: "c1",
    label: "Search the web",
    detail: "brave · search",
    status: "ok",
    elapsedMs: 42,
    ...over,
  };
}

function fold(frames: CompanyStreamEvent[], run = RUN): LiveActivityState {
  return frames.reduce<LiveActivityState>(
    (state, frame) => foldLiveFrame(state, frame, run),
    {},
  );
}

describe("folding a workflow node's live tool frames (issue #1702)", () => {
  it("groups a running tool call under its node", () => {
    const nodes = liveNodes(fold([toolCall({})]));
    expect(nodes).toHaveLength(1);
    expect(nodes[0].nodeId).toBe("summarise");
    expect(nodes[0].rows).toHaveLength(1);
    expect(nodes[0].rows[0].status).toBe("running");
    expect(nodes[0].rows[0].label).toBe("Search the web");
  });

  it("merges a result onto the row its call created — one row, not two", () => {
    const nodes = liveNodes(fold([toolCall({}), toolResult({})]));
    expect(nodes[0].rows).toHaveLength(1);
    const row = nodes[0].rows[0];
    expect(row.status).toBe("ok");
    expect(row.detail).toBe("brave · search");
    expect(row.elapsedMs).toBe(42);
    // The row keeps the start's seq so a later result cannot reorder it.
    expect(row.seq).toBe(0);
  });

  it("keeps the result summary, which is all an ACP node reports", () => {
    // The chat timeline carried `result` and this reducer dropped it — fine
    // while only the built-in harness streamed, since its rows lean on the
    // argument-derived `detail`. An ACP tool call puts no arguments on the
    // wire, so `result` is the whole of what its finished row can say
    // (PR #1904 review).
    const nodes = liveNodes(
      fold([
        toolCall({}),
        toolResult({ detail: undefined, result: "42 lines" }),
      ]),
    );
    expect(nodes[0].rows).toHaveLength(1);
    expect(nodes[0].rows[0].result).toBe("42 lines");
    expect(nodes[0].rows[0].status).toBe("ok");
  });

  it("ignores a frame that belongs to a different run", () => {
    const nodes = liveNodes(
      fold([toolCall({ workflowRunId: "some-other-run" })]),
    );
    expect(nodes).toHaveLength(0);
  });

  it("does not double a row when a frame is replayed", () => {
    // A reconnect re-delivers the same start; the keyed fold must rewrite the
    // one row rather than append a second.
    const nodes = liveNodes(fold([toolCall({}), toolCall({}), toolResult({})]));
    expect(nodes[0].rows).toHaveLength(1);
    expect(nodes[0].rows[0].status).toBe("ok");
  });

  it("keeps rows from different nodes apart, ordered by when each started", () => {
    const nodes = liveNodes(
      fold([
        toolCall({ nodeId: "fetch", toolCallId: "a", seq: 0 }),
        toolCall({ nodeId: "summarise", toolCallId: "b", seq: 1 }),
      ]),
    );
    expect(nodes.map((n) => n.nodeId)).toEqual(["fetch", "summarise"]);
  });

  it("does not let an out-of-order start blank a value the result already wrote", () => {
    // If the start arrives after its result (broadcast reordering), the merge
    // must not clobber the result's detail/elapsed/status with the start's
    // absence — the call completed, and it started where its own start says.
    const nodes = liveNodes(fold([toolResult({}), toolCall({})]));
    const row = nodes[0].rows[0];
    expect(row.detail).toBe("brave · search");
    expect(row.elapsedMs).toBe(42);
    expect(row.status).toBe("ok");
    // The late start still anchors the row to the call's own start sequence,
    // rather than inheriting the later result's.
    expect(row.seq).toBe(0);
  });
});
