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
