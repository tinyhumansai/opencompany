/**
 * Bounds an in-flight read that may never settle at all (PR #1875 review
 * finding — the three admin-check/activation/setup-roster threads on
 * `app-shell.tsx:957`, `useActivationGate.ts:155`, `gate-logic.ts:191`).
 *
 * Every retry/stuck escape this file's callers already have (`isGateAdminStuck`,
 * `useActivationGate.stuck`, `shouldHoldShellPending`'s `setupChecked` guard)
 * is driven by the read's promise *settling* — a `catch` block counts
 * failures, or a `.then` clears them. None of that machinery ever runs for a
 * request that neither resolves nor rejects: a stalled proxy, a backend that
 * accepted the connection and then never answered, or any other case where
 * the browser's own `fetch` simply never produces an event. `OpenCompanyClient`
 * has no timeout anywhere in its request path (`api/transport/browser.ts`
 * calls bare `fetch` with no `AbortSignal`), so that is not a hypothetical —
 * it is the only way any of the three surfaces above can hang forever despite
 * three rounds of "escape the stuck read" fixes already landing.
 *
 * This turns "never settles" into "settles late, as a rejection" so the
 * existing per-surface handling (all of which already treats a non-terminal
 * rejection as a retryable failure) sees a normal failure instead of nothing
 * at all. It does not cancel the underlying request — `OpenCompanyClient`'s
 * `get`/`fetchMe`/`listTeam` do not accept an `AbortSignal` (only `getBlob`
 * does), and threading one through every call site the three surfaces above
 * use is a materially larger change than closing the hang. A late response
 * arriving after the timeout is simply ignored by whichever caller raced it
 * — each of the three sites already discards a response it no longer wants
 * (generation checks, `cancelled` flags), the same shape this reuses.
 */
export class ReadTimeoutError extends Error {
  constructor(ms: number) {
    super(`read did not settle within ${ms}ms`);
    this.name = "ReadTimeoutError";
  }
}

/**
 * Races `promise` against a `ms`-millisecond timer, rejecting with a
 * {@link ReadTimeoutError} if the timer wins.
 *
 * The timer is always cleared once `promise` itself settles, timeout or not
 * — otherwise every read would leak a `setTimeout` for the life of whichever
 * component mounted it.
 */
export function withReadTimeout<T>(promise: Promise<T>, ms: number): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const timer = setTimeout(() => reject(new ReadTimeoutError(ms)), ms);
    promise.then(
      (value) => {
        clearTimeout(timer);
        resolve(value);
      },
      (err) => {
        clearTimeout(timer);
        reject(err);
      },
    );
  });
}
