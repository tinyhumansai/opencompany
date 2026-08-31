import type { NotificationDto } from "@/api/types";

/**
 * The wire `kind` week-1 nudge notifications carry — mirrors
 * `NUDGE_KIND` in `src/company/week1_nudge.rs` on the host. Kept as one
 * constant, imported everywhere the console asks for or files this kind, so
 * the two sides cannot drift the way a duplicated string literal would let
 * them (issue #1845).
 */
export const WEEK1_NUDGE_KIND = "workflow_nudge";

/**
 * Which row out of a `GET …/notifications?kind=workflow_nudge` feed the
 * week-1 nudge banner should show, if any.
 *
 * `LifecycleScheduler` files at most one nudge per user in practice (its own
 * idempotency ledger — see that module's docs), but this reads the feed
 * defensively rather than leaning on that invariant holding forever on the
 * wire: unread rows only, newest first, so a future multi-touch nudge (the
 * issue's own "deferred" scope cut) or a stale duplicate from a retried write
 * both resolve to one sane answer instead of an arbitrary one.
 *
 * A read row is filtered out here rather than by whoever calls this — which
 * is what makes "mark it read, then reload" work with no separate
 * reload-specific code path: a reload is just another call into this same
 * function over the host's now-updated feed, and a row the host has marked
 * read never gets picked, on this call or any later one.
 *
 * Also filters to [`WEEK1_NUDGE_KIND`] itself, even though every caller today
 * already requests `?kind=workflow_nudge` server-side: a helper this small
 * should not trust its one caller never to hand it a mixed feed, and it costs
 * nothing to be the thing that is actually true regardless.
 */
export function pickActiveNudge(rows: readonly NotificationDto[]): NotificationDto | null {
  const unread = rows.filter(
    (row) => row.kind === WEEK1_NUDGE_KIND && row.readAt === undefined,
  );
  if (unread.length === 0) return null;
  return unread.reduce((newest, row) => (row.createdAt > newest.createdAt ? row : newest));
}
