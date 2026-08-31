// The live task-board API: the console's Kanban reads and writes real cards
// through the host's `…/tasks` routes (REST, camelCase over the wire). Replaces
// the client-side illustrative data the console shipped before it had one.

import type { OpenCompanyClient } from "./client";
import type { ArtifactKind } from "./artifacts";
import type { RunSummary } from "./runs";
import type { TurnStepFailure } from "./types";

/**
 * One deliverable a run published, pinned at the revision **that run** wrote
 * (issue #339).
 *
 * The version is part of the link on purpose. Artifact versions are
 * append-only and an operator edit appends a new one, so a link naming only the
 * record would quietly re-point at whatever a human last wrote — turning "what
 * this task produced" into "what the artifact says now". Pinned, it can never
 * dangle, and the console can compare it against the latest to say a human has
 * edited it since.
 */
export interface TaskOutputArtifact {
  artifactId: string;
  /** The 1-based revision this attempt wrote. */
  version: number;
  /** Display title, copied so the card can label its link without a read. */
  title: string;
  kind: ArtifactKind;
}

/** Whether the attempt executed a workflow or authored one. */
export type TaskOutputAction = "ran" | "created";

/**
 * A workflow the attempt ran or built (issue #339).
 *
 * Deliberately **not** pinned to a graph revision, unlike an artifact: workflow
 * graphs mutate in place and there is no version history to pin to. The link
 * opens the workflow as it is now, and `runId` — present only when the attempt
 * actually executed it — opens the overlay showing what really ran.
 *
 * Only ever present on an orchestrator-desk card: `run_workflow` and
 * `create_workflow` are orchestrator-only tools.
 */
export interface TaskOutputWorkflow {
  workflowId: string;
  runId?: string;
  action: TaskOutputAction;
}

/**
 * What produced a {@link TaskOutput} — and what its link of last resort points
 * at (issue #806).
 *
 * A run is *an* addressable producer, not the only one. An operator chat turn
 * produces things too, and run records stay reserved for actual work attempts,
 * so such a turn has no run to name. This is a union rather than an optional
 * `runId` so the console can label what is at the other end instead of calling
 * a conversation an attempt.
 *
 * Discriminate with `"runId" in source` — the host flattens these keys onto the
 * output, so a run-sourced record is byte-identical to what this type carried
 * before the union existed.
 */
export type TaskOutputSource =
  | {
      /** The attempt that produced it. */
      runId: string;
      /** Its 1-based ordinal, for the label. Absent if unreadable. */
      attempt?: number;
    }
  | {
      /** The conversation whose turn produced it. */
      chatId: string;
    };

/**
 * What a card's latest **successful** attempt produced (issue #339, epic #183
 * §6) — the link that turns a finished card into something you can open.
 *
 * **A last-resort link is unconditional, and that is the whole design**: an
 * output with no artifacts and no workflows is not an absence, it is the *trace
 * case*. Plenty of tasks produce no file — a message sent, a record updated, a
 * question answered — and for those the producer's own record **is** the
 * deliverable. It is also the fallback when an artifact is later deleted.
 *
 * The guarantee is "there is always something to open", not "there is always a
 * `runId`" (issue #806) — see {@link TaskOutputSource}.
 *
 * Absent entirely on a card that has never succeeded, one moved to Done by
 * hand, and every card settled before this shipped. Those link to the card
 * itself rather than to a synthesized output.
 */
export type TaskOutput = TaskOutputSource & {
  /** Epoch-millis the stamp was written. */
  atMillis: number;
  artifacts?: TaskOutputArtifact[];
  workflows?: TaskOutputWorkflow[];
};

/**
 * What surface a plan's prerequisite is checked against (issue #337).
 *
 * The model proposes the kind; the host decides what to do with it. A kind this
 * host cannot check arrives as `other` and always carries `unknown`.
 */
export type PrereqKind =
  | "connection"
  | "composio"
  | "mcp"
  | "credential"
  | "file"
  | "permission"
  | "assignee"
  | "other";

/**
 * The **host's** verdict on a prerequisite — never the model's claim.
 *
 * Only `missing` blocks a card from dispatching. `needsApproval` means the tool
 * is there but the call will stop for a person, and `unknown` means an
 * inventory could not be reached, which is deliberately not treated as absent:
 * an outage must not make every card unplannable.
 */
export type PrereqStatus = "satisfied" | "missing" | "needsApproval" | "unknown";

/** One thing the work needs first, with the host's verdict on it. */
export interface Prerequisite {
  kind: PrereqKind;
  /** A provider slug, MCP server name, workspace path, tool namespace or id. */
  name: string;
  status: PrereqStatus;
  /** One line a person can act on. */
  note: string;
}

/**
 * One step of a plan.
 *
 * The estimates are the model's guesses and must be rendered as guesses.
 * Nothing on either side of the wire budgets from them — the real caps are the
 * teammate's daily limit and the capability tier, both enforced host-side from
 * live meter reads.
 */
export interface PlanStep {
  title: string;
  detail: string;
  estimatedCostUsd?: number;
  estimatedMinutes?: number;
}

/**
 * The brief a planning pass wrote for a card (issue #337).
 *
 * Read-only on this side: the console never posts a plan, and the host's create
 * body has no field for one, so a prerequisite verdict cannot be forged from a
 * browser. Re-planning replaces the whole object; the card's note keeps the
 * history of both passes.
 */
export interface TaskPlan {
  description: string;
  steps: PlanStep[];
  prerequisites: Prerequisite[];
  risks: string[];
  verification: string;
  scope: string;
  /**
   * The teammate the planner would hand it to. Applied to the card only when it
   * had no assignee — a plan never reassigns work a person routed.
   */
  proposedAssignee?: string;
  /**
   * The teammates that plausibly fit, when more than one did (issue #1106).
   *
   * Non-empty means the pass **declined to choose** and the card is waiting on
   * a person — it is never populated alongside `proposedAssignee`, which is set
   * only when exactly one candidate resolved. Absent on every plan written
   * before #1106, and on every unambiguous plan written since.
   */
  assigneeCandidates?: AssigneeCandidate[];
  plannedAtMillis: number;
}

/**
 * One teammate the planner thinks could take a card, and why (issue #1106).
 *
 * `id` is canonical — the host resolves it against the roster before storing
 * it, and drops anything the roster does not carry, so every candidate rendered
 * is one the assignee write boundary will accept.
 */
export interface AssigneeCandidate {
  /** A teammate id, or a desk id. Submitted verbatim, never resolved here. */
  id: string;
  /** One line on why this one fits. Model prose, shown to a person deciding. */
  reason: string;
}

/** A board card as the host returns it. */
/**
 * Whether a card produces a one-off result or a reusable workflow (issue #580).
 * The operator chooses explicitly — nothing infers it. Omitted on the wire when
 * `once` (the default), so a one-off card is byte-identical to a pre-#580 card;
 * treat an absent value as `"once"`.
 */
export type TaskDeliverable = "once" | "workflow";

/**
 * What the operator says one chat **message** is for (issue #1152).
 *
 * The two work words mean exactly what they mean on a card. `"chat"` is the
 * operator saying this message is not a request for work at all — the composer's
 * "Just chatting" — so no deterministic path opens a card for it.
 *
 * Deliberately **not** a widening of `TaskDeliverable`. That type is the card
 * field, shared by `CreateTask`, `TaskPatch` and `TaskDto`, and a card can never
 * *be* "not work": widening it would make `"chat"` assignable in every one of
 * those positions and put the compiler on the wrong side of the invariant. This
 * is message-scoped, and the host draws the same line (`MessageIntent` beside
 * `TaskDeliverable` in `src/ports/types.rs`).
 *
 * Sent only when it is not the default — see `OpenCompanyClient.chat`.
 */
export type MessageIntent = "chat" | TaskDeliverable;

/** A positive source-currency USD amount or an explicit role-redacted state. */
export interface TaskCost {
  amountUsd?: number;
  hidden?: boolean;
}

/**
 * A workflow the builder pass proposed for a `workflow`-deliverable card,
 * awaiting operator approval (issue #580).
 *
 * It is NOT a saved workflow: the graph reaches the workflow list only when a
 * person applies it (see {@link applyWorkflowProposal}). `ops` is the stored
 * `{ id, name, description, nodes, edges }` authoring payload the host rebuilds
 * and re-validates from — treated as opaque here.
 */
export interface TaskWorkflowProposal {
  /** One line on what the proposed workflow does — what a review panel leads with. */
  summary: string;
  /** The stored authoring graph. Opaque on the client; the host is its authority. */
  ops: unknown;
  /** Epoch-millis the builder pass produced it. */
  generatedAtMillis: number;
  /** The build attempt this proposal accounts for (issues #339 / #242). */
  runId: string;
}

export interface Task {
  id: string;
  title: string;
  note?: string;
  /**
   * The board's column: `pending`, `working` or `done` (issue #1512).
   *
   * Three, not six. The four states that used to sit between To-do and Done —
   * `planning`, `in_progress`, `paused`, `in_review` — all say the same thing
   * to a reader of a board, so they are one column now and {@link Task.stage}
   * carries which of them it actually is.
   */
  column: string;
  /**
   * Which kind of working, on a working card: `planning`, `in_progress`,
   * `paused` or `in_review`. Absent on a pending or done card.
   *
   * Read this — never `column` — for anything genuinely stage-specific: the
   * Resume button on a paused card, the review link on one waiting for a
   * verdict. Those reads are what used to force `column` to stay six-valued.
   */
  stage?: string;
  priority: string;
  /** The desk/teammate label that owns it (a roster agent id routes a turn). */
  assignee: string;
  updatedAt: number;
  /** Lifetime total, including descendants. Absent for a true zero. */
  cost?: TaskCost;
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
  /**
   * The workflow run whose agent node opened this card (issue #661), and the
   * graph it is a run of.
   *
   * Carried on the **board** read, not just task detail, because a card with no
   * parent and no origin chat is otherwise unexplained: with these two an
   * operator finding a card nobody opened can see the schedule that did. They
   * are stamped together by one call site on the host, so either both are
   * present or neither is — but they are typed independently, because a host
   * predating them sends neither and the console must make no claim then.
   *
   * `originRunId` is what deep-links the workflow canvas to *the run that
   * opened this card* rather than to the graph's current shape.
   */
  originRunId?: string;
  originWorkflowId?: string;
  /**
   * What this card's latest successful attempt produced (issue #339).
   *
   * Carried on the **board** read, not just task detail, because the link is
   * rendered on the card itself — a board that had to open every card to
   * discover what it produced would cost one read per card per poll.
   */
  output?: TaskOutput;
  /**
   * The brief the last planning pass wrote (issue #337). Absent for a card
   * nobody has planned, which is every card until it is dragged into Planning.
   */
  plan?: TaskPlan;
  /**
   * Whether the card produces a one-off result or a reusable workflow
   * (issue #580). Absent means `"once"` — the default and the historical
   * behaviour, so a one-off card's wire shape is unchanged.
   */
  deliverable?: TaskDeliverable;
  /**
   * The workflow the builder pass proposed, awaiting approval (issue #580).
   * Present only while a `workflow` card sits In Review with a built proposal;
   * absent otherwise. Apply or reject it with {@link applyWorkflowProposal} /
   * {@link rejectWorkflowProposal}.
   */
  workflowProposal?: TaskWorkflowProposal;
  /**
   * Why a failed or cancelled run returned this card to `todo` (issue #1865).
   *
   * Absent for every card that has never bounced — a fresh card, one dragged
   * to `todo` by an operator, or one re-dispatched since its last bounce
   * (cleared on the `todo` → `in_progress` transition, so a retry does not go
   * on advertising the reason its last attempt failed). This is the one field
   * that tells a bounced card apart from a fresh one without opening it.
   */
  bounced?: string;
}

/** The create body; the host defaults column→`pending`, priority→`medium`. */
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
   * Note what is deliberately NOT sent alongside it: `column`. Entering
   * Working is what spends money, so the server's intake default decides where
   * a chat-created card lands and a **human drag** stays the only way to start
   * spending on it.
   */
  originChatId?: string;
  /**
   * The operator's explicit once-vs-workflow choice (issue #580, decision D2a).
   * Omitting it means `"once"`. A `"workflow"` card lands in Pending like any
   * other; the builder pass fires only when it is dragged into Working.
   */
  deliverable?: TaskDeliverable;
}

/**
 * A partial update; any omitted field is left as-is.
 *
 * A drag sends `{column}` — one of the three phase words. The host resolves it
 * to the stage a drop actually means, so `working` becomes `in_progress` and
 * dispatches. There is no way to write a stage from here, and there should not
 * be: the three states are the vocabulary (issue #1512).
 */
export interface PatchTask {
  title?: string;
  note?: string;
  column?: string;
  priority?: string;
  assignee?: string;
  /**
   * Flip the once-vs-workflow choice (issue #580). Omitting it leaves the choice
   * untouched — so an operator can flip a Pending card to `"workflow"` before
   * dragging it into Working.
   */
  deliverable?: TaskDeliverable;
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
 *
 * `awaiting_approval` (#411) is the other non-failure: the call was gated and
 * is waiting on a person. Use {@link isFailedStep} rather than testing for
 * "not ok", which would sweep both of these in as failures.
 */
export type StepStatus = "ok" | "error" | "running" | "awaiting_approval";

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
  /**
   * Optional scrubbed detail; expands under the row when present. On a run
   * step this is **what the call was doing** — its arguments, redacted
   * host-side and bounded (#411).
   */
  detail?: string;
  /** Stable key for a durable cost row that is not a journal event. */
  costKey?: string;
  /** Cost incurred by this line. Absent for a true zero. */
  cost?: TaskCost;
  /**
   * On a run step: **what came back** — a shape summary, an intrinsic tool's
   * own message, or a failure's plain-language cause (#411). Absent on
   * task-timeline entries, which have no such notion.
   */
  result?: string;
  /** On a run step: the typed reason it did not succeed (#411). */
  failure?: TurnStepFailure;
  /** On a run step: the result was cut before the agent read all of it (#410). */
  truncated?: boolean;
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
 * What became of an approval (#333). `pending` is still waiting on a human.
 *
 * `expired` was declared here from the start and was **unreachable** until
 * #971: nothing swept approvals for a company without a manifest schedule, so
 * no approval ever aged out and no surface ever needed to render this. It is
 * reachable now. Render it through `approvalStatusLabel` in `lib/language` —
 * an operator must be able to tell the no they made from the one the deadline
 * made, and neither should ever appear as a raw identifier.
 */
export type TaskApprovalStatus = "pending" | "approved" | "denied" | "expired";

/**
 * One approval that belongs to this task (#333).
 *
 * Distinct from an `approval` {@link TimelineEntry}, which can only describe a
 * *resolution*: an approval still parked has no resolution event, so it cannot
 * reach the timeline at all, and it is the row that matters most — the card is
 * stopped behind it.
 *
 * **Three fields on purpose (#468).** This used to back an Approvals tab on the
 * task card. That tab is gone — approvals are decided in one place, and a
 * second half-surface beside it could not decide anything. All the card does
 * now is say *that* it is waiting and for how long, linking to the Approvals
 * page, so this type carries only what that line reads. `kind`,
 * `resolvedAtMillis` and `waitedMillis` went with the tab; the park to resolve
 * span is still on the `approval` {@link TimelineEntry}.
 */
export interface TaskApproval {
  /** The approval's id, the same one the Approvals page resolves against. */
  id: string;
  /**
   * Epoch-millis the effect parked. Rows are ordered by this, oldest first, and
   * the card measures "waiting for N" from the oldest pending one.
   */
  atMillis: number;
  status: TaskApprovalStatus;
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
  /**
   * The message text, exactly as posted — or the host's fixed "This message was
   * removed." once withdrawn (#358). The original text is never sent again on
   * any surface, so a console that ignores {@link redacted} still cannot render
   * it.
   */
  text: string;
  /**
   * Whether this message was withdrawn (#358).
   *
   * Sent only when true, so an ordinary row keeps exactly the shape it had
   * before. The substitution is server-side; this only lets the thread style a
   * withdrawn row as what it is instead of as something a person typed.
   */
  redacted?: boolean;
  /**
   * Who withdrew it, as the same kind of label as {@link author}. Present only
   * on a withdrawn row — a removal with nobody's name on it is one that can be
   * made quietly from a thread other people were reading.
   */
  redactedBy?: string;
}

/** A neighbouring card in the lineage, trimmed to what a link needs (#185). */
export interface LineageRef {
  id: string;
  title: string;
  column: string;
  /** This child's lifetime total, including its descendants. */
  cost?: TaskCost;
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
   * This task's own approvals, oldest first, still-parked ones included (#333).
   *
   * The Approvals tab reads this rather than filtering the timeline: the old
   * filter could only surface resolutions that happened to fall inside the run
   * window, so a card with an approval waiting on a human read as having none.
   */
  approvals: TaskApproval[];
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
 * per-task timeline, this task's approvals (#333), discussion, and lineage into
 * one response. 404s when the id names no card.
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
 * Withdraw a posted discussion message (#358).
 *
 * Answers `200` with the row as every reader now sees it: same `seq`, same
 * author, same time, `redacted: true`, and the host's placeholder where the
 * text was. The message is not deleted — the journal is append-only, so this
 * appends a tombstone the read fold and the bundle writer both honour, which is
 * what stops the text being served *and* stops it shipping in an export.
 *
 * Any member of the company may: the same authority that can delete the whole
 * card, and the withdrawal is attributed. A `seq` that is not a discussion post
 * on this card is a `404`; withdrawing twice is a no-op success.
 */
export function redactTaskDiscussion(
  client: OpenCompanyClient,
  company: string | null,
  id: string,
  seq: number,
): Promise<DiscussionMessage> {
  return client.del<DiscussionMessage>(
    `${client.scopeFor(company)}/tasks/${encodeURIComponent(id)}/discussion/${encodeURIComponent(seq)}`,
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

/**
 * Approve the workflow the builder pass proposed for a card sitting In Review
 * (issue #580). The host rebuilds the graph from the STORED proposal, runs it
 * through the one workflow-authoring path (full validation, #276 create-disarm
 * for a scheduled graph), links the card to the created workflow, and returns
 * the card moved to Done. A refused create (roster drift, a name taken since)
 * rejects with a 400 and leaves the card In Review with its proposal intact.
 */
export function applyWorkflowProposal(
  client: OpenCompanyClient,
  company: string | null,
  id: string,
): Promise<Task> {
  return client.post<Task>(
    `${client.scopeFor(company)}/tasks/${encodeURIComponent(id)}/workflow-proposal/apply`,
    {},
  );
}

/**
 * Reject the proposed workflow (issue #580, decision D2c): clears the proposal
 * and returns the card to To-do. The card keeps its `workflow` deliverable, so
 * dragging it back into In Progress builds again. Returns the updated card.
 */
export function rejectWorkflowProposal(
  client: OpenCompanyClient,
  company: string | null,
  id: string,
): Promise<Task> {
  return client.post<Task>(
    `${client.scopeFor(company)}/tasks/${encodeURIComponent(id)}/workflow-proposal/reject`,
    {},
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

/**
 * The live board state Chat needs for a card-linked background turn (#1758).
 *
 * `column` is the task's stage when the current three-column API provides one,
 * otherwise its column. `startedAt` is present only while the task appears in
 * the in-flight read, so it supplies the elapsed clock and wins a brief race
 * where the board has already moved but the run has not disappeared yet.
 */
export interface TaskStatus {
  column: string;
  startedAt?: number;
}

/** Task id -> status, merged from the board and in-flight reads (#1758). */
export function taskStatusesById(
  tasks: readonly Task[],
  inflight: readonly InflightRun[],
): Record<string, TaskStatus> {
  const statuses: Record<string, TaskStatus> = {};

  for (const task of tasks) {
    statuses[task.id] = { column: task.stage ?? task.column };
  }

  for (const run of inflight) {
    if (!run.taskId) continue;
    statuses[run.taskId] = {
      column: statuses[run.taskId]?.column ?? "in_progress",
      startedAt: run.startedAt,
    };
  }

  return statuses;
}

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
