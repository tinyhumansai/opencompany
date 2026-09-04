// A typed, company-agnostic client for the OpenCompany operator API.
//
// The same instance serves both deployment shapes:
//   - Single-company (prosumer): construct with `company = null`; calls use the
//     host's `/api/v1/company/*` aliases for the sole registered company.
//   - Multi-company (platform): pass a company id per call (or as the default),
//     and calls use `/api/v1/companies/{id}/*`.

import type { ConsoleConfig } from "../config";
import type { MessageIntent } from "./tasks";
import { defaultTransport, needsCarriedSession } from "./transport";
import type { StreamHandlers, Transport, TransportResponse } from "./transport";
import {
  type AgentDetailDto,
  ApiError,
  type BlockerVerdict,
  type BoardComment,
  type BoardDetail,
  type BoardItem,
  type BoardPage,
  type BoardQuery,
  type BoardVote,
  type ReadMarker,
  type ChatMentionInput,
  type MarkNotificationsReadResponse,
  type NotificationFeedResponse,
  type MentionablesResponse,
  type PresenceListResponse,
  type ReadStateResponse,
  type ApiErrorBody,
  type WorkflowProblem,
  type AppSpec,
  type ApprovalSummary,
  type CapabilityStatusDto,
  type ChatHistoryMessageDto,
  type ChatPostResult,
  type ChatResponse,
  type ChatReviewReceipt,
  type CompanyStatus,
  type ConnectionState,
  type CreateDeskInput,
  type DeskDto,
  type EditAgentInput,
  type FeedbackInput,
  type FeedbackResponse,
  type FeedbackSummary,
  type FinancesDto,
  type GrantScope,
  type HarnessDto,
  type BudgetPauseMarker,
  type InboxDto,
  type InboxMessageDto,
  type OperatorChannelDto,
  type PageManifestDto,
  type ProvisioningInfo,
  type ResolveReceipt,
  type SetBudgetInput,
  type StandingGrant,
  type TeamMemberDto,
  type UsageDto,
  type Verdict,
} from "./types";

export type LifecycleAction = "pause" | "resume" | "suspend" | "archive";

/** Per-call overrides for a request's deadline and cancellation. */
export interface RequestOptions {
  /**
   * A caller-owned signal — an unmount, a superseded read — that cancels the
   * request. Its abort surfaces as an `AbortError`, distinct from a timeout, so
   * the caller can tell "I cancelled this" from "the host went away".
   */
  signal?: AbortSignal;
  /**
   * How long to wait for the host before giving up, in milliseconds. Omit for
   * the method default ({@link DEFAULT_REQUEST_TIMEOUT_MS} on a `GET`, none on a
   * mutation, whose duration is the host's to decide). `null` disables the
   * bound explicitly for a read that is expected to run long.
   */
  timeoutMs?: number | null;
}

/**
 * The default deadline for a `GET`.
 *
 * Reads are the request the console blocks a view on, and a host that accepts
 * the connection and then never answers used to leave that view on its skeleton
 * forever — the browser's `fetch` produces no event for a stalled response, so
 * no `catch` an already-written load path holds ever runs (issue #2014). This
 * turns that silence into an ordinary rejection every view's existing error
 * state can render. Well above any healthy read; mutations opt in per call,
 * since a chat turn or a company provision is legitimately unbounded.
 */
export const DEFAULT_REQUEST_TIMEOUT_MS = 30_000;

/**
 * The `/spec` capability a host advertises when `blocker_verdict` reaches a
 * blocker resume rather than being ignored. Mirrors the string pushed in
 * `AppState::capabilities` (`src/app/types.rs`); a host that does not name it
 * lowers every four-way answer to its two-way form.
 */
const BLOCKER_VERDICT_CAPABILITY = "blocker-verdict";

export class OpenCompanyClient {
  readonly baseUrl: string;
  readonly defaultCompany: string | null;
  private readonly token: string | null;
  private readonly session: string | null;
  private readonly transport: Transport;
  private capabilityProbe: Promise<string[] | undefined> | null = null;

  constructor(
    config: Pick<ConsoleConfig, "baseUrl" | "company" | "operatorToken" | "sessionHeader">,
    // Injected so a desktop shell can route the same client through its own
    // core, and so tests can drive one without a network. Defaults to the
    // browser's `fetch`/`EventSource`, which is what every web build uses.
    transport: Transport = defaultTransport(),
  ) {
    this.baseUrl = config.baseUrl;
    this.defaultCompany = config.company;
    this.token = config.operatorToken;
    this.session = config.sessionHeader ?? null;
    this.transport = transport;
  }

  /**
   * The credential headers every request carries, whichever kind this client
   * holds.
   *
   * One method rather than the line repeated at each call site, because a
   * request path that forgot one would not fail loudly — it would silently
   * make an *anonymous* request, and the surfaces that read fine anonymously
   * would look like they worked.
   *
   * Both may be present: a platform bearer authenticates the *hosting layer*
   * and a session authenticates a *person*, and a hub console holding a
   * platform token still signs its operator in per tenant.
   */
  private authHeaders(): Record<string, string> {
    const headers: Record<string, string> = {};
    if (this.token) {
      headers["authorization"] = `Bearer ${this.token}`;
    }
    // Only ever set for a connection that cannot use a cookie — a console on a
    // different origin from its host. Same-origin consoles leave this null and
    // keep the HttpOnly cookie, which nothing here can read. See
    // `SESSION_CARRIER_HEADER` in the host's `users/cookie.rs`.
    if (this.session) headers["x-opencompany-session"] = this.session;
    return headers;
  }

  /** Resolves the `/companies/{id}` vs single-company `/company` route prefix. */
  private scope(company: string | null | undefined): string {
    const id = company ?? this.defaultCompany;
    return id ? `/api/v1/companies/${encodeURIComponent(id)}` : "/api/v1/company";
  }

  /** Called on any 401, so the app can drop to the login view. */
  onUnauthorized: (() => void) | null = null;

  private async request<T>(
    method: string,
    path: string,
    body?: unknown,
    extraHeaders?: Record<string, string>,
    options?: RequestOptions,
  ): Promise<T> {
    const headers: Record<string, string> = {};
    if (body !== undefined) headers["content-type"] = "application/json";
    Object.assign(headers, this.authHeaders(), extraHeaders);

    const timeoutMs =
      options?.timeoutMs !== undefined
        ? options.timeoutMs
        : method === "GET"
          ? DEFAULT_REQUEST_TIMEOUT_MS
          : null;
    const caller = options?.signal;
    const controller = new AbortController();
    const onCallerAbort = () => controller.abort(caller?.reason);
    if (caller) {
      if (caller.aborted) controller.abort(caller.reason);
      else caller.addEventListener("abort", onCallerAbort, { once: true });
    }
    let timedOut = false;
    const timer =
      timeoutMs === null
        ? null
        : setTimeout(() => {
            timedOut = true;
            controller.abort();
          }, timeoutMs);

    let res: TransportResponse;
    try {
      res = await settleWithin(
        this.transport.request({
          method,
          url: `${this.baseUrl}${path}`,
          headers,
          body: body === undefined ? undefined : JSON.stringify(body),
          signal: controller.signal,
        }),
        controller.signal,
      );
    } catch (err) {
      if (timedOut) {
        throw new ApiError(
          0,
          "timeout",
          `the company host at ${this.baseUrl || "this origin"} did not respond in time`,
        );
      }
      // The caller tore the request down itself; let its `AbortError` through,
      // the way `getBlob` does, so it can tell "cancelled" from "unreachable".
      if (caller?.aborted) throw abortError(err);
      throw new ApiError(0, "network_error", `cannot reach the company host at ${this.baseUrl || "this origin"}`);
    } finally {
      if (timer !== null) clearTimeout(timer);
      if (caller) caller.removeEventListener("abort", onCallerAbort);
    }

    const text = res.text;
    if (!isOk(res)) {
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

  /**
   * Whether this client sends a **platform** bearer.
   *
   * Asked by the surfaces that offer `PlatformScope` routes — `suspend` and
   * `archive` (issue #1401). Those resolve through `resolve_claims`, which
   * cannot return a human, so a console authenticating as a person through the
   * session cookie is refused by construction rather than by policy: there is
   * no credential it could hold, and no setting an operator could change, that
   * would let the call through. A control for one of them is only honest on a
   * client that answers `true` here.
   *
   * True does not promise the call succeeds — the bearer still has to carry the
   * `platform` scope, and a tenant token without it gets a `403`. That is a
   * configuration mistake with a legible answer, which is a different thing
   * from an unreachable button.
   */
  get carriesPlatformBearer(): boolean {
    return Boolean(this.token);
  }

  /** The route prefix for `company`, for callers building their own paths. */
  scopeFor(company: string | null | undefined): string {
    return this.scope(company);
  }

  /** A typed GET, for surfaces that live outside this class (e.g. auth). */
  get<T>(path: string, options?: RequestOptions): Promise<T> {
    return this.request<T>("GET", path, undefined, undefined, options);
  }

  /**
   * What this host advertises at `/spec`, read once and shared by every
   * caller after that.
   *
   * `undefined` is the meaningful answer, not an error case: a host predating
   * the field omits it, and that must read as "assume REST only" rather than
   * as "supports nothing". A `/spec` that cannot be reached at all answers the
   * same way — an unreachable capability is one this client must not rely on,
   * and the request the caller actually wanted still gets to fail on its own
   * terms rather than being masked by a probe failure.
   */
  private hostCapabilities(): Promise<string[] | undefined> {
    this.capabilityProbe ??= this.get<Record<string, unknown>>("/spec")
      .then((spec) =>
        Array.isArray(spec.capabilities) ? (spec.capabilities as string[]) : undefined,
      )
      .catch(() => undefined);
    return this.capabilityProbe;
  }

  /** Whether this host names `capability` in its `/spec`. */
  async supports(capability: string): Promise<boolean> {
    return (await this.hostCapabilities())?.includes(capability) ?? false;
  }

  /**
   * A POST carrying `FormData`, for the workspace upload route (issue #553).
   *
   * Separate from `post` because the two are incompatible in both directions:
   * `request` sets `content-type: application/json` and `JSON.stringify`s its
   * body, and a multipart upload must instead let the browser set the header so
   * it can include the boundary it generated. Setting it by hand produces a
   * body the server cannot parse.
   *
   * Like `getBlob`, this reaches `fetch` directly — the `Transport` surface
   * takes a `string` body and cannot express a `FormData`.
   */
  async postForm<T>(path: string, form: FormData): Promise<T> {
    const headers: Record<string, string> = {};
    Object.assign(headers, this.authHeaders());

    let res: Response;
    try {
      res = await fetch(`${this.baseUrl}${path}`, {
        method: "POST",
        headers,
        body: form,
        credentials: "include",
      });
    } catch {
      throw new ApiError(
        0,
        "network_error",
        `cannot reach the company host at ${this.baseUrl || "this origin"}`,
      );
    }
    const text = await res.text();
    if (!(res.status >= 200 && res.status < 300)) {
      if (res.status === 401) this.onUnauthorized?.();
      // Adapted to the shape `httpError` reads, so a failed direct-`fetch`
      // read produces the same `ApiError` envelope as every other route.
      throw httpError(
        {
          status: res.status,
          statusText: res.statusText,
          url: res.url,
          text,
          header: (name: string) => res.headers.get(name),
        },
        text,
      );
    }
    return (text ? parseJson(text) : undefined) as T;
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
    Object.assign(headers, this.authHeaders());

    let res: TransportResponse;
    try {
      res = await this.transport.request({ method: "GET", url: `${this.baseUrl}${path}`, headers });
    } catch {
      throw new ApiError(0, "network_error", `cannot reach the company host at ${this.baseUrl || "this origin"}`);
    }
    const text = res.text;
    if (!isOk(res)) {
      if (res.status === 401) this.onUnauthorized?.();
      throw httpError(res, text);
    }
    return { text, filename: attachmentFilename(res.header("content-disposition")) };
  }

  /**
   * A GET whose answer is **bytes** (issue #553).
   *
   * The workspace can hold images, PDFs and archives now, and the console has
   * to be able to show one. `getDocument` above cannot: the whole `Transport`
   * surface is text — `TransportResponse` exposes `res.text` and nothing else —
   * so anything read through it has already been decoded and a PNG would come
   * back mangled.
   *
   * So this is the one method that reaches past the transport to `fetch`
   * directly, because it needs a `Blob` the transport has no way to express.
   * Everything else about it matches `getDocument`: the same bearer header, the
   * same `credentials: "include"`, the same 401 hand-off.
   *
   * It returns a `Blob` rather than a URL so the caller owns the object URL's
   * lifetime — an object URL leaks until it is revoked, and only the component
   * holding it knows when that is.
   */
  async getBlob(path: string, signal?: AbortSignal): Promise<Blob> {
    const headers: Record<string, string> = {};
    Object.assign(headers, this.authHeaders());

    let res: Response;
    try {
      res = await fetch(`${this.baseUrl}${path}`, {
        method: "GET",
        headers,
        credentials: "include",
        signal,
      });
    } catch (err) {
      // An aborted fetch is the caller's own doing — a preview that scrolled
      // out of view tore the request down deliberately — not a connection
      // failure. Let the `AbortError` through so the caller can tell
      // "cancelled" from "couldn't reach the host" (codex review finding).
      if (err instanceof Error && err.name === "AbortError") throw err;
      throw new ApiError(
        0,
        "network_error",
        `cannot reach the company host at ${this.baseUrl || "this origin"}`,
      );
    }
    if (!res.ok) {
      if (res.status === 401) this.onUnauthorized?.();
      // The body of a failed blob read is an error envelope, not bytes, so it
      // is safe (and useful) to read it as text for the message.
      const text = await res.text().catch(() => "");
      // Adapted to the shape `httpError` reads, so a failed direct-`fetch`
      // read produces the same `ApiError` envelope as every other route.
      throw httpError(
        {
          status: res.status,
          statusText: res.statusText,
          url: res.url,
          text,
          header: (name: string) => res.headers.get(name),
        },
        text,
      );
    }
    return res.blob();
  }

  /**
   * Subscribes to this host's company event stream.
   *
   * On the client rather than in the hook so that "everything that talks to a
   * host goes through the client" stays true — the hook would otherwise have to
   * know which transport it is on, and every future caller would too.
   */
  subscribeToEvents(company: string | null | undefined, handlers: StreamHandlers): () => void {
    return this.transport.subscribe(
      `${this.baseUrl}${this.scope(company)}/events`,
      handlers,
      // The same credential the request path carries. A stream authenticated
      // differently from the requests beside it would load every view and then
      // never update one.
      this.authHeaders(),
    );
  }

  /** A typed POST, for surfaces that live outside this class (e.g. auth). */
  post<T>(path: string, body?: unknown): Promise<T> {
    return this.request<T>("POST", path, body);
  }

  /**
   * Whether a sign-in through this client yields a session it must hold itself.
   *
   * Exposed so a caller knows to *store* what {@link postSignIn} returns. It is
   * the same question `needsCarriedSession` answers, kept on the client so no
   * view has to re-derive it from a base url.
   */
  get carriesOwnSession(): boolean {
    return needsCarriedSession(this.baseUrl);
  }

  /**
   * A POST to a sign-in route, asking for a carrier this client can use.
   *
   * Separate from {@link post} so the carrier request cannot leak onto an
   * ordinary call: the header is meaningless everywhere but a route that mints
   * a session, and a client sending it indiscriminately would be asserting
   * something about itself on every request it makes.
   *
   * On a same-origin console this is exactly `post` — no header, and the host
   * replies with the `HttpOnly` cookie it always did.
   */
  postSignIn<T>(path: string, body?: unknown): Promise<T> {
    return this.request<T>(
      "POST",
      path,
      body,
      this.carriesOwnSession ? { "x-opencompany-session-carrier": "header" } : undefined,
    );
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
    /**
     * What this message is for (issues #580, #845, #1152): `"once"` and
     * `"workflow"` say what the card it opens produces, `"chat"` says it is not
     * a request for work and no card should be opened for it.
     *
     * Everything except `"once"` reaches the wire; `"once"` and no selection
     * are sent as *nothing at all*. That preserves the historical wire shape:
     * an unmarked message posts exactly what it did before any of these controls
     * existed, so the host can apply its normal triage without a browser-asserted
     * override — the same omitted-field compatibility rule the deliverable field
     * follows everywhere (see `CreateTask.deliverable`).
     *
     * One key, not two. `"chat"` rides `deliverable` rather than arriving as a
     * second `intent` field, so a body cannot claim "build me the workflow" and
     * "just chatting" about the same message.
     */
    intent?: MessageIntent,
    /**
     * Ask for the turn's id instead of its answer (issue #983): the host
     * journals the message, mints a durable turn row and answers `202` without
     * holding the request open for a turn whose duration is unbounded.
     *
     * Sending this does **not** mean a detached answer came back. A host that
     * predates the field ignores it and answers the full synchronous `200`, so
     * the caller must branch on the returned shape via `isDetachedChat` — which
     * is exactly why this returns a union rather than the detached type.
     */
    detach?: boolean,
    /**
     * Workspace node ids of files attached to this message (issue #1682).
     *
     * Ids only — each was returned by `uploadChatAttachment` after the file's
     * bytes were uploaded. The host re-resolves every id within this company's
     * own workspace and takes the name / mime / size from the store, so the
     * client neither can nor needs to send those. Sent only when non-empty, so
     * a message with no attachment keeps the exact pre-#1682 body shape — the
     * same omitted-field rule `deliverable` and `detach` follow.
     */
    attachments?: string[],
    /**
     * Who this message names, as the picker resolved them.
     *
     * Sent only when the picker actually resolved something, so an ordinary
     * post keeps the exact body shape it had before mentions existed — the
     * same omitted-field rule `deliverable` and `detach` follow.
     *
     * The host re-validates every entry against the live roster and demotes
     * what no longer resolves, so this is a suggestion, not an instruction.
     * Omitting it entirely asks the host to extract from the text instead.
     */
    mentions?: ChatMentionInput[],
  ): Promise<ChatPostResult> {
    const body: {
      text: string;
      chat?: string;
      parent?: string;
      deliverable?: MessageIntent;
      detach?: boolean;
      attachments?: string[];
      mentions?: ChatMentionInput[];
    } = {
      text,
    };
    if (chat) body.chat = chat;
    if (parent) body.parent = parent;
    if (intent && intent !== "once") body.deliverable = intent;
    // Sent only when asked for, so an ordinary post keeps the exact body shape
    // it had before #983 — the same omitted-field rule `deliverable` follows.
    if (detach) body.detach = detach;
    if (attachments && attachments.length > 0) body.attachments = attachments;
    // `undefined` means the client has no directory and asks the host to
    // extract mentions. An explicit empty list means the loaded directory
    // resolved none and must suppress fallback extraction.
    if (mentions !== undefined) body.mentions = mentions;
    return this.request<ChatPostResult>("POST", `${this.scope(company)}/chat`, body);
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

  /** The identity of the company's durable, read-only Operator feed. */
  getOperatorChannel(company?: string | null): Promise<OperatorChannelDto> {
    return this.request<OperatorChannelDto>("GET", `${this.scope(company)}/operator-channel`);
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
   * line. `before` is an exclusive message-id cursor and `limit` bounds one
   * page; callers decide whether an unavailable route is an empty transcript
   * or an error they need to surface.
   */
  getChatHistory(
    desk?: string | null,
    company?: string | null,
    options?: { before?: string; limit?: number },
  ): Promise<ChatHistoryMessageDto[]> {
    const query = new URLSearchParams();
    if (desk) query.set("desk", desk);
    if (options?.before) query.set("before", options.before);
    if (options?.limit !== undefined) query.set("limit", String(options.limit));
    const qs = query.size > 0 ? `?${query}` : "";
    return this.request<ChatHistoryMessageDto[]>(
      "GET",
      `${this.scope(company)}/chat/history${qs}`,
    );
  }

  /**
   * Where the signed-in person has read to, per channel (issue #755).
   *
   * A host that predates this route answers 404; the caller treats that as "no
   * markers" and falls back to the in-browser floor, so an older host degrades
   * to the previous behaviour rather than throwing on load.
   */
  readState(company?: string | null): Promise<ReadStateResponse> {
    return this.request<ReadStateResponse>("GET", `${this.scope(company)}/chat/read-state`);
  }

  /**
   * This person's notification feed, filtered to one `kind` — mentions by
   * default.
   *
   * The durable half of a mention: the live feed only reaches an open browser,
   * so a mention that landed overnight is here and nowhere else. Issue #1845's
   * week-1 nudge banner is the second consumer, passing `kind:
   * "workflow_nudge"` — see the route's own docs
   * (`src/server/ops/notifications.rs`) for why this is a query parameter
   * rather than a second route.
   *
   * A host that predates this route answers 404; callers treat that as an empty
   * feed and simply show no badge/banner, rather than throwing on load.
   */
  notifications(company?: string | null, kind?: string): Promise<NotificationFeedResponse> {
    const query = kind ? `?kind=${encodeURIComponent(kind)}` : "";
    return this.request<NotificationFeedResponse>(
      "GET",
      `${this.scope(company)}/notifications${query}`,
    );
  }

  /**
   * Mark notifications read for this person.
   *
   * Omitting `ids` marks everything they can see — what "clear the badge"
   * means. An explicitly empty array marks nothing, which is a different
   * instruction and is honoured as one.
   *
   * Answers with what is *actually* still unread rather than what the caller
   * expects, because marking is a latch and two tabs race constantly.
   */
  markNotificationsRead(
    ids?: string[],
    company?: string | null,
  ): Promise<MarkNotificationsReadResponse> {
    return this.request<MarkNotificationsReadResponse>(
      "PUT",
      `${this.scope(company)}/notifications`,
      ids ? { ids } : {},
    );
  }

  /**
   * Everything an `@` can name: teammates, people, desks, and the broadcast
   * token's spellings.
   *
   * A host that predates this route answers 404; the caller treats that as
   * "no picker" and typing an `@` stays plain text, which the host still
   * extracts what it can from. So an older host degrades to the previous
   * behaviour rather than throwing on load.
   */
  mentionables(company?: string | null): Promise<MentionablesResponse> {
    return this.request<MentionablesResponse>(
      "GET",
      `${this.scope(company)}/chat/mentionables`,
    );
  }

  /** Who is present on this replica right now. */
  presence(company?: string | null): Promise<PresenceListResponse> {
    return this.request<PresenceListResponse>("GET", `${this.scope(company)}/presence`);
  }

  /**
   * A heartbeat, and what to appear as.
   *
   * The body deliberately carries **no user id**: the host takes the subject
   * from the session, so no caller can move somebody else's dot. `consoleId`
   * is not an identity either — it is this tab's opaque lease key, so closing
   * one of several open tabs drops only that tab's lease rather than logging
   * every tab for this person out (see `usePresence`'s `consoleId`).
   */
  announcePresence(
    status: "online" | "away" | "offline",
    company?: string | null,
    consoleId?: string,
  ): Promise<void> {
    return this.request<void>("PUT", `${this.scope(company)}/presence`, {
      status,
      ...(consoleId ? { consoleId } : {}),
    });
  }

  /**
   * Clear this console's dot on the way out.
   *
   * Goes through `this.transport`, the same seam every other call on this
   * class uses — **not** a direct `fetch`, which this used to be. A desktop
   * console's webview cannot satisfy this route on its own even with the
   * right headers: it is cross-origin with the host (so a direct request
   * needs CORS the host does not grant it) and the device credential lives
   * only in the Rust core's keychain, never in JS. `ProxyTransport` is what
   * gets both right, and only routing through `this.transport` reaches it.
   *
   * The one thing this needs beyond an ordinary request — surviving the
   * document going away, since this fires from `pagehide` — is
   * `keepalive: true`, threaded through `TransportRequest` for exactly this
   * call. `BrowserTransport` forwards it to `fetch`'s own `keepalive` option;
   * `ProxyTransport` ignores it, because a Tauri `invoke` is core-process IPC
   * with no equivalent teardown-survival problem. `sendBeacon` would be the
   * usual browser-only tool here but cannot issue a `DELETE` or run through
   * the desktop bridge at all.
   *
   * Carries the same session and bearer headers `request` would attach —
   * `credentials: "include"` alone (still set unconditionally inside
   * `BrowserTransport`) only carries a same-origin cookie, and a console
   * authenticated cross-origin holds its session in
   * `x-opencompany-session`/`authorization` instead.
   *
   * Best-effort by design, and allowed to fail silently: if it does not land,
   * the host's lease expires the dot within a few minutes anyway. That is the
   * whole reason presence is a lease — no disconnect path has to be correct.
   */
  disconnectPresenceBeacon(company?: string | null, consoleId?: string): void {
    const query = consoleId ? `?consoleId=${encodeURIComponent(consoleId)}` : "";
    void this.transport
      .request({
        method: "DELETE",
        url: `${this.baseUrl}${this.scope(company)}/presence${query}`,
        headers: this.authHeaders(),
        keepalive: true,
      })
      .catch(() => {
        // Best-effort by design; see this method's doc comment.
      });
  }

  /**
   * Say this console is typing. Fire-and-forget: an undelivered ping is not
   * worth a retry, and the indicator expires on its own regardless.
   */
  typing(
    chatId: string,
    parentId?: string,
    company?: string | null,
  ): Promise<void> {
    return this.request<void>("POST", `${this.scope(company)}/chat/typing`, {
      chatId,
      ...(parentId ? { parentId } : {}),
    });
  }

  /**
   * Moves one channel's read floor forward.
   *
   * The host's write is monotonic, and it answers with where the marker
   * actually stands — which is not always what was sent, because two tabs of
   * one person race constantly.
   */
  markChannelRead(
    channelId: string,
    lastReadAt: number,
    company?: string | null,
  ): Promise<ReadMarker> {
    return this.request<ReadMarker>("PUT", `${this.scope(company)}/chat/read-state`, {
      channelId,
      lastReadAt,
    });
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
    _note?: string,
    company?: string | null,
    options: {
      detach?: boolean;
      scope?: GrantScope;
      /**
       * The four-way answer to a parked blocker (#2028). It narrows `verdict`
       * rather than replacing it — the host refuses a pair that disagrees — and
       * `answer` is mandatory and non-blank on `amend`, refused on the rest.
       *
       * Negotiated before it is sent: `skip` and `amend` are the two whose
       * lowered form asks an unaware host for a different action, so both are
       * refused against a host that does not advertise `blocker-verdict`. See
       * {@link refuseUnperformableBlockerVerdict}.
       */
      blocker?: { verdict: BlockerVerdict; answer?: string };
    } = {},
  ): Promise<ChatResponse | ResolveReceipt> {
    const body: {
      verdict: Verdict;
      detach?: boolean;
      scope?: "once" | "tool";
      expires_in_millis?: number;
      blocker_verdict?: BlockerVerdict;
      blocker_answer?: string;
    } = { verdict };
    if (options.detach) body.detach = true;
    // Sent as nothing at all when absent, for the reason `once` is: the
    // omitted-field form is what a host predating the field understands.
    if (options.blocker) {
      await this.refuseUnperformableBlockerVerdict(options.blocker.verdict);
      body.blocker_verdict = options.blocker.verdict;
      if (options.blocker.verdict === "amend") {
        body.blocker_answer = options.blocker.answer ?? "";
      }
    }
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

  /**
   * Refuses a blocker verdict this host would carry out as a different action.
   *
   * A host predating `blocker_verdict` ignores the unknown field and resolves
   * from the lowered two-way `verdict` alone. Two of the four survive that:
   * `retry` rides an `approve` and `cancel` rides a `deny`, and an unaware host
   * retries and cancels exactly as asked, so both are still sent. The other two
   * do not. `skip` rides an `approve`, so an unaware host **re-runs the step it
   * was asked to leave out**; `amend` rides one too, so it re-runs the step
   * **without the operator's words**. Either way the console would report the
   * four-way result it asked for while the host performed something else.
   *
   * Refusing is the point: a request that never leaves is a failure the
   * operator can see and act on, where a lowered one is a wrong action nobody
   * is told about.
   */
  private async refuseUnperformableBlockerVerdict(verdict: BlockerVerdict): Promise<void> {
    if (verdict !== "skip" && verdict !== "amend") return;
    if (await this.supports(BLOCKER_VERDICT_CAPABILITY)) return;
    throw new Error(
      `This host cannot ${verdict === "skip" ? "skip a stopped step" : "answer a stopped step in words"}: ` +
        "it is running a version that would run the step again instead. " +
        "Retry or Cancel it here, or update the host.",
    );
  }

  /**
   * Settle the in-review dispatch card a chat thread is reviewing: `"approve"`
   * finishes it, `"revise"` re-runs it with `note`. This is the board card the
   * origin thread is reviewing — **not** the native-tool approval gate
   * {@link resolveApproval} settles.
   *
   * `chatId` is the origin conversation (the channel/desk id) whose in-review
   * card this settles; `taskId` is the specific card the operator clicked —
   * a desk can have more than one card `in_review` at once, so the host
   * validates the verdict against that card rather than resolving by `chatId`
   * alone. Hosts predating the route return 404 — callers roll back their
   * optimistic move.
   */
  reviewCard(
    chatId: string,
    taskId: string,
    decision: "approve" | "revise",
    note?: string,
    company?: string | null,
  ): Promise<ChatReviewReceipt> {
    const body: { chatId: string; taskId: string; decision: string; note?: string } = {
      chatId,
      taskId,
      decision,
    };
    if (note) body.note = note;
    return this.request<ChatReviewReceipt>(
      "POST",
      `${this.scope(company)}/chat/review`,
      body,
    );
  }

  /**
   * The parked budget-pause marker for an agent (issue #1846), or `null` when
   * nothing is paused. Read-only — does not consume the marker.
   */
  getBudgetPause(
    agentId: string,
    company?: string | null,
  ): Promise<BudgetPauseMarker | null> {
    return this.request<BudgetPauseMarker | null>(
      "GET",
      `${this.scope(company)}/agents/${encodeURIComponent(agentId)}/budget-pause`,
    );
  }

  /**
   * The Add-Credits CTA (issue #1846): redeems the parked marker and
   * re-dispatches the original message. Not true resume (#561) — a fresh
   * turn runs from the top on the same chat thread the pause happened on.
   *
   * `expectedId` is the marker id the caller last read via
   * {@link getBudgetPause} (issue #1846 review, Codex #3866418876 /
   * #3866802268) — sent as `?id=` so the server can refuse with a `409` when
   * a background turn (a workflow node, an unstreamed task) has since
   * overwritten the SAME agent's marker with one that has no chat
   * destination, rather than silently re-dispatching whatever is parked NOW
   * under the assumption it is still what the operator clicked. Omitted only
   * for a caller with no prior read to compare against, in which case the
   * server falls back to its pre-fix unconditional redeem.
   */
  redeemBudgetPause(
    agentId: string,
    company?: string | null,
    expectedId?: string | null,
  ): Promise<BudgetPauseMarker> {
    const qs = expectedId ? `?id=${encodeURIComponent(expectedId)}` : "";
    return this.request<BudgetPauseMarker>(
      "POST",
      `${this.scope(company)}/agents/${encodeURIComponent(agentId)}/budget-pause/redeem${qs}`,
    );
  }

  /**
   * Push a parked approval's deadline out to a fresh full TTL window (#1805),
   * so a stalled run does not default-deny before someone can decide it.
   *
   * Returns the approval's **new** default-deny instant (epoch-millis) — the
   * number the card's countdown will now project — so the caller can redraw the
   * deadline without re-fetching the whole approvals list. A 404 means there was
   * nothing to extend: an unknown id, or one that has since resolved or expired,
   * which the caller should treat by refreshing the list rather than as a
   * failure to report.
   */
  async extendApproval(approvalId: string, company?: string | null): Promise<number> {
    const answer = await this.request<{ expiresAtMillis: number }>(
      "POST",
      `${this.scope(company)}/approvals/${encodeURIComponent(approvalId)}/extend`,
    );
    return answer.expiresAtMillis;
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
   * One page of the shared feedback board.
   *
   * Rejects with a 404 `tinyhumans_no_board` on a host with no TinyHumans
   * credential — there is no board to show, which is a different thing from an
   * empty one, so the caller hides the surface instead of rendering "nobody has
   * asked for anything yet".
   */
  feedbackBoard(query: BoardQuery = {}, company?: string | null): Promise<BoardPage> {
    const search = new URLSearchParams();
    if (query.sort) search.set("sort", query.sort);
    if (query.kind) search.set("type", query.kind);
    if (query.status) search.set("status", query.status);
    if (query.page !== undefined) search.set("page", String(query.page));
    if (query.limit !== undefined) search.set("limit", String(query.limit));
    const suffix = search.toString() ? `?${search}` : "";
    return this.request<BoardPage>("GET", `${this.scope(company)}/feedback/board${suffix}`);
  }

  /** One board item with its comments. */
  feedbackBoardItem(id: string, company?: string | null): Promise<BoardDetail> {
    return this.request<BoardDetail>(
      "GET",
      `${this.scope(company)}/feedback/board/${encodeURIComponent(id)}`,
    );
  }

  /** Casts (or, with `0`, retracts) this instance's vote. Returns the new row. */
  voteFeedbackBoard(id: string, value: BoardVote, company?: string | null): Promise<BoardItem> {
    return this.request<BoardItem>(
      "POST",
      `${this.scope(company)}/feedback/board/${encodeURIComponent(id)}/vote`,
      { value },
    );
  }

  /** Comments on a board item. Returns the stored comment. */
  commentFeedbackBoard(id: string, body: string, company?: string | null): Promise<BoardComment> {
    return this.request<BoardComment>(
      "POST",
      `${this.scope(company)}/feedback/board/${encodeURIComponent(id)}/comments`,
      { body },
    );
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
    input: {
      name: string;
      role: string;
      description?: string;
      budgetUsdDaily?: number;
      /**
       * Optional persona instructions to give the teammate at birth (issue
       * #1530). Omitted keys are left off the wire, so a caller that does not
       * collect instructions changes nothing.
       */
      instructions?: string;
      /**
       * The job shape that decides this teammate's tool belt (issue #1674),
       * carried by the first-run setup build-out. Sent as the validated wire
       * spelling the roster proposal returned (`research`, `writing`, …); the
       * host derives the belt from it, so the console never chooses a
       * permission boundary. Omitted on every other add path.
       */
      focus?: string;
    },
    company?: string | null,
  ): Promise<TeamMemberDto> {
    return this.request<TeamMemberDto>("POST", `${this.scope(company)}/team`, input);
  }

  /**
   * One agent in full (issue #264): identity, tier, **resolved** tool grants and
   * desk membership.
   *
   * Not derivable from `listTeam`, and that is the point — the roster row
   * carries none of it, and the tool grants had no read surface anywhere before
   * this route, so what a company actually grants an agent could not be checked
   * from outside the process.
   *
   * Hosts predating the route 404; callers should treat that as "this host
   * can't open an agent yet" rather than as a missing teammate.
   */
  getAgent(agentId: string, company?: string | null): Promise<AgentDetailDto> {
    return this.request<AgentDetailDto>(
      "GET",
      `${this.scope(company)}/team/${encodeURIComponent(agentId)}`,
    );
  }

  /**
   * Every harness this company has declared (issue #1245's harness-picker
   * follow-up): what Settings' Harnesses card and an agent's Harness picker
   * both read, so the two cannot disagree about what the company has
   * declared. Read-only — hosts predating the route 404, which callers should
   * treat as "this host can't list harnesses yet" rather than as an empty set.
   */
  listHarnesses(company?: string | null): Promise<HarnessDto[]> {
    return this.request<HarnessDto[]>("GET", `${this.scope(company)}/harnesses`);
  }

  /**
   * Edit an agent, and get the whole agent back (issue #264).
   *
   * A patch: keys absent from `input` are left alone, so a caller that renders
   * some of an agent's fields cannot blank the rest by omission. `description:
   * null` clears the instructions and `description: undefined` leaves them,
   * which is why the two must not be collapsed on the way in.
   *
   * A manifest teammate is editable too: the host stores the change as an
   * override on the company record and never rewrites `company.toml`, including
   * persona instructions (issue #1530). `instructions: null` clears that
   * override and restores the blueprint value. Ask `getAgent` first — its
   * `editable` list is the host's own statement of which fields this call will
   * accept, and `tools` is admin-only.
   */
  updateAgent(
    agentId: string,
    input: EditAgentInput,
    company?: string | null,
  ): Promise<AgentDetailDto> {
    return this.request<AgentDetailDto>(
      "PATCH",
      `${this.scope(company)}/team/${encodeURIComponent(agentId)}`,
      input,
    );
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

  /**
   * Remove a teammate. A blueprint teammate is removed by tombstone rather than
   * by rewriting `company.toml`, so it works for both kinds; the only refusal is
   * a `409` on the company's last teammate.
   */
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
   * The company's agent-authored dashboard pages (Pages tab). Each manifest
   * names a slug served at `pageUrl(slug, company)` — the iframe host
   * document that mounts the page's compiled bundle. Hosts without the
   * surface 404; callers treat that as "no pages".
   */
  listPages(company?: string | null): Promise<PageManifestDto[]> {
    return this.request<PageManifestDto[]>("GET", `${this.scope(company)}/pages`);
  }

  /**
   * The URL to load as an iframe `src` for one page — a fixed HTML shell the
   * host serves (not agent content) that sets up an import map for `react`,
   * `react-dom/client`, and `@opencompany/site`, then mounts the page's own
   * `bundle.mjs`. Absolute, since the iframe's `src` is resolved against its
   * own (opaque, sandboxed) document rather than the console's.
   *
   * The iframe is a normal navigation and so can only carry the credentials a
   * browser attaches to a same-origin request — the operator's HttpOnly
   * session cookie. It cannot send this client's `authorization` /
   * `x-opencompany-session` headers, so the shell and its bundle load only
   * when the console is same-origin with the host (the console's supported
   * deployment); a cross-origin console therefore cannot host pages.
   */
  pageUrl(slug: string, company?: string | null): string {
    return `${this.baseUrl}${this.scope(company)}/pages/${encodeURIComponent(slug)}`;
  }

  /**
   * Runs one GraphQL operation — query or mutation — against the host's
   * `/graphql` endpoint, with this client's own credentials. This is the
   * console's one real GraphQL entry point; `PagesView`'s postMessage bridge
   * (`docs/spec/runtime/pages.md` §6) forwards a sandboxed page's requests
   * through this exact method rather than opening a second client, so a page
   * and the console proper can never disagree about how a request is
   * authenticated or parsed.
   *
   * Deliberately untyped in `variables`/return shape: the caller (a page
   * author, indirectly) supplies an arbitrary document, so there is no fixed
   * response type to declare here the way every other method has one.
   *
   * Routed through {@link scope} like every REST call, so the company travels
   * in the path. A document's own company argument is invisible to the host's
   * auth layer, which runs before the body is read; naming it in the URL is
   * what lets a browser holding a session per company on one origin be matched
   * to the right one.
   */
  graphqlRequest(
    query: string,
    variables?: Record<string, unknown>,
    company?: string | null,
  ): Promise<{ data?: unknown; errors?: unknown }> {
    return this.request("POST", `${this.scope(company)}/graphql`, {
      query,
      variables,
    });
  }

  /**
   * Third-party connections for a company (forward-looking surface). Hosts
   * that don't expose it yet return 404 — callers treat that as "unavailable".
   */
  listConnections(company?: string | null): Promise<ConnectionState[]> {
    return this.request<ConnectionState[]>("GET", `${this.scope(company)}/connections`);
  }

  /** Revoke a connected provider. */
  disconnectConnection(provider: string, company?: string | null): Promise<void> {
    return this.request<void>(
      "POST",
      `${this.scope(company)}/connections/${encodeURIComponent(provider)}/disconnect`,
    );
  }

  // The company's MCP tool servers are NOT reachable from this class. They live
  // in `api/mcp.ts`, as standalone functions over the shared client, and that is
  // the only MCP surface the console has. A second set of methods used to sit
  // here — `listMcpServers` promising a `{ servers }` wrapper, servers keyed by
  // `server_id`, `/connect` and `/disconnect` routes — none of which any host
  // has ever served. `request<T>` casts an unparsed body to `T`, so those types
  // were never checked against the wire and the view built on them crashed on
  // open (issue #414). Add MCP calls to `api/mcp.ts`, next to the ones the host
  // answers.

  /**
   * Provision a company from a manifest (issue #1807).
   *
   * Platform-scoped on the host (`PlatformScope`): only a client that carries a
   * platform bearer ({@link carriesPlatformBearer}) can reach it — a person
   * signed in with a session cookie is refused by construction, the same wall
   * `suspend`/`archive` sit behind. The console gates the New-company control on
   * that flag rather than letting the call 401 after the operator has typed a
   * name (the #1401 dishonest-button lesson).
   *
   * `manifest_toml` is the company manifest as TOML; the host fills in
   * `[policy].mode = "auto"` and `[users].mode = "email"` when the text omits
   * them, so `[company].name` alone is a valid body. `id` overrides the id the
   * host would otherwise derive from the company name.
   *
   * Returns the fresh company's status (the host answers `201`), which the
   * caller switches the console into.
   */
  provisionCompany(body: { manifest_toml: string; id?: string }): Promise<CompanyStatus> {
    return this.request<CompanyStatus>("POST", "/api/v1/companies", body);
  }

  /**
   * The auth-mode preflight: the sign-in mode a company
   * provisioned on this host right now would land in, and whether wallet
   * addresses are required.
   *
   * Platform-scoped like {@link provisionCompany}, so a client that carries a
   * platform bearer ({@link carriesPlatformBearer}) can read it. The create /
   * reset dialog calls this on open so it can render the mode's identity field
   * — an email admin or a wallet address — before it builds a manifest, rather
   * than provisioning an `admins`-only manifest a `wallet`-mode host refuses.
   */
  provisioningInfo(): Promise<ProvisioningInfo> {
    return this.request<ProvisioningInfo>("GET", "/api/v1/companies/provisioning");
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

/**
 * Rejects when `signal` aborts, even if `work` never settles.
 *
 * A transport whose `fetch` honours the signal already rejects `work` on abort;
 * the desktop proxy cannot cancel its in-flight IPC and never would, so the
 * abort is raised here too. Either way the caller stops waiting the instant the
 * deadline (or its own signal) fires, and a late transport answer is dropped.
 */
function settleWithin<T>(work: Promise<T>, signal: AbortSignal): Promise<T> {
  if (signal.aborted) return Promise.reject(abortError(signal.reason));
  return new Promise<T>((resolve, reject) => {
    const onAbort = () => reject(abortError(signal.reason));
    signal.addEventListener("abort", onAbort, { once: true });
    work.then(
      (value) => {
        signal.removeEventListener("abort", onAbort);
        resolve(value);
      },
      (err) => {
        signal.removeEventListener("abort", onAbort);
        reject(err);
      },
    );
  });
}

/** A real `AbortError`, reusing the signal's reason when it already is one. */
function abortError(reason: unknown): Error {
  if (reason instanceof Error && reason.name === "AbortError") return reason;
  return new DOMException("The operation was aborted.", "AbortError");
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
  const { error, code, problems } = parsed as Record<string, unknown>;
  if (typeof error !== "string" || typeof code !== "string") return undefined;
  const breakdown = workflowProblems(problems);
  return breakdown ? { error, code, problems: breakdown } : { error, code };
}

/**
 * The `problems` array off an envelope, or `undefined` when there is not one.
 *
 * Held to the same strictness as the envelope itself, for the same reason: this
 * is rendered to operators, so an entry is kept only when it carries a real
 * `message`. Entries are filtered rather than the whole array rejected — a host
 * that grows a new problem shape should cost the operator that one line, not
 * the entire breakdown — and an array that filters down to nothing returns
 * `undefined` so a caller cannot mistake "every entry was junk" for "the host
 * refused with an empty list".
 *
 * `node_id` and `field` are dropped unless they are strings, keeping a
 * malformed locator from reaching the UI as `[object Object]`.
 */
function workflowProblems(value: unknown): WorkflowProblem[] | undefined {
  if (!Array.isArray(value)) return undefined;
  const kept: WorkflowProblem[] = [];
  for (const entry of value) {
    if (typeof entry !== "object" || entry === null) continue;
    const { node_id, field, message } = entry as Record<string, unknown>;
    if (typeof message !== "string" || !message.trim()) continue;
    kept.push({
      ...(typeof node_id === "string" ? { node_id } : {}),
      ...(typeof field === "string" ? { field } : {}),
      message,
    });
  }
  return kept.length ? kept : undefined;
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
function statusMessage(res: TransportResponse): string {
  return res.statusText.trim() || `HTTP ${res.status}`;
}

/**
 * Whether the host accepted the request.
 *
 * `fetch` hands back a `Response.ok`; a transport hands back a status. Same
 * rule, stated once, so the two readers below cannot disagree about what "not
 * an error" means.
 */
function isOk(res: TransportResponse): boolean {
  return res.status >= 200 && res.status < 300;
}

/**
 * The `ApiError` for a response the host refused (issue #380).
 *
 * Shared by `request` and `getDocument`, which had drifted into two
 * byte-identical copies of this logic — so the bug existed twice and had to be
 * fixed twice. One function means the next change to error handling cannot land
 * on only one of the two readers.
 */
function httpError(res: TransportResponse, text: string): ApiError {
  const envelope = errorEnvelope(text);
  const err = new ApiError(
    res.status,
    envelope?.code ?? `http_${res.status}`,
    envelope?.error ?? statusMessage(res),
    envelope !== undefined,
  );
  // Issue #836: the host has sent this breakdown since #1016 and the console
  // dropped it here, so a refused graph read as one flat sentence with no node
  // named. Carried, not rendered here — what a surface does with it is the
  // surface's call.
  if (envelope?.problems) err.problems = envelope.problems;
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
