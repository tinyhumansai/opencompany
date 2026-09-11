// @vitest-environment jsdom

import { beforeEach, describe, expect, it } from "vitest";

import type { ActivationStatus } from "@/api/activation";
import type { LocalScope } from "@/connections/types";
import {
  type GateDecisionInput,
  shouldHoldShellPending,
  shouldShowOnboardingGate,
} from "@/onboarding/gate-logic";
import {
  clearGateDismissed,
  gateDismissed,
  gateSkippedThisSession,
  markGateDismissed,
  markGateSkipped,
} from "@/onboarding/state";

/**
 * "Skip setup" must end the gate for good.
 *
 * The trap it closes: step 3 (`workflow_run_succeeded`) has no waiver. The
 * integration step grew one for bugs B-001/B-020 precisely because an
 * unfinishable step plus a session-scoped skip is a checklist that re-prompts
 * on every new tab, forever — and the workflow step is the same shape of
 * condition (a run parked on an approval leaves it honestly unticked, and the
 * gate offers nothing to answer it with) that never got the same escape.
 *
 * The other half of the fix is what this must NOT do: following a link out of
 * the gate still writes only the session marker, because that is the founder
 * going to *do* a step rather than answering it.
 */

const incomplete: ActivationStatus = {
  nameConfirmed: true,
  integrationConnected: true,
  // The step with no waiver — the one that strands a founder.
  workflowRunSucceeded: false,
  isActivated: false,
};

const base: GateDecisionInput = {
  status: incomplete,
  checked: true,
  setupOpen: false,
  skippedThisSession: false,
  isAdmin: true,
};

const scope: LocalScope = { connection: "local", company: "acme" };
const other: LocalScope = { connection: "local", company: "beta" };

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
});

describe("the durable dismissal", () => {
  it("keeps the gate off for a company whose funnel is still incomplete", () => {
    // Without it this exact input renders the gate — that is what makes the
    // assertion below about the dismissal rather than about the fixture.
    expect(shouldShowOnboardingGate(base)).toBe(true);
    expect(shouldShowOnboardingGate({ ...base, dismissed: true })).toBe(false);
  });

  it("does not hold the shell on a loader either", () => {
    // The gate and the pending loader are two different ways to take the
    // console away from someone. Closing only the first would swap a
    // checklist they cannot finish for a spinner they cannot leave.
    const pending = { ...base, retrying: false, setupChecked: true, isAdmin: null };
    expect(shouldHoldShellPending(pending)).toBe(true);
    expect(shouldHoldShellPending({ ...pending, dismissed: true })).toBe(false);
  });

  it("survives the tab that set it, unlike the session skip", () => {
    markGateDismissed(scope);
    // `sessionStorage` is what a new tab does not carry over; `localStorage`
    // is. Asserting on the stores directly, because "a new tab" is exactly the
    // thing a single test process cannot open.
    expect(gateDismissed(scope)).toBe(true);
    sessionStorage.clear();
    expect(gateDismissed(scope)).toBe(true);
  });

  it("speaks only for the company it was pressed on", () => {
    markGateDismissed(scope);
    expect(gateDismissed(other)).toBe(false);
  });

  it("is not written by the session skip that in-gate navigation uses", () => {
    // `leaveGateFor` writes the session marker alone. If this ever starts
    // writing the durable one too, every "Open Workflows" click silently
    // becomes a permanent dismissal.
    markGateSkipped(scope);
    expect(gateSkippedThisSession(scope)).toBe(true);
    expect(gateDismissed(scope)).toBe(false);
  });

  it("is cleared once the funnel genuinely completes", () => {
    // Housekeeping, for the reason the skip and waiver clears exist: a stale
    // dismissal would speak for a later incomplete funnel nobody answered.
    markGateDismissed(scope);
    clearGateDismissed(scope);
    expect(gateDismissed(scope)).toBe(false);
  });

  it("reads as not dismissed when storage refuses", () => {
    const original = Object.getOwnPropertyDescriptor(window, "localStorage");
    Object.defineProperty(window, "localStorage", {
      configurable: true,
      get() {
        throw new Error("private mode");
      },
    });
    try {
      // The safe direction: an unreadable store re-offers the gate rather than
      // silently suppressing it on a company nobody dismissed.
      expect(gateDismissed(scope)).toBe(false);
      expect(() => markGateDismissed(scope)).not.toThrow();
    } finally {
      if (original) Object.defineProperty(window, "localStorage", original);
    }
  });
});
