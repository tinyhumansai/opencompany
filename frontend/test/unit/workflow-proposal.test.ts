import { describe, expect, it } from "vitest";

import {
  applyProposal,
  diffGraphs,
  isEmptyDiff,
  PROPOSAL_FENCE,
  proposalIsStale,
  readProposal,
  validateProposal,
  type WorkflowProposal,
} from "@/api/workflow-proposal";
import type { WorkflowGraph } from "@/api/workflows";

/**
 * Issue #415. The copilot proposes an edit; the operator reviews a diff and
 * applies it. Everything below is the part of that round trip that is decidable
 * without a host: what a reply is allowed to contain, what the candidate graph
 * would be, what the operator is shown, and when a proposal must be refused.
 *
 * The refusals get as much attention as the happy path on purpose. A proposal
 * that applies something the operator did not review — because it was parsed
 * loosely, because the graph moved underneath it, or because it silently
 * dropped a node it never mentioned — is the failure mode this issue exists to
 * prevent, and each of those has a test here.
 */

function graph(): WorkflowGraph {
  return {
    id: "weekly_report",
    name: "Weekly report",
    version: "v1",
    editable: true,
    nodes: [
      { id: "collect", kind: "agent", name: "Collect", agent: "analyst" },
      { id: "send", kind: "output", name: "Send", destination: { kind: "owner" } },
    ],
    edges: [{ from: "collect", to: "send" }],
  };
}

function block(body: unknown): string {
  return "Here is what I'd change.\n\n```" + PROPOSAL_FENCE + "\n" + JSON.stringify(body) + "\n```";
}

describe("readProposal", () => {
  it("leaves an ordinary answer alone", () => {
    const read = readProposal("It collects, then sends.", graph());
    expect(read.prose).toBe("It collects, then sends.");
    expect(read.proposal).toBeUndefined();
    expect(read.problem).toBeUndefined();
  });

  it("splits the proposal out of the prose so no raw JSON is rendered", () => {
    const read = readProposal(
      block({
        summary: "Retry the collect step",
        ops: [{ op: "updateNode", id: "collect", set: { retry: { maxAttempts: 3 } } }],
      }),
      graph(),
    );
    expect(read.prose).toBe("Here is what I'd change.");
    expect(read.proposal?.summary).toBe("Retry the collect step");
    expect(read.proposal?.ops).toHaveLength(1);
  });

  /**
   * The version is stamped by the console from the graph it parsed against,
   * never read from the model. A model that could assert which graph it was
   * looking at could assert a stale proposal was fresh.
   */
  it("stamps the version of the graph it was read against", () => {
    const read = readProposal(
      block({ summary: "s", ops: [{ op: "removeNode", id: "send" }] }),
      graph(),
    );
    expect(read.proposal?.basedOnVersion).toBe("v1");
  });

  it("reports a block that isn't JSON instead of dropping it", () => {
    const read = readProposal("prose\n\n```" + PROPOSAL_FENCE + "\nnot json\n```", graph());
    expect(read.proposal).toBeUndefined();
    expect(read.problem?.reason).toMatch(/valid JSON/);
  });
});

describe("validateProposal", () => {
  const g = graph();

  it("refuses a proposal that says nothing about what it does", () => {
    const out = validateProposal({ ops: [{ op: "removeNode", id: "send" }] }, g);
    expect(out).toHaveProperty("reason");
  });

  it("refuses an empty edit list", () => {
    expect(validateProposal({ summary: "s", ops: [] }, g)).toHaveProperty("reason");
  });

  it("refuses an edit aimed at a step that isn't there", () => {
    const out = validateProposal(
      { summary: "s", ops: [{ op: "updateNode", id: "ghost", set: { name: "x" } }] },
      g,
    );
    expect(out).toMatchObject({ reason: expect.stringContaining("ghost") });
  });

  it("refuses a new step that reuses an existing id", () => {
    const out = validateProposal(
      { summary: "s", ops: [{ op: "addNode", node: { id: "collect", kind: "agent", name: "X" } }] },
      g,
    );
    expect(out).toMatchObject({ reason: expect.stringContaining("collect") });
  });

  /**
   * An id keys a node's edges, so renaming through `set` would orphan them
   * silently. A rename has to be an add plus a remove, which the diff then
   * shows as what it is.
   */
  it("refuses to rename a step through an update", () => {
    const out = validateProposal(
      { summary: "s", ops: [{ op: "updateNode", id: "collect", set: { id: "gather" } }] },
      g,
    );
    expect(out).toMatchObject({ reason: expect.stringContaining("id") });
  });

  /**
   * A key this console does not know cannot be rendered — an added step's diff
   * line carries its id and name — so keeping it would put something in the
   * candidate graph the operator never saw, and dropping it would apply a step
   * that is not the one described. Refused, like `updateNode`'s unknown field.
   */
  it("refuses a new step carrying a field that isn't a step field", () => {
    const out = validateProposal(
      {
        summary: "s",
        ops: [
          {
            op: "addNode",
            node: { id: "notify", kind: "output", name: "Notify", sudo: true },
          },
        ],
      },
      g,
    );
    expect(out).toMatchObject({ reason: expect.stringContaining("sudo") });
  });

  /**
   * A duplicate edge would append a second identical entry and diff to nothing
   * — the diff compares sets keyed by `from`/`to` — so the operator would apply
   * a change the review never showed.
   */
  it("refuses a connection that already exists", () => {
    const out = validateProposal(
      { summary: "s", ops: [{ op: "addEdge", from: "collect", to: "send" }] },
      g,
    );
    expect(out).toMatchObject({ reason: expect.stringContaining("already connected") });
  });

  it("refuses a connection to a step the proposal never adds", () => {
    const out = validateProposal(
      { summary: "s", ops: [{ op: "addEdge", from: "collect", to: "ghost" }] },
      g,
    );
    expect(out).toHaveProperty("reason");
  });

  /** …but accepts one to a step the same proposal adds, in order. */
  it("accepts a connection to a step the same proposal adds", () => {
    const out = validateProposal(
      {
        summary: "s",
        ops: [
          { op: "addNode", node: { id: "notify", kind: "output", name: "Notify" } },
          { op: "addEdge", from: "collect", to: "notify" },
        ],
      },
      g,
    );
    expect(out).not.toHaveProperty("reason");
  });

  it("refuses to disconnect two steps that were never connected", () => {
    const out = validateProposal(
      { summary: "s", ops: [{ op: "removeEdge", from: "send", to: "collect" }] },
      g,
    );
    expect(out).toHaveProperty("reason");
  });

  it("refuses an edit kind this console cannot render", () => {
    const out = validateProposal({ summary: "s", ops: [{ op: "replaceGraph", graph: {} }] }, g);
    expect(out).toMatchObject({ reason: expect.stringContaining("replaceGraph") });
  });

  /**
   * Issue #783. The host accepts a fixed set of node kinds
   * (`WORKFLOW_NODE_KINDS`); a step of any other kind is rejected on write, so
   * offering a diff for one is offering a change Apply will bounce. Refused here
   * with the valid set named, so the operator's next ask can be a real one.
   */
  it("refuses a new step whose kind the host would reject", () => {
    const out = validateProposal(
      { summary: "s", ops: [{ op: "addNode", node: { id: "x", kind: "webhook", name: "X" } }] },
      g,
    );
    expect(out).toMatchObject({ reason: expect.stringContaining("webhook") });
  });

  /**
   * The "wrong kind when applied" failure: a `tool_call` with no `config.slug`
   * is incoherent for its kind, and the host refuses it. The console mirror of
   * that rule catches it before the diff, with the host's actionable sentence.
   */
  it("refuses a tool_call that names no tool slug", () => {
    const out = validateProposal(
      {
        summary: "s",
        ops: [{ op: "addNode", node: { id: "call", kind: "tool_call", name: "Call" } }],
      },
      g,
    );
    expect(out).toMatchObject({ reason: expect.stringContaining("config.slug") });
  });

  /** …and an `agent` step that names no teammate, the roster half of the rule. */
  it("refuses an agent step that names no teammate", () => {
    const out = validateProposal(
      {
        summary: "s",
        ops: [{ op: "addNode", node: { id: "worker", kind: "agent", name: "Worker" } }],
      },
      g,
    );
    expect(out).toMatchObject({ reason: expect.stringContaining("agent") });
  });

  /**
   * A well-formed `tool_call` — kind valid, `slug` nested under `config` — is
   * accepted, so the grounding fix does not turn away the proposals it exists to
   * make possible.
   */
  it("accepts a well-formed tool_call with its slug nested in config", () => {
    const out = validateProposal(
      {
        summary: "Add a web search",
        ops: [
          {
            op: "addNode",
            node: {
              id: "search",
              kind: "tool_call",
              name: "Search",
              config: { slug: "web_search", args: { query: "=item.topic" } },
            },
          },
        ],
      },
      g,
    );
    expect(out).not.toHaveProperty("reason");
  });

  /**
   * Coherence is judged on the node an `updateNode` WOULD produce, not the change
   * alone: switching an existing step to `tool_call` without giving it a slug is
   * the same incoherent node the host rejects on write, so it is refused here.
   */
  it("refuses an update that switches a step to a kind whose config it lacks", () => {
    const out = validateProposal(
      { summary: "s", ops: [{ op: "updateNode", id: "send", set: { kind: "tool_call" } }] },
      g,
    );
    expect(out).toMatchObject({ reason: expect.stringContaining("config.slug") });
  });

  /**
   * `set: { kind: null }` used to slip through: the string-typed unknown-kind
   * check is skipped for a non-string, and the coherence pass saw a node whose
   * kind was `null`, so a graph the host rejects on write was offered as a diff.
   */
  it("refuses an update whose kind is present but not a string", () => {
    const out = validateProposal(
      { summary: "s", ops: [{ op: "updateNode", id: "send", set: { kind: null } }] },
      g,
    );
    expect(out).toMatchObject({ reason: expect.stringContaining("invalid kind") });
  });

  /**
   * The whole reply is raw model JSON, so any field can be the wrong type. An
   * `agent` set to an object must be refused, not throw `.trim` and take the
   * entire validation down with it.
   */
  it("refuses — without throwing — an agent field that isn't a string", () => {
    const run = () =>
      validateProposal(
        {
          summary: "s",
          ops: [{ op: "updateNode", id: "collect", set: { agent: { id: "analyst" } } }],
        },
        g,
      );
    expect(run).not.toThrow();
    expect(run()).toMatchObject({ reason: expect.stringContaining("agent") });
  });

  /**
   * A later op is judged against the graph as the proposal builds it, not the
   * original. Adding a valid `tool_call` and then emptying its `config` in the
   * same proposal produces the slug-less node the host rejects — caught only
   * because the update sees the node the add just introduced.
   */
  it("judges a later op against a node the same proposal added", () => {
    const out = validateProposal(
      {
        summary: "s",
        ops: [
          {
            op: "addNode",
            node: { id: "call", kind: "tool_call", name: "Call", config: { slug: "web_search" } },
          },
          { op: "updateNode", id: "call", set: { config: {} } },
        ],
      },
      g,
    );
    expect(out).toMatchObject({ reason: expect.stringContaining("config.slug") });
  });
});

describe("applyProposal", () => {
  it("never mutates the graph it was given, so a rejected proposal changes nothing", () => {
    const before = graph();
    const snapshot = JSON.stringify(before);
    applyProposal(before, {
      summary: "s",
      ops: [{ op: "removeNode", id: "send" }],
    });
    expect(JSON.stringify(before)).toBe(snapshot);
  });

  it("takes a removed step's connections with it", () => {
    const candidate = applyProposal(graph(), {
      summary: "s",
      ops: [{ op: "removeNode", id: "send" }],
    });
    expect(candidate.nodes.map((n) => n.id)).toEqual(["collect"]);
    expect(candidate.edges).toEqual([]);
  });

  it("applies field-level updates without touching the rest of the step", () => {
    const candidate = applyProposal(graph(), {
      summary: "s",
      ops: [{ op: "updateNode", id: "collect", set: { retry: { maxAttempts: 3 } } }],
    });
    const collect = candidate.nodes.find((n) => n.id === "collect");
    expect(collect?.retry).toEqual({ maxAttempts: 3 });
    expect(collect?.agent).toBe("analyst");
    expect(candidate.id).toBe("weekly_report");
  });
});

describe("diffGraphs", () => {
  it("reports added, removed and changed steps and the fields that moved", () => {
    const current = graph();
    const candidate = applyProposal(current, {
      summary: "s",
      ops: [
        { op: "updateNode", id: "collect", set: { name: "Collect numbers" } },
        { op: "addNode", node: { id: "notify", kind: "output", name: "Notify" } },
        { op: "addEdge", from: "send", to: "notify" },
        { op: "removeEdge", from: "collect", to: "send" },
      ],
    });
    const diff = diffGraphs(current, candidate);

    expect(diff.nodes).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          id: "collect",
          change: "changed",
          fields: [{ field: "name", before: "Collect", after: "Collect numbers" }],
        }),
        expect.objectContaining({ id: "notify", change: "added" }),
      ]),
    );
    expect(diff.edges).toEqual(
      expect.arrayContaining([
        { from: "send", to: "notify", change: "added" },
        { from: "collect", to: "send", change: "removed" },
      ]),
    );
  });

  /**
   * The diff is computed from the graphs, not from the op list, so an edit that
   * sets a field to what it already holds shows up as nothing. A proposal is
   * never allowed to look bigger than what it would do.
   */
  /**
   * A new step's whole payload reaches the review. `kind` decides what the step
   * IS, and a `schedule` or `requiresApproval` decides what the workflow does
   * without anybody pressing anything — applying those off a row that showed
   * only a name is the reviewed-one-thing-applied-another failure this module
   * exists to prevent.
   */
  it("shows every field a new step carries, not just its name", () => {
    const current = graph();
    const candidate = applyProposal(current, {
      summary: "s",
      ops: [
        {
          op: "addNode",
          node: {
            id: "digest",
            kind: "trigger",
            name: "Digest",
            schedule: "0 9 * * *",
            requiresApproval: true,
          },
        },
      ],
    });
    const added = diffGraphs(current, candidate).nodes.find((n) => n.id === "digest");
    expect(added?.change).toBe("added");
    const shown = Object.fromEntries(added!.fields.map((f) => [f.field, f.after]));
    expect(shown).toMatchObject({
      kind: "trigger",
      schedule: "0 9 * * *",
      requiresApproval: "true",
    });
    // The header already carries the name; repeating it as a field line would
    // be noise in the one place noise costs attention.
    expect(shown).not.toHaveProperty("name");
  });

  it("shows nothing for an edit that changes nothing", () => {
    const current = graph();
    const candidate = applyProposal(current, {
      summary: "s",
      ops: [{ op: "updateNode", id: "collect", set: { name: "Collect" } }],
    });
    expect(isEmptyDiff(diffGraphs(current, candidate))).toBe(true);
  });
});

describe("edge identity", () => {
  /**
   * A space-delimited key collides: `("a b", "c")` and `("a", "b c")` both read
   * as `a b c`. Two different connections comparing equal would let validation
   * refuse a genuinely new edge, and would let `diffGraphs` hide a real one from
   * the review — the two failures this module is most careful about, reached
   * through a node id nobody thought to forbid.
   */
  it("tells apart two edges a space-joined key would confuse", () => {
    const spaced: WorkflowGraph = {
      ...graph(),
      nodes: [
        { id: "a b", kind: "agent", name: "A B" },
        { id: "c", kind: "output", name: "C" },
        { id: "a", kind: "agent", name: "A" },
        { id: "b c", kind: "output", name: "B C" },
      ],
      edges: [{ from: "a b", to: "c" }],
    };

    // `("a", "b c")` is NOT the existing `("a b", "c")`, so it is a real new
    // connection rather than a duplicate to refuse.
    const out = validateProposal(
      { summary: "s", ops: [{ op: "addEdge", from: "a", to: "b c" }] },
      spaced,
    );
    expect(out).not.toHaveProperty("reason");

    // …and it shows up in the diff as an addition rather than vanishing into
    // the edge that was already there.
    const candidate = applyProposal(spaced, {
      summary: "s",
      ops: [{ op: "addEdge", from: "a", to: "b c" }],
    });
    expect(diffGraphs(spaced, candidate).edges).toEqual([
      { from: "a", to: "b c", change: "added" },
    ]);
  });
});

describe("proposalIsStale", () => {
  const proposal: WorkflowProposal = {
    summary: "s",
    ops: [{ op: "removeNode", id: "send" }],
    basedOnVersion: "v1",
  };

  it("is fresh against the graph it was proposed on", () => {
    expect(proposalIsStale(proposal, graph())).toBe(false);
  });

  it("is stale once the graph moves, so it is refused rather than rebased", () => {
    expect(proposalIsStale(proposal, { ...graph(), version: "v2" })).toBe(true);
  });

  it("is stale when the version it was based on is unknown", () => {
    expect(proposalIsStale({ ...proposal, basedOnVersion: undefined }, graph())).toBe(true);
  });

  /**
   * A host predating the version token (#259) has no concurrency protection at
   * all, for the canvas editor as much as for a proposal. Refusing every
   * proposal there would invent a protection that does not exist rather than
   * report one. A current host's `null` (issue #1013 — a non-editable graph's
   * honest "no token") must be treated the same way.
   */
  it("does not invent staleness on a host that has no versions", () => {
    const versionless = { ...graph(), version: null };
    expect(proposalIsStale({ ...proposal, basedOnVersion: undefined }, versionless)).toBe(false);
  });
});
