import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import type { WorkflowBlockedNode } from "@/api/workflows";
import { parkedSplit, resumeClaim, resumeClaimFor } from "@/views/workflows/resume-claim";

/**
 * Issue B-013: two screens made opposite claims about what approving does.
 *
 * A founder parked a run on one approval and read, about that one run:
 *
 *   Workflow detail — "Approve it in Approvals and this run continues on its
 *   own — approving re-runs the step, so a changed decision may ask again."
 *
 *   Observatory — "The card is a question the agent raised, not a call waiting
 *   to be authorised: answering it is recorded against the card, but it does
 *   not restart this run — re-run the workflow once the answer is in hand."
 *
 * The Observatory was right at the time. A gated tool call's park carries the
 * node's turn key, so approving re-runs the turn and the run continues; a
 * blocker is parked with no continuation, because answering a question is not
 * the act of authorising a call. The workflow panel stated the gated-call
 * behaviour for both kinds because the host never put the split on the wire,
 * so the console could not tell them apart.
 *
 * Node-level restart has since shipped (issues #1863, #2005):
 * `resume_node_blocker` banks a blocker's answer and re-dispatches the node
 * from the run's own trigger input, so answering a question restarts the run
 * too now — just by a different route than a gated call's. Both sentences
 * below say so, in the host's own words for each route.
 */

const node = (over: Partial<WorkflowBlockedNode> = {}): WorkflowBlockedNode => ({
  nodeId: "draft",
  tools: ["publish_artifact"],
  approvalIds: ["appr-1"],
  ...over,
});

describe("splitting a blocked run's cards by what deciding them does", () => {
  it("reads a node that parked only gated calls as all gated", () => {
    expect(parkedSplit([node({ approvalIds: ["a", "b"], blockers: 0 })])).toEqual({
      gated: 2,
      blockers: 0,
      unknown: false,
    });
  });

  it("reads a node that parked only questions as all blockers", () => {
    // What a `park_node_blocker` push looks like on the wire: no tools, because
    // nothing the agent called was gated — the node itself is what stopped.
    expect(parkedSplit([node({ tools: [], approvalIds: ["a"], blockers: 1 })])).toEqual({
      gated: 0,
      blockers: 1,
      unknown: false,
    });
  });

  it("splits a node whose cards are of both kinds", () => {
    expect(parkedSplit([node({ approvalIds: ["a", "b", "c"], blockers: 1 })])).toEqual({
      gated: 2,
      blockers: 1,
      unknown: false,
    });
  });

  it("sums across nodes", () => {
    const split = parkedSplit([
      node({ nodeId: "one", approvalIds: ["a"], blockers: 0 }),
      node({ nodeId: "two", tools: [], approvalIds: ["b"], blockers: 1 }),
    ]);
    expect(split).toEqual({ gated: 1, blockers: 1, unknown: false });
  });

  it("ignores a node whose every park failed", () => {
    // No ids, so there is nothing to decide and nothing to claim about.
    expect(parkedSplit([node({ approvalIds: [], unparkable: 2 })])).toEqual({
      gated: 0,
      blockers: 0,
      unknown: false,
    });
  });

  it("marks a node from a host that cannot answer the question as unknown", () => {
    // `blockers` is `skip_serializing_if = "is_zero"` on the host, so an absent
    // key from a host that HAS the field means zero — but a host predating it
    // sends nothing either, and one blocked node cannot tell those apart.
    expect(parkedSplit([node({ approvalIds: ["a"] })])).toEqual({
      gated: 0,
      blockers: 0,
      unknown: true,
    });
  });

  it("never counts more questions than the node has cards", () => {
    // Defensive: a host that miscounted must not make `gated` negative.
    expect(parkedSplit([node({ approvalIds: ["a"], blockers: 4 })])).toEqual({
      gated: 0,
      blockers: 1,
      unknown: false,
    });
  });
});

describe("the claim each split produces", () => {
  it("promises the run continues only when every card is a gated call", () => {
    const claim = resumeClaim({ gated: 2, blockers: 0, unknown: false });
    expect(claim).toContain("continues this run automatically");
    expect(claim).toContain("a changed decision may ask again");
  });

  it("says answering re-enters the step, not the gated-call sentence, when every card is a question", () => {
    // Before B-013's fix this branch did not exist and the screen promised
    // the gated-call sentence for a question instead. Since #1863/#2005,
    // answering really does re-enter the node — just not by the gated call's
    // own turn-key route, which is the distinction this branch still has to
    // make.
    const claim = resumeClaim({ gated: 0, blockers: 1, unknown: false });
    expect(claim).toContain("re-enters this step");
    expect(claim).toContain("approving runs it again, and denying stops the run");
    expect(claim).not.toContain("continues this run automatically");
  });

  it("says the cards are, plural, when more than one question is parked", () => {
    expect(resumeClaim({ gated: 0, blockers: 2, unknown: false })).toContain("The cards are");
    expect(resumeClaim({ gated: 0, blockers: 1, unknown: false })).toContain("The card is");
  });

  it("names the right verdicts for each kind when the cards are mixed", () => {
    const claim = resumeClaim({ gated: 1, blockers: 1, unknown: false });
    expect(claim).toContain("gated tool calls, which continue this run when approved");
    expect(claim).toContain("re-enter the step they stopped");
  });

  it("degrades to the mixed sentence rather than promising a resume it cannot verify", () => {
    // The regression risk of shipping this: a host that sends no `blockers` key
    // would fall back to "all gated" and re-tell the original lie on every run
    // it serves.
    const claim = resumeClaim({ gated: 0, blockers: 0, unknown: true });
    expect(claim).not.toContain("continues this run automatically");
    expect(claim).toContain("re-enter the step they stopped");
  });

  it("claims nothing when the run has nothing decidable", () => {
    expect(resumeClaim({ gated: 0, blockers: 0, unknown: false })).toBeNull();
    expect(resumeClaimFor([])).toBeNull();
  });
});

/**
 * The wording parity that makes this one answer rather than two agreeing ones.
 *
 * The host's sentence and the run drawer's are written in different languages
 * and cannot literally share a string. What they can share is the rule and the
 * words, and this is what fails when they drift — which is the whole shape of
 * the original bug: two independently written strings for one state, and
 * nobody noticing they disagreed.
 *
 * Defect B-072 moved the host's half out of `blocked_diagnosis` and into
 * `src/workflows/resume_claim.rs`, because the host turned out to compose this
 * sentence *twice* and only one of the two branched — so a corrected panel
 * rendered the host's uncorrected claim directly beneath it. The wording is
 * read from its new home rather than from `caps/mod.rs`, which now calls it.
 * That the host still has only one copy of these words is `resume_claim`'s own
 * `both_host_composers_make_the_same_claim` to enforce; this file's job is the
 * language boundary.
 */
const here = dirname(fileURLToPath(import.meta.url));
const hostWording = readFileSync(
  resolve(here, "../../../src/workflows/resume_claim.rs"),
  "utf8",
);
// The counts the branch is fed still come from `caps`, and are asserted below.
const caps = readFileSync(
  resolve(here, "../../../src/workflows/caps/mod.rs"),
  "utf8",
);

/** Collapse Rust's `\` line continuations and the source's own indentation. */
function rustText(source: string): string {
  return source.replace(/\\\s*\n\s*/g, "").replace(/\s+/g, " ");
}

describe("the console's claim matches the host's, phrase for phrase", () => {
  const host = rustText(hostWording);

  it("uses the host's own words for an all-gated run", () => {
    const claim = resumeClaim({ gated: 1, blockers: 0, unknown: false });
    expect(host).toContain(claim!.replace(/\s+/g, " "));
  });

  it("uses the host's own words for an all-question run", () => {
    const claim = resumeClaim({ gated: 0, blockers: 1, unknown: false })!;
    // The leading "The card is" / "The cards are" is interpolated host-side, so
    // the shared half is what follows it.
    const shared = claim.slice(claim.indexOf("a question the agent raised"));
    expect(host).toContain(shared.replace(/\s+/g, " "));
  });

  it("uses the host's own words for a mixed run", () => {
    const claim = resumeClaim({ gated: 1, blockers: 1, unknown: false });
    expect(host).toContain(claim!.replace(/\s+/g, " "));
  });

  it("is fed a real count — the host puts the split on the wire", () => {
    // The other half of the fix, and the half no console test can reach on its
    // own: `resumeClaim` is only ever as honest as `WorkflowBlockedNode.blockers`,
    // and if the host pushes a hardcoded zero there the console degrades to
    // "every card is a gated call" and tells the original lie again. Read off
    // the same source this file already reads for wording parity.
    //
    // The gated push carries the count `blocked_diagnosis` branches on.
    expect(caps).toContain("blockers: parked.blockers,");
    // Every `park_node_blocker*` push is a question, never a gated call, and
    // there are three of them.
    expect(caps.match(/blockers: 1,\n/g)?.length ?? 0).toBeGreaterThanOrEqual(3);
  });

  it("no longer says the sentence the two screens disagreed over", () => {
    // The literal string from the founder's screenshot. It must not come back
    // on either screen, in any branch.
    const everyClaim = [
      resumeClaim({ gated: 1, blockers: 0, unknown: false }),
      resumeClaim({ gated: 0, blockers: 1, unknown: false }),
      resumeClaim({ gated: 1, blockers: 1, unknown: false }),
      resumeClaim({ gated: 0, blockers: 0, unknown: true }),
    ].join(" ");
    expect(everyClaim).not.toContain("this run continues on its own");
  });
});

/** Source with comments removed, so a note *about* the old copy is not read as
 *  the old copy. Both panels explain the change in a comment that quotes it. */
function withoutComments(source: string): string {
  return source.replace(/\/\*[\s\S]*?\*\//g, "").replace(/(^|[^:])\/\/.*$/gm, "$1");
}

describe("both run screens render the shared claim rather than their own", () => {
  const panel = (name: string) =>
    readFileSync(resolve(here, `../../src/views/workflows/${name}`), "utf8");

  for (const file of ["RunResultPanel.tsx", "RunHistoryPanel.tsx"]) {
    it(`${file} calls resumeClaimFor and states nothing itself`, () => {
      const source = panel(file);
      expect(source).toContain('from "./resume-claim"');
      expect(source).toContain("resumeClaimFor(");
      // The claim each screen used to write for itself, in code it renders.
      expect(withoutComments(source)).not.toContain("this run continues on its own");
    });
  }
});
