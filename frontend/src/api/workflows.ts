// The live workflow API: the console's Workflows canvas reads the company's
// saved graphs through the host's `…/workflows` routes (REST, camelCase over
// the wire) and runs one via `…/workflows/{wid}/run`. Replaces the client-side
// `workflow-sample` illustrative data.

import type { ArtifactKind } from "./artifacts";
import type { OpenCompanyClient } from "./client";

/** A one-line workflow entry, as the picker lists it. */
export interface WorkflowSummary {
  id: string;
  name: string;
  description?: string;
  /**
   * The trigger node's 5-field UTC cron. `null` means the current host inspected
   * the graph and found no schedule; `undefined` means an older host did not
   * send this summary field, so the console must make no claim either way.
   */
  schedule?: string | null;
  /**
   * How many steps are in the graph. Absent on hosts predating the widened
   * summary response; `0` is a real count and must still render.
   */
  nodeCount?: number;
  /**
   * Whether this workflow can be edited or deleted through the API (issue
   * #259). `false` for a graph defined by a file in the company source tree,
   * and for a name-only entry with no saved graph at all — the host refuses
   * both with a 409, so the console greys the affordance out instead.
   *
   * **Optional on the type, not on the wire.** A host predating #259 sends no
   * such field, and `undefined` must not read as "not editable" — treat only an
   * explicit `false` as a refusal.
   */
  editable?: boolean;
  /**
   * Whether this workflow's schedule is armed (issue #276). `false` means the
   * graph is saved and still runnable by hand, but the scheduler skips it.
   *
   * Independent of {@link WorkflowSummary.editable}: a seed-defined workflow is
   * `editable: false` and still toggleable, because pausing writes to the
   * company record rather than to the source tree.
   *
   * **Optional on the type, not on the wire**, same rule as `editable`: a host
   * predating #276 sends no such field, and `undefined` must not render as
   * paused — treat only an explicit `false` as off.
   */
  enabled?: boolean;
}

/** A single graph node. `kind` is one of the tinyflows node kinds. */
export interface WorkflowNode {
  id: string;
  /**
   * `trigger` | `agent` | `tool_call` | `http_request` | `condition` |
   * `output` | `switch` | `merge` | `split_out` | `transform` |
   * `output_parser` | `sub_workflow`.
   */
  kind: string;
  name: string;
  summary?: string;
  /** The roster agent id — only present on `agent` nodes. */
  agent?: string;
  /**
   * A standard 5-field cron saying when the workflow starts on its own — only
   * present on `trigger` nodes, and always interpreted in **UTC** (issue #169).
   * Absent means the workflow only runs when something starts it (the Run
   * button, the run route, or another workflow).
   */
  schedule?: string;
  /** Kind-specific configuration (a slug, URL, case labels, schema, …). */
  config?: unknown;
  /** How the engine handles an error on this node, when set. */
  onError?: string;
  /** The node's retry policy, when set. */
  retry?: {
    maxAttempts?: number;
    backoffMs?: number;
    backoff?: string;
  };
  /** Whether the node pauses for a human approval before proceeding. */
  requiresApproval?: boolean;
  /**
   * `false` when a continuation must not repeat this node's call — it replays
   * the result the earlier run recorded instead (issue #850).
   *
   * Only meaningful on `tool_call` and `http_request`, the two kinds that make
   * a call. Absent is the default: the node repeats unless the host already
   * classifies its call as reaching outside the company.
   */
  repeatable?: boolean;
  /**
   * A deterministic postcondition (issue #1866): a mechanical predicate
   * checked against the node's output before it is allowed to flow
   * downstream — `require` is `"non_empty"` | `"field_present"` |
   * `"non_empty_list"`, `field` a dotted path into the output (required for
   * `field_present`, optional for `non_empty_list`).
   *
   * Only ever set through the write route, on `agent` nodes today. This
   * console has no control for it, so every read/write path here must carry
   * it through verbatim like `onError`/`retry`/`requiresApproval`/
   * `repeatable` — dropping it on an unrelated edit silently removes a
   * run-safety gate the operator declared (issue #1937 review).
   */
  postcondition?: {
    require: string;
    field?: string;
  };
  /** Where an `output` node's report goes when the run finishes. */
  destination?: WorkflowDestination;
}

/**
 * Where a terminal `output` node routes its report once the run completes.
 *
 * `owner` is resolved server-side from the company's admins and carries no
 * target — the graph names nobody, which is what keeps it safe by construction.
 * `email` names an address and only sends when the company grants `email` AND
 * the recipient has already written in. `channel` must name a channel this
 * company can deliver to — a desk chat or a connected channel, never the
 * operator surface (issue #981).
 */
export interface WorkflowDestination {
  kind: "owner" | "email" | "channel";
  /** Required for `email` (an address) and `channel` (an id); absent for `owner`. */
  target?: string;
}

/**
 * The destination kinds the creator's picker offers, with prosumer labels.
 *
 * Kept in step with the host's `WORKFLOW_DESTINATION_KINDS` by
 * `destination_kinds_match_the_console` in `src/company/workflow_file.rs`, which
 * reads the `value:` entries out of this very block — so adding a kind on one
 * side alone fails `cargo test` rather than shipping a picker the server rejects
 * (or withholding one it accepts). Issue #260.
 */
export const DESTINATION_KINDS: {
  value: WorkflowDestination["kind"];
  label: string;
}[] = [
  { value: "owner", label: "Owner — the company's admins" },
  { value: "email", label: "Email — a specific address" },
  { value: "channel", label: "Channel — a wired chat channel" },
];

/**
 * The collapsed-form label for a stored destination value.
 *
 * The output node's destination `Select` stores `node.destinationKind` — or the
 * `"__none__"` sentinel when unset, because a `Select` item cannot carry an
 * empty string. base-ui renders the raw stored value in the collapsed control
 * unless it is given explicit text, so without this the trigger showed a bare
 * `__none__` while the open list showed a friendly label. Maps the sentinel and
 * every {@link DESTINATION_KINDS} value to its prosumer label; an unrecognized
 * value falls back to itself. Issue #813.
 */
export function destinationLabel(value: string): string {
  if (value === "__none__" || value === "") {
    return "Nowhere (run result only)";
  }
  return DESTINATION_KINDS.find((kind) => kind.value === value)?.label ?? value;
}

/** A directed edge between two node ids, with an optional branch label. */
export interface WorkflowEdge {
  from: string;
  to: string;
  label?: string;
}

/** The full graph the canvas renders. */
export interface WorkflowGraph {
  id: string;
  name: string;
  description?: string;
  /**
   * The owning desk (issue #1862 prerequisite) — a desk id or name, resolved
   * against the company's wired desks host-side. `undefined` for a graph with
   * no owner (every graph saved before this field existed, or one an author
   * chose not to assign).
   *
   * **No control in this dialog edits it yet** — the create/edit form has no
   * field for it. It is carried on {@link GraphDraft} and round-tripped
   * verbatim by {@link assembleGraph} purely so a Save never clears it: a
   * `PUT` replaces the whole graph, so an edit that omitted this field here
   * would silently wipe whatever desk an operator (or the workflow-proposal
   * defaulting) had set (issue #1882 review).
   */
  ownerDesk?: string;
  nodes: WorkflowNode[];
  edges: WorkflowEdge[];
  /** See {@link WorkflowSummary.editable}. Same "only `false` means no" rule. */
  editable?: boolean;
  /** See {@link WorkflowSummary.enabled}. Same "only `false` means off" rule. */
  enabled?: boolean;
  /**
   * The opaque optimistic-concurrency token for this graph (issue #259). Always
   * serialized by a current host (issue #1013): a `string` when the graph is
   * `editable`, and `null` when it is not (a source-defined or body-less graph
   * has nothing to version). It used to be omitted for a non-editable graph,
   * which read back as `undefined` and let a caller send nothing — silently
   * overwriting a concurrent save; an explicit `null` is the honest "no token".
   *
   * **Echo it back, never parse it.** Pass it to {@link updateWorkflow},
   * {@link deleteWorkflow}, or {@link restoreWorkflowRevision} and the host
   * refuses the write with a 409 if the graph changed since this read — which is
   * what stops one console silently overwriting another's edit. A current host
   * now also refuses the write with a 400 if you send no token at all. `null`
   * from a non-editable graph is falsy and sends nothing, which the console never
   * does — those graphs are not editable.
   */
  version: string | null;
}

/**
 * What became of one attempt to deliver an output node's report.
 *
 * `pending` means the send was parked for operator approval (a cold email
 * recipient a workflow may not open a conversation with on its own). It is a
 * SNAPSHOT taken when the run finished: runs are not persisted, so nothing ever
 * comes back to flip the row to `sent`. The Approvals view is the live source of
 * truth — approving there actually sends the mail.
 */
export type DeliveryStatus =
  "sent" | "pending" | "skipped" | "denied" | "failed";

/**
 * One attempt to route a reached `output` node's report to its destination.
 *
 * A delivery failure never fails the run and never moves a node's status, so
 * these rows — and, since issue #981, the run's own
 * {@link WorkflowRunVerdict} — are the only places an operator learns a report
 * was not delivered. The rows are the *reason*; the verdict is the *reading*.
 * An output node the run never reached contributes no row at all.
 */
export interface DeliveryReport {
  /** The output node whose report this was. */
  node: string;
  /** The destination kind as authored. */
  kind: string;
  /** The address or channel actually addressed, when there was one. */
  target?: string;
  status: DeliveryStatus;
  /**
   * An operator-readable reason — populated even on success. This is the half
   * the drawer renders: it may quote the transport verbatim, which is what
   * makes a refused send fixable, and this console is a tenant surface.
   */
  detail: string;
  /**
   * The same outcome as a stable token out of a closed set (issue #248) —
   * `"mail-transport-refused"`, `"channel-not-wired"`, and so on.
   *
   * It exists because the host's own logs may not carry `detail`: on the
   * transport-failure arms `detail` interpolates the transport's reply, which
   * routinely quotes the recipient's address, and host stdout is a platform
   * surface rather than a tenant one. Nothing renders it today — `detail` is
   * strictly more informative here — so it is optional on the type (not the
   * wire), and a response from a host predating #248 still parses.
   */
  reason?: string;
}

/**
 * What a run adds up to, in one word — **the host's reading, not ours** (issue
 * #981).
 *
 * The console used to own the only definition of "did this run succeed", and a
 * definition living in one client is a definition every other client has to
 * guess at. The obvious guess is wrong: delivery happens after the engine
 * returns, so a run whose report was refused still reports every node `ok`, and
 * anything folding `nodes[].status` — the QA harness included — scored a
 * dropped report green.
 *
 * The words are unchanged from the ladder this console has always used, in the
 * same precedence order; only the place they are decided has moved. See
 * {@link runTone}.
 *
 * `stranded` is the one addition (issue #1189): every person the run stopped
 * for has nothing left to answer, so no decision can move it. It outranks
 * `blocked` and `awaiting-approval` because it contradicts them — both tell an
 * operator to go and decide something, and this is the state in which there is
 * nothing there.
 *
 * `degraded` is the newest addition (issue #1865): a node under
 * `on_error: continue|route` errored and the graph kept going past it, or an
 * agent node's turn truncated at the iteration cap. Checked LAST, immediately
 * above `ok` — every reading above it describes something more actionable, so
 * a run that is also failed, stopped, stranded, blocked, undelivered or
 * awaiting approval reports that instead.
 */
export type WorkflowRunVerdict =
  | "running"
  | "failed"
  | "stopped"
  | "stranded"
  | "blocked"
  | "undelivered"
  | "awaiting-approval"
  | "degraded"
  | "ok";

/** The result of a run: the engine's final state and any pending approvals. */
export interface WorkflowRunResult {
  /** The engine's final run state — a nested JSON payload. */
  output: unknown;
  /** Node ids left waiting on a human approval, if any. */
  pendingApprovals: string[];
  /**
   * One row per report-delivery attempt. Optional on the type (not the wire)
   * so a response from a host predating issue #170 still parses.
   */
  deliveries?: DeliveryReport[];
  /**
   * The run's correlation id (issue #371). Optional on the type so a response
   * from a host predating #371 still parses.
   *
   * The console needs it because the run's progress events arrive over SSE
   * *while this request is still in flight*: without it, a cron fire that
   * overlapped the manual run could not be told apart from the run being
   * awaited, and its frames would repaint the canvas.
   */
  runId?: string;
  /**
   * Per-node progress for this run, in the order the nodes finished (issue
   * #542) — the same structural rows {@link WorkflowRunOutcome.nodes} carries.
   * Present on every synchronous run from a host that supports it; for a dry
   * run it is the ONLY record of what ran, since a test run journals nothing.
   * Optional on the type (not the wire) so a response from an older host still
   * parses.
   */
  nodes?: WorkflowRunNode[];
  /**
   * `true` when the host ran this as a **dry run / test run** (issue #542): the
   * real graph walked over stubbed effects, so nothing was sent, no tokens were
   * spent, and nothing was journaled or parked.
   *
   * **The presence discriminator, and always `true` when set.** An older host
   * ignores the `dryRun` request flag and runs FOR REAL, answering with a body
   * that has no `dryRun` key — so a console that asked for a test run must read
   * this back (see {@link isDryRun}) rather than trust what it asked for, and
   * warn loudly when it is absent. Optional/absent must read as "was NOT a dry
   * run", i.e. the run was real.
   */
  dryRun?: boolean;
  /**
   * The nodes this run blocked on a person (issue #881) — the run drawer is
   * where an operator who pressed Run first learns the pipeline delivered
   * nothing, and why. Absent when nothing blocked.
   */
  blockedNodes?: WorkflowBlockedNode[];
  /**
   * The approvals this run parked (issue #880). A receipt of what it opened,
   * never a count of what is still outstanding.
   */
  approvals?: WorkflowRunApprovalRow[];
  /**
   * The board writes this run's agent nodes performed (issue #661 / M5) — the
   * cards it opened or re-owned. Absent when it touched no card, which is
   * nearly every run. Rendered by the run drawer since issue #1014.
   */
  board?: WorkflowRunBoardRow[];
  /**
   * What this run adds up to, as the host reads it (issue #981).
   *
   * Optional on the type, **never** on the wire from a host that has it: a
   * response predating #981 carries no key at all, and the reading has to be
   * derived locally in that case. It is the only field on this body that says a
   * report did not go out — `nodes[].status` deliberately does not move for a
   * delivery failure, because the nodes really did run.
   */
  verdict?: WorkflowRunVerdict;
}

/**
 * The `detach: true` answer (issue #383): the run's id, before it has finished.
 *
 * `detached` is the discriminator and it is always `true`. See
 * {@link isDetached} for why the shape rather than the request is what the
 * console must read.
 */
export interface WorkflowRunAccepted {
  runId: string;
  detached: true;
}

/**
 * What `POST …/workflows/{wid}/run` can answer with — a settled run, or an
 * accepted one.
 */
export type WorkflowRunResponse = WorkflowRunResult | WorkflowRunAccepted;

/**
 * Whether the host answered "accepted, watch the stream" rather than "here is
 * the finished run".
 *
 * **Discriminates on the response, never on what we asked for**, and that is
 * the whole compatibility story. A host predating #383 ignores the unknown
 * `detach` field — the body has no `deny_unknown_fields` — and answers the full
 * synchronous `200`. So a console that assumed "I asked to detach, therefore
 * this is a run id" would read a settled run's body as an acceptance and then
 * wait forever for frames that already arrived.
 *
 * Reading `detached` back is also what makes the older-host case *work* rather
 * than merely not crash: the settled body is a perfectly good answer, so the
 * console simply uses it.
 */
export function isDetached(
  response: WorkflowRunResponse,
): response is WorkflowRunAccepted {
  return (response as WorkflowRunAccepted).detached === true;
}

/**
 * Whether the host actually ran this as a **dry run** (issue #542).
 *
 * **Discriminates on the response, never on what we asked for** — the same
 * compatibility story as {@link isDetached}. A host predating test mode ignores
 * the `dryRun` request flag and runs FOR REAL, and its settled body carries no
 * `dryRun` key. So a console that asked for a test run and reasoned "I asked to
 * dry-run, therefore nothing happened" would be wrong on exactly the hosts where
 * a real run just fired every effect. Read `dryRun` back instead, and when it is
 * absent from a run you asked to be dry, warn loudly: the run was real.
 *
 * Only meaningful on a settled result (a detached response carries no output and
 * no `dryRun`); guarded so it is safe to call on either shape.
 */
export function isDryRun(response: WorkflowRunResponse): boolean {
  return (response as WorkflowRunResult).dryRun === true;
}

/** One node's outcome inside a run (issue #371, third reading #881). */
export interface WorkflowRunNode {
  nodeId: string;
  /**
   * Whether the node succeeded, errored, or **blocked** (issue #881).
   *
   * `blocked` is neither of the other two: the node stopped because a tool call
   * in its turn was parked for a person, so it produced no deliverable and
   * nothing after it ran. Rendering it as a failure sends an operator hunting
   * for a bug when the fix is a click in Approvals; rendering it as `ok` is the
   * lie the issue was filed about.
   */
  status: "ok" | "error" | "blocked";
  /** Wall-clock duration of the node's execution, in milliseconds. */
  elapsedMs: number;
  /**
   * The node's null-resolved config paths (issue #1014) — the engine's own list
   * of the broken wiring behind this step: every config `=`-expression that
   * resolved to `null`, as its dotted config **location** (e.g. `args.to`).
   *
   * Paths only, never a resolved value: a null resolution has no value, and the
   * host forwards only the config location — the same no-payload stance
   * `status`/`elapsedMs` take. Absent (the host omits an empty list) for a node
   * with no unresolved wiring.
   */
  diagnostics?: string[];
}

/** One node a run blocked on a person (issue #881). */
export interface WorkflowBlockedNode {
  nodeId: string;
  /** The tools whose calls were gated. Names only — never arguments. */
  tools: string[];
  /** The approvals this node's gated calls opened. Absent when every park failed. */
  approvalIds?: string[];
  /**
   * How many of the node's gated calls could NOT be queued for approval.
   *
   * Strictly worse than a parked one: there is no card, so pointing the
   * operator at Approvals would send them to an empty page. Absent when zero.
   */
  unparkable?: number;
  /**
   * How many of `approvalIds` the host no longer holds (issue #1143).
   *
   * The same end state as `unparkable`, reached later: the card was opened, so
   * the run recorded an id for it, but the question did not survive and the
   * queue has nothing to decide. Pointing the operator at Approvals for these
   * sends them to an empty page — which is the dead end #1143 was filed for.
   *
   * Computed by the host on each read of run history rather than stored, so it
   * reflects the queue as it is now. Absent when zero, which is every healthy
   * run.
   */
  stranded?: number;
}

/** What became of one gated tool call a run tried to park (issue #880). */
export type WorkflowApprovalOutcome = "parked" | "parkFailed" | "discarded";

/**
 * One approval a run parked (issue #880) — a **receipt**, not a live status.
 *
 * Read it as "this run opened this card", never as "this card is still
 * waiting": nothing comes back to flip a row once the operator approves. The
 * Approvals page is the live source of truth. Wording follows from that — the
 * console says "parked N approvals", never "waiting on N".
 */
export interface WorkflowRunApprovalRow {
  /** The agent node whose turn made the call. Absent when node identity was unavailable. */
  nodeId?: string;
  /** The tool whose call was gated. Absent on a `discarded` row — see the outcome. */
  tool?: string;
  outcome: WorkflowApprovalOutcome;
  /** The card the operator can decide, on the `parked` arm. */
  approvalId?: string;
}

/**
 * What a run's node did to the task board, as a closed set (issue #661 / M5).
 *
 * The two spawn arms opened (or tried to open) a card; the two assign arms set
 * (or tried to set) an owner. The `*Failed` arms are NOT run failures — they
 * record that the store refused a write the node's turn was already told would
 * happen, the same honesty {@link DeliveryStatus} `failed` gives a report that
 * did not send. Mirrors the Rust `WorkflowBoardAction` camelCase serde.
 */
export type WorkflowBoardAction =
  | "spawned"
  | "assigned"
  | "spawnFailed"
  | "assignFailed";

/**
 * One board write a run's agent node performed (issue #661 / M5) — "this run
 * opened card X", the half the run drawer never rendered (issue #1014).
 *
 * Structural only: the action, the ids involved, nothing a model wrote. The
 * same shape rides all three surfaces — the synchronous run response, the run
 * history, and the `WorkflowRunFinished` event — so a console reads a run's
 * board effects identically wherever it finds them. Mirrors the Rust
 * `WorkflowRunBoardRow` camelCase serde.
 */
export interface WorkflowRunBoardRow {
  /** What was attempted, and whether it landed. */
  action: WorkflowBoardAction;
  /**
   * The card the row is about, so a console can link straight to it. Absent on
   * a `spawnFailed` row — no card was written, so there is no id to point at.
   */
  taskId?: string;
  /**
   * The title the node asked for, on the two spawn arms — including
   * `spawnFailed`, where it is the only thing that explains the failure. Absent
   * on the assign arms, which name a card by id and carry no title.
   */
  title?: string;
  /**
   * The owner the node asked for, as it wrote it — the requested name rather
   * than a resolved roster id. `None` when the node named nobody.
   */
  assignee?: string;
}

/**
 * One finished workflow run, read back from the company's journal (issue #228).
 *
 * Before this, a run's outcome existed only in the moment: a manual run's
 * `deliveries` lived in the run drawer until it was dismissed, and a scheduled
 * run's reached only the host's stdout — which on a hosted tenant is the
 * platform team, not the operator. This is the same information, durable, so it
 * survives a console reload and a scheduled run nobody watched.
 */
export interface WorkflowRunOutcome {
  /** The journal sequence position — a stable, monotonic row key. */
  seq: number;
  /** Epoch-millis the outcome was recorded. */
  atMillis: number;
  workflowId: string;
  /** True when a cron started the run rather than an operator. */
  scheduled: boolean;
  /** A run correlation id, when the entry point minted one. */
  runId?: string;
  /** The same delivery rows a manual run's response carries. */
  deliveries: DeliveryReport[];
  /** Node ids the run left waiting on a human approval. */
  pendingApprovals: string[];
  /**
   * How many of `pendingApprovals` have **no live card left in the queue**
   * (issue #1189) — the gate-shaped sibling of `blockedNodes[].stranded`.
   *
   * A gate the engine paused at is parked as a `workflow.approve` card that
   * records no receipt and no blocked-node row, so #1143's per-node count
   * cannot describe it: the only join is `(runId, nodeId)`, and only the host
   * can make it. Absent when zero, and absent entirely from a host predating
   * this — which the console reads as "not reconciled", never as "nothing is
   * stranded".
   *
   * Derived on each read, like `blockedNodes[].stranded`, so it reflects the
   * queue as it is now rather than what the run recorded when it stopped.
   */
  strandedApprovals?: number;
  /** Set when the run failed outright instead of finishing with rows. */
  error?: string;
  /**
   * Per-node progress, in the order the nodes finished (issue #371). Absent on
   * a run journaled before #371 — so an empty/absent list means "no per-node
   * trail", never "the run did nothing".
   */
  nodes?: WorkflowRunNode[];
  /**
   * The nodes this run has *begun* executing, in start order (issue #1010).
   *
   * The other half of {@link nodes}, which is written by the finish bracket
   * only — so before this a run in flight came back listing what was already
   * over and nothing about the node working right now. Every console that
   * learns about a run from the history rather than from a live start frame (a
   * reload, a cron fire, an `EventSource` reconnect, a workflow switch and
   * back) reads the graph through this.
   *
   * A **receipt of what started**, kept once the run settles: an id here with
   * no matching {@link nodes} row on a settled run is the node the run was
   * standing on when it was cancelled or lost. So it must ALWAYS be paired
   * with {@link running} before anything is painted as in flight — see
   * `statesFromRun`.
   *
   * Absent on a host predating #1010 and on a run journaled before #382, so
   * absent must read as "no start trail", never as "nothing started".
   */
  startedNodes?: string[];
  /** When the run started. Absent on a pre-#371 row, whose only time is the finish. */
  startedAtMillis?: number;
  /**
   * True for a run that started and has not settled.
   *
   * Trustworthy only because the host settles runs its previous process left
   * open, at boot — so nothing sits here spinning forever.
   */
  running?: boolean;
  /**
   * True for a run an operator stopped (issue #383).
   *
   * Separate from {@link error} because it is a separate outcome: a cancelled
   * run carries no error, so reading only `error` would render a deliberate
   * stop as a clean success. With `error` this gives the three terminal
   * readings the history distinguishes — failed, interrupted by a host restart,
   * stopped by an operator.
   *
   * Optional on the type, not the wire: a host predating #383 sends nothing,
   * and absent must read as "not cancelled".
   */
  cancelled?: boolean;
  /**
   * System notices raised about this run (issue #638) — today, that a node
   * gated more tool calls than the per-batch cap allows and the excess was
   * discarded.
   *
   * NOT a failure. A run that overflowed the cap still succeeded: its nodes
   * ran and its output is valid. Render it as a warning beside the outcome,
   * never as an error, or a run that worked reads as one that broke.
   *
   * Omitted on the wire for the overwhelming majority of runs, which raise
   * nothing — so absent must read as "nothing to tell you", not "unknown".
   */
  notices?: string[];
  /**
   * The nodes this run blocked on a person (issue #881).
   *
   * NOT a failure and NOT a pause. The branch stopped, but approving does not
   * continue this run — an agent node is not re-enterable, so the operator
   * decides the card and runs the workflow again. Absent on a host predating
   * #881 and on every run that blocked on nobody, so absent must read as
   * "nothing blocked".
   */
  blockedNodes?: WorkflowBlockedNode[];
  /**
   * The approvals this run parked (issue #880) — including the parks that
   * failed, which are the rows that matter most: nobody will ever be asked
   * about those calls.
   *
   * A receipt of what the run opened, never a count of what is outstanding.
   */
  approvals?: WorkflowRunApprovalRow[];
  /**
   * The board writes this run's agent nodes performed (issue #661 / M5) — the
   * same rows a manual run's response carries, durable in the history. Absent
   * on a run that touched no card.
   */
  board?: WorkflowRunBoardRow[];
  /**
   * What this run adds up to, as the host reads it (issue #981).
   *
   * **Derived by the host, never journaled**, so a run recorded long before
   * this field existed still comes back with one — the whole history re-scores
   * on deploy rather than on a migration. Optional on the type only because a
   * *host* predating #981 sends no key; when that happens {@link runTone} falls
   * back to deriving it here, which is where the definition used to live.
   */
  verdict?: WorkflowRunVerdict;
}

export function listWorkflows(
  client: OpenCompanyClient,
  company: string | null,
): Promise<WorkflowSummary[]> {
  return client.get<WorkflowSummary[]>(`${client.scopeFor(company)}/workflows`);
}

export function getWorkflow(
  client: OpenCompanyClient,
  company: string | null,
  wid: string,
): Promise<WorkflowGraph> {
  return client.get<WorkflowGraph>(
    `${client.scopeFor(company)}/workflows/${encodeURIComponent(wid)}`,
  );
}

/**
 * One tool this company is granted but that cannot run on this deployment
 * (issue #874) — reported so the console can tell "not allowed here" from
 * "allowed, not configured yet".
 */
export interface UnwiredWorkflowTool {
  slug: string;
  /** `searchBackendNotConfigured` | `capabilityTierFiltered` — new tokens may appear. */
  reason: string;
  /** The same reason in prose, safe to show as-is. */
  detail: string;
}

/** The `GET …/workflows/tool-slugs` answer (issues #783, #874). */
interface WorkflowToolSlugsResponse {
  slugs: string[];
  /** Absent on a host predating issue #874. */
  unwired?: UnwiredWorkflowTool[];
}

/** What {@link listWorkflowToolSlugs} resolves to. */
export interface WorkflowToolSlugs {
  /** The effective slugs — granted AND wired here. Ground prompts on these. */
  slugs: string[];
  /**
   * Granted but unwired here. Empty on a host predating issue #874, which is
   * indistinguishable from "everything granted is wired" — both mean there is
   * nothing extra to warn about, so no caller needs to tell them apart.
   */
  unwired: UnwiredWorkflowTool[];
}

/**
 * The `tool_call` slugs the per-workflow copilot may ground a proposal on
 * (issue #783), narrowed to the **effective** set by issue #874.
 *
 * A slug in `slugs` is granted by `[tools].allow` AND wired on this deployment,
 * so a proposed node has a chance of running — the route serves the SAME set the
 * create-time copilot grounds on, so the two cannot drift. Tools the company
 * holds a grant for but that cannot run here come back under `unwired` instead
 * of being dropped, so the copilot can be told not to author them and still say
 * why when asked. A host predating the route 404s; the caller degrades to empty
 * lists, exactly as it does for the roster read, rather than blocking the
 * copilot.
 */
export function listWorkflowToolSlugs(
  client: OpenCompanyClient,
  company: string | null,
): Promise<WorkflowToolSlugs> {
  return client
    .get<WorkflowToolSlugsResponse>(
      `${client.scopeFor(company)}/workflows/tool-slugs`,
    )
    .then((r) => ({ slugs: r.slugs, unwired: r.unwired ?? [] }));
}

/** The `GET …/workflows/wired-channels` answer (issue #813). */
interface WiredChannelsResponse {
  channels: string[];
}

/**
 * The chat channels this running company can deliver to — the real targets an
 * output node's `channel` destination may name (issue #813): its desk chats and
 * its enabled OpenHuman-provider manifest channels.
 *
 * **`operator` is always one of them** (issue #1757). It was excluded per
 * issue #981, back when the in-memory `operator` adapter had no durable reader
 * and workflow delivery refused it by name; the built-in Operator channel is
 * now a durable, journal-backed delivery target present on every running
 * company, so the host serves it here like any other real channel.
 *
 * The console reads this to offer a picker instead of a free-text box that only
 * fails at delivery with `ChannelNotWired`. An empty list has two causes and the
 * fallback is the same for both: a host predating the route 404s, and a company
 * with no desks and no connected channels genuinely has nowhere to deliver. The
 * caller degrades to an empty list (the picker falls back to free text, and the
 * save is refused server-side if the target is not deliverable) rather than
 * blocking authoring.
 */
export function listWiredChannels(
  client: OpenCompanyClient,
  company: string | null,
): Promise<string[]> {
  return client
    .get<WiredChannelsResponse>(
      `${client.scopeFor(company)}/workflows/wired-channels`,
    )
    .then((r) => r.channels);
}

/**
 * Runs a workflow.
 *
 * **The console omits `detach` on purpose (issue #528).** A run's full `output`
 * is carried ONLY by the settled `200` body: the journal, the SSE frames, and
 * {@link listWorkflowRuns} are all structural (per-node status and timing, no
 * agent text), and there is no run-detail route to fetch the output back later.
 * The one surface that renders what a run produced mounts on that body, so the
 * console must hold the request open to receive it. This is affordable because
 * since #383 the host runs even the synchronous path on a spawned server task —
 * the run itself survives a dropped connection; only the answer rides the wire.
 *
 * With `detach` the host instead answers as soon as the run has an id (`202`),
 * without holding the request open — which stops a proxy's idle timeout from
 * severing a healthy multi-minute run, at the cost of the console never seeing
 * the output. The option is kept for callers that only need the run to start
 * (e.g. a fire-and-forget trigger) and will read the outcome from the stream or
 * the history.
 *
 * **Always check {@link isDetached} on the result**, whatever you asked for: a
 * host predating #383 ignores the flag and answers with the settled run, and
 * that answer is fine to use.
 *
 * **`dryRun` (issue #542)** asks the host to run a **test run**: the real graph
 * over stubbed effects, so nothing is sent and no tokens are spent. It is
 * synchronous by nature — the whole point is to read the settled `output` and
 * per-node `nodes` back — so it composes with the default (no `detach`). Just
 * like `detach`, an older host ignores the flag and runs FOR REAL, so the caller
 * MUST confirm with {@link isDryRun} rather than assume, and warn when a run it
 * asked to be dry comes back without the marker.
 */
export function runWorkflow(
  client: OpenCompanyClient,
  company: string | null,
  wid: string,
  input?: unknown,
  options?: { detach?: boolean; dryRun?: boolean },
): Promise<WorkflowRunResponse> {
  return client.post<WorkflowRunResponse>(
    `${client.scopeFor(company)}/workflows/${encodeURIComponent(wid)}/run`,
    {
      input: input ?? {},
      ...(options?.detach ? { detach: true } : {}),
      ...(options?.dryRun ? { dry_run: true } : {}),
    },
  );
}

/**
 * Stops a run that is still walking its graph (issue #383).
 *
 * Resolves when the host has fired the stop signal — **not** when the run has
 * wound down. The run settles a moment later and announces itself on the event
 * stream with `cancelled`, which is the frame to believe.
 *
 * Throws `404` when the run is unknown or has already settled (one answer:
 * there is nothing to stop) and when the host predates this route. Callers
 * should tell those apart by context rather than by the status, and degrade to
 * "this host can't stop runs" rather than surfacing a raw error.
 *
 * Stopping is not finishing: the executing node is dropped mid-flight, so an
 * external side effect it had started may be half-done. Completed nodes stay in
 * the run history, and approvals earlier nodes parked stay valid in the queue.
 */
export function cancelWorkflowRun(
  client: OpenCompanyClient,
  company: string | null,
  runId: string,
): Promise<{ cancelling: boolean }> {
  return client.post<{ cancelling: boolean }>(
    `${client.scopeFor(company)}/workflows/runs/${encodeURIComponent(runId)}/cancel`,
    {},
  );
}

/**
 * One page of {@link listWorkflowRuns} (issue #1012).
 *
 * `hasMore` says whether an older page exists behind `nextBeforeSeq` — the run
 * history drawer's "Load older" affordance is gated on it, so a truncated
 * history never silently reads as the whole thing.
 */
export interface WorkflowRunsPage {
  runs: WorkflowRunOutcome[];
  hasMore: boolean;
  /**
   * The cursor to pass back as `beforeSeq` for the page behind this one — the
   * page's **lowest** `seq`, which is not in general the last row in display
   * order.
   *
   * Server-issued rather than derived here, and that is the point. The host
   * cuts a page by `seq` (monotonic, and the key its journal read is bounded
   * by) and then sorts it for display by `(atMillis, seq)`; `atMillis` is
   * wall-clock, so a clock regression makes the two orders disagree and
   * `runs.at(-1)!.seq` is no longer the boundary. Paging off the last
   * displayed row then skips runs permanently — the very bug #1012 is about.
   *
   * **Absent on a host predating this field.** That must fall back to the old
   * `runs.at(-1)?.seq` derivation, never to "there are no more pages": the
   * latter would ship this fix as a fresh silent truncation. `hasMore` remains
   * the only thing that says whether to keep going.
   */
  nextBeforeSeq?: number;
}

/**
 * The company's finished workflow runs, **newest first** (issue #228) — now
 * genuinely true of the *displayed* `seq`/`atMillis`, not just the order two
 * runs started in (issue #1012).
 *
 * `workflow` narrows to one graph's runs; `limit` caps the page (the host
 * defaults to a short recent list and clamps a large ask). `beforeSeq` pages
 * further back: pass the previous page's {@link WorkflowRunsPage.nextBeforeSeq}
 * to fetch the page before it (issue #1012) — `hasMore` says whether one
 * exists. A host predating this route answers 404 — callers should treat that
 * as "no history yet" rather than an error, since the console still works
 * without it.
 */
export function listWorkflowRuns(
  client: OpenCompanyClient,
  company: string | null,
  options?: { workflow?: string; limit?: number; beforeSeq?: number },
): Promise<WorkflowRunsPage> {
  const params = new URLSearchParams();
  if (options?.workflow) params.set("workflow", options.workflow);
  if (options?.limit) params.set("limit", String(options.limit));
  // `!== undefined`, not truthiness: `0` is a legitimate cursor (the journal's
  // first row) and a truthy check drops it, silently asking for the newest
  // page again and looping the caller on the same rows.
  if (options?.beforeSeq !== undefined) params.set("before_seq", String(options.beforeSeq));
  const query = params.toString();
  return client.get<WorkflowRunsPage>(
    `${client.scopeFor(company)}/workflows/runs${query ? `?${query}` : ""}`,
  );
}

/**
 * One past run's durable per-node output snapshot (issue #596) — the data the
 * run inspector renders when an operator opens a node.
 *
 * `nodes` is the engine's `{ "<node id>": { "items": [ … ] } }` map, bounded for
 * storage; `truncated` says whether any value was clipped to fit the caps, so the
 * inspector can badge it honestly. `partial` says the run FAILED or BLOCKED, so
 * the map is only what the runner captured from the nodes that finished before
 * the stop — not a complete outcome (issue #1008).
 */
export interface WorkflowRunOutputRecord {
  runId: string;
  workflowId: string;
  /** Epoch-millis the snapshot was captured (the run's settle time). */
  atMillis: number;
  /** The per-node output map — `{ "<node id>": { "items": [ … ] } }`. */
  nodes: unknown;
  /** Whether any value was clipped to fit the durable size caps. */
  truncated: boolean;
  /**
   * Whether this is a partial capture from a run that failed or blocked rather
   * than a clean settled outcome (issue #1008). Optional so a snapshot written
   * before this field existed (always a clean settle) reads back as absent,
   * which the inspector treats as `false`.
   */
  partial?: boolean;
}

/**
 * Fetches one past run's per-node output snapshot (issue #596).
 *
 * This is the durable extension of {@link runWorkflow}'s in-memory `output`: a
 * run reopened from History has no live result, so the inspector reads what each
 * node produced from here instead. Lazy per-run by design — the history list
 * stays structural (status + timing), and only the run an operator clicks into is
 * fetched.
 *
 * **Throws `404` for a run with no captured output**, which is the normal state
 * for a run that predates this feature, a dry run (persists nothing), or a
 * hard-aborted run (no outcome to persist) — and for a host predating this
 * route. Callers should treat a 404 as "no output to show" and render the empty
 * state, not surface an error.
 */
export function workflowRunOutput(
  client: OpenCompanyClient,
  company: string | null,
  runId: string,
): Promise<WorkflowRunOutputRecord> {
  return client.get<WorkflowRunOutputRecord>(
    `${client.scopeFor(company)}/workflows/runs/${encodeURIComponent(runId)}/output`,
  );
}

/**
 * One file a workflow run produced (issue #1684) — a row of the run inspector's
 * "Files associated" section.
 *
 * The host resolves these through the run's provenance chain (`run_id → cards
 * opened by the run → each card's artifacts`), so a row carries exactly what the
 * console needs to deep-link the file and nothing more: it is **metadata only**,
 * never the artifact body. {@link artifactHref} turns `taskId` + `artifactId` +
 * `latestVersion` into the Artifacts-tab address, and `workspaceNodeId` — when
 * the file was mirrored into the shared tree — into the second `#/workspace/<id>`
 * link.
 */
export interface RunArtifactRow {
  /** The card that produced the file — scopes the Artifacts tab the link opens. */
  taskId: string;
  /** The artifact's stable id → the tab's open artifact. */
  artifactId: string;
  /** The artifact's display title. */
  title: string;
  /** What the file holds — `text` | `markdown` | `image` | `file`. */
  kind: ArtifactKind;
  /**
   * The workspace-relative path the agent published (e.g. `specs/launch.md`).
   * Absent on a legacy record captured before the source path existed (issue
   * #244); the console labels those rather than hiding them.
   */
  source?: string;
  /** The newest revision number → the version the deep-link pins. */
  latestVersion: number;
  /** Epoch-millis of the newest revision — the host's sort key and the row's time. */
  updatedAtMillis: number;
  /**
   * The workspace node the newest revision was mirrored into (issue #552), when
   * one was → an optional `#/workspace/<id>` link. Absent when nothing mirrored it.
   */
  workspaceNodeId?: string;
  /** The producing card's title, for grouping rows by card. */
  taskTitle?: string;
}

/** The `GET …/workflows/runs/{rid}/artifacts` response. A wrapper rather than a
 * bare array so a run whose file count exceeds the host's defensive cap can say
 * so instead of the console presenting an incomplete list as exhaustive. */
export interface RunArtifactsResponse {
  /** The run's files, newest first (the host's sort). */
  files: RunArtifactRow[];
  /**
   * Whether older rows were cut by the host's cap. `false` for every run in
   * practice — the cap is a ceiling, not a page size — but when `true` the
   * console labels the list rather than silently showing a subset.
   */
  truncated: boolean;
}

/**
 * Fetches the files one past run produced (issue #1684), for the run inspector's
 * lazy "Files associated" disclosure.
 *
 * Lazy per-run by design, exactly like {@link workflowRunOutput}: the history
 * list stays structural, and only the run an operator expands is fetched.
 *
 * **Answers `200 { files: [] }` — never `404` — for a run that produced no
 * files**, which is the common case (a run that opened no cards, or cards that
 * published nothing). That is the one contract difference from
 * `workflowRunOutput`: a fileless run is normal, so callers render the empty
 * state off an empty array rather than off a caught 404.
 */
export function fetchRunArtifacts(
  client: OpenCompanyClient,
  company: string | null,
  runId: string,
): Promise<RunArtifactsResponse> {
  return client.get<RunArtifactsResponse>(
    `${client.scopeFor(company)}/workflows/runs/${encodeURIComponent(runId)}/artifacts`,
  );
}

/**
 * Authors a new workflow graph (issues #69, #168): the console's form creator
 * posts the same shape `getWorkflow` returns, and the host persists it on the
 * company record — so this works on every deployment, including a hosted tenant
 * whose company source tree is a read-only mount. Rejections carry a
 * prosumer-language `ApiError` message (bad id, duplicate id or name, an edge or
 * `agent` node the graph can't support).
 */
export function createWorkflow(
  client: OpenCompanyClient,
  company: string | null,
  graph: WorkflowGraph,
): Promise<WorkflowGraph> {
  return client.post<WorkflowGraph>(
    `${client.scopeFor(company)}/workflows`,
    graph,
  );
}

/**
 * Asks the host whether Create would accept this graph, **without saving it**
 * (issue #1074).
 *
 * Two of Create's rules cannot be pre-empted by a client without
 * re-implementing them — node reachability, and the condition branch-label rule
 * — and a client-side copy of a host rule drifts. So the console asks instead of
 * mirroring. The host runs the same validation Create runs, so a `200` here and
 * a refusal there (or the reverse) is not a state the two can be in.
 *
 * **A rejection is an `ApiError`, not a return value**, unlike {@link previewCron}
 * and {@link draftFromDescription}: the body is byte-for-byte the `400` Create
 * would have answered with, so a caller can render one code path for both. It
 * does NOT answer id/name uniqueness — that is decided under a write lock at
 * save time, so a `200` here still permits a `409` there.
 */
export function validateWorkflow(
  client: OpenCompanyClient,
  company: string | null,
  graph: WorkflowGraph,
): Promise<{ valid: boolean }> {
  return client.post<{ valid: boolean }>(
    `${client.scopeFor(company)}/workflows/validate`,
    graph,
  );
}

/**
 * What the create-time copilot drafted from an operator's description (issue
 * #753).
 *
 * Exactly one shape is returned, told apart by `automatable`: a drafted
 * `workflow` to review, or a `reason` the described work is better done once.
 * The host answers **200 in both cases** (like {@link previewCron}) — neither is
 * an error the operator fixes by retrying differently.
 *
 * `workflow` is a plain {@link WorkflowGraph}: the host strips the model's
 * approval gating and mints the id, and the node/edge shape is the same camelCase
 * the read routes return, so it hydrates the create form with no adapter. It
 * carries no `version` (nothing is saved yet) — Create persists it for the first
 * time.
 */
export interface WorkflowDraftFromDescription {
  /** `true` with a `workflow` to review; `false` with a `reason`. */
  automatable: boolean;
  /** A one-line gloss of what the drafted workflow does. Present when `automatable`. */
  summary?: string;
  /** The drafted graph to hydrate the create form. Present when `automatable`. */
  workflow?: WorkflowGraph;
  /** Why the work is better done once. Present when not `automatable`. */
  reason?: string;
  /**
   * Host corrections the operator should see (issue #813) — e.g. the copilot
   * matched a teammate named by role to their roster id. Present (and non-empty)
   * only when the draft was corrected; older hosts omit the field entirely.
   */
  notes?: string[];
}

/**
 * Drafts a workflow graph from a free-text description (issue #753) — the
 * New-workflow dialog's copilot.
 *
 * **Nothing is persisted.** The host validates the draft the same way Create
 * would, hands it back, and the operator reviews it in the hydrated form before
 * pressing Create (which is the only call that saves). So this is safe to call
 * speculatively; a bad draft costs a review, not a rollback.
 *
 * A build with no embedded brain answers a capability gap the same way the run
 * route does — `not_wired` (404), `restart_required` or `inference_required`
 * (409) — which the caller should surface inline rather than treat as a drafted
 * answer. An empty description is a `400`.
 */
export function draftWorkflowFromDescription(
  client: OpenCompanyClient,
  company: string | null,
  description: string,
): Promise<WorkflowDraftFromDescription> {
  return client.post<WorkflowDraftFromDescription>(
    `${client.scopeFor(company)}/workflows/draft-from-description`,
    { description },
  );
}

/**
 * The static authoring readiness of a copilot-corrected graph (issue #840, PR-3)
 * — **advisory only**.
 *
 * `ok` is whether the host's always-compiled authoring gates found nothing;
 * `advisories` names each remaining smell to look at before saving. It never
 * blocks the save, so a non-`ok` readiness rides a 200 alongside the graph — the
 * console renders it read-only, not as an error.
 */
export interface WorkflowReadiness {
  ok: boolean;
  /** Each remaining authoring smell, in prosumer language. Omitted when `ok`. */
  advisories?: string[];
}

/**
 * What the copilot drafted when correcting a workflow whose run failed (issue
 * #840, PR-3). Mirrors {@link WorkflowDraftFromDescription}: exactly one shape,
 * told apart by `automatable` — a corrected `workflow` to review (keeping the
 * SAME id, so Save is a new version), or a `reason` the failure cannot be fixed
 * by re-wiring. Carries {@link WorkflowReadiness} on the corrected graph.
 */
export interface WorkflowFixFromRun {
  automatable: boolean;
  /** A one-line gloss of the correction. Present when `automatable`. */
  summary?: string;
  /** The corrected graph, same id as the failing workflow. Present when `automatable`. */
  workflow?: WorkflowGraph;
  /** Host corrections the operator should see (a role→id rewrite, etc.). */
  notes?: string[];
  /** Why the failure cannot be fixed by re-wiring. Present when not `automatable`. */
  reason?: string;
  /** Static readiness advisories over the corrected graph. Present when `automatable`. */
  readiness?: WorkflowReadiness;
}

/**
 * A copilot-corrected graph handed straight to the edit dialog to hydrate (issue
 * #840, PR-3), bypassing the description→draft round trip. The dialog loads the
 * `workflow` nodes/edges/name directly and shows the summary/notes/readiness
 * banners read-only.
 */
export interface PrefilledDraft {
  summary?: string;
  workflow: WorkflowGraph;
  notes?: string[];
  readiness?: WorkflowReadiness;
}

/**
 * Corrects a saved workflow whose run failed, with the copilot (issue #840,
 * PR-3) — the engine behind the run-history "Fix with copilot" affordance.
 *
 * **Nothing is persisted.** The host drafts a corrected graph — keeping the same
 * id, so the operator's Save (`PUT …/workflows/{wid}`) is a new *version*, not an
 * orphan — validates it, and hands it back for review in the edit dialog. A build
 * with no embedded brain answers a capability gap the same way the run route does
 * (`not_wired` 404, `restart_required` / `inference_required` 409). A run with no
 * recorded error and no `errorHint` is a `400`.
 */
export function fixWorkflowFromRun(
  client: OpenCompanyClient,
  company: string | null,
  wid: string,
  args: { runId: string; errorHint?: string },
): Promise<WorkflowFixFromRun> {
  return client.post<WorkflowFixFromRun>(
    `${client.scopeFor(company)}/workflows/${encodeURIComponent(wid)}/fix-from-run`,
    { runId: args.runId, errorHint: args.errorHint },
  );
}

/**
 * Replaces a saved workflow graph wholesale (issue #259) — the fix for a
 * workflow being write-once, so a typo'd cron or a node pointed at the wrong
 * teammate can be corrected instead of abandoned.
 *
 * `graph.id` must equal `wid`: a workflow's id keys its saved graph, its
 * schedule and its run history, so the host rejects a rename with a `400`. A
 * rename is a create plus a delete.
 *
 * Pass `expectedVersion` — the `version` from the {@link getWorkflow} this edit
 * was based on — to make the write conditional. It is **required** (issue
 * #1013): a `null`/absent token makes the host answer `400` rather than writing
 * unconditionally, which is what stops a stale editor silently clobbering a
 * concurrent save. If the graph moved in between, the host answers `409` and
 * **nothing is written**; surface either as a reload rather than retrying, which
 * is the silent-overwrite the guard exists to prevent.
 *
 * Other rejections carry the same prosumer-language `ApiError` a create does:
 * `400` for a bad graph or a missing token, `404` for an unknown id, `409` for a
 * source-defined workflow or a display name already taken.
 *
 * Returns the stored graph with a **fresh** `version`, so a second save needs no
 * intervening read.
 */
export function updateWorkflow(
  client: OpenCompanyClient,
  company: string | null,
  wid: string,
  graph: WorkflowGraph,
  expectedVersion?: string | null,
): Promise<WorkflowGraph> {
  return client.put<WorkflowGraph>(
    `${client.scopeFor(company)}/workflows/${encodeURIComponent(wid)}`,
    expectedVersion ? { ...graph, expectedVersion } : graph,
  );
}

/**
 * Removes a saved workflow (issue #259): the graph body and its entry in the
 * company's enabled list, in one write, so it leaves the picker and stops
 * firing on its schedule — and stays gone across a host restart.
 *
 * **Past runs are kept.** They record what the workflow did, which stays true
 * after it is gone; {@link listWorkflowRuns} keeps serving them.
 *
 * `expectedVersion` is **required** (issue #1013) and makes the delete
 * conditional in exactly the sense the operator means by clicking Delete on a
 * graph they are looking at: a `null`/absent token is a `400` rather than an
 * unconditional delete, and if the token changed underneath them the host
 * answers `409` and removes nothing.
 *
 * Follows the same runtime-vs-source contract as `deleteDesk`: a workflow
 * defined by a file in the company source tree cannot be removed from the
 * console and returns `409`; an unknown id is `404`.
 */
export function deleteWorkflow(
  client: OpenCompanyClient,
  company: string | null,
  wid: string,
  expectedVersion?: string | null,
): Promise<void> {
  const query = expectedVersion
    ? `?expectedVersion=${encodeURIComponent(expectedVersion)}`
    : "";
  return client.del<void>(
    `${client.scopeFor(company)}/workflows/${encodeURIComponent(wid)}${query}`,
  );
}

/**
 * Arms or pauses a workflow's schedule without touching its graph (issue #276).
 *
 * **Pausing stops the schedule, not the workflow.** A paused workflow keeps its
 * graph, stays in the picker, and still runs from the Run button — only the
 * scheduler consults the flag. Resolves with the workflow's graph as the host
 * now holds it, so a caller renders the state the store agreed to rather than
 * the one it asked for.
 *
 * Unlike {@link updateWorkflow} and {@link deleteWorkflow} this takes **no**
 * `expectedVersion`: a switch has no content to overwrite, and requiring a token
 * would make a seed-defined workflow untoggleable since only overlay bodies have
 * one. It is also a wider set than those two — a `409` here means only "this id
 * has no saved graph, so there is no schedule to switch off"; an unknown id is
 * `404`.
 *
 * Idempotent: setting the state a workflow already holds is a `200` that changes
 * nothing.
 */
export function setWorkflowEnabled(
  client: OpenCompanyClient,
  company: string | null,
  wid: string,
  enabled: boolean,
): Promise<WorkflowGraph> {
  return client.put<WorkflowGraph>(
    `${client.scopeFor(company)}/workflows/${encodeURIComponent(wid)}/enabled`,
    { enabled },
  );
}

/**
 * One entry in a workflow's edit history (issue #274) — **metadata only**.
 *
 * The graph body is deliberately absent: the history list is a chooser, and the
 * body arrives (and is applied to the canvas) only when
 * {@link restoreWorkflowRevision} actually restores one. `version` is the same
 * opaque token {@link getWorkflow} hands out, so the console can tell which
 * snapshot matches the graph it currently holds.
 */
export interface WorkflowRevision {
  /** Stable id of the snapshot, used to address it for a restore. */
  id: string;
  /** The workflow's display name at the moment the snapshot was captured. */
  name: string;
  /** The opaque version token of the snapshotted body. Never parse it. */
  version: string;
  /** Epoch-millis the snapshot was captured. */
  createdAtMillis: number;
}

/** The `GET …/workflows/{wid}/revisions` response. */
interface WorkflowRevisionsResponse {
  revisions: WorkflowRevision[];
}

/**
 * Lists one workflow's edit history (issue #274), **newest first**.
 *
 * Each `PUT` that actually changed the graph left the prior body here, bounded
 * to the most recent 20. A workflow that was never edited — or a seed-defined
 * one that cannot be edited from the console — resolves to an empty list rather
 * than an error: "no history" is a normal state the History panel renders as
 * empty, not a failure.
 *
 * Returns metadata only (see {@link WorkflowRevision}); the graph body is
 * fetched by {@link restoreWorkflowRevision} when an operator picks one.
 */
export async function listWorkflowRevisions(
  client: OpenCompanyClient,
  company: string | null,
  wid: string,
): Promise<WorkflowRevision[]> {
  const res = await client.get<WorkflowRevisionsResponse>(
    `${client.scopeFor(company)}/workflows/${encodeURIComponent(wid)}/revisions`,
  );
  return res.revisions;
}

/**
 * Restores a workflow to one of its captured revisions (issue #274), returning
 * the restored graph with a **fresh** `version`.
 *
 * A restore is an ordinary edit whose new body is an old one, so it inherits
 * everything {@link updateWorkflow} guarantees: the revision is re-validated
 * against the *current* company (a snapshot naming a since-removed teammate is a
 * `400`, not a broken restore), the body it replaces is itself snapshotted (so a
 * restore is undoable), and a restored schedule lands **switched off** pending
 * review (issue #276) — read `enabled` on the result to reflect that.
 *
 * Pass `expectedVersion` — the `version` of the graph the operator was looking
 * at — to make the restore conditional. It is **required** (issue #1013): a
 * `null`/absent token is a `400` rather than an unconditional restore. On a `409`
 * the graph moved underneath them: **reload and let them re-choose, do not
 * retry** without the token, which is the silent-overwrite the guard exists to
 * prevent. Other rejections carry the host's prosumer-language message: `400` for
 * a missing token, `404` for an unknown workflow or revision, `409` for a
 * source-defined / body-less workflow or a name collision.
 */
export function restoreWorkflowRevision(
  client: OpenCompanyClient,
  company: string | null,
  wid: string,
  revisionId: string,
  expectedVersion?: string | null,
): Promise<WorkflowGraph> {
  return client.post<WorkflowGraph>(
    `${client.scopeFor(company)}/workflows/${encodeURIComponent(wid)}/revisions/${encodeURIComponent(
      revisionId,
    )}/restore`,
    expectedVersion ? { expectedVersion } : {},
  );
}

/**
 * The node kinds the form creator's palette offers.
 *
 * The first six author from a bare form — `trigger` starts the graph, `agent`
 * runs a teammate, `condition` branches, `merge` fans inputs into one stream,
 * `transform` reshapes items, and `output` reports back. The last five need
 * kind-specific config to run (a slug, a URL, a branch key, a schema, a
 * `workflow_id`); they were withheld until the config forms landed, and now
 * carry them — each renders `NodeConfigFields`, whose spec table
 * (`@/lib/workflow-node-config`) is the single source of the engine keys it
 * emits (issue #541). Every one of these kinds also renders on the canvas and
 * can be authored by hand in `workflows/<id>.toml`.
 */
/**
 * What a trigger's cron expression actually means, per the host's parser
 * (issue #262).
 *
 * Exactly one of `error` or (`description`, `next`) is present — the host
 * answers with two shapes, not one shape with half its fields null.
 * `description` is `null` for a schedule the host declines to paraphrase; the
 * fire times still say what it means.
 */
export interface CronPreview {
  /** A plain-English gloss, or `null` when the shape is too gnarly for one. */
  description?: string | null;
  /**
   * The next few fire times as epoch millis. The UTC reading and the viewer's
   * local reading are BOTH rendered from these numbers, so the two can never
   * disagree about the same instant — which is the entire point, since the
   * schedule is UTC and the author is usually not.
   */
  next?: number[];
  /** The parser's message, when the expression did not parse. */
  error?: string;
}

/**
 * Previews a cron expression (issue #262).
 *
 * **Answers 200 even for a malformed expression**, with the parser's message in
 * `error`. The console calls this while the author is still typing, so a
 * half-written expression is the normal state — and {@link OpenCompanyClient}
 * throws on any non-2xx, so a 400 per keystroke would make an ordinary parse
 * failure arrive as a thrown error. Callers still need a `catch` for genuine
 * network failure; they should render nothing in that case rather than block
 * authoring on a preview.
 *
 * `after` pins the instant the fire times are computed from; the host defaults
 * to now.
 */
export function previewCron(
  client: OpenCompanyClient,
  company: string | null,
  expr: string,
  after?: number,
): Promise<CronPreview> {
  return client.post<CronPreview>(
    `${client.scopeFor(company)}/workflows/cron/preview`,
    { expr, after },
  );
}

/**
 * Every workflow node kind the host accepts, paired with its operator-facing
 * label. This is the console's single vocabulary for authoring and display:
 * consumers that need only the wire values derive them below rather than
 * maintaining another list that can drift.
 */
export const NODE_KINDS: readonly { value: string; label: string }[] = [
  { value: "trigger", label: "Trigger — starts the workflow" },
  { value: "agent", label: "Agent — a teammate performs a step" },
  { value: "condition", label: "Condition — branches on something" },
  { value: "merge", label: "Merge — combines several inputs into one" },
  { value: "transform", label: "Transform — reshapes the data" },
  { value: "output", label: "Output — reports the result back" },
  { value: "tool_call", label: "Tool call — runs a tool by slug" },
  { value: "http_request", label: "HTTP request — calls a URL" },
  { value: "switch", label: "Switch — routes to a labeled branch" },
  { value: "split_out", label: "Split out — sends each item down the next step" },
  { value: "output_parser", label: "Output parser — coerces to a schema" },
  { value: "sub_workflow", label: "Sub-workflow — runs another workflow" },
];

/**
 * A readable node-kind label, including for a kind introduced by a newer host.
 * The fallback deliberately humanises separators instead of exposing a raw
 * snake_case machine token as the primary label.
 */
export function nodeKindLabel(kind: string): string {
  const known = NODE_KINDS.find((candidate) => candidate.value === kind)?.label;
  if (known) return known.split(" — ", 1)[0];

  const words = kind.trim().replace(/[_-]+/g, " ").replace(/\s+/g, " ");
  if (!words) return "Unknown node kind";
  return words.replace(/^\p{L}/u, (letter) => letter.toLocaleUpperCase());
}

/**
 * Every node kind the host accepts in a saved graph — the OpenCompany authoring
 * contract, mirroring `WORKFLOW_NODE_KINDS` in `src/company/workflow_file.rs`.
 *
 * Derived from {@link NODE_KINDS}, so the authoring palette, inspector labels,
 * and proposal validation cannot disagree about which kinds are allowed.
 */
export const WORKFLOW_NODE_KINDS: readonly string[] = NODE_KINDS.map(
  (kind) => kind.value,
);
