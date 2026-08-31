// The run-records API (#242): one *attempt* at a task, and its persisted step
// trace. Mirrors `tasks.ts` — plain functions over `OpenCompanyClient`, the
// company scope resolved by `client.scopeFor`, and the host's wire shapes
// re-transcribed here so `tsc` pins the contract.
//
// The host answers these from an indexed store read, not a journal fold, so the
// attempts list stays cheap however long a company's history gets.
//
// **Read-only on purpose.** There is no write here at all: a run is minted,
// begun and settled by the dispatch path. The console inventing an attempt, or
// re-statusing one, would fabricate a record of work that never ran — the same
// reason `artifacts.ts` declines to wire the host's create/delete routes.

import type { OpenCompanyClient } from "./client";
import type { TimelineEntry } from "./tasks";

/**
 * Where one attempt stands.
 *
 * The *parked* statuses are separated by *who unblocks the work* (epic #183):
 * `waiting_approval` means a **person** must decide; `blocked` means a person
 * must **answer** (issue #1861 — a rejected model id, an expired credential, a
 * missing prerequisite, an agent's own question); `paused` means anything else
 * must — a dependency, a rate limit, a retry, an operator's pause.
 *
 * `waiting_approval` and `blocked` are both "waiting on you" and still must not
 * be collapsed: an approval has an effect behind it that approving performs,
 * while a blocker has only a question to answer. The console offers different
 * controls for the two, so it has to be able to tell them apart.
 *
 * `paused` is currently unreachable from a delegation, so nothing in the UI may
 * *depend* on seeing it — but it is handled everywhere a status is, because a
 * status the console cannot render is worse than one it never meets.
 *
 * `declined` is terminal and means the workflow compiler declined *by design* to
 * automate the work ("better done once than built into a workflow", issue
 * #1809) — neither an error nor an ordinary success, so it must never render in
 * a failure tone.
 */
export type RunStatus =
  | "pending"
  | "running"
  | "waiting_approval"
  | "paused"
  | "blocked"
  | "succeeded"
  | "failed"
  | "cancelled"
  | "declined";

/**
 * The coarse phase of a {@link RunStatus}, projected by the host.
 *
 * **Use this, never a timestamp, to decide whether an attempt is live.**
 * `finishedAtMillis` is absent for a *parked* attempt exactly as it is for a
 * running one — a parked run can still resume, so stamping it would make a
 * resumable attempt read as over. Keying liveness off the missing timestamp is
 * how an attempt waiting on a human ends up rendered as running forever.
 *
 * The host derives it from the state machine that owns the statuses, so a
 * status added later lands in the right bucket without a console change.
 */
export type RunPhase = "active" | "parked" | "terminal";

/** Tokens and cost attributed to an attempt, folded across its turns. */
export interface RunUsage {
  input: number;
  output: number;
  cachedInput: number;
  /** Zero when the path reports tokens but bills elsewhere (managed passthrough). */
  costUsd: number;
}

/** One recorded attempt at a task. */
export interface RunSummary {
  /** Stable id for the attempt within the company. */
  id: string;
  /**
   * The card this is an attempt at.
   *
   * **Absent, never null**, when the attempt is at no card — an operator chat
   * turn (issue #983), which the run store now records so a turn in flight is
   * queryable. Test for it by presence.
   */
  taskId?: string;
  /**
   * The conversation this attempt belongs to, when one raised it (issue #983).
   * Absent for a card dispatch, which is reachable through its card instead.
   *
   * This is what lets a reloading console match an open turn back to the thread
   * whose working indicator it should re-arm.
   */
  chatId?: string;
  /** The desk/teammate it was dispatched to. */
  agentId: string;
  /** Which attempt at the card this is — **1-based**; the first run is `1`. */
  attempt: number;
  status: RunStatus;
  /** `active` | `parked` | `terminal`. See {@link RunPhase} before using it. */
  phase: RunPhase;
  /**
   * The event-log sequence of the dispatch that drove this attempt. Absent
   * while the run is still `pending` — the row is written *before* the event,
   * on purpose, so a crash in that gap is visible instead of silent.
   */
  triggerEventSeq?: number;
  /** Epoch-millis the row was minted. */
  createdAtMillis: number;
  /** Epoch-millis the cycle began. Absent while `pending`. */
  startedAtMillis?: number;
  /** Epoch-millis the attempt reached a **terminal** status. See {@link RunPhase}. */
  finishedAtMillis?: number;
  /**
   * Why the attempt failed, in plain language.
   *
   * One value is worth expecting rather than treating as corruption: an attempt
   * whose start never landed stays `pending`, cannot legally be parked, and is
   * then closed `failed` by the dispatch cycle's backstop. Rare, but it is the
   * honest record of a dispatch that died in the gap — render it, don't hide it.
   */
  error?: string;
  /**
   * Tokens and cost, written on the settle — so a live attempt's figures are
   * **provisional** until `phase` leaves `active`.
   *
   * Deliberately not rendered on the Task Detail screen: epic #184 scopes that
   * screen with the cost/currency dimension removed (no per-line cost, no
   * total-cost header). It rides the wire for the usage surfaces that do own it.
   */
  usage: RunUsage;
  /**
   * The **high-water step ordinal actually persisted**, also written on the
   * settle. Once {@link stepCountCapped} is set this is no longer "how many
   * steps the agent took" — say so rather than implying otherwise.
   */
  stepCount: number;
  /** Whether the trace hit the per-attempt ceiling and is a prefix of the run. */
  stepCountCapped: boolean;
}

/**
 * One attempt with its full persisted trace.
 *
 * **Refresh-on-read.** Steps persist incrementally *as the turn executes*, so
 * re-reading a live attempt shows the progress made since the last read. There
 * is deliberately no live stream: that would mean widening the harness turn
 * stream for a surface a re-read already serves.
 */
export interface RunDetail {
  run: RunSummary;
  /**
   * The step trace, oldest first, in the same {@link TimelineEntry} shape the
   * Task Detail timeline renders — which is why the grouped-timeline renderer
   * is reused for it rather than reinvented.
   */
  steps: TimelineEntry[];
}

/** Which attempts to list. All selectors are optional. */
export interface RunQuery {
  /** Only attempts at this card. */
  task?: string;
  /**
   * Only attempts dispatched to this desk/teammate (issue #1573).
   *
   * Sent to the host rather than applied to the answer. Filtering a fetched
   * page here would make {@link limit} mean "the newest N attempts in the
   * *company*, of which some happen to be this desk's" — so a teammate who has
   * been quiet while the rest of the company was busy would render as one who
   * has never run at all.
   *
   * A host predating the selector ignores it and answers with the whole
   * company's attempts. That is why the caller must still not assume every row
   * it gets back is this desk's; see `AgentRuns`.
   */
  agent?: string;
  /** Only these statuses. An unknown word is refused with a 400, not silently dropped. */
  status?: RunStatus[];
  /** Cap the page; the host clamps it. */
  limit?: number;
}

function queryString(query: RunQuery): string {
  const params = new URLSearchParams();
  if (query.task) params.set("task", query.task);
  if (query.agent) params.set("agent", query.agent);
  if (query.status?.length) params.set("status", query.status.join(","));
  if (query.limit !== undefined) params.set("limit", String(query.limit));
  const encoded = params.toString();
  return encoded ? `?${encoded}` : "";
}

/** The company's recorded attempts, **newest first**. */
export function listRuns(
  client: OpenCompanyClient,
  company: string | null,
  query: RunQuery = {},
): Promise<RunSummary[]> {
  return client.get<RunSummary[]>(`${client.scopeFor(company)}/runs${queryString(query)}`);
}

/** One attempt and its persisted step trace. 404s when the id names no attempt. */
export function getRun(
  client: OpenCompanyClient,
  company: string | null,
  id: string,
): Promise<RunDetail> {
  return client.get<RunDetail>(`${client.scopeFor(company)}/runs/${encodeURIComponent(id)}`);
}

// ---------------------------------------------------------------------------
// Shared presentation helpers
// ---------------------------------------------------------------------------

/**
 * A short, human word for a status.
 *
 * `waiting_approval` reads "Waiting on you" rather than "In review" (issue
 * #465). The old wording came from the board column such an attempt used to
 * land its card in; since that card now parks instead, "In review" here would
 * both contradict the column beside it and repeat the claim the issue was
 * about — that there is a result to look at, when the attempt stopped at a call
 * it was not allowed to make. This is the wording the approvals tab and the
 * step timeline's waiting band already use, and it is where the "who unblocks
 * it" distinction now lives.
 */
export const RUN_STATUS_LABEL: Record<RunStatus, string> = {
  pending: "Queued",
  running: "Running",
  waiting_approval: "Waiting on you",
  paused: "Paused",
  blocked: "Waiting on your answer",
  succeeded: "Succeeded",
  failed: "Failed",
  cancelled: "Cancelled",
  declined: "Declined",
};

/**
 * How long an attempt has been going, in millis, or `null` when it has not
 * started.
 *
 * The one place liveness is decided, so it cannot be got wrong twice: a
 * **terminal** attempt measures to its finish time; anything else — running
 * *or* parked — measures to `now`, because a parked attempt is still an open
 * attempt whose clock has not stopped. `now` is the caller's, so the span is
 * clamped at zero against clock skew.
 */
export function runElapsedMillis(run: RunSummary, now: number): number | null {
  if (run.startedAtMillis === undefined) return null;
  const end = run.phase === "terminal" ? (run.finishedAtMillis ?? now) : now;
  return Math.max(0, end - run.startedAtMillis);
}

/** Whether an attempt's clock is still ticking — i.e. it has not settled. */
export function isRunOpen(run: RunSummary): boolean {
  return run.phase !== "terminal";
}
