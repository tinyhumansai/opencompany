// The company's own TinyHumans credential (issue #586): the one key its admin
// sets on this tenant, and the identity every surface the platform brokers
// presents on the company's behalf.
//
// Set it once and Composio rides it — no separate Composio token, no per-tenant
// provider app to register. Rotate it and every brokered surface moves together,
// because they all resolve through one seam on the host rather than each keeping
// its own copy.
//
// WRITE-ONLY, like every other credential the console handles: the key goes out
// on `PUT .../credential`, lands in the host's secret store, and is never
// returned. The read shape carries only `configured` plus the non-secret tier
// name. Standalone functions over the shared client (mirrors `api/composio.ts`).
//
// NOT the same thing as the inference key in `api/inference.ts`. That one holds
// whatever the company's *declared provider* wants — an OpenRouter key, a raw
// BYOK token — so it is provider-scoped, not an identity, and handing it to the
// TinyHumans backend would present one vendor's credential to another.

import type { OpenCompanyClient } from "./client";

/**
 * Which identity this company's brokered calls present right now.
 *
 * - `company` — this company's own key. What setting one buys you.
 * - `attested` / `static` — no company key set, so calls fall back to the
 *   instance's platform identity.
 * - `none` — neither, so nothing the platform brokers for this company works:
 *   no provider can be connected, and there is no TinyHumans account to bill
 *   thinking to. The honest degraded state, and the one the picker must not
 *   paper over — but it is **not** "nothing works". A company whose LLM page
 *   holds a key of its own thinks perfectly well with this unset: that key
 *   outranks this one in the managed chain, and a provider of its own does not
 *   consult this one at all.
 */
export type CompanyCredentialSource = "company" | "attested" | "static" | "none";

/** The company's credential status. Never carries the key. */
export interface CompanyCredentialStatus {
  /**
   * Whether this company has its **own** key stored. `false` does not mean no
   * credential — read {@link source} for what calls actually present.
   */
  configured: boolean;
  /** Which identity a brokered call presents right now. */
  source: CompanyCredentialSource;
  /**
   * The consequence of setting this key, or the degraded state when nothing can
   * be presented. Rendered verbatim: the host words it, so the console cannot
   * drift from what the host actually does.
   */
  notice: string;
  /**
   * Whether this host can complete a one-click key grant against the hub.
   *
   * `false` on every host with no hub wired — self-hosted, or a build with no
   * exchange — and the console then renders exactly what it rendered before this
   * flow existed: the paste field, alone. Carried on the status rather than
   * asked for separately so the decision costs no extra request, and absent on a
   * host predating the field, which `?? false` reads as "no button", the safe
   * direction.
   */
  hubLink?: boolean;
  /**
   * Where this person looks after the account behind the key — the hub
   * dashboard's key list, and its top-up page.
   *
   * Resolved by the **host**, because only the host knows which hub it was
   * pointed at: a console talking to staging must link to the staging
   * dashboard, and a link assembled in the browser would send an operator to
   * production's billing page. Absent on a host whose backend the naming
   * convention does not describe (self-hosted, loopback), where there is no
   * dashboard to link to — the console then renders no link rather than a
   * guess.
   */
  account?: HubAccountLinks;
}

/** The two hub pages the console links out to. */
export interface HubAccountLinks {
  /** The dashboard's API-key list — where a minted key is seen and revoked. */
  manageKeysUrl: string;
  /** The dashboard's balance and top-up page. */
  topUpUrl: string;
}

/** A mutating response: the resulting status plus a plain-language note. */
export interface CompanyCredentialMutation {
  status: CompanyCredentialStatus;
  note: string;
}

/** Whether this company has its own credential, and which identity it presents. */
export function getCompanyCredential(
  client: OpenCompanyClient,
  company: string | null,
): Promise<CompanyCredentialStatus> {
  return client.get<CompanyCredentialStatus>(`${client.scopeFor(company)}/credential`);
}

/**
 * Set / rotate / clear the company's TinyHumans credential. A non-empty value
 * sets or rotates it; an empty string clears it, falling back to the instance's
 * platform identity where there is one. Admin-only — a member gets a 403.
 */
export function setCompanyCredential(
  client: OpenCompanyClient,
  company: string | null,
  key: string,
): Promise<CompanyCredentialMutation> {
  return client.put<CompanyCredentialMutation>(`${client.scopeFor(company)}/credential`, { key });
}

/**
 * A started key grant: where to send the browser.
 *
 * The console navigates to this and nothing more. It never sees the PKCE
 * verifier, and it never sees the key — both stay on the host, which is the
 * point of doing the exchange there (see `server::hub_link`).
 */
export interface CredentialLinkStart {
  authorizeUrl: string;
}

/**
 * Begin a one-click TinyHumans connection. Admin-only; 404 on a host with no hub.
 *
 * The returned URL is a **top-level navigation**, not a fetch: the hub signs the
 * person in through their provider and shows them a consent screen, and both
 * need to happen on the hub's own origin with its own address bar visible.
 */
export function startCredentialLink(
  client: OpenCompanyClient,
  company: string | null,
): Promise<CredentialLinkStart> {
  return client.post<CredentialLinkStart>(`${client.scopeFor(company)}/credential/link/start`, {});
}

/**
 * Finish a connection: hand the host the code the hub returned, and the `state`
 * it started with.
 *
 * The host redeems these for a key and stores it as both the company credential
 * and the inference key. The response is the same shape a paste would have
 * produced, so the card that called this can repaint from it either way.
 */
export function finishCredentialLink(
  client: OpenCompanyClient,
  company: string | null,
  state: string,
  code: string,
): Promise<CompanyCredentialMutation> {
  return client.post<CompanyCredentialMutation>(
    `${client.scopeFor(company)}/credential/link/finish`,
    { state, code },
  );
}

/** The account's money, as the API Key page draws it. */
export interface BillingSummary {
  /** Everything spendable — promotional credit and top-up together, in USD. */
  balanceUsd: number;
  /** The plan slug (`free`, `pro`, …). */
  plan: string;
  /** Whether a paid subscription is live right now. */
  activeSubscription: boolean;
  /** When the plan lapses, if it does. */
  planExpiry?: string;
  /** Where a person tops up, on the hub that issued the key. */
  topUpUrl?: string;
  /** Where a person changes the plan. */
  manageUrl?: string;
}

/**
 * The billing panel's whole state, including its two empty cases.
 *
 * `configured: false` is "no key, so nothing to ask about" — the page shows the
 * pitch. `unavailable` is "there is a key but the hub would not answer", which
 * is deliberately not the same as a zero balance: they look identical on a card
 * and mean opposite things, one "top up" and one "try again".
 */
export interface CompanyBilling {
  configured: boolean;
  summary?: BillingSummary;
  unavailable?: string;
}

/**
 * What the account behind this company's key has left to spend.
 *
 * Read through the **host**, which presents the key it holds — the console
 * never sees the credential, so it could not ask the hub itself. A read and
 * only a read: topping up and changing plans happen signed in on the hub.
 */
export function getCompanyBilling(
  client: OpenCompanyClient,
  company: string | null,
): Promise<CompanyBilling> {
  return client.get<CompanyBilling>(`${client.scopeFor(company)}/credential/billing`);
}
