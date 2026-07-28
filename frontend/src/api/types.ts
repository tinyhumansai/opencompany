// TypeScript mirrors of the OpenCompany operator API payloads.
// Kept in sync with src/runtime/types.rs, src/server/operator.rs, and
// src/feedback/{types,service}.rs.

/** `GET /api/v1/companies` and `GET /api/v1/companies/{id}`. */
export interface CompanyStatus {
  id: string;
  name: string;
  /** e.g. "running", "paused", "suspended", "archived". */
  lifecycle: string;
  pending_approvals: number;
}

/** What kind of processing step this is (drives the timeline icon). */
export type TurnStepKind = "tool_call" | "thinking" | "note";

/** How a processing step ended. */
export type TurnStepStatus = "ok" | "error" | "running";

/**
 * One visible step in an agent turn's processing timeline. Mirrors `TurnStep`
 * in `src/ports/types.rs`. The host folds and scrubs these from the turn's
 * progress stream: `label`/`detail` never carry raw tool arguments, tool
 * output, or call ids — only a safe label and an optional scrubbed detail.
 */
export interface TurnStep {
  kind: TurnStepKind;
  status: TurnStepStatus;
  label: string;
  /** A muted, scrubbed detail (e.g. an MCP `server · tool`, a failure cause). */
  detail?: string;
  /** How long a tool call took, in milliseconds, when known. */
  elapsedMs?: number;
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
}

/** Response of `/chat` and approval-resolution routes. */
export interface ChatResponse {
  responses: OutboundMessage[];
}

/** One parked approval from `/approvals`. */
export interface ApprovalSummary {
  id: string;
  /** The parked effect's dotted kind, e.g. "payment.send". */
  kind: string;
  amount_usd: number | null;
  at_millis: number;
}

export type Verdict = "approve" | "deny";

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
}

/**
 * One third-party connection's state, from `GET .../connections`.
 * Forward-looking: hosts that don't expose the connections surface yet simply
 * 404, and the console treats connections as unavailable.
 */
export interface ConnectionState {
  /** Provider id, matching the console's connection catalog (e.g. "slack"). */
  provider: string;
  connected: boolean;
  /** The connected account label, when known (e.g. an email or workspace). */
  account?: string;
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
  /** The last recorded (scrubbed) probe outcome, when the server has been probed. */
  health?: McpHealth;
}

/**
 * A mutating MCP response: the resulting server, a rebuild reminder, the live
 * probe result (absent on a non-`openhuman` host), and any non-blocking
 * endpoint advisory.
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
  /** Whether a per-tenant Composio token is stored — never the token itself. */
  composioTokenConfigured?: boolean;
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
  costUsd: number;
  oauthCalls: number;
  connections: number;
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
  constructor(
    public status: number,
    public code: string,
    message: string,
  ) {
    super(message);
    this.name = "ApiError";
  }
}
