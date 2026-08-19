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
  return deliveries.filter(
    (d) => d.status !== "sent" && d.status !== PENDING_STATUS,
  ).length;
}

/** Reports waiting on an operator's verdict rather than on a fix. */
export function pendingCount(deliveries: DeliveryReport[]): number {
  return deliveries.filter((d) => d.status === PENDING_STATUS).length;
}

/**
 * Everything about this run that is waiting on a person: the gates it paused at
 * **and** the reports it parked (issue #846).
 *
 * The two were never read together, and that is what let a run report success
 * while a human had not answered it. `pendingCount` sees only `deliveries`, so a
 * run that paused at a `requires_approval` node and therefore never reached an
 * `output` node at all — the exact shape of a gated workflow — has an empty
 * `deliveries` array and scored as a clean run.
 *
 * `pendingApprovals` has been on the wire since #395 and the history row has
 * badged its count since; nothing read it for the run's *state*. This is that
 * read, in one place, so the tone, the chip and the row cannot disagree about
 * whether somebody is being waited on.
 */
export function awaitingCount(run: WorkflowRunOutcome): number {
  return (run.pendingApprovals?.length ?? 0) + pendingCount(run.deliveries);
}

/**
 * The approvals this run **parked** (issue #880).
 *
 * A receipt, and the wording everywhere this feeds must follow from that:
 * "parked N approvals", never "waiting on N". Nothing comes back to decrement
 * this once the operator approves a card, so a "still waiting" phrasing becomes
 * a fresh lie the moment they do — and the Approvals page, which IS live, is
 * where that question belongs.
 */
export function parkedApprovalCount(run: WorkflowRunOutcome): number {
  return run.approvals?.length ?? 0;
}

/**
 * The approvals this run parked that are actually sitting on the Approvals
 * page right now (issue #900) — `outcome === "parked"` only.
 *
 * {@link parkedApprovalCount} deliberately folds in the parks that failed and
 * the calls that were discarded, because those are "the rows that matter
 * most: nobody will ever be asked about those calls" (see
 * `WorkflowRunOutcome.approvals` in `api/workflows.ts`). That is the right
 * receipt for "what happened to this run's gated calls" — but wrong for any
 * copy that tells the operator something is waiting to be decided. A run
 * whose every park failed has `parkedApprovalCount` of 1 and zero cards on
 * Approvals; telling the operator to "decide it in Approvals" would send them
 * to an empty page. Use this count wherever the sentence claims a card
 * exists.
 */
export function decidableApprovalCount(run: WorkflowRunOutcome): number {
  return (run.approvals ?? []).filter((a) => a.outcome === "parked").length;
}

/**
 * Whether this run stopped short because a step is waiting on a person (issue
 * #881).
 *
 * Its own reading rather than a fold into {@link runTone}'s chain, because a
 * blocked run is the shape that fooled every existing check: it carries no
 * `error`, it is not `cancelled`, it is not `running`, and it routed no report
 * — so before this it fell through every arm to the green "ok" and told the
 * operator that a pipeline which delivered nothing had succeeded.
 */
export function isBlocked(run: WorkflowRunOutcome): boolean {
  return (run.blockedNodes?.length ?? 0) > 0;
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

/** The status dot for a whole run.
 *
 * Every arm returns one of the console's five run states, so a dot here means
 * exactly what the same dot means on the task board and in the runs table:
 * running, blocked on a human, done, failed, or idle. See
 * docs/design-system/color.md.
 *
 * **A run still in flight is checked FIRST**, ahead of every terminal reading.
 * A running run has no `error`, no `cancelled` and no deliveries yet, so
 * without this arm it falls all the way through to the green "ok" — and every
 * caller that trusts this function (the last-run chip, the history rows, the
 * cards) paints a run that has not finished as one that succeeded. That is a
 * claim the host has not made.
 *
 * The label is not decoration: it ships with the dot at every call site,
 * because roughly 1 in 12 men cannot separate the red/green pair and a bare
 * dot puts the whole signal on hue.
 */
export function runTone(run: WorkflowRunOutcome): {
  dot: string;
  label: string;
} {
  if (isRunning(run))
    return { dot: "animate-pulse bg-status-running", label: "running" };
  if (run.error) return { dot: "bg-status-failed", label: "failed" };
  // Issue #383: checked before the delivery reads, and deliberately NOT red. A
  // stop somebody asked for is not a fault, and a cancelled run has no
  // deliveries to weigh anyway — so without this arm it would fall through to
  // the green "ok" and read as a clean success. Idle is the state for "nothing
  // is happening and nothing went wrong".
  if (run.cancelled) return { dot: "bg-status-idle", label: "stopped" };
  // Issue #881: checked BEFORE the delivery reads and before `awaitingCount`,
  // and deliberately not red. A blocked run stopped short of its work but
  // nothing about it failed, and its own delivery rows are empty — so without
  // this arm it fell through to the green "ok". The label says what happened to
  // the run, not what is owed: "blocked" is the state, and the count of what it
  // parked belongs on the row beneath.
  if (isBlocked(run)) return { dot: "bg-status-blocked", label: "blocked" };
  if (undeliveredCount(run.deliveries) > 0)
    return { dot: "bg-status-failed", label: "not delivered" };
  // Blocked, not running. This was the running colour, which said "the machine
  // is working on it" about the one state that means the opposite: it is
  // parked until a human decides. Amber is the colour that gets looked at.
  //
  // Issue #846: `awaitingCount`, not `pendingCount`. A run that paused at a gate
  // parked no report — it never reached an output node — so the delivery-only
  // read scored it green and the operator was told a run that did none of its
  // work had succeeded.
  if (awaitingCount(run) > 0)
    return { dot: "bg-status-blocked", label: "awaiting approval" };
  return { dot: "bg-status-done", label: "ok" };
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

/**
 * How long a run took, in milliseconds — `null` when the host recorded no
 * start (a row journaled before #371 carries only its finish, and a duration
 * measured from nothing would be the age of the epoch).
 *
 * Issue #1007: `startedAtMillis` has been on the wire since #371 and no
 * workflow surface read it, so every run reported *when* it happened and none
 * reported how long it took — the one number that tells a run that hung from
 * one that failed immediately.
 *
 * `now` is passed in rather than read here so a still-running row ticks against
 * the same clock as everything else on screen, and so this stays pure.
 */
export function runDuration(
  run: WorkflowRunOutcome,
  now: number = Date.now(),
): number | null {
  if (run.startedAtMillis == null) return null;
  // A run in flight has no end yet, so the honest reading is "so far".
  const end = isRunning(run) ? now : run.atMillis;
  const ms = end - run.startedAtMillis;
  // Host clock and browser clock are different clocks, so a live row can come
  // out negative for the first second or two. Report nothing rather than "-1s".
  return ms >= 0 ? ms : null;
}

/** A duration in the console's compact form: `840ms`, `12.4s`, `3m 07s`. */
export function formatDuration(ms: number): string {
  if (ms < 1000) return `${Math.round(ms)}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  // Rounded to whole seconds FIRST, so the minute and the second can never
  // disagree — `${Math.floor(ms / 60_000)}m ${Math.round(…)}s` renders "3m 60s"
  // for anything within half a second of the minute.
  const total = Math.round(ms / 1000);
  return `${Math.floor(total / 60)}m ${String(total % 60).padStart(2, "0")}s`;
}
