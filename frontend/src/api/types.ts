// TypeScript mirrors of the OpenCompany operator API payloads.
// Kept in sync with src/runtime/types.rs, src/server/operator.rs, and
// src/feedback/{types,service}.rs.

/**
 * Where a company's manifest was seeded from — the source template's stable
 * identity, recorded once at launch. Mirrors `TemplateProvenance` in
 * `src/ports/types.rs`. Absent for a company provisioned from a raw manifest.
 */
export interface TemplateProvenance {
  /** The template's stable id — the source directory slug. */
  source_id: string;
  /** The template's version, when the source exposes one. */
  version?: string | null;
  /** The source directory the company was launched from, when recorded. */
  path?: string | null;
}

/** `GET /api/v1/companies` and `GET /api/v1/companies/{id}`. */
export interface CompanyStatus {
  id: string;
  name: string;
  /** e.g. "running", "paused", "suspended", "archived". */
  lifecycle: string;
  pending_approvals: number;
  /**
   * The source-template provenance recorded at launch (issue #85). Absent for
   * a company provisioned from a raw manifest rather than a template.
   */
  template_provenance?: TemplateProvenance | null;
  /**
   * Whether the governance kill switch is engaged (issue #86): new effects
   * outside the `Other` group are being denied.
   *
   * Deliberately independent of `lifecycle`, which stays `"running"` — chat
   * still works while a company is stopped. Anything showing company state must
   * read this too, or it will report a stopped company as perfectly healthy.
   */
  emergency_paused?: boolean;
}

/**
 * `GET /api/v1/companies/provisioning` — the sign-in mode a company provisioned
 * on this host right now would land in, so the create/reset dialog can collect
 * the right identity field before it builds a manifest. Mirrors
 * `ProvisioningInfoDto` in `src/server/provision.rs`.
 */
export interface ProvisioningInfo {
  /** The effective sign-in mode: `wallet`, `email`, or `none`. */
  auth_mode: "wallet" | "email" | "none";
  /** Whether provisioning requires at least one `[users].wallets` address. */
  wallets_required: boolean;
}

/** What kind of processing step this is (drives the timeline icon). */
export type TurnStepKind = "tool_call" | "thinking" | "note";

/**
 * How a processing step ended.
 *
 * `awaiting_approval` (#411) is **not** a failure: the call was gated and is
 * waiting on a person. It used to arrive as `error`, which made the one step an
 * operator could act on look like the one thing that had crashed. Anything
 * counting failures must key on `error` alone — see {@link isFailedStep}.
 */
export type TurnStepStatus = "ok" | "error" | "running" | "awaiting_approval";

/**
 * Why a step did not succeed, in the failure's own terms (#411). Mirrors
 * `TurnStepFailure` in `src/ports/types.rs`.
 *
 * Rendered by lookup, never by reading the prose in `result` — the whole point
 * is that the console switches on a known state instead of pattern-matching a
 * sentence. Absent on a success, on a step still `running`, and on a parked
 * one (its status already says what it is).
 */
export type TurnStepFailure =
  | "declined"
  | "blocked_by_policy"
  | "unauthorized"
  | "missing_permission"
  | "missing_app"
  | "not_found"
  | "timeout"
  | "unavailable"
  | "failed";

/**
 * One visible step in an agent turn's processing timeline. Mirrors `TurnStep`
 * in `src/ports/types.rs`. The host folds and scrubs these from the turn's
 * progress stream: nothing here carries raw tool output or call ids, and
 * arguments reach `detail` only through the host-side redactor an approval card
 * already uses (#372).
 */
export interface TurnStep {
  kind: TurnStepKind;
  status: TurnStepStatus;
  label: string;
  /**
   * **What the step was doing** — its arguments, redacted host-side and
   * bounded (#411). This is what tells two calls to the same tool apart.
   */
  detail?: string;
  /**
   * **What came back** — a shape summary (`"12 items"`), an intrinsic tool's
   * own message, or a failure's plain-language cause (#411). Never a remote
   * body's content.
   */
  result?: string;
  /** The typed reason the step did not succeed (#411). */
  failure?: TurnStepFailure;
  /** The result was cut before the agent could read all of it (#410). */
  truncated?: boolean;
  /** How long a tool call took, in milliseconds, when known. */
  elapsedMs?: number;
}

/**
 * Short, operator-facing copy for each {@link TurnStepFailure}, plus the word
 * for a parked step.
 *
 * One table, used by both render surfaces (the chat bubble's step timeline and
 * the task Attempts tab), so a failure cannot be named two different things
 * depending on where you happen to be looking at it.
 */
export const STEP_FAILURE_LABEL: Record<TurnStepFailure, string> = {
  declined: "Declined",
  blocked_by_policy: "Blocked by policy",
  unauthorized: "Unauthorized",
  missing_permission: "Missing permission",
  missing_app: "App unavailable",
  not_found: "Not found",
  timeout: "Timed out",
  unavailable: "Service unavailable",
  failed: "Failed",
};

/** The word for a step waiting on a person. Not a failure. */
export const AWAITING_APPROVAL_LABEL = "Awaiting approval";

/**
 * Whether a step counts as **failed**.
 *
 * The single place the question is answered on the client, mirroring
 * `TurnStepStatus::is_failure` on the host. A parked step is deliberately not a
 * failure (#411).
 */
export function isFailedStep(status: TurnStepStatus | undefined): boolean {
  return status === "error";
}

/** One channel reply from a cycle. */
export interface OutboundMessage {
  channel: string;
  text: string;
  /**
   * The visible processing steps behind this reply (tool calls, thinking runs,
   * surfaced MCP failures). Omitted by the host when empty — a memory-served or
   * tool-less answer carries no steps, which is the tell that distinguishes it
   * from a tool-backed one.
   */
  steps?: TurnStep[];
  /** Channel-specific reply addressing. Absent on operator messages. */
  replyTo?: ReplyTo;
  /**
   * The board card this turn opened, when it opened one (issue #246). Drives
   * the reply bubble's "card opened" chip. Absent when the turn opened nothing
   * — which is every reply the host sent before this field existed.
   *
   * Only the *first* card of a turn that opened several: the journal field this
   * is persisted into is a single id, so the claim is incomplete but never
   * wrong. The bubble's `steps` timeline still shows every spawn.
   */
  taskId?: string;
  /**
   * Who this reply names, as the host resolved them (issue #1645).
   *
   * Absent when it names nobody, and on a host that predates the field.
   * When present the renderer can chip the resolved spans immediately
   * rather than waiting for the history-rehydration path - the live POST
   * response delivers the same mentions `chat/history` will later return.
   */
  mentions?: ChatMentionDto[];
    /**
   * The durable id this reply was journaled under (issue #364) — the id
   * `chat/history` will return for it. Absent on a reply the host could not
   * journal, and on a host that predates the field; either way the console
   * treats the bubble as un-threadable rather than inventing an id for it.
   */
  messageId?: string;
}

/** Channel-specific reply addressing. Mirrors `ReplyTo` in `src/ports/types.rs`. */
export interface ReplyTo {
  /** The chat/thread id to deliver back to. */
  chatId: string;
}

/**
 * `GET {scope}/desks` — one desk (group chat). Mirrors `DeskDto` in
 * `src/server/operator.rs`. The `id` doubles as the chat thread id; `members[0]`
 * is the desk's lead.
 */
export interface DeskDto {
  id: string;
  name: string;
  description?: string;
  /** Effective members: manifest members unioned with overlay additions. */
  members: string[];
  /**
   * The subset of `members` added through the operator overlay (issue #72).
   * Only these can be removed at runtime; manifest members are part of the
   * company blueprint. Omitted (undefined) when there are none.
   */
  overlayMembers?: string[];
  /**
   * How this desk's unmentioned messages find their answerer (issue #1835):
   * `"auto"` is a channel with **no lead** — `members[0]` carries no rank, and
   * the host picks a best-fit member per message — so every lead affordance
   * (crown, badge, Make lead) is suppressed for it. Omitted means `"lead"`,
   * which is every manifest desk and every desk created before the field
   * existed.
   */
  responder?: "lead" | "auto";
  /**
   * Whether the whole desk was operator-created (an overlay desk) rather than
   * declared in the manifest blueprint. The console offers a delete action only
   * for these. Omitted (undefined/false) for blueprint desks.
   */
  overlayCreated?: boolean;
}

/**
 * `GET {scope}/operator-channel` — the identity of the company's
 * always-present, durable Operator feed (issue #1757 rework): a read-only
 * "what happened" feed aggregating workflow-run reports and the owner/
 * no-mailbox fallback. Its own surface, not a desk — the console pins it
 * below a divider in the chat rail instead of folding it into `GET
 * {scope}/desks`. Mirrors `OperatorChannelDto` in `src/server/operator.rs`.
 */
export interface OperatorChannelDto {
  /** The channel id — the `desk` query param `chat/history` reads through. */
  id: string;
  /** Always "Operator" — the console's pinned-row label. */
  name: string;
  /** The channel's purpose line, shown under the name in the pinned row. */
  description: string;
}

/**
 * Body for `POST {scope}/desks` — create a desk. `name` is required; `id` is
 * derived from the name when omitted; `members` are optional roster teammate
 * ids (the first becomes the lead).
 */
export interface CreateDeskInput {
  name: string;
  description?: string;
  id?: string;
  members?: string[];
  /**
   * How the desk routes its unmentioned messages (issue #1835). Absent means
   * `"lead"` — today's model, what the org chart's create sends. `"auto"`
   * creates a leadless channel whose answerer is picked per message.
   */
  responder?: "lead" | "auto";
}

/**
 * `GET {scope}/chat/history` — one persisted transcript message. Mirrors
 * `ChatHistoryMessageDto` in `src/server/operator.rs`. Shares its filter +
 * projection logic with the GraphQL `Chat.history` resolver, so the two can
 * never disagree about a desk's history (issue #65).
 */
export interface ChatHistoryMessageDto {
  id: string;
  channel: string;
  author: string;
  text: string;
  atMillis: number;
  mine: boolean;
  /**
   * Whether a **person** typed this line rather than the runtime (issue #1734).
   *
   * `mine` answers a different question — "did *you* write it" — and is relative
   * to the reader, so a colleague's own message is `mine: false` and arrives on
   * the company side of the transcript beside the agent replies. Nothing here
   * can separate the two without this field, and the obvious substitute is a
   * trap: the offline echo brain names its own outbound channel `operator`,
   * exactly as an operator message does, so `channel === "operator"` matches
   * both.
   *
   * Optional because a host predating it omits it. `undefined` means "cannot
   * say", and the honest rendering of that is today's behaviour — never a
   * confident "the runtime wrote this".
   */
  byPerson?: boolean;
  /**
   * The scrubbed processing steps behind a company reply, so a rehydrated
   * transcript renders the same timeline the live turn showed. Omitted when
   * empty (operator messages, tool-less replies).
   */
  steps?: TurnStep[];
  /**
   * The board card this reply is about (issue #246) — the card the turn opened,
   * or the dispatched card it ran for (#185). Projected from the same shared
   * `MessageView` field the GraphQL `Chat.history` resolver reads, so the chip
   * renders identically whichever surface hydrated the transcript.
   */
  taskId?: string;
  /**
   * The message this one replies to (issue #364), by that message's own `id`.
   * Absent for a message posted straight into the channel — which is every
   * message journaled before threads were persisted.
   */
  parentId?: string;
  /**
   * Who reacted to this message with what (issue #364), one row per person per
   * emoji. Absent when nobody has, and on a host that predates the field.
   */
  reactions?: ChatReactionDto[];
  /**
   * Files attached to this message (issue #1682), each a reference into the
   * company workspace with the store-computed name / mime / size. Absent when
   * the message carries none — which is every reply, every system pill, and
   * every operator message journaled before the field existed — and on a host
   * that predates it.
   */
  attachments?: AttachmentDto[];
  /**
   * Who this message names, in reading order. Absent when it names nobody, and
   * on a host that predates the field.
   */
  mentions?: ChatMentionDto[];
}

/**
 * One file attached to a message (issue #1682). Mirrors `ChatAttachmentDto` in
 * `src/server/operator.rs`. Every field is store-authored metadata; the bytes
 * are fetched separately through the hardened `…/workspace/blob/{nodeId}` route.
 */
export interface AttachmentDto {
  /** The workspace node id the payload is stored under — handed to the blob
   * route to download or preview it. */
  nodeId: string;
  /** The stored file's display name. */
  name: string;
  /** The stored payload's media type, so the console decides download-vs-
   * preview without fetching the bytes. */
  mime: string;
  /** The stored payload's exact length in bytes. */
  size: number;
}

/**
 * What a mention points at. Mirrors the Rust `MentionTarget`.
 *
 * `everyone` is a scope rather than an actor, which is why this is a union and
 * not an `{ kind, id }` pair.
 */
export type MentionTarget =
  | { kind: "agent"; id: string }
  | { kind: "user"; id: string }
  | { kind: "desk"; id: string }
  | { kind: "everyone" };

/**
 * One mention as the composer *sends* it. Mirrors the Rust `Mention` input.
 *
 * Distinct from {@link ChatMentionDto}, which is what comes back: outgoing
 * carries the target the picker resolved, incoming carries the label the host
 * resolved it to. A client never sends a label and never receives a target.
 */
export interface ChatMentionInput {
  target: MentionTarget;
  /** The literal span typed, `@` included. */
  text: string;
  /** UTF-8 byte offset of `text` in the message body. */
  offset: number;
}

/** One mention. Mirrors `ChatMentionDto` in `src/server/operator.rs`. */
export interface ChatMentionDto {
  /** The literal span the author typed, `@` included. */
  text: string;
  /** Byte offset of `text` in the message body. */
  offset: number;
  /** Who was named, as a display label — never a raw user id. */
  label: string;
  /** Whether the reading viewer is the one named (or was named by @everyone). */
  mine: boolean;
  /** Whether this mention renders but pinged nobody. */
  quiet?: boolean;
}

/** One person's reaction. Mirrors `ChatReactionDto` in `src/server/operator.rs`. */
export interface ChatReactionDto {
  emoji: string;
  /** Who reacted, as a display label — never a raw user id. */
  by: string;
  /** Whether the reading viewer is the one who reacted. */
  mine: boolean;
}

/** Response of `/chat` and approval-resolution routes. */
export interface ChatResponse {
  responses: OutboundMessage[];
  /**
   * On a resolve: how many OTHER decisions the turn behind that approval is
   * still blocked on (issue #561). `0` means this decision released it.
   *
   * Absent on every other answer, and on a host that predates the field — which
   * the console reads as "cannot tell", and words its confirmation without a
   * claim about what happens next rather than guessing the optimistic one.
   */
  stillAwaiting?: number;
  /**
   * The durable id the operator's own message was journaled under (issue #364)
   * — the id `chat/history` will return for it. Absent on a host that predates
   * the field, which the console reads as "this message cannot be threaded or
   * reacted to" rather than guessing an id.
   */
  messageId?: string;
  /**
   * The durable turn row this message opened (issue #983) — pollable at
   * `GET {scope}/runs/{turnId}`. Additive: absent on a host that predates the
   * field, which the console reads as "this turn cannot be watched", falling
   * back to re-reading history.
   */
  turnId?: string;
  /**
   * On a resolve: which end state it reached (#1449).
   *
   * The Approvals page resolves **without** `detach`, so it never sees a
   * {@link ResolveReceipt} — this is the only shape that can tell it its click
   * was refused. Absent on every other answer, and on a host that predates the
   * field.
   */
  outcome?: ResolveOutcome;
}

/**
 * The answer to a **detached** chat post (issue #983): the turn has been
 * accepted, journaled and given an id, and that is all it claims. The reply
 * arrives afterwards on the event stream's `agent_reply` frame, and durably in
 * `chat/history`.
 *
 * `detached` is a constant `true` and exists to be *present*: a newer console
 * pointed at a host that predates the field sends `detach`, the host ignores it,
 * and the full synchronous body comes back. So the console can only tell the two
 * apart by what arrived — never by what it asked for.
 */
export interface DetachedChatResponse {
  /**
   * The turn's durable row, to poll. Optional for the same reason
   * `ChatResponse.turnId` is: a run store that refused a row does not get to
   * refuse the turn, so a detached turn can exist unwatched.
   */
  turnId?: string;
  /**
   * The durable id of the operator's own message. Never optional here — since
   * #983 the append happens at accept time, so it is already a fact when this
   * body is written. That is what lets the console reconcile its optimistic
   * bubble immediately instead of waiting for the turn to settle.
   */
  messageId: string;
  detached: true;
}

/** What `POST {scope}/chat` can answer with — settled, or accepted (#983). */
export type ChatPostResult = ChatResponse | DetachedChatResponse;

/**
 * Which shape came back, decided on the **response**, never on the request.
 *
 * Reads `detached` as a presence check rather than trusting `detach` was
 * honoured: an older host silently ignores the field and answers synchronously,
 * and a console that assumed otherwise would sit waiting for a reply it was
 * already holding.
 */
export function isDetachedChat(answer: ChatPostResult): answer is DetachedChatResponse {
  return (answer as DetachedChatResponse).detached === true;
}

/** One parked approval from `/approvals`. */
export interface ApprovalSummary {
  id: string;
  /** The parked effect's dotted kind, e.g. "payment.send". */
  kind: string;
  amount_usd: number | null;
  /**
   * Epoch-millis the effect was parked — stamped in the same turn that composed
   * its arguments, so it dates the **payload**, not the queue (#1024).
   */
  at_millis: number;
  /**
   * Epoch-millis this approval default-denies if nobody decides it (#971) —
   * `at_millis` plus the company's approval deadline
   * (`[policy].approval_ttl_hours`, 24 hours by default).
   *
   * **Never recompute it.** The host projects it from the gate that actually
   * enforces the deadline; a console that added its own 24 hours to
   * `at_millis` would show a deadline nothing enforces, and an operator would
   * act on "in 3h" and be refused.
   *
   * Optional because a host may predate the field. Absent means "this host
   * does not report deadlines" — render the card exactly as before rather
   * than guessing one.
   */
  expires_at_millis?: number | null;
  /**
   * The host's consequence group for the parked effect (#1024).
   *
   * Derived server-side from the tool **and its arguments**, so a
   * `composio_execute` carrying `GMAIL_SEND_EMAIL` arrives as `"send"` rather
   * than as the catch-all its tool name alone implies. It cannot be computed
   * here: for a harness tool call `kind` is the tool name, so a console keying
   * on `kind` would miss exactly the outbound sends this marks.
   *
   * Optional, and that is how an old host degrades: no field, no age label,
   * exactly the pre-#1024 card.
   */
  group?: "spend" | "send" | "sign" | "publish" | "hire" | "identity" | "other";
  /**
   * Which board task **owns** this approval (#333, resolved by #1891).
   *
   * Three states, deliberately: `{link: "task"}` is owned by that card,
   * `{link: "unlinked"}` is owned by no card (a workflow delivery, an
   * operator-chat turn, a scheduler tick), and *absent* means the park predates
   * the field. Only the last one is ambiguous — the server keeps a run-window
   * heuristic for it, and for nothing else.
   *
   * **The host's answer, not the park's stamp.** Until #1891 this mirrored the
   * raw `TaskLink` the parking cycle wrote, which is only the *fallback* half
   * of the host's ownership rule: the attempt behind a park outranks the card
   * it was stamped with wherever there is one, and the task detail read has
   * always applied that (`approval_owner`). So an approval parked under one
   * card's attempt and stamped with another's arrived here under the stamp, and
   * a client joining on it put the row on the wrong card. `pending_approvals_resolved`
   * now applies the same rule before serialising, so this field and
   * `…/tasks/{id}`'s `approvals` cannot disagree.
   *
   * That is what makes joining on it safe enough to hang a decision off — see
   * `approvalsForTask` in `@/lib/task-approvals`, which is how the board card
   * finds what it is blocked on and what it is offering to resolve.
   */
  task?: { link: "task"; id: string } | { link: "unlinked" };
  /** The host-resolved task owner, when an authoritative task-detail projection provides it. */
  ownerTaskId?: string;
  /**
   * The roster teammate whose blocked tool call this is (#372). Mirrors
   * `Effect::agent`: present exactly when the effect came from a harness tool
   * call, absent for a native effect the runtime performs itself.
   *
   * Optional because the host may predate the field — an old host omits it, and
   * the card then names no asker rather than showing a raw id or a guess.
   */
  agent?: string | null;
  /**
   * Whether the operator may grant this tool **broadly** — one standing
   * permission covering any arguments until a deadline (#374).
   *
   * `true` exactly when the effect came from a harness tool call and its
   * consequence group is the catch-all one. Anything that spends, sends, signs,
   * publishes, hires or touches identity stays a per-call decision, so the scope
   * control is not rendered for it.
   *
   * **This is a hint, not the boundary.** The host re-checks the same rule when
   * the resolve arrives and answers 400, so ignoring this field buys nothing.
   * Optional and defaulting to absent, which is how an old host degrades: no
   * field, no control, approve-once exactly as before.
   */
  broadly_grantable?: boolean;
  /** Whether a time-bounded standing refusal can be created for this tool. */
  broadly_deniable?: boolean;
  /**
   * What the effect will actually do — the tool call's arguments (#372).
   *
   * Already redacted and bounded by the host
   * (`src/runtime/approval_display.rs`): credential-named keys arrive as the
   * literal string `"[redacted]"`, long strings are truncated, and an oversized
   * subtree arrives as `"[unrenderable]"`. The console never has to decide what
   * is safe to show, and must not try to "unredact" anything.
   *
   * `unknown` rather than a shape: the payload is an arbitrary tool argument
   * object, so every read of it is a narrowing one.
   */
  payload?: unknown;
  /**
   * Whether {@link payload} and {@link amount_usd} were withheld from **this
   * reader** because of their role (#618).
   *
   * Membership decides whether you may know an approval exists; role decides
   * whether you may read its contents. An admin never sees this set.
   *
   * The console must not treat a withheld approval as an empty one. `payload`
   * being absent already means "the effect carries no arguments", so without
   * this flag a hidden payment and a no-argument tool call are the same bytes —
   * and a member would read "nothing to show" where the truth is "not shown to
   * you". Render the difference; see `ApprovalPayload`.
   */
  contents_hidden?: boolean;
  /**
   * The chat thread this approval was raised in (#379) — the **host** thread id,
   * which is a desk id for a channel and a roster agent id for a direct message.
   * Resolve it to a console channel id with `channelIdForThread`.
   *
   * Not derivable from {@link agent}: a desk channel and a direct message to
   * that desk's lead are answered by the same teammate, so placing a card by
   * asker would raise one conversation's request inside the other.
   *
   * Absent for an approval no conversation produced (a workflow delivery, a
   * scheduler tick) and for one parked before the field existed. Both mean the
   * same thing here: it matches no channel and belongs to the Approvals page
   * alone, exactly as every approval did before this shipped.
   */
  thread?: string | null;
  /**
   * Which **workflow run** parked this approval (#880) — the run's correlation
   * id, the same value `WorkflowRunResult.runId` carries back to the console
   * that pressed Run.
   *
   * The join, and the only one there is: it is what lets a second surface — the
   * run drawer (#1002) — show the cards *this* run is held on without the host
   * growing a run-scoped approvals route. Compare it for equality and nothing
   * else; like every other id here it never reaches the screen.
   *
   * **Absent is not "unknown", it is "no workflow run behind this card"** — a
   * chat turn, a scheduler tick, a task attempt. The host stamps it only for an
   * *unlinked* park that carries a run id, precisely because `Effect::run_id`
   * also carries task-attempt ids that must never be read as a workflow run
   * (`workflow_run_of` in `src/company/runtime.rs`). Absent on a host predating
   * the field too, and both must read the same way: such a card belongs to the
   * Approvals page alone, exactly as every approval did before this shipped.
   *
   * Snake-case because the REST projection is
   * (`ApprovalSummary::workflow_run_id` in `src/runtime/types.rs`); the GraphQL
   * schema camel-cases the same field, and this console reads REST.
   */
  workflow_run_id?: string | null;
  /**
   * Which **workflow** a parked `workflow.approve` gate is asking about
   * (#1418) — the second half of the run address, beside {@link workflow_run_id}.
   *
   * A run id alone cannot name a console page, so this is what turns a native
   * workflow approval into an "Open the run" link.
   *
   * **Deliberately not read from {@link payload}.** Payload is a redacted
   * rendering, and role redaction (#618) strips it from a member reader
   * entirely; the host projects this top-level field from the raw parked effect
   * (`gate_workflow_id`) so it survives redaction the way `workflow_run_id`
   * already does — a member holding up a stalled workflow keeps the address.
   *
   * Absent on every non-gate approval (a chat turn, a scheduler tick) and on a
   * tool call parked *by* a workflow; only native `workflow.approve` effects
   * carry it. Optional because an old host predates the field.
   */
  workflow_id?: string | null;
  /**
   * Which turn's gated calls this one belongs to (#842) — an opaque key shared
   * by every approval a single agent turn parked.
   *
   * **A display grouping, never a decision.** One research turn that reaches
   * three sites parks three approvals, and each stays its own record with its
   * own id, its own approve/decline and — on approve — its own host-scoped
   * grant (#739). The conversation consolidates them into one card so it
   * interrupts once instead of three times; the Approvals page deliberately
   * keeps one row per approval, matching how `Standing permissions` lists one
   * revocable row per grant. Resolving is per id on both surfaces.
   *
   * Never compare it for anything but equality, and never show it: it is a
   * runtime identifier, which the glossary rule keeps off an operator's screen.
   *
   * Absent for an approval no turn raised (a workflow node, a scheduler tick)
   * and against a host that predates the field. Both are grouped alone, which
   * is exactly the pre-#842 rendering — so an old host still produces a card
   * that can be decided.
   */
  batch?: string | null;
}

/**
 * **Which** end state a resolve reached (#1449).
 *
 * A resolve can succeed as a *request* and still not be the operator's
 * decision, and the console has to be able to tell those apart — the whole of
 * #1449 is that it could not, so it rendered the success line over a click the
 * host had refused.
 *
 * * `settled` — the verdict is the operator's and it is recorded.
 * * `expired` — the approval was still queued but past its deadline, so the
 *   host default-denied it whatever the button said. **Nothing was carried
 *   out**, and nothing was recorded against the operator's name.
 * * `already_resolved` — there was nothing left to resolve. The click changed
 *   nothing. Could be a double-submit, another operator, another tab, or the
 *   sweeper retiring it a moment earlier; the host cannot tell which, and
 *   neither may the wording.
 */
export type ResolveOutcome = "settled" | "expired" | "already_resolved";

/**
 * The answer to a **detached** resolve (#383): the verdict is durable, and that
 * is all it claims. The agent's continuation arrives afterwards on the event
 * stream's `agent_reply` frame.
 */
export interface ResolveReceipt {
  recorded: boolean;
  /** There was nothing left to resolve — a double-click, not a failure. */
  alreadyResolved: boolean;
  /**
   * Which end state this resolve reached (#1449). Absent on a host that
   * predates the field, which the console reads as "cannot tell" and words its
   * confirmation exactly as it did before rather than guessing.
   */
  outcome?: ResolveOutcome;
  /**
   * How many OTHER decisions the turn behind this approval is still blocked on
   * (issue #561). `0` means this decision released it; absent on a host that
   * predates the field.
   */
  stillAwaiting?: number;
}

export type Verdict = "approve" | "deny";

/**
 * What an approve buys (#374).
 *
 * `once` is the default and needs no interaction: one call, with exactly the
 * arguments the operator saw. `tool` is the broader option, and its duration is
 * mandatory — there is no unbounded form, on the wire or in the UI.
 */
export type GrantScope = { kind: "once" } | { kind: "tool"; expiresInMillis: number };

/** The duration options offered with the broader scope. Nothing else is valid. */
export const GRANT_DURATIONS: { label: string; millis: number }[] = [
  { label: "1 hour", millis: 60 * 60 * 1000 },
  { label: "8 hours", millis: 8 * 60 * 60 * 1000 },
  { label: "7 days", millis: 7 * 24 * 60 * 60 * 1000 },
];

/**
 * One standing permission, as `GET {scope}/grants` returns it (#374).
 *
 * Carries **no arguments** — a standing grant has none, which is what makes it
 * structurally unable to be widened into an argument-matching rule, and why this
 * list needs no redaction of its own.
 */
export interface BudgetPauseMarker {
  id: string;
  agent: string;
  chatId?: string;
  message: string;
  summary: string;
  atMillis: number;
}

export interface StandingGrant {
  id: string;
  agent: string;
  workflow?: string;
  tool: string;
  verdict: Verdict;
  granted_by: { kind: string; id: string };
  at_millis: number;
  expires_at_millis: number;
  scope?: string;
}

export type FeedbackCategory =
  | "wrong-output"
  | "bug"
  | "missing-capability"
  | "approval-friction"
  | "template-gap"
  | "docs";

export interface FeedbackInput {
  category: FeedbackCategory;
  note: string;
  work_ref?: string;
  preview?: boolean;
  /** Confirm the previewed item by id (Send after Preview). */
  item_id?: string;
}

/**
 * Where a submitted report ended up. `tinyhumans` means the instance is
 * provisioned with a credential and the report was recorded against its owner;
 * `github` is the unprovisioned filing path; `local` means it never left.
 */
export type FeedbackDestination = "local" | "tinyhumans" | "github";

/** Response of `/feedback`. */
export interface FeedbackResponse {
  item_id: string;
  destination: FeedbackDestination;
  filed: boolean;
  blocked: boolean;
  reason?: string;
  preview_body?: string;
  prefilled_url?: string;
  issue_url?: string;
  deduped: boolean;
}

/**
 * One past report from `GET .../feedback`. Deliberately omits the operator's
 * own words, which never leave the host that captured them.
 */
export interface FeedbackSummary {
  id: string;
  category: FeedbackCategory;
  work_item: string | null;
  at_millis: number;
  filed_issue_url: string | null;
  issue_status: string | null;
}

/**
 * The shared feedback board (`GET .../feedback/board`).
 *
 * The board is not this host's: it lives on the TinyHumans hub, where every
 * product's operators file into the same list, and the host proxies it so the
 * console never holds a hub credential. An instance with no credential has no
 * board and every board route 404s with `tinyhumans_no_board` — the console
 * hides the surface rather than rendering an empty one.
 */
export type BoardKind = "feature" | "bug";

/** Where an item sits in the hub's triage. */
export type BoardStatus = "open" | "planned" | "completed" | "closed";

/** The orderings the board exposes. */
export type BoardSort = "hot" | "top" | "new";

/** `1` up, `-1` down, `0` no vote (or a retracted one). */
export type BoardVote = 1 | -1 | 0;

/** One row on the board. */
export interface BoardItem {
  id: string;
  kind: BoardKind;
  title: string;
  body: string;
  status: BoardStatus;
  author: string | null;
  upvotes: number;
  downvotes: number;
  score: number;
  comment_count: number;
  /**
   * This *instance's* vote, not this operator's: every console on a host votes
   * through the one hub account the host is provisioned with.
   */
  my_vote: BoardVote;
  issue_url: string | null;
  /** ISO-8601, as the hub reports it. */
  created_at: string;
}

/** One comment on a board item. */
export interface BoardComment {
  id: string;
  author: string | null;
  body: string;
  created_at: string;
}

/** One page of board rows, plus the total the query matches. */
export interface BoardPage {
  items: BoardItem[];
  total: number;
  page: number;
  limit: number;
}

/** One board item with its comments. */
export interface BoardDetail {
  item: BoardItem;
  comments: BoardComment[];
}

/** The query one board page is fetched with. */
export interface BoardQuery {
  sort?: BoardSort;
  kind?: BoardKind;
  status?: BoardStatus;
  page?: number;
  limit?: number;
}

/**
 * `GET /spec` — the host's runtime specification. Unauthenticated, so the
 * console can read it before (and regardless of) a session.
 */
export interface AppSpec {
  name: string;
  version: string;
  api_url: string;
  /**
   * Whether hosted cognition can run, which is true exactly when this instance
   * has a TinyHumans credential and a hosted brain. The console uses it as the
   * "is this instance provisioned" signal. No secret bytes are surfaced.
   */
  cycles_available: boolean;
  /**
   * Whether the first-run setup flow has been completed on this instance.
   *
   * Reported on this unauthenticated handshake because an instance nobody has
   * configured has nobody who *can* sign in — gating the answer behind auth
   * would make the setup wizard unreachable exactly when it is needed.
   *
   * Optional: a host predating the field omits it, and the console must read
   * `undefined` as "assume configured" rather than showing a wizard that host
   * has no route for.
   */
  setup_complete?: boolean;
}

/**
 * One agent in the company's roster, from `GET .../team`. Forward-looking:
 * hosts that don't expose the roster yet 404, and the console falls back to a
 * locally-editable starter team. Mirrors a `company.toml` `[[agent]]` entry.
 */
export interface TeamMemberDto {
  id: string;
  /** Display name; falls back to the role when a company only names roles. */
  name?: string;
  role: string;
  description?: string;
  /**
   * The face somebody chose for this teammate (`lib/avatar.ts`) — a
   * `tiny:<flavour>` mascot or a `blob:<nodeId>` upload.
   *
   * Absent means **nobody has chosen**, which is not "no face": the console
   * draws the mascot it hashes from the id. Never default it to a flavour — the
   * distinction is what makes "reset to the default face" offerable.
   */
  avatar?: string;
  /**
   * Whether this teammate has an enabled inbox, as the host's `InboxStore` sees
   * it. Absent on hosts predating the field; the console reads that as `false`.
   */
  inboxEnabled?: boolean;
  /**
   * This teammate's daily spend cap in USD, as the host will actually enforce
   * it: an operator override set from this console when one exists, otherwise
   * the manifest's `budget_usd_daily`.
   *
   * Absent when the teammate is uncapped — absence IS the uncapped signal, so
   * never default it to `0`, which would render a permanently exhausted
   * teammate.
   */
  budgetUsdDaily?: number;
  /**
   * What this teammate has spent since 00:00 UTC. Sent only alongside
   * `budgetUsdDaily`; absent for an uncapped teammate and on hosts predating
   * the field.
   */
  spentTodayUsd?: number;
  /**
   * The user id of the admin who last set this teammate's cap from the console.
   * Absent when no override is stored — i.e. when the cap (if any) is just the
   * company's manifest default.
   *
   * Deliberately NOT paired with `budgetUsdDaily`: it is present even for an
   * override that *removed* a cap, which is the only way to tell "nobody has
   * touched this" from "an admin deliberately uncapped this".
   */
  budgetSetBy?: string;
  /** When that cap was set (epoch millis). Paired with `budgetSetBy`. */
  budgetSetAtMillis?: number;
  /**
   * Whether this teammate came from the **global baseline** — the agents,
   * workflows and skills every company gets whichever vertical it started from
   * (`docs/spec/runtime/globals.md`) — rather than from this company's own
   * roster or from an operator.
   *
   * Provenance, and the field first-run setup is gated on (issue #1404). The
   * baseline is merged into every company whatever its manifest says, so
   * `roster.length === 0` is never true and the gate that used it could never
   * open — including on `companies/e2e_setup`, the fixture that exists solely
   * to reach that flow. Read this rather than testing ids against a hard-coded
   * list of baseline agents, which re-breaks the moment the baseline changes.
   *
   * **Optional on the type, not on the wire**: a host predating the field omits
   * it, and `undefined` means "this host cannot say". The setup gate reads that
   * as *not* baseline, which is the conservative answer — counting an unknown
   * row as baseline would offer setup to a company that already has a team and
   * stack a second one on it.
   */
  global?: boolean;
  /**
   * The declared cognition-tier hint (`[[agent]].tier`) verbatim, from the same
   * host-side helper that answers `GET .../team/{agentId}` (issue #643).
   *
   * **Optional on the type, and genuinely absent on the wire** for a teammate
   * that declares none — which is a different statement from any tier string.
   * Do not coalesce it to a default: the overview graph stamped a literal
   * `"worker"` on every node for exactly this reason, so a company declaring
   * `tier = "orchestrator"` read back as a worker on its own graph. `undefined`
   * means "cannot say", and the honest rendering of that is "not declared".
   */
  tier?: string;
  /**
   * Whether this teammate is the company's orchestrator (issue #643).
   *
   * **NOT the same question as `tier`** — this is the host's roster rule (the
   * agent tagged with the orchestrator tier, else the first declared agent),
   * and the two disagree in both directions: a company that tags nobody still
   * has an orchestrator (no `tier`, `true` here), and a second agent tagged
   * with that tier is not one (`tier` present, `false` here). Never re-derive
   * this from the tier string; read it.
   *
   * Optional only because a host predating #643 does not send it, in which case
   * `undefined` means "this host cannot say" — draw no marker rather than
   * guessing one from `tier`.
   */
  isOrchestrator?: boolean;
  /**
   * This teammate's tool grants (issue #601) — the **same** three lists, from
   * the same host-side constructor, that `GET .../team/{agentId}` serves.
   *
   * On the list because the overview graph draws a ring of each teammate's
   * tools and is built from the roster read: without this it would have to
   * fetch every agent's detail on page load, and it invented a tool shelf
   * instead. Read `effective` and nothing else when the question is "what does
   * this agent hold" — see {@link AgentToolsDto} for why `requested` alone
   * inverts the answer.
   *
   * **Optional on the type, not on the wire.** A host predating #601 sends no
   * such field; `undefined` means "this host cannot say", which is a different
   * statement from an empty `effective` ("holds nothing") and must not be
   * collapsed into it.
   */
  tools?: AgentToolsDto;
  /**
   * The desks this teammate sits on (issue #601), same shape as the detail
   * read. Desks are the company's real grouping, so these are what the
   * overview graph draws its department pillars from.
   *
   * **Optional on the type, not on the wire**, same rule as `tools`: absent
   * means the host does not answer, empty means "on no desk".
   */
  desks?: AgentDeskDto[];
}

/**
 * One agent in full, from `GET .../team/{agentId}` (issue #264).
 *
 * `GET .../team` answers "who is on the roster"; this answers "what is this
 * agent". Everything below the identity block was unreachable from the console
 * before this route existed, and the tool grants were unreachable from
 * *anywhere* — which is why a change to what a company grants its agents could
 * not be checked from outside the process.
 */
export interface AgentDetailDto {
  id: string;
  /** Absent for a manifest teammate, which is named by its role. */
  name?: string;
  role: string;
  /**
   * What the agent was defined with. This text frames the agent's persona on
   * every turn, so it is the closest thing the company has to an `AGENT.md`.
   */
  description?: string;
  /**
   * The persona instructions **in force** for this teammate (issue #1530): the
   * per-agent override when one is set, otherwise the blueprint seed. This is
   * the text the agent actually runs with, and the draft an edit starts from.
   */
  instructions?: string | null;
  /**
   * The blueprint's own instructions — the manifest seed a manifest teammate
   * was declared with, kept beside the effective value so the console can show
   * what "Reset to blueprint" restores. Absent for a bare overlay teammate,
   * which has no blueprint.
   */
  blueprintInstructions?: string;
  /**
   * Whether an override is currently masking the blueprint. The console shows
   * the Reset-to-blueprint control exactly when this is true — an agent running
   * on its blueprint has nothing to reset.
   */
  instructionsOverridden?: boolean;
  /**
   * Which half of the roster this teammate comes from. `manifest` teammates are
   * declared in the version-controlled `company.toml`; `overlay` teammates were
   * added at runtime. Both are editable and both are removable — a manifest
   * teammate's edits are stored as an override on the company record and its
   * removal as a tombstone, so `company.toml` is never rewritten either way. The
   * only refusal is the company's last teammate.
   */
  source: "manifest" | "overlay";
  /**
   * The field names the host will accept in a `PATCH`. **The console renders a
   * field read-only exactly when this list omits it** rather than deciding for
   * itself — a client-side copy of the rule would eventually disagree with the
   * host, and the operator would meet the disagreement as a failed save.
   */
  editable: string[];
  /** The declared cognition-tier hint, when the manifest sets one. */
  tier?: string;
  /**
   * Which declared harness this teammate runs on, by id (issue #1245's
   * harness-picker follow-up). `undefined` means the harness marked
   * `default = true` — read `GET {scope}/harnesses` ([`HarnessDto`]) for the
   * full declared set, including which one that is.
   */
  harness?: string;
  /**
   * This teammate's own model override, when it has one (issue #1245's
   * per-agent follow-up). Meaningful only when the teammate runs on an ACP
   * harness (an operator's own coding CLI) — the host does not tell this
   * response which harness that is, so the console shows it as informational
   * rather than validating it against one.
   */
  model?: string;
  /**
   * Whether this teammate is the company's orchestrator. Resolved by the roster
   * rule (a tagged tier first, else the first declared agent), so it is NOT the
   * same question as `tier === "orchestrator"`: a company that tags nobody still
   * has one.
   */
  isOrchestrator: boolean;
  tools: AgentToolsDto;
  desks: AgentDeskDto[];
  inboxEnabled: boolean;
  /**
   * The face somebody chose for this teammate, absent when nobody has — the
   * same field and the same contract as `TeamMemberDto.avatar`.
   */
  avatar?: string;
  /** The cap in force and its attribution; same absent-means-uncapped contract as `TeamMemberDto`. */
  budgetUsdDaily?: number;
  spentTodayUsd?: number;
  budgetSetBy?: string;
  budgetSetAtMillis?: number;
}

/**
 * An agent's tool grants at all three levels.
 *
 * The distinction is the point: `requested` is what the agent's own `tools`
 * line asks for, `companyAllow` is the ceiling it is intersected with, and
 * `effective` is what the agent actually holds. Since issue #1804 `requested`
 * is three-state: **`null` means the company's standard grant** (the agent
 * lists no tools of its own and inherits `[tools].allow`), an **empty array
 * `[]` is a deliberate deny-all** (holds nothing), and a **non-empty array
 * narrows**. A surface that treats `null` and `[]` alike reports the opposite
 * of the truth for exactly those agents.
 */
export interface AgentToolsDto {
  requested: string[] | null;
  companyAllow: string[];
  /**
   * The ceiling contributed by the desks this agent sits on — the union of
   * their `tools`, already narrowed by `companyAllow`. **Empty means the
   * narrowed ceiling grants nothing**, not "no desk narrows anything" — see
   * `deskCeilingActive`, which tells those apart.
   */
  deskAllow: string[];
  /**
   * Whether any desk this agent sits on states a `tools` ceiling. Distinct
   * from `deskAllow`: a ceiling can be active yet narrow to an empty list
   * (a desk whose only grant the company does not allow), and the preview
   * must keep the desk level as the gate in that case instead of falling
   * back to `companyAllow`.
   */
  deskCeilingActive: boolean;
  effective: string[];
}

/** A desk this agent sits on, and whether it leads it. */
export interface AgentDeskDto {
  id: string;
  name: string;
  /** The desk's first effective member, who receives a `delegate_to_desk` hand-off. */
  lead: boolean;
}

/**
 * The body of `PATCH .../team/{agentId}` (issue #264).
 *
 * A patch, not a replacement: an omitted key leaves that field alone. That is
 * why `description` is `string | null | undefined` and the three are all
 * different — `undefined` leaves the instructions be, `null` clears them, and a
 * string sets them. Building this object with a spread that drops `undefined`
 * is correct; one that turns `undefined` into `null` would erase an agent's
 * instructions on every partial save.
 */
export interface EditAgentInput {
  name?: string;
  role?: string;
  description?: string | null;
  /**
   * The persona instructions, three-state exactly like `description` (issue
   * #1530): `undefined` leaves the override untouched, `null` clears it —
   * resetting the teammate to its blueprint — and a string sets it. The three
   * are different on the wire (`JSON.stringify` keeps `null`, drops `undefined`)
   * and must never be collapsed, or a partial save would silently reset a
   * persona the operator did not touch.
   */
  instructions?: string | null;
  /**
   * The face this teammate wears, three-state exactly like `instructions`:
   * `undefined` leaves it alone, `null` resets it to the mascot the console
   * hashes from the id, and a reference (`tiny:<flavour>` / `blob:<nodeId>`)
   * sets it. See `lib/avatar.ts`.
   */
  avatar?: string | null;
  /**
   * The teammate's own model override (issue #1245's per-agent follow-up).
   * Same double-option shape as `description`: absent leaves it alone, `null`
   * clears it back to the harness's own default, and a string sets it.
   * Admin-only on the host, alongside `tools` — a member's `PATCH` carrying
   * this key gets a `403`.
   */
  model?: string | null;
  /** Which declared harness this teammate runs on. */
  harness?: string | null;
  /**
   * The teammate's own tool-grant globs, three-state since issue #1804 (like a
   * double-`Option` on the wire): `undefined` leaves the grant untouched,
   * `null` resets it to the standard company-wide grant, an empty array `[]` is
   * a deliberate deny-all (holds nothing), and a non-empty array narrows. The
   * four are different on the wire (`JSON.stringify` keeps `null`/`[]`, drops
   * `undefined`) and must never be collapsed, or a partial save would silently
   * re-scope a grant the operator did not touch.
   */
  tools?: string[] | null;
}

/** One declared or detected harness. */
export interface HarnessDto {
  id: string;
  kind: "built_in" | "acp";
  default: boolean;
  agent?: string;
  runsHere?: boolean;
  transport?: string;
  detected: boolean;
}

/**
 * The body of `PUT .../team/{id}/budget`.
 *
 * `budgetUsdDaily` must always be present — `null` to remove the cap, a number
 * to set one (including `0`, which caps at nothing). The host rejects a body
 * that omits the key with a 422 rather than reading it as "remove the cap", so
 * never build this object conditionally.
 */
export interface SetBudgetInput {
  budgetUsdDaily: number | null;
}

/**
 * One teammate inbox's non-secret status, from `GET .../inboxes`. Both inbound
 * paths (the ingest webhook and the IMAP poller) file into the same store this
 * projects, so received mail shows up here.
 */
/**
 * One agent-authored dashboard page's manifest, from `GET {scope}/pages`.
 * Mirrors the `page.toml` a page's `pages/<slug>/` bundle carries
 * (`docs/spec/runtime/pages.md`) — the page's own compiled bundle is served
 * separately, at `GET {scope}/pages/{slug}` (the iframe host document) and
 * `GET {scope}/pages/{slug}/bundle.mjs` (the compiled JS).
 */
export interface PageManifestDto {
  slug: string;
  title: string;
  /** Optional in the DTO: `page.toml`'s `description` is omitted when absent. */
  description?: string;
  /** Optional in the DTO: `page.toml`'s `icon` is omitted when absent. */
  icon?: string;
  navVisible: boolean;
}

export interface InboxDto {
  /** The inbox key (a teammate's local part / slug). */
  key: string;
  /** The teammate's display name. */
  name: string;
  /** The full address (`{key}@{domain}` when a domain is configured). */
  address: string;
  /** Whether the inbox is enabled on this teammate's detail page. */
  enabled: boolean;
  /** The number of unread received (inbound) messages. */
  unread: number;
}

/** One email in an inbox, from `GET .../inboxes/{key}/messages`. */
export interface InboxMessageDto {
  id: string;
  /** The inbox this belongs to (the teammate local part). */
  inbox: string;
  /** The sender's display name (may be empty). */
  fromName: string;
  /** The sender's email address. */
  fromEmail: string;
  subject: string;
  /** Plain-text body. */
  body: string;
  /** When it arrived / was sent, epoch millis. */
  atMillis: number;
  read: boolean;
  /** True for a sent message, false for a received one. */
  outbound: boolean;
}

/**
 * Which route a Connect for one provider can take on this host. Deliberately
 * the same vocabulary as `ComposioCredentialSource` (`api/composio.ts`) — the
 * two console surfaces answer the same question and should read the same to an
 * operator.
 *
 * - `attested` — this instance carries a platform-minted identity, so
 *   connections are the platform's to run. Nothing to register here, and the
 *   console offers no local Connect.
 * - `company` — this company's own TinyHumans credential (issue #586). Reported
 *   by the Composio plane, which brokers through it. The native OAuth catalog
 *   does **not** route through the company key today, so this value does not
 *   appear on a native-only provider — see `api/credential.ts`.
 * - `static` — a legacy native OAuth token this company already stored. It is
 *   visible and revocable, but no agent can use it.
 * - `none` — neither. A registered native provider application also lands
 *   here: its start route is a dated 410 retirement bridge (issue #838), not a
 *   connection route.
 */
export type ConnectionCredentialSource = "attested" | "company" | "static" | "none";

/**
 * One third-party connection's state, from `GET .../connections`.
 * Forward-looking: hosts that don't expose the connections surface yet simply
 * 404, and the console treats connections as unavailable.
 */
export interface ConnectionState {
  /** Provider id, matching the console's connection catalog (e.g. "slack"). */
  provider: string;
  connected: boolean;
  /**
   * Which credential route a Connect for this provider would take — a tier
   * name, never a credential. Optional so a host predating issue #319 (which
   * omits the field) still parses; the view falls back to today's behaviour.
   */
  credentialSource?: ConnectionCredentialSource;
  /** The connected account label, when known (e.g. an email or workspace). */
  account?: string;
  /**
   * Which namespace(s) report this provider connected — `native` for the
   * host's own `oauth/{provider}` catalog, `composio` for a Composio
   * connection. Empty when not connected (issue #316).
   *
   * The host reconciles both namespaces into one row, so the page can no
   * longer show the same provider twice with two different answers. Optional
   * so a host predating this field still parses.
   */
  via?: ("native" | "composio")[];
  /**
   * A Composio path exists for this company but could not be read, so
   * `connected: false` means "we could not check", not "no". Render that
   * distinction rather than a confident disconnected state.
   */
  unverified?: boolean;
}

/** The coarse health tier of an MCP server, from a probe. */
export type McpStatus = "ok" | "needs_config" | "error" | "unknown";

/**
 * The last (scrubbed) probe outcome for an MCP server. `message` is always
 * scrubbed on the host — it can never carry a credential, response body, or URL
 * query string.
 */
export interface McpHealth {
  status: McpStatus;
  message: string;
  toolCount: number;
  checkedAtMillis: number;
  /** A stable auth-failure reason code, when the status is a credential problem. */
  authHint?: string;
}

/**
 * One roster agent named on a console coverage line — {@link
 * McpServer.reachableBy} ("Reachable by") and the console's other coverage lines'
 * `grantedAgents` ("Readable by"), both computed from one roster walk on the
 * host.
 *
 * `name` is the display label the rest of the console uses for that teammate (a
 * manifest teammate's role, an operator-added teammate's name) and is the only
 * thing worth showing a reader: an operator-added teammate's `id` is a minted
 * internal string, which both lines used to print raw (issue #931). The id is
 * still carried so a client can key or link on it.
 */
export interface RosterAgent {
  id: string;
  name: string;
}

/**
 * How a server got into the company's effective set.
 *
 * - `manifest` — committed in `company.toml`'s `[[mcp_server]]`.
 * - `runtime` — typed into the console by URL.
 * - `default` — shipped enabled by the packaged install (issue #527). Nobody
 *   wrote it into *this* company, so it is not "operator-added".
 * - `registry` — installed from an upstream MCP directory through the browse
 *   surface (issue #1270). Keyed by {@link McpServer.serverId}, not by name,
 *   and addressed by the `…/mcp/registry/…` routes rather than List A's.
 */
export type McpSource = "manifest" | "runtime" | "default" | "registry";

/**
 * One effective MCP tool server (issue #50), as `.../mcp/servers` returns it.
 * The credential is never present — only the non-secret `authConfigured` flag
 * and the last (scrubbed) probe `health`.
 *
 * Since issue #1270 this one list carries directory installs too. The four
 * registry fields below are all optional and omitted by the host on a row with
 * no install behind it, so a manifest / runtime / default row arrives exactly
 * as it always did.
 */
export interface McpServer {
  name: string;
  endpoint: string;
  description?: string;
  /**
   * Where this row came from. **The console's single source of provenance** —
   * it decides the badge, the delete guard, and which set of routes the row's
   * controls may call.
   *
   * A row that is *both* a directory install and a List A declaration is one
   * reconciled row carrying List A's provenance, not `registry` (issue #1270).
   * Never re-derive provenance from {@link McpServer.serverId} being present:
   * a manifest server that was also installed from the directory carries a
   * `serverId` and must still badge `manifest` and still refuse a delete.
   */
  source: McpSource;
  enabled: boolean;
  allowedTools: string[];
  disallowedTools: string[];
  /**
   * Remote tool names the operator has declared **read-only** on this server
   * (issue #1124). A bridge call to one is priced as an outward read rather than
   * parked for approval, so it can run unattended under `auto`; every other call
   * through the server still parks. Independent of {@link McpServer.allowedTools}
   * / {@link McpServer.disallowedTools} — it says nothing about whether a tool is
   * exposed, only how a call to it is gated. Carries this row's provenance badge
   * exactly as the two lists above do.
   */
  readOnlyTools: string[];
  timeoutSecs: number;
  /** Whether an outbound credential is stored — never the credential itself. */
  authConfigured: boolean;
  /**
   * The company's agents whose effective tool grants cover this server — who can
   * actually call it (issue #568). On an **enabled** server an empty array means
   * no teammate can reach it, a probable misconfiguration the console flags
   * loudly. A **disabled** server is always empty (the harness hands out no tool
   * for it whatever the grants say), so the console reads the empty case against
   * `enabled` and stays quiet there. Optional only for forward-compat with an
   * older backend that does not send the field; `undefined` (unknown) is treated
   * differently from `[]` (known-empty).
   *
   * Render {@link RosterAgent.name}, never the id (issue #931).
   */
  reachableBy?: RosterAgent[];
  /** The last recorded (scrubbed) probe outcome, when the server has been probed. */
  health?: McpHealth;
  /**
   * The stable install id, present only on a row backed by a directory install
   * (issue #1270). Every `…/mcp/registry/{serverId}/…` route keys on this;
   * `name` is a display slug the host mints for the row and addresses nothing
   * in the registry's own store.
   *
   * Present does **not** mean `source === "registry"` — a reconciled row is a
   * List A server that also has an install. See {@link McpServer.source}.
   */
  serverId?: string;
  /** The directory's qualified name (`@org/server`), when this row came from one. */
  qualifiedName?: string;
  /** The directory's icon, when this row came from one. */
  iconUrl?: string;
  /** How an install is dialled — `http_remote` or `stdio`. Absent on a List A-only row. */
  transport?: string;
}

/**
 * A mutating MCP response: the resulting server, the host's pickup note (since
 * issue #566 that is next-turn pickup with no restart, not a rebuild reminder),
 * the live probe result (absent on a non-`openhuman` host), and any
 * non-blocking endpoint advisory.
 */
export interface McpMutationResponse {
  server: McpServer;
  note: string;
  /** The probe result from right after the mutation (the server is never rolled back). */
  test?: McpHealth;
  /** A non-blocking advisory (e.g. a secret-looking query string in the URL). */
  warning?: string;
}

/** One remote tool advertised by an MCP server (live discovery). */
export interface McpToolInfo {
  name: string;
  title?: string;
  description?: string;
  inputSchema: unknown;
}

/** One capability tier's budget status (issue #108). */
export interface CapabilityTierDto {
  /** The exec tool namespace this tier gates (`shell` / `code` / `web` / `subagent`). */
  namespace: string;
  /** Tokens allowed this period. */
  budgetTokens: number;
  /** Tokens spent this period (company-wide — no per-tier attribution). */
  spentTokens: number;
  /** `budget - spent`, floored at zero. */
  remainingTokens: number;
  /** Whether spend has reached the threshold — the tier's tools are disabled. */
  exhausted: boolean;
}

/**
 * The plan-level total token ceiling (issue #188). Unlike a per-namespace tier
 * — a *soft* gate that only trims exec tools — crossing this is a *hard* stop:
 * the harness refuses to dispatch further turns this period. Present only when
 * the manifest set `[plan].total_tokens`.
 */
export interface CapabilityTotalDto {
  /** Total tokens allowed this period before dispatch is refused. */
  budgetTokens: number;
  /** Tokens spent this period. */
  spentTokens: number;
  /** `budget - spent`, floored at zero. */
  remainingTokens: number;
  /** Whether spend has reached the ceiling — dispatch is paused until reset. */
  exhausted: boolean;
}

/**
 * The company's capability-budget status. When no `[plan]` is configured only
 * `configured: false` is present; the other fields accompany a configured plan.
 */
export interface CapabilityStatusDto {
  configured: boolean;
  /** The configured built-in tier name, or absent for a bare `token_budgets` plan. */
  plan?: string | null;
  /** Budget window (`daily` / `monthly`). */
  period?: string;
  /** Epoch-millis start of the current budget period. */
  periodStartMillis?: number;
  /** Total inference tokens spent this period. */
  spentTokens?: number;
  /** One row per configured tier, namespace-sorted. */
  tiers?: CapabilityTierDto[];
  /**
   * The plan-level total token ceiling (issue #188), when configured. Crossing
   * it is a hard stop — the harness refuses to dispatch further turns this
   * period, unlike the soft per-namespace `tiers`.
   */
  total?: CapabilityTotalDto;
  /**
   * Media generation (issue #109): whether the company **explicitly** grants the
   * real-money `media` namespace (a `*` wildcard does not count). Present
   * regardless of whether a `[plan]` is configured.
   */
  mediaGranted?: boolean;
  /** Whether the `media` feature is compiled into this build at all. */
  mediaInBuild?: boolean;
  /** Whether a managed media credential is configured on this build (env-only). */
  mediaCredentialConfigured?: boolean;
  /**
   * Per-tenant Composio (issue #110): whether the company **explicitly** grants
   * the `composio` namespace (a `*` wildcard does not count). Opt-in per tool
   * grant, independent of a `[plan]`.
   */
  composioGranted?: boolean;
  /** Whether the `composio` feature is compiled into this build at all. */
  composioInBuild?: boolean;
  /**
   * Whether a per-tenant Composio **BYO override** token is stored under
   * `composio/token` — never the token itself.
   *
   * Narrow on purpose, and **not** "can this company reach Composio" (issue
   * #886). The BYO slot is the first of three credential tiers; on a hosted
   * tenant nobody pastes one and the instance's platform identity answers, so
   * this reads `false` for companies whose Composio tools are wired and
   * working. Read `composioCredentialSource` for the resolution verdict.
   */
  composioTokenConfigured?: boolean;
  /**
   * Which tier this company's Composio credential actually resolves from
   * (issue #886) — the same three-tier resolution the toolbelt gates on, and
   * the same `credentialSource` the Composio status route reports:
   *
   * * `attested` — the instance's platform identity (nothing stored here);
   * * `company` — the company's own TinyHumans key;
   * * `static` — a pasted BYO token, or a static instance key;
   * * `none` — nothing resolves, so no tools are wired.
   *
   * A **resolution** verdict, not a liveness one: `attested` says a bearer can
   * be obtained, not that Composio answered or that any account is connected.
   *
   * `undefined` is **unknown** — either an older host that does not send the
   * field, or one whose secret store could not be read this request. It must
   * never be rendered as `none`: that is the #886 lie in the other direction.
   */
  composioCredentialSource?: "attested" | "company" | "static" | "none";
  /**
   * Metered web search (issue #238): whether the company **explicitly** grants
   * the `search` namespace (a `*` wildcard does not count). Every call is a
   * priced request on the managed platform, so it is opt-in by name.
   */
  searchGranted?: boolean;
  /**
   * Whether the harness carrying `web_search` is compiled into this build.
   * There is no `search` Cargo feature — the tool rides the harness feature so
   * CI actually compiles and tests it.
   */
  searchInBuild?: boolean;
  /** Whether a managed search credential is configured on this build (env-only). */
  searchCredentialConfigured?: boolean;
  /** The company's daily `web_search` call ceiling. */
  searchDailyCallCap?: number;
  /**
   * Which provider the company's searches actually reach: `managed` (the
   * platform's own account, metered and daily-capped) or the slug it configured
   * in Settings → Search.
   *
   * Read beside `searchCredentialConfigured` rather than instead of it: the two
   * disagree in both directions. A host with no platform credential still
   * searches for a company that brought its own key, and a company that picked a
   * provider without finishing it is still on `managed`.
   */
  searchProvider?: string;
  /**
   * Publishing (issue #244, panel half #1192): whether the company's grants
   * confer `publish_artifact` — the only way a file an agent wrote becomes a
   * deliverable.
   *
   * **Unlike every other `*Granted` flag here, a bare `*` DOES confer this.**
   * Publishing spends nothing and reaches nothing outside the company's own
   * board, so it rides the ordinary namespace rule rather than the
   * opt-in-by-name rule the real-money surfaces use. The host derives it from
   * the same predicate the toolbelt's own gate calls, so the panel cannot
   * report a capability no agent has.
   *
   * `undefined` is an older host that does not send the field, and must not be
   * rendered as "not granted".
   */
  publishGranted?: boolean;
  /**
   * Whether the harness carrying `publish_artifact` is compiled into this
   * build. There is no `publish` Cargo feature — the tool rides the harness
   * feature, exactly as `searchInBuild` does.
   *
   * There is deliberately no third flag beside these two. Media, Composio and
   * search each carry a credential/config rung because each can be granted and
   * still wire nothing; publishing has neither a credential nor a store toggle,
   * so a `artifactStoreConfigured` field could only ever be a hardcoded `true`
   * — the always-reassuring flag issue #886 was filed about.
   */
  publishInBuild?: boolean;
  /**
   * Whether the agent-side MCP bridge is compiled into this build (issue #567).
   * Not a grant question like the flags above: the `/mcp/servers` management
   * routes ship in every build, so without this an operator can add a server,
   * store a token and watch it probe healthy on a deployment that hands agents
   * no MCP tool at all. `undefined` is **unknown** (an older host that does not
   * send the field) and must never be rendered as "absent".
   */
  mcpInBuild?: boolean;
  /**
   * Whether this company's teammates can actually think, and why not when they
   * cannot (issue #1735).
   *
   * * `configured` — a cognition path that runs a real model is live. It says
   *   nothing about whether that provider will *answer*; reachability is what
   *   `POST .../inference/test` probes.
   * * `unconfigured` — a harness pool is attached to this company's runtime,
   *   but it resolved no inference source at boot, so it is running the offline
   *   echo brain and replying `"You said: …"` to everything. **Fixable in the
   *   app**, at Settings → Inference. This is the state a fresh instance starts
   *   in.
   * * `unavailable` — no agent harness is reachable on this host, so no
   *   configuration reaches a model. Only a different build or host wiring
   *   changes it, and the console must say so rather than offering a settings
   *   link that cannot help.
   * * `restart-required` — a provider is configured and resolves, but this
   *   company is still on the brain its runtime was built with, so the model is
   *   not live yet. The remedy is that restart, **not** provider selection —
   *   telling this operator to choose a provider sends them back to the page
   *   they just came from. Reported as `restartRequired` on the Inference card
   *   (issue #266).
   * * `undetermined` — a harness is reachable, but the host could not *read*
   *   this company's inference configuration, so it cannot say why the company
   *   fell back to the echo brain. **Name no remedy here**: an unreadable
   *   config is no evidence that saving one would help, which is why the
   *   workflow-run route refuses to answer `inference_required` in this same
   *   state.
   *
   * The states are named for their **remedy**, not their mechanism: both "the
   * harness is not compiled in" and "it is, and this host never attached a
   * pool" report `unavailable`, because the operator can act on neither.
   *
   * The only field here that is not a build fact alone — `mediaInBuild` and its
   * neighbours answer "was this compiled in", and cognition is that question
   * *and* "is a harness attached" *and* "did a model resolve at boot". A fifth
   * boolean would have collapsed them, sending an operator who needs one
   * settings page off looking for a new binary.
   *
   * `undefined` is **unknown** — an older host that does not send the field —
   * and must never be rendered as either working or broken. The chat banner
   * stays down in that case: a host we cannot ask is not evidence of an echo.
   */
  cognition?: CognitionState;
}

/**
 * Whether a company's teammates can think, and why not when they cannot
 * (issue #1735). See `CapabilityStatusDto.cognition`.
 */
export type CognitionState =
  | "configured"
  | "unconfigured"
  | "restart-required"
  | "unavailable"
  | "undetermined";

/** One day's token totals in the usage series (`GET .../usage`). */
export interface UsagePointDto {
  /** ISO day, `YYYY-MM-DD`. */
  date: string;
  inputTokens: number;
  outputTokens: number;
}

/** Tokens attributed to one teammate (desk) over the window. */
export interface AgentTokensDto {
  name: string;
  tokens: number;
  /** Source-currency USD, absent when role-redacted. */
  costUsd?: number;
}

/** OAuth-connected calls counted for one provider over the window. */
export interface ProviderCallsDto {
  provider: string;
  calls: number;
}

/** Rolled-up usage totals for the window. */
export interface UsageTotalsDto {
  inputTokens: number;
  outputTokens: number;
  tokens: number;
  costUsd?: number;
  oauthCalls: number;
  connections: number;
  /**
   * Metered web searches in the window (issue #238). Their USD cost is already
   * inside `costUsd`; this is the call count. Deliberately separate from
   * `oauthCalls` / `connections`, which count connected third-party accounts —
   * a search is a platform call, not an account the company connected.
   */
  searchCalls: number;
}

/**
 * `GET .../usage?range=` — the company's usage read. A REST twin of the
 * `Company.usage(range)` GraphQL surface. Token/cost figures populate on a
 * harness build; the offline build reports a zero-filled series (that is the
 * real value, not a stub). `byProvider` / `oauthCalls` stay empty/zero until the
 * OAuth-call emit lands (Phase 2).
 */
export interface UsageDto {
  /** Zero-filled daily token series over the range, oldest first. */
  series: UsagePointDto[];
  /** Tokens per teammate (desk), highest first. */
  byAgent: AgentTokensDto[];
  /** OAuth calls per provider, highest first (empty until Phase 2 emit). */
  byProvider: ProviderCallsDto[];
  totals: UsageTotalsDto;
  /** Positive cost exists but is hidden from this role. */
  costHidden?: boolean;
}

/** Spend rolled up by prosumer category (`GET .../finances`). */
export interface CategorySpendDto {
  category: string;
  amount: number;
}

/** One monetary ledger movement in the finance journal. */
export interface TransactionDto {
  id: string;
  /** ISO day, `YYYY-MM-DD`. */
  date: string;
  description: string;
  category: string;
  /** Absolute USD magnitude; sign is carried by `direction`. */
  amountUsd: number;
  direction: "in" | "out";
}

/**
 * `GET .../finances` — the company's finance read. A REST twin of the
 * `Company.finances` GraphQL surface: the ledger + manifest `[budget]` folded
 * into balance, budget vs spend, revenue, spend by category, and the journal.
 * The ledger fills on a harness build; the offline build reports zeroes.
 * `balanceUsd` is the bookkeeping net until the wallet balance is surfaced
 * (Phase 2).
 */
export interface FinancesDto {
  balanceUsd: number;
  /**
   * The monthly budget cap. `null` when the manifest sets no `[budget]`;
   * `0` when it is explicitly capped at zero — a hard cap, not an absence.
   */
  budgetUsd: number | null;
  spentUsd: number;
  revenueUsd: number;
  netUsd: number;
  byCategory: CategorySpendDto[];
  transactions: TransactionDto[];
}

/**
 * One structured problem with a workflow graph the host refused (issue #1016).
 *
 * Field names are the **wire** names, not the console's usual camelCase. The
 * host serialises `WorkflowProblem` with serde's defaults (`src/error.rs`), so
 * the key really is `node_id`; renaming it here to match house style would
 * compile, type-check, and silently read `undefined` at runtime for every
 * problem — the failure this shape exists to prevent.
 *
 * Both locators are optional because a problem need not have one: a graph-level
 * refusal (an inescapable cycle) names several nodes at once and owns neither.
 * `message` is the only field always present, and it is prosumer language ready
 * to render.
 */
export interface WorkflowProblem {
  /** The node at fault, or the dangling endpoint for an edge problem. */
  node_id?: string;
  /** The config path at fault (`config.url`, `workflow_id`, `from`). */
  field?: string;
  /** The human-readable problem. */
  message: string;
}

/**
 * Error envelope shape: `{ error, code }`, plus `problems` on a refusal that
 * has them.
 *
 * `problems` is additive and scoped to `workflow_invalid` on the host side, so
 * it is absent from every other error and must stay optional here.
 */
export interface ApiErrorBody {
  error: string;
  code: string;
  problems?: WorkflowProblem[];
}

/**
 * The "where" of a {@link WorkflowProblem}, or `undefined` when it names no
 * location at all.
 *
 * A function rather than a conditional inside the card's JSX, because the three
 * shapes are the whole of this feature's correctness and the console has no
 * component-test harness to catch a mistake in markup. Extracted after review
 * caught the field-only case being dropped: the first version keyed the whole
 * locator on `node_id`, so a problem carrying `field` and no node rendered its
 * message with no indication of where it came from — and nothing could have
 * failed, because there was nothing to call.
 *
 * All three shapes are real. The host builds a node+field problem through
 * `WorkflowProblem::node_field`, which stores `node_id` only when it is
 * non-blank — so a blank node id with a real field emits exactly the field-only
 * shape — and a graph-level refusal (an inescapable cycle) carries neither.
 */
export function workflowProblemLocator(problem: WorkflowProblem): string | undefined {
  const parts = [problem.node_id, problem.field].filter(
    (part): part is string => typeof part === "string" && part.trim() !== "",
  );
  return parts.length ? parts.join(" · ") : undefined;
}

/**
 * The per-node breakdown a refusal carries, or `null` when it carries none
 * (issue #1191).
 *
 * A function rather than an inline ternary at each call site because the three
 * branches are the whole of the decision and the console has no component-test
 * harness that could catch getting them wrong in JSX:
 *
 * * not an {@link ApiError} at all (a network failure, an abort) — no breakdown;
 * * an `ApiError` the host answered without a `problems` array (every refusal
 *   that is not a workflow refusal) — no breakdown;
 * * an `ApiError` carrying an EMPTY array — still no breakdown, because a list
 *   with nothing in it renders as an empty bullet list under the sentence, which
 *   reads as a rendering bug rather than as "there was nothing more to say".
 *
 * `null` rather than `undefined` so a caller holding it in state can distinguish
 * "asked and there was none" from "never asked", and so the render guard is a
 * single truthiness check.
 */
export function workflowRefusalProblems(error: unknown): WorkflowProblem[] | null {
  if (!(error instanceof ApiError)) return null;
  return error.problems?.length ? error.problems : null;
}

export class ApiError extends Error {
  /**
   * The raw response body, kept only when it was **not** the host's envelope
   * (issue #380) — a proxy's error page, an HTML 502, a plain-text upstream
   * failure.
   *
   * Diagnostic only, and deliberately not part of `message`: `message` is
   * rendered to operators as prose, and an arbitrary upstream body is not
   * prose. Nothing in the console renders this field; it exists so that
   * "which hop failed" survives for whoever is reading a bug report, and it is
   * bounded by the client so a megabyte error page is not retained on an Error
   * that may sit in state for the life of the view.
   */
  detail?: string;

  /**
   * The per-node, per-field breakdown behind `message`, when the host sent one
   * (issues #1016, #836).
   *
   * The host already computes this and puts it on the wire; before this field
   * existed the console parsed the envelope's `error` and `code` and dropped
   * `problems` on the floor, so an operator was told *that* a graph was refused
   * and never *which node*. `message` remains the flattened sentence and stays
   * the fallback — a renderer may show this list instead, never in addition to
   * nothing.
   *
   * Absent for every error that is not a workflow refusal, and absent (rather
   * than empty) when the host sent no array, so `problems?.length` distinguishes
   * "no breakdown offered" from "a breakdown with nothing in it".
   */
  problems?: WorkflowProblem[];

  constructor(
    public status: number,
    public code: string,
    message: string,
    /**
     * Whether `code` and `message` came from the host's own `{error, code}`
     * envelope, rather than being synthesised from the status line (#380).
     *
     * This is the difference between "the host considered the request and
     * refused" and "something between the browser and the host gave up", and
     * callers cannot recover it from `status` alone: the host itself answers
     * 503 while quiescing and 502 on an upstream transport failure, so those
     * codes are ambiguous on the wire and unambiguous here. `ApprovalsView`
     * turns on exactly this to decide whether a decision may still have landed.
     */
    public readonly fromHost: boolean = false,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

/**
 * `GET {scope}/chat/read-state` — one channel's floor for the signed-in person
 * (issue #755).
 *
 * A channel with no marker is **absent** from the response rather than zero.
 * "Never opened" and "opened before anything was said" are different states,
 * and only the console knows which floor a never-opened channel deserves.
 */
export interface ReadMarker {
  channelId: string;
  /** Milliseconds since the epoch. At or before this is read. */
  lastReadAt: number;
}

/** Response of `GET {scope}/chat/read-state`. */
export interface ReadStateResponse {
  markers: ReadMarker[];
}

/** Response of `GET {scope}/notifications`. */
export interface NotificationFeedResponse {
  /** Newest first, and only what this person is addressed by. */
  notifications: NotificationDto[];
  /** How many are still unread for this person — what the badge renders. */
  unread: number;
}

/** One notification, as the person it is for reads it. */
export interface NotificationDto {
  id: string;
  /** A free-form tag — `"mention"` today. */
  kind: string;
  /** `task` / `run` / `approval` / `workflow` / `message`. */
  subjectKind: string;
  /** The subject's id in its own space; a chat message id for `message`. */
  subjectId: string;
  title: string;
  createdAt: number;
  /** When this person read it; absent while unread for them. */
  readAt?: number;
  /**
   * The console channel this belongs to, so a badge lands without the
   * transcript being loaded.
   */
  context?: string;
}

/** Response of `PUT {scope}/notifications`. */
export interface MarkNotificationsReadResponse {
  /** Still unread for this person after the mark. */
  unread: number;
}

/** Response of `GET {scope}/presence`. */
export interface PresenceListResponse {
  /**
   * Present people, most recently seen first.
   *
   * **This replica's view.** A second host serving the same tenant keeps its
   * own map, so somebody connected there is simply absent here — which is why
   * an absence reads as "no live signal" and never as a grey "offline" dot.
   */
  people: PresenceDto[];
}

/** One person's live presence. */
export interface PresenceDto {
  /** The user id, as `GET {scope}/chat/mentionables` already names them. */
  userId: string;
  status: "online" | "away" | "offline";
  /** When their lease was last renewed, epoch millis. */
  atMillis: number;
}

/**
 * Response of `GET {scope}/chat/mentionables` — everything an `@` can name.
 *
 * Mirrors `MentionablesDto` in `src/server/ops/mentions.rs`.
 */
export interface MentionablesResponse {
  agents: MentionableAgentDto[];
  people: MentionablePersonDto[];
  desks: MentionableDeskDto[];
  everyone: MentionableEveryoneDto;
}

/** One teammate the picker can offer. */
export interface MentionableAgentDto {
  id: string;
  name: string;
  role: string;
}

/**
 * One person the picker can offer.
 *
 * Id, label, and chosen face only, by design — this is deliberately not the
 * admin user record, and must not grow toward it.
 */
export interface MentionablePersonDto {
  id: string;
  /** How this person is named to colleagues; never their login identity. */
  label: string;
  /** The collaboration-facing face chosen by this person, when any. */
  avatar?: string;
  /** A short typable alias, disambiguated company-wide. Not a handle. */
  slug: string;
}

/** One desk the picker can offer. */
export interface MentionableDeskDto {
  id: string;
  name: string;
  /** The teammates a mention of this desk expands to. */
  memberIds: string[];
}

/**
 * The broadcast token, described by the host rather than hard-coded here — a
 * console that disagreed about the spellings would offer a row resolving to
 * nothing.
 */
export interface MentionableEveryoneDto {
  label: string;
  aliases: string[];
}
