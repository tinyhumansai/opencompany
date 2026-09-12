// The search configuration API: which providers a company has connected, which
// one its teammates search through, and the credential behind each.
//
// Keys are write-only: they are sent into the host's secret store and are never
// returned. Every read shape carries slugs, booleans and the non-secret
// endpoints only, so there is no field on these types that could leak a key into
// a rendered page.
//
// Standalone functions over the shared client, mirroring `api/hosting.ts`, so
// `OpenCompanyClient` needs no new methods.

import type { OpenCompanyClient } from "./client";
import type { ConnectOutcome, SearchProvider } from "@/search-providers/types";

/**
 * The non-secret view of a company's search configuration.
 *
 * `provider` is the active provider; `effectiveProvider` is what the teammates
 * actually search through. They differ exactly when the active provider is
 * missing its credential, which is the one state a single "connected" flag
 * would render as a working connection.
 */
export interface SearchStatus {
  /** The active provider. `managed` when nothing resolves. */
  provider: string;
  /** The provider the teammates actually search through. */
  effectiveProvider: string;
  /** Every connected provider. */
  providers: SearchProvider[];
  /** Whether a key is stored for the active provider. Never the key. */
  apiKeyConfigured: boolean;
  /** The active provider's instance URL, where it has one. Not secret. */
  endpoint: string | null;
  /** Whether the active provider is still missing its key. */
  needsApiKey: boolean;
  /** Whether the active provider is still missing its endpoint. */
  needsEndpoint: boolean;
  /** Whether the company's manifest explicitly grants `search`. */
  granted: boolean;
  /** Whether the running host has the search tools compiled in. */
  inBuild: boolean;
  /**
   * Whether the platform's own managed search credential resolves here.
   *
   * The Managed row is rendered from this rather than from a permanent
   * "Always on" badge: managed search is always the *fallback*, which is a
   * different claim from always *working*.
   */
  managedConfigured: boolean;
  /** The company's daily managed-search ceiling. */
  managedDailyCallCap: number;
  /** The providers this build can search through. */
  supportedProviders: string[];
}

/** What a connect or test attempt came back with, beside the new status. */
export interface SearchConnectResult extends ConnectOutcome {
  status: SearchStatus;
}

/** The write-only save body for the single-slot route. */
export interface SearchConfig {
  /** The provider slug. Omit to leave it unchanged. */
  provider?: string;
  /** Write-only. Omit to leave the stored key unchanged. */
  apiKey?: string;
  /** The instance URL, for SearXNG. Omit to leave it unchanged. */
  endpoint?: string;
}

/** Reads the company's search configuration status. */
export async function getSearch(
  client: OpenCompanyClient,
  company: string | null,
): Promise<SearchStatus> {
  return client.get<SearchStatus>(`${client.scopeFor(company)}/search`);
}

/**
 * Saves whatever is supplied, and returns the resulting status.
 *
 * A patch, not a replace. **Note what this no longer means**: it used to be safe
 * to switch provider without re-typing a key, because there was one key slot for
 * the whole company — which is precisely the bug, since the old provider's key
 * then authenticated against the new provider and every surface reported a
 * working connection until an agent's first search came back 401. Each provider
 * now holds its own credential, so naming a provider here applies the key to
 * *that* provider and to nothing else.
 */
export async function saveSearch(
  client: OpenCompanyClient,
  company: string | null,
  config: SearchConfig,
): Promise<SearchStatus> {
  return client.put<SearchStatus>(`${client.scopeFor(company)}/search`, config);
}

/** Connects one provider, probing it and reporting what the probe meant. */
export async function connectSearchProvider(
  client: OpenCompanyClient,
  company: string | null,
  body: { slug: string; apiKey?: string; endpoint?: string },
): Promise<SearchConnectResult> {
  return client.post<SearchConnectResult>(
    `${client.scopeFor(company)}/search/providers`,
    body,
  );
}

/** Turns one provider on or off, or re-addresses a self-hosted one. */
export async function updateSearchProvider(
  client: OpenCompanyClient,
  company: string | null,
  slug: string,
  body: { enabled?: boolean; endpoint?: string },
): Promise<SearchStatus> {
  return client.put<SearchStatus>(
    `${client.scopeFor(company)}/search/providers/${encodeURIComponent(slug)}`,
    body,
  );
}

/** Removes one provider, clearing its credential with it. */
export async function removeSearchProvider(
  client: OpenCompanyClient,
  company: string | null,
  slug: string,
): Promise<SearchStatus> {
  return client.del<SearchStatus>(
    `${client.scopeFor(company)}/search/providers/${encodeURIComponent(slug)}`,
  );
}

/**
 * Replaces one provider's key, or clears it with an empty value.
 *
 * Write-only in both directions: nothing is returned but the status, and the
 * status carries `keyConfigured` rather than anything derived from the key.
 */
export async function replaceSearchProviderKey(
  client: OpenCompanyClient,
  company: string | null,
  slug: string,
  apiKey: string,
): Promise<SearchStatus> {
  return client.put<SearchStatus>(
    `${client.scopeFor(company)}/search/providers/${encodeURIComponent(slug)}/key`,
    { apiKey },
  );
}

/** Marks which provider the teammates search through, or unmarks it. */
export async function setSearchDefault(
  client: OpenCompanyClient,
  company: string | null,
  slug: string | null,
): Promise<SearchStatus> {
  return client.put<SearchStatus>(
    `${client.scopeFor(company)}/search/default`,
    { slug },
  );
}

/**
 * Checks a provider without changing anything.
 *
 * A credential may be supplied to check a draft: testing a credential and
 * committing to it are separate acts, and the host discards what it is given.
 */
export async function testSearchProvider(
  client: OpenCompanyClient,
  company: string | null,
  body: { slug: string; apiKey?: string; endpoint?: string },
): Promise<SearchConnectResult> {
  return client.post<SearchConnectResult>(
    `${client.scopeFor(company)}/search/test`,
    body,
  );
}

/** Clears every connection, falling the company back to managed search. */
export async function clearSearch(
  client: OpenCompanyClient,
  company: string | null,
): Promise<SearchStatus> {
  return client.del<SearchStatus>(`${client.scopeFor(company)}/search/key`);
}
