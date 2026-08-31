import type { NotificationDto } from "@/api/types";

import { WEEK1_NUDGE_KIND } from "@/lib/week1-nudge";

/**
 * The non-mention rows the honest-verdicts work (issue #1865) started
 * writing through the same durable store `GET /notifications` returns:
 * `dispatch_failed`, `approval_expired`, `workflow_run_failed` /
 * `_stranded` / `_blocked`.
 *
 * Every consumer of that feed on the console — `mentionCountsByChannel`,
 * `mentionsToClear`, `threadsToReReadForMentions` — filters to
 * `kind === "mention"` by design (a reply or reaction must not silently
 * start badging as a summons). That is correct for those three, but it left
 * these rows with nothing: no badge, no rendered item anywhere, and no path
 * back to the server to mark them read, so they sat unread forever despite
 * being returned on every poll (Codex #1883 P1). This module is the minimal
 * surface that closes the loop — a one-shot toast per row, immediately
 * eligible to be marked read the same way a viewed mention is.
 *
 * [`WEEK1_NUDGE_KIND`] is excluded even though it is, mechanically, just
 * another non-mention row on this same feed (PR #1878 review, comment
 * 3893066248). `notifications()` on the host has no server-side kind
 * allowlist — every caller gets every unread row and filters client-side,
 * which is exactly the design this module itself relies on. That means an
 * unfiltered poll here would classify a week-1 nudge as operational too:
 * toast it as a generic warning, then mark it read the instant the tab is
 * visible (`scheduleAcknowledgement` below), before
 * `pickActiveNudge`/`WorkflowsView` ever gets a chance to show its own
 * purpose-built banner. The nudge has its own dedicated UI and its own
 * dismiss path (`week1-nudge-banner.tsx`); this module's job is the rows
 * that have no other consumer, and the nudge is not one of them.
 */
export function isOperationalNotification(notification: NotificationDto): boolean {
  return notification.kind !== "mention" && notification.kind !== WEEK1_NUDGE_KIND;
}

/**
 * Unread operational rows not yet announced this session.
 *
 * `announced` is the caller's running set of ids already toasted — a row is
 * durable and keeps coming back on every poll until it is marked read, so
 * without this guard the same dispatch failure would toast once per poll
 * interval rather than once, ever.
 */
export function operationalNotificationsToAnnounce(
  notifications: readonly NotificationDto[],
  announced: ReadonlySet<string>,
): NotificationDto[] {
  return notifications.filter(
    (n) => n.readAt === undefined && isOperationalNotification(n) && !announced.has(n.id),
  );
}

/** Toast severity for an operational row's `kind`. */
export type OperationalNotificationSeverity = "error" | "warning";

/**
 * `dispatch_failed` and every `workflow_run_*` kind name a run that did not
 * complete — an error. `approval_expired` is a deadline that passed rather
 * than a failure in the strict sense, so it gets the lighter warning
 * treatment. An unrecognized future kind defaults to warning rather than
 * error: this module cannot know its severity, and under-alarming a novel
 * kind is the safer default than crying wolf on it.
 */
export function operationalNotificationSeverity(
  notification: NotificationDto,
): OperationalNotificationSeverity {
  if (notification.kind === "dispatch_failed" || notification.kind.startsWith("workflow_run_")) {
    return "error";
  }
  return "warning";
}

/** One toasted id still waiting for the tab to become visible before it can be marked read. */
export interface PendingAcknowledgement {
  company: string | null;
  id: string;
}

/**
 * Decide which just-toasted ids may be marked read on the server right now,
 * versus which must wait (Codex #1883 P2, the toast+ack fix's own fallout).
 *
 * `app-shell` calls `toast.error`/`toast.warning` the instant a row is
 * announced, whether or not the tab is visible — sonner still enqueues and
 * renders it, it is only `toast-lifetime.ts`'s auto-dismiss clock that pauses
 * for a hidden tab (`sweepToasts`'s `env.documentHidden` guard), specifically
 * so the operator gets the toast's full life once they return. But the
 * previous revision of this fix marked the row read at that same enqueue
 * instant, not at the moment a person actually saw it. If the tab is closed
 * or reloaded before it is ever brought to the foreground, sonner's
 * in-memory toast — and the operator's only chance to see it — is gone, while
 * the durable row is already `readAt`-stamped server-side. A `dispatch_failed`
 * nobody ever laid eyes on reads as handled, which defeats the point of the
 * toast consumer this whole fix exists for.
 *
 * While the tab is hidden, every id is parked in `pending` instead of
 * acknowledged — see [`flushPendingAcknowledgements`] for when it finally is.
 * This does NOT reopen the "toasts once per poll interval instead of once"
 * bug the toast+ack fix closed: the caller's `operationalAnnouncedRef` guard
 * (a separate, non-durable set) is updated the moment a row is toasted,
 * hidden tab or not, so a still-unacknowledged row is never re-toasted on the
 * next poll — it is only left unacknowledged *server-side* until the tab is
 * actually seen.
 */
export function scheduleAcknowledgement(
  ids: readonly string[],
  company: string | null,
  documentHidden: boolean,
  pending: readonly PendingAcknowledgement[],
): { ackNow: string[]; pending: PendingAcknowledgement[] } {
  if (!documentHidden) {
    return { ackNow: [...ids], pending: [...pending] };
  }
  return {
    ackNow: [],
    pending: [...pending, ...ids.map((id) => ({ company, id }))],
  };
}

/**
 * Flush every id parked by [`scheduleAcknowledgement`] for `company`, called
 * on the hidden → visible transition — the moment the operator can actually
 * see whatever sonner already rendered while the tab sat in the background.
 *
 * Scoped to `company` rather than flushing everything parked: the tab could
 * in principle switch companies while hidden, and acknowledging an id under
 * the wrong company's scope would mark a stranger's row read (or 404/silently
 * no-op against a company that never held it). An id parked under a company
 * this tab has since left stays parked — there is no visibility edge for a
 * company nobody is looking at either, so nothing is lost, only deferred
 * again until that company (if ever) becomes current.
 */
export function flushPendingAcknowledgements(
  company: string | null,
  pending: readonly PendingAcknowledgement[],
): { ackNow: string[]; pending: PendingAcknowledgement[] } {
  const ackNow: string[] = [];
  const stillPending: PendingAcknowledgement[] = [];
  for (const p of pending) {
    if (p.company === company) ackNow.push(p.id);
    else stillPending.push(p);
  }
  return { ackNow, pending: stillPending };
}
