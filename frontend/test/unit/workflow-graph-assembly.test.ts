import { describe, expect, it } from "vitest";

import {
  assembleGraph,
  preflightIsCurrent,
  type GraphDraft,
  type Preflight,
} from "@/views/WorkflowCreateDialog";

/**
 * The graph the dialog sends, and whether the host's answer is still about it
 * (issue #1074, review round).
 *
 * `assembleGraph` was extracted out of `submit()` so the pre-flight asks the
 * host about **the same bytes** the submit will post. A second assembly written
 * for the pre-flight would be a mirror — and a pre-flight that validates
 * something other than what is submitted is worse than none, which is the whole
 * subject of the issue. These pin the extraction, and the currency rule that
 * stops a superseded verdict from being shown as a current one.
 */

const node = (over: Partial<GraphDraft["nodes"][number]> = {}) => ({
  key: "k",
  id: "start",
  kind: "trigger",
  name: "Start",
  summary: "",
  agent: "",
  schedule: "",
  destinationKind: "" as const,
  destinationTarget: "",
  configDraft: {},
  ...over,
});

const draft = (over: Partial<GraphDraft> = {}): GraphDraft => ({
  id: "  greeter ",
  name: " Greeter ",
  description: "  ",
  nodes: [node()],
  edges: [],
  ...over,
});

describe("assembleGraph", () => {
  it("trims the identity fields and drops an empty description", () => {
    const out = assembleGraph(draft());
    expect(out.ok).toBe(true);
    if (!out.ok) return;
    expect(out.graph.id).toBe("greeter");
    expect(out.graph.name).toBe("Greeter");
    expect(out.graph.description).toBeUndefined();
    // The conditional-write token is sent separately as `expectedVersion`.
    expect(out.graph.version).toBeNull();
  });

  // Regression, issue #1882 review (found after the backend fix): this dialog
  // has no control that edits `ownerDesk`, so the only way a Save doesn't
  // clear one is if the draft carries it through untouched. The bug this pins
  // was silent because a test that builds `GraphDraft` by hand (as the one
  // above does) can't reproduce it — it has to prove the field survives THIS
  // assembly step, the same one `runWrite` and the pre-flight both call.
  it("carries ownerDesk through unedited, since no control here sets it", () => {
    const out = assembleGraph(draft({ ownerDesk: "engineering" }));
    expect(out.ok).toBe(true);
    if (!out.ok) return;
    expect(out.graph.ownerDesk).toBe("engineering");
  });

  it("assembles no ownerDesk for a draft that never had one (a fresh create)", () => {
    const out = assembleGraph(draft());
    expect(out.ok).toBe(true);
    if (!out.ok) return;
    expect(out.graph.ownerDesk).toBeUndefined();
  });

  // The host rejects a schedule on any non-trigger node and a destination on
  // any non-output node, so the assembly must not send either off-kind — a draft
  // row can still be carrying one after `changeKind`.
  it("sends a schedule only from a trigger and a destination only from an output", () => {
    const out = assembleGraph(
      draft({
        nodes: [
          node({ id: "start", kind: "trigger", schedule: "0 9 * * *" }),
          node({
            id: "worker",
            kind: "agent",
            agent: "assistant",
            schedule: "0 9 * * *",
            destinationKind: "owner",
          }),
          node({ id: "done", kind: "output", destinationKind: "owner" }),
        ],
      }),
    );
    expect(out.ok).toBe(true);
    if (!out.ok) return;
    const [start, worker, done] = out.graph.nodes;
    expect(start.schedule).toBe("0 9 * * *");
    expect(worker.schedule).toBeUndefined();
    expect(worker.destination).toBeUndefined();
    expect(done.destination).toEqual({ kind: "owner", target: undefined });
  });

  // `owner` resolves server-side and the host REJECTS a target on it — and a row
  // can be carrying one, because `changeKind` leaves the target string in place
  // when the destination kind changes. So the owner case must be asserted with a
  // non-empty target, or the rule is untested.
  it("never sends a target for an owner destination, but keeps one for email", () => {
    const out = assembleGraph(
      draft({
        nodes: [
          node({
            id: "owned",
            kind: "output",
            destinationKind: "owner",
            destinationTarget: "left-over@example.com",
          }),
          node({
            id: "done",
            kind: "output",
            destinationKind: "email",
            destinationTarget: "  ops@example.com ",
          }),
        ],
      }),
    );
    expect(out.ok).toBe(true);
    if (!out.ok) return;
    expect(out.graph.nodes[0].destination).toEqual({ kind: "owner", target: undefined });
    expect(out.graph.nodes[1].destination).toEqual({
      kind: "email",
      target: "ops@example.com",
    });
  });

  // An empty label is omitted rather than sent as `""`: the host reads a
  // present-but-blank label differently from an absent one.
  it("trims edges and omits an empty label", () => {
    const out = assembleGraph(
      draft({
        edges: [
          { key: "a", from: " start ", to: " done ", label: "  " },
          { key: "b", from: "gate", to: "done", label: " yes " },
        ],
      }),
    );
    expect(out.ok).toBe(true);
    if (!out.ok) return;
    expect(out.graph.edges).toEqual([
      { from: "start", to: "done", label: undefined },
      { from: "gate", to: "done", label: "yes" },
    ]);
  });

  // An edit must not delete what the form cannot show: a kind with no config
  // form carries its raw overlay straight back out.
  it("passes a form-less kind's raw config through untouched", () => {
    const carried = { connection_ref: "acct-1", nested: { deep: true } };
    const out = assembleGraph(
      draft({ nodes: [node({ id: "x", kind: "merge", config: carried })] }),
    );
    expect(out.ok).toBe(true);
    if (!out.ok) return;
    expect(out.graph.nodes[0].config).toEqual(carried);
  });

  // A form kind whose draft cannot be serialized is reported with the node, not
  // thrown: `submit()` names it and stops, the pre-flight stays quiet.
  it("reports the offending node when a form kind's config will not serialize", () => {
    const out = assembleGraph(
      draft({
        nodes: [
          node({
            id: "call",
            kind: "tool_call",
            configDraft: { slug: "web_search", args: "{ not json" },
          }),
        ],
      }),
    );
    expect(out.ok).toBe(false);
    if (out.ok) return;
    expect(out.node.id).toBe("call");
    expect(out.error).toBeTruthy();
  });
});

describe("preflightIsCurrent", () => {
  const key = '{"id":"greeter"}';

  it("is false before anything has been asked", () => {
    expect(preflightIsCurrent({ status: "idle" }, key)).toBe(false);
  });

  // The point of the rule: a verdict about a graph the author has since changed
  // is not an answer about the graph on screen. Showing a stale "would be
  // accepted" is the false green #1048 was about, one layer up.
  it("is false for a verdict about a different graph", () => {
    const stale: Preflight = { status: "ok", key: '{"id":"other"}' };
    expect(preflightIsCurrent(stale, key)).toBe(false);
  });

  it("is true for every verdict about this graph", () => {
    const states: Preflight[] = [
      { status: "asking", key },
      { status: "ok", key },
      { status: "refused", key, message: "nope" },
      { status: "unavailable", key },
    ];
    for (const state of states) expect(preflightIsCurrent(state, key)).toBe(true);
  });
});
