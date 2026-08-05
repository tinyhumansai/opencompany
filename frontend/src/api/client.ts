// A typed, company-agnostic client for the OpenCompany operator API.
//
// The same instance serves both deployment shapes:
//   - Single-company (prosumer): construct with `company = null`; calls use the
//     host's `/api/v1/company/*` aliases for the sole registered company.
//   - Multi-company (platform): pass a company id per call (or as the default),
//     and calls use `/api/v1/companies/{id}/*`.

import type { ConsoleConfig } from "../config";
import type {
  InstallMcpServerInput,
  McpCallResult,
  McpServer,
  McpTool,
} from "../lib/mcp";
import {
  ApiError,
  type ApiErrorBody,
  type AppSpec,
  type ApprovalSummary,
  type CapabilityStatusDto,
  type ChatHistoryMessageDto,
  type ChatResponse,
  type CompanyStatus,
  type ConnectionStart,
  type ConnectionState,
  type CreateDeskInput,
  type DeskDto,
  type FeedbackInput,
  type FeedbackResponse,
  type FeedbackSummary,
  type FinancesDto,
  type GrantScope,
  type InboxDto,
  type InboxMessageDto,
  type ResolveReceipt,
  type SetBudgetInput,
  type StandingGrant,
  type TeamMemberDto,
  type UsageDto,
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

  /** Called on any 401, so the app can drop to the login view. */
  onUnauthorized: (() => void) | null = null;

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
        // The session is an HttpOnly cookie, so it only travels if we ask.
        // Same-origin (the normal deployment, baseUrl "") this is a no-op.
        // Cross-origin dev (`?api=http://localhost:8080` from a Vite server on
        // another port) additionally needs CORS with
        // Access-Control-Allow-Credentials, which the backend does not have —
        // use the Vite proxy so the console stays same-origin.
        credentials: "include",
      });
    } catch (cause) {
      throw new ApiError(0, "network_error", `cannot reach the company host at ${this.baseUrl || "this origin"}`);
    }

    const text = await res.text();
    if (!res.ok) {
      // Let the app react to an expired or revoked session. Auth routes opt out
      // (they 401 as a normal answer) so a failed login cannot loop the view.
      if (res.status === 401 && !path.includes("/auth/")) this.onUnauthorized?.();
      throw httpError(res, text);
    }
    return (text ? parseJson(text) : undefined) as T;
  }

  /** Whether a specific company is being operated (vs single-company mode). */
  get isSingleCompany(): boolean {
    return this.defaultCompany === null;
  }

  /** The route prefix for `company`, for callers building their own paths. */
  scopeFor(company: string | null | undefined): string {
    return this.scope(company);
  }

  /** A typed GET, for surfaces that live outside this class (e.g. auth). */
  get<T>(path: string): Promise<T> {
    return this.request<T>("GET", path);
  }

  /**
   * A GET whose answer is a document, not JSON (issue #352).
   *
   * `request` parses every successful response as JSON and hands back
   * `undefined` when it is not — so a route that answers HTML needs its own
   * reader to see the body at all. Same auth, same `credentials: "include"`,
   * same 401 handling, and since #380 the same `httpError` on the failure
   * path; only the success-path parsing differs.
   *
   * Returns the host's own `Content-Disposition` filename alongside the body.
   * The host already names the file; without this the caller has to invent a
   * name, which is a second naming rule that can disagree with the one a `curl
   * -OJ` of the same route gets.
   */
  async getDocument(path: string): Promise<{ text: string; filename?: string }> {
    const headers: Record<string, string> = {};
    if (this.token) headers["authorization"] = `Bearer ${this.token}`;

    let res: Response;
    try {
      res = await fetch(`${this.baseUrl}${path}`, { method: "GET", headers, credentials: "include" });
    } catch {
      throw new ApiError(0, "network_error", `cannot reach the company host at ${this.baseUrl || "this origin"}`);
    }
    const text = await res.text();
    if (!res.ok) {
      if (res.status === 401) this.onUnauthorized?.();
      throw httpError(res, text);
    }
    return { text, filename: attachmentFilename(res.headers.get("content-disposition")) };
  }

  /** A typed POST, for surfaces that live outside this class (e.g. auth). */
  post<T>(path: string, body?: unknown): Promise<T> {
    return this.request<T>("POST", path, body);
  }

  /** A typed PATCH, for surfaces that live outside this class (e.g. auth). */
  patch<T>(path: string, body?: unknown): Promise<T> {
    return this.request<T>("PATCH", path, body);
  }

  /** A typed PUT, for surfaces that live outside this class (e.g. skills). */
  put<T>(path: string, body?: unknown): Promise<T> {
    return this.request<T>("PUT", path, body);
  }

  /** A typed DELETE, for surfaces that live outside this class (e.g. auth). */
  del<T>(path: string): Promise<T> {
    return this.request<T>("DELETE", path);
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

  /**
   * The company's capability-budget status (issue #108): the effective tier plan
   * and per-tier token spend. Hosts without the surface (or with no `[plan]`)
   * return `{ configured: false }`; callers render a "no plan configured" note.
   */
  capabilityStatus(company?: string | null): Promise<CapabilityStatusDto> {
    return this.request<CapabilityStatusDto>("GET", `${this.scope(company)}/capabilities`);
  }

  /**
   * The company's usage read (Usage view): daily token series, tokens by desk,
   * OAuth calls by provider, and window totals over a `7d` / `30d` / `90d`
   * range. Token figures are real-or-zero — the offline build reports a
   * zero-filled series until the harness cost hook meters spend.
   */
  usage(range?: string | null, company?: string | null): Promise<UsageDto> {
    const qs = range ? `?range=${encodeURIComponent(range)}` : "";
    return this.request<UsageDto>("GET", `${this.scope(company)}/usage${qs}`);
  }

  /**
   * The company's finance read (Finances view): balance, budget vs spend,
   * revenue, spend by category, and the transaction journal. Figures are
   * real-or-zero — the offline build reports zeroes until the ledger fills.
   */
  finances(company?: string | null): Promise<FinancesDto> {
    return this.request<FinancesDto>("GET", `${this.scope(company)}/finances`);
  }

  /**
   * Send the operator's message and return the company's reply. `chat` is the
   * addressed desk / thread id (issue #53): the orchestrator routes an addressed
   * message to that desk's lead, and an unaddressed one answers itself. Omitted /
   * unknown ids fall to the orchestrator, so callers can always pass the active
   * thread id safely.
   */
  chat(
    text: string,
    company?: string | null,
    chat?: string | null,
    /**
     * The host-side id of the message being replied to, making this a thread
     * reply rather than a new line in the channel (issue #364). Never a console
     * id — callers strip the `h` prefix with `toHostMessageId` first.
     */
    parent?: string | null,
  ): Promise<ChatResponse> {
    const body: { text: string; chat?: string; parent?: string } = { text };
    if (chat) body.chat = chat;
    if (parent) body.parent = parent;
    return this.request<ChatResponse>("POST", `${this.scope(company)}/chat`, body);
  }

  /**
   * Set or clear one reaction on one message (issue #364).
   *
   * `on` is explicit rather than a toggle so the call is idempotent: a retry or
   * a double tap converges on what the caller asked for instead of flipping
   * twice. `messageSeq` is the host-side id (no `h` prefix). Hosts that predate
   * the route return 404 — callers roll their optimistic change back and say
   * the host can't keep reactions.
   */
  reactToMessage(
    messageSeq: string,
    emoji: string,
    on: boolean,
    company?: string | null,
  ): Promise<void> {
    return this.request<void>(
      "POST",
      `${this.scope(company)}/chat/messages/${encodeURIComponent(messageSeq)}/reactions`,
      { emoji, on },
    );
  }

  /**
   * The company's desks (group chats). Hosts that don't expose `.../desks` yet
   * return 404 — callers fall back to the static default threads.
   */
  listDesks(company?: string | null): Promise<DeskDto[]> {
    return this.request<DeskDto[]>("GET", `${this.scope(company)}/desks`);
  }

  /**
   * Add a teammate to a desk through the operator overlay (issue #72). The
   * teammate must be on the company roster; the desk must exist. Adding one
   * already on the desk is a 409.
   */
  addDeskMember(deskId: string, agentId: string, company?: string | null): Promise<void> {
    return this.request<void>(
      "POST",
      `${this.scope(company)}/desks/${encodeURIComponent(deskId)}/members`,
      { agent_id: agentId },
    );
  }

  /**
   * Remove an operator-added desk member (issue #72). Only overlay members can
   * be removed — a teammate declared on the desk in the manifest is part of the
   * blueprint and returns a 409.
   */
  removeDeskMember(deskId: string, agentId: string, company?: string | null): Promise<void> {
    return this.request<void>(
      "DELETE",
      `${this.scope(company)}/desks/${encodeURIComponent(deskId)}/members/${encodeURIComponent(agentId)}`,
    );
  }

  /**
   * Set the operator's explicit member order (the desk hierarchy) for a desk
   * (issue #131). `orderedMemberIds` is the full member list in the intended
   * order — the first is the desk lead. Every id must be a current member; an
   * empty list resets the desk to its blueprint order. The version-controlled
   * manifest is never rewritten — the order lives in the overlay.
   */
  setDeskOrder(
    deskId: string,
    orderedMemberIds: string[],
    company?: string | null,
  ): Promise<void> {
    return this.request<void>(
      "PUT",
      `${this.scope(company)}/desks/${encodeURIComponent(deskId)}/order`,
      { ordered_member_ids: orderedMemberIds },
    );
  }

  /**
   * Create a desk through the operator overlay. `name` is required; the id is
   * derived from it when omitted. Members are optional and must be roster
   * teammates; the first is the desk's lead. The manifest is never rewritten and
   * the desk survives rebuilds. A duplicate id is a 409, an invalid id/empty
   * name a 400.
   */
  createDesk(input: CreateDeskInput, company?: string | null): Promise<DeskDto> {
    return this.request<DeskDto>("POST", `${this.scope(company)}/desks`, input);
  }

  /**
   * Delete an operator-created desk. A manifest (blueprint) desk cannot be
   * deleted at runtime and returns a 409; an unknown id is a 404.
   */
  deleteDesk(deskId: string, company?: string | null): Promise<void> {
    return this.request<void>(
      "DELETE",
      `${this.scope(company)}/desks/${encodeURIComponent(deskId)}`,
    );
  }

  /**
   * A desk's persisted transcript (issue #65), so the console can rehydrate a
   * thread on login/reload instead of always starting empty. `desk` is the
   * thread id (as passed to {@link chat}); omitted reads the operator/General
   * line. Hosts that don't expose `.../chat/history` yet return 404 — callers
   * fall back to an empty transcript.
   */
  getChatHistory(desk?: string | null, company?: string | null): Promise<ChatHistoryMessageDto[]> {
    const qs = desk ? `?desk=${encodeURIComponent(desk)}` : "";
    return this.request<ChatHistoryMessageDto[]>(
      "GET",
      `${this.scope(company)}/chat/history${qs}`,
    );
  }

  /** The approvals awaiting the operator. */
  approvals(company?: string | null): Promise<ApprovalSummary[]> {
    return this.request<ApprovalSummary[]>("GET", `${this.scope(company)}/approvals`);
  }

  /** Approve or deny a parked approval; returns the follow-up reply. */
  /**
   * Resolve a parked approval.
   *
   * Two answer shapes, chosen by `detach` (#383):
   *
   * * **default** — the response holds the follow-up turn's replies
   *   (`ChatResponse`). The Approvals page wants this: it is not sitting in a
   *   transcript, so the body is its only sight of what happened next.
   * * **detached** — the response is a receipt (`ResolveReceipt`) and the
   *   continuation arrives on the event stream's `agent_reply` frame instead.
   *   The **inline chat card** must use this (#379): rendering the POST body
   *   *and* receiving the SSE echo would deliver one reply to the channel
   *   twice, and detach has exactly one delivery path so the race cannot exist.
   *
   * The parse is deliberately tolerant rather than trusting the flag. A host
   * that predates #383 ignores `detach` and answers with `responses` anyway, so
   * the caller is handed whichever shape actually arrived — and never a receipt
   * fabricated from a body that isn't one.
   */
  async resolveApproval(
    approvalId: string,
    verdict: Verdict,
    note?: string,
    company?: string | null,
    options: { detach?: boolean; scope?: GrantScope } = {},
  ): Promise<ChatResponse | ResolveReceipt> {
    const body: {
      verdict: Verdict;
      note?: string;
      detach?: boolean;
      scope?: "once" | "tool";
      expires_in_millis?: number;
    } = { verdict };
    if (note) body.note = note;
    if (options.detach) body.detach = true;
    // Issue #374. The `once` scope is sent as *nothing at all*, not as
    // `scope: "once"`: the omitted-field form is what an old host understands,
    // so a new console against an old host keeps working instead of 400ing on a
    // key that host has never heard of. Only the broader scope is ever on the
    // wire, and it always carries its duration — the host rejects it otherwise.
    if (options.scope?.kind === "tool") {
      body.scope = "tool";
      body.expires_in_millis = options.scope.expiresInMillis;
    }
    const answer = await this.request<unknown>(
      "POST",
      `${this.scope(company)}/approvals/${encodeURIComponent(approvalId)}`,
      body,
    );
    return isResolveReceipt(answer) ? answer : (answer as ChatResponse);
  }

  /** The live standing permissions, newest first (#374). */
  listGrants(company?: string | null): Promise<StandingGrant[]> {
    return this.request<StandingGrant[]>("GET", `${this.scope(company)}/grants`);
  }

  /**
   * Take a standing permission back (#374).
   *
   * Effective on the tool's **next** call — a call already underway is not
   * aborted. A 404 means it was already gone (revoked elsewhere, or expired),
   * which the caller should treat as success: the permission is not live either
   * way, and the list refresh will show that.
   */
  revokeGrant(grantId: string, company?: string | null): Promise<void> {
    return this.request<void>(
      "DELETE",
      `${this.scope(company)}/grants/${encodeURIComponent(grantId)}`,
    );
  }

  /** Capture feedback (optionally preview the exact issue body first). */
  feedback(input: FeedbackInput, company?: string | null): Promise<FeedbackResponse> {
    return this.request<FeedbackResponse>("POST", `${this.scope(company)}/feedback`, input);
  }

  /** This company's past reports, newest first. */
  listFeedback(company?: string | null): Promise<FeedbackSummary[]> {
    return this.request<FeedbackSummary[]>("GET", `${this.scope(company)}/feedback`);
  }

  /**
   * The host's runtime spec. Unauthenticated and company-agnostic, so it sits
   * outside `scope()`; the console reads `cycles_available` from it to tell
   * whether this instance is provisioned with a TinyHumans credential.
   */
  spec(): Promise<AppSpec> {
    return this.request<AppSpec>("GET", "/spec");
  }

  /**
   * The company's agent roster (forward-looking surface). Hosts that don't
   * expose `.../team` yet return 404 — callers fall back to a local roster.
   */
  listTeam(company?: string | null): Promise<TeamMemberDto[]> {
    return this.request<TeamMemberDto[]>("GET", `${this.scope(company)}/team`);
  }

  /**
   * Add an operator-defined teammate (a "team overlay" agent). Persists on the
   * host and shows up in `listTeam` afterwards — never returned by the write
   * itself, so callers should refetch. Hosts without the write plane 404;
   * callers fall back to a local-only add.
   */
  addTeamMember(
    input: { name: string; role: string; description?: string; budgetUsdDaily?: number },
    company?: string | null,
  ): Promise<TeamMemberDto> {
    return this.request<TeamMemberDto>("POST", `${this.scope(company)}/team`, input);
  }

  /**
   * Set, change, or remove a teammate's daily spend cap (issue #343). Admin-only
   * on the host — a member gets 403 — and the change is enforced on the
   * company's next dispatch, with no restart.
   *
   * Pass `null` to remove the cap and a number to set one; `0` is a real cap of
   * nothing, not the same thing as `null`. The argument is required precisely so
   * an accidental `undefined` cannot be serialised away into `{}`, which the host
   * rejects with a 422 rather than reading as "remove the cap".
   *
   * Returns the teammate's updated roster row, so the caller can refresh one
   * card instead of refetching the team.
   */
  setTeamBudget(
    agentId: string,
    budgetUsdDaily: number | null,
    company?: string | null,
  ): Promise<TeamMemberDto> {
    const body: SetBudgetInput = { budgetUsdDaily };
    return this.request<TeamMemberDto>(
      "PUT",
      `${this.scope(company)}/team/${encodeURIComponent(agentId)}/budget`,
      body,
    );
  }

  /**
   * Drop a teammate's cap override so the company's manifest default applies
   * again (issue #343). Admin-only.
   *
   * Not the same as `setTeamBudget(id, null)`: that stores "uncapped, decided by
   * an admin", while this restores whatever the company defines — which for a
   * manifest-capped teammate brings the cap back.
   */
  clearTeamBudgetOverride(agentId: string, company?: string | null): Promise<TeamMemberDto> {
    return this.request<TeamMemberDto>(
      "DELETE",
      `${this.scope(company)}/team/${encodeURIComponent(agentId)}/budget`,
    );
  }

  /** Remove an operator-added teammate. 409s for a manifest teammate (can't be removed here). */
  removeTeamMember(agentId: string, company?: string | null): Promise<void> {
    return this.request<void>(
      "DELETE",
      `${this.scope(company)}/team/${encodeURIComponent(agentId)}`,
    );
  }

  /**
   * The company's inboxes with unread counts (Inbox tab). Both inbound paths —
   * the ingest webhook and the IMAP poller — file into the store this reads, so
   * received mail appears here. Hosts without the surface 404; callers treat
   * that as "no inboxes".
   */
  listInboxes(company?: string | null): Promise<InboxDto[]> {
    return this.request<InboxDto[]>("GET", `${this.scope(company)}/inboxes`);
  }

  /** One inbox's messages, oldest first. */
  inboxMessages(key: string, company?: string | null): Promise<InboxMessageDto[]> {
    return this.request<InboxMessageDto[]>(
      "GET",
      `${this.scope(company)}/inboxes/${encodeURIComponent(key)}/messages`,
    );
  }

  /** Mark inbox messages read (the given ids, or all when omitted); returns the count still unread. */
  markInboxRead(
    key: string,
    ids?: string[],
    company?: string | null,
  ): Promise<{ unread: number }> {
    return this.request<{ unread: number }>(
      "POST",
      `${this.scope(company)}/inboxes/${encodeURIComponent(key)}/read`,
      ids ? { ids } : undefined,
    );
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

  /** Installed MCP servers plus their live connection state. */
  listMcpServers(company?: string | null): Promise<{ servers: McpServer[] }> {
    return this.request<{ servers: McpServer[] }>("GET", `${this.scope(company)}/mcp/servers`);
  }

  /** Persist a manual stdio or streamable-HTTP MCP server install. */
  installMcpServer(
    input: InstallMcpServerInput,
    company?: string | null,
  ): Promise<{ server_id: string }> {
    return this.request<{ server_id: string }>("POST", `${this.scope(company)}/mcp/servers`, input);
  }

  /** Connect an installed server and return its advertised tools. */
  connectMcpServer(serverId: string, company?: string | null): Promise<{ tools: McpTool[] }> {
    return this.request<{ tools: McpTool[] }>(
      "POST",
      `${this.scope(company)}/mcp/servers/${encodeURIComponent(serverId)}/connect`,
    );
  }

  /** Stop an installed server's live connection. */
  disconnectMcpServer(
    serverId: string,
    company?: string | null,
  ): Promise<{ disconnected: boolean }> {
    return this.request<{ disconnected: boolean }>(
      "POST",
      `${this.scope(company)}/mcp/servers/${encodeURIComponent(serverId)}/disconnect`,
    );
  }

  /** Disconnect and remove an MCP install. */
  uninstallMcpServer(serverId: string, company?: string | null): Promise<void> {
    return this.request<void>(
      "DELETE",
      `${this.scope(company)}/mcp/servers/${encodeURIComponent(serverId)}`,
    );
  }

  /** Cached tools advertised by a connected MCP server. */
  listMcpTools(serverId: string, company?: string | null): Promise<{ tools: McpTool[] }> {
    return this.request<{ tools: McpTool[] }>(
      "GET",
      `${this.scope(company)}/mcp/servers/${encodeURIComponent(serverId)}/tools`,
    );
  }

  /** Invoke one tool through a connected MCP server. */
  callMcpTool(
    serverId: string,
    tool: string,
    arguments_: Record<string, unknown>,
    company?: string | null,
  ): Promise<McpCallResult> {
    return this.request<McpCallResult>(
      "POST",
      `${this.scope(company)}/mcp/servers/${encodeURIComponent(serverId)}/tools/${encodeURIComponent(tool)}/call`,
      { arguments: arguments_ },
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

/**
 * The `filename` out of a `Content-Disposition` header, when the host sent one.
 *
 * Deliberately narrow: it accepts the quoted `attachment; filename="…"` form
 * this codebase emits and nothing else, and it drops any path separator, so a
 * header cannot steer a download out of the browser's download directory.
 * Returns `undefined` when there is nothing usable, leaving the caller to fall
 * back rather than saving a file named after a malformed header.
 */
function attachmentFilename(header: string | null): string | undefined {
  if (!header) return undefined;
  const match = /filename="([^"]+)"/i.exec(header);
  const name = match?.[1]?.split(/[\\/]/).pop()?.trim();
  return name && name !== "." && name !== ".." ? name : undefined;
}

/** How much of an unrecognised body is kept on `ApiError.detail`. */
const DETAIL_CHARS = 2_000;

/**
 * `JSON.parse`, or `undefined` when the body is not JSON.
 *
 * This used to be `safeJson`, which failed **open**: an unparseable body became
 * `{ error: text, code: "unparseable" }`, so the body itself arrived at the
 * throw sites below wearing the shape of the host's error envelope and was
 * taken as the operator-facing message. On a hosted tenant that meant an nginx
 * `504` page — `<html><head><title>…`, padding comments and all — was rendered
 * as the reason a request failed (issue #380). Returning `undefined` is what
 * makes the fallback below reachable.
 */
function parseJson(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
}

/**
 * The host's `{error, code}` envelope, or `undefined` when the body is not one.
 *
 * Both fields are required and both must be strings, matching `ApiErrorBody`
 * and what the host actually emits (`server/error.rs` serialises
 * `{error: Display, code: &str}` for every route). Anything else — a bare JSON
 * string, an array, a `null`, a proxy's JSON-shaped health blob — is not our
 * envelope, and guessing at it is how an arbitrary upstream body becomes UI
 * prose. The strictness is the point: this predicate is the only thing standing
 * between a foreign response body and `ApiError.message`.
 */
function errorEnvelope(text: string): ApiErrorBody | undefined {
  const parsed = parseJson(text);
  if (typeof parsed !== "object" || parsed === null) return undefined;
  const { error, code } = parsed as Record<string, unknown>;
  if (typeof error !== "string" || typeof code !== "string") return undefined;
  return { error, code };
}

/**
 * A short, safe description of a status the host refused on.
 *
 * `statusText` first, but it cannot be the whole answer: HTTP/2 and HTTP/3
 * carry no reason phrase, so `res.statusText` is `""` on most hosted
 * deployments — exactly the tenants that sit behind the proxy this bug came
 * from. Falling back to `statusText` alone would have traded an HTML dump for a
 * blank message, so `HTTP 504` is the floor.
 */
function statusMessage(res: Response): string {
  return res.statusText.trim() || `HTTP ${res.status}`;
}

/**
 * The `ApiError` for a response the host refused (issue #380).
 *
 * Shared by `request` and `getDocument`, which had drifted into two
 * byte-identical copies of this logic — so the bug existed twice and had to be
 * fixed twice. One function means the next change to error handling cannot land
 * on only one of the two readers.
 */
function httpError(res: Response, text: string): ApiError {
  const envelope = errorEnvelope(text);
  const err = new ApiError(
    res.status,
    envelope?.code ?? `http_${res.status}`,
    envelope?.error ?? statusMessage(res),
    envelope !== undefined,
  );
  // Not discarded, just not rendered. A proxy error page is the only clue to
  // which hop gave up, which is worth keeping for a bug report even though it
  // is worthless as prose.
  if (!envelope && text.trim()) {
    err.detail = text.slice(0, DETAIL_CHARS);
    console.debug(`[api] ${res.status} ${res.url}: unrecognised error body`, err.detail);
  }
  return err;
}

/**
 * Whether a resolve answered with a **receipt** rather than the follow-up
 * turn's replies (#383).
 *
 * Structural, not a flag read. `detach: true` is a *request*, and a host that
 * predates the option ignores it and answers with `responses` — trusting the
 * request would then have the caller read `recorded` off a body that has no
 * such key and report a decision as unrecorded. Distinguishing on `recorded`
 * versus `responses` reads what actually arrived.
 */
function isResolveReceipt(answer: unknown): answer is ResolveReceipt {
  if (typeof answer !== "object" || answer === null) return false;
  const body = answer as Record<string, unknown>;
  return typeof body.recorded === "boolean" && !Array.isArray(body.responses);
}
