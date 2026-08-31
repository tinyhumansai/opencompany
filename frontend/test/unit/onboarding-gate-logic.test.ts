import { describe, expect, it } from "vitest";

import type { ActivationStatus } from "@/api/activation";
import { ApiError } from "@/api/types";
import {
  type GateDecisionInput,
  resolveActivationReadError,
  resolveGateAdminCheckError,
  shouldHoldShellPending,
  shouldPollActivation,
  shouldPollActivationForRole,
  shouldShowOnboardingGate,
} from "@/onboarding/gate-logic";

const complete: ActivationStatus = {
  nameConfirmed: true,
  integrationConnected: true,
  workflowRunSucceeded: true,
  isActivated: true,
  activationCompletedAtMillis: 1_700_000_000_000,
};

const incomplete: ActivationStatus = {
  nameConfirmed: true,
  integrationConnected: false,
  workflowRunSucceeded: false,
  isActivated: false,
};

const base: GateDecisionInput = {
  status: null,
  checked: false,
  setupOpen: false,
  skippedThisSession: false,
  // Every pre-existing case below is written from an admin's point of view
  // (the only role the gate could originally distinguish), so the shared
  // fixture defaults to `true` and the admin-specific guard gets its own
  // cases below.
  isAdmin: true,
};

describe("shouldShowOnboardingGate", () => {
  it("does not block before the first read has landed", () => {
    expect(shouldShowOnboardingGate({ ...base, checked: false, status: null })).toBe(false);
  });

  it("blocks once the funnel reads incomplete", () => {
    expect(shouldShowOnboardingGate({ ...base, checked: true, status: incomplete })).toBe(true);
  });

  it("does not block once the funnel reads activated", () => {
    expect(shouldShowOnboardingGate({ ...base, checked: true, status: complete })).toBe(false);
  });

  it("blocks on isActivated alone, regardless of which individual steps still read false", () => {
    // The gate's only question is the latch, not a re-derivation of the three
    // steps — a status object with every individual flag false but
    // `isActivated: true` (the latch carrying an operator through a later
    // regression, per `ActivationStatus::is_activated` on the host) must still
    // read as "let them in".
    const latchedDespiteRegressedSteps: ActivationStatus = {
      nameConfirmed: false,
      integrationConnected: false,
      workflowRunSucceeded: false,
      isActivated: true,
      activationCompletedAtMillis: 1_700_000_000_000,
    };
    expect(
      shouldShowOnboardingGate({ ...base, checked: true, status: latchedDespiteRegressedSteps }),
    ).toBe(false);
  });

  it("holds while first-run setup (staffing) is still on screen, even with an incomplete funnel", () => {
    expect(
      shouldShowOnboardingGate({
        ...base,
        checked: true,
        status: incomplete,
        setupOpen: true,
      }),
    ).toBe(false);
  });

  it("never blocks once the operator skipped it this session", () => {
    expect(
      shouldShowOnboardingGate({
        ...base,
        checked: true,
        status: incomplete,
        skippedThisSession: true,
      }),
    ).toBe(false);
  });

  it("skip wins even over an in-progress setup dialog", () => {
    expect(
      shouldShowOnboardingGate({
        ...base,
        checked: true,
        status: incomplete,
        setupOpen: true,
        skippedThisSession: true,
      }),
    ).toBe(false);
  });

  // PR #1875 review finding: an invited member cannot clear any of the three
  // steps this gate blocks on — naming the company is `require_admin`-gated
  // on the host, and `OAuthView` disables every connect control unless
  // `/auth/me` reports `role === "admin"`. Their only way past an
  // unconditional gate was "Skip for now", which is deliberately
  // session-scoped (`onboarding/state.ts`) so it re-traps them on every new
  // tab. These three cases are what closes that dead end.
  describe("the admin-only guard (PR #1875)", () => {
    it("does not block before the admin read has landed, even with an incomplete funnel", () => {
      expect(
        shouldShowOnboardingGate({ ...base, checked: true, status: incomplete, isAdmin: null }),
      ).toBe(false);
    });

    it("never blocks a non-admin member — they cannot act on any step", () => {
      expect(
        shouldShowOnboardingGate({ ...base, checked: true, status: incomplete, isAdmin: false }),
      ).toBe(false);
    });

    it("still blocks an admin exactly as before once the read confirms the role", () => {
      expect(
        shouldShowOnboardingGate({ ...base, checked: true, status: incomplete, isAdmin: true }),
      ).toBe(true);
    });
  });
});

// PR #1875 review finding: `AppShell`'s `isGateAdmin` effect caught every
// `fetchMe` failure the same way — treat as non-admin — mirroring every
// other admin-gated view's `catch { admin = false }` pattern. That pattern is
// safe on a read-only view (a button stays disabled one round trip longer).
// It is wrong here: `isAdmin: false` fails the *blocking* gate open, so a
// transient failure (a dropped connection, a 5xx) would let an actual admin
// past onboarding for the rest of that mount. A definitive `401` — no
// session at all — is the one answer that genuinely means non-admin and must
// still settle immediately; everything else must be retried instead.
describe("resolveGateAdminCheckError", () => {
  it("settles to non-admin on a definitive 401 — no session on this host", () => {
    const outcome = resolveGateAdminCheckError(new ApiError(401, "no_session", "no session"));
    expect(outcome).toEqual({ settled: true, isAdmin: false });
  });

  it("does not settle on a network failure — retry instead of failing the gate open", () => {
    const outcome = resolveGateAdminCheckError(new ApiError(0, "network_error", "offline"));
    expect(outcome.settled).toBe(false);
  });

  it("does not settle on a 5xx — the host, not the session, is the problem", () => {
    const outcome = resolveGateAdminCheckError(new ApiError(503, "unavailable", "quiescing"));
    expect(outcome.settled).toBe(false);
  });

  it("does not settle on a non-ApiError throw (e.g. a raw fetch TypeError)", () => {
    const outcome = resolveGateAdminCheckError(new TypeError("Failed to fetch"));
    expect(outcome.settled).toBe(false);
  });
});

// PR #1875 review finding, round 3: `useActivationGate`'s first-read catch
// treated every failure the same — settle `checked` with `status` left
// `null`. That is correct for a legacy host that will never have this route
// (a `404`), but wrong for a transient failure (a dropped connection, a
// 5xx): it fails the gate open for up to a full poll interval, on the very
// read that decides whether a real, non-activated company gets blocked.
describe("resolveActivationReadError", () => {
  it("settles on a definitive 404 — this host has no such route", () => {
    const outcome = resolveActivationReadError(new ApiError(404, "not_found", "no route"));
    expect(outcome).toEqual({ settled: true });
  });

  it("does not settle on a network failure — retry instead of failing the gate open", () => {
    const outcome = resolveActivationReadError(new ApiError(0, "network_error", "offline"));
    expect(outcome.settled).toBe(false);
  });

  it("does not settle on a 5xx — the host, not the route, is the problem", () => {
    const outcome = resolveActivationReadError(new ApiError(503, "unavailable", "quiescing"));
    expect(outcome.settled).toBe(false);
  });

  it("does not settle on a non-ApiError throw (e.g. a raw fetch TypeError)", () => {
    const outcome = resolveActivationReadError(new TypeError("Failed to fetch"));
    expect(outcome.settled).toBe(false);
  });
});

// PR #1875 review finding, round 4: the shell wired the poll's `enabled` to
// `!gateSkipped`, so "skip for now" stopped `useActivationGate` from ever
// reading again. `GET {scope}/activation` is the only production caller of
// `compute_and_latch`, so a funnel that genuinely completes after a skip
// (an admin connects an integration and runs a workflow from the ordinary
// shell) never got its `activation_completed_at` persisted at all.
describe("shouldPollActivation", () => {
  it("keeps polling before the first read lands", () => {
    expect(shouldPollActivation(null)).toBe(true);
  });

  it("keeps polling while the funnel reads incomplete — this is what a skip must not silence", () => {
    expect(shouldPollActivation(incomplete)).toBe(true);
  });

  it("stops once the latch is set — nothing left for the poll to notice", () => {
    expect(shouldPollActivation(complete)).toBe(false);
  });
});

// PR #1875 review finding, round 5: the shell wired the poll's `enabled` to
// a bare `true`, so every invited member's tab kept polling `compute_and_latch`
// for as long as the company stayed unactivated — even though (round 5
// believed) none of the three funnel steps can ever be cleared by a
// non-admin, so that poll can never be the read that observes the funnel
// complete.
//
// Round 5's premise was wrong (PR #1875 review finding, round 7):
// `POST {scope}/workflows/{wid}/run` is gated by `ScopedCompany`, not
// `AdminScopedCompany` — any signed-in member can run a workflow, which is
// the funnel's third step. An admin who confirms the name, connects an
// integration, then closes their tab before running a workflow, can have a
// member complete the funnel from the ordinary shell. If that member's own
// tab had already stopped polling (round 5's `isAdmin === false` check),
// nothing left calls `GET {scope}/activation` — the only production caller
// of `compute_and_latch` — so `activation_completed_at` never gets stamped
// until an admin happens to reopen the console.
describe("shouldPollActivationForRole", () => {
  it("keeps polling before the admin check has landed — must not cost a real admin their fast first read", () => {
    expect(shouldPollActivationForRole(null)).toBe(true);
  });

  it("keeps polling for a confirmed admin", () => {
    expect(shouldPollActivationForRole(true)).toBe(true);
  });

  it("keeps polling for a confirmed non-admin too (round 7) — a member can still be the read that observes the workflow step complete after an admin who already cleared the other two steps closes their tab", () => {
    expect(shouldPollActivationForRole(false)).toBe(true);
  });
});

// PR #1875 review finding, round 8: `shouldShowOnboardingGate`'s
// `!checked || !status` guard reads identically for "first read still in
// flight" and "stuck retrying a transient outage of unknown length" —
// `AppShell` plugged that straight into rendering the ordinary, fully
// interactive shell, for the whole outage in the second case. `retrying`
// (from `useActivationGate`) is what tells these two apart.
describe("shouldHoldShellPending", () => {
  const pendingBase = { ...base, retrying: false, setupChecked: true };

  it("does not hold once the first read has landed, even if a later poll starts retrying", () => {
    expect(
      shouldHoldShellPending({ ...pendingBase, checked: true, status: incomplete, retrying: true }),
    ).toBe(false);
  });

  it("holds the shell pending while the very first read is still in flight, not yet failed (PR #1875 review finding, round 10) — `retrying` only flips true after a rejection, so a slow-but-eventually-successful first read (e.g. a large journal scan) read as identical to a brief one (was: does not hold while merely waiting on the first read — not retrying yet)", () => {
    expect(shouldHoldShellPending({ ...pendingBase, checked: false, retrying: false })).toBe(true);
  });

  it("holds the shell pending once stuck retrying before the first read lands", () => {
    expect(shouldHoldShellPending({ ...pendingBase, checked: false, retrying: true })).toBe(true);
  });

  it("does not hold once 'skip for now' was clicked — that must always win, same as the gate itself", () => {
    expect(
      shouldHoldShellPending({
        ...pendingBase,
        checked: false,
        retrying: true,
        skippedThisSession: true,
      }),
    ).toBe(false);
  });

  it("does not hold while setup/staffing is still open — the gate could not show yet either way", () => {
    expect(
      shouldHoldShellPending({ ...pendingBase, checked: false, retrying: true, setupOpen: true }),
    ).toBe(false);
  });

  // PR #1875 review finding, round 12: `setupOpen` starts `true` in `AppShell`
  // from mount — "until `SetupController` has read the roster we do not know
  // whether setup is about to open" (its own doc comment) — and only flips
  // once that async `listTeam` read lands. The guard just above (`setupOpen`
  // alone) cannot tell that starting value apart from a roster read that
  // already landed and found the company genuinely unstaffed; both read as
  // `true`. Round 8 copied that single-field check verbatim into this
  // function without asking whether `setupOpen`'s own unresolved substate
  // needed the same treatment as `checked`'s (round 8), `isAdmin`'s (round 9)
  // and the pre-`checked` window's (round 10) — so a staffed, incompletely
  // activated admin got the ordinary interactive shell for the entire roster
  // read (any duration — it is a full `listTeam` call, not a cached flag),
  // then had it abruptly replaced by the pending loader or the gate the
  // instant that unrelated read finally landed and flipped `setupOpen` to
  // `false`. `setupChecked` — mirroring `checked` on the activation side —
  // is what tells the two `setupOpen: true` cases apart.
  it("holds the shell pending while setup's own roster read has not landed yet — indistinguishable from a genuinely open dialog until it does (PR #1875 review finding, round 12)", () => {
    expect(
      shouldHoldShellPending({
        ...pendingBase,
        checked: true,
        status: incomplete,
        setupChecked: false,
      }),
    ).toBe(true);
  });

  it("does not hold once setup's roster read has landed and the company turns out staffed — falls through to the activation checks exactly as before", () => {
    expect(
      shouldHoldShellPending({
        ...pendingBase,
        checked: true,
        status: incomplete,
        setupChecked: true,
        setupOpen: false,
      }),
    ).toBe(false);
  });

  it("does not hold for a confirmed non-admin even while setup's roster read is unresolved — the gate could never block them, so there is nothing to wait for", () => {
    expect(
      shouldHoldShellPending({
        ...pendingBase,
        checked: false,
        isAdmin: false,
        setupChecked: false,
      }),
    ).toBe(false);
  });

  it("does not hold for a confirmed non-admin — the gate could never block them, so there is nothing to protect the shell from", () => {
    expect(
      shouldHoldShellPending({ ...pendingBase, checked: false, retrying: true, isAdmin: false }),
    ).toBe(false);
  });

  it("holds the shell pending while the admin role is still unresolved and activation is stuck retrying (round 9) — the old null guard bypassed this exactly like a confirmed non-admin, so round 8's own fix never fired unless the role happened to resolve `true` first (was: does not hold before the admin check itself has landed)", () => {
    expect(
      shouldHoldShellPending({ ...pendingBase, checked: false, retrying: true, isAdmin: null }),
    ).toBe(true);
  });

  it("holds the shell pending once activation has already read incomplete but the admin role is still unresolved (PR #1875 review finding, round 9)", () => {
    // `checked` landing first does NOT mean `shouldShowOnboardingGate` has
    // everything it needs — it still cannot rule the gate in or out while
    // `isAdmin` is null. The old `if (input.checked) return false` bypass
    // let the ordinary shell render for the whole time `/auth/me` was slow
    // or transiently failing, only to have the gate abruptly replace it the
    // instant the role resolved.
    expect(
      shouldHoldShellPending({ ...pendingBase, checked: true, status: incomplete, isAdmin: null }),
    ).toBe(true);
  });

  it("does not hold once activation reads incomplete for a confirmed non-admin, admin role resolved or not", () => {
    expect(
      shouldHoldShellPending({ ...pendingBase, checked: true, status: incomplete, isAdmin: false }),
    ).toBe(false);
  });

  it("does not hold for an already-activated company even while the admin role is unresolved — no role can make the gate appear", () => {
    expect(
      shouldHoldShellPending({ ...pendingBase, checked: true, status: complete, isAdmin: null }),
    ).toBe(false);
  });

  it("does not hold for a legacy host that settled with no status (terminal 404), admin role resolved or not", () => {
    expect(
      shouldHoldShellPending({ ...pendingBase, checked: true, status: null, retrying: false, isAdmin: null }),
    ).toBe(false);
  });
});
