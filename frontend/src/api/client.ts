// A typed, company-agnostic client for the OpenCompany operator API.
//
// The same instance serves both deployment shapes:
//   - Single-company (prosumer): construct with `company = null`; calls use the
//     host's `/api/v1/company/*` aliases for the sole registered company.
//   - Multi-company (platform): pass a company id per call (or as the default),
//     and calls use `/api/v1/companies/{id}/*`.

import type { ConsoleConfig } from "../config";
import {
  ApiError,
  type ApiErrorBody,
  type ApprovalSummary,
  type ChatResponse,
  type CompanyStatus,
  type ConnectionStart,
  type ConnectionState,
  type FeedbackInput,
  type FeedbackResponse,
  type TeamMemberDto,
  type Verdict,
} from "./types";

export type LifecycleAction = "pause" | "resume" | "suspend" | "archive";

export class OpenCompanyClient {
  readonly baseUrl: string;
  readonly defaultCompany: string | null;
  private readonly token: string | null;

  constructor(config: Pick<ConsoleConfig, "baseUrl" | "company" | "operatorToken">) {
    this.baseUrl = config.baseUrl;
    this.defaultCompany = config.company;
    this.token = config.operatorToken;
  }

  /** Resolves the `/companies/{id}` vs single-company `/company` route prefix. */
  private scope(company: string | null | undefined): string {
    const id = company ?? this.defaultCompany;
    return id ? `/api/v1/companies/${encodeURIComponent(id)}` : "/api/v1/company";
  }

  private async request<T>(method: string, path: string, body?: unknown): Promise<T> {
    const headers: Record<string, string> = {};
    if (body !== undefined) headers["content-type"] = "application/json";
    if (this.token) headers["authorization"] = `Bearer ${this.token}`;

    let res: Response;
    try {
      res = await fetch(`${this.baseUrl}${path}`, {
        method,
        headers,
        body: body === undefined ? undefined : JSON.stringify(body),
      });
    } catch (cause) {
      throw new ApiError(0, "network_error", `cannot reach the company host at ${this.baseUrl || "this origin"}`);
    }

    const text = await res.text();
    const data = text ? safeJson(text) : undefined;
    if (!res.ok) {
      const envelope = data as ApiErrorBody | undefined;
      throw new ApiError(res.status, envelope?.code ?? `http_${res.status}`, envelope?.error ?? res.statusText);
    }
    return data as T;
  }

  /** Whether a specific company is being operated (vs single-company mode). */
  get isSingleCompany(): boolean {
    return this.defaultCompany === null;
  }

  /** Liveness probe. */
  async healthz(): Promise<boolean> {
    try {
      await this.request<{ status: string }>("GET", "/healthz");
      return true;
    } catch {
      return false;
    }
  }

  /** Every registered company (platform mode). */
  listCompanies(): Promise<CompanyStatus[]> {
    return this.request<CompanyStatus[]>("GET", "/api/v1/companies");
  }

  /** One company's status. Uses the single-company alias when unscoped. */
  status(company?: string | null): Promise<CompanyStatus> {
    return this.request<CompanyStatus>("GET", `${this.scope(company)}`);
  }

  /** Send the operator's message and return the company's reply. */
  chat(text: string, company?: string | null): Promise<ChatResponse> {
    return this.request<ChatResponse>("POST", `${this.scope(company)}/chat`, { text });
  }

  /** The approvals awaiting the operator. */
  approvals(company?: string | null): Promise<ApprovalSummary[]> {
    return this.request<ApprovalSummary[]>("GET", `${this.scope(company)}/approvals`);
  }

  /** Approve or deny a parked approval; returns the follow-up reply. */
  resolveApproval(
    approvalId: string,
    verdict: Verdict,
    note?: string,
    company?: string | null,
  ): Promise<ChatResponse> {
    const body: { verdict: Verdict; note?: string } = { verdict };
    if (note) body.note = note;
    return this.request<ChatResponse>(
      "POST",
      `${this.scope(company)}/approvals/${encodeURIComponent(approvalId)}`,
      body,
    );
  }

  /** Capture feedback (optionally preview the exact issue body first). */
  feedback(input: FeedbackInput, company?: string | null): Promise<FeedbackResponse> {
    return this.request<FeedbackResponse>("POST", `${this.scope(company)}/feedback`, input);
  }

  /**
   * The company's agent roster (forward-looking surface). Hosts that don't
   * expose `.../team` yet return 404 — callers fall back to a local roster.
   */
  listTeam(company?: string | null): Promise<TeamMemberDto[]> {
    return this.request<TeamMemberDto[]>("GET", `${this.scope(company)}/team`);
  }

  /**
   * Third-party connections for a company (forward-looking surface). Hosts
   * that don't expose it yet return 404 — callers treat that as "unavailable".
   */
  listConnections(company?: string | null): Promise<ConnectionState[]> {
    return this.request<ConnectionState[]>("GET", `${this.scope(company)}/connections`);
  }

  /** Begin an OAuth connect flow; returns the provider authorize URL to open. */
  startConnection(provider: string, company?: string | null): Promise<ConnectionStart> {
    return this.request<ConnectionStart>(
      "POST",
      `${this.scope(company)}/connections/${encodeURIComponent(provider)}/start`,
    );
  }

  /** Revoke a connected provider. */
  disconnectConnection(provider: string, company?: string | null): Promise<void> {
    return this.request<void>(
      "POST",
      `${this.scope(company)}/connections/${encodeURIComponent(provider)}/disconnect`,
    );
  }

  /** Platform lifecycle control (requires a scoped company id). */
  lifecycle(action: LifecycleAction, company?: string | null): Promise<CompanyStatus> {
    const id = company ?? this.defaultCompany;
    if (!id) throw new ApiError(0, "no_company", "lifecycle controls require a company id");
    return this.request<CompanyStatus>(
      "POST",
      `/api/v1/companies/${encodeURIComponent(id)}/${action}`,
    );
  }
}

function safeJson(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return { error: text, code: "unparseable" };
  }
}
