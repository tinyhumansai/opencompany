// @vitest-environment jsdom
//
// Bugs B-001 / B-020: a self-hosted founder can satisfy "name the company" and
// "run a workflow" and never "connect an integration" — `integration_connected`
// (`src/company/activation.rs`) needs a Composio credential the build offers no
// way to obtain. The skip marker is `sessionStorage` by design, so every new
// tab re-asked the same unfinishable checklist. These cover the durable waiver
// that closes it, and the rule that makes a waived step count as answered.

import { beforeEach, describe, expect, it } from "vitest";

import type { ActivationStatus } from "@/api/activation";
import {
  type GateDecisionInput,
  outstandingGateSteps,
  shouldHoldShellPending,
  shouldShowOnboardingGate,
} from "@/onboarding/gate-logic";
import {
  clearGateStepWaivers,
  markGateStepWaived,
  waivedGateSteps,
} from "@/onboarding/state";
import type { LocalScope } from "@/connections/types";

const scope: LocalScope = { connection: "local", company: "acme" };
const otherScope: LocalScope = { connection: "local", company: "other" };

/** The operator's own state, read off the host rather than the screen: two of
 *  three genuinely true, and the third unreachable on this build. */
const operatorState: ActivationStatus = {
  nameConfirmed: true,
  integrationConnected: false,
  workflowRunSucceeded: true,
  isActivated: false,
};

const base: GateDecisionInput = {
  status: null,
  checked: false,
  setupOpen: false,
  skippedThisSession: false,
  isAdmin: true,
};

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
});

describe("waiver storage", () => {
  it("remembers a waived step across a fresh read (the new-tab case)", () => {
    expect(waivedGateSteps(scope)).toEqual([]);
    markGateStepWaived(scope, "integration");
    // A new tab re-reads from storage; nothing else carries over.
    expect(waivedGateSteps(scope)).toEqual(["integration"]);
  });

  it("writes to localStorage, not sessionStorage — the whole point of B-020", () => {
    markGateStepWaived(scope, "integration");
    expect(sessionStorage.length).toBe(0);
    expect(localStorage.length).toBe(1);
  });

  it("does not let one company's waiver speak for another", () => {
    markGateStepWaived(scope, "integration");
    expect(waivedGateSteps(otherScope)).toEqual([]);
  });

  it("does not duplicate a step waived twice", () => {
    markGateStepWaived(scope, "integration");
    markGateStepWaived(scope, "integration");
    expect(waivedGateSteps(scope)).toEqual(["integration"]);
  });

  it("does not let a garbage value in an unrelated key count as a waiver", () => {
    // tinysweeper critique, PR #2046: one key PER STEP, not one shared array —
    // `waivedGateSteps` only asks "does this step's own key exist at all", so
    // there is no shape to corrupt the way a JSON blob could be.
    localStorage.setItem("some-other-key:local::acme", "{not json");
    expect(() => waivedGateSteps(scope)).not.toThrow();
    expect(waivedGateSteps(scope)).toEqual([]);
  });

  it("reads only the steps that were actually waived, one key each", () => {
    localStorage.setItem("oc-onboarding-gate-waived:integration:local::acme", String(Date.now()));
    expect(waivedGateSteps(scope)).toEqual(["integration"]);
  });

  it("does not race two tabs waiving different steps at once", () => {
    // The bug the per-step key scheme exists to close: a read-modify-write
    // over one shared array let two near-simultaneous writes for DIFFERENT
    // steps clobber each other, because each write's `Set` was built from a
    // read that happened before the other tab's write landed. A per-step key
    // makes that impossible to reproduce even with real concurrency — this
    // asserts the shape holds when both calls have already landed, in either
    // order, which is what any interleaving of two real tabs converges to.
    markGateStepWaived(scope, "name");
    markGateStepWaived(scope, "workflow");
    expect(waivedGateSteps(scope).sort()).toEqual(["name", "workflow"]);
  });

  it("clears every waiver once the funnel completes", () => {
    markGateStepWaived(scope, "integration");
    clearGateStepWaivers(scope);
    expect(waivedGateSteps(scope)).toEqual([]);
  });
});

describe("outstandingGateSteps", () => {
  it("counts a step the host reports incomplete and nobody waived", () => {
    expect(outstandingGateSteps(operatorState, [])).toEqual(["integration"]);
  });

  it("stops counting a step once it is waived", () => {
    expect(outstandingGateSteps(operatorState, ["integration"])).toEqual([]);
  });

  it("ignores a waiver for a step that actually completed", () => {
    // A stale waiver must never be the reason a step reads settled — if the
    // host says done, done is why.
    expect(outstandingGateSteps(operatorState, ["name"])).toEqual(["integration"]);
  });

  it("still counts the steps that were not waived", () => {
    const nothingDone: ActivationStatus = {
      nameConfirmed: false,
      integrationConnected: false,
      workflowRunSucceeded: false,
      isActivated: false,
    };
    expect(outstandingGateSteps(nothingDone, ["integration"])).toEqual(["name", "workflow"]);
  });
});

describe("shouldShowOnboardingGate with waivers", () => {
  const read = { ...base, checked: true };

  it("blocks the operator's exact state before they waive anything", () => {
    expect(shouldShowOnboardingGate({ ...read, status: operatorState })).toBe(true);
  });

  it("stops blocking once the only outstanding step is waived", () => {
    expect(
      shouldShowOnboardingGate({ ...read, status: operatorState, waived: ["integration"] }),
    ).toBe(false);
  });

  it("keeps blocking while another step is still outstanding", () => {
    const twoLeft: ActivationStatus = {
      nameConfirmed: false,
      integrationConnected: false,
      workflowRunSucceeded: true,
      isActivated: false,
    };
    expect(shouldShowOnboardingGate({ ...read, status: twoLeft, waived: ["integration"] })).toBe(
      true,
    );
  });

  it("treats an omitted waiver list as nothing waived", () => {
    expect(shouldShowOnboardingGate({ ...read, status: operatorState })).toBe(true);
  });
});

describe("shouldHoldShellPending with waivers", () => {
  const held = { ...base, setupChecked: true, retrying: false, checked: true };

  it("holds while the role is unresolved and a step is still outstanding", () => {
    expect(shouldHoldShellPending({ ...held, status: operatorState, isAdmin: null })).toBe(true);
  });

  it("does not hold once every outstanding step is waived", () => {
    // Otherwise a founder who answered everything they can gets a loader on
    // every fresh tab instead of a gate — the same trap, quieter.
    expect(
      shouldHoldShellPending({
        ...held,
        status: operatorState,
        isAdmin: null,
        waived: ["integration"],
      }),
    ).toBe(false);
  });
});
