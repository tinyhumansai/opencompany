import { channelForThread } from "@/views/chat/model";

/**
 * The drain half of app-shell's deferred transcript re-read (issue #1701).
 *
 * `reReadSettledThread` folds a settled turn's durable history into both the
 * `threads` store and — keyed by channel id — the `transcripts` store. The
 * channel id comes from `chatChannelByThread`, which the desks/roster effect
 * populates asynchronously and clears to `{}` on every company switch. A turn
 * that settles while that map is still empty (cold load, or the moment after a
 * switch) folds into `threads` but has no channel to fold the transcript into,
 * so the Chat panel stays stale until an unrelated update repaints it.
 *
 * The fix parks such thread ids and replays them once the map names their
 * channel. This is its own module rather than a closure in the component so the
 * replay rule can be tested without rendering the shell — the same shape the
 * settings route rule is tested in (`test/unit/settings-route.test.ts`).
 */

/**
 * Replay every parked thread whose channel is now known.
 *
 * Iterates a **snapshot** of `pending` so the `reRead` call — which may fold and
 * re-render — cannot disturb the set mid-iteration. Each id present in
 * `channelMap` is removed from the set *before* `reRead` runs, so a re-entrant
 * or repeated drain never re-reads the same settled thread twice; ids whose
 * channel is still unknown stay parked for a later drain. The folds `reRead`
 * performs are idempotent (both stores drop already-known message ids), so a
 * deferred replay adds nothing a live frame already delivered.
 *
 * Checked through `channelForThread`, not a bare `channelMap[threadId]` index
 * (issue #1781 review, Codex P2): a settled turn can park under any casing
 * the host accepted for the General line (`MAIN`, `General`, …), and the map
 * only ever holds the four canonical spellings. A bare index on an
 * uncanonical id never matches, even once the map is fully populated, so
 * that thread would stay parked — and its transcript stale — forever, not
 * just until the next drain.
 */
export function drainReReadQueue(
  pending: Set<string>,
  channelMap: Record<string, string>,
  reRead: (threadId: string) => void,
): void {
  for (const threadId of [...pending]) {
    if (!channelForThread(channelMap, threadId)) continue;
    pending.delete(threadId);
    reRead(threadId);
  }
}
