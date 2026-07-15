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

/** Response of `/feedback`. */
export interface FeedbackResponse {
  item_id: string;
  filed: boolean;
  blocked: boolean;
  reason?: string;
  preview_body?: string;
  prefilled_url?: string;
  issue_url?: string;
  deduped: boolean;
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
