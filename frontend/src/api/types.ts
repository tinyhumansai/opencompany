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
  /** Channel-specific reply addressing (Telegram). Absent on operator messages. */
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

/** Telegram channel configuration status (no secrets). */
export interface TelegramChannelStatus {
  /** Whether the channel is fully configured (both token + secret stored). */
  configured: boolean;
  /** Whether a bot token is stored (never the token itself). */
  tokenSet: boolean;
  /** Whether a webhook secret is stored (never the secret itself). */
  secretSet: boolean;
  /** The public webhook URL to register with setWebhook. */
  webhookUrl: string;
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
   * Whether the whole desk was operator-created (an overlay desk) rather than
   * declared in the manifest blueprint. The console offers a delete action only
   * for these. Omitted (undefined/false) for blueprint desks.
   */
  overlayCreated?: boolean;
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
   * Which board task this approval was parked for (#333). Mirrors `TaskLink` in
   * `src/runtime/journal.rs`.
   *
   * Three states, deliberately: `{link: "task"}` is owned by that card,
   * `{link: "unlinked"}` is owned by no card (a workflow delivery, an
   * operator-chat turn, a scheduler tick), and *absent* means the park predates
   * the field. Only the last one is ambiguous — the server keeps a run-window
   * heuristic for it, and for nothing else.
   */
  task?: { link: "task"; id: string } | { link: "unlinked" };
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
 * The answer to a **detached** resolve (#383): the verdict is durable, and that
 * is all it claims. The agent's continuation arrives afterwards on the event
 * stream's `agent_reply` frame.
 */
export interface ResolveReceipt {
  recorded: boolean;
  /** There was nothing left to resolve — a double-click, not a failure. */
  alreadyResolved: boolean;
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
export interface StandingGrant {
  id: string;
  /** The teammate it was granted to. */
  agent: string;
  /** The tool it admits, with any arguments. */
  tool: string;
  /** Who granted it: a signed-in user, or the platform credential. */
  granted_by: { kind: string; id: string };
  at_millis: number;
  /** Epoch-millis it stops admitting calls. */
  expires_at_millis: number;
  /**
   * The slice of the tool it is confined to, when the tool's name is not the
   * whole of what it can do (#457).
   *
   * **Two kinds of value, in one untyped string** — both minted by the host's
   * `standing_scope_of`, and a reader that assumes either one is the bug #785
   * was:
   *
   * * a **Composio toolkit** identifier like `github` — a slug, which has to be
   *   spelled out before an operator can read it;
   * * a **URL origin** like `https://docs.rs`, added for `web_fetch` by
   *   #673/#739 — already exactly what the operator approved, and to be shown
   *   untouched.
   *
   * Render it through `grantHeadline` in `lib/language`, which is the one place
   * that tells them apart. Do not spell a scope out at a call site.
   *
   * Absent for every tool whose name already says everything, and absent from
   * an older host that predates the field. Both mean "nothing to narrow", so
   * the row simply says what it always said.
   */
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
   * Which half of the roster this teammate comes from, and therefore what may
   * be done to it. `manifest` teammates are declared in the version-controlled
   * `company.toml`; `overlay` teammates were added at runtime and live on the
   * company record this host writes.
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
   * Whether this teammate is the company's orchestrator. Resolved by the roster
   * rule (a tagged tier first, else the first declared agent), so it is NOT the
   * same question as `tier === "orchestrator"`: a company that tags nobody still
   * has one.
   */
  isOrchestrator: boolean;
  tools: AgentToolsDto;
  desks: AgentDeskDto[];
  inboxEnabled: boolean;
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
 * `effective` is what the agent actually holds. An **empty `requested` means
 * the company's standard grant**, not "no tools", so a surface that renders the
 * request alone reports the opposite of the truth for exactly those agents.
 */
export interface AgentToolsDto {
  requested: string[];
  companyAllow: string[];
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
export interface InboxDto {
  /** The inbox key (a teammate's local part / slug). */
  key: string;
  /** The teammate's display name. */
  name: string;
  /** The full address (`{key}@{domain}` when a domain is configured). */
  address: string;
  /** Whether the inbox is enabled on the Team page. */
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
 * - `static` — a token this company already stored, or this host's own
 *   registered provider application (the self-hosted hatch). The handshake
 *   works; the console stopped offering it in issue #822, because what it
 *   stores is read by no agent tool (#396). So this tier now says what the host
 *   *could* do, and `connectRoute` (`lib/connections.ts`) routes such a
 *   provider through Composio or reports it unavailable.
 * - `none` — neither, so no Connect can succeed on this host.
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

/** Response of `POST .../connections/{provider}/start`: where to send the user. */
export interface ConnectionStart {
  /** The provider's OAuth authorize URL to redirect the operator to. */
  url: string;
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
 * McpServer.reachableBy} ("Reachable by") and the repositories card's
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
 * One effective MCP tool server (issue #50), as `.../mcp/servers` returns it.
 * The credential is never present — only the non-secret `authConfigured` flag
 * and the last (scrubbed) probe `health`.
 */
export interface McpServer {
  name: string;
  endpoint: string;
  description?: string;
  /** `manifest` (committed in company.toml) or `runtime` (console-added). */
  source: "manifest" | "runtime";
  enabled: boolean;
  allowedTools: string[];
  disallowedTools: string[];
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
   * Bound repositories (issue #245, agent half): whether the company
   * **explicitly** grants the `repo` namespace (a `*` wildcard does not count).
   *
   * The grant is only half the setup — the other half is whether anything is
   * bound — so the repositories card reads this alongside the bindings list and
   * names whichever half is missing. `undefined` is an older host that does not
   * send the field, and must not be rendered as "not granted".
   */
  repoGranted?: boolean;
  /**
   * Whether the agent-side MCP bridge is compiled into this build (issue #567).
   * Not a grant question like the flags above: the `/mcp/servers` management
   * routes ship in every build, so without this an operator can add a server,
   * store a token and watch it probe healthy on a deployment that hands agents
   * no MCP tool at all. `undefined` is **unknown** (an older host that does not
   * send the field) and must never be rendered as "absent".
   */
  mcpInBuild?: boolean;
}

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
  budgetUsd: number;
  spentUsd: number;
  revenueUsd: number;
  netUsd: number;
  byCategory: CategorySpendDto[];
  transactions: TransactionDto[];
}

/** Error envelope shape: `{ error, code }`. */
export interface ApiErrorBody {
  error: string;
  code: string;
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
