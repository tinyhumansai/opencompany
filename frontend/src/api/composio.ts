// The live Composio API (issue #110, epic #26 Cell D): the console reads the
// company's Composio status and — when a company brings its own account — writes
// a per-company OAuth bearer token through the host's `.../composio` routes
// (REST, camelCase over the wire).
//
// On the hosted platform nothing is pasted: the instance authenticates with a
// platform-minted, audience-bound identity and Composio calls present that, which
// the read shape reports as `credentialSource: "attested"`. The write route is the
// per-company BYO override, and the only option when this repo is run standalone
// — which is UNSUPPORTED. Either way the token is WRITE-ONLY: sent on
// `PUT .../composio/token`, stored in the host's secret store, never returned. The
// read shape carries only `credentialSource` plus non-secret routing (backend URL,
// toolkit allowlist). Standalone functions over the shared client (mirrors
// `api/inference.ts`), so no change to `OpenCompanyClient` is needed.

import type { OpenCompanyClient, RequestOptions } from "./client";

/**
 * Where this company's Composio credential comes from.
 *
 * - `attested` — the instance's own platform identity; nothing is stored here.
 * - `company` — this company's **own** TinyHumans credential, set by its admin
 *   (issue #586). The backend derives the Composio entity from it, so a company
 *   with a key set connects providers as itself with no Composio token and no
 *   per-tenant provider app. Rotating that one key moves every brokered surface
 *   at once — see `api/credential.ts`.
 * - `static` — a Composio token this company pasted, or a static instance key.
 * - `none` — no credential can be obtained, so agents get no Composio tools.
 */
export type ComposioCredentialSource =
  "attested" | "company" | "static" | "none";

/**
 * Which host this company's Composio calls go to.
 *
 * - `managed` — proxied through the OpenHuman backend, which owns the Composio
 *   API key, the toolkit allowlist and the billing. The default, and the route
 *   that needs no configuration at all.
 * - `byok` — straight to this company's **own** Composio account with the API key
 *   its admin stored. Nothing is proxied and nothing is billed here; the
 *   providers it can connect are whatever that Composio account permits.
 *
 * Orthogonal to {@link ComposioCredentialSource}, which names *whose identity* a
 * call presents rather than *which host* it is presented to. A BYOK company
 * reports `byok` + `static`; a company that pasted a backend token override
 * reports `managed` + `static`.
 */
export type ComposioMode = "managed" | "byok";

/**
 * One provider in the catalog the host offers, with the backend's own display
 * metadata (issue #600).
 *
 * Every field but `slug` is best-effort and may be empty — a manifest
 * allowlist, a degraded fallback, and a backend predating Composio's dynamic
 * catalog all yield slug-only entries. The console fills those in from
 * `@/lib/composio-catalog`, which is why that local typography table survives
 * rather than being deleted in favour of the backend's names.
 */
export interface ComposioToolkitEntry {
  /** Toolkit slug, e.g. `googlecalendar`. The key every host call is made with. */
  slug: string;
  /** Human-readable name, e.g. `Google Calendar`. Empty when unpublished. */
  name: string;
  /** One-line description. Empty when unpublished. Searched alongside the name. */
  description: string;
  /** Composio-hosted logo URL, or `null` when unpublished. */
  logo: string | null;
  /**
   * Composio's own free-form category names, e.g. `["productivity", "email"]`.
   *
   * Forwarded verbatim by the host and bucketed here by substring — which is
   * what means a Composio integration added tomorrow lands in the right group
   * with no code change on either side of the wire.
   */
  categories: string[];
}

/** The company's Composio status. Never carries the token. */
export interface ComposioStatus {
  /** Whether the `composio` feature is compiled into this build at all. */
  inBuild: boolean;
  /** Whether the company explicitly grants `composio` (a `*` wildcard does not count). */
  granted: boolean;
  /** Which credential this company's Composio calls present — never the credential itself. */
  credentialSource: ComposioCredentialSource;
  /**
   * Which host those calls go to — OpenHuman-managed, or this company's own
   * Composio account.
   *
   * Optional on the wire: a host predating BYOK answers without it, and absent
   * must read as `managed` (the only route those hosts have) rather than as
   * "unknown".
   */
  mode?: ComposioMode;
  /**
   * What the **managed chain** resolves to, independently of the stored mode.
   *
   * Under `mode: "managed"` it equals {@link credentialSource}. Under
   * `mode: "byok"` it says what managed *would* resolve to if the company went
   * back — which is the only way the console can offer that route honestly
   * rather than offering a switch into an outage.
   *
   * A **tier name**, never a credential and never a boolean about a secret
   * slot. That distinction is issue #886: `composioTokenConfigured` answers
   * only about the first of three tiers and is routinely `false` on a working
   * hosted tenant, so anything driven off it paints a live connector red.
   *
   * Optional on the wire: a host predating this field omits it, and absent must
   * read as "not said" rather than as `none` — see `composio/rows.ts`, which
   * falls back to {@link credentialSource} only where the two are defined to
   * agree.
   */
  managedCredentialSource?: ComposioCredentialSource;
  /**
   * The endpoint the calls actually reach (non-secret) — the managed backend, or
   * Composio's own API host under `byok`.
   */
  backendUrl: string;
  /** The manifest toolkit allowlist verbatim (empty = defer to the backend allowlist). */
  toolkits: string[];
  /**
   * Whether this company is in **open mode** — an empty manifest allowlist,
   * which means the backend's own allowlist governs and every toolkit it
   * permits is reachable (issue #397).
   *
   * The host tells us rather than letting us infer it: an empty `toolkits`
   * means *allow everything*, which is the opposite of what an empty list
   * reads as. Gating provider rows on `toolkits.length > 0` was exactly that
   * misreading, and it left 19 of 20 shipped templates with nothing to click.
   */
  openMode: boolean;
  /**
   * The toolkits to render as provider rows — the manifest list when non-empty,
   * else the backend's live catalog. In open mode this is still not a hard
   * limit: any slug the backend permits can be authorized, which is what the
   * "other provider" field is for.
   */
  effectiveToolkits: string[];
  /**
   * The same providers as {@link effectiveToolkits}, in the same order, each
   * carrying whatever display metadata the backend published for it (issue
   * #600).
   *
   * This is what makes the panel browsable. Before it, the host reduced every
   * catalog entry to a bare slug one layer before the console, so 123 providers
   * could only be a flat list: there was nothing to group by, nothing to brand
   * with, and nothing to search but the slug.
   *
   * Additive rather than a replacement — {@link effectiveToolkits} is still the
   * slug contract, and it is still all an authorize call needs.
   */
  effectiveCatalog: ComposioToolkitEntry[];
  /**
   * Where {@link effectiveToolkits} came from (issue #397).
   *
   * - `manifest` — the company's own allowlist, offered verbatim. Not a
   *   degradation: the company chose this list.
   * - `backend` — Composio's live catalog. Authoritative.
   * - `fallback` — the catalog could not be fetched, so this is a built-in
   *   starter list that may be incomplete. Always paired with
   *   {@link catalogNotice}.
   *
   * Rendering a `fallback` the same way as a `backend` list would waste the
   * distinction: eight providers would look like the whole set, which is the
   * shape of the bug this issue was reopened for.
   */
  catalogSource: "manifest" | "backend" | "fallback";
  /** Why the list is a fallback, for the operator. `null` unless degraded. */
  catalogNotice: string | null;
}

/**
 * What a failed credential check means.
 *
 * The host classifies; the console only renders. The raw upstream error is what
 * the classifier reads and it must not reach the console at all — it can echo
 * request headers or fragments of the key just written, and a console banner is
 * the most screenshot-able surface there is. `composio/classify.ts` turns this
 * value into a sentence.
 *
 * Only `auth` is destructive: the host rejects the write with a 4xx and stores
 * nothing. Every other class **stores the key** and returns an {@link
 * ComposioMutation.advisory} beside it, because a corporate proxy, a WAF, a
 * rate limit and a slow upstream all fail a probe while the key is perfectly
 * good — and the naive roll-back-on-any-failure flow destroys valid credentials.
 */
export type ComposioProbeClass =
  "auth" | "endpoint" | "quota" | "timeout" | "unknown";

/** A mutating response: the resulting status plus a plain-language note. */
export interface ComposioMutation {
  status: ComposioStatus;
  note: string;
  /**
   * Operator-facing copy for a **non-destructive** probe failure. The key
   * **was** stored; only reachability is in question.
   *
   * Optional, and absent on success and on every host predating the probe.
   * Never rendered as an error — see {@link ComposioProbeClass}.
   */
  advisory?: string;
  /** Which class of failure {@link advisory} is about. Absent when there was none. */
  probeClass?: ComposioProbeClass;
}

/**
 * The `POST …/composio/api-key/test` verdict — the stored key, checked in place.
 *
 * A different shape from {@link ComposioMutation} on purpose, because it is a
 * different act. That one reports on a write that happened and hangs an
 * advisory off it; this one writes nothing at all, on any path — **including
 * `auth`**. A Test that cleared a rejected key would be the worst control on
 * the page: the operator pressed the one thing that promised to be safe.
 *
 * `probeClass` and `message` are **omitted** rather than nulled when `ok` is
 * true, matching every other optional field the host sends. The copy comes from
 * the host rather than from `composio/classify.ts`: the advisory copy there is
 * framed for a write that landed ("Saved, but …") and saying that about a check
 * which stored nothing is a statement about an event that did not happen.
 */
export interface ComposioApiKeyTest {
  /** Whether Composio answered the check. */
  ok: boolean;
  /** Why it did not, when it did not. Absent when `ok`. */
  probeClass?: ComposioProbeClass;
  /** The host's verdict sentence for {@link probeClass}. Absent when `ok`. */
  message?: string;
}

/** The `POST …/composio/authorize` response: the hosted connect URL to open. */
export interface ComposioAuthorize {
  /** Composio-hosted OAuth URL the operator opens in a new browser tab. */
  connectUrl: string;
}

/**
 * One connected account inside a {@link ComposioConnection} (issue #404).
 *
 * A non-secret projection of the host's `ConnectedAccountDto` — the id is
 * Composio's connection id, which is safe to hold here because it is the path
 * segment `DELETE …/composio/connections/{id}` takes and carries no credential.
 */
export interface ComposioConnectedAccount {
  /** Composio's connection id — what a disconnect is addressed to. */
  id: string;
  /**
   * Composio's raw status string (`ACTIVE`, `INITIATED`, `EXPIRED`, …),
   * forwarded verbatim so the console can tell "never set up" from "set up and
   * since expired" rather than flattening both to "not connected".
   */
  status: string;
  /** Whether this individual account is usable. */
  connected: boolean;
  /**
   * When Composio recorded the connection. Absent when Composio did not say —
   * and absent for every other connection system, none of which records one.
   * Rendered as "not recorded" rather than as a blank that reads like "never".
   */
  createdAt?: string;
  /**
   * The account label the provider published (`ops@acme.test`). Omitted rather
   * than guessed: an account the provider did not name has no honest label, and
   * inventing one from the toolkit is how two Gmail accounts become
   * indistinguishable at exactly the moment the operator has to pick one.
   */
  account?: string;
  /**
   * Whether this is the account the company chose to act as for the toolkit
   * (issue #820). False on every account until somebody chooses — nothing is
   * defaulted implicitly, because a default the harness does not honour reads
   * as a guarantee.
   *
   * Optional on the wire for the same reason {@link ComposioConnection.accounts}
   * is: a host predating #820 answers without it, and absent must read as "no
   * choice", not as "this one".
   */
  isDefault?: boolean;
}

/** One toolkit's connected state, as returned by `GET …/composio/connections`. */
export interface ComposioConnection {
  /** Toolkit slug, e.g. `gmail`. */
  toolkit: string;
  /** Whether the company has at least one active connection for this toolkit. */
  connected: boolean;
  /**
   * Every connection this company holds for the toolkit, oldest id first
   * (issue #404).
   *
   * Usually one. Composio permits several accounts per toolkit and a company
   * that connected Gmail twice needs to see which is which before revoking one
   * — `connected: boolean` alone cannot back a disconnect, because it names
   * nothing to disconnect.
   *
   * Optional on the wire rather than required: the field was added additively
   * to a route this console already called, and a host predating it answers
   * without one. Callers treat a missing list as "this host cannot open a
   * provider", not as "no accounts".
   */
  accounts?: ComposioConnectedAccount[];
  /**
   * The account the company chose for this toolkit (issue #820), or absent.
   *
   * **Absent is the ordinary state and means nothing is chosen** — Composio
   * resolves the account itself, exactly as it did before a company could
   * express a preference. The console must not fill this in from the account
   * list: a default the harness does not honour reads as a guarantee.
   */
  defaultConnectionId?: string;
}

/**
 * The host's own budget for the upstream catalog fetch behind `GET …/composio`
 * — `composio_toolkits::FETCH_TIMEOUT` in `src/server/ops/composio_toolkits.rs`.
 *
 * Declared here so {@link CATALOG_READ_TIMEOUT_MS} can be checked against it.
 * A change on the host that is not mirrored here breaks the invariant test
 * rather than the Apps page.
 */
export const SERVER_FETCH_TIMEOUT_MS = 5_000;

/**
 * How long the console waits on `GET …/composio` before giving up.
 *
 * Must strictly dominate {@link SERVER_FETCH_TIMEOUT_MS}: on a cold catalog the
 * host spends up to that budget upstream and then answers with a flagged
 * fallback, so a client deadline at or below it cancels the read at exactly the
 * moment the host is about to explain itself, and the fallback can never be
 * rendered. Below the client's default `GET` deadline, because this read blocks a
 * view and wants a tighter bound than the shared default.
 */
export const CATALOG_READ_TIMEOUT_MS = 15_000;

/**
 * The company's Composio status.
 *
 * `options` reaches the client's own deadline and cancellation: this route
 * fetches a live catalog from the platform, so a caller that blocks a view on
 * it wants a bound of its own and a way to drop a superseded read.
 */
export function getComposioStatus(
  client: OpenCompanyClient,
  company: string | null,
  options?: RequestOptions,
): Promise<ComposioStatus> {
  return client.get<ComposioStatus>(
    `${client.scopeFor(company)}/composio`,
    options,
  );
}

/**
 * Set / rotate / clear this company's own Composio token. A non-empty value
 * rotates it; an empty string clears it, reverting to the instance's identity
 * where there is one.
 */
export function setComposioToken(
  client: OpenCompanyClient,
  company: string | null,
  token: string,
): Promise<ComposioMutation> {
  return client.put<ComposioMutation>(
    `${client.scopeFor(company)}/composio/token`,
    { token },
  );
}

/**
 * Point this company at its **own** Composio account, or give the managed route
 * back (BYOK).
 *
 * A non-empty `apiKey` stores the key and switches the company to `byok`; an
 * empty string clears it and returns it to OpenHuman-managed Composio. One call
 * for both because the mode is a consequence of the key rather than a separate
 * control — selecting BYOK with nothing stored would leave the company with no
 * Composio tools and no visible reason why.
 *
 * WRITE-ONLY, like every other credential here: the key goes out on this call,
 * lands in the host's secret store, and is never returned. Admin-only — a member
 * gets a 403.
 *
 * **Not** interchangeable with {@link setComposioToken}. That one stores a bearer
 * the *TinyHumans backend* recognises and leaves the route managed; this one
 * stores a key *Composio* recognises and changes the route. They authenticate
 * different hosts.
 */
export function setComposioApiKey(
  client: OpenCompanyClient,
  company: string | null,
  apiKey: string,
  /**
   * Store the key without probing it first.
   *
   * Offered **only after a typed probe failure**, never as a standing option: a
   * Composio account behind a corporate proxy fails the check while the key is
   * fine, and without this the operator cannot get past a check that is wrong
   * about them. Defaulted to `false` here rather than omitted-means-skip, so a
   * caller that forgets the argument gets the verified path.
   */
  skipVerify = false,
): Promise<ComposioMutation> {
  return client.put<ComposioMutation>(
    `${client.scopeFor(company)}/composio/api-key`,
    {
      apiKey,
      skipVerify,
    },
  );
}

/**
 * Check the Composio API key this company already has stored, and report the
 * verdict. Changes nothing.
 *
 * **No arguments beyond the scope.** The key is read from the company's own
 * store and the endpoint is a compile-time constant on the host, so there is
 * nothing here that could point a credential at a caller-chosen address. A
 * *draft* key is checked by {@link setComposioApiKey}, which has to be handed
 * one anyway.
 *
 * Admin-only: it spends the company's credential against a third party, and the
 * answer is about the company's standing with a vendor. A member gets a 403.
 *
 * Answers `409 not_configured` when there is nothing to check — the company is
 * on the managed route, or it is on BYOK with a blank slot. That is a permanent
 * state rather than a failed check, which is why the rows hide the control
 * instead of offering one that can only fail.
 */
export function testComposioApiKey(
  client: OpenCompanyClient,
  company: string | null,
): Promise<ComposioApiKeyTest> {
  return client.post<ComposioApiKeyTest>(
    `${client.scopeFor(company)}/composio/api-key/test`,
    {},
  );
}

/**
 * Begin a per-provider OAuth handoff for `toolkit` (e.g. `gmail`). Returns the
 * Composio-hosted connect URL the console opens in a new tab. Composio runs the
 * OAuth itself — there is no local callback — so the console then polls
 * {@link listComposioConnections} until the toolkit reports connected. 409 when
 * the feature is not in the build, or no per-tenant token is configured yet.
 */
export function startComposioAuthorize(
  client: OpenCompanyClient,
  company: string | null,
  toolkit: string,
): Promise<ComposioAuthorize> {
  return client.post<ComposioAuthorize>(
    `${client.scopeFor(company)}/composio/authorize`,
    {
      toolkit,
    },
  );
}

/**
 * The company's per-toolkit connected state — one row per toolkit that has at
 * least one connection. The console cross-references this against the granted
 * `toolkits` to render each provider row's connected / sign-in state. 409 when
 * the feature is not in the build, or no per-tenant token is configured yet.
 */
export function listComposioConnections(
  client: OpenCompanyClient,
  company: string | null,
): Promise<ComposioConnection[]> {
  return client.get<ComposioConnection[]>(
    `${client.scopeFor(company)}/composio/connections`,
  );
}

/** What the host says it revoked, in its own words. */
export interface ComposioDisconnect {
  note: string;
}

/** The `…/default` response: what the company now acts as, and a sentence. */
export interface ComposioDefaultMutation {
  /** The toolkit the change applied to. Empty on a clear. */
  toolkit: string;
  /** The account now acting for that toolkit — absent after a clear. */
  connectionId?: string;
  /** Plain-language confirmation, in the host's own words. */
  note: string;
}

/**
 * Revoke one connected account (issue #404).
 *
 * Addressed by Composio's connection id, not by toolkit: a company with two
 * Gmail accounts has two ids and exactly one of them is being released.
 *
 * This is **not** interchangeable with `client.disconnectConnection`, which
 * posts to `…/connections/{provider}/disconnect` and blanks the host's own
 * `oauth/{provider}` secret. That route has never touched Composio, so calling
 * it for a Composio-connected provider reports success and leaves the account
 * connected on the next refresh. 404 when the id is not this company's; 409
 * when Composio is not in the build.
 */
export function disconnectComposioConnection(
  client: OpenCompanyClient,
  company: string | null,
  connectionId: string,
): Promise<ComposioDisconnect> {
  return client.del<ComposioDisconnect>(
    `${client.scopeFor(company)}/composio/connections/${encodeURIComponent(connectionId)}`,
  );
}

/**
 * Make `connectionId` the account this company's agents act as for its toolkit
 * (issue #820). Admin-only; 404 when the id names no connection this company
 * holds, or names one that is connected but not usable.
 *
 * The toolkit is not passed: it is a property of the connection, and asking the
 * caller to repeat it would only let the two disagree.
 */
export function setComposioDefaultAccount(
  client: OpenCompanyClient,
  company: string | null,
  connectionId: string,
): Promise<ComposioDefaultMutation> {
  return client.put<ComposioDefaultMutation>(
    `${client.scopeFor(company)}/composio/connections/${encodeURIComponent(connectionId)}/default`,
    {},
  );
}

/**
 * Stop naming an account for that connection's toolkit — Composio resolves it
 * again, as it did before. Needs no live provider, so it still works when the
 * account is gone or the backend is unreachable.
 */
export function clearComposioDefaultAccount(
  client: OpenCompanyClient,
  company: string | null,
  connectionId: string,
): Promise<ComposioDefaultMutation> {
  return client.del<ComposioDefaultMutation>(
    `${client.scopeFor(company)}/composio/connections/${encodeURIComponent(connectionId)}/default`,
  );
}
