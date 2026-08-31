// How a run reads at a glance: its terminal status, its delivery counts, and
// how long ago it happened.
//
// Extracted from `WorkflowsView.tsx` (issue #303) because the workflow cards now
// need the SAME reading the history rows and the last-run chip use. Two
// implementations of "is this run healthy?" would drift, and the card grid is
// precisely the surface where a wrong green dot is most costly — it is the one
// an operator scans instead of opening anything.

import type {
  DeliveryReport,
  WorkflowRunOutcome,
  WorkflowRunVerdict,
} from "@/api/workflows";

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

/** The `skipped` delivery status, widened for the same reason as
 * {@link PENDING_STATUS}: the host owns this union and may grow it. */
const SKIPPED_STATUS: string = "skipped";

/**
 * The two `skipped` reasons that do NOT count as a lost report (issue #981).
 *
 * `already-delivered` — an earlier run in this approval lineage sent it (issue
 * #438); approving a gate re-runs the graph from the trigger, so every upstream
 * `output` node is reached again and the report is already at its destination.
 * `dry-run` — a test run (issue #542) attempted nothing, on purpose.
 *
 * `no-destination-configured` is deliberately NOT here. That report was produced
 * and then lost with nothing accounting for it, which is exactly what issue #925
 * added the row to make visible; excusing it restores the silence issues #947
 * and #963 were filed about. See docs/modules/server/run-verdict.md.
 */
const ACCOUNTED_FOR_REASONS: readonly string[] = [
  "already-delivered",
  "dry-run",
];

/**
 * Whether THIS one report did not reach a destination and will not without a
 * change — the host's `is_undelivered`, mirrored (issue #981).
 *
 * Every count and every marker below folds this, so the run's dot, the drawer's
 * badge, the toast and the per-node marker cannot disagree about a single row.
 *
 * `reason` is optional on the type: a host predating issue #248 sends no reason
 * at all. An absent one **counts**, which is the safe direction — an unreadable
 * reason must not excuse a report from the number an operator acts on.
 */
export function isUndelivered(d: DeliveryReport): boolean {
  if (d.status === "sent" || d.status === PENDING_STATUS) return false;
  // Only a skip can be accounted for. A `failed` or `denied` row is a report
  // that was attempted and did not work, whatever it claims about why.
  if (d.status !== SKIPPED_STATUS) return true;
  return !ACCOUNTED_FOR_REASONS.includes(d.reason ?? "");
}

/** Reports that did NOT reach their destination **and will not without a
 * change** — the number worth acting on. A fold of {@link isUndelivered}. */
export function undeliveredCount(deliveries: DeliveryReport[]): number {
  return deliveries.filter(isUndelivered).length;
}

/**
 * The `output` nodes whose report did not go out (issue #981).
 *
 * The join that lets the console answer TWO questions about one node without
 * conflating them. `nodes[].status` says whether the engine ran the step — for a
 * dropped report the honest answer is `ok`, because delivery happens after the
 * engine returns — and this says whether its report reached anybody. Rendering
 * only the first is what put a green `DONE` on the output node of a run the same
 * screen called `undelivered`.
 *
 * Local, off `DeliveryReport.node` (the `output` node's id, on every delivery
 * row the host writes), so this needs **no new wire field** and adds no third
 * notion of "this run did not fully work" beyond the verdict itself.
 *
 * Takes the rows rather than a run: the settled `WorkflowRunResult` and the
 * history's `WorkflowRunOutcome` are different shapes that both carry them.
 */
export function undeliveredNodes(deliveries: DeliveryReport[]): Set<string> {
  return new Set(deliveries.filter(isUndelivered).map((d) => d.node));
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
  return liveApprovalCount(run) + pendingCount(run.deliveries);
}

/**
 * The gate nodes this run stopped at that STILL have a card in the queue
 * (issue #1189).
 *
 * `pendingApprovals` is a receipt of where the run stopped and cannot go stale
 * — but the question each entry points at can, and on one staging tenant 34 of
 * 60 runs were pointing at nothing. The host reconciles the two and reports the
 * difference as `strandedApprovals`; this is the subtraction, done once, so the
 * header chip, the drawer and the summary line cannot disagree about whether
 * anybody is being waited on.
 *
 * Clamped at 0. The two numbers come from the same host read, so the count can
 * never legitimately exceed the list — but a negative here would render as
 * "-1 awaiting approval", which is a worse failure than the one being fixed.
 */
export function liveApprovalCount(run: WorkflowRunOutcome): number {
  const pending = run.pendingApprovals?.length ?? 0;
  return Math.max(0, pending - strandedApprovalCount(run));
}

/**
 * The gate nodes this run stopped at with **no live card left** (issue #1189).
 *
 * Host-derived, never guessed: only the host can join a gate to the queue, and
 * a host predating #1189 sends no key at all — which reads as 0, i.e. "not
 * reconciled". That is the safe direction. Inventing a dead end would retire a
 * run an operator can still act on, which is strictly worse than the defect
 * this closes: an operator cannot argue with a run the console says is over.
 */
export function strandedApprovalCount(run: WorkflowRunOutcome): number {
  return run.strandedApprovals ?? 0;
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
 * The approvals this run parked that are STILL on the Approvals page (issue
 * #1189).
 *
 * {@link decidableApprovalCount} is live-blind: it reads the run's receipt, and
 * a receipt records that a card was opened, never that it is still open. That
 * was right for #900's question ("did the park land?") and is wrong for any
 * copy that tells the operator to go and decide something — which is how the
 * drawer came to say "Approve them in Approvals and this run continues on its
 * own" one line above "none of its approvals are in the queue any more".
 *
 * Subtracts the per-node `stranded` counts the host already computes (#1143).
 * Both numbers are in the same units — approval **ids** — because `stranded` is
 * a filter over `blockedNodes[].approvalIds` and the receipt rows are keyed by
 * the same ids.
 *
 * Clamped at 0 for the same reason {@link liveApprovalCount} is, plus one of
 * its own: the receipt deliberately excludes parks that never landed while
 * `stranded` counts only ids that did, so the two lists are not identical and
 * arithmetic across them must not be trusted to stay ordered.
 */
export function liveParkedApprovalCount(run: WorkflowRunOutcome): number {
  const stranded = (run.blockedNodes ?? []).reduce(
    (n, b) => n + (b.stranded ?? 0),
    0,
  );
  return Math.max(0, decidableApprovalCount(run) - stranded);
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

/**
 * Whether nothing in the queue is waiting on this run any more, so no decision
 * can move it (issue #1189).
 *
 * The host's word first, exactly as {@link verdictOf} takes it: the host is the
 * only reader that can join a gate against the live queue. The local fold is
 * the fallback for a host predating #1189 that nevertheless sent the per-node
 * `stranded` counts #1143 added — the `feature_pipeline` case — and it mirrors
 * the host's rule rather than inventing a second one: it stopped for somebody,
 * EVERY gate lost its card, and no report is parked either.
 *
 * A partly stranded run is deliberately NOT stranded. Something there really is
 * still decidable, and the per-node count is what carries the loss.
 */
export function isStranded(run: WorkflowRunOutcome): boolean {
  if (run.verdict) return run.verdict === "stranded";
  const pending = run.pendingApprovals?.length ?? 0;
  return (
    pending > 0 &&
    strandedApprovalCount(run) >= pending &&
    pendingCount(run.deliveries) === 0
  );
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

/**
 * Two of {@link runTone}'s labels, named because {@link runSummaryLine} has to
 * recognise them: they are the states whose *count* is a separate fact, and the
 * only ones where repeating the words would say the same thing twice.
 */
const NOT_DELIVERED = "not delivered";
const AWAITING_APPROVAL = "awaiting approval";

/**
 * The dot and the label for each verdict.
 *
 * The label is not decoration: it ships with the dot at every call site,
 * because roughly 1 in 12 men cannot separate the red/green pair and a bare dot
 * puts the whole signal on hue.
 *
 * Two pairs deliberately share a colour and differ only in the word.
 * `undelivered` is red like `failed` — a report that will not go out needs the
 * same attention as a break — but it says "not delivered", because the run
 * itself did not fail and telling an operator it did would send them at a graph
 * that was fine. `awaiting approval` shares amber with `blocked`: both are
 * waiting on a person, and neither is a fault. See docs/design-system/color.md.
 *
 * Exported (issue #1697) so the traces list's status filter can enumerate the
 * closed set of verdicts with the same dot and label every other reader uses,
 * rather than a second wording that could drift from this one.
 */
export const VERDICT_TONE: Record<WorkflowRunVerdict, { dot: string; label: string }> =
  {
    running: { dot: "animate-pulse bg-status-running", label: "running" },
    failed: { dot: "bg-status-failed", label: "failed" },
    // Issue #383: a stop somebody asked for is not a fault. Idle is the state
    // for "nothing is happening and nothing went wrong".
    stopped: { dot: "bg-status-idle", label: "stopped" },
    // Issue #1189: idle, NOT the amber it shares a shape with.
    //
    // The status vocabulary is a closed set of five (docs/design-system/color.md),
    // so this picks from them rather than adding a sixth. Amber is the console's
    // "needs your attention, nothing is broken" state — it is what `blocked` and
    // `awaiting approval` wear, and both of them mean "go and decide this". A
    // stranded run is the one state in which that is false: there is nothing to
    // decide and nothing is owed. Painting it amber would reprint, in colour,
    // the exact claim this issue was filed to remove.
    //
    // Idle is `stopped`'s token for "nothing is happening and nothing went
    // wrong", and this run is inert in the same way. It is deliberately not
    // `failed` either: the graph did not break, and — since approving a gate
    // spawns a NEW run with no link back — a run whose gates were all approved
    // is indistinguishable from one whose cards were lost. Red would be a claim
    // about which, and the console cannot tell.
    stranded: { dot: "bg-status-idle", label: "stranded" },
    // Issue #881: deliberately not red. A blocked run stopped short of its work
    // but nothing about it failed. The label says what happened to the run, not
    // what is owed; the count of what it parked belongs on the row beneath.
    blocked: { dot: "bg-status-blocked", label: "blocked" },
    undelivered: { dot: "bg-status-failed", label: NOT_DELIVERED },
    // Amber rather than the running colour, which said "the machine is working
    // on it" about the one state that means the opposite: it is parked until a
    // human decides.
    "awaiting-approval": {
      dot: "bg-status-blocked",
      label: AWAITING_APPROVAL,
    },
    // Issue #1865: amber, sharing `blocked`'s shape rather than `failed`'s red.
    // A node under `on_error: continue|route` errored and the graph kept
    // going past it — the author's own config asked for that branch to
    // survive, and it did, so this is "needs your attention, nothing is
    // broken" the same way `blocked`/`awaiting approval` are, not the red
    // that says the run itself came apart.
    degraded: { dot: "bg-status-blocked", label: "degraded" },
    ok: { dot: "bg-status-done", label: "ok" },
  };

/**
 * A run's verdict — **the host's when it sends one** (issue #981).
 *
 * The words and their precedence order are unchanged (bar #1189's
 * `stranded`); what changed is who
 * owns them. They lived only here, in this console's TypeScript, so every other
 * reader of the same run had to re-derive them — and the obvious derivation,
 * folding `nodes[].status`, is wrong for the one case that matters: delivery
 * runs *after* the engine returns, so a run whose report was refused reports
 * every node `ok`. The QA harness made exactly that mistake and scored a
 * dropped report as a pass.
 *
 * The fallback is not legacy tolerance for its own sake — it is the reading a
 * host predating #981 forces, and it is the same ladder the host now runs, kept
 * here so switching between hosts cannot change what a run means.
 */
export function verdictOf(run: WorkflowRunOutcome): WorkflowRunVerdict {
  // Checked against the map rather than trusted, because `verdict` is
  // host-controlled and a host is free to grow an eighth word this console has
  // never heard of — the same reason `DeliveryStatus` is compared through a
  // widened `string` further up. An unrecognised word falls through to the
  // ladder below, which reads the rows the host also sent.
  //
  // It deliberately does NOT fall back to "ok". Painting a word we cannot read
  // as a clean run is the exact failure this function exists to close, and it
  // would be the one arm nothing on screen could contradict.
  if (run.verdict && run.verdict in VERDICT_TONE) return run.verdict;
  if (isRunning(run)) return "running";
  if (run.error) return "failed";
  if (run.cancelled) return "stopped";
  // Issue #1189, above both readings that say "go and decide it".
  if (isStranded(run)) return "stranded";
  if (isBlocked(run)) return "blocked";
  if (undeliveredCount(run.deliveries) > 0) return "undelivered";
  if (awaitingCount(run) > 0) return "awaiting-approval";
  // Legacy hosts may omit the verdict while still sending node statuses. An
  // error node under continue/route is degraded, not a clean run.
  if (run.nodes?.some((node) => node.status === "error")) return "degraded";
  return "ok";
}

/** The status dot for a whole run.
 *
 * A lookup on {@link verdictOf} rather than a ladder of its own (issue #981).
 * The ladder moved to the host; this is the last step, turning the host's word
 * into a colour and a label. Every arm returns one of the console's five run
 * states, so a dot here means exactly what the same dot means on the task board
 * and in the runs table: running, blocked on a human, done, failed, or idle.
 * See docs/design-system/color.md.
 *
 * **A run still in flight reads `running` FIRST**, ahead of every terminal
 * reading, on the host and in the fallback alike. A running run has no `error`,
 * no `cancelled` and no deliveries yet, so without that precedence it falls all
 * the way through to the green "ok" — and every caller that trusts this
 * function (the last-run chip, the history rows, the cards) paints a run that
 * has not finished as one that succeeded. That is a claim the host has not
 * made.
 */
export function runTone(run: WorkflowRunOutcome): {
  dot: string;
  label: string;
} {
  return VERDICT_TONE[verdictOf(run)];
}

/**
 * How the most recent run went, as ONE sentence: what it did, where it failed if
 * it failed, and the counts a card carries as badges beside the words.
 *
 * For a fixed-width column (issue #1136: the workflow list's status column is a
 * fixed 13rem so the dots line up down the page). A badge cannot truncate — one
 * "2 not delivered" pill beside the label leaves the label four characters and
 * two pills leave it none — but a sentence can, and it truncates from the right,
 * which drops the counts before it drops the state. Callers hang the full string
 * on a `title`, so nothing is unrecoverable.
 *
 * `state` is {@link runTone}'s label, passed in rather than recomputed so the
 * words and the dot beside them can never come from two different readings.
 *
 * Nothing is lost against the card's badges: an undelivered report and a waiting
 * approval are already the run's *state* by the time `runTone` has spoken, so
 * what the badges add is the number — and that is what this appends.
 */
export function runSummaryLine(
  run: WorkflowRunOutcome,
  state: string,
  failedNode?: string | null,
): string {
  const undelivered = undeliveredCount(run.deliveries);
  const pending = pendingCount(run.deliveries);

  let head = `${run.scheduled ? "Scheduled" : "Manual"} run ${state}${
    failedNode ? ` at “${failedNode}”` : ""
  }`;
  // A count goes in brackets beside the words it counts when the state IS that
  // condition, and spells itself out when the state is something else. Joining
  // both cases the same way produces "not delivered · 2 not delivered" — the
  // card's badge has the same redundancy and can afford it; a column that
  // truncates cannot.
  const also: string[] = [];
  if (undelivered > 0) {
    if (state === NOT_DELIVERED) head += ` (${undelivered})`;
    else also.push(`${undelivered} ${NOT_DELIVERED}`);
  }
  if (pending > 0) {
    if (state === AWAITING_APPROVAL) head += ` (${pending})`;
    else also.push(`${pending} ${AWAITING_APPROVAL}`);
  }
  return [head, ...also].join(" · ");
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

/** A duration in the console's compact form: `840ms`, `12.4s`, `1h 03m 07s`. */
export function formatDuration(ms: number): string {
  if (ms < 1000) return `${Math.round(ms)}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  // Rounded to whole seconds FIRST, so the minute and the second can never
  // disagree — `${Math.floor(ms / 60_000)}m ${Math.round(…)}s` renders "3m 60s"
  // for anything within half a second of the minute.
  const total = Math.round(ms / 1000);
  const days = Math.floor(total / 86_400);
  const hours = Math.floor((total % 86_400) / 3_600);
  const minutes = Math.floor((total % 3_600) / 60);
  const seconds = total % 60;
  const tail = `${String(minutes).padStart(2, "0")}m ${String(seconds).padStart(2, "0")}s`;

  if (days > 0) {
    return `${days}d ${String(hours).padStart(2, "0")}h ${tail}`;
  }
  if (hours > 0) return `${hours}h ${tail}`;
  return `${minutes}m ${String(seconds).padStart(2, "0")}s`;
}
