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

import type { OpenCompanyClient } from "./client";

/**
 * Where this company's Composio credential comes from.
 *
 * - `attested` — the instance's own platform identity; nothing is stored here.
 * - `static` — a token this company pasted, or a static instance key.
 * - `none` — no credential can be obtained, so agents get no Composio tools.
 */
export type ComposioCredentialSource = "attested" | "static" | "none";

/** The company's Composio status. Never carries the token. */
export interface ComposioStatus {
  /** Whether the `composio` feature is compiled into this build at all. */
  inBuild: boolean;
  /** Whether the company explicitly grants `composio` (a `*` wildcard does not count). */
  granted: boolean;
  /** Which credential this company's Composio calls present — never the credential itself. */
  credentialSource: ComposioCredentialSource;
  /** The effective Composio backend URL (non-secret). */
  backendUrl: string;
  /** The manifest toolkit allowlist (empty = defer to the backend allowlist). */
  toolkits: string[];
}

/** A mutating response: the resulting status plus a plain-language note. */
export interface ComposioMutation {
  status: ComposioStatus;
  note: string;
}

/** The `POST …/composio/authorize` response: the hosted connect URL to open. */
export interface ComposioAuthorize {
  /** Composio-hosted OAuth URL the operator opens in a new browser tab. */
  connectUrl: string;
}

/** One toolkit's connected state, as returned by `GET …/composio/connections`. */
export interface ComposioConnection {
  /** Toolkit slug, e.g. `gmail`. */
  toolkit: string;
  /** Whether the company has at least one active connection for this toolkit. */
  connected: boolean;
}

/** The company's Composio status. */
export function getComposioStatus(
  client: OpenCompanyClient,
  company: string | null,
): Promise<ComposioStatus> {
  return client.get<ComposioStatus>(`${client.scopeFor(company)}/composio`);
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
  return client.put<ComposioMutation>(`${client.scopeFor(company)}/composio/token`, { token });
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
  return client.post<ComposioAuthorize>(`${client.scopeFor(company)}/composio/authorize`, {
    toolkit,
  });
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
  return client.get<ComposioConnection[]>(`${client.scopeFor(company)}/composio/connections`);
}
