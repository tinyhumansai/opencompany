// The live Composio API (issue #110, epic #26 Cell D): the console reads the
// company's Composio status and writes its per-tenant OAuth bearer token through
// the host's `.../composio` routes (REST, camelCase over the wire).
//
// The token is the entire tenant-isolation lever — the backend derives the
// Composio entity from it — so it is WRITE-ONLY: sent on `PUT .../composio/token`
// and stored in the host's secret store; it is never returned. The read shape
// carries only a `tokenConfigured` boolean plus non-secret routing (backend URL,
// toolkit allowlist). Standalone functions over the shared client (mirrors
// `api/inference.ts`), so no change to `OpenCompanyClient` is needed.

import type { OpenCompanyClient } from "./client";

/** The company's Composio status. Never carries the token. */
export interface ComposioStatus {
  /** Whether the `composio` feature is compiled into this build at all. */
  inBuild: boolean;
  /** Whether the company explicitly grants `composio` (a `*` wildcard does not count). */
  granted: boolean;
  /** Whether a per-tenant token is stored — never the token itself. */
  tokenConfigured: boolean;
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

/** The company's Composio status. */
export function getComposioStatus(
  client: OpenCompanyClient,
  company: string | null,
): Promise<ComposioStatus> {
  return client.get<ComposioStatus>(`${client.scopeFor(company)}/composio`);
}

/**
 * Set / rotate / clear the write-only per-tenant Composio token. A non-empty
 * value rotates it; an empty string clears it.
 */
export function setComposioToken(
  client: OpenCompanyClient,
  company: string | null,
  token: string,
): Promise<ComposioMutation> {
  return client.put<ComposioMutation>(`${client.scopeFor(company)}/composio/token`, { token });
}
