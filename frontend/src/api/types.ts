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

/** One channel reply from a cycle. */
export interface OutboundMessage {
  channel: string;
  text: string;
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
  members: string[];
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

/**
 * One effective MCP tool server (issue #50), as `.../mcp/servers` returns it.
 * The credential is never present — only the non-secret `authConfigured` flag.
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
}

/** A mutating MCP response: the resulting server plus a rebuild reminder. */
export interface McpMutationResponse {
  server: McpServer;
  note: string;
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
