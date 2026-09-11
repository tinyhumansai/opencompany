// Session-local "skip for now" state for the onboarding gate (issue #1844).
//
// Deliberately `sessionStorage`, not `localStorage`. The gate's own latch —
// `ActivationStatus.isActivated` on the host — is what makes a *completed*
// funnel never reappear; this flag only ever suppresses an *unfinished* one,
// and only for the tab that clicked "skip for now". A hard lock behind a
// broken Composio connect is worse than the blank app the gate replaces (the
// issue's own words), so skipping must always be reachable — but it must also
// re-prompt, or the gate this issue adds would be exactly as toothless as the
// cosmetic tour it demotes. `sessionStorage` buys both for free: it survives
// in-tab navigation (so a reload mid-session does not re-trap someone who just
// skipped) and disappears on the next fresh tab/window, which is the
// "re-prompts" the issue asks for without a second host round trip to track it.

import { type LocalScope, scopedKey } from "@/connections/types";

// Plain `scopedKey`, not `scopedKeyAdoptingLegacy`: this flag has no
// pre-connection predecessor to adopt — the funnel it gates did not exist
// before connections did — so there is nothing to migrate.
const KEY = (scope: LocalScope): string => scopedKey("oc-onboarding-gate-skip", scope);

/** Records that the operator dismissed the gate without finishing it. */
export function markGateSkipped(scope: LocalScope): void {
  try {
    sessionStorage.setItem(KEY(scope), String(Date.now()));
  } catch {
    /* private mode / quota — the gate simply re-offers on the next check */
  }
}

/** Whether the gate was skipped earlier in this tab's session. */
export function gateSkippedThisSession(scope: LocalScope): boolean {
  try {
    return sessionStorage.getItem(KEY(scope)) !== null;
  } catch {
    return false;
  }
}

/**
 * Clears the skip marker — called the moment the funnel actually completes, so
 * a stale marker from an earlier abandoned attempt cannot outlive it (it
 * cannot matter once `isActivated` is `true`, but leaving it set is still a
 * leak worth cleaning up rather than reasoning about later).
 */
export function clearGateSkipped(scope: LocalScope): void {
  try {
    sessionStorage.removeItem(KEY(scope));
  } catch {
    /* nothing to clear */
  }
}

// ---------------------------------------------------------------------------
// The durable dismissal — "Skip setup".
//
// `localStorage`, unlike the `sessionStorage` skip marker above, and the two are
// deliberately separate markers rather than one with a longer life.
//
// The session skip is set by TWO different actions: pressing the footer button,
// and following any link out of the gate (`leaveGateFor` in `app-shell.tsx`).
// Those mean opposite things. Following "Open Workflows" is the founder going to
// *do* step 3 — the gate must stand down for that navigation and must still be
// owed afterwards. Pressing the button is the founder saying they are done being
// asked. Collapsing them would make every in-gate link a permanent dismissal.
//
// Why the button now outlives the tab at all: step 3 has no waiver. The
// integration step grew one for bugs B-001/B-020 — a founder on a build with no
// credential path could never complete it, so the session skip re-trapped them
// on every new tab, forever. `workflow_run_succeeded` is the same shape of
// condition (`src/company/activation.rs` scans the journal for a real run that
// reached `Succeeded`) and `WorkflowStep` was never given the same escape: a run
// parked on an approval has proven nothing, so the step stays honestly unticked,
// and the founder who does not want to run one has no answer to give. The gate
// then reappears on every fresh tab with a checklist that cannot be finished.
//
// So the button is the answer to the step it has no waiver for. Scoped per
// company by the same `scopedKey` the markers above use, so dismissing one
// company's gate never speaks for another's.

const DISMISSED_KEY = (scope: LocalScope): string =>
  scopedKey("oc-onboarding-gate-dismissed", scope);

/** Records that the founder dismissed the gate for good on this company. */
export function markGateDismissed(scope: LocalScope): void {
  try {
    localStorage.setItem(DISMISSED_KEY(scope), String(Date.now()));
  } catch {
    /* private mode / quota — the gate re-offers, same as a failed skip write */
  }
}

/** Whether the founder has dismissed this company's gate for good. */
export function gateDismissed(scope: LocalScope): boolean {
  try {
    return localStorage.getItem(DISMISSED_KEY(scope)) !== null;
  } catch {
    return false;
  }
}

/**
 * Drops the dismissal — housekeeping once the funnel genuinely completes, for
 * the same reason [`clearGateSkipped`] and [`clearGateStepWaivers`] exist. It
 * cannot matter while `isActivated` is true, and leaving it set would silently
 * speak for a *later* incomplete funnel the founder never saw.
 */
export function clearGateDismissed(scope: LocalScope): void {
  try {
    localStorage.removeItem(DISMISSED_KEY(scope));
  } catch {
    /* nothing to clear */
  }
}

// ---------------------------------------------------------------------------
// Durable per-step waivers (bugs B-001 / B-020).
//
// `localStorage`, deliberately NOT the `sessionStorage` the skip marker above
// uses, and the difference is the whole point of this half of the module.
//
// "Skip for now" is a statement about *this moment* — "not now, ask me again" —
// so re-prompting in a fresh tab is the correct behaviour and the comment at
// the top of this file defends it properly.
//
// A waiver is a statement about *this company on this build*: "there is no
// credential I can give you, and there will not be one when I open a new tab."
// `integration_connected` (`src/company/activation.rs`) only ever reads true
// when the build compiled the `composio` feature AND the manifest grants the
// namespace AND a live connection exists. A self-hosted founder on a build with
// no credential path can satisfy the other two steps and never this one — so
// the session-scoped skip re-trapped them in the same unfinishable checklist on
// every new window, forever. That is B-020: two individually-defensible
// decisions (a session-scoped skip, a strict completion condition) that are only
// wrong together.
//
// The fix is not to loosen the completion condition — the host is right that no
// connection means no connection — but to let the founder record, durably, that
// they have answered this step as far as this build allows. The operator's own
// words for the rule: "once this step has passed it should never come back."
// A waiver is what "passed" means for a step whose precondition is out of the
// founder's reach.
//
// Scoped per company by the same `scopedKey` the skip marker uses, so waiving
// the step on one company never speaks for another.

/** The gate's three step ids, as `OnboardingGate` names them. */
export type GateStepId = "name" | "integration" | "workflow";

/** Every step id, in the order the gate itself lists them. */
const GATE_STEP_IDS: readonly GateStepId[] = ["name", "integration", "workflow"];

/**
 * One `localStorage` key PER STEP, not one key holding all three in a JSON
 * array (tinysweeper critique, PR #2046).
 *
 * The array form was a read-modify-write over a single slot: two tabs waiving
 * DIFFERENT steps at nearly the same moment (finish the name step in one,
 * finish the workflow step in the other — an ordinary way to have two tabs
 * open on the same company) would each read the array before the other's
 * write landed, and whichever write happened last silently discarded the
 * other tab's waiver. A step's own key can only ever be written by an action
 * that waives THAT step, so there is no shared slot left to race over — two
 * tabs waiving the same step at once just write the same thing twice, and two
 * tabs waiving different steps never touch each other's key at all. Safe to
 * change the wire format outright: this waiver feature is new in this PR, so
 * there is no previously-written array-shaped value anywhere to migrate.
 */
const WAIVED_STEP_KEY = (scope: LocalScope, step: GateStepId): string =>
  scopedKey(`oc-onboarding-gate-waived:${step}`, scope);

/**
 * Reads the durably-waived step ids for this company.
 *
 * Tolerates anything in a slot — a hand-edited value, a half-written entry —
 * by treating only "present at all" as waived and everything else (including
 * a read that throws) as not, rather than letting a corrupt slot throw into a
 * render. The cost of a misread is one extra prompt; the cost of a throw is
 * the blank app this whole gate exists to replace.
 */
export function waivedGateSteps(scope: LocalScope): GateStepId[] {
  return GATE_STEP_IDS.filter((step) => {
    try {
      return localStorage.getItem(WAIVED_STEP_KEY(scope, step)) !== null;
    } catch {
      return false;
    }
  });
}

/** Records that the founder answered `step` as far as this build allows. */
export function markGateStepWaived(scope: LocalScope, step: GateStepId): void {
  try {
    localStorage.setItem(WAIVED_STEP_KEY(scope, step), String(Date.now()));
  } catch {
    /* private mode / quota — the gate re-offers, same as a failed skip write */
  }
}

/**
 * Drops the waiver for one step — called the moment the HOST reports that step
 * complete, without waiting for the whole funnel (Codex review, PR #2046).
 *
 * [`outstandingGateSteps`]'s own doc already states the rule this enforces: a
 * waiver is only ever consulted for a step the host reports incomplete, and "a
 * stale waiver must never be able to mask a step going incomplete again
 * later". Ignoring the waiver while the step is done was only half of that.
 * The other half is this: a founder waives `integration`, the integration then
 * genuinely connects while some other step is still outstanding — so
 * `isActivated` never latches and [`clearGateStepWaivers`] never runs — and
 * the connection is later revoked or expires. Without this, the months-old
 * waiver comes back into force against a step that a credential now makes
 * ordinarily completable, and this browser stops showing a gate the host still
 * considers owed.
 *
 * Clearing at completion rather than at activation makes the waiver mean what
 * it says: an answer to the step as it stood when it could not be finished.
 */
export function clearGateStepWaiver(scope: LocalScope, step: GateStepId): void {
  try {
    localStorage.removeItem(WAIVED_STEP_KEY(scope, step));
  } catch {
    /* nothing to clear */
  }
}

/**
 * Drops every waiver — called once the funnel genuinely completes, for the same
 * housekeeping reason [`clearGateSkipped`] exists: a waiver cannot matter once
 * `isActivated` is true, and a stale one left behind would silently speak for a
 * *later* incomplete funnel (a company whose connection is later revoked)
 * without the founder ever having answered that one.
 */
export function clearGateStepWaivers(scope: LocalScope): void {
  for (const step of GATE_STEP_IDS) {
    try {
      localStorage.removeItem(WAIVED_STEP_KEY(scope, step));
    } catch {
      /* nothing to clear */
    }
  }
}
