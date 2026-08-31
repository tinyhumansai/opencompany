import { useCallback, useEffect, useRef, useState } from "react";

import type { OpenCompanyClient } from "@/api/client";
import { type ActivationStatus, getActivation } from "@/api/activation";
import { resolveActivationReadError } from "@/onboarding/gate-logic";
import { startVisiblePolling } from "@/lib/visible-poll";
import { withReadTimeout } from "@/lib/read-timeout";

/**
 * How often the gate re-reads the funnel while it is on screen.
 *
 * The gate has no SSE frame of its own to key a refresh off — the three steps
 * it watches live behind three different flows (a manifest write, an OAuth
 * round trip, a workflow run) and none of them tells this hook "something
 * changed" directly. Polling on the same cadence `useCompany` already uses for
 * the sidebar is cheap (the host's own read short-circuits to a record load
 * once the latch is set — see `compute_and_latch`) and buys "re-poll after
 * each [step]" (issue #1844's own words) without wiring a bespoke event for
 * three surfaces that do not otherwise need one.
 */
const POLL_MS = 5000;

/**
 * How soon a `getActivation` failure that `resolveActivationReadError` does
 * NOT settle (a network error, a proxy 5xx — anything but the legacy-host
 * `404`) retries, rather than waiting for the next `POLL_MS` tick. Mirrors
 * `GATE_ADMIN_CHECK_RETRY_MS` (`app-shell.tsx`, PR #1875 review finding):
 * a real, non-activated company whose first read merely glitched should get
 * a fast second attempt, not the standard 5s cadence — the gap between "the
 * shell renders unblocked" and "the gate correctly locks it" is exactly the
 * window an operator could start clicking around in, and a fast retry keeps
 * that window small instead of leaving it as wide as a full poll interval.
 */
const ACTIVATION_READ_RETRY_MS = 3000;

/**
 * How long a single `getActivation` call is allowed to sit with no response
 * at all before it is treated as a failure (PR #1875 review finding).
 *
 * The `inFlight` guard just below only clears once the call it is guarding
 * *settles* — resolve or reject, either flips it back to `false` in `finally`.
 * `getActivation` goes through `OpenCompanyClient`, whose request path has no
 * timeout of its own (`api/transport/browser.ts` calls bare `fetch`, no
 * `AbortSignal`), so a stalled proxy or backend that accepts the connection
 * and never answers leaves that promise pending forever: `inFlight` never
 * clears, every later poll tick keeps getting skipped by the very guard that
 * exists to stop them racing ahead of a live read, and `failures`/`stuck`
 * never fire either, because nothing here is a caught error. The escape this
 * hook already offers for a durable failure never has a failure to catch.
 * `withReadTimeout` turns that silence into an ordinary rejection at this
 * bound — `resolveActivationReadError` already classifies a non-`404` error
 * as non-terminal, so it flows straight into the existing `failures` counter
 * and eventually `stuck`, and `inFlight` clears in the same `finally` as any
 * other rejection. Comfortably above `SLOW_READ_MS` (`onboarding-gate-stuck-
 * escape.test.ts`) so a merely slow-but-successful read is never mistaken for
 * a hang.
 */
const ACTIVATION_READ_TIMEOUT_MS = 20000;

/**
 * How many consecutive non-terminal `getActivation` failures before the hook
 * reports `stuck`, and the shell stops showing a bare loader.
 *
 * `retrying` alone cannot carry this: it flips true on the very first
 * failure, which is routine (a proxy blip, a cold host still scanning the
 * journal) and resolves on the next attempt. Holding the shell for that is
 * correct. What is not correct is holding it forever — a durable backend
 * error (issue #1875 review: a malformed event that fails the whole-journal
 * scan on every read) leaves `checked` false permanently, and the pending
 * branch renders only `RouteLoading`, so the operator is locked out of the
 * entire console with the "skip for now" escape sitting inside a gate that
 * is never mounted.
 *
 * Three failures is ~9s of retries at `ACTIVATION_READ_RETRY_MS` — long
 * enough that a transient blip never shows the operator an error, short
 * enough that a real outage does not read as a hang.
 */
const STUCK_AFTER_FAILURES = 3;

export interface ActivationGate {
  /** Whether the first read has landed — before this, render nothing blocking. */
  checked: boolean;
  /** `null` only before the first read lands, or after a read that failed. */
  status: ActivationStatus | null;
  /**
   * True from the first transient `getActivation` failure (a network error, a
   * proxy 5xx — anything `resolveActivationReadError` does not settle) until
   * either a read succeeds or a later attempt settles terminally (PR #1875
   * review finding, round 8). Distinguishes "still stuck retrying, unknown
   * how long this outage lasts" from the ordinary brief "first read has not
   * landed yet" window `checked` alone conflates them into — see
   * `shouldHoldShellPending`'s own doc for why that distinction matters to
   * the caller.
   */
  retrying: boolean;
  /**
   * True once `STUCK_AFTER_FAILURES` consecutive reads have failed without
   * settling. The caller must offer a way out at this point rather than
   * keeping the operator on a loader — see `STUCK_AFTER_FAILURES`.
   */
  stuck: boolean;
  /** Re-reads the funnel immediately — called after an in-gate action. */
  refresh: () => Promise<void>;
}

/**
 * Polls `GET {scope}/activation` for as long as the caller wants it running,
 * re-subscribing whenever `company` changes.
 *
 * `enabled` lets the shell stop polling once the gate has nothing left to
 * decide (the company is activated, or the operator dismissed it) instead of
 * every open tab quietly reading a route it no longer renders anything from.
 */
export function useActivationGate(
  client: OpenCompanyClient,
  company: string | null,
  enabled: boolean,
): ActivationGate {
  const [checked, setChecked] = useState(false);
  const [status, setStatus] = useState<ActivationStatus | null>(null);
  const [retrying, setRetrying] = useState(false);
  const [stuck, setStuck] = useState(false);
  const failures = useRef(0);
  const generation = useRef(0);
  /**
   * Set once a read reports the latch. `isActivated` is monotonic on the host
   * (`ActivationStatus::is_activated`'s own contract) — once true it can never
   * go false again — so this lets every later tick short-circuit before the
   * network call instead of polling a settled answer forever.
   */
  const activated = useRef(false);
  /**
   * Set once `resolveActivationReadError` settles a read as a definitive
   * `404` — this host predates the route (PR #1875 review finding, round 6).
   * That answer can never change on a later tick either, the same way
   * `activated` never un-sets — without this, every tick after the first 404
   * still calls `getActivation` again, so a legacy-host tab keeps requesting
   * a route it already learned does not exist for the lifetime of the mount.
   */
  const terminal = useRef(false);
  /**
   * True while a `getActivation` call started by this hook is still awaiting
   * its response (PR #1875 review finding).
   *
   * `startVisiblePolling` (`lib/visible-poll.ts`) is a bare, non-waiting
   * `setInterval` — it has no idea whether the `load` it last called has
   * returned — so without this guard a read that consistently takes longer
   * than `POLL_MS` (the host's whole-journal scan on an incomplete company,
   * same class of cost `STUCK_AFTER_FAILURES`'s own doc calls out) starves
   * forever: every tick starts a new call that bumps `generation` before the
   * previous one can land, so `gen !== generation.current` discards every
   * single response as stale, in perpetuity, and `checked` never settles.
   * Unlike a caught failure this is not an error `resolveActivationReadError`
   * or `STUCK_AFTER_FAILURES` ever sees — every individual read "succeeds",
   * just always one generation too late — so nothing else in this hook would
   * ever surface it.
   *
   * Guarding on this (skip starting a new call while one is already in
   * flight) rather than skipping the *older* one that started it keeps the
   * fix local to this hook: no change to the shared poller, which
   * `ArtifactsTab` and the task-detail screen also depend on.
   */
  const inFlight = useRef(false);
  /** Pending fast retry from a read `resolveActivationReadError` did not settle. */
  const retryTimer = useRef<ReturnType<typeof setTimeout>>();

  const clearRetry = () => {
    if (retryTimer.current !== undefined) {
      clearTimeout(retryTimer.current);
      retryTimer.current = undefined;
    }
  };

  const load = useCallback(async () => {
    if (activated.current || terminal.current || inFlight.current) return;
    clearRetry();
    inFlight.current = true;
    const gen = ++generation.current;
    try {
      const next = await withReadTimeout(getActivation(client, company), ACTIVATION_READ_TIMEOUT_MS);
      if (gen !== generation.current) return;
      setStatus(next);
      setChecked(true);
      setRetrying(false);
      failures.current = 0;
      setStuck(false);
      if (next.isActivated) activated.current = true;
    } catch (err) {
      if (gen !== generation.current) return;
      const outcome = resolveActivationReadError(err);
      if (outcome.settled) {
        // A host predating this route: definitively no such funnel, and
        // retrying will not change that. `status` stays `null` —
        // `shouldShowOnboardingGate`'s own `!status` guard keeps the gate off
        // permanently, same as before this fix. `terminal` stops every later
        // tick from re-requesting a route this host already answered it does
        // not have (PR #1875 review finding, round 6) — same shape as
        // `activated` just below, for the other permanent answer this hook
        // can land on.
        terminal.current = true;
        setChecked(true);
        setRetrying(false);
        failures.current = 0;
        setStuck(false);
        return;
      }
      // A transient failure (network error, 5xx) — not an answer, so do not
      // settle `checked` on it. Retry sooner than the regular poll cadence
      // instead of waiting out the full `POLL_MS` tick — see
      // `ACTIVATION_READ_RETRY_MS`. `retrying` flips on here (PR #1875 review
      // finding, round 8): the caller must be able to tell "still waiting on
      // the very first read" apart from "stuck in an outage of unknown
      // length" — `checked` alone reads identically for both, and the two
      // need different renders (see `shouldHoldShellPending`).
      setRetrying(true);
      failures.current += 1;
      if (failures.current >= STUCK_AFTER_FAILURES) setStuck(true);
      retryTimer.current = setTimeout(() => {
        if (gen !== generation.current) return;
        void load();
      }, ACTIVATION_READ_RETRY_MS);
    } finally {
      // Only the call that is still the current generation owns clearing the
      // flag — a stale call that lost the `gen !== generation.current` race
      // above must not clear it out from under whichever call is now current
      // (see `inFlight`'s own doc for why that race exists at all).
      if (gen === generation.current) inFlight.current = false;
    }
  }, [client, company]);

  useEffect(() => {
    if (!enabled) return;
    activated.current = false;
    terminal.current = false;
    inFlight.current = false;
    setChecked(false);
    setStatus(null);
    setRetrying(false);
    failures.current = 0;
    setStuck(false);
    void load();
    const stopPolling = startVisiblePolling(() => void load(), POLL_MS);
    return () => {
      stopPolling();
      clearRetry();
    };
  }, [enabled, load]);

  return { checked, status, retrying, stuck, refresh: load };
}
