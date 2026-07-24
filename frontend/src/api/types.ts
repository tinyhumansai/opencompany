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
