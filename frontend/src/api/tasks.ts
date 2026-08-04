// The live task-board API: the console's Kanban reads and writes real cards
// through the host's `…/tasks` routes (REST, camelCase over the wire). Replaces
// the client-side `tasks-sample` illustrative data.

import type { OpenCompanyClient } from "./client";
import type { RunSummary } from "./runs";

/** A board card as the host returns it. */
export interface Task {
  id: string;
  title: string;
  note?: string;
  column: string;
  priority: string;
  /** The desk/teammate label that owns it (a roster agent id routes a turn). */
  assignee: string;
  updatedAt: number;
  /**
   * The card this one was spawned from (#185), when it has a parent. Omitted on
   * a lineage root — every card the board creates today — so the board's wire
   * shape is unchanged.
   */
  parentTaskId?: string;
  /**
   * The chat thread this card was opened from (issue #246), when it came from
   * a conversation rather than the board's `+` button. Omitted otherwise, which
   * is every card created before this shipped.
   */
  originChatId?: string;
}

/** The create body; the host defaults column→`todo`, priority→`medium`. */
export interface CreateTask {
  title: string;
  note?: string;
  column?: string;
  priority?: string;
  assignee?: string;
  /**
   * The chat thread this card is being opened from (issue #246). Set by the
   * transcript's "Add to board" action; the board's `+` button omits it.
   *
   * Note what is deliberately NOT sent alongside it: `column`. Dropping a card
   * into `in_progress` is what dispatches an agent turn — it spends money — so
   * the server's intake default decides where a chat-created card lands, and
   * the human drag stays the only spend gate.
   */
  originChatId?: string;
}

/** A partial update; any omitted field is left as-is. A drag sends `{column}`. */
export interface PatchTask {
  title?: string;
  note?: string;
  column?: string;
  priority?: string;
  assignee?: string;
}

export function listTasks(
  client: OpenCompanyClient,
  company: string | null,
): Promise<Task[]> {
  return client.get<Task[]>(`${client.scopeFor(company)}/tasks`);
}

/**
 * A stable wire word for what a {@link TimelineEntry} records (#185). The host
 * emits exactly this set today; re-transcribed here so `tsc` pins the contract.
 *
 * Widened **additively** by #242 with the three step kinds a run's trace
 * produces (`tool_call` | `thinking` | `note`). A task timeline never emits
 * those and a run trace never emits the journal's five, but both arrive in this
 * one shape on purpose: the grouped-timeline renderer is then reused for the
 * run-detail drawer rather than reinvented beside it.
 */
export type TimelineKind =
  | "dispatched"
  | "reply"
  | "tool_failed"
  | "approval"
  | "completed"
  | "tool_call"
  | "thinking"
  | "note";

/**
 * How a run-trace step ended (#242). Present only on entries that came from a
 * run's step trace; a journal-derived task-timeline entry has no such notion.
 *
 * `running` is a **real and expected** resting state of a persisted row, not a
 * glitch: the trace is written *as the turn executes*, so a host killed
 * mid-tool-call leaves that call recorded exactly as it stood. It means
 * in-flight-when-the-trace-stopped — render it as such, never as a failure.
 */
export type StepStatus = "ok" | "error" | "running";

/**
 * One entry on a task's timeline (#185) — the same scrubbed vocabulary the host
 * uses for a chat bubble's steps. `detail`, when present, is a value the
 * producing event already scrubbed at source; nothing here carries raw tool
 * arguments, output, or call ids.
 */
export interface TimelineEntry {
  /**
   * The stable render key, and the strict order.
   *
   * On a task timeline this is the company-wide journal sequence. On a run's
   * step trace (#242) it is the **run-scoped** step ordinal, 0-based and dense
   * — so two different runs both have a step `0`. Never compare a `seq` across
   * the two surfaces.
   */
  seq: number;
  /** Epoch-millis the event was journaled, or the step recorded. */
  atMillis: number;
  /** What happened. See {@link TimelineKind} for which surface emits which words. */
  kind: TimelineKind;
  /** A short, past-tense human label rendered verbatim. */
  label: string;
  /** Optional scrubbed detail; expands under the row when present. */
  detail?: string;
  /**
   * How a run-trace step ended (#242). Absent on task-timeline entries, which
   * have no such notion — so `undefined` means "not a step", never "unknown
   * outcome".
   */
  status?: StepStatus;
  /**
   * How long a run-trace step took (#242), when known. Tool calls report it;
   * thinking and note steps do not.
   */
  elapsedMs?: number;
  /**
   * On an `approval` entry: how long the company sat waiting on the operator
   * before this resolution landed (#305), clamped to the run window.
   *
   * Absent — never `0` — when the host cannot recover the park instant, so the
   * console degrades to rendering the row with no waiting band rather than
   * claiming an instant sign-off that never happened.
   */
  waitedMillis?: number;
}

/**
 * One irreversible effect a task already executed (#351).
 *
 * Read by the host from the runtime journal's executed record — what the
 * runtime committed to run — not from the timeline, which reports what an agent
 * said it did. The record is written before the effect is performed (that
 * ordering is what makes effects at-most-once) and the runtime never
 * re-attempts it, so an entry is something to assume happened rather than
 * something proven to have finished.
 *
 * `kind` is the dotted effect kind, the same vocabulary the Approvals page
 * receives; `effectDone` in `@/lib/language` turns it into the sentence a
 * person reads. There is deliberately no payload here, so a recipient or a
 * message body cannot reach the screen.
 */
export interface IrreversibleEffect {
  /** The dotted effect kind, e.g. `payment.send`. Never rendered raw. */
  kind: string;
  /** Epoch-millis the effect was committed. */
  atMillis: number;
  /** The USD amount involved, if any. */
  amountUsd?: number;
}

/**
 * One message in a task's discussion thread (#335).
 *
 * The thread is the card's own, not a filtered view of company chat: it is
 * journaled per task and served by the task-detail read, so a message posted
 * here belongs to this card and nowhere else.
 */
export interface DiscussionMessage {
  /** The journal sequence — the stable render key, and the thread's order. */
  seq: number;
  /** Epoch-millis the message was journaled. */
  atMillis: number;
  /**
   * Who posted, as a label: a roster display name (or an email's local part),
   * `someone` for a poster no longer on the roster, `operator` for a post made
   * with a machine credential. The host never sends a user id or an email here.
   */
  author: string;
  /** The message text, exactly as posted. */
  text: string;
}

/** A neighbouring card in the lineage, trimmed to what a link needs (#185). */
export interface LineageRef {
  id: string;
  title: string;
  column: string;
}

/** The parent/children view of a task (#185). */
export interface TaskLineage {
  /** The card this one was spawned from, when it has one. */
  parent?: LineageRef;
  /** Cards spawned from this one, oldest-updated first for a stable render. */
  children: LineageRef[];
}

/** The assembled Task Detail response (#185): one read for the whole screen. */
/**
 * The worked/waiting split (#305), computed by the host.
 *
 * Both totals used to be derived twice — here in the console and again in the
 * exported record — so the screen and an exported copy of the same task could
 * disagree about how long a person was waited on, with nothing failing when
 * they drifted. The host does the arithmetic once and both read it.
 *
 * A still-open span cannot be carried by a snapshot: `workedLive` / `waitingLive`
 * mark those, and a caller that wants a ticking figure adds `now - asOfMillis`
 * to the live half. That is exact — every closed span ends in the past, so past
 * `asOfMillis` only the open one grows.
 */
export interface TaskDurations {
  /** Milliseconds actively worked, as of `asOfMillis`. */
  workedMillis: number;
  /** A dispatch window is still open. */
  workedLive: boolean;
  /** Milliseconds spent waiting on a person, interval-merged. */
  waitingMillis: number;
  /** An approval is still parked. */
  waitingLive: boolean;
  /** The instant both totals were taken. */
  asOfMillis: number;
}

export interface TaskDetail {
  /** The card header — the same shape a board card carries. */
  task: Task;
  /** The per-task event stream, oldest first. */
  timeline: TimelineEntry[];
  /** The worked/waiting split, so the screen and an export cannot disagree. */
  durations: TaskDurations;
  /**
   * What this task already did that cannot be undone (#351), oldest first.
   * Empty for a task that only read, thought and replied.
   *
   * Retrying re-runs the work, so this list is the difference between a
   * one-click Retry and one that stops to say what already happened.
   */
  irreversibleEffects: IrreversibleEffect[];
  /**
   * Whether the company's journal holds executed history it cannot describe
   * (#351) — records written before descriptions existed.
   *
   * The qualifier on {@link irreversibleEffects}: an empty list means "this
   * card did nothing irreversible" only while this is `false`. When it is
   * `true`, Retry confirms regardless and says earlier activity cannot be
   * described, rather than presenting a gap as an all-clear.
   */
  historyIncomplete: boolean;
  /**
   * The card's discussion thread, oldest first (#335).
   *
   * Carried on this read rather than fetched by the tab, so it refreshes on the
   * screen's existing 4s poll: a message another operator posts appears here
   * without a reload. Empty for a card nobody has posted on.
   *
   * Only the newest page of the thread — the host caps it so a long discussion
   * is not re-serialized on every poll. Older messages come from the same read
   * with `discussionBefore`.
   */
  discussion: DiscussionMessage[];
  /**
   * Whether the thread continues before {@link discussion}'s oldest message —
   * i.e. whether "load earlier" has anything to load.
   */
  discussionHasMore: boolean;
  /** Parent and children. */
  lineage: TaskLineage;
  /**
   * The card's recorded attempts, newest first (#242).
   *
   * Empty is a legitimate answer, not an error: run records were not
   * backfilled, so a card dispatched before they existed genuinely has none —
   * synthesising attempts from old reply events would fabricate identity.
   */
  runs: RunSummary[];
  /**
   * Epoch-millis this task started waiting on an operator *right now* (#305),
   * or absent when nothing is currently parked for its run.
   *
   * A still-parked approval has no resolution event yet, so it cannot reach the
   * timeline — this is the only signal that the task is idle on a human at this
   * moment, which is the state the screen exists to expose.
   */
  waitingSince?: number;
}

/**
 * The Task Detail screen's single read (#185): assembles the card header, the
 * per-task timeline, the approvals trail (as `approval` timeline rows), and the
 * lineage into one response. 404s when the id names no card.
 *
 * `discussionBefore` walks *backwards* through the discussion: pass the `seq` of
 * the oldest message held and the response carries the page before it. Omitted
 * (the poll's shape) it returns the newest page — the thread is capped host-side
 * so a long one is not re-sent whole every 4s.
 */
export function getTaskDetail(
  client: OpenCompanyClient,
  company: string | null,
  id: string,
  discussionBefore?: number,
): Promise<TaskDetail> {
  const query =
    discussionBefore === undefined
      ? ""
      : `?discussionBefore=${encodeURIComponent(discussionBefore)}`;
  return client.get<TaskDetail>(
    `${client.scopeFor(company)}/tasks/${encodeURIComponent(id)}${query}`,
  );
}

/**
 * Post a message to a task's discussion thread (#335).
 *
 * Answers `201` with the stored message, so the caller can render it at once;
 * the next {@link getTaskDetail} poll returns the same message under the same
 * `seq`. Rejects an empty message with a `400` and an unknown card with a `404`;
 * a very long message is truncated by the host rather than refused.
 *
 * Posting runs no agent turn: this is a note on the card, not a way to ask for
 * work. Dispatching stays the board's column drag.
 */
export function postTaskDiscussion(
  client: OpenCompanyClient,
  company: string | null,
  id: string,
  text: string,
): Promise<DiscussionMessage> {
  return client.post<DiscussionMessage>(
    `${client.scopeFor(company)}/tasks/${encodeURIComponent(id)}/discussion`,
    { text },
  );
}

/**
 * The task's record as a self-contained HTML document (#352).
 *
 * The host renders it *and names it*, so the console's job is delivery only:
 * the text and the `Content-Disposition` filename come back together, and the
 * same document — under the same name — reaches a `curl -OJ` or a scheduled job.
 */
export function exportTaskRecord(
  client: OpenCompanyClient,
  company: string | null,
  id: string,
): Promise<{ text: string; filename?: string }> {
  return client.getDocument(
    `${client.scopeFor(company)}/tasks/${encodeURIComponent(id)}/export`,
  );
}

export function createTask(
  client: OpenCompanyClient,
  company: string | null,
  body: CreateTask,
): Promise<Task> {
  return client.post<Task>(`${client.scopeFor(company)}/tasks`, body);
}

export function patchTask(
  client: OpenCompanyClient,
  company: string | null,
  id: string,
  body: PatchTask,
): Promise<Task> {
  return client.patch<Task>(
    `${client.scopeFor(company)}/tasks/${encodeURIComponent(id)}`,
    body,
  );
}

export function deleteTask(
  client: OpenCompanyClient,
  company: string | null,
  id: string,
): Promise<void> {
  return client.del<void>(
    `${client.scopeFor(company)}/tasks/${encodeURIComponent(id)}`,
  );
}

/** A steer verb the operator can apply to an in-flight run (issue #111). */
export type SteerAction = "pause" | "cancel" | "redirect";

/**
 * One in-flight run the operator can steer (issue #111): a dispatched board
 * task (`kind: "task"`) or a sub-agent delegation (`kind: "delegation"`).
 * `key` is the steer identifier — for a board task it equals the task id.
 * `pendingAction` is non-null while a steer of that verb is already in flight
 * for this run, so the console can badge + disable its row.
 */
export interface InflightRun {
  taskId: string | null;
  key: string;
  kind: "task" | "delegation";
  title: string;
  agentId: string;
  startedAt: number;
  pendingAction: string | null;
}

/** The steer body. `redirect` requires `instruction`; `cancel` requires `confirm`. */
export interface SteerInput {
  action: SteerAction;
  instruction?: string;
  confirm?: boolean;
}

/** The runs currently in flight for a company, steerable from company chat. */
export function listInflight(
  client: OpenCompanyClient,
  company: string | null,
): Promise<InflightRun[]> {
  return client.get<InflightRun[]>(
    `${client.scopeFor(company)}/tasks/inflight`,
  );
}

/**
 * Steer an in-flight run by its `key`: pause it, cancel it (requires
 * `confirm: true`), or redirect it with a fresh `instruction`. The host answers
 * 202 with no body; callers should refetch {@link listInflight} afterwards.
 */
export function steerTask(
  client: OpenCompanyClient,
  company: string | null,
  key: string,
  body: SteerInput,
): Promise<void> {
  return client.post<void>(
    `${client.scopeFor(company)}/tasks/${encodeURIComponent(key)}/steer`,
    body,
  );
}
