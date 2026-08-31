// The pure half of the onboarding gate (issue #1844): whether to render
// `OnboardingGate` instead of the ordinary shell.
//
// Pinned here rather than left as an inline `if` in `app-shell.tsx`, for the
// same reason `shouldOfferSetup` lives in `lib/company-setup.ts` instead of
// inside `SetupController`: getting this wrong is expensive in a specific
// direction — showing the blocking gate to an operator who already cleared
// it, or over a company still being staffed — and a pure function of state is
// what makes that a unit test instead of a manual click-through.

import type { ActivationStatus } from "@/api/activation";
import { ApiError } from "@/api/types";

export interface GateDecisionInput {
  /** The funnel's last successful read, or `null` before the first one lands. */
  status: ActivationStatus | null;
  /** Whether that first read has landed (distinct from `status` — see the hook). */
  checked: boolean;
  /**
   * Whether first-run setup (staffing) is still on screen or the company is
   * unstaffed — `AppShell`'s own `setupOpen`, the same signal `TourController`
   * holds on. Setup runs first: an operator with nobody on the roster yet has
   * no workflow to run, so the activation gate must wait for it exactly the
   * way the tour already does.
   */
  setupOpen: boolean;
  /** Whether "skip for now" was clicked earlier in this tab's session. */
  skippedThisSession: boolean;
  /**
   * Whether the signed-in user is this company's admin — `null` before that
   * read has landed (PR #1875 review finding).
   *
   * Two of the three steps this gate blocks on cannot be cleared by anyone
   * else: naming the company (`PATCH {scope}`) is `require_admin`-gated on
   * the host, and `OAuthView` disables every connect control unless
   * `/auth/me` reports `role === "admin"`. (The third — running a workflow —
   * is not admin-gated; see `shouldPollActivationForRole`'s doc for why that
   * still doesn't make the gate itself safe to show a member: two blockers
   * they cannot clear is already a dead end.) An invited member's only way
   * past an unconditional gate was "Skip for now" — session-scoped by design
   * (see `onboarding/state.ts`), so it re-traps them every new tab — which
   * made this screen a dead end for exactly the people it cannot ask
   * anything of.
   */
  isAdmin: boolean | null;
}

/**
 * Whether the blocking first-run gate should be on screen right now.
 *
 * Order matters and is deliberate: a session-scoped skip and an in-progress
 * setup both win over an unread or incomplete funnel, because rendering the
 * gate over either would be rendering it wrong rather than merely early — see
 * each guard's own reasoning below.
 */
export function shouldShowOnboardingGate(input: GateDecisionInput): boolean {
  // "Skip for now" must always win. A hard lock behind a broken Composio
  // connect is worse than the blank app this gate replaces (the issue's own
  // words) — an operator who dismissed it must never be trapped back in it
  // until they navigate again, even if a poll landed in between.
  if (input.skippedThisSession) return false;

  // Staffing runs first. A company with nobody on the roster has no workflow
  // an operator authored to run, so asking them to clear step 3 here would be
  // asking for something `SetupController` has not offered them yet.
  if (input.setupOpen) return false;

  // Before the first read lands there is nothing to gate on — rendering the
  // gate here would flash it open for every company, activated or not, for
  // the one round trip it takes to learn which.
  if (!input.checked || !input.status) return false;

  // PR #1875 review finding: an invited member cannot act on two of the
  // three steps below (see `isAdmin`'s own doc comment), so the gate must
  // never be their dead end. `null` — the admin read has not landed yet — is
  // held here the same way `checked`/`status` are just above: an admin who
  // would otherwise see the gate immediately now waits one extra round trip
  // rather than this ever flashing open for a member who cannot clear it.
  if (input.isAdmin === null || !input.isAdmin) return false;

  return !input.status.isActivated;
}

/**
 * Whether the shell should hold in a neutral pending state rather than
 * render the ordinary interactive shell (PR #1875 review finding, round 8).
 *
 * `shouldShowOnboardingGate`'s `!input.checked || !input.status` guard above
 * answers one question — "do not show the *gate* yet" — and `AppShell`
 * plugged that straight into "so render the ordinary shell instead", on the
 * premise that the unresolved window is always the brief one round trip a
 * fresh mount's first read takes. `useActivationGate` breaks that premise
 * whenever `getActivation` hits a transient failure (a dropped connection, a
 * proxy 5xx): the hook deliberately leaves `checked: false` for as long as it
 * keeps retrying (see `ActivationGate.retrying`'s own doc) rather than
 * settling early on a non-answer, so an outage of any length reads exactly
 * like "first read still in flight" to `shouldShowOnboardingGate` — and
 * `AppShell` rendered the full, ordinary, clickable shell for the entire
 * outage on that account. Worse, if the company is genuinely not activated,
 * the funnel abruptly replaces that already-interactive shell with the
 * blocking gate the instant a retry finally lands — the same "abrupt
 * replacement" `resolveActivationReadError`'s own doc calls out for a single
 * glitched read, just stretched over however long the outage runs.
 *
 * This predicate names that gap so `AppShell` can render a neutral loading
 * state instead of the ordinary shell while `retrying` is true — never the
 * gate itself (this is still an unknown, not a "not activated" answer) and
 * never the interactive shell (which is the state that was wrong to show).
 * Gated the same way `shouldShowOnboardingGate` is on `skippedThisSession`
 * and `setupOpen`: neither matters to an operator who dismissed the gate or
 * is still being staffed.
 *
 * `isAdmin` is deliberately NOT gated the same way here (PR #1875 review
 * finding, round 9 — the original round-8 cut copied
 * `shouldShowOnboardingGate`'s `isAdmin === null || !isAdmin` guard
 * verbatim, which was wrong for this predicate specifically). Only a
 * confirmed non-admin (`isAdmin === false`) can never see the gate — for
 * `null` (the read has not landed yet), the gate is still an open question,
 * not a settled "no". Bypassing pending for `null` reintroduced the exact
 * bug this predicate exists to close: whenever the admin check simply
 * hadn't resolved yet — which is the common case for at least the first
 * leg of any outage, since both reads start together on mount — this guard
 * fired before `retrying` was ever consulted, so round 8's fix only worked
 * on the accident of `isAdmin` resolving `true` before the outage was
 * noticed.
 *
 * The `checked` branch has the same class of bug: `checked` landing does
 * NOT mean `shouldShowOnboardingGate` has everything it needs — it also
 * needs `isAdmin` resolved, and while that is still `null`,
 * `shouldShowOnboardingGate` returns `false` (its own null guard) exactly
 * the way it does for "not checked yet". The unconditional
 * `if (input.checked) return false` therefore let the ordinary shell render
 * for as long as `/auth/me` was slow or failing even after activation had
 * already read incomplete — then had the gate abruptly replace it the
 * instant the role resolved, the same "abrupt replacement" this whole file
 * exists to prevent.
 *
 * The pre-`checked` branch had the same class of bug once more (PR #1875
 * review finding, round 10): it held only while `retrying` was true, on the
 * premise that the window before the very first read lands is always brief.
 * `retrying` does not flip true until the first attempt has already
 * *rejected* — it reads `false` for as long as that first `getActivation`
 * call is merely in flight, indistinguishable here from "just mounted, read
 * due any moment". That premise is false for exactly the population this
 * gate exists to protect: an incomplete company's read is not a cached
 * latch short-circuit (see `compute_and_latch`'s own doc) — it scans the
 * journal for `WorkflowRunFinished`, so the very first call can legitimately
 * take longer than one quick round trip. An admin of such a company got the
 * fully interactive shell for that entire window, then the gate abruptly
 * replacing it the instant the slow-but-successful read finally landed — the
 * same failure this predicate exists to close, just triggered by latency
 * instead of a caught rejection. `retrying` therefore no longer gates this
 * branch: any unresolved first read holds, whether it is still in flight or
 * has already failed once and is retrying — the caller cannot tell those
 * two states apart from the outside, so neither should this predicate.
 *
 * The `setupOpen` branch below has the same class of bug once more (PR #1875
 * review finding, round 12): round 8 copied it verbatim from
 * `shouldShowOnboardingGate` — see this file's own `setupOpen` doc — on the
 * premise that "setup is open or the company is unstaffed" is a settled
 * answer whenever it reads `true`. It is not: `AppShell`'s `setupOpen` state
 * *starts* `true` "until `SetupController` has read the roster we do not
 * know whether setup is about to open" (its own doc comment), and only
 * flips once that async `listTeam` call lands — indistinguishable here from
 * a roster read that already landed and found the company genuinely
 * unstaffed. A staffed, incompletely-activated admin therefore got the
 * ordinary interactive shell for the entire roster read (a real network
 * call, not a cached flag — no bound on how long), then had it abruptly
 * replaced by this pending loader or the gate the instant that unrelated
 * read finally landed and flipped `setupOpen` to `false` — the exact
 * failure this predicate exists to close, just triggered by the setup read
 * instead of the activation read. `setupChecked` names whether that read has
 * landed, mirroring `checked` on the activation side; only once it has can
 * `setupOpen`'s value be trusted to mean "genuinely open" rather than "no
 * answer yet".
 */
export function shouldHoldShellPending(
  input: GateDecisionInput & { retrying: boolean; setupChecked: boolean },
): boolean {
  if (input.skippedThisSession) return false;
  // A confirmed non-admin can never see the gate (`shouldShowOnboardingGate`'s
  // own guard) — nothing to hold the shell pending for, regardless of what
  // either read below is still resolving.
  if (input.isAdmin === false) return false;

  if (!input.setupChecked) {
    // Setup's own roster read has not landed — `setupOpen` cannot yet be
    // trusted (see this function's own doc); hold rather than let the
    // ordinary shell render on what might turn out to be an unstaffed,
    // not-yet-activated company.
    return true;
  }
  if (input.setupOpen) return false;

  if (!input.checked) {
    // The first read has not landed at all yet — still in flight, or it has
    // already failed once and is retrying. Both look identical from here,
    // and both are worth holding the ordinary shell back for; see this
    // function's own doc for why `retrying` cannot distinguish them safely.
    return true;
  }

  if (!input.status) {
    // `checked` landed with no `status`: a terminal legacy-host 404
    // (`resolveActivationReadError`) — that answer is final, this hook never
    // retries it, so there is nothing left to hold the shell for.
    return false;
  }

  // Activation has landed. Once the company reads activated, no role can
  // make the gate appear — nothing left to hold for, `isAdmin` resolved or
  // not.
  if (input.status.isActivated) return false;

  // The company is not (yet) activated and the admin role is still
  // unresolved: `shouldShowOnboardingGate` cannot rule the gate in or out
  // yet. Hold the neutral screen rather than let the ordinary shell render
  // and then get yanked out from under the operator the moment the role
  // resolves.
  return input.isAdmin === null;
}

/** What a failed `/auth/me` read (behind `isGateAdmin` in `AppShell`) resolves to. */
export type GateAdminCheckOutcome =
  | { settled: true; isAdmin: boolean }
  | { settled: false };

/**
 * Classifies a `fetchMe` failure for the gate's admin check (PR #1875 review
 * finding).
 *
 * Every other `fetchMe`-gated view in this app (`OAuthView`, `TeamView`, …)
 * catches every failure the same way — `admin = false` — and that is safe
 * there: the worst case is a connect button staying disabled one round trip
 * longer. It is the wrong direction here, because `isAdmin: false` is what
 * makes `shouldShowOnboardingGate` suppress the blocking gate — a transient
 * failure (a dropped connection, a proxy 5xx) would resolve to "not admin"
 * exactly like a real 401 does, and fail the gate open for an actual admin
 * for the rest of that mount.
 *
 * A definitive `401` — no session on this host at all, whether because there
 * is no user plane or the operator is signed out — is the one answer that
 * genuinely means non-admin, and settles immediately, same as before.
 * Anything else (`ApiError` with any other status, or a raw `fetch` throw
 * that never reached the host) is not an answer about *who this user is* —
 * `settled: false` tells the caller to retry rather than guess.
 */
export function resolveGateAdminCheckError(error: unknown): GateAdminCheckOutcome {
  if (error instanceof ApiError && error.status === 401) {
    return { settled: true, isAdmin: false };
  }
  return { settled: false };
}

/** What a failed `GET {scope}/activation` read (behind `useActivationGate`) resolves to. */
export type ActivationReadOutcome =
  | { settled: true }
  | { settled: false };

/**
 * Classifies a `getActivation` failure for `useActivationGate`'s first read
 * (PR #1875 review finding, round 3).
 *
 * The hook's old `catch { setChecked(true) }` treated every failure the same
 * — a legacy host predating this route, a dropped connection, a proxy 5xx —
 * which settles `checked` with `status` left `null`. `shouldShowOnboardingGate`
 * renders the ordinary shell for that combination exactly as it would for
 * "not checked yet", so a real (non-activated) company whose *first* read
 * merely glitched got a multi-second window of the ordinary, unblocked shell
 * before a later poll succeeded and the gate abruptly replaced it — worse
 * than the same "abrupt" appearance on a normal first read, because that one
 * lands before the operator has had any chance to start clicking around.
 *
 * A `404` is the one answer that is genuinely final: this host does not have
 * the route at all, and retrying will not change that, so it settles —
 * `status` stays `null` and the gate stays off, same as before this fix.
 * Anything else (`ApiError` with any other status, or a raw `fetch` throw
 * that never reached the host) is not an answer about *whether this company
 * is activated* — `settled: false` tells the caller to retry rather than
 * guess, sooner than the regular poll cadence.
 */
export function resolveActivationReadError(error: unknown): ActivationReadOutcome {
  if (error instanceof ApiError && error.status === 404) {
    return { settled: true };
  }
  return { settled: false };
}

/**
 * Whether `useActivationGate`'s poll should keep running (PR #1875 review
 * finding, round 4).
 *
 * `GET {scope}/activation` is the only production caller of
 * `compute_and_latch` on the host — nothing else notices when the funnel's
 * three steps have all gone true and persists `activation_completed_at`. The
 * poll must therefore keep running for as long as the funnel is incomplete,
 * regardless of "skip for now": an admin who skips, then connects an
 * integration and runs a workflow from the ordinary shell, has genuinely
 * completed the funnel — but if the poll stopped the moment they skipped,
 * nothing would ever observe that and persist it. Reloading the same tab
 * preserves the session skip, and the funnel reads incomplete forever even
 * though every step is actually done.
 *
 * Only `isActivated` — the one thing the poll exists to notice — stops it;
 * `status: null` (nothing read yet) must keep polling, not stop it.
 */
export function shouldPollActivation(status: ActivationStatus | null): boolean {
  return status?.isActivated !== true;
}

/**
 * Whether `useActivationGate`'s poll should run at all, given what is known
 * about the signed-in user's admin status (PR #1875 review finding, round 5;
 * corrected round 7).
 *
 * Round 5's premise was that none of the three funnel steps this gate blocks
 * on can be cleared by anyone but this company's admin, so a confirmed
 * non-admin's poll could never be the read that observes the funnel go
 * complete. That holds for the first two steps — naming and the integration
 * connect are both `require_admin`-gated routes on the host — but not the
 * third: `POST {scope}/workflows/{wid}/run` (`src/server/ops/workflows.rs`)
 * is gated by `ScopedCompany`, the same guard `GET {scope}/activation`
 * itself uses, not `AdminScopedCompany`. Any signed-in member can run a
 * workflow.
 *
 * That makes a confirmed non-admin's poll load-bearing in exactly the
 * scenario round 5 tried to save load on: an admin confirms the name,
 * connects an integration, then closes their tab before running a workflow.
 * A member picks up where they left off and runs one from the ordinary
 * shell — the funnel's last domino, cleared by someone this predicate had
 * decided could never move it. Had that member's own tab already stopped
 * polling on `isAdmin === false`, and the admin's tab is gone, nothing left
 * running calls `GET {scope}/activation` — the only production caller of
 * `compute_and_latch` — so `activation_completed_at` never gets stamped
 * until an admin happens to open the console again, arriving late and
 * mistimed.
 *
 * There is no cheaper role-based split that stays correct: a non-admin's
 * poll only looks provably useless from a single read taken while the first
 * two steps are still incomplete, and a *later* admin action (from a
 * different tab or session) can make it useful again without this tab ever
 * re-reading that change. So every role polls exactly alike now — same as
 * `shouldPollActivation` already governs by `isActivated` alone, independent
 * of role. This predicate keeps taking `isAdmin` so call sites keep naming
 * the input `useActivationGate`'s `enabled` was originally wired to, in case
 * a real role-aware split is worth re-deriving later; today it isn't safe.
 */
export function shouldPollActivationForRole(_isAdmin: boolean | null): boolean {
  return true;
}
