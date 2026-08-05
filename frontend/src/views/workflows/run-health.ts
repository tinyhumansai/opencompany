// How a run reads at a glance: its terminal status, its delivery counts, and
// how long ago it happened.
//
// Extracted from `WorkflowsView.tsx` (issue #303) because the workflow cards now
// need the SAME reading the history rows and the last-run chip use. Two
// implementations of "is this run healthy?" would drift, and the card grid is
// precisely the surface where a wrong green dot is most costly — it is the one
// an operator scans instead of opening anything.

import type { DeliveryReport, WorkflowRunOutcome } from "@/api/workflows";

/**
 * The `pending` delivery status — a report parked for an operator's approval —
 * is added to `DeliveryStatus` by issue #227. It is typed `string` rather than
 * written as a literal so these comparisons compile both before and after that
 * lands: against today's union TypeScript would reject the literal as a
 * no-overlap comparison, and once the union widens this keeps behaving
 * identically. The runtime check is what matters — the host can already send a
 * status this console's type doesn't name yet.
 */
const PENDING_STATUS: string = "pending";

/** Reports that did NOT reach their destination **and will not without a
 * change** — the number worth acting on. `pending` is excluded on purpose: it
 * is a report parked for an operator's approval, so counting it here would
 * badge a working approvals queue as a failure. */
export function undeliveredCount(deliveries: DeliveryReport[]): number {
  return deliveries.filter((d) => d.status !== "sent" && d.status !== PENDING_STATUS).length;
}

/** Reports waiting on an operator's verdict rather than on a fix. */
export function pendingCount(deliveries: DeliveryReport[]): number {
  return deliveries.filter((d) => d.status === PENDING_STATUS).length;
}

/** A compact "N minutes ago" for a run timestamp — enough to tell last night's
 * scheduled run from the one just clicked, without a date library. */
export function relativeTime(atMillis: number): string {
  const seconds = Math.max(0, Math.round((Date.now() - atMillis) / 1000));
  if (seconds < 60) return "just now";
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  return `${Math.round(hours / 24)}d ago`;
}

/** The status dot for a whole run: red when it failed or lost a report, sky
 * when something is parked for approval, green when everything landed. */
export function runTone(run: WorkflowRunOutcome): { dot: string; label: string } {
  if (run.error) return { dot: "bg-red-500", label: "failed" };
  // Issue #383: checked before the delivery reads, and deliberately NOT red. A
  // stop somebody asked for is not a fault, and a cancelled run has no
  // deliveries to weigh anyway — so without this arm it would fall through to
  // the green "ok" and read as a clean success.
  if (run.cancelled) return { dot: "bg-slate-400", label: "stopped" };
  if (undeliveredCount(run.deliveries) > 0) return { dot: "bg-red-500", label: "not delivered" };
  if (pendingCount(run.deliveries) > 0) return { dot: "bg-sky-500", label: "awaiting approval" };
  return { dot: "bg-emerald-500", label: "ok" };
}

/**
 * A run that is still walking its graph.
 *
 * Its own reading, ahead of {@link runTone}: an in-flight run has not failed and
 * has not succeeded, and painting it with either colour is a claim the host has
 * not made yet.
 */
export function isRunning(run: WorkflowRunOutcome): boolean {
  return run.running === true;
}
