/**
 * One bounded retry for a fetch the caller already treats as best-effort
 * (issue #1781 review, Codex P2).
 *
 * # The gap this closes
 *
 * `app-shell.tsx`'s hydration pass and `ChatView`'s own render pass each call
 * `client.getOperatorChannel(company)` independently, and each degrades a
 * failure to `null` on its own — a company can still open Chat, or still
 * rehydrate its real desks/DMs, without the pinned Operator row. That is the
 * right degrade for a **permanent** miss (an older host that predates the
 * route). It is the wrong one for a **transient** blip: if the shell's call
 * fails a single request while `ChatView`'s later, independent call
 * succeeds, the pinned row renders — but the shell's rehydration targets and
 * five-second polling permanently omitted its id, since that pass already
 * ran and gave up. The row then looks ready and empty, with nothing left to
 * retry it.
 *
 * A single retry after a short delay closes the common case — one dropped
 * request, not a host that genuinely lacks the route — without turning a
 * best-effort fetch into an open-ended one.
 *
 * # Why here, not inline
 *
 * A retry loop inline in the effect is not unit-testable without rendering
 * the whole shell (this repo has no harness for that — see
 * `chat-realtime-poll.test.ts`'s comment on testing app-shell.tsx by
 * extracting the pure logic instead). Extracted, the retry rule itself can be
 * proven directly: it retries at most once, waits before doing so, and gives
 * up to `null` rather than looping.
 */
export async function fetchWithOneRetry<T>(
  fetch: () => Promise<T>,
  delayMs = 300,
): Promise<T | null> {
  try {
    return await fetch();
  } catch {
    await new Promise((resolve) => setTimeout(resolve, delayMs));
    try {
      return await fetch();
    } catch {
      return null;
    }
  }
}
